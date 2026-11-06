import Foundation

/// Off-the-shelf arrays whose channel layout is known only from their firmware.
///
/// Multichannel devices are never matched by name: ReSpeaker firmwares disagree on where the raw
/// mics are (XVF3800/Flex 6-channel builds use CH2–5, XVF3000 uses CH1–4 and puts playback on
/// CH5), and pairing a mic with a processed or playback channel would yield confident nonsense.
public enum SpatialArrayPreset: String, Codable, Sendable, CaseIterable {
    /// Seeed ReSpeaker Flex, linear 4-mic board, firmware `respeaker_flex_ua-io16-6ch-lin`:
    /// 16 kHz, CH0 conference, CH1 ASR, CH2–5 raw Mic0–3 at 33 mm pitch.
    case respeakerFlexLinear6ch = "respeaker-flex-linear-6ch"
}

/// Why a device cannot be used with the requested preset.
public enum SpatialArrayProfileError: Error, Equatable, CustomStringConvertible {
    case notAMicArray(device: String)
    case channelCountMismatch(device: String, preset: SpatialArrayPreset, expected: Int, actual: Int)
    case sampleRateMismatch(device: String, preset: SpatialArrayPreset, expected: Double, actual: Double)
    case bufferShape(samples: Int, channels: Int, needed: Int)

    /// Human-readable message carrying the device and the offending value.
    public var description: String {
        switch self {
        case let .notAMicArray(device):
            "device \"\(device)\" is virtual or aggregate, not a microphone array"
        case let .channelCountMismatch(device, preset, expected, actual):
            "device \"\(device)\" has \(actual) input channels, preset \(preset.rawValue) needs \(expected); flash the matching firmware"
        case let .sampleRateMismatch(device, preset, expected, actual):
            "device \"\(device)\" runs at \(Int(actual)) Hz, preset \(preset.rawValue) needs \(Int(expected)) Hz"
        case let .bufferShape(samples, channels, needed):
            "interleaved buffer of \(samples) samples does not split into \(channels) channels with channel \(needed) present"
        }
    }
}

/// Which two channels of a capture device feed ``StereoCueAnalyzer``, and at what rate.
public struct SpatialArrayProfile: Sendable, Equatable {
    /// Zero-based input channel used as analyzer channel `a`.
    public var channelA: Int
    /// Zero-based input channel used as analyzer channel `b`.
    public var channelB: Int
    /// Distance between the two chosen mics when the hardware fixes it; `nil` for unknown geometry.
    /// Lets a caller turn delays into absolute azimuths; the relative cues never need it.
    public var spacingMeters: Float?
    /// Analyzer settings matched to the device rate.
    public var analyzerConfig: StereoCueConfig

    /// Picks the spatial pair for a capture device.
    /// - Parameters:
    ///   - device: The input as reported by ``AudioInputProbe``.
    ///   - preset: Explicit firmware layout for a multichannel array; `nil` to auto-detect.
    ///   - testDeviceUID: UID of a loopback device used as a test rig (see the body).
    /// - Returns: A profile, or `nil` when the device carries no usable pair (mono, virtual, or a
    ///   multichannel device without a preset).
    /// - Throws: ``SpatialArrayProfileError`` when `preset` is given but the device does not match it.
    public static func resolve(for device: AudioInputInfo, preset: SpatialArrayPreset? = nil,
                               testDeviceUID: String? = nil) throws -> SpatialArrayProfile? {
        guard let preset else {
            // Virtual devices are never mic arrays in normal use; the only exception is a test
            // rig that feeds a loopback device a synthetic stereo signal and names its UID
            // (`MM_SPATIAL_TEST_DEVICE_UID`, never set by the app).
            let isTestRig = testDeviceUID != nil && device.uid == testDeviceUID
            // Only a plain stereo device is unambiguous; anything wider needs a preset.
            guard device.supportsSpatialCues || isTestRig, device.channels == 2 else { return nil }
            return SpatialArrayProfile(channelA: 0, channelB: 1, spacingMeters: nil,
                                       analyzerConfig: config(sampleRate: Float(device.sampleRate)))
        }
        guard !device.isVirtual else { throw SpatialArrayProfileError.notAMicArray(device: device.name) }
        switch preset {
        case .respeakerFlexLinear6ch:
            try require(device, preset, channels: 6, sampleRate: 16_000)
            // End mics (Mic0, Mic3) give the widest aperture: 3 × 33.3 mm.
            return SpatialArrayProfile(channelA: 2, channelB: 5, spacingMeters: 0.0999,
                                       analyzerConfig: config(sampleRate: 16_000))
        }
    }

