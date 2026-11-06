import Accelerate
import Foundation

/// Time-difference-of-arrival estimate between two microphone channels.
public struct TDOAEstimate: Sendable, Equatable {
    /// How many samples channel `b` lags channel `a` (positive: sound reached `a` first).
    public var delaySamples: Float
    /// Peak height of the PHAT-weighted correlation, 0…1 (1 = a single clean path).
    public var confidence: Float
}

/// GCC-PHAT time-delay estimator for one microphone pair.
///
/// PHAT weighting whitens the cross-spectrum so the correlation peak stays sharp
/// in reverberant meeting rooms, which is what makes per-frame direction usable
/// as a speaker cue.
public final class GCCPHAT {
    private let frameCount: Int
    private let fftSize: Int
    private let forward: vDSP.DiscreteFourierTransform<Float>
    private let inverse: vDSP.DiscreteFourierTransform<Float>
    /// Per-bin weight (1 inside the analysis band, 0 outside); all ones for full band.
    private let bandMask: [Float]
    /// PHAT exponent: 1 is classic PHAT; 0.8 when a sample rate (speech band) is given.
    private let phatBeta: Float

    /// Creates an estimator for frames of `frameCount` samples.
    ///
    /// PHAT gives every frequency bin the same weight, so bins with no speech are as loud as
    /// bins with speech. When those empty bins carry noise that is identical in both channels
    /// (upsampled audio, loopback drivers, USB electrical noise) the peak locks onto lag 0
    /// (measured: 48 kHz capture of 16 kHz speech read ±24 samples as ~0). Restricting the
    /// weighting to the speech band, and skipping bins far below the frame's strongest one,
    /// keeps the peak on the talker.
    /// - Parameters:
    ///   - frameCount: Samples per channel per estimate (e.g. 1024 at 16 kHz ≈ 64 ms).
    ///   - sampleRate: Capture rate in Hz; `nil` analyses the full band (no frequency mapping).
    ///   - band: Analysis band in Hz, used when `sampleRate` is given.
    /// - Throws: If vDSP cannot create a transform of the padded size.
    public init(frameCount: Int, sampleRate: Float? = nil, band: ClosedRange<Float> = 80...6_000) throws {
        self.frameCount = frameCount
        // Zero-pad to ≥ 2× so circular correlation does not wrap real lags.
        var n = 16
        while n < 2 * frameCount { n <<= 1 }
        fftSize = n
        forward = try vDSP.DiscreteFourierTransform(
            count: n, direction: .forward, transformType: .complexComplex, ofType: Float.self)
        inverse = try vDSP.DiscreteFourierTransform(
            count: n, direction: .inverse, transformType: .complexComplex, ofType: Float.self)
        phatBeta = sampleRate == nil ? 1 : 0.8
        if let sampleRate {
            bandMask = (0..<n).map { k in
                let hz = Float(min(k, n - k)) * sampleRate / Float(n)
                return band.contains(hz) ? 1 : 0
            }
        } else {
            bandMask = [Float](repeating: 1, count: n)
        }
    }

    /// Estimates the delay of `b` relative to `a`.
    /// - Parameters:
    ///   - a: Reference channel, `frameCount` samples.
    ///   - b: Second channel, `frameCount` samples.
    ///   - maxLag: Largest physically possible lag in samples (mic spacing / speed of sound × rate).
    /// - Returns: Sub-sample delay and confidence.
    public func estimate(_ a: [Float], _ b: [Float], maxLag: Int) -> TDOAEstimate {
        precondition(a.count == frameCount && b.count == frameCount, "frame size mismatch")
        let zeros = [Float](repeating: 0, count: fftSize)
        let pad = { (x: [Float]) in x + [Float](repeating: 0, count: self.fftSize - x.count) }
        let (ar, ai) = forward.transform(real: pad(a), imaginary: zeros)
        let (br, bi) = forward.transform(real: pad(b), imaginary: zeros)

        // R = A · conj(B), then PHAT: R / |R|.
        var cre = [Float](repeating: 0, count: fftSize)
        var cim = [Float](repeating: 0, count: fftSize)
        var mags = [Float](repeating: 0, count: fftSize)
        var strongest: Float = 0
        for k in 0..<fftSize where bandMask[k] > 0 {
            cre[k] = ar[k] * br[k] + ai[k] * bi[k]
            cim[k] = ai[k] * br[k] - ar[k] * bi[k]
            mags[k] = (cre[k] * cre[k] + cim[k] * cim[k]).squareRoot()
            strongest = max(strongest, mags[k])
        }
        // Bins 60 dB below the strongest carry no speech at all.
        let floor = max(strongest * 1e-6, 1e-12)
        var rr = [Float](repeating: 0, count: fftSize)
        var ri = [Float](repeating: 0, count: fftSize)
        var used: Float = 0
        for k in 0..<fftSize where bandMask[k] > 0 && mags[k] > floor {
            // PHAT-β: dividing by |R|^β (β < 1) keeps part of the magnitude, so bins where the
            // talker is loud outweigh noise-only bins, while the peak stays sharp in reverb.
            let w = 1 / pow(mags[k], phatBeta)
            rr[k] = cre[k] * w
            ri[k] = cim[k] * w
            used += pow(mags[k], 1 - phatBeta)
        }
        let (corr, _) = inverse.transform(real: rr, imaginary: ri)

        // corr[k] = Σ a[m+k]·b[m]; a delay d of b peaks at k = −d.
        let limit = min(maxLag, fftSize / 2 - 1)
        var bestLag = 0
        var best = -Float.infinity
        for lag in -limit...limit {
            let v = corr[(lag + fftSize) % fftSize]
            if v > best { best = v; bestLag = lag }
        }
        let at = { (lag: Int) in corr[(lag + self.fftSize) % self.fftSize] }
        let (l, c, r) = (at(bestLag - 1), at(bestLag), at(bestLag + 1))
        let denom = l - 2 * c + r
        let offset = abs(denom) > 1e-9 ? 0.5 * (l - r) / denom : 0
        let peak = Float(bestLag) + max(-0.5, min(0.5, offset))
        // Normalise by the bins that took part, so band-limiting does not shrink confidence.
        return TDOAEstimate(delaySamples: -peak, confidence: max(0, min(1, best / max(used, 1))))
    }

    /// Converts a delay into an azimuth for a two-microphone array.
    /// - Parameters:
    ///   - delaySamples: Output of ``estimate(_:_:maxLag:)``.
    ///   - sampleRate: Capture rate in Hz.
    ///   - spacingMeters: Distance between the two microphones.
    ///   - speedOfSound: m/s (343 at 20 °C).
    /// - Returns: Degrees in −90…90 (0 = broadside), or `nil` when spacing is unknown.
    public static func azimuth(delaySamples: Float, sampleRate: Float, spacingMeters: Float?, speedOfSound: Float = 343) -> Float? {
        guard let d = spacingMeters, d > 0 else { return nil }
        let s = max(-1, min(1, delaySamples / sampleRate * speedOfSound / d))
        return asin(s) * 180 / .pi
    }
}
