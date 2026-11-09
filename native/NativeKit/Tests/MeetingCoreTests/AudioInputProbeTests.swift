import CoreAudio
import Testing
@testable import MeetingCore

@Test func virtualStereoDevicesAreNotMicArrays() {
    let loopback = AudioInputInfo(uid: "WMAudioPluginDevice_UID", name: "WeMeet Audio Device", channels: 2, sampleRate: 48_000,
                                  isVirtual: AudioInputInfo.isVirtualTransport(kAudioDeviceTransportTypeVirtual))
    let usbStereo = AudioInputInfo(uid: "usb", name: "XREAL One Pro", channels: 2, sampleRate: 48_000,
                                   isVirtual: AudioInputInfo.isVirtualTransport(kAudioDeviceTransportTypeUSB))
    let builtIn = AudioInputInfo(uid: "BuiltInMicrophoneDevice", name: "MacBook Pro Microphone", channels: 1, sampleRate: 48_000,
                                 isVirtual: AudioInputInfo.isVirtualTransport(kAudioDeviceTransportTypeBuiltIn))
    #expect(!loopback.supportsSpatialCues)
    #expect(usbStereo.supportsSpatialCues)
    #expect(!builtIn.supportsSpatialCues)
}

@Test func tapRatePrefersTheDevicesLiveRate() {
    // AirPods in the hands-free profile: the engine still says 48 kHz, the stream runs at 24 kHz.
    #expect(AudioInputProbe.tapSampleRate(engineReported: 48_000, device: 24_000) == 24_000)
    #expect(AudioInputProbe.tapSampleRate(engineReported: 48_000, device: 48_000) == 48_000)
    // Unreadable device rate: keep what the engine reported.
    #expect(AudioInputProbe.tapSampleRate(engineReported: 44_100, device: 0) == 44_100)
}

@Test func liveRateOfTheDefaultInputIsReadable() throws {
    // Machines without any input (CI) have nothing to read.
    guard let uid = AudioInputProbe.defaultInputUID(), let id = AudioInputProbe.deviceID(forUID: uid) else { return }
    #expect(AudioInputProbe.inputStreamRate(id) > 0)
}
