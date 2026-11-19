import Foundation

/// Local speech-gating and chunk-coalescing state machine for cloud live ASR.
///
/// Tracks per-chunk Silero VAD probabilities and RMS amplitude so cloud sockets/endpoints
/// only receive confirmed speech:
/// 1. Delays opening a cloud utterance until speech is confirmed beyond short-blip duration.
/// 2. Requires signal RMS above a configurable floor (blocks digital silence and faint hiss).
/// 3. Pauses uploading during trailing VAD hangover silence instead of streaming 500 ms of silence.
/// 4. Coalesces small 50 ms tick slices into compact ~200 ms frames while streaming.
/// 5. Aborts any orphaned open utterance if the segmenter dropped it without emitting a final.
public struct CloudUploadGate: Sendable {
    /// Tunable thresholds for VAD + energy gating and chunk coalescing.
    public struct Config: Sendable, Equatable {
        /// Stream sample rate in Hz (always 16 kHz after `StreamingResampler`).
        public var sampleRate: Int
        /// Audio kept before `activeStart` when opening a cloud utterance so initial plosives survive.
        public var preRollSamples: Int
        /// Minimum samples from `activeStart` before streaming begins (> `UtteranceSegmenter.minFinalDuration`).
        public var minSpeechConfirmSamples: Int
        /// Silero probability threshold to enter voiced state.
        public var speechOnsetProb: Float
        /// Silero probability threshold to remain in voiced state.
        public var speechOffsetProb: Float
        /// Minimum chunk RMS amplitude (`-50.5 dBFS` at `0.003`) required to treat audio as speech.
        public var minActiveRMS: Float
        /// Minimum samples coalesced per incremental WebSocket upload frame (`200 ms` at `16 kHz`).
        public var minStreamChunkSamples: Int
        /// Short cushion (`64 ms`) past the last voiced VAD chunk so trailing consonants are not clipped.
        public var trailingHoldPaddingSamples: Int

        /// Creates a gating configuration.
        public init(
            sampleRate: Int = 16_000,
            preRollSamples: Int = 1_600,
            minSpeechConfirmSamples: Int = 5_600,
            speechOnsetProb: Float = 0.50,
            speechOffsetProb: Float = 0.35,
            minActiveRMS: Float = 0.003,
            minStreamChunkSamples: Int = 3_200,
            trailingHoldPaddingSamples: Int = 1_024
        ) {
            self.sampleRate = sampleRate
            self.preRollSamples = preRollSamples
            self.minSpeechConfirmSamples = minSpeechConfirmSamples
            self.speechOnsetProb = speechOnsetProb
            self.speechOffsetProb = speechOffsetProb
            self.minActiveRMS = minActiveRMS
            self.minStreamChunkSamples = minStreamChunkSamples
            self.trailingHoldPaddingSamples = trailingHoldPaddingSamples
        }
    }

    /// Action to perform during a live streaming tick.
    public enum StreamingStep: Equatable, Sendable {
        case idle
        case abort(id: Int)
        case upload(id: Int, range: Range<Int>, isFirstChunk: Bool)
    }

    /// Action to perform when `UtteranceSegmenter` finalizes an utterance.
    public enum FinalizeStep: Equatable, Sendable {
        case skip
        case commit(tailRange: Range<Int>?, totalUploadedSamples: Int, wasOpened: Bool)
    }

    public let config: Config
    private var inSpeech = false
    private var lastVoicedSample = 0
    private var openUtteranceId: Int?
    private var fedUntil: [Int: Int] = [:]
    private var uploadedSamples: [Int: Int] = [:]

    /// Creates a gate with the given configuration.
    public init(config: Config = Config()) {
        self.config = config
    }

    /// Updates hysteresis and trailing-silence freeze state after one Silero VAD chunk runs.
    /// - Parameters:
    ///   - endSample: Stream sample index immediately after this chunk.
    ///   - speechProb: Silero probability `0...1` returned for this chunk.
    ///   - rms: Root-mean-square amplitude of the chunk.
    public mutating func observeVADChunk(endSample: Int, speechProb: Float, rms: Float) {
        let exitRMS = config.minActiveRMS * 0.5
        if inSpeech {
            if speechProb < config.speechOffsetProb || rms < exitRMS {
                inSpeech = false
            }
        } else if speechProb >= config.speechOnsetProb && rms >= config.minActiveRMS {
            inSpeech = true
        }
        if inSpeech {
            lastVoicedSample = max(lastVoicedSample, endSample)
        }
    }

