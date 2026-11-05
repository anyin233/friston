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
public struct StereoCueConfig: Sendable {
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
    /// 95th-percentile |delay| below this means coincident capsules.
    public var coincidentMaxDelaySeconds: Float = 1e-4
    /// 95th-percentile |delay| above this means mics placed far apart.
    public var spacedMaxDelaySeconds: Float = 1.5e-3
    /// Level-difference spread (p95 − p5) needed to call a zero-delay pair coincident.
    public var minLevelSpreadDb: Float = 3
    /// Floors for the normalisation half-ranges, so near-constant axes are not blown up into noise.
    public var minDelayHalfRangeSamples: Float = 2
    public var minLevelHalfRangeDb: Float = 1.5

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

    private let gcc: GCCPHAT
    private let initialSearchLag: Int
    private var delays: [Float] = []
    private var levels: [Float] = []
    private var duplicates: [Bool] = []
    private var sinceClassify = 0
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
        // Classify every frame while warming up, then periodically: sorting the history is not free.
        if delays.count >= config.warmupFrames && (layout == .undetermined || sinceClassify >= 50) {
            classify()
        }
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

        let maxDelay = percentile(delays.map(abs), 0.95) / config.sampleRate
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
