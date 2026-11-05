import Foundation

/// A geometry-free position cue for one stretch of speech, comparable only within one meeting.
///
/// Each axis is dimensionless and roughly −1…1 (the extremes seen so far in this meeting).
/// Two cues are comparable only when their `position` arrays have the same length.
public struct SpatialCue: Sendable, Equatable {
    /// Relative position; for ``StereoCueAnalyzer`` it is `[time difference, level difference]`.
    public var position: [Float]
    /// 0…1 reliability.
    public var confidence: Float

    /// Creates a cue from already-normalised coordinates.
    /// - Parameters:
    ///   - position: Dimensionless coordinates, about −1…1 per axis.
    ///   - confidence: 0…1 reliability.
    public init(position: [Float], confidence: Float) {
        self.position = position
        self.confidence = confidence
    }

    /// Creates a 1-D cue from a calibrated azimuth (array spacing known), on the same scale
    /// as relative cues so the fusion tunables mean the same thing in both cases.
    /// - Parameters:
    ///   - azimuthDeg: −90…90 degrees from ``GCCPHAT/azimuth(delaySamples:sampleRate:spacingMeters:speedOfSound:)``.
    ///   - confidence: 0…1 reliability.
    public init(azimuthDeg: Float, confidence: Float) {
        self.init(position: [azimuthDeg / 90], confidence: confidence)
    }
}

/// What kind of two-channel input this is, inferred from the signal alone.
public enum StereoLayout: String, Codable, Sendable {
    /// Not enough varied speech yet.
    case undetermined
    /// Both channels carry the same signal (a mono mic reported as stereo): no position cue.
    case duplicateMono
    /// Coincident capsules (XY/MS): no time difference, direction is in the level difference.
    case coincident
    /// A closely spaced pair (AR glasses, a bracket): direction is in the time difference.
    case spaced
    /// Mics placed far apart (one per side of the table): who is nearer to which mic.
    case distributed
}

/// Raw two-channel evidence for one frame.
public struct StereoFrameCue: Sendable, Equatable {
    /// GCC-PHAT delay of channel b relative to a, in samples (positive: reached a first).
    public var delaySamples: Float
    /// 10·log10(energy a / energy b): positive when a is louder.
    public var levelDiffDb: Float
    /// GCC-PHAT peak height, 0…1.
    public var confidence: Float
    /// Pearson correlation at zero lag; ≈ 1 means duplicated channels.
    public var zeroLagCorrelation: Float
}

/// Tunables for ``StereoCueAnalyzer``.
public struct StereoCueConfig: Sendable, Equatable {
    public var sampleRate: Float = 48_000
    /// Samples per channel per frame (2048 at 48 kHz ≈ 43 ms).
    public var frameCount: Int = 2048
    /// Widest mic spacing considered before anything is known (two mics across a table).
    public var maxSpacingMeters: Float = 1.7
    public var speedOfSound: Float = 343
    /// Frames below this RMS (either channel) are treated as silence.
    public var silenceRMS: Float = 1e-4
    /// Frames below this GCC-PHAT confidence do not feed the statistics.
    public var minConfidence: Float = 0.1
    /// Zero-lag correlation above this marks a duplicated channel.
    public var duplicateCorrelation: Float = 0.999
    /// Confident frames needed before the layout is classified.
    public var warmupFrames: Int = 40
    /// Statistics window, in confident frames (≈ 2 min of speech at 43 ms per frame).
    public var historyFrames: Int = 3000
    /// Delay spread (half of p95 − p5) below this means coincident capsules.
    public var coincidentMaxDelaySeconds: Float = 1e-4
    /// Delay spread (half of p95 − p5) above this means mics placed far apart.
    public var spacedMaxDelaySeconds: Float = 1.5e-3
    /// Level-difference spread (p95 − p5) needed to call a zero-delay pair coincident.
    public var minLevelSpreadDb: Float = 3
    /// Floors for the normalisation half-ranges, so near-constant axes are not blown up into noise.
    public var minDelayHalfRangeSamples: Float = 2
    public var minLevelHalfRangeDb: Float = 1.5
    /// Recent confident frames compared against the long window to spot a link-delay jump
    /// (≈ 1 s of speech); 0 disables jump detection.
    public var delayJumpWindowFrames: Int = 24
    /// Smallest shift treated as a jump. Above the TDOA of any bracket or glasses pair (20 cm ≈
    /// 0.58 ms), below the several-millisecond re-buffering of a wireless relink.
    public var minDelayJumpSeconds: Float = 7.5e-4
    /// Consecutive voiced frames without a confident peak before a narrowed search window is
    /// widened again: after a large jump the true peak lies outside it.
    public var lostFramesBeforeWidening: Int = 24

