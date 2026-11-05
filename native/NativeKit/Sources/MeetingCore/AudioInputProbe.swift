import CoreAudio
import Foundation

/// An audio input device and what it can contribute to speaker identification.
public struct AudioInputInfo: Codable, Sendable, Equatable {
    public var uid: String
    public var name: String
    public var channels: Int
    public var sampleRate: Double
    /// Virtual (loopback drivers such as meeting apps' audio devices) or aggregate device.
    public var isVirtual: Bool

    /// Whether direction-of-arrival is possible: ≥ 2 channels from real microphones.
    /// Virtual loopback devices often report 2 channels but carry mixed app audio, not an array.
    public var supportsSpatialCues: Bool { channels >= 2 && !isVirtual }

    /// Classifies a Core Audio transport type.
    /// - Parameter transport: `kAudioDevicePropertyTransportType` value.
    /// - Returns: `true` for virtual and aggregate devices.
    public static func isVirtualTransport(_ transport: UInt32) -> Bool {
        transport == kAudioDeviceTransportTypeVirtual || transport == kAudioDeviceTransportTypeAggregate
            || transport == kAudioDeviceTransportTypeAutoAggregate
    }
}

/// Enumerates Core Audio input devices.
///
/// Note: on current MacBook Pros the built-in array is exposed as a single, already
/// beamformed channel, so spatial cues need a multichannel USB/conference mic (or a
/// stereo device such as AR glasses). AVFoundation's first-order-ambisonics capture is
/// also unsupported on the built-in mic (verified with permission granted, macOS 27.2);
/// `scripts/probe-spatial-capture.swift` re-checks it after OS or hardware changes.
public enum AudioInputProbe {
    /// Lists every device that has input channels.
    /// - Returns: Devices sorted by name.
    public static func inputs() -> [AudioInputInfo] {
        deviceIDs().compactMap(info(for:)).sorted { $0.name < $1.name }
    }

    /// Resolves a device UID (stable across reboots) to the current Core Audio object ID.
    /// - Parameter uid: `kAudioDevicePropertyDeviceUID`, as reported by `inputs()`.
    /// - Returns: The device ID, or `nil` when no input device has that UID.
    public static func deviceID(forUID uid: String) -> AudioObjectID? {
        deviceIDs().first { stringProperty($0, kAudioDevicePropertyDeviceUID) == uid && inputChannelCount($0) > 0 }
    }

    private static func deviceIDs() -> [AudioObjectID] {
        var address = AudioObjectPropertyAddress(
            mSelector: kAudioHardwarePropertyDevices, mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain)
        var size: UInt32 = 0
        guard AudioObjectGetPropertyDataSize(AudioObjectID(kAudioObjectSystemObject), &address, 0, nil, &size) == noErr
        else { return [] }
        var ids = [AudioObjectID](repeating: 0, count: Int(size) / MemoryLayout<AudioObjectID>.size)
        guard AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &address, 0, nil, &size, &ids) == noErr
        else { return [] }
        return ids
    }

    private static func info(for id: AudioObjectID) -> AudioInputInfo? {
        let channels = inputChannelCount(id)
        guard channels > 0 else { return nil }
        return AudioInputInfo(
            uid: stringProperty(id, kAudioDevicePropertyDeviceUID) ?? "\(id)",
            name: stringProperty(id, kAudioObjectPropertyName) ?? "Device \(id)",
            channels: channels,
            sampleRate: nominalSampleRate(id),
            isVirtual: AudioInputInfo.isVirtualTransport(transportType(id)))
    }

    private static func inputChannelCount(_ id: AudioObjectID) -> Int {
        var address = AudioObjectPropertyAddress(
            mSelector: kAudioDevicePropertyStreamConfiguration, mScope: kAudioDevicePropertyScopeInput,
            mElement: kAudioObjectPropertyElementMain)
        var size: UInt32 = 0
        guard AudioObjectGetPropertyDataSize(id, &address, 0, nil, &size) == noErr, size > 0 else { return 0 }
        let raw = UnsafeMutableRawPointer.allocate(byteCount: Int(size), alignment: MemoryLayout<AudioBufferList>.alignment)
        defer { raw.deallocate() }
        guard AudioObjectGetPropertyData(id, &address, 0, nil, &size, raw) == noErr else { return 0 }
        let list = UnsafeMutableAudioBufferListPointer(raw.assumingMemoryBound(to: AudioBufferList.self))
        return list.reduce(0) { $0 + Int($1.mNumberChannels) }
    }

    private static func stringProperty(_ id: AudioObjectID, _ selector: AudioObjectPropertySelector) -> String? {
        var address = AudioObjectPropertyAddress(
            mSelector: selector, mScope: kAudioObjectPropertyScopeGlobal, mElement: kAudioObjectPropertyElementMain)
        var value: Unmanaged<CFString>?
        var size = UInt32(MemoryLayout<Unmanaged<CFString>?>.size)
        guard AudioObjectGetPropertyData(id, &address, 0, nil, &size, &value) == noErr, let value else { return nil }
        return value.takeRetainedValue() as String
    }

    private static func transportType(_ id: AudioObjectID) -> UInt32 {
        var address = AudioObjectPropertyAddress(
            mSelector: kAudioDevicePropertyTransportType, mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain)
        var value: UInt32 = 0
        var size = UInt32(MemoryLayout<UInt32>.size)
        return AudioObjectGetPropertyData(id, &address, 0, nil, &size, &value) == noErr ? value : 0
    }

    private static func nominalSampleRate(_ id: AudioObjectID) -> Double {
        var address = AudioObjectPropertyAddress(
            mSelector: kAudioDevicePropertyNominalSampleRate, mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain)
        var rate: Float64 = 0
        var size = UInt32(MemoryLayout<Float64>.size)
        return AudioObjectGetPropertyData(id, &address, 0, nil, &size, &rate) == noErr ? rate : 0
    }
}
