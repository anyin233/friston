import Foundation

/// Decides how often a stream refreshes its volatile caption, from how long partials take.
///
/// Live captions are pseudo-streaming: each partial re-decodes the utterance so far. A fixed
/// 1 s refresh left the caption bar up to ~1.2 s behind the speaker on a fast Mac, while on a
/// slow one it could not keep up at all. The pacer refreshes as often as `minInterval` allows
/// when decoding is cheap and backs off so partials use at most `1 / dutyFactor` of the GPU,
/// leaving room for finals, Sortformer and the second stream.
public struct PartialPacer: Sendable {
    /// Fastest refresh, in seconds of speech. Below ~0.3 s the text barely changes between partials.
    public var minInterval: Double
    /// Slowest refresh; beyond this the caption feels stuck.
    public var maxInterval: Double
    /// Interval = smoothed decode time × this factor.
    public var dutyFactor: Double
    /// Weight of the newest sample in the moving average.
    public var smoothing: Double
    private let initial: Double
    /// Smoothed decode time of one partial, in seconds; `nil` before the first measurement.
    public private(set) var averageDecode: Double?

    /// Creates a pacer.
    /// - Parameters:
    ///   - initial: Interval until the first decode has been timed.
    ///   - minInterval: Lower bound, seconds.
    ///   - maxInterval: Upper bound, seconds.
    ///   - dutyFactor: Interval per second of decode time.
    ///   - smoothing: EMA weight of a new sample (0…1).
    public init(initial: Double = 0.5, minInterval: Double = 0.3, maxInterval: Double = 1.5,
                dutyFactor: Double = 2.5, smoothing: Double = 0.3) {
        self.initial = initial
        self.minInterval = minInterval
        self.maxInterval = maxInterval
        self.dutyFactor = dutyFactor
        self.smoothing = smoothing
    }

    /// Records how long one partial took to decode.
    /// - Parameter decodeSeconds: Wall time of the transcription; negative or non-finite values are ignored.
    public mutating func record(decodeSeconds: Double) {
        guard decodeSeconds.isFinite, decodeSeconds >= 0 else { return }
        averageDecode = averageDecode.map { $0 + smoothing * (decodeSeconds - $0) } ?? decodeSeconds
    }

    /// Seconds of speech between consecutive partials.
    public var interval: Double {
        guard let averageDecode else { return initial }
        return min(maxInterval, max(minInterval, averageDecode * dutyFactor))
    }
}

/// Finds pauses and quiet points in speech by short-time energy, for splitting long utterances.
///
/// Pure arithmetic on 16 kHz-class mono audio (no model), so it is cheap enough to run on every
/// VAD step and deterministic in tests. Levels are relative to the loud frames of the span,
/// which makes the result independent of microphone gain.
public enum PauseFinder {
    /// Frame length for the energy envelope.
    static let frameSeconds = 0.02
    /// A frame is "quiet" below this level relative to the span's 90th-percentile frame (−20 dB).
    static let quietRatio: Float = 0.1

    /// RMS per non-overlapping frame.
    /// - Parameters:
    ///   - samples: Mono audio.
    ///   - frame: Frame length in samples.
    /// - Returns: One RMS value per whole frame.
    static func envelope(_ samples: [Float], frame: Int) -> [Float] {
        guard frame > 0, samples.count >= frame else { return [] }
        return (0..<samples.count / frame).map { f in
            var sum: Float = 0
            for x in samples[f * frame..<(f + 1) * frame] { sum += x * x }
            return (sum / Float(frame)).squareRoot()
        }
    }

    /// The longest pause of at least `minPause` with speech after it; the later one on a tie.
    ///
    /// Gaps between sentences are usually longer than the pause at a comma, so the longest
    /// pause is the best guess for a sentence boundary. A quiet run that reaches the end of the
    /// span is not returned: it may be the end of the utterance, which the VAD finalizes on its own.
    /// - Parameters:
    ///   - samples: Mono audio of the span to search.
    ///   - sampleRate: Its rate.
    ///   - minPause: Shortest pause in seconds.
    /// - Returns: The pause as a sample range relative to `samples`, or `nil`.
    public static func longestPause(in samples: [Float], sampleRate: Int, minPause: Double) -> Range<Int>? {
        let frame = Int(Double(sampleRate) * frameSeconds)
        let env = envelope(samples, frame: frame)
        guard env.count >= 3 else { return nil }
        let loud = env.sorted()[env.count * 9 / 10]
        guard loud > 0 else { return nil }
        let threshold = loud * quietRatio
        let minFrames = max(1, Int((minPause / frameSeconds).rounded(.up)))
        var best: Range<Int>?
        var runStart: Int?
        for (i, level) in env.enumerated() {
            if level < threshold {
                if runStart == nil { runStart = i }
            } else if let start = runStart {
                // Speech resumed: the run is a closed pause. Skip quiet before any speech.
                // `>=` keeps the later of equally long pauses, so more text goes into the final.
                if start > 0, i - start >= minFrames, i - start >= (best.map { $0.count / frame } ?? 0) {
                    best = start * frame..<i * frame
                }
                runStart = nil
            }
        }
        return best
    }

