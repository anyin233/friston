import AudioCommon
import Foundation
import MeetingCore
import MLX
@preconcurrency import Qwen3ASR

/// Qwen3-ASR decoding for live partials that forces a known transcript prefix.
///
/// `Qwen3ASRModel.transcribe` decodes every partial from the first token, so each refresh costs
/// the whole utterance and may rewrite any word. This reuses the model's public encoder, decoder
/// and tokenizer but builds the prompt itself: the previous hypothesis minus its last
/// ``rollback`` tokens is appended after `<asr_text>` and prefilled in the same pass as the
/// audio, so only the tail is decoded token by token (Foresight's `decoder_prefill_backoff`;
/// antirez/qwen-asr uses the same 5-token rollback). Finals keep using `transcribe` from
/// scratch, so their accuracy is unchanged.
///
/// Not thread-safe: run only inside ``MLXGate``, like the model it wraps.
final class PrefixDecoder: @unchecked Sendable {
    /// One decoded partial.
    struct Hypothesis {
        /// Generated tokens without EOS (the `language X<asr_text>` header included when auto-detected).
        var tokens: [Int32]
        /// Caption text (header stripped, trimmed).
        var text: String
        /// Leading Unicode scalars of `text` the next partial will force.
        var stableChars: Int
        /// Tokens that were forced rather than decoded this time.
        var forced: Int
    }

    let model: Qwen3ASRModel
    private let tokenizer: Qwen3Tokenizer
    /// Tokens of the previous hypothesis given back to the decoder on each partial.
    let rollback: Int

    /// Creates a decoder.
    /// - Parameters:
    ///   - model: Loaded live model (its encoder and decoder are shared, not copied).
    ///   - tokenizer: The model's tokenizer.
    ///   - rollback: Tail tokens re-decoded on every partial.
    init(model: Qwen3ASRModel, tokenizer: Qwen3Tokenizer, rollback: Int = 5) {
        self.model = model
        self.tokenizer = tokenizer
        self.rollback = rollback
    }

    /// Loads the tokenizer that sits next to the model weights.
    /// - Parameter directory: Model directory containing `vocab.json` and `merges.txt`.
    /// - Returns: The tokenizer.
    /// - Throws: When the files are missing or unreadable (the error names the path).
    static func loadTokenizer(from directory: URL) throws -> Qwen3Tokenizer {
        let vocab = directory.appendingPathComponent("vocab.json")
        guard FileManager.default.fileExists(atPath: vocab.path) else {
            throw PrefixDecoderError.missingTokenizer(vocab.path)
        }
        let tokenizer = Qwen3Tokenizer()
        try tokenizer.load(from: vocab)
        return tokenizer
    }