    public init() {}
}

/// Turns raw two-channel audio into relative position cues without knowing the mic geometry.
///
/// Speaker fusion only needs to know whether two utterances came from the same place, so the
/// analyzer never converts to degrees. It measures the time difference (GCC-PHAT) and level
/// difference per frame, learns their range over the meeting, and maps each to −1…1 around the
/// middle of that range. Centring on the observed range is what makes a constant link delay
/// between channels (two wireless transmitters, for instance) harmless.
public final class StereoCueAnalyzer {
    public let config: StereoCueConfig
    /// Current guess at the input kind; re-evaluated as speech accumulates.
    public private(set) var layout: StereoLayout = .undetermined
    /// GCC-PHAT search half-width in samples; narrows to the observed range once classified.
    public private(set) var searchLagSamples: Int
    /// Inter-channel delay jumps detected so far (each one restarted the statistics).
    public private(set) var delayJumps = 0

    private let gcc: GCCPHAT
    private let initialSearchLag: Int
    private var delays: [Float] = []
    private var levels: [Float] = []
    private var duplicates: [Bool] = []
    private var sinceClassify = 0
    /// Voiced frames in a row that produced no confident peak.
    private var lostFrames = 0
    /// Whether ``delayRange`` has been learned from data (jump detection needs a reference).
    private var rangeLearned = false
    private var delayRange: (mid: Float, half: Float)
    private var levelRange: (mid: Float, half: Float)

    /// Creates an analyzer for one meeting.
    /// - Parameter config: Frame size, sample rate and thresholds.
    /// - Throws: If the FFT for `config.frameCount` cannot be created.
    public init(config: StereoCueConfig = .init()) throws {
        self.config = config
        gcc = try GCCPHAT(frameCount: config.frameCount)
        let lag = Int((config.maxSpacingMeters / config.speedOfSound * config.sampleRate).rounded(.up))
        // GCC-PHAT pads to ≥ 2× the frame, so lags beyond the frame would wrap around.
        initialSearchLag = min(lag, config.frameCount - 1)
        searchLagSamples = initialSearchLag
        delayRange = (0, config.minDelayHalfRangeSamples)
        levelRange = (0, config.minLevelHalfRangeDb)
    }

    /// Measures one frame and folds it into the meeting statistics.
    /// - Parameters:
    ///   - a: Channel a, `config.frameCount` samples.
    ///   - b: Channel b, `config.frameCount` samples.
    /// - Returns: The frame's raw cue, or `nil` for silence.
    public func analyze(_ a: [Float], _ b: [Float]) -> StereoFrameCue? {
        precondition(a.count == config.frameCount && b.count == config.frameCount,
                     "stereo frame must be \(config.frameCount) samples, got \(a.count)/\(b.count)")
        let ea = a.reduce(0) { $0 + $1 * $1 }, eb = b.reduce(0) { $0 + $1 * $1 }
        let n = Float(a.count)
        let floor = config.silenceRMS * config.silenceRMS * n
        guard ea > floor, eb > floor else { return nil }

        let est = gcc.estimate(a, b, maxLag: searchLagSamples)
        trackLostPeak(confident: est.confidence >= config.minConfidence)
        let cue = StereoFrameCue(
            delaySamples: est.delaySamples,
            levelDiffDb: 10 * log10(ea / eb),
            confidence: est.confidence,
            zeroLagCorrelation: pearson(a, b))
        record(cue)
        return cue
    }

    /// Maps one frame's raw cue onto the meeting's relative scale.
    /// - Parameter cue: Output of ``analyze(_:_:)``.
    /// - Returns: A 2-D `[time, level]` cue, or `nil` when the input carries no position.
    public func position(of cue: StereoFrameCue) -> SpatialCue? {
        guard layout != .duplicateMono, cue.confidence >= config.minConfidence else { return nil }
        let (wt, wl) = axisWeights
        let t = (cue.delaySamples - delayRange.mid) / delayRange.half
        let l = (cue.levelDiffDb - levelRange.mid) / levelRange.half
        return SpatialCue(position: [wt * t, wl * l], confidence: cue.confidence)
    }