    /// Whether a span ends in a pause of at least `minPause` after some speech.
    ///
    /// Used to show a partial at a word gap instead of waiting for the next timed refresh, so
    /// new text tends to land on word boundaries (Foresight's `interim_results_trigger_silence_ms`).
    /// - Parameters:
    ///   - samples: Mono audio, ending now; levels are relative to its loud frames.
    ///   - sampleRate: Its rate.
    ///   - minPause: Shortest trailing quiet run, in seconds.
    /// - Returns: `true` when the last `minPause` is quiet and something before it is not.
    public static func endsInPause(_ samples: [Float], sampleRate: Int, minPause: Double) -> Bool {
        let frame = Int(Double(sampleRate) * frameSeconds)
        let env = envelope(samples, frame: frame)
        let minFrames = max(1, Int((minPause / frameSeconds).rounded(.up)))
        guard env.count > minFrames else { return false }
        let loud = env.sorted()[env.count * 9 / 10]
        guard loud > 0 else { return false }
        let threshold = loud * quietRatio
        return env.suffix(minFrames).allSatisfy { $0 < threshold }
            && env.dropLast(minFrames).contains { $0 >= threshold }
    }

    /// The centre of the quietest 100 ms of a span, the least damaging place for a forced cut.
    /// - Parameters:
    ///   - samples: Mono audio of the span.
    ///   - sampleRate: Its rate.
    /// - Returns: Sample offset relative to `samples` (the end when the span is too short).
    public static func quietestPoint(in samples: [Float], sampleRate: Int) -> Int {
        let frame = Int(Double(sampleRate) * frameSeconds)
        let env = envelope(samples, frame: frame)
        let width = 5 // 5 × 20 ms
        guard env.count >= width else { return samples.count }
        var best = (index: 0, energy: Float.infinity)
        var window = env[0..<width].reduce(0, +)
        for i in 0...(env.count - width) {
            if i > 0 { window += env[i + width - 1] - env[i - 1] }
            // `<=` prefers the later of equally quiet windows, keeping more text in the final.
            if window <= best.energy { best = (i, window) }
        }
        return (best.index * frame) + width * frame / 2
    }
}

/// Decode timings of one caption stream, summarised in the engine log when captions stop.
///
/// Field logs are the only place real-meeting latency shows up, so the summary says whether a
/// Mac kept up: partial decode time against the refresh interval, and dropped partials.
public struct DecodeStats: Sendable, Equatable {
    /// Seconds per partial transcription.
    public var partials: [Double] = []
    /// Seconds per final transcription (unsplit finals only).
    public var finals: [Double] = []
    /// Partials skipped because the loop fell behind real time.
    public var skippedPartials = 0

    /// Creates empty stats.
    public init() {}

    /// Nearest-rank percentile in milliseconds.
    /// - Parameters:
    ///   - values: Seconds.
    ///   - q: Percentile, 0…100.
    /// - Returns: Milliseconds, or `nil` without values.
    static func percentileMs(_ values: [Double], _ q: Double) -> Int? {
        guard !values.isEmpty else { return nil }
        let sorted = values.sorted()
        let rank = Int((q / 100 * Double(sorted.count)).rounded(.up)) - 1
        return Int((sorted[min(max(rank, 0), sorted.count - 1)] * 1000).rounded())
    }

    /// One-line summary, e.g. `partials 120 (decode median 140 ms, p90 210 ms, 0 skipped), finals 30 (…)`.
    public var summary: String {
        func part(_ name: String, _ v: [Double]) -> String {
            guard let median = Self.percentileMs(v, 50), let p90 = Self.percentileMs(v, 90) else { return "\(name) 0" }
            return "\(name) \(v.count) (decode median \(median) ms, p90 \(p90) ms"
        }
        let p = part("partials", partials)
        let f = part("finals", finals)
        return (partials.isEmpty ? p : p + ", \(skippedPartials) skipped)") + ", " + (finals.isEmpty ? f : f + ")")
    }
}
