import Foundation

/// Draft captions from a streaming recognizer (sherpa-onnx Zipformer).
///
/// A transducer only appends tokens and occasionally revises the newest one, so everything
/// but the last `volatileTail` characters is shown as stable.
public enum DraftCaption {
    /// Leading Unicode scalars of a draft that the UI may show as settled.
    /// - Parameters:
    ///   - text: Draft text.
    ///   - volatileTail: Trailing scalars that may still change.
    /// - Returns: `max(0, scalars − volatileTail)`.
    public static func stableScalars(_ text: String, volatileTail: Int = 1) -> Int {
        max(0, text.unicodeScalars.count - max(0, volatileTail))
    }
}

/// Decides whether a caption stream shows draft captions or Qwen3-ASR partials.
///
/// Measured on an M5 Pro, prefix-rollback Qwen3-ASR partials appear sooner than the streaming
/// Zipformer draft (first caption 0.70 s against 1.10 s; the draft waits for a 320 ms chunk
/// plus lookahead). The draft wins only when the GPU cannot keep up, so in `auto` mode a stream
/// switches to it, for the rest of the session, once the partial refresh interval stays at or
/// above `switchInterval` (a slow Mac, or the embedding engine competing for the GPU).
public struct DraftPolicy: Sendable, Equatable {
    public enum Mode: Sendable, Equatable {
        /// Qwen3-ASR partials until they get too slow, then the draft.
        case auto
        /// Always the draft (for comparisons).
        case always
    }

    public let mode: Mode
    /// Partial refresh interval (seconds) that counts as "cannot keep up".
    public let switchInterval: Double
    /// Whether new utterances use the draft.
    public private(set) var useDraft: Bool

    /// Creates a policy.
    /// - Parameters:
    ///   - mode: Auto or always.
    ///   - switchInterval: Threshold on ``PartialPacer/interval``, seconds.
    public init(mode: Mode, switchInterval: Double = 0.6) {
        self.mode = mode
        self.switchInterval = switchInterval
        useDraft = mode == .always
    }

    /// Feeds the pacer's current interval after a Qwen3-ASR partial.
    /// - Parameter partialInterval: Seconds between partials the pacer settled on.
    /// - Returns: `true` exactly when this call switched the stream to the draft.
    public mutating func observe(partialInterval: Double) -> Bool {
        guard !useDraft, partialInterval.isFinite, partialInterval >= switchInterval else { return false }
        useDraft = true
        return true
    }
}

/// Bookkeeping for prefix-rollback decoding of live partials.
///
/// Re-transcribing a growing utterance from scratch makes every partial cost the whole sentence
/// and lets any earlier word change, so captions jump instead of growing. Like Foresight's
/// `decoder_prefill_backoff`, each partial instead forces the previous hypothesis minus its
/// last `rollback` tokens as a known prefix and only decodes the tail. The forced part is what
/// the UI may show as stable; only the tail can still be rewritten.
///
/// Pure token arithmetic (no model), so it is unit-tested apart from MLX.
public enum PrefixRollback {
    /// Tokens of the generated header (`language X<asr_text>`) when the model detects the
    /// language itself; 0 when no `<asr_text>` was generated (the prompt already ends with it).
    /// - Parameters:
    ///   - tokens: Generated tokens of a hypothesis, without EOS.
    ///   - asrTextToken: Id of `<asr_text>`.
    /// - Returns: Index just after `<asr_text>`, or 0.
    public static func headerLength(_ tokens: [Int32], asrTextToken: Int32) -> Int {
        tokens.firstIndex(of: asrTextToken).map { $0 + 1 } ?? 0
    }

    /// How many leading tokens of the previous hypothesis to force on the next partial.
    ///
    /// The header is always kept once complete (it only names the language). Without a complete
    /// header and with auto-detection, nothing is forced: the model has not committed yet.
    /// - Parameters:
    ///   - previous: Tokens of the previous partial of the same utterance, without EOS.
    ///   - rollback: Tokens to give back to the decoder at the end of the hypothesis.
    ///   - asrTextToken: Id of `<asr_text>`.
    ///   - headerExpected: `true` when the language is auto-detected, so a header must be generated.
    /// - Returns: Prefix length, `0...previous.count`.
    public static func forcedCount(previous: [Int32], rollback: Int, asrTextToken: Int32,
                                   headerExpected: Bool) -> Int {
        let header = headerLength(previous, asrTextToken: asrTextToken)
        if headerExpected && header == 0 { return 0 }
        return min(previous.count, max(header, previous.count - max(0, rollback)))
    }

    /// Length, in Unicode scalars, of the start of `text` that the next partial will keep.
    ///
    /// `stableText` is the decoded forced prefix of the *next* partial (this hypothesis minus its
    /// rollback). Byte-level BPE can split a Chinese character across tokens, so a dangling
    /// replacement character is dropped, and only the part both strings share counts.
    /// - Parameters:
    ///   - text: Full caption text shown now.
    ///   - stableText: Decoded text of the tokens that will be forced next time.
    /// - Returns: Scalar count, `0...text.unicodeScalars.count`.
    public static func stableScalars(text: String, stableText: String) -> Int {
        var stable = Array(stableText.unicodeScalars.drop { CharacterSet.whitespacesAndNewlines.contains($0) })
        while stable.last == "\u{FFFD}" { stable.removeLast() }
        var count = 0
        for (a, b) in zip(text.unicodeScalars, stable) {
            guard a == b else { break }
            count += 1
        }
        return count
    }
}