    /// Summarises the frames of one speech segment (one diarisation turn).
    ///
    /// Uses the per-axis median so a few reverberant or overlapped frames cannot drag the cue.
    /// - Parameter frames: Raw cues collected while the segment was spoken.
    /// - Returns: The segment's cue; confidence is the mean frame confidence scaled by the
    ///   fraction of usable frames. `nil` when no frame is usable.
    public func segmentCue(_ frames: [StereoFrameCue]) -> SpatialCue? {
        let cues = frames.compactMap(position(of:))
        guard !cues.isEmpty else { return nil }
        let dims = cues[0].position.count
        let position = (0..<dims).map { d in percentile(cues.map { $0.position[d] }, 0.5) }
        let meanConf = cues.reduce(0) { $0 + $1.confidence } / Float(cues.count)
        return SpatialCue(position: position, confidence: meanConf * Float(cues.count) / Float(frames.count))
    }

    /// Collapses a cue to one left-right coordinate, using the axes the current layout trusts.
    ///
    /// A coincident pair carries direction only in the level difference and a spaced pair only
    /// in the time difference, so neither axis alone works as "left/right" for every mic.
    /// - Parameter cue: A 2-D cue from ``segmentCue(_:)`` or ``position(of:)``.
    /// - Returns: About −1…1 in the analyzer's sign (positive: towards channel a).
    public func lateral(of cue: SpatialCue) -> Float {
        let (wt, wl) = axisWeights
        guard cue.position.count == 2, wt + wl > 0 else { return cue.position.first ?? 0 }
        return (cue.position[0] + cue.position[1]) / (wt + wl)
    }

    /// The `spatial/cue` coordinates of a segment cue: `[left-right, time, level]`, mirrored so
    /// negative is channel a (the first, conventionally left channel) and positive channel b.
    ///
    /// The analyzer's own sign follows GCC-PHAT (positive: sound reached a first), which reads
    /// backwards on a left-to-right screen axis.
    /// - Parameter cue: A 2-D cue from ``segmentCue(_:)``.
    /// - Returns: Three coordinates, each about −1…1.
    public func reportedPosition(of cue: SpatialCue) -> [Float] {
        ([lateral(of: cue)] + cue.position).map { -$0 }
    }

    /// How much each axis counts for the current layout: (time, level).
    /// In a closely spaced pair the level difference is mostly noise; in a coincident pair the
    /// time difference is. Until the layout is known, lean on time but keep some level evidence.
    private var axisWeights: (Float, Float) {
        switch layout {
        case .undetermined: (1, 0.5)
        case .duplicateMono: (0, 0)
        case .coincident: (0, 1)
        case .spaced: (1, 0)
        case .distributed: (1, 1)
        }
    }

    private func record(_ cue: StereoFrameCue) {
        // Unreliable delays would only widen the learned range with reverberation peaks.
        guard cue.confidence >= config.minConfidence else { return }
        delays.append(cue.delaySamples)
        levels.append(cue.levelDiffDb)
        duplicates.append(cue.zeroLagCorrelation >= config.duplicateCorrelation)
        if delays.count > config.historyFrames {
            delays.removeFirst(); levels.removeFirst(); duplicates.removeFirst()
        }
        sinceClassify += 1
        if detectDelayJump() { return }
        // Classify every frame while warming up, then periodically: sorting the history is not free.
        // A short history (start, or just after a jump) is re-read more often so it settles quickly.
        let short = delays.count < 4 * config.warmupFrames
        if delays.count >= config.warmupFrames
            && (layout == .undetermined || sinceClassify >= 50 || (short && sinceClassify >= 10)) {
            classify()
        }
    }

    /// Widens a narrowed search window when voiced frames stop yielding a peak.
    ///
    /// After a link-delay jump larger than the narrowed window, every frame's true peak lies
    /// outside it, nothing confident is recorded and the statistics could never see the jump.
    /// - Parameter confident: Whether this voiced frame's peak passed `minConfidence`.
    private func trackLostPeak(confident: Bool) {
        guard !confident else { lostFrames = 0; return }
        lostFrames += 1
        if lostFrames >= config.lostFramesBeforeWidening && searchLagSamples < initialSearchLag {
            searchLagSamples = initialSearchLag
            lostFrames = 0
        }
    }

