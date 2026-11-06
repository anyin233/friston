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

/// Low-pass "speech": white noise through four 16-tap moving averages (content well below 3 kHz at 48 kHz).
private func lowpassNoise(count: Int, seed: UInt64) -> [Float] {
    var state = seed
    var x = (0..<count).map { _ -> Float in
        state = state &* 6364136223846793005 &+ 1442695040888963407
        return Float(Int64(bitPattern: state >> 11) % 2000) / 1000 - 1
    }
    for _ in 0..<4 {
        var y = [Float](repeating: 0, count: count)
        var acc: Float = 0
        for i in 0..<count { acc += x[i]; if i >= 16 { acc -= x[i - 16] }; y[i] = acc / 16 }
        x = y
    }
    return x
}

@Test func speechBandDelayWinsOverCommonBroadbandNoiseAt48kHz() throws {
    // Upsampled or band-limited speech leaves the upper band to noise that is identical in both
    // channels (loopback drivers, USB electrical noise). Full-band PHAT weights those empty bins
    // like speech and locks onto lag 0; the estimator must look at the speech band only.
    let n = 2048, delay = 24
    let speech = lowpassNoise(count: n + delay, seed: 7).map { $0 * 4 }
    var noiseState: UInt64 = 99
    let common = (0..<n).map { _ -> Float in
        noiseState = noiseState &* 6364136223846793005 &+ 1442695040888963407
        return (Float(Int64(bitPattern: noiseState >> 11) % 2000) / 1000 - 1) * 0.003
    }
    let a = (0..<n).map { speech[$0 + delay] + common[$0] }
    let b = (0..<n).map { speech[$0] + common[$0] }
    let gcc = try GCCPHAT(frameCount: n, sampleRate: 48_000)
    let est = gcc.estimate(a, b, maxLag: 240)
    #expect(abs(est.delaySamples - Float(delay)) < 1.5, "got \(est.delaySamples)")
    // Control: full-band PHAT on the same frames is pulled off the talker by the common noise.
    let fullBand = try GCCPHAT(frameCount: n).estimate(a, b, maxLag: 240)
    #expect(abs(fullBand.delaySamples - Float(delay)) > 1.5, "control should fail, got \(fullBand.delaySamples)")
}
