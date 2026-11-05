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
