// Spatial test rig, part 1: plays a (stereo) audio file to one output device, keeping channels apart.
// Build: swiftc -O scripts/spatial-rig/play-to-device.swift -o /tmp/play-to-device
// Usage: play-to-device <file> <device-name-substring>   e.g. "WeMeet" (a 2-ch loopback driver)
import AVFoundation
import CoreAudio
// Plays an audio file (any channel count) to the output device whose name contains argv[2].
let file = try AVAudioFile(forReading: URL(fileURLWithPath: CommandLine.arguments[1]))
let want = CommandLine.arguments[2]
var a = AudioObjectPropertyAddress(mSelector: kAudioHardwarePropertyDevices, mScope: kAudioObjectPropertyScopeGlobal, mElement: kAudioObjectPropertyElementMain)
var size: UInt32 = 0; AudioObjectGetPropertyDataSize(AudioObjectID(kAudioObjectSystemObject), &a, 0, nil, &size)
var ids = [AudioObjectID](repeating: 0, count: Int(size)/4); AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &a, 0, nil, &size, &ids)
func name(_ id: AudioObjectID) -> String {
    var na = AudioObjectPropertyAddress(mSelector: kAudioObjectPropertyName, mScope: kAudioObjectPropertyScopeGlobal, mElement: kAudioObjectPropertyElementMain)
    var n: Unmanaged<CFString>?; var s = UInt32(MemoryLayout<Unmanaged<CFString>?>.size)
    AudioObjectGetPropertyData(id, &na, 0, nil, &s, &n); return n?.takeRetainedValue() as String? ?? ""
}
func outChannels(_ id: AudioObjectID) -> Int {
    var pa = AudioObjectPropertyAddress(mSelector: kAudioDevicePropertyStreamConfiguration, mScope: kAudioDevicePropertyScopeOutput, mElement: kAudioObjectPropertyElementMain)
    var s: UInt32 = 0; guard AudioObjectGetPropertyDataSize(id, &pa, 0, nil, &s) == noErr, s > 0 else { return 0 }
    let buf = UnsafeMutableRawPointer.allocate(byteCount: Int(s), alignment: 16); defer { buf.deallocate() }
    AudioObjectGetPropertyData(id, &pa, 0, nil, &s, buf)
    return UnsafeMutableAudioBufferListPointer(buf.assumingMemoryBound(to: AudioBufferList.self)).reduce(0) { $0 + Int($1.mNumberChannels) }
}
guard var dev = ids.first(where: { name($0).contains(want) && outChannels($0) > 0 }) else { fatalError("no output device \(want)") }
let engine = AVAudioEngine()
let unit = engine.outputNode.audioUnit!
let st = AudioUnitSetProperty(unit, kAudioOutputUnitProperty_CurrentDevice, kAudioUnitScope_Global, 0, &dev, UInt32(MemoryLayout<AudioObjectID>.size))
precondition(st == noErr, "set device \(st)")
let player = AVAudioPlayerNode(); engine.attach(player)
engine.connect(player, to: engine.mainMixerNode, format: file.processingFormat)
// Keep channels apart: the mixer must not downmix a stereo source.
engine.connect(engine.mainMixerNode, to: engine.outputNode, format: AVAudioFormat(standardFormatWithSampleRate: engine.outputNode.outputFormat(forBus: 0).sampleRate, channels: 2))
try engine.start()
let done = DispatchSemaphore(value: 0)
player.scheduleFile(file, at: nil) { done.signal() }
player.play(); done.wait(); Thread.sleep(forTimeInterval: 0.5)
print("played \(file.length) frames, \(file.processingFormat.channelCount) ch to \(name(dev))")
