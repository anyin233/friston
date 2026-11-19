import EngineKit
import Foundation
import MeetingCore

/// Result returned once a cloud streaming utterance commits and finalizes.
struct CloudUtteranceResult: Sendable, Equatable {
    var text: String
    var inputTokens: Int
    var outputTokens: Int
}

/// Errors raised by ``CloudLiveStreamClient``.
enum CloudLiveClientError: Error, CustomStringConvertible {
    case unsupportedProvider(String)
    case invalidURL
    case socketClosed(String)
    case timeout(String)
    case serverError(String)
    case wrongUtterance(expected: Int?, got: Int)

    var description: String {
        switch self {
        case .unsupportedProvider(let p): "unsupported cloud live provider: \(p)"
        case .invalidURL: "invalid cloud live WebSocket URL"
        case .socketClosed(let why): "cloud live socket closed: \(why)"
        case .timeout(let step): "cloud live \(step) timed out"
        case .serverError(let msg): "cloud live server error: \(msg)"
        case .wrongUtterance(let expected, let got):
            "unexpected utterance \(got) while \(expected.map(String.init) ?? "none") is open"
        }
    }

    var isAuthOrQuotaError: Bool {
        CloudAuthErrorDetector.isAuthOrQuotaFailure(statusCode: nil, message: description)
    }
}

