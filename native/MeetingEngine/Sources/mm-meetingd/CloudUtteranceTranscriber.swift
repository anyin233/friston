import EngineKit
import Foundation
import MeetingCore

/// Errors raised by ``CloudUtteranceClient`` when uploading a completed VAD utterance via HTTP.
enum CloudUtteranceError: Error, CustomStringConvertible {
    case unsupportedProvider(String)
    case httpError(statusCode: Int, message: String)
    case transportError(String)
    case decodeError(String)

    var description: String {
        switch self {
        case .unsupportedProvider(let p): "unsupported cloud utterance provider: \(p)"
        case .httpError(let status, let msg): "HTTP \(status): \(msg)"
        case .transportError(let msg): "HTTP transport error: \(msg)"
        case .decodeError(let msg): "response decode error: \(msg)"
        }
    }

    /// Whether this failure represents invalid/unauthorized credentials or exhausted quota
    /// that warrants immediate fallback to local Qwen3-ASR.
    var isAuthOrQuotaError: Bool {
        switch self {
        case .httpError(let status, let msg):
            CloudAuthErrorDetector.isAuthOrQuotaFailure(statusCode: status, message: msg)
        case .unsupportedProvider:
            true
        case .transportError(let msg), .decodeError(let msg):
            CloudAuthErrorDetector.isAuthOrQuotaFailure(statusCode: nil, message: msg)
        }
    }
}

/// Async HTTP client that uploads completed speech utterances (16 kHz mono WAV) to OpenAI
/// (`/v1/audio/transcriptions`) or Google Gemini (`generateContent` / `interactions`).
///
/// Never opens persistent WebSockets and never logs `config.apiKey`.
final class CloudUtteranceClient: Sendable {
    private let config: LiveAsrConfig
    private let provider: CloudLiveProvider
    private let language: String?
    private let baseURLOverride: String?
    private let session: URLSession

    /// Creates an HTTP utterance client.
    /// - Parameters:
    ///   - config: Provider, model, and API key (`apiKey` is never logged).
    ///   - language: Optional BCP-47 caption language hint.
    ///   - baseURLOverride: Optional API base URL override (defaults to `MM_CLOUD_UTTERANCE_BASE_URL` env or provider default).
    init(config: LiveAsrConfig, language: String?, baseURLOverride: String? = nil) throws {
        guard let provider = CloudLiveProvider(rawValue: config.provider.lowercased()) else {
            throw CloudUtteranceError.unsupportedProvider(config.provider)
        }
        self.config = config
        self.provider = provider
        self.language = language
        let envBase = ProcessInfo.processInfo.environment["MM_CLOUD_UTTERANCE_BASE_URL"]?
            .trimmingCharacters(in: .whitespacesAndNewlines)
        let resolvedBase = baseURLOverride?.trimmingCharacters(in: .whitespacesAndNewlines)
        self.baseURLOverride = (resolvedBase?.isEmpty == false ? resolvedBase : nil)
            ?? (envBase?.isEmpty == false ? envBase : nil)

        let cfg = URLSessionConfiguration.ephemeral
        cfg.timeoutIntervalForRequest = 20
        cfg.timeoutIntervalForResource = 30
        self.session = URLSession(configuration: cfg)
    }

    deinit {
        session.invalidateAndCancel()
    }

    /// Encodes `samples16k` as 16 kHz mono WAV and transcribes via the configured cloud HTTP endpoint.
    /// - Parameter samples16k: Completed speech utterance samples at 16 kHz.
    /// - Returns: Normalized transcript text and metered token/audio usage.
    func transcribe(samples16k: [Float]) async throws -> CloudUtteranceTranscript {
        guard !samples16k.isEmpty else {
            return CloudUtteranceTranscript(text: "")
        }
        let wav = WAVEncoder.encode16kMonoPCM16(samples16k)
        switch provider {
        case .openai:
            return try await transcribeOpenAI(wav: wav)
        case .gemini:
            return try await transcribeGemini(wav: wav)
        }
    }

