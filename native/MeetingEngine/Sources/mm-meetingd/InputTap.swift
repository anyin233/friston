import AVFoundation
import EngineKit
import MeetingCore

/// Taps on a microphone input node that cannot crash the engine.
///
/// `AVAudioNode.installTap` raises an Objective-C exception (which Swift cannot catch, so the
/// whole process dies) when the tap format's rate differs from the hardware's. That happens
/// with Bluetooth headsets: after the device is selected the engine still reports the 48 kHz
/// it saw before, while the headset already runs its hands-free profile at 16 or 24 kHz.
enum InputTap {
    /// Frames per tap callback.
    static let bufferSize: AVAudioFrameCount = 4096

    /// The format to tap a selected input with: the engine's format at the device's live rate.
    /// - Parameters:
    ///   - engine: Engine whose input unit already points at the device.
    ///   - uid: Device UID, or `nil` for the system default input.
    /// - Returns: The engine's own format when the rates agree (keeps its channel layout),
    ///   otherwise Float32 non-interleaved with the same channels at the live rate.
    static func format(of engine: AVAudioEngine, uid: String?) -> AVAudioFormat {
        let reported = engine.inputNode.inputFormat(forBus: 0)
        let device = (uid ?? AudioInputProbe.defaultInputUID()).flatMap(AudioInputProbe.deviceID(forUID:))
        let rate = AudioInputProbe.tapSampleRate(engineReported: reported.sampleRate,
                                                 device: device.map(AudioInputProbe.inputStreamRate) ?? 0)
        guard rate != reported.sampleRate else { return reported }
        log("audio input \(uid ?? "default"): engine reports \(reported.sampleRate) Hz, device runs at \(rate) Hz; tapping at \(rate) Hz")
        let channels = reported.channelCount
        // Mono and stereo need no layout; more channels do, or AVAudioFormat returns nil.
        if channels <= 2, let f = AVAudioFormat(commonFormat: .pcmFormatFloat32, sampleRate: rate, channels: channels,
                                                interleaved: false) {
            return f
        }
        let layout = reported.channelLayout
            ?? AVAudioChannelLayout(layoutTag: kAudioChannelLayoutTag_DiscreteInOrder | AudioChannelLayoutTag(channels))
        if let layout {
            return AVAudioFormat(commonFormat: .pcmFormatFloat32, sampleRate: rate, interleaved: false, channelLayout: layout)
        }
        return reported
    }

    /// Installs a tap, reporting a format mismatch as an error instead of crashing.
    /// - Parameters:
    ///   - node: The engine's input node.
    ///   - format: From ``format(of:uid:)``.
    ///   - device: Device name or UID, for the error message.
    ///   - handler: Called on the audio thread with each buffer.
    /// - Throws: ``RecorderError/microphoneTap(_:_:)`` when the tap is refused (macOS 27 and later;
    ///   earlier systems only have the raising API, so the rate check above is all they get).
    static func install(on node: AVAudioInputNode, format: AVAudioFormat, device: String,
                        handler: @escaping @Sendable (AVAudioPCMBuffer) -> Void) throws {
        if #available(macOS 27.0, *) {
            do {
                try node.installAudioTap(onBus: 0, bufferSize: bufferSize, format: format) { buffer, _ in
                    // One copy per callback (~4096 frames) so the writers and captioner keep their API.
                    handler(AVAudioPCMBuffer(copying: buffer))
                }
            } catch {
                throw RecorderError.microphoneTap(device, "\(Int(format.sampleRate)) Hz, \(format.channelCount) ch: \(error)")
            }
        } else {
            node.installTap(onBus: 0, bufferSize: bufferSize, format: format) { buffer, _ in handler(buffer) }
        }
    }
}