    /// Computes what incremental streaming action (if any) should happen on this tick.
    /// - Parameters:
    ///   - activeId: Current `UtteranceSegmenter.activeID`.
    ///   - activeStart: Current `UtteranceSegmenter.activeStart`.
    ///   - processed: Total 16 kHz samples fed through VAD so far.
    ///   - audioBase: Oldest sample still retained in the ring buffer.
    ///   - heldFinalIds: Utterance ids that already closed in the segmenter and await Sortformer.
    /// - Returns: Whether to idle, abort a dropped utterance buffer, or upload a slice.
    public mutating func nextStreamingStep(
        activeId: Int?,
        activeStart: Int?,
        processed: Int,
        audioBase: Int,
        heldFinalIds: Set<Int>
    ) -> StreamingStep {
        if let open = openUtteranceId, open != activeId, !heldFinalIds.contains(open) {
            openUtteranceId = nil
            fedUntil.removeValue(forKey: open)
            uploadedSamples.removeValue(forKey: open)
            return .abort(id: open)
        }
        guard heldFinalIds.isEmpty, let id = activeId, let start = activeStart else {
            return .idle
        }
        let voicedCeiling = min(processed, max(start, lastVoicedSample + config.trailingHoldPaddingSamples))
        if let fed = fedUntil[id] {
            let lo = max(audioBase, fed)
            guard voicedCeiling - lo >= config.minStreamChunkSamples else {
                return .idle
            }
            fedUntil[id] = voicedCeiling
            uploadedSamples[id, default: 0] += (voicedCeiling - lo)
            return .upload(id: id, range: lo..<voicedCeiling, isFirstChunk: false)
        }

        // Delay opening until sustained voiced audio is confirmed past short-blip length.
        guard processed - start >= config.minSpeechConfirmSamples,
              lastVoicedSample > start
        else {
            return .idle
        }
        let lo = max(audioBase, start - config.preRollSamples)
        guard voicedCeiling > lo else {
            return .idle
        }
        openUtteranceId = id
        fedUntil[id] = voicedCeiling
        uploadedSamples[id] = voicedCeiling - lo
        return .upload(id: id, range: lo..<voicedCeiling, isFirstChunk: true)
    }

    /// Closes cloud accounting for a finalized utterance and returns any remaining tail range.
    /// - Parameters:
    ///   - id: Finalized utterance id.
    ///   - start: Utterance start sample.
    ///   - end: Utterance end sample (excludes trailing VAD silence).
    ///   - audioBase: Oldest buffered sample available.
    ///   - maxRMS: Peak chunk RMS observed across this utterance.
    /// - Returns: Whether to skip (silent/sub-floor) or commit `(tailRange, totalUploadedSamples, wasOpened)`.
    public mutating func finalize(
        id: Int,
        start: Int,
        end: Int,
        audioBase: Int,
        maxRMS: Float
    ) -> FinalizeStep {
        if let fed = fedUntil.removeValue(forKey: id) {
            if openUtteranceId == id {
                openUtteranceId = nil
            }
            let lo = max(audioBase, fed)
            let tail: Range<Int>? = end > lo ? (lo..<end) : nil
            let total = (uploadedSamples.removeValue(forKey: id) ?? 0) + (tail?.count ?? 0)
            return .commit(tailRange: tail, totalUploadedSamples: total, wasOpened: true)
        }
        uploadedSamples.removeValue(forKey: id)
        guard maxRMS >= config.minActiveRMS else {
            return .skip
        }
        let lo = max(audioBase, start - config.preRollSamples)
        let hi = max(lo, end)
        guard hi > lo else {
            return .skip
        }
        return .commit(tailRange: lo..<hi, totalUploadedSamples: hi - lo, wasOpened: false)
    }

    /// Computes NaN-safe RMS amplitude over a Mono Float32 buffer.
    public static func rms(_ samples: [Float]) -> Float {
        guard !samples.isEmpty else { return 0 }
        var sumSq: Double = 0
        var count = 0
        for s in samples where s.isFinite {
            let d = Double(s)
            sumSq += d * d
            count += 1
        }
        guard count > 0 else { return 0 }
        return Float((sumSq / Double(count)).squareRoot())
    }
}
