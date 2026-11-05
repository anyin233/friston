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

    /// Creates an estimator for frames of `frameCount` samples.
    /// - Parameter frameCount: Samples per channel per estimate (e.g. 1024 at 16 kHz ≈ 64 ms).
    /// - Throws: If vDSP cannot create a transform of the padded size.
    public init(frameCount: Int) throws {
        self.frameCount = frameCount
        // Zero-pad to ≥ 2× so circular correlation does not wrap real lags.
        var n = 16
        while n < 2 * frameCount { n <<= 1 }
        fftSize = n
        forward = try vDSP.DiscreteFourierTransform(
            count: n, direction: .forward, transformType: .complexComplex, ofType: Float.self)
        inverse = try vDSP.DiscreteFourierTransform(
            count: n, direction: .inverse, transformType: .complexComplex, ofType: Float.self)
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
        var rr = [Float](repeating: 0, count: fftSize)
        var ri = [Float](repeating: 0, count: fftSize)
        for k in 0..<fftSize {
            let re = ar[k] * br[k] + ai[k] * bi[k]
            let im = ai[k] * br[k] - ar[k] * bi[k]
            let mag = max((re * re + im * im).squareRoot(), 1e-12)
            rr[k] = re / mag
            ri[k] = im / mag
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
        return TDOAEstimate(delaySamples: -peak, confidence: max(0, min(1, best / Float(fftSize))))
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