    /// Restarts the statistics when the recent delays sit persistently far outside the learned range.
    ///
    /// A wireless transmitter that re-pairs shifts every talker by the same amount at once. The
    /// long window would need ~2 minutes of speech to forget the old offset; a talker who is
    /// merely new stays within about one range width of the others, a relink lands far beyond.
    /// The recent frames seed the new history and the old half-range is kept, so positions are
    /// usable again after a couple of seconds instead of collapsing to noise.
    /// - Returns: `true` when a jump was detected and the history was replaced.
    private func detectDelayJump() -> Bool {
        let window = config.delayJumpWindowFrames
        guard window > 0, rangeLearned, delays.count >= max(window, config.warmupFrames) else { return false }
        let recent = Array(delays.suffix(window))
        let median = percentile(recent, 0.5)
        let minJump = config.minDelayJumpSeconds * config.sampleRate
        guard abs(median - delayRange.mid) > delayRange.half + max(delayRange.half, minJump) else { return false }
        delayJumps += 1
        // The window straddles the jump; frames from before it would pin the 5th percentile to
        // the old offset for as long as they stay in the history.
        let start = delays.count - window
        let keep = (start..<delays.count).filter { abs(delays[$0] - median) < abs(delays[$0] - delayRange.mid) }
        delays = keep.map { delays[$0] }
        levels = keep.map { levels[$0] }
        duplicates = keep.map { duplicates[$0] }
        // Centre on the recent talkers until the next classification measures the new spread.
        delayRange = (median, delayRange.half)
        rangeLearned = false
        sinceClassify = 0
        return true
    }

    private func classify() {
        sinceClassify = 0
        let duplicateShare = Float(duplicates.filter { $0 }.count) / Float(duplicates.count)
        if duplicateShare >= 0.9 {
            layout = .duplicateMono
            return
        }
        let (d5, d95) = (percentile(delays, 0.05), percentile(delays, 0.95))
        let (l5, l95) = (percentile(levels, 0.05), percentile(levels, 0.95))
        delayRange = ((d5 + d95) / 2, max((d95 - d5) / 2, config.minDelayHalfRangeSamples))
        levelRange = ((l5 + l95) / 2, max((l95 - l5) / 2, config.minLevelHalfRangeDb))
        rangeLearned = true

        // Spread around the centre, not |delay|: a constant link offset between the channels
        // (two wireless transmitters) must not make a bracket pair look like mics across a table.
        let maxDelay = (d95 - d5) / 2 / config.sampleRate
        if maxDelay > config.spacedMaxDelaySeconds {
            layout = .distributed
        } else if maxDelay < config.coincidentMaxDelaySeconds {
            // Zero delay with a flat level difference may just be one talker straight ahead.
            layout = l95 - l5 >= config.minLevelSpreadDb ? .coincident : .undetermined
        } else {
            layout = .spaced
        }
        if layout == .spaced || layout == .coincident {
            // Keep 2× headroom: a talker at the far end of the array may not have spoken yet.
            let observed = percentile(delays.map(abs), 0.99)
            searchLagSamples = min(initialSearchLag, max(8, Int((2 * observed).rounded(.up))))
        } else {
            searchLagSamples = initialSearchLag
        }
    }
}

/// Pearson correlation of two equal-length signals.
/// - Returns: −1…1, or 0 when either signal is constant.
func pearson(_ a: [Float], _ b: [Float]) -> Float {
    let n = Float(a.count)
    let ma = a.reduce(0, +) / n, mb = b.reduce(0, +) / n
    var sab: Float = 0, saa: Float = 0, sbb: Float = 0
    for (x, y) in zip(a, b) {
        sab += (x - ma) * (y - mb); saa += (x - ma) * (x - ma); sbb += (y - mb) * (y - mb)
    }
    let denom = (saa * sbb).squareRoot()
    return denom > 0 ? sab / denom : 0
}

/// Nearest-rank percentile.
/// - Parameters:
///   - values: Non-empty sample.
///   - p: 0…1.
/// - Returns: The value at rank `p`.
func percentile(_ values: [Float], _ p: Float) -> Float {
    let sorted = values.sorted()
    let i = Int((p * Float(sorted.count - 1)).rounded())
    return sorted[max(0, min(sorted.count - 1, i))]
}
