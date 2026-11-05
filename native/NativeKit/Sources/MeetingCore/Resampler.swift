import Foundation

/// Converts a mono stream between sample rates in arbitrary-sized chunks.
///
/// Speech models want 16 kHz while microphones and ScreenCaptureKit deliver 44.1/48 kHz.
/// Downsampling first applies a windowed-sinc low-pass at 90 % of the new Nyquist (otherwise
/// fricatives above 8 kHz would alias into the speech band), then reads the filtered signal at
/// fractional positions with linear interpolation. State carries across calls, so chunked and
/// one-shot processing give the same samples. The filter delays the output by
/// `(taps − 1) / 2` input samples (< 1 ms), which is negligible for caption timing.
public struct StreamingResampler: Sendable {
    public let inputRate: Double
    public let outputRate: Double
    private let step: Double
    private let taps: [Float]
    /// The last `taps.count − 1` raw input samples (filter memory).
    private var history: [Float]
    /// Last filtered sample of the previous chunk, for interpolation across the boundary.
    private var previous: Float?
    /// Global index of the first filtered sample of the next chunk.
    private var base: Int = 0
    /// Global fractional position (in filtered samples) of the next output sample.
    private var position: Double = 0

    /// Creates a resampler.
    /// - Parameters:
    ///   - inputRate: Rate of the samples passed to ``process(_:)``, Hz (> 0).
    ///   - outputRate: Desired rate, Hz (> 0).
    public init(inputRate: Double, outputRate: Double) {
        precondition(inputRate > 0 && outputRate > 0, "sample rates must be positive, got \(inputRate) → \(outputRate)")
        self.inputRate = inputRate
        self.outputRate = outputRate
        step = inputRate / outputRate
        if outputRate < inputRate {
            taps = Self.lowPass(cutoff: 0.45 * outputRate / inputRate, ratio: step)
        } else {
            taps = [1]
        }
        history = [Float](repeating: 0, count: taps.count - 1)
    }

    /// Resamples the next chunk of the stream.
    /// - Parameter input: Mono samples at `inputRate`.
    /// - Returns: Samples at `outputRate`; roughly `input.count × outputRate / inputRate` of them.
    public mutating func process(_ input: [Float]) -> [Float] {
        if inputRate == outputRate { return input }
        guard !input.isEmpty else { return [] }
        let filtered = filter(input)
        // ys[k] is filtered sample number (base − 1 + k); ys[0] is the previous chunk's last one.
        let ys: [Float]
        let first: Int
        if let previous {
            ys = [previous] + filtered
            first = base - 1
        } else {
            ys = filtered
            first = base
        }
        let last = first + ys.count - 1
        var out: [Float] = []
        out.reserveCapacity(Int(Double(input.count) / step) + 2)
        while position <= Double(last) {
            let i = Int(position.rounded(.down))
            let frac = Float(position - Double(i))
            if i + 1 > last {
                // Need the next sample to interpolate unless we sit exactly on this one.
                guard frac == 0 else { break }
                out.append(ys[i - first])
            } else {
                let a = ys[i - first], b = ys[i + 1 - first]
                out.append(a + (b - a) * frac)
            }
            position += step
        }
        previous = ys.last
        base += filtered.count
        return out
    }

    /// Averages deinterleaved channels into one.
    /// - Parameter channels: One array per channel, equal lengths.
    /// - Returns: The mono mix (the first channel's length).
    public static func downmix(_ channels: [[Float]]) -> [Float] {
        guard let first = channels.first else { return [] }
        if channels.count == 1 { return first }
        let scale = 1 / Float(channels.count)
        var mix = [Float](repeating: 0, count: first.count)
        for channel in channels {
            for i in 0..<min(mix.count, channel.count) { mix[i] += channel[i] * scale }
        }
        return mix
    }

    /// Averages interleaved frames into one channel.
    /// - Parameters:
    ///   - interleaved: Samples laid out `c0 c1 … c0 c1 …`.
    ///   - channelCount: Channels per frame (≥ 1).
    /// - Returns: The mono mix, one sample per frame.
    public static func downmix(interleaved: [Float], channelCount: Int) -> [Float] {
        let n = max(channelCount, 1)
        if n == 1 { return interleaved }
        let scale = 1 / Float(n)
        return (0..<(interleaved.count / n)).map { frame in
            var sum: Float = 0
            for c in 0..<n { sum += interleaved[frame * n + c] }
            return sum * scale
        }
    }

    /// Runs the FIR over `input`, continuing from the stored history.
    private mutating func filter(_ input: [Float]) -> [Float] {
        if taps.count == 1 { return input }
        let padded = history + input
        let n = taps.count
        var out = [Float](repeating: 0, count: input.count)
        padded.withUnsafeBufferPointer { x in
            taps.withUnsafeBufferPointer { h in
                for i in 0..<input.count {
                    var acc: Float = 0
                    // padded[i + n − 1] is input[i]; the taps are symmetric, so direction does not matter.
                    for k in 0..<n { acc += h[k] * x[i + k] }
                    out[i] = acc
                }
            }
        }
        history = Array(padded.suffix(n - 1))
        return out
    }

    /// Designs a Blackman-windowed sinc low-pass with unit DC gain.
    /// - Parameters:
    ///   - cutoff: Cutoff as a fraction of the input rate (0…0.5).
    ///   - ratio: Decimation ratio; longer filters for steeper ratios.
    /// - Returns: Odd-length symmetric taps.
    private static func lowPass(cutoff: Double, ratio: Double) -> [Float] {
        let half = max(8, Int((8 * ratio).rounded(.up)))
        let n = 2 * half + 1
        var taps = (0..<n).map { i -> Double in
            let m = Double(i - half)
            let sinc = m == 0 ? 2 * cutoff : sin(2 * Double.pi * cutoff * m) / (Double.pi * m)
            let w = 0.42 - 0.5 * cos(2 * Double.pi * Double(i) / Double(n - 1)) + 0.08 * cos(4 * Double.pi * Double(i) / Double(n - 1))
            return sinc * w
        }
        let sum = taps.reduce(0, +)
        taps = taps.map { $0 / sum }
        return taps.map(Float.init)
    }
}