    /// Transcribes `audio`, forcing most of `previous`.
    /// - Parameters:
    ///   - audio: 16 kHz mono audio of the utterance so far.
    ///   - language: Qwen language name (e.g. `Chinese`), or `nil` to auto-detect.
    ///   - previous: Tokens of the previous partial of this utterance (empty for the first).
    ///   - maxTokens: Cap on the whole hypothesis (forced plus new tokens).
    /// - Returns: The new hypothesis.
    func decode(audio: [Float], language: String?, previous: [Int32], maxTokens: Int) -> Hypothesis {
        guard let decoder = model.textDecoder else {
            return Hypothesis(tokens: [], text: "", stableChars: 0, forced: 0)
        }
        let asrText = Int32(Qwen3ASRTokens.asrTextTokenId)
        let forcedCount = PrefixRollback.forcedCount(previous: previous, rollback: rollback,
                                                     asrTextToken: asrText, headerExpected: language == nil)
        let forced = Array(previous.prefix(forcedCount))

        let mel = model.featureExtractor.process(audio, sampleRate: CaptionStream.rate)
        let audioEmbeds = model.audioEncoder(mel.expandedDimensions(axis: 0)).expandedDimensions(axis: 0)
        let (before, after) = promptTokens(language: language)
        let ids = MLXArray(before + after + forced).expandedDimensions(axis: 0)
        let embeds = decoder.embedTokens(ids)
        let split = before.count
        let input = concatenated([embeds[0..., 0..<split, 0...], audioEmbeds.asType(embeds.dtype),
                                  embeds[0..., split..., 0...]], axis: 1)

        var (hidden, cache) = decoder(inputsEmbeds: input, cache: nil)
        var tokens = forced
        let eos = Int32(Qwen3ASRTokens.eosTokenId)
        while tokens.count < maxTokens {
            let logits = decoder.embedTokens.asLinear(hidden[0..., (-1)..., 0...])
            // int32 like speech-swift's greedy loop: quantized embedding lookups differ on uint32.
            let next = argMax(logits, axis: -1).squeezed().asType(.int32)
            let token = next.item(Int32.self)
            if token == eos { break }
            tokens.append(token)
            (hidden, cache) = decoder(inputsEmbeds: decoder.embedTokens(next.reshaped([1, 1])), cache: cache)
        }
        return hypothesis(tokens: tokens, forced: forcedCount, headerExpected: language == nil)
    }

    /// Prompt ids around the audio placeholder, matching `Qwen3ASRModel`'s own prompt without context.
    /// - Parameter language: Language hint, or `nil` to let the model generate the header.
    /// - Returns: Ids before the audio embeddings and ids after them (ending with `<asr_text>` when forced).
    private func promptTokens(language: String?) -> (before: [Int32], after: [Int32]) {
        let t = Qwen3ASRTokens.self
        let before = [t.imStartTokenId, t.systemTokenId, t.newlineTokenId, t.imEndTokenId, t.newlineTokenId,
                      t.imStartTokenId, t.userTokenId, t.newlineTokenId, t.audioStartTokenId].map { Int32($0) }
        var after = [t.audioEndTokenId, t.imEndTokenId, t.newlineTokenId,
                     t.imStartTokenId, t.assistantTokenId, t.newlineTokenId].map { Int32($0) }
        if let language {
            after += tokenizer.encode("language \(language)").map { Int32($0) }
        }
        // The model's own prompt always ends with <asr_text>; with auto-detection it then emits
        // `language X<asr_text>` again, which `hypothesis` strips.
        after.append(Int32(t.asrTextTokenId))
        return (before, after)
    }

    /// Decodes tokens into caption text and works out how much of it is stable.
    /// - Parameters:
    ///   - tokens: Generated tokens without EOS.
    ///   - forced: How many were forced.
    ///   - headerExpected: Auto-detected language.
    /// - Returns: The hypothesis.
    private func hypothesis(tokens: [Int32], forced: Int, headerExpected: Bool) -> Hypothesis {
        let asrText = Int32(Qwen3ASRTokens.asrTextTokenId)
        let header = PrefixRollback.headerLength(tokens, asrTextToken: asrText)
        let body = { (range: Range<Int>) -> String in
            self.tokenizer.decode(tokens: tokens[range].map { Int($0) })
        }
        let text = header <= tokens.count ? body(header..<tokens.count).trimmingCharacters(in: .whitespacesAndNewlines) : ""
        let keep = PrefixRollback.forcedCount(previous: tokens, rollback: rollback, asrTextToken: asrText,
                                              headerExpected: headerExpected)
        let stable = keep > header ? PrefixRollback.stableScalars(text: text, stableText: body(header..<keep)) : 0
        return Hypothesis(tokens: tokens, text: text, stableChars: stable, forced: forced)
    }
}

/// Why the prefix decoder could not be set up.
enum PrefixDecoderError: Error, CustomStringConvertible {
    case missingTokenizer(String)

    var description: String {
        switch self {
        case .missingTokenizer(let path): "tokenizer not found at \(path)"
        }
    }
}