/// Per-stream WebSocket client for OpenAI Realtime Transcription or Gemini Live.
///
/// One actor owns one `URLSessionWebSocketTask` (mic or system audio) so state transitions
/// and continuations are strictly serialized under Swift 6 concurrency. Never logs `apiKey`.
actor CloudLiveStreamClient {
    private let config: LiveAsrConfig
    private let provider: CloudLiveProvider
    private let language: String?
    private let originLabel: String

    private var task: URLSessionWebSocketTask?
    private var receiveTask: Task<Void, Never>?
    private var connectedAt: Date?
    private var lastActivityAt: Date?
    private var isReady = false
    private var resampler24k: StreamingResampler?

    private var activeUtteranceId: Int?
    private var partialText = ""
    private var onPartial: (@Sendable (String) -> Void)?
    private var pendingInputTokens = 0
    private var pendingOutputTokens = 0

    private var waiterToken = 0
    private var readyWaiter: (token: Int, cont: CheckedContinuation<Void, Error>)?
    private var finalWaiter: (token: Int, cont: CheckedContinuation<CloudUtteranceResult, Error>)?
    private var earlyFinalResult: CloudUtteranceResult?

    /// Proactive reconnect threshold (9 minutes) before a new utterance starts.
    private static let maxSessionAgeSeconds: TimeInterval = 9 * 60
    /// 200 ms chunks at 16 kHz keep WebSocket frames compact and responsive.
    private static let frameSamples16k = 3_200

    /// Creates a stream client without opening the socket yet.
    /// - Parameters:
    ///   - config: Provider, model and API key (`apiKey` is never logged).
    ///   - language: Optional BCP-47 caption language hint.
    ///   - origin: Stream label for diagnostics.
    init(config: LiveAsrConfig, language: String?, origin: AudioOrigin) throws {
        guard let provider = CloudLiveProvider(rawValue: config.provider.lowercased()) else {
            throw CloudLiveClientError.unsupportedProvider(config.provider)
        }
        self.config = config
        self.provider = provider
        self.language = language
        self.originLabel = origin.rawValue
    }

    /// Utterance id currently open on this stream, if any.
    var currentUtteranceId: Int? { activeUtteranceId }

    /// Whether a WebSocket connection is open and past its initial handshake.
    var isConnected: Bool { task != nil && isReady }

    /// Opens the WebSocket and completes the provider handshake if not already ready.
    func connect() async throws {
        if task != nil && isReady { return }
        disconnect(with: CloudLiveClientError.socketClosed("reconnecting"))

        let request: URLRequest
        let setupPayload: String
        switch provider {
        case .openai:
            var req = URLRequest(url: OpenAIRealtimeCodec.endpointURL())
            for (k, v) in OpenAIRealtimeCodec.headers(apiKey: config.apiKey) {
                req.setValue(v, forHTTPHeaderField: k)
            }
            request = req
            setupPayload = OpenAIRealtimeCodec.sessionUpdateJSON(model: config.model, language: language)
            resampler24k = StreamingResampler(inputRate: 16_000, outputRate: 24_000)
        case .gemini:
            guard let url = GeminiLiveCodec.endpointURL(apiKey: config.apiKey) else {
                throw CloudLiveClientError.invalidURL
            }
            request = URLRequest(url: url)
            setupPayload = GeminiLiveCodec.setupJSON(model: config.model)
            resampler24k = nil
        }

        let ws = URLSession.shared.webSocketTask(with: request)
        task = ws
        let now = Date()
        connectedAt = now
        lastActivityAt = now
        isReady = false
        ws.resume()

        receiveTask = Task { [weak self] in
            while !Task.isCancelled {
                do {
                    let message = try await ws.receive()
                    let text: String? = switch message {
                    case .string(let s): s
                    case .data(let d): String(data: d, encoding: .utf8)
                    @unknown default: nil
                    }
                    if let text {
                        await self?.handleServerText(text)
                    }
                } catch {
                    await self?.handleSocketFailure(error)
                    break
                }
            }
        }

        try await sendText(setupPayload, on: ws)
        if isReady {
            lastActivityAt = Date()
            return
        }
        guard task != nil else { throw CloudLiveClientError.socketClosed("disconnected during handshake") }
        try await waitForReady(timeoutSeconds: 8.0)
        lastActivityAt = Date()
        log("cloud live \(originLabel): connected (\(provider.rawValue) · \(config.model))")
    }

    /// Starts streaming a new VAD utterance, rotating aging connections cleanly between turns.
    /// - Parameters:
    ///   - id: Engine utterance id.
    ///   - onPartial: Invoked whenever the server emits updated interim text.
    func beginUtterance(id: Int, onPartial: @escaping @Sendable (String) -> Void) async throws {
        if activeUtteranceId != nil {
            throw CloudLiveClientError.wrongUtterance(expected: activeUtteranceId, got: id)
        }
        if let connectedAt, Date().timeIntervalSince(connectedAt) >= Self.maxSessionAgeSeconds {
            log("cloud live \(originLabel): rotating WebSocket session after 9 min")
            disconnect(with: CloudLiveClientError.socketClosed("session rotation"))
        }
        try await connect()
        guard let ws = task else { throw CloudLiveClientError.socketClosed("missing socket") }

        activeUtteranceId = id
        partialText = ""
        pendingInputTokens = 0
        pendingOutputTokens = 0
        earlyFinalResult = nil
        lastActivityAt = Date()
        self.onPartial = onPartial

        switch provider {
        case .openai:
            resampler24k = StreamingResampler(inputRate: 16_000, outputRate: 24_000)
        case .gemini:
            try await sendText(GeminiLiveCodec.activityStartJSON(), on: ws)
        }
    }

    /// Streams 16 kHz mono samples for the open utterance in 200 ms chunks.
    /// - Parameter samples: 16 kHz Float32 audio slice.
    func pushAudio16k(_ samples: [Float]) async throws {
        guard !samples.isEmpty else { return }
        guard activeUtteranceId != nil else { return }
        guard let ws = task else { throw CloudLiveClientError.socketClosed("not connected") }

        var offset = 0
        while offset < samples.count {
            let end = min(samples.count, offset + Self.frameSamples16k)
            let chunk = Array(samples[offset..<end])
            offset = end

            let frameJSON: String
            switch provider {
            case .openai:
                let pcm24k = resampler24k?.process(chunk) ?? chunk
                let b64 = PCM16Encoder.encodeBase64(pcm24k)
                guard !b64.isEmpty else { continue }
                frameJSON = OpenAIRealtimeCodec.audioAppendJSON(base64PCM24k: b64)
            case .gemini:
                let b64 = PCM16Encoder.encodeBase64(chunk)
                guard !b64.isEmpty else { continue }
                frameJSON = GeminiLiveCodec.audioChunkJSON(base64PCM16k: b64)
            }
            try await sendText(frameJSON, on: ws)
            lastActivityAt = Date()
        }
    }

    /// Commits the open utterance and awaits its finalized transcript and token usage.
    /// - Parameter timeoutSeconds: Maximum wait for the server's final transcript event.
    /// - Returns: Final transcript text and metered tokens.
    func finishUtterance(timeoutSeconds: Double = 8.0) async throws -> CloudUtteranceResult {
        defer {
            activeUtteranceId = nil
            onPartial = nil
            partialText = ""
            earlyFinalResult = nil
            lastActivityAt = Date()
        }
        guard let ws = task else { throw CloudLiveClientError.socketClosed("not connected") }

        let commitJSON = switch provider {
        case .openai: OpenAIRealtimeCodec.audioCommitJSON()
        case .gemini: GeminiLiveCodec.activityEndJSON()
        }
        try await sendText(commitJSON, on: ws)
        if let early = earlyFinalResult {
            earlyFinalResult = nil
            return early
        }
        guard task != nil else { throw CloudLiveClientError.socketClosed("disconnected during commit") }
        return try await waitForFinal(timeoutSeconds: timeoutSeconds)
    }

    /// Aborts an open utterance whose VAD segment was dropped without finalizing, clearing any
    /// buffered uncommitted noise on the server so it cannot contaminate the next utterance.
    /// - Parameter id: Utterance id to discard (no-op if no longer open).
    func abortUtterance(id: Int) async {
        guard activeUtteranceId == id else { return }
        activeUtteranceId = nil
        onPartial = nil
        partialText = ""
        earlyFinalResult = nil
        pendingInputTokens = 0
        pendingOutputTokens = 0
        lastActivityAt = Date()
        switch provider {
        case .openai:
            if let ws = task {
                try? await ws.send(.string(OpenAIRealtimeCodec.audioClearJSON()))
            }
        case .gemini:
            // Gemini Live has no input buffer clear frame; closing guarantees a clean socket on next speech.
            disconnect(with: CloudLiveClientError.socketClosed("aborted blip"))
        }
    }

    /// Closes an idle WebSocket after `idleSeconds` with no active utterance so silent streams
    /// do not hold open persistent connections.
    func disconnectIfIdle(after idleSeconds: TimeInterval = 60) {
        guard task != nil, activeUtteranceId == nil, let last = lastActivityAt else { return }
        guard Date().timeIntervalSince(last) >= idleSeconds else { return }
        log("cloud live \(originLabel): closing idle socket after \(Int(idleSeconds))s of silence")
        disconnect(with: CloudLiveClientError.socketClosed("idle disconnect"))
    }

    /// Closes the WebSocket and fails any pending waiters cleanly.
    func close() {
        disconnect(with: CloudLiveClientError.socketClosed("session ended"))
    }

    // MARK: Internals

    private func sendText(_ text: String, on ws: URLSessionWebSocketTask) async throws {
        do {
            try await ws.send(.string(text))
        } catch {
            disconnect(with: error)
            throw error
        }
    }

    private func waitForReady(timeoutSeconds: Double) async throws {
        waiterToken &+= 1
        let token = waiterToken
        let timeoutTask = Task { [weak self] in
            try? await Task.sleep(for: .seconds(timeoutSeconds))
            await self?.failReadyWaiter(token: token, error: CloudLiveClientError.timeout("handshake"))
        }
        defer { timeoutTask.cancel() }
        try await withCheckedThrowingContinuation { (cont: CheckedContinuation<Void, Error>) in
            readyWaiter = (token, cont)
        }
    }

    private func waitForFinal(timeoutSeconds: Double) async throws -> CloudUtteranceResult {
        waiterToken &+= 1
        let token = waiterToken
        let timeoutTask = Task { [weak self] in
            try? await Task.sleep(for: .seconds(timeoutSeconds))
            await self?.failFinalWaiter(token: token, error: CloudLiveClientError.timeout("transcription"))
        }
        defer { timeoutTask.cancel() }
        return try await withCheckedThrowingContinuation { (cont: CheckedContinuation<CloudUtteranceResult, Error>) in
            finalWaiter = (token, cont)
        }
    }

    private func handleServerText(_ text: String) {
        let event: CloudLiveServerEvent? = switch provider {
        case .openai: OpenAIRealtimeCodec.parseServerEvent(text)
        case .gemini: GeminiLiveCodec.parseServerEvent(text)
        }
        guard let event else { return }

        switch event {
        case .ready:
            isReady = true
            if let waiter = readyWaiter {
                readyWaiter = nil
                waiter.cont.resume()
            }
        case .partialDelta(let delta):
            partialText += delta
            let snapshot = partialText.trimmingCharacters(in: .whitespacesAndNewlines)
            if !snapshot.isEmpty {
                onPartial?(snapshot)
            }
        case .partialReplace(let full):
            partialText = full
            if !full.isEmpty {
                onPartial?(full)
            }
        case .usage(let inTok, let outTok):
            pendingInputTokens = max(pendingInputTokens, inTok)
            pendingOutputTokens = max(pendingOutputTokens, outTok)
        case .finalTranscript(let finalText, let inTok, let outTok):
            let inputTokens = max(pendingInputTokens, inTok)
            let outputTokens = max(pendingOutputTokens, outTok)
            pendingInputTokens = 0
            pendingOutputTokens = 0
            let result = CloudUtteranceResult(
                text: finalText, inputTokens: inputTokens, outputTokens: outputTokens)
            if let waiter = finalWaiter {
                finalWaiter = nil
                waiter.cont.resume(returning: result)
            } else if activeUtteranceId != nil {
                earlyFinalResult = result
            }
        case .error(let msg):
            log("cloud live \(originLabel): server error: \(msg)")
            let err = CloudLiveClientError.serverError(msg)
            if let waiter = readyWaiter {
                readyWaiter = nil
                waiter.cont.resume(throwing: err)
            }
            if let waiter = finalWaiter {
                finalWaiter = nil
                waiter.cont.resume(throwing: err)
            }
        }
    }

    private func handleSocketFailure(_ error: Error) {
        log("cloud live \(originLabel): socket disconnected: \(error.localizedDescription)")
        disconnect(with: error)
    }

    private func failReadyWaiter(token: Int, error: Error) {
        guard let waiter = readyWaiter, waiter.token == token else { return }
        readyWaiter = nil
        waiter.cont.resume(throwing: error)
        disconnect(with: error)
    }

    private func failFinalWaiter(token: Int, error: Error) {
        guard let waiter = finalWaiter, waiter.token == token else { return }
        finalWaiter = nil
        waiter.cont.resume(throwing: error)
        disconnect(with: error)
    }

    private func disconnect(with error: Error) {
        receiveTask?.cancel()
        receiveTask = nil
        task?.cancel(with: .goingAway, reason: nil)
        task = nil
        connectedAt = nil
        isReady = false
        if let waiter = readyWaiter {
            readyWaiter = nil
            waiter.cont.resume(throwing: error)
        }
        if let waiter = finalWaiter {
            finalWaiter = nil
            waiter.cont.resume(throwing: error)
        }
    }
}
