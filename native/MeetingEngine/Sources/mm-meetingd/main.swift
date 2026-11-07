import EngineKit
import Foundation
import MeetingCore

// mm-meetingd: newline-delimited JSON-RPC 2.0 over stdio. Logs go to stderr.

/// The protocol sink. speech-swift `print`s model-loading progress, so the real stdout is
/// reserved for protocol lines and descriptor 1 is pointed at stderr before any model loads.
let protocolSink: @Sendable (Data) -> Void = {
    do { return StdoutGuard.sink(try StdoutGuard.reserve()) } catch {
        log("stdout guard unavailable, library output may corrupt the protocol: \(error)")
        return RPCServer.stdout
    }
}()
let server = RPCServer(sink: protocolSink)
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
            "liveCaptions": .string("Qwen3-ASR 1.7B 4bit per Silero-VAD utterance, partials every 1 s"),
            "diarization": .string("live: Sortformer streaming turns + WeSpeaker embeddings + spatial fusion; file: Pyannote + WeSpeaker clustering"),
            "models": .object([
                "liveASR": .string(SpeechModelIDs.liveASR),
                "offlineASR": .string(HubCacheLocator.snapshot(repo: SpeechModelIDs.offlineASRHub) != nil
                    ? SpeechModelIDs.offlineASRHub : SpeechModelIDs.offlineASRDownload),
                "vad": .string(SpeechModelIDs.vad),
                "speakerEmbedding": .string(SpeechModelIDs.speakerEmbedding),
                "segmentation": .string(SpeechModelIDs.segmentation),
                "liveDiarization": .string(SpeechModelIDs.turnDiarizer),
            ]),
            "speakerEmbeddingModel": .string(SpeechModelIDs.speakerEmbedding),
            "spatialInputs": .array(inputs.filter(\.supportsSpatialCues).map { .string($0.name) }),
        ]),
    ])
}

await server.register(Methods.Meeting.audioProbeInputs) { _ in
    try JSONValue.from(AudioInputProbe.inputs())
}

await server.register(Methods.Meeting.permissionsStatus) { _ in
    try JSONValue.from(Permissions.status())
}

await server.register(Methods.Meeting.permissionsRequest) { params in
    guard let kind = params?["kind"]?.stringValue else { throw RPCError.invalidParams("permissions/request needs `kind`") }
    return try JSONValue.from(try await Permissions.request(kind))
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

await server.register(Methods.Meeting.voiceprintFromClip) { params in
    let p = try (params ?? .object([:])).decode(VoiceprintFromClipParams.self)
    return try JSONValue.from(try await SpeechRequests.fromClip(p))
}

await server.register(Methods.Meeting.voiceprintEnroll) { params in
    let p = try (params ?? .object([:])).decode(VoiceprintEnrollParams.self)
    return try JSONValue.from(try await SpeechRequests.enroll(p))
}

await server.register(Methods.Meeting.transcribeFile) { params in
    let p = try (params ?? .object([:])).decode(TranscribeFileParams.self)
    return try JSONValue.from(try await SpeechRequests.transcribeFile(p))
}

await server.register(Methods.Meeting.captionSimulateFile) { params in
    let p = try (params ?? .object([:])).decode(SimulateFileParams.self)
    return try JSONValue.from(try await CaptionSimulator.run(p, notify: { method, params in
        Task { await server.notify(method, params) }
    }))
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