    // MARK: OpenAI

    private func transcribeOpenAI(wav: Data) async throws -> CloudUtteranceTranscript {
        let base = baseURLOverride ?? "https://api.openai.com"
        let endpoint = OpenAIUtteranceCodec.endpointURL(baseURL: base)
        let req = OpenAIUtteranceCodec.buildRequest(
            endpoint: endpoint,
            apiKey: config.apiKey,
            model: config.model,
            language: language,
            wavData: wav
        )
        let (data, http) = try await perform(req)
        guard (200..<300).contains(http.statusCode) else {
            throw CloudUtteranceError.httpError(
                statusCode: http.statusCode,
                message: OpenAIUtteranceCodec.parseErrorMessage(data)
            )
        }
        do {
            return try OpenAIUtteranceCodec.parseResponse(data)
        } catch {
            throw CloudUtteranceError.decodeError("\(error)")
        }
    }

    // MARK: Gemini

    private func transcribeGemini(wav: Data) async throws -> CloudUtteranceTranscript {
        let base = baseURLOverride ?? GeminiUtteranceCodec.defaultBase
        let apiKey = config.apiKey.trimmingCharacters(in: .whitespacesAndNewlines)

        if GeminiUtteranceCodec.isDedicatedTranscribeModel(config.model) {
            return try await transcribeGeminiInteractions(base: base, apiKey: apiKey, wav: wav)
        }

        var req = URLRequest(url: GeminiUtteranceCodec.generateContentURL(model: config.model, baseURL: base))
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.setValue(apiKey, forHTTPHeaderField: "x-goog-api-key")
        req.httpBody = GeminiUtteranceCodec.generateContentBody(wavData: wav, language: language)

        let (data, http) = try await perform(req)
        guard (200..<300).contains(http.statusCode) else {
            throw CloudUtteranceError.httpError(
                statusCode: http.statusCode,
                message: GeminiUtteranceCodec.parseErrorMessage(data)
            )
        }
        do {
            return try GeminiUtteranceCodec.parseGenerateContentResponse(data)
        } catch {
            throw CloudUtteranceError.decodeError("\(error)")
        }
    }

