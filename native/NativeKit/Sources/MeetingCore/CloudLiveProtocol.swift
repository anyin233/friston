import Foundation

/// Supported cloud streaming ASR providers.
public enum CloudLiveProvider: String, Codable, Sendable, Equatable {
    case openai
    case gemini
}

/// PCM16 little-endian base64 encoder for WebSocket audio frames.
public enum PCM16Encoder {
    /// Converts floating-point mono samples (`-1…1`) to little-endian 16-bit PCM base64.
    /// - Parameter samples: Normalized audio samples.
    /// - Returns: Base64-encoded PCM16LE bytes.
    public static func encodeBase64(_ samples: [Float]) -> String {
        guard !samples.isEmpty else { return "" }
        var data = Data(capacity: samples.count * 2)
        for sample in samples {
            let clamped = max(-1.0 as Float, min(1.0 as Float, sample.isFinite ? sample : 0))
            let scaled: Int16 = if clamped >= 1.0 {
                .max
            } else if clamped <= -1.0 {
                .min
            } else {
                Int16((clamped * 32_767.0).rounded())
            }
            var le = scaled.littleEndian
            withUnsafeBytes(of: &le) { data.append(contentsOf: $0) }
        }
        return data.base64EncodedString()
    }
}

/// Normalized inbound event parsed from OpenAI Realtime or Gemini Live WebSockets.
public enum CloudLiveServerEvent: Sendable, Equatable {
    case ready
    case partialDelta(String)
    case partialReplace(String)
    case finalTranscript(text: String, inputTokens: Int, outputTokens: Int)
    case usage(inputTokens: Int, outputTokens: Int)
    case error(String)
}

/// Pure JSON wire codec for OpenAI's Realtime Transcription WebSocket (`intent=transcription`).
public enum OpenAIRealtimeCodec {
    /// OpenAI Realtime transcription WebSocket endpoint (model MUST be sent in `session.update`, not URL).
    public static func endpointURL() -> URL {
        URL(string: "wss://api.openai.com/v1/realtime?intent=transcription")!
    }

    /// Authorization headers for the WebSocket handshake.
    /// - Parameter apiKey: OpenAI API key.
    /// - Returns: HTTP header dictionary.
    public static func headers(apiKey: String) -> [String: String] {
        ["Authorization": "Bearer \(apiKey.trimmingCharacters(in: .whitespacesAndNewlines))"]
    }

    /// Encodes the initial `session.update` message for transcription-only mode.
    /// - Parameters:
    ///   - model: Transcription model (`gpt-live-transcribe`, `gpt-4o-transcribe`, …).
    ///   - language: Optional BCP-47 hint normalized to ISO-639-1 (`zh`, `en`, …).
    /// - Returns: JSON text frame.
    public static func sessionUpdateJSON(model: String, language: String?) -> String {
        var transcription: [String: Any] = ["model": model]
        if let code = normalizedISO639(language) {
            transcription["language"] = code
        }
        let payload: [String: Any] = [
            "type": "session.update",
            "session": [
                "type": "transcription",
                "audio": [
                    "input": [
                        "format": [
                            "type": "audio/pcm",
                            "rate": 24_000,
                        ],
                        "transcription": transcription,
                        "turn_detection": NSNull(),
                    ] as [String: Any],
                ],
            ] as [String: Any],
        ]
        return encodeJSON(payload)
    }

    /// Encodes one `input_audio_buffer.append` frame with 24 kHz base64 PCM16 audio.
    public static func audioAppendJSON(base64PCM24k: String) -> String {
        encodeJSON([
            "type": "input_audio_buffer.append",
            "audio": base64PCM24k,
        ])
    }

    /// Encodes an `input_audio_buffer.commit` frame to finalize the buffered utterance.
    public static func audioCommitJSON() -> String {
        encodeJSON(["type": "input_audio_buffer.commit"])
    }

    /// Encodes an `input_audio_buffer.clear` frame to discard un-committed noise or blips.
    public static func audioClearJSON() -> String {
        encodeJSON(["type": "input_audio_buffer.clear"])
    }

    /// Parses one JSON server message from the OpenAI Realtime socket.
    /// - Parameter text: Raw JSON frame text.
    /// - Returns: Normalized event, or `nil` for informational bookkeeping events.
    public static func parseServerEvent(_ text: String) -> CloudLiveServerEvent? {
        guard let obj = decodeObject(text), let type = obj["type"] as? String else { return nil }
        switch type {
        case "session.updated":
            return .ready
        case "conversation.item.input_audio_transcription.delta":
            let delta = obj["delta"] as? String ?? ""
            return delta.isEmpty ? nil : .partialDelta(delta)
        case "conversation.item.input_audio_transcription.completed":
            let transcript = (obj["transcript"] as? String ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
            let usage = obj["usage"] as? [String: Any]
            let inTok = intValue(usage?["input_tokens"])
            let outTok = intValue(usage?["output_tokens"])
            return .finalTranscript(text: transcript, inputTokens: inTok, outputTokens: outTok)
        case "conversation.item.input_audio_transcription.failed", "error":
            let errObj = obj["error"] as? [String: Any]
            let msg = errObj?["message"] as? String ?? obj["message"] as? String ?? type
            return .error(msg)
        default:
            return nil
        }
    }

    private static func normalizedISO639(_ raw: String?) -> String? {
        guard let raw else { return nil }
        let clean = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !clean.isEmpty, clean.caseInsensitiveCompare("auto") != .orderedSame else { return nil }
        return clean.split(whereSeparator: { $0 == "-" || $0 == "_" }).first.map(String.init)
    }
}

/// Pure JSON wire codec for Google Gemini Live (`BidiGenerateContent` with `gemini-3.5-transcribe-live`).
public enum GeminiLiveCodec {
    /// Gemini Live WebSocket endpoint carrying the API key as a URL query parameter.
    /// - Parameter apiKey: Google AI Studio / Gemini API key.
    /// - Returns: WebSocket URL.
    public static func endpointURL(apiKey: String) -> URL? {
        var comps = URLComponents(
            string: "wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent"
        )
        comps?.queryItems = [
            URLQueryItem(name: "key", value: apiKey.trimmingCharacters(in: .whitespacesAndNewlines)),
        ]
        return comps?.url
    }

