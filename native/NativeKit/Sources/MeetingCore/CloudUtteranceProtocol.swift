import Foundation

/// Normalized transcript and usage parsed from an HTTP utterance transcription call.
public struct CloudUtteranceTranscript: Sendable, Equatable {
    public var text: String
    public var inputTokens: Int
    public var outputTokens: Int
    public var billedAudioMs: Int?

    public init(text: String, inputTokens: Int = 0, outputTokens: Int = 0, billedAudioMs: Int? = nil) {
        self.text = text
        self.inputTokens = inputTokens
        self.outputTokens = outputTokens
        self.billedAudioMs = billedAudioMs
    }
}

/// Errors raised while decoding HTTP cloud utterance payloads.
public enum CloudUtteranceCodecError: Error, CustomStringConvertible, Equatable {
    case invalidJSON

    public var description: String {
        switch self {
        case .invalidJSON: "invalid JSON response from cloud transcription endpoint"
        }
    }
}

/// Encodes 16 kHz mono Float32 audio into standard PCM16LE RIFF/WAVE bytes.
public enum WAVEncoder {
    /// Encodes 16 kHz mono PCM16LE WAV data suitable for OpenAI and Gemini REST upload.
    /// - Parameter samples: Normalized audio samples (`-1...1`).
    /// - Returns: Complete RIFF/WAVE payload (44-byte header + PCM16LE frames).
    public static func encode16kMonoPCM16(_ samples: [Float]) -> Data {
        let sampleRate: UInt32 = 16_000
        let channels: UInt16 = 1
        let bitsPerSample: UInt16 = 16
        let blockAlign = channels * (bitsPerSample / 8)
        let byteRate = sampleRate * UInt32(blockAlign)
        let dataBytes = UInt32(samples.count * Int(blockAlign))
        let riffSize = UInt32(36) &+ dataBytes

        var out = Data(capacity: 44 + Int(dataBytes))
        func appendASCII(_ s: String) {
            out.append(contentsOf: s.utf8)
        }
        func appendLE<T: FixedWidthInteger>(_ v: T) {
            var le = v.littleEndian
            withUnsafeBytes(of: &le) { out.append(contentsOf: $0) }
        }

        appendASCII("RIFF")
        appendLE(riffSize)
        appendASCII("WAVE")
        appendASCII("fmt ")
        appendLE(UInt32(16)) // PCM fmt chunk length
        appendLE(UInt16(1)) // PCM linear quantization
        appendLE(channels)
        appendLE(sampleRate)
        appendLE(byteRate)
        appendLE(blockAlign)
        appendLE(bitsPerSample)
        appendASCII("data")
        appendLE(dataBytes)

        for sample in samples {
            let clamped = max(-1.0 as Float, min(1.0 as Float, sample.isFinite ? sample : 0))
            let scaled: Int16 = if clamped >= 1.0 {
                .max
            } else if clamped <= -1.0 {
                .min
            } else {
                Int16((clamped * 32_767.0).rounded())
            }
            appendLE(scaled)
        }
        return out
    }
}

/// Pure HTTP request builder and response parser for OpenAI `/v1/audio/transcriptions`.
public enum OpenAIUtteranceCodec {
    /// Resolves `/v1/audio/transcriptions` under `baseURL`.
    public static func endpointURL(baseURL: String = "https://api.openai.com") -> URL {
        let clean = baseURL.trimmingCharacters(in: CharacterSet(charactersIn: "/"))
        return URL(string: "\(clean)/v1/audio/transcriptions")!
    }

    /// Builds a multipart/form-data POST request carrying 16 kHz mono WAV audio.
    public static func buildRequest(
        endpoint: URL,
        apiKey: String,
        model: String,
        language: String?,
        wavData: Data,
        boundary: String = "----FristonUtterance\(UUID().uuidString.replacingOccurrences(of: "-", with: ""))"
    ) -> URLRequest {
        var body = Data()
        func append(_ s: String) {
            body.append(contentsOf: s.utf8)
        }
        func appendText(_ name: String, _ value: String) {
            append("--\(boundary)\r\n")
            append("Content-Disposition: form-data; name=\"\(name)\"\r\n\r\n")
            append("\(value)\r\n")
        }

        append("--\(boundary)\r\n")
        append("Content-Disposition: form-data; name=\"file\"; filename=\"utterance.wav\"\r\n")
        append("Content-Type: audio/wav\r\n\r\n")
        body.append(wavData)
        append("\r\n")

        let trimmedModel = model.trimmingCharacters(in: .whitespacesAndNewlines)
        appendText("model", trimmedModel)
        if let code = normalizedISO639(language) {
            appendText("language", code)
        }
        let lowerModel = trimmedModel.lowercased()
        if lowerModel.contains("diarize") {
            appendText("response_format", "diarized_json")
            appendText("chunking_strategy", "auto")
        } else if lowerModel.hasPrefix("whisper") {
            appendText("response_format", "verbose_json")
        } else {
            appendText("response_format", "json")
        }
        append("--\(boundary)--\r\n")

        var req = URLRequest(url: endpoint)
        req.httpMethod = "POST"
        req.setValue("Bearer \(apiKey.trimmingCharacters(in: .whitespacesAndNewlines))", forHTTPHeaderField: "Authorization")
        req.setValue("multipart/form-data; boundary=\(boundary)", forHTTPHeaderField: "Content-Type")
        req.httpBody = body
        return req
    }

