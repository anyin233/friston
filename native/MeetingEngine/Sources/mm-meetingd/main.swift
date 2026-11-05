import EngineKit
import Foundation
import MeetingCore
import Qwen3ASR
import SpeechVAD

// mm-meetingd: newline-delimited JSON-RPC 2.0 over stdio. Logs go to stderr.

let server = RPCServer()
let calendar = CalendarService()
let capture = CaptureController(notify: { method, params in
    Task { await server.notify(method, params) }
})

/// Encodes a result with ISO-8601 dates (JSONValue.from would emit seconds since 2001).
/// - Parameter value: Any encodable result.
/// - Returns: The JSON tree.
func isoJSON<T: Encodable>(_ value: T) throws -> JSONValue {
    let encoder = JSONEncoder()
    encoder.dateEncodingStrategy = .iso8601
    return try JSONDecoder().decode(JSONValue.self, from: try encoder.encode(value))
}

await server.register(Methods.initialize) { _ in
    let inputs = AudioInputProbe.inputs()
    return .object([
        "name": .string("mm-meetingd"),
        "version": .string("0.1.0"),
        "capabilities": .object([
            "windowCapture": .bool(true),
            "capture": .string("window/display HEVC + multichannel mic + system audio"),
            "liveCaptions": .string("planned: Qwen3-ASR streaming (speech-swift)"),
            "diarization": .string("planned: \(String(describing: SortformerStreamingSession.self)) + speaker embeddings"),
            "spatialInputs": .array(inputs.filter(\.supportsSpatialCues).map { .string($0.name) }),
        ]),
    ])
}

await server.register(Methods.Meeting.audioProbeInputs) { _ in
    try JSONValue.from(AudioInputProbe.inputs())
}

await server.register(Methods.Meeting.captureListSources) { _ in
    try JSONValue.from(try await listCaptureSources())
}

await server.register(Methods.Meeting.calendarEvents) { params in
    guard let fromText = params?["from"]?.stringValue, let toText = params?["to"]?.stringValue,
          // ISO8601FormatStyle is Sendable, unlike ISO8601DateFormatter, so it is safe in concurrent handlers.
          let from = try? Date.ISO8601FormatStyle().parse(fromText),
          let to = try? Date.ISO8601FormatStyle().parse(toText)
    else { throw RPCError.invalidParams("calendar/events needs ISO-8601 `from` and `to`") }
    return try isoJSON(try await calendar.events(from: from, to: to))
}

await server.register(Methods.Meeting.captureStart) { params in
    let preset = try (params ?? .object([:])).decode(CapturePreset.self)
    return try isoJSON(try await capture.start(preset))
}

await server.register(Methods.Meeting.captureStop) { _ in
    try isoJSON(try await capture.stop())
}

for method in [
    Methods.Meeting.voiceprintEnroll, Methods.Meeting.voiceprintFromClip, Methods.Meeting.transcribeFile,
] {
    await server.register(method) { _ in throw RPCError.notImplemented(method) }
}

await server.register(Methods.shutdown) { _ in
    // Finalize recordings first; an unfinished screen.mov has no moov atom and is unreadable.
    _ = try? await capture.stop()
    // Exit only after every in-flight request (including this one) has replied.
    Task {
        try? await Task.sleep(for: .milliseconds(5))
        await server.waitUntilIdle()
        exit(0)
    }
    return .null
}

log("mm-meetingd ready")
await server.runStdio()
