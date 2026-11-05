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

private let flexDevice = AudioInputInfo(uid: "flex", name: "reSpeaker Flex", channels: 6, sampleRate: 16_000, isVirtual: false)

@Test func plainStereoDeviceUsesFirstTwoChannelsAtItsOwnRate() throws {
    let xreal = AudioInputInfo(uid: "usb", name: "XREAL One Pro", channels: 2, sampleRate: 48_000, isVirtual: false)
    let profile = try #require(try SpatialArrayProfile.resolve(for: xreal))
    #expect(profile.channelA == 0 && profile.channelB == 1)
    #expect(profile.spacingMeters == nil)
    #expect(profile.analyzerConfig.sampleRate == 48_000)
    #expect(profile.analyzerConfig.frameCount == 2048)
}

@Test func multichannelDeviceNeedsAnExplicitPreset() throws {
    // Channel layouts differ between ReSpeaker firmwares (XVF3000 puts playback on CH5),
    // so a 6-channel device is never guessed.
    #expect(try SpatialArrayProfile.resolve(for: flexDevice) == nil)
}

@Test func virtualAndMonoDevicesGetNoProfile() throws {
    let loopback = AudioInputInfo(uid: "v", name: "WeMeet Audio Device", channels: 2, sampleRate: 48_000, isVirtual: true)
    let builtIn = AudioInputInfo(uid: "b", name: "MacBook Pro Microphone", channels: 1, sampleRate: 48_000, isVirtual: false)
    #expect(try SpatialArrayProfile.resolve(for: loopback) == nil)
    #expect(try SpatialArrayProfile.resolve(for: builtIn) == nil)
}

@Test func flexPresetPicksTheEndMicsAt16kHz() throws {
    let profile = try #require(try SpatialArrayProfile.resolve(for: flexDevice, preset: .respeakerFlexLinear6ch))
    #expect(profile.channelA == 2 && profile.channelB == 5)
    #expect(abs(profile.spacingMeters! - 0.0999) < 1e-6)
    #expect(profile.analyzerConfig.sampleRate == 16_000)
    #expect(profile.analyzerConfig.frameCount == 1024)
}

@Test func presetRejectsADeviceThatDoesNotMatchItsFirmware() {
    let stereo = AudioInputInfo(uid: "s", name: "reSpeaker Flex", channels: 2, sampleRate: 16_000, isVirtual: false)
    let wrongRate = AudioInputInfo(uid: "r", name: "reSpeaker Flex", channels: 6, sampleRate: 48_000, isVirtual: false)
    let virtual = AudioInputInfo(uid: "v", name: "Aggregate", channels: 6, sampleRate: 16_000, isVirtual: true)
    #expect(throws: SpatialArrayProfileError.self) { try SpatialArrayProfile.resolve(for: stereo, preset: .respeakerFlexLinear6ch) }
    #expect(throws: SpatialArrayProfileError.self) { try SpatialArrayProfile.resolve(for: wrongRate, preset: .respeakerFlexLinear6ch) }
    #expect(throws: SpatialArrayProfileError.self) { try SpatialArrayProfile.resolve(for: virtual, preset: .respeakerFlexLinear6ch) }
    // The error names the device and the offending value so logs are actionable.
    let error = SpatialArrayProfileError.sampleRateMismatch(device: "reSpeaker Flex", preset: .respeakerFlexLinear6ch,
                                                             expected: 16_000, actual: 48_000)
    #expect("\(error)".contains("reSpeaker Flex") && "\(error)".contains("48000"))
}

@Test func pairIsDeinterleavedFromTheChosenChannels() throws {
    let profile = try #require(try SpatialArrayProfile.resolve(for: flexDevice, preset: .respeakerFlexLinear6ch))
    // Two frames of six channels; value = 10·frame + channel.
    let interleaved: [Float] = (0..<2).flatMap { f in (0..<6).map { Float(10 * f + $0) } }
    let (a, b) = try profile.pair(fromInterleaved: interleaved, channels: 6)
    #expect(a == [2, 12] && b == [5, 15])
    #expect(throws: SpatialArrayProfileError.self) { try profile.pair(fromInterleaved: [1, 2, 3], channels: 6) }
    #expect(throws: SpatialArrayProfileError.self) { try profile.pair(fromInterleaved: [1, 2], channels: 2) }
}

@Test func flexLinearArrayOrdersTalkersAt16kHz() throws {
    // Four mics in a line; a talker reaches mic k `k·step` samples late. The end pair spans 3·step.
    let profile = try #require(try SpatialArrayProfile.resolve(for: flexDevice, preset: .respeakerFlexLinear6ch))
    let analyzer = try StereoCueAnalyzer(config: profile.analyzerConfig)
    let n = profile.analyzerConfig.frameCount, pad = 16
    let steps = [-1, 0, 1]
    var frames = [[StereoFrameCue]](repeating: [], count: steps.count)
    var seed: UInt64 = 1
    for _ in 0..<20 {
        for (i, step) in steps.enumerated() {
            let src = noise(n + 2 * pad, seed: seed)
            let processed = noise(n, seed: seed &+ 7)  // CH0/CH1: unrelated processed audio
            let sensors = (0..<6).map { noise(n, seed: seed &+ 1_000_003 &* UInt64($0 + 1)) }
            seed += 1
            var interleaved = [Float](repeating: 0, count: n * 6)
            for t in 0..<n {
                interleaved[t * 6] = processed[t]
                interleaved[t * 6 + 1] = processed[t]
                for mic in 0..<4 {
                    interleaved[t * 6 + 2 + mic] = src[pad + t - mic * step] + 0.05 * sensors[mic][t]
                }
            }
            let (a, b) = try profile.pair(fromInterleaved: interleaved, channels: 6)
            if let cue = analyzer.analyze(a, b) { frames[i].append(cue) }
        }
    }
    #expect(analyzer.layout == .spaced)
    let x = frames.map { analyzer.segmentCue($0)!.position[0] }
    #expect(x[0] < -0.5 && abs(x[1]) < 0.3 && x[2] > 0.5, "positions \(x)")
}