    /// Parses OpenAI `json`, `verbose_json`, or `diarized_json` responses.
    public static func parseResponse(_ data: Data) throws -> CloudUtteranceTranscript {
        guard let obj = decodeObject(data) else {
            throw CloudUtteranceCodecError.invalidJSON
        }
        var text = (obj["text"] as? String ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
        if text.isEmpty, let segments = obj["segments"] as? [[String: Any]] {
            let parts = segments.compactMap { seg -> String? in
                let t = (seg["text"] as? String ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
                return t.isEmpty ? nil : t
            }
            text = parts.joined(separator: " ")
        }
        let usage = obj["usage"] as? [String: Any]
        let inTok = intValue(usage?["input_tokens"])
        let outTok = intValue(usage?["output_tokens"])
        let billedSec = doubleValue(usage?["seconds"]) ?? doubleValue(obj["duration"])
        let billedMs: Int? = if let s = billedSec, s.isFinite, s > 0 {
            Int((s * 1000).rounded())
        } else {
            nil
        }
        return CloudUtteranceTranscript(
            text: text,
            inputTokens: inTok,
            outputTokens: outTok,
            billedAudioMs: billedMs
        )
    }

    /// Extracts an informative error message from a failed OpenAI HTTP response body.
    public static func parseErrorMessage(_ data: Data) -> String {
        if let obj = decodeObject(data),
           let err = obj["error"] as? [String: Any],
           let msg = err["message"] as? String,
           !msg.isEmpty {
            return msg
        }
        return String(data: data, encoding: .utf8)?
            .trimmingCharacters(in: .whitespacesAndNewlines) ?? "HTTP error"
    }

    private static func normalizedISO639(_ raw: String?) -> String? {
        guard let raw else { return nil }
        let clean = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !clean.isEmpty, clean.caseInsensitiveCompare("auto") != .orderedSame else { return nil }
        return clean.split(whereSeparator: { $0 == "-" || $0 == "_" }).first.map(String.init)
    }
}

/// Pure HTTP request builder and response parser for Google Gemini REST utterance transcription.
public enum GeminiUtteranceCodec {
    public static let defaultBase = "https://generativelanguage.googleapis.com"

    /// Whether `model` targets the dedicated `v1beta/interactions` speech-to-text endpoint.
    public static func isDedicatedTranscribeModel(_ model: String) -> Bool {
        model.lowercased().contains("transcribe")
    }

    public static func generateContentURL(model: String, baseURL: String = defaultBase) -> URL {
        let clean = cleanBase(baseURL)
        return URL(string: "\(clean)/v1beta/models/\(model.trimmingCharacters(in: .whitespacesAndNewlines)):generateContent")!
    }

    public static func uploadStartURL(baseURL: String = defaultBase) -> URL {
        URL(string: "\(cleanBase(baseURL))/upload/v1beta/files")!
    }

    public static func interactionsURL(baseURL: String = defaultBase) -> URL {
        URL(string: "\(cleanBase(baseURL))/v1beta/interactions")!
    }

    public static func fileURL(name: String, baseURL: String = defaultBase) -> URL {
        let cleanName = name.trimmingCharacters(in: CharacterSet(charactersIn: "/"))
        return URL(string: "\(cleanBase(baseURL))/v1beta/\(cleanName)")!
    }

    /// Encodes a compact inline `audio/wav` `generateContent` payload requesting `{ "text": "..." }` JSON.
    public static func generateContentBody(wavData: Data, language: String?) -> Data {
        let langHint: String = switch language?.trimmingCharacters(in: .whitespacesAndNewlines).lowercased() {
        case "zh", "zh-cn", "zh-hans":
            "Use Simplified Chinese for Mandarin speech."
        case "en", "en-us":
            "Transcribe speech in English."
        case "ja", "ja-jp":
            "Transcribe speech in Japanese."
        case let other? where !other.isEmpty && other != "auto":
            "Follow the spoken language (\(other))."
        default:
            "Auto-detect and preserve the spoken language accurately."
        }
        let prompt = """
        Transcribe all audible human speech in this single meeting utterance accurately.
        - \(langHint)
        - Return {"text": ""} if the clip contains only silence or noise; never fabricate speech.
        """
        let payload: [String: Any] = [
            "contents": [[
                "parts": [
                    [
                        "inline_data": [
                            "mime_type": "audio/wav",
                            "data": wavData.base64EncodedString(),
                        ],
                    ],
                    ["text": prompt],
                ],
            ]],
            "generationConfig": [
                "responseMimeType": "application/json",
                "responseSchema": [
                    "type": "OBJECT",
                    "properties": [
                        "text": ["type": "STRING"],
                    ],
                    "required": ["text"],
                ],
            ],
        ]
        return encodeJSONData(payload)
    }

    /// Parses a Gemini `generateContent` JSON or plain-text response into a transcript.
    public static func parseGenerateContentResponse(_ data: Data) throws -> CloudUtteranceTranscript {
        guard let obj = decodeObject(data) else {
            throw CloudUtteranceCodecError.invalidJSON
        }
        var rawText = ""
        if let candidates = obj["candidates"] as? [[String: Any]],
           let content = candidates.first?["content"] as? [String: Any],
           let parts = content["parts"] as? [[String: Any]] {
            rawText = parts.compactMap { $0["text"] as? String }.first ?? ""
        }
        let cleaned = stripCodeFence(rawText)
        var transcriptText = cleaned
        if let innerData = cleaned.data(using: .utf8),
           let innerObj = decodeObject(innerData),
           let t = innerObj["text"] as? String {
            transcriptText = t.trimmingCharacters(in: .whitespacesAndNewlines)
        } else {
            transcriptText = cleaned.trimmingCharacters(in: .whitespacesAndNewlines)
        }
        let (inTok, outTok) = extractGeminiUsage(obj)
        return CloudUtteranceTranscript(text: transcriptText, inputTokens: inTok, outputTokens: outTok)
    }

    /// Encodes a Gemini `v1beta/interactions` request body for dedicated `*-transcribe` models.
    public static func interactionsBody(
        model: String,
        fileUri: String,
        mimeType: String,
        language: String?
    ) -> Data {
        var txCfg: [String: Any] = [
            "mode": [
                "type": "verbatim",
                "timestamp_granularities": ["word"],
            ],
        ]
        if let code = geminiLanguageCode(language) {
            txCfg["language_codes"] = [code]
        }
        let payload: [String: Any] = [
            "model": model.trimmingCharacters(in: .whitespacesAndNewlines),
            "input": [[
                "role": "user",
                "content": [[
                    "type": "audio",
                    "uri": fileUri,
                    "mime_type": mimeType,
                ]],
            ]],
            "generation_config": [
                "transcription_config": txCfg,
            ],
        ]
        return encodeJSONData(payload)
    }

    /// Parses a Gemini Interactions API response (supporting `word_info`, `output_text`, and `output`/`outputs`).
    public static func parseInteractionsResponse(_ data: Data) throws -> CloudUtteranceTranscript {
        guard let obj = decodeObject(data) else {
            throw CloudUtteranceCodecError.invalidJSON
        }
        var words: [String] = []
        if let steps = obj["steps"] as? [[String: Any]] {
            for step in steps {
                if let contents = step["content"] as? [[String: Any]] {
                    for block in contents {
                        if let anns = block["annotations"] as? [[String: Any]] {
                            for ann in anns where (ann["type"] as? String) == "word_info" {
                                if let w = (ann["text"] as? String) ?? (ann["word"] as? String) {
                                    let trimmed = w.trimmingCharacters(in: .whitespacesAndNewlines)
                                    if !trimmed.isEmpty { words.append(trimmed) }
                                }
                            }
                        }
                    }
                }
            }
        }
        if words.isEmpty,
           let meta = obj["transcription_metadata"] as? [String: Any],
           let info = meta["word_info"] as? [[String: Any]] {
            for item in info {
                if let w = (item["text"] as? String) ?? (item["word"] as? String) {
                    let trimmed = w.trimmingCharacters(in: .whitespacesAndNewlines)
                    if !trimmed.isEmpty { words.append(trimmed) }
                }
            }
        }

        var text = joinTokens(words)
        if text.isEmpty {
            if let out = obj["output_text"] as? String {
                text = out.trimmingCharacters(in: .whitespacesAndNewlines)
            } else if let outputs = (obj["output"] as? [[String: Any]]) ?? (obj["outputs"] as? [[String: Any]]),
                      let first = outputs.first {
                if let direct = first["text"] as? String {
                    text = direct.trimmingCharacters(in: .whitespacesAndNewlines)
                } else if let blocks = first["content"] as? [[String: Any]],
                          let blockText = blocks.first?["text"] as? String {
                    text = blockText.trimmingCharacters(in: .whitespacesAndNewlines)
                }
            } else if let steps = obj["steps"] as? [[String: Any]] {
                let stepTexts = steps.compactMap { step -> String? in
                    guard let contents = step["content"] as? [[String: Any]] else { return nil }
                    let s = contents.compactMap { $0["text"] as? String }.joined(separator: " ")
                        .trimmingCharacters(in: .whitespacesAndNewlines)
                    return s.isEmpty ? nil : s
                }
                text = stepTexts.joined(separator: " ")
            }
        }
        let (inTok, outTok) = extractGeminiUsage(obj)
        return CloudUtteranceTranscript(text: text, inputTokens: inTok, outputTokens: outTok)
    }

    /// Extracts an informative error message from a failed Gemini HTTP response body.
    public static func parseErrorMessage(_ data: Data) -> String {
        if let obj = decodeObject(data),
           let err = obj["error"] as? [String: Any],
           let msg = err["message"] as? String,
           !msg.isEmpty {
            return msg
        }
        return String(data: data, encoding: .utf8)?
            .trimmingCharacters(in: .whitespacesAndNewlines) ?? "HTTP error"
    }

    private static func cleanBase(_ baseURL: String) -> String {
        baseURL.trimmingCharacters(in: CharacterSet(charactersIn: "/"))
    }

    private static func geminiLanguageCode(_ lang: String?) -> String? {
        guard let lang else { return nil }
        let clean = lang.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !clean.isEmpty, clean.caseInsensitiveCompare("auto") != .orderedSame else { return nil }
        return switch clean.lowercased() {
        case "zh", "zh-hans": "zh-CN"
        case "zh-hant": "zh-TW"
        case "en": "en-US"
        case "ja": "ja-JP"
        default: clean
        }
    }

    private static func extractGeminiUsage(_ obj: [String: Any]) -> (Int, Int) {
        let usage = (obj["usage_metadata"] as? [String: Any])
            ?? (obj["usageMetadata"] as? [String: Any])
            ?? (obj["usage"] as? [String: Any])
        let inTok = max(
            intValue(usage?["prompt_token_count"]),
            max(
                intValue(usage?["promptTokenCount"]),
                max(intValue(usage?["input_tokens"]), intValue(usage?["total_input_tokens"]))
            )
        )
        let outTok = max(
            intValue(usage?["candidates_token_count"]),
            max(
                intValue(usage?["candidatesTokenCount"]),
                max(intValue(usage?["output_tokens"]), intValue(usage?["total_output_tokens"]))
            )
        )
        return (inTok, outTok)
    }

    private static func stripCodeFence(_ raw: String) -> String {
        var s = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        if s.hasPrefix("```json") {
            s.removeFirst("```json".count)
        } else if s.hasPrefix("```") {
            s.removeFirst("```".count)
        }
        if s.hasSuffix("```") {
            s.removeLast("```".count)
        }
        return s.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    private static func joinTokens(_ tokens: [String]) -> String {
        var out = ""
        for tok in tokens where !tok.isEmpty {
            if out.isEmpty {
                out = tok
                continue
            }
            let last = out.unicodeScalars.last
            let first = tok.unicodeScalars.first
            let attachedPunct = first.map { ",.!?;:)]，。！？、".unicodeScalars.contains($0) } ?? false
            let cjkBoundary = (last.map(isCJK) ?? false) || (first.map(isCJK) ?? false)
            if !attachedPunct && !cjkBoundary {
                out.append(" ")
            }
            out.append(tok)
        }
        return out
    }

    private static func isCJK(_ s: UnicodeScalar) -> Bool {
        switch s.value {
        case 0x3040...0x30FF, 0x3400...0x4DBF, 0x4E00...0x9FFF,
             0xF900...0xFAFF, 0xAC00...0xD7AF, 0x3000...0x303F, 0xFF00...0xFFEF:
            true
        default:
            false
        }
    }
}

private func decodeObject(_ data: Data) -> [String: Any]? {
    guard let raw = try? JSONSerialization.jsonObject(with: data),
          let dict = raw as? [String: Any] else { return nil }
    return dict
}

private func encodeJSONData(_ obj: [String: Any]) -> Data {
    (try? JSONSerialization.data(withJSONObject: obj, options: [.sortedKeys, .withoutEscapingSlashes])) ?? Data()
}

private func intValue(_ any: Any?) -> Int {
    if let n = any as? Int { return n }
    if let n = any as? NSNumber { return n.intValue }
    return 0
}

private func doubleValue(_ any: Any?) -> Double? {
    if let d = any as? Double { return d }
    if let n = any as? NSNumber { return n.doubleValue }
    return nil
}
