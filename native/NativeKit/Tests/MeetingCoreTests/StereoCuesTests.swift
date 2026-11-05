import Testing
@testable import MeetingCore

/// Deterministic noise in −1…1 so tests never depend on a random seed.
/// - Parameters:
///   - n: Sample count.
///   - seed: Generator seed; different seeds give independent sequences.
/// - Returns: `n` samples.
private func noise(_ n: Int, seed: UInt64) -> [Float] {
    var s = seed &* 0x9E37_79B9_7F4A_7C15 | 1
    return (0..<n).map { _ in
        s = s &* 6364136223846793005 &+ 1442695040888963407
        return Float(Int64(bitPattern: s >> 11) % 2000) / 1000 - 1
    }
}

private let frame = 2048
private let pad = 400

/// One stereo frame of a talker whose sound reaches channel `a` `delay` samples before `b`,
/// with `b` scaled by `gainB`. Each channel gets its own sensor noise (real microphones),
/// and a fixed `offset` models a constant link delay added to channel `b`.
/// - Returns: The two channels, `frame` samples each.
private func talker(delay: Int, gainB: Float = 1, offset: Int = 0, seed: UInt64) -> ([Float], [Float]) {
    let src = noise(frame + 2 * pad, seed: seed)
    let na = noise(frame, seed: seed &+ 1_000_003)
    let nb = noise(frame, seed: seed &+ 2_000_003)
    let lag = delay + offset
    let a = (0..<frame).map { src[pad + $0] + 0.05 * na[$0] }
    let b = (0..<frame).map { gainB * src[pad + $0 - lag] + 0.05 * nb[$0] }
    return (a, b)
}

/// Runs a warm-up with several talkers taking turns, then returns per-talker segment cues.
/// - Parameters:
///   - analyzer: Analyzer under test.
///   - talkers: (delay, gainB) per talker.
///   - offset: Constant extra delay on channel `b`.
/// - Returns: One segment cue per talker, in input order.
private func runMeeting(_ analyzer: StereoCueAnalyzer, talkers: [(Int, Float)], offset: Int = 0) -> [SpatialCue?] {
    var frames = [[StereoFrameCue]](repeating: [], count: talkers.count)
    var seed: UInt64 = 1
    for _ in 0..<20 {
        for (i, t) in talkers.enumerated() {
            let (a, b) = talker(delay: t.0, gainB: t.1, offset: offset, seed: seed)
            seed += 1
            if let cue = analyzer.analyze(a, b) { frames[i].append(cue) }
        }
    }
    return frames.map { analyzer.segmentCue($0) }
}

@Test func duplicatedMonoIsDetectedAndYieldsNoCue() throws {
    let analyzer = try StereoCueAnalyzer()
    var last: StereoFrameCue?
    for seed in 1...60 {
        let a = noise(frame, seed: UInt64(seed))
        last = analyzer.analyze(a, a.map { 0.8 * $0 })
    }
    #expect(analyzer.layout == .duplicateMono)
    #expect(analyzer.segmentCue([last!]) == nil)
}

@Test func silenceGivesNoFrameCue() throws {
    let analyzer = try StereoCueAnalyzer()
    let quiet = [Float](repeating: 0, count: frame)
    #expect(analyzer.analyze(quiet, quiet) == nil)
}

@Test func spacedPairWithoutGeometryOrdersTalkersByDirection() throws {
    // ~20 cm pair at 48 kHz: physical limit ≈ 28 samples. Spacing is never given.
    let analyzer = try StereoCueAnalyzer()
    let cues = runMeeting(analyzer, talkers: [(-20, 1), (0, 1), (20, 1)])
    #expect(analyzer.layout == .spaced)
    let x = cues.map { $0!.position[0] }
    #expect(x[0] < -0.7 && abs(x[1]) < 0.3 && x[2] > 0.7, "positions \(x)")
    #expect(cues.allSatisfy { $0!.confidence > 0.3 })
    // Search window shrinks from the 1.7 m default once the real range is known.
    #expect(analyzer.searchLagSamples < 100)
}

@Test func constantChannelOffsetDoesNotMoveRelativePositions() throws {
    let plain = runMeeting(try StereoCueAnalyzer(), talkers: [(-20, 1), (0, 1), (20, 1)])
    let shifted = runMeeting(try StereoCueAnalyzer(), talkers: [(-20, 1), (0, 1), (20, 1)], offset: 15)
    for (p, s) in zip(plain, shifted) {
        #expect(abs(p!.position[0] - s!.position[0]) < 0.15, "\(p!.position) vs \(s!.position)")
    }
}

@Test func coincidentPairUsesLevelDifference() throws {
    // XY-style pair: no time difference, direction lives in the level difference (±6 dB).
    let analyzer = try StereoCueAnalyzer()
    let cues = runMeeting(analyzer, talkers: [(0, 2), (0, 1), (0, 0.5)])
    #expect(analyzer.layout == .coincident)
    let ild = cues.map { $0!.position[1] }
    #expect(ild[0] < -0.7 && abs(ild[1]) < 0.3 && ild[2] > 0.7, "positions \(ild)")
}

@Test func farApartMicsAreDistributed() throws {
    // Two mics ~1.5 m apart on a table: ±3 ms and strong level differences.
    let analyzer = try StereoCueAnalyzer()
    let cues = runMeeting(analyzer, talkers: [(-150, 2.5), (150, 0.4)])
    #expect(analyzer.layout == .distributed)
    #expect(cues[0]!.position[0] < -0.7 && cues[1]!.position[0] > 0.7)
    #expect(cues[0]!.position[1] < -0.7 && cues[1]!.position[1] > 0.7)
}

@Test func relativeCuesSeparateSimilarVoicesWithoutKnownSpacing() throws {
    // End to end: analyzer output drives fusion. Timbre alone (cosine 0.6) would merge them.
    let analyzer = try StereoCueAnalyzer()
    let cues = runMeeting(analyzer, talkers: [(-20, 1), (20, 1)])
    let a: [Float] = [1, 0, 0]
    let similar: [Float] = normalized([0.6, 0.8, 0])
    let tracker = OnlineSpeakerTracker(bank: VoiceprintBank())
    _ = tracker.assign(.init(embedding: a, origin: .microphone, spatial: cues[0]))
    #expect(tracker.assign(.init(embedding: similar, origin: .microphone, spatial: cues[1])) == .cluster(index: 1))
}

@Test func oneTalkerStraightAheadStaysUndetermined() throws {
    // Zero delay and a flat level difference cannot tell coincident from spaced yet.
    let analyzer = try StereoCueAnalyzer()
    _ = runMeeting(analyzer, talkers: [(0, 1), (0, 1), (0, 1)])
    #expect(analyzer.layout == .undetermined)
}