    /// Encodes the mandatory initial `setup` frame (disables server VAD so local Silero VAD gates silence).
    /// - Parameter model: Gemini Live model id (with or without `"models/"` prefix).
    /// - Returns: JSON text frame.
    public static func setupJSON(model: String) -> String {
        let fullModel = model.hasPrefix("models/") ? model : "models/\(model)"
        let payload: [String: Any] = [
            "setup": [
                "model": fullModel,
                "generationConfig": [
                    "responseModalities": ["TEXT"],
                ],
                "realtimeInputConfig": [
                    "automaticActivityDetection": [
                        "disabled": true,
                    ],
                ],
            ] as [String: Any],
        ]
        return encodeJSON(payload)
    }

    /// Encodes a manual VAD speech-start frame.
    public static func activityStartJSON() -> String {
        encodeJSON(["realtimeInput": ["activityStart": [String: Any]()]])
    }

    /// Encodes a 16 kHz PCM16LE base64 audio chunk frame.
    public static func audioChunkJSON(base64PCM16k: String) -> String {
        encodeJSON([
            "realtimeInput": [
                "mediaChunks": [[
                    "mimeType": "audio/pcm;rate=16000",
                    "data": base64PCM16k,
                ]],
            ],
        ])
    }

    /// Encodes a manual VAD speech-end frame to finalize the utterance.
    public static func activityEndJSON() -> String {
        encodeJSON(["realtimeInput": ["activityEnd": [String: Any]()]])
    }

    /// Parses one JSON server message from the Gemini Live socket.
    /// - Parameter text: Raw JSON frame text.
    /// - Returns: Normalized event, or `nil` if the frame carries no actionable update.
    public static func parseServerEvent(_ text: String) -> CloudLiveServerEvent? {
        guard let obj = decodeObject(text) else { return nil }
        if let errObj = obj["error"] as? [String: Any] {
            let msg = errObj["message"] as? String ?? "Gemini Live error"
            return .error(msg)
        }
        if obj["setupComplete"] != nil {
            return .ready
        }
        let usage = obj["usageMetadata"] as? [String: Any]
        let inTok = intValue(usage?["promptTokenCount"])
        let outTok = intValue(usage?["candidatesTokenCount"])

        if let server = obj["serverContent"] as? [String: Any] {
            if let final = server["inputTranscription"] as? [String: Any],
               let text = final["text"] as? String {
                let clean = text.trimmingCharacters(in: .whitespacesAndNewlines)
                return .finalTranscript(text: clean, inputTokens: inTok, outputTokens: outTok)
            }
            if let interim = server["interimInputTranscription"] as? [String: Any],
               let text = interim["text"] as? String {
                let clean = text.trimmingCharacters(in: .whitespacesAndNewlines)
                return clean.isEmpty ? nil : .partialReplace(clean)
            }
        }
        if inTok > 0 || outTok > 0 {
            return .usage(inputTokens: inTok, outputTokens: outTok)
        }
        return nil
    }
}

private func decodeObject(_ text: String) -> [String: Any]? {
    guard let data = text.data(using: .utf8),
          let raw = try? JSONSerialization.jsonObject(with: data),
          let dict = raw as? [String: Any] else { return nil }
    return dict
}

private func encodeJSON(_ obj: [String: Any]) -> String {
    guard let data = try? JSONSerialization.data(withJSONObject: obj, options: [.sortedKeys, .withoutEscapingSlashes]),
          let s = String(data: data, encoding: .utf8) else { return "{}" }
    return s
}

private func intValue(_ any: Any?) -> Int {
    if let n = any as? Int { return n }
    if let n = any as? NSNumber { return n.intValue }
    return 0
}

/// Classifies cloud errors that warrant immediate fallback to local ASR rather than retries.
public enum CloudAuthErrorDetector {
    /// Returns `true` when HTTP status or message indicates invalid credentials, denied access,
    /// or exhausted account quota/credits.
    public static func isAuthOrQuotaFailure(statusCode: Int?, message: String) -> Bool {
        if statusCode == 401 || statusCode == 403 {
            return true
        }
        let lower = message.lowercased()
        if statusCode == 429
            && (lower.contains("insufficient_quota") || lower.contains("credit_balance_exhausted")) {
            return true
        }
        let markers = [
            "invalid_api_key",
            "incorrect api key",
            "unauthorized",
            "api_key_invalid",
            "api key not valid",
            "unauthenticated",
            "permission_denied",
            "invalid authentication",
            "insufficient_quota",
            "credit_balance_exhausted",
        ]
        return markers.contains { lower.contains($0) }
    }
}