    /// Applies the user's microphone settings.
    ///
    /// Flipping swaps the two channels, which mirrors every cue (time and level difference
    /// change sign together) without the analyzer or fusion knowing. A spacing the hardware
    /// preset already fixes is kept: the firmware layout is exact, a typed value is not.
    /// - Parameters:
    ///   - flip: Swap left and right.
    ///   - spacingCm: Distance between the two mics in centimetres; `nil` or ≤ 0 for unknown.
    /// - Returns: The adjusted profile.
    public func adjusted(flip: Bool, spacingCm: Double?) -> SpatialArrayProfile {
        var p = self
        if flip { swap(&p.channelA, &p.channelB) }
        if p.spacingMeters == nil, let cm = spacingCm, cm > 0, cm.isFinite { p.spacingMeters = Float(cm / 100) }
        return p
    }

    /// Absolute direction of a delay between the pair, when the spacing is known.
    /// - Parameter delaySamples: GCC-PHAT delay of channel b relative to a, at the analyzer rate.
    /// - Returns: Degrees from straight ahead, positive towards channel b (right, matching
    ///   ``StereoCueAnalyzer/reportedPosition(of:)``), or `nil` without a spacing.
    public func azimuthDeg(delaySamples: Float) -> Float? {
        GCCPHAT.azimuth(delaySamples: delaySamples, sampleRate: analyzerConfig.sampleRate,
                        spacingMeters: spacingMeters, speedOfSound: analyzerConfig.speedOfSound).map { -$0 }
    }

    /// Splits the chosen pair out of an interleaved capture buffer.
    /// - Parameters:
    ///   - samples: Frame-interleaved samples (`channels` values per frame).
    ///   - channels: Channel count of the buffer.
    /// - Returns: Channel `a` and channel `b`, one value per frame.
    /// - Throws: ``SpatialArrayProfileError/bufferShape(samples:channels:needed:)`` when the buffer
    ///   is ragged or lacks one of the chosen channels.
    public func pair(fromInterleaved samples: [Float], channels: Int) throws -> ([Float], [Float]) {
        let needed = max(channelA, channelB)
        guard channels > needed, samples.count % channels == 0 else {
            throw SpatialArrayProfileError.bufferShape(samples: samples.count, channels: channels, needed: needed)
        }
        let frames = samples.count / channels
        var a = [Float](repeating: 0, count: frames), b = a
        for f in 0..<frames {
            a[f] = samples[f * channels + channelA]
            b[f] = samples[f * channels + channelB]
        }
        return (a, b)
    }

    /// Analyzer settings for a sample rate.
    /// - Parameter sampleRate: Device rate in Hz.
    /// - Returns: Defaults with the rate set and a power-of-two frame of roughly 40–64 ms,
    ///   long enough for a stable GCC-PHAT peak but short enough to follow turn-taking.
    static func config(sampleRate: Float) -> StereoCueConfig {
        var config = StereoCueConfig()
        config.sampleRate = sampleRate
        config.frameCount = sampleRate <= 24_000 ? 1024 : 2048
        return config
    }

    /// Checks that a device runs the firmware a preset describes.
    /// - Throws: ``SpatialArrayProfileError`` naming the device and the mismatched value.
    private static func require(_ device: AudioInputInfo, _ preset: SpatialArrayPreset, channels: Int, sampleRate: Double) throws {
        guard device.channels == channels else {
            throw SpatialArrayProfileError.channelCountMismatch(device: device.name, preset: preset,
                                                                 expected: channels, actual: device.channels)
        }
        guard device.sampleRate == sampleRate else {
            throw SpatialArrayProfileError.sampleRateMismatch(device: device.name, preset: preset,
                                                               expected: sampleRate, actual: device.sampleRate)
        }
    }
}
