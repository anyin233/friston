import Foundation
import Synchronization

/// Hands out caption utterance IDs, unique within one capture session across all streams.
///
/// Microphone and system audio are segmented independently but share one caption bar, so a
/// single counter keeps `utteranceId` unambiguous for the app.
public final class UtteranceIDAllocator: Sendable {
    private let counter = Mutex(0)

    /// Creates an allocator whose first ID is 1.
    public init() {}

    /// Returns the next unused ID.
    /// - Returns: 1, 2, 3, … across all callers.
    public func next() -> Int {
        counter.withLock { value in
            value += 1
            return value
        }
    }
}

/// Turns voice-activity events of one stream into caption work items.
///
/// The VAD says where speech starts and ends; this decides when to show a volatile partial,
/// when to finalize, and when to cut a monologue so captions never wait for a pause that does
/// not come. All positions are sample indices at `config.sampleRate` since the stream began.
public struct UtteranceSegmenter: Sendable {
    /// Timing rules, in seconds.
    public struct Config: Sendable {
        public var sampleRate: Int
        /// Speech between consecutive partials.
        public var partialInterval: Double
        /// Speech before the first partial. Shorter than `partialInterval` so a caption appears
        /// sooner (measured: first partial 2.6 s after speech onset at 1 s); later partials keep
        /// the slower cadence so the decoder is not run more often.
        public var firstPartialDelay: Double
        /// Longest utterance before it is finalized and a new one starts (ASR quality drops on long inputs).
        public var maxDuration: Double
        /// Utterances shorter than this are dropped unless a partial was already shown.
        public var minFinalDuration: Double

        /// Creates a config.
        /// - Parameters: see the stored properties.
        public init(sampleRate: Int = 16_000, partialInterval: Double = 1.0, firstPartialDelay: Double = 0.5,
                    maxDuration: Double = 12, minFinalDuration: Double = 0.3) {
            self.sampleRate = sampleRate
            self.partialInterval = partialInterval
            self.firstPartialDelay = firstPartialDelay
            self.maxDuration = maxDuration
            self.minFinalDuration = minFinalDuration
        }
    }

    /// What the caller should transcribe now.
    public enum Action: Equatable, Sendable {
        /// Transcribe `start..<end` and show it as a replaceable caption.
        case partial(id: Int, start: Int, end: Int)
        /// Transcribe `start..<end` as the stable text of utterance `id`. `hadPartial` tells the
        /// caller a partial is on screen and must be retired even if the text comes back empty.
        case final(id: Int, start: Int, end: Int, hadPartial: Bool)
    }

    public let config: Config
    private let ids: UtteranceIDAllocator
    private var id: Int?
    /// Start of the utterance in progress, or `nil` between utterances. Audio before it can be dropped.
    public private(set) var activeStart: Int?
    private var lastPartialEnd: Int?

    /// Creates a segmenter for one stream.
    /// - Parameters:
    ///   - config: Timing rules.
    ///   - ids: Session-wide ID allocator shared with the other stream.
    public init(config: Config, ids: UtteranceIDAllocator) {
        self.config = config
        self.ids = ids
    }

    private func samples(_ seconds: Double) -> Int { Int(seconds * Double(config.sampleRate)) }

    /// Records confirmed speech onset.
    /// - Parameter sample: Where speech began.
    /// - Returns: No actions (kept as an array so callers can treat every event uniformly).
    public mutating func speechStarted(atSample sample: Int) -> [Action] {
        guard activeStart == nil else { return [] }
        activeStart = sample
        id = ids.next()
        lastPartialEnd = nil
        return []
    }

    /// Records the end of speech.
    /// - Parameter endSample: Where speech ended.
    /// - Returns: The final for the utterance in progress, unless it was a blip nobody saw.
    public mutating func speechEnded(endSample: Int) -> [Action] {
        guard let start = activeStart, let id else { return [] }
        let hadPartial = lastPartialEnd != nil
        activeStart = nil
        self.id = nil
        lastPartialEnd = nil
        guard endSample > start, hadPartial || endSample - start >= samples(config.minFinalDuration) else { return [] }
        return [.final(id: id, start: start, end: endSample, hadPartial: hadPartial)]
    }

    /// Advances the stream clock while speech may be ongoing.
    /// - Parameter sample: Samples processed so far.
    /// - Returns: A forced final (and a fresh utterance) when the current one is too long,
    ///   otherwise a partial when one is due.
    public mutating func advance(to sample: Int) -> [Action] {
        guard let start = activeStart, let id else { return [] }
        if sample - start >= samples(config.maxDuration) {
            let action = Action.final(id: id, start: start, end: sample, hadPartial: lastPartialEnd != nil)
            activeStart = sample
            self.id = ids.next()
            lastPartialEnd = nil
            return [action]
        }
        let since = sample - (lastPartialEnd ?? start)
        let due = lastPartialEnd == nil ? config.firstPartialDelay : config.partialInterval
        if since >= samples(due) {
            lastPartialEnd = sample
            return [.partial(id: id, start: start, end: sample)]
        }
        return []
    }
}

/// Converts stream sample positions into caption milliseconds since capture start.
public struct CaptionClock: Sendable, Equatable {
    public var sampleRate: Int
    /// When the stream's first sample was captured, in ms since capture start.
    public var offsetMs: Int

    /// Creates a clock.
    /// - Parameters:
    ///   - sampleRate: Stream sample rate.
    ///   - offsetMs: Capture-relative time of sample 0.
    public init(sampleRate: Int, offsetMs: Int) {
        self.sampleRate = sampleRate
        self.offsetMs = offsetMs
    }

    /// Converts a sample index.
    /// - Parameter sample: Index since the stream began.
    /// - Returns: Milliseconds since capture start.
    public func ms(atSample sample: Int) -> Int {
        offsetMs + Int((Double(sample) * 1000 / Double(sampleRate)).rounded())
    }
}