    private func transcribeGeminiInteractions(base: String, apiKey: String, wav: Data) async throws -> CloudUtteranceTranscript {
        // 1. Start resumable Files API upload.
        var initReq = URLRequest(url: GeminiUtteranceCodec.uploadStartURL(baseURL: base))
        initReq.httpMethod = "POST"
        initReq.setValue(apiKey, forHTTPHeaderField: "x-goog-api-key")
        initReq.setValue("resumable", forHTTPHeaderField: "x-goog-upload-protocol")
        initReq.setValue("start", forHTTPHeaderField: "x-goog-upload-command")
        initReq.setValue("\(wav.count)", forHTTPHeaderField: "x-goog-upload-header-content-length")
        initReq.setValue("audio/wav", forHTTPHeaderField: "x-goog-upload-header-content-type")
        initReq.setValue("application/json", forHTTPHeaderField: "Content-Type")
        initReq.httpBody = Data(#"{"file":{"display_name":"utterance.wav"}}"#.utf8)

        let (initData, initHttp) = try await perform(initReq)
        guard (200..<300).contains(initHttp.statusCode) else {
            throw CloudUtteranceError.httpError(
                statusCode: initHttp.statusCode,
                message: GeminiUtteranceCodec.parseErrorMessage(initData)
            )
        }
        guard let uploadURLStr = initHttp.value(forHTTPHeaderField: "x-goog-upload-url")?
                .trimmingCharacters(in: .whitespacesAndNewlines),
              !uploadURLStr.isEmpty,
              let uploadURL = URL(string: uploadURLStr) else {
            throw CloudUtteranceError.decodeError("Gemini upload missing x-goog-upload-url")
        }

        // 2. Finalize bytes upload.
        var putReq = URLRequest(url: uploadURL)
        putReq.httpMethod = "POST"
        putReq.setValue("0", forHTTPHeaderField: "x-goog-upload-offset")
        putReq.setValue("upload, finalize", forHTTPHeaderField: "x-goog-upload-command")
        putReq.httpBody = wav

        let (putData, putHttp) = try await perform(putReq)
        guard (200..<300).contains(putHttp.statusCode) else {
            throw CloudUtteranceError.httpError(
                statusCode: putHttp.statusCode,
                message: GeminiUtteranceCodec.parseErrorMessage(putData)
            )
        }
        guard let obj = (try? JSONSerialization.jsonObject(with: putData)) as? [String: Any] else {
            throw CloudUtteranceError.decodeError("invalid JSON from Gemini file finalize")
        }
        let fileObj = (obj["file"] as? [String: Any]) ?? obj
        guard let name = fileObj["name"] as? String,
              let uri = fileObj["uri"] as? String else {
            throw CloudUtteranceError.decodeError("Gemini file response missing name/uri")
        }

        // Best-effort cleanup of temporary utterance audio on exit.
        let deleteURL = GeminiUtteranceCodec.fileURL(name: name, baseURL: base)
        let sessionRef = session
        defer {
            Task.detached {
                var del = URLRequest(url: deleteURL)
                del.httpMethod = "DELETE"
                del.setValue(apiKey, forHTTPHeaderField: "x-goog-api-key")
                _ = try? await sessionRef.data(for: del)
            }
        }

        var state = (fileObj["state"] as? String) ?? "ACTIVE"
        var polls = 0
        while state == "PROCESSING" && polls < 40 {
            try await Task.sleep(for: .milliseconds(250))
            polls += 1
            var pollReq = URLRequest(url: deleteURL)
            pollReq.httpMethod = "GET"
            pollReq.setValue(apiKey, forHTTPHeaderField: "x-goog-api-key")
            let (pollData, pollHttp) = try await perform(pollReq)
            guard (200..<300).contains(pollHttp.statusCode) else {
                throw CloudUtteranceError.httpError(
                    statusCode: pollHttp.statusCode,
                    message: GeminiUtteranceCodec.parseErrorMessage(pollData)
                )
            }
            if let pollObj = (try? JSONSerialization.jsonObject(with: pollData)) as? [String: Any] {
                let inner = (pollObj["file"] as? [String: Any]) ?? pollObj
                state = (inner["state"] as? String) ?? "ACTIVE"
            }
        }
        if state != "ACTIVE" {
            throw CloudUtteranceError.decodeError("Gemini file state \(state) not ACTIVE")
        }

        // 3. Call Interactions API.
        var intReq = URLRequest(url: GeminiUtteranceCodec.interactionsURL(baseURL: base))
        intReq.httpMethod = "POST"
        intReq.setValue("application/json", forHTTPHeaderField: "Content-Type")
        intReq.setValue(apiKey, forHTTPHeaderField: "x-goog-api-key")
        intReq.httpBody = GeminiUtteranceCodec.interactionsBody(
            model: config.model, fileUri: uri, mimeType: "audio/wav", language: language
        )
        let (intData, intHttp) = try await perform(intReq)
        guard (200..<300).contains(intHttp.statusCode) else {
            throw CloudUtteranceError.httpError(
                statusCode: intHttp.statusCode,
                message: GeminiUtteranceCodec.parseErrorMessage(intData)
            )
        }
        do {
            return try GeminiUtteranceCodec.parseInteractionsResponse(intData)
        } catch {
            throw CloudUtteranceError.decodeError("\(error)")
        }
    }

    private func perform(_ request: URLRequest) async throws -> (Data, HTTPURLResponse) {
        let data: Data
        let response: URLResponse
        do {
            (data, response) = try await session.data(for: request)
        } catch {
            throw CloudUtteranceError.transportError(error.localizedDescription)
        }
        guard let http = response as? HTTPURLResponse else {
            throw CloudUtteranceError.transportError("non-HTTP response")
        }
        return (data, http)
    }
}
