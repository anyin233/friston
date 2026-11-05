// Probes whether macOS can capture spatial audio from this Mac's microphones.
//
// Usage: swift scripts/probe-spatial-capture.swift
//
// For every audio capture device it reports whether AVFoundation accepts the stereo and
// first-order-ambisonics (FOA) multichannel modes, checked again with the input attached to a
// session, plus every physical stream format Core Audio offers. Re-run after macOS upgrades or
// on new Mac hardware: if `foa=true` ever appears for the built-in mic, the system array can
// feed direction-of-arrival directly. Needs microphone permission for the calling terminal.
import AVFoundation
import CoreAudio

/// Waits for microphone permission, prompting when it has not been decided yet.
/// - Returns: Whether capture is authorised.
func ensureMicrophoneAccess() -> Bool {
    switch AVCaptureDevice.authorizationStatus(for: .audio) {
    case .authorized: return true
    case .notDetermined:
        let done = DispatchSemaphore(value: 0)
        var granted = false
        AVCaptureDevice.requestAccess(for: .audio) { granted = $0; done.signal() }
        _ = done.wait(timeout: .now() + 60)
        return granted
    default: return false
    }
}

/// Lists the distinct "<channels>ch@<rate>" physical formats of a Core Audio device's inputs.
/// - Parameter uid: Core Audio device UID (equals `AVCaptureDevice.uniqueID` on macOS).
/// - Returns: Sorted format labels, or an empty list when the device cannot be resolved.
func physicalInputFormats(uid: String) -> [String] {
    var address = AudioObjectPropertyAddress(
        mSelector: kAudioHardwarePropertyTranslateUIDToDevice, mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain)
    // The qualifier is a CFString reference; pass it as Unmanaged so no object is copied as raw bytes.
    let uidRef = uid as CFString
    var cfUID = Unmanaged.passUnretained(uidRef)
    var device = AudioObjectID(0)
    var size = UInt32(MemoryLayout<AudioObjectID>.size)
    guard AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &address,
                                     UInt32(MemoryLayout<Unmanaged<CFString>>.size), &cfUID, &size, &device) == noErr,
          device != 0 else { return [] }
    withExtendedLifetime(uidRef) {}
    address = AudioObjectPropertyAddress(
        mSelector: kAudioDevicePropertyStreams, mScope: kAudioDevicePropertyScopeInput,
        mElement: kAudioObjectPropertyElementMain)
    guard AudioObjectGetPropertyDataSize(device, &address, 0, nil, &size) == noErr, size > 0 else { return [] }
    var streams = [AudioStreamID](repeating: 0, count: Int(size) / MemoryLayout<AudioStreamID>.size)
    guard AudioObjectGetPropertyData(device, &address, 0, nil, &size, &streams) == noErr else { return [] }
    var labels = Set<String>()
    for stream in streams {
        var fmtAddress = AudioObjectPropertyAddress(
            mSelector: kAudioStreamPropertyAvailablePhysicalFormats, mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain)
        var fmtSize: UInt32 = 0
        guard AudioObjectGetPropertyDataSize(stream, &fmtAddress, 0, nil, &fmtSize) == noErr else { continue }
        var formats = [AudioStreamRangedDescription](
            repeating: .init(), count: Int(fmtSize) / MemoryLayout<AudioStreamRangedDescription>.size)
        guard AudioObjectGetPropertyData(stream, &fmtAddress, 0, nil, &fmtSize, &formats) == noErr else { continue }
        for f in formats { labels.insert("\(f.mFormat.mChannelsPerFrame)ch@\(Int(f.mFormat.mSampleRate))") }
    }
    return labels.sorted()
}

guard ensureMicrophoneAccess() else {
    FileHandle.standardError.write(Data("microphone permission denied: grant it to this terminal in System Settings\n".utf8))
    exit(2)
}

let devices = AVCaptureDevice.DiscoverySession(
    deviceTypes: [.microphone], mediaType: .audio, position: .unspecified).devices
for device in devices {
    let input: AVCaptureDeviceInput
    do {
        input = try AVCaptureDeviceInput(device: device)
    } catch {
        print("\(device.localizedName) [\(device.uniqueID)]: cannot open input: \(error)")
        continue
    }
    let before = (input.isMultichannelAudioModeSupported(.stereo), input.isMultichannelAudioModeSupported(.firstOrderAmbisonics))
    // Some capabilities only resolve once the input belongs to a session, so ask again there.
    let session = AVCaptureSession()
    session.beginConfiguration()
    if session.canAddInput(input) { session.addInput(input) }
    session.commitConfiguration()
    let after = (input.isMultichannelAudioModeSupported(.stereo), input.isMultichannelAudioModeSupported(.firstOrderAmbisonics))
    print("\(device.localizedName) [\(device.uniqueID)]")
    print("  standalone: stereo=\(before.0) foa=\(before.1)   in session: stereo=\(after.0) foa=\(after.1)")
    print("  physical input formats: \(physicalInputFormats(uid: device.uniqueID).joined(separator: ", "))")
}
