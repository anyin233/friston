import Foundation

/// Direction evidence attached to a final caption. Mirrors `mm_core::SpatialCue`, whose
/// serde field names are snake_case.
public struct CaptionSpatialCue: Codable, Sendable, Equatable {
    /// Calibrated azimuth in degrees; `nil` because live cues are geometry-free.
    public var azimuthDeg: Float?
    /// Median inter-channel delay over the utterance, in samples at the mic rate.
    public var tdoaSamples: Float
    /// 0…1 reliability.
    public var confidence: Float

    private enum CodingKeys: String, CodingKey {
        case azimuthDeg = "azimuth_deg"
        case tdoaSamples = "tdoa_samples"
        case confidence
    }

    /// Creates a cue.
    /// - Parameters: see the stored properties.
    public init(azimuthDeg: Float?, tdoaSamples: Float, confidence: Float) {
        self.azimuthDeg = azimuthDeg
        self.tdoaSamples = tdoaSamples
        self.confidence = confidence
    }
}

/// `caption/partial` payload: a volatile hypothesis that replaces earlier partials of the same utterance.
public struct CaptionPartialEvent: Codable, Sendable, Equatable {
    public var sessionId: String
    public var utteranceId: Int
    public var origin: AudioOrigin
    /// Milliseconds since capture start.
    public var startMs: Int
    public var text: String
    public var speaker: SpeakerLabel

    /// Creates the payload.
    /// - Parameters: see the stored properties.
    public init(sessionId: String, utteranceId: Int, origin: AudioOrigin, startMs: Int, text: String, speaker: SpeakerLabel) {
        self.sessionId = sessionId
        self.utteranceId = utteranceId
        self.origin = origin
        self.startMs = startMs
        self.text = text
        self.speaker = speaker
    }
}

/// `caption/final` payload: stable text that retires the utterance's partial.
public struct CaptionFinalEvent: Codable, Sendable, Equatable {
    public var sessionId: String
    public var utteranceId: Int
    public var origin: AudioOrigin
    public var startMs: Int
    public var endMs: Int
    /// May be empty when a partial was shown but the full utterance turned out to be noise.
    public var text: String
    /// BCP-47 code: the preset hint, or guessed from the script of `text`.
    public var language: String?
    public var speaker: SpeakerLabel
    public var spatial: CaptionSpatialCue?

    /// Creates the payload.
    /// - Parameters: see the stored properties.
    public init(sessionId: String, utteranceId: Int, origin: AudioOrigin, startMs: Int, endMs: Int, text: String,
                language: String?, speaker: SpeakerLabel, spatial: CaptionSpatialCue?) {
        self.sessionId = sessionId
        self.utteranceId = utteranceId
        self.origin = origin
        self.startMs = startMs
        self.endMs = endMs
        self.text = text
        self.language = language
        self.speaker = speaker
        self.spatial = spatial
    }
}

/// `speaker/relabel` payload: every caption of this session labelled `from` now reads `to`.
public struct SpeakerRelabelEvent: Codable, Sendable, Equatable {
    public var sessionId: String
    public var from: SpeakerLabel
    public var to: SpeakerLabel

    /// Creates the payload.
    /// - Parameters: see the stored properties.
    public init(sessionId: String, from: SpeakerLabel, to: SpeakerLabel) {
        self.sessionId = sessionId
        self.from = from
        self.to = to
    }
}

/// Maps caption language hints onto Qwen3-ASR's prompt vocabulary and guesses languages from text.
public enum ASRLanguage {
    /// BCP-47 primary subtag → Qwen3-ASR language name (the model's `support_languages`).
    static let names: [String: String] = [
        "zh": "Chinese", "en": "English", "yue": "Cantonese", "ar": "Arabic", "de": "German",
        "fr": "French", "es": "Spanish", "pt": "Portuguese", "id": "Indonesian", "it": "Italian",
        "ko": "Korean", "ru": "Russian", "th": "Thai", "vi": "Vietnamese", "ja": "Japanese",
        "tr": "Turkish", "hi": "Hindi", "ms": "Malay", "nl": "Dutch", "sv": "Swedish", "da": "Danish",
        "fi": "Finnish", "pl": "Polish", "cs": "Czech", "fil": "Filipino", "tl": "Filipino",
        "fa": "Persian", "el": "Greek", "ro": "Romanian", "hu": "Hungarian", "mk": "Macedonian",
    ]

    /// Resolves a caption-language hint.
    /// - Parameter code: BCP-47 tag such as `zh`, `zh-Hans-CN` or `en-US`; `nil`/`auto` for detection.
    /// - Returns: The Qwen3-ASR language name, or `nil` to let the model detect the language
    ///   (also for languages it does not support, where forcing a wrong hint would be worse).
    public static func qwenName(for code: String?) -> String? {
        guard let code else { return nil }
        let primary = code.lowercased().split(whereSeparator: { $0 == "-" || $0 == "_" }).first.map(String.init) ?? ""
        return names[primary]
    }

    /// Normalizes a hint to its primary subtag when the model supports it.
    /// - Parameter code: BCP-47 tag.
    /// - Returns: e.g. `zh` for `zh-Hans-CN`, or `nil` when unsupported/auto.
    public static func primaryCode(for code: String?) -> String? {
        guard let code, qwenName(for: code) != nil else { return nil }
        return code.lowercased().split(whereSeparator: { $0 == "-" || $0 == "_" }).first.map(String.init)
    }

    /// Decoder token budget for an utterance.
    ///
    /// Greedy decoding occasionally loops ("不，不，不，…") on noisy or overlapped audio. A
    /// repetition penalty would fix that but disables speech-swift's fast greedy path, so the
    /// output length is capped instead: 15 tokens per second is about three times normal
    /// speaking rate in both Chinese (≈ 5 characters/s) and English (≈ 3 words/s).
    /// - Parameter seconds: Utterance duration.
    /// - Returns: `maxTokens` for Qwen3-ASR, at most its default of 448.
    public static func maxTokens(forSeconds seconds: Double) -> Int {
        min(448, 16 + Int((15 * max(0, seconds)).rounded(.up)))
    }

    /// Guesses the language of a transcript from its script.
    ///
    /// Qwen3-ASR detects the language internally but speech-swift strips that tag from the
    /// output, so the script is the cheapest reliable signal for the languages meetings here use.
    /// - Parameter text: Transcript text.
    /// - Returns: `ja` (any kana), `ko` (Hangul), `zh` (Han without kana), `en` (Latin letters only), or `nil`.
    public static func guess(fromText text: String) -> String? {
        var han = 0, kana = 0, hangul = 0, latin = 0
        for scalar in text.unicodeScalars {
            switch scalar.value {
            case 0x3040...0x30FF, 0x31F0...0x31FF: kana += 1
            case 0xAC00...0xD7AF, 0x1100...0x11FF, 0x3130...0x318F: hangul += 1
            case 0x4E00...0x9FFF, 0x3400...0x4DBF, 0xF900...0xFAFF: han += 1
            case 0x41...0x5A, 0x61...0x7A: latin += 1
            default: break
            }
        }
        if kana > 0 { return "ja" }
        if hangul > 0 && hangul >= han { return "ko" }
        if han > 0 { return "zh" }
        if latin > 0 { return "en" }
        return nil
    }
}
