import Testing
@testable import MeetingCore

/// Deterministic white-ish noise so the test does not depend on a random seed.
private func noise(_ n: Int, seed: UInt64 = 42) -> [Float] {
    var s = seed
    return (0..<n).map { _ in
        s = s &* 6364136223846793005 &+ 1442695040888963407
        return Float(Int64(bitPattern: s >> 11) % 2000) / 1000 - 1
    }
}

@Test(arguments: [-7, -2, 0, 3, 5])
func recoversIntegerDelay(delay: Int) throws {
    let frame = 1024
    let source = noise(frame + 32)
    let a = Array(source[16..<(16 + frame)])
    let b = Array(source[(16 - delay)..<(16 - delay + frame)]) // b[m] = a[m - delay]
    let est = try GCCPHAT(frameCount: frame).estimate(a, b, maxLag: 16)
    #expect(abs(est.delaySamples - Float(delay)) < 0.5)
    #expect(est.confidence > 0.5)
}

@Test func azimuthFromDelay() {
    // 10 cm spacing at 16 kHz: max delay ≈ 4.66 samples; half of it ≈ 30°.
    let az = GCCPHAT.azimuth(delaySamples: 2.332, sampleRate: 16_000, spacingMeters: 0.10)!
    #expect(abs(az - 30) < 1)
    #expect(GCCPHAT.azimuth(delaySamples: 1, sampleRate: 16_000, spacingMeters: nil) == nil)
}
