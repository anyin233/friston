import Foundation
import Testing
@testable import MeetingCore

private let usbStereo = AudioInputInfo(uid: "usb", name: "XREAL One Pro", channels: 2, sampleRate: 48_000, isVirtual: false)
private let builtIn = AudioInputInfo(uid: "BuiltInMicrophoneDevice", name: "MacBook Pro Microphone", channels: 1,
                                     sampleRate: 48_000, isVirtual: false)

@Test func presetDecodesTheWireShape() throws {
    // Same JSON as the Rust round-trip test in crates/mm-core/src/capture.rs.
    let json = #"{"title":"周会","windowID":4242,"microphoneUID":"usb","systemAudio":true,"captionLanguage":"zh","fps":15,"spatialFlip":true,"spatialSpacingCm":14.5}"#
    let preset = try JSONDecoder().decode(CapturePreset.self, from: Data(json.utf8))
    #expect(preset.windowID == 4242)
    #expect(preset.spatialFlip && preset.spatialSpacingCm == 14.5)
    #expect(preset.displayID == nil)
    #expect(preset.microphoneUID == "usb")
    #expect(preset.systemAudio)
    #expect(preset.fps == 15)
}

@Test func presetDefaultsAreAudioOff() throws {
    let preset = try JSONDecoder().decode(CapturePreset.self, from: Data(#"{"windowID":1}"#.utf8))
    #expect(!preset.systemAudio)
    #expect(preset.microphoneUID == nil)
    #expect(preset.fps == nil)
    #expect(!preset.spatialFlip && preset.spatialSpacingCm == nil)
}

@Test func validationRejectsEmptyAndAmbiguousPresets() {
    #expect(throws: CapturePlanError.nothingToRecord) {
        try CapturePlan.make(preset: CapturePreset(), inputs: [builtIn])
    }
    #expect(throws: CapturePlanError.windowAndDisplay) {
        try CapturePlan.make(preset: CapturePreset(windowID: 1, displayID: 2), inputs: [builtIn])
    }
    #expect(throws: CapturePlanError.unknownMicrophone("nope")) {
        try CapturePlan.make(preset: CapturePreset(microphoneUID: "nope"), inputs: [builtIn])
    }
    #expect(throws: CapturePlanError.invalidFPS(0)) {
        try CapturePlan.make(preset: CapturePreset(windowID: 1, fps: 0), inputs: [])
    }
}

@Test func planKeepsEveryMicChannelAndNamesFiles() throws {
    let start = try Date("2026-11-05T09:30:00Z", strategy: .iso8601)
    let plan = try CapturePlan.make(
        preset: CapturePreset(title: "产品周会 / Q4", windowID: 7, microphoneUID: "usb", systemAudio: true),
        inputs: [builtIn, usbStereo], baseDirectory: URL(filePath: "/tmp/mm"), start: start,
        timeZone: TimeZone(identifier: "Asia/Shanghai")!)
    #expect(plan.microphone == usbStereo)
    #expect(plan.fps == CapturePlan.defaultFPS)
    // Slashes would create nested folders; they are replaced, the Chinese title is kept.
    #expect(plan.directory.path == "/tmp/mm/2026-11-05 1730 产品周会 - Q4")
    #expect(plan.files.screen?.lastPathComponent == "screen.mov")
    #expect(plan.files.microphone?.lastPathComponent == "mic.caf")
    #expect(plan.files.systemAudio?.lastPathComponent == "system.caf")
}

@Test func audioOnlyPlanHasNoScreenFile() throws {
    let plan = try CapturePlan.make(preset: CapturePreset(systemAudio: true), inputs: [],
                                    baseDirectory: URL(filePath: "/tmp/mm"),
                                    start: Date(timeIntervalSince1970: 0), timeZone: .gmt)
    #expect(plan.files.screen == nil)
    #expect(plan.files.microphone == nil)
    #expect(plan.directory.lastPathComponent == "1970-01-01 0000 会议")
}

@Test func levelMeterReportsPerChannelDecibels() {
    // Channel 0: full-scale square wave (0 dBFS); channel 1: half amplitude (≈ −6 dBFS); channel 2: silence.
    let n = 480
    let ch0 = (0..<n).map { $0 % 2 == 0 ? Float(1) : -1 }
    let ch1 = ch0.map { $0 * 0.5 }
    let ch2 = [Float](repeating: 0, count: n)
    let levels = LevelMeter.measure(channels: [ch0, ch1, ch2])
    #expect(abs(levels.rmsDb[0]) < 0.01)
    #expect(abs(levels.peakDb[1] + 6.02) < 0.01)
    #expect(levels.rmsDb[2] == LevelMeter.floorDb)
}

@Test func levelMeterReadsInterleavedSamples() {
    let interleaved: [Float] = [1, 0, -1, 0, 1, 0, -1, 0]
    let levels = LevelMeter.measure(interleaved: interleaved, channelCount: 2)
    #expect(levels.rmsDb.count == 2)
    #expect(abs(levels.rmsDb[0]) < 0.01)
    #expect(levels.rmsDb[1] == LevelMeter.floorDb)
}

@Test func throttleEmitsAtMostOncePerInterval() {
    var throttle = Throttle(interval: 0.1)
    let fired = [10.00, 10.05, 10.10, 10.19].map { throttle.shouldFire(at: $0) }
    #expect(fired == [true, false, true, false])
}

@Test func stateMachineAllowsOneSessionAtATime() throws {
    var machine = CaptureStateMachine()
    #expect(machine.state == .idle)
    try machine.transition(to: .starting)
    #expect(throws: CaptureStateError.self) { try machine.transition(to: .starting) }
    try machine.transition(to: .recording)
    try machine.transition(to: .stopping)
    try machine.transition(to: .stopped)
    // A finished session frees the engine for the next one.
    try machine.transition(to: .starting)
    try machine.transition(to: .failed)
    try machine.transition(to: .starting)
}

@Test func stopIsOnlyValidWhileRecording() {
    var machine = CaptureStateMachine()
    #expect(throws: CaptureStateError.invalidTransition(from: .idle, to: .stopping)) {
        try machine.transition(to: .stopping)
    }
}

@Test func eventsEncodeTheWireShape() throws {
    let encoder = JSONEncoder()
    encoder.outputFormatting = [.sortedKeys, .withoutEscapingSlashes]
    let state = try encoder.encode(CaptureStateEvent(sessionId: "s1", state: .recording))
    #expect(String(decoding: state, as: UTF8.self) == #"{"sessionId":"s1","state":"recording"}"#)
    let levels = try encoder.encode(AudioLevelsEvent(sessionId: "s1", source: .system,
                                                     levels: ChannelLevels(rmsDb: [-20], peakDb: [-6])))
    #expect(String(decoding: levels, as: UTF8.self) == #"{"peakDb":[-6],"rmsDb":[-20],"sessionId":"s1","source":"system"}"#)
    let files = try encoder.encode(CaptureFiles(microphone: URL(filePath: "/tmp/a b/mic.caf")))
    // Plain paths, not file:// URLs, so the Rust side reads them as PathBuf.
    #expect(String(decoding: files, as: UTF8.self) == #"{"microphone":"/tmp/a b/mic.caf"}"#)
}
