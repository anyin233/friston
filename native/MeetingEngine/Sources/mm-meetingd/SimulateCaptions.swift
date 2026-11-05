import EngineKit
import Foundation
import MeetingCore

/// `caption/simulateFile` params.
struct SimulateFileParams: Decodable {
    var path: String
    /// Stream the file pretends to be: `microphone` (default) or `system`.
    var origin: AudioOrigin?
    /// Playback speed: 1 = real time (default), 4 = four times faster, 0 = as fast as possible.
    var speed: Double?
    var language: String?
    var voiceprints: [VoiceprintSample]?
}

/// `caption/simulateFile` result: what the app would have shown, after all relabels.
struct SimulateFileResult: Encodable {
    var sessionId: String
    var origin: AudioOrigin
    var durationMs: Int
    /// Every `caption/final` sent, in order, with the speaker as finally relabelled.
    var finals: [CaptionFinalEvent]
    var relabels: Int
}

/// Runs the live caption pipeline over an audio file instead of a capture device.
///
/// The file goes through the same ``LiveCaptioner`` code as a real capture (inbox, resampling,
/// VAD, Sortformer, embeddings, fusion), so captions and speakers can be tested without
/// microphone or Screen Recording permission, and system-audio captions without a meeting.
/// Notifications carry a `sim-…` session id and do not touch the capture state machine.
enum CaptionSimulator {
    /// Streams a file through live captions and waits for the last final.
    /// - Parameters:
    ///   - params: File, origin, speed, language and voiceprints.
    ///   - notify: Notification sink (the same one capture uses).
    /// - Returns: The finals and how many relabels were sent.
    /// - Throws: `RPCError` -32602 for a missing file or bad speed, -32000 when the file cannot be
    ///   decoded or the caption models cannot load.
    static func run(_ params: SimulateFileParams, notify: @escaping Notify) async throws -> SimulateFileResult {
        let method = Methods.Meeting.captionSimulateFile
        let speed = params.speed ?? 1
        guard speed >= 0, speed.isFinite else { throw RPCError.invalidParams("\(method): speed must be ≥ 0, got \(speed)") }
        let audio = try SpeechRequests.loadAudio(path: params.path, method: method)
        let rate = Double(SpeechRequests.rate)
        let origin = params.origin ?? .microphone
        let sessionId = "sim-\(UUID().uuidString.prefix(8).lowercased())"
        let seconds = Double(audio.count) / rate

        let collected = LockedValue(Collected())
        let sink: Notify = { method, payload in
            collected.mutate { $0.record(method, payload) }
            notify(method, payload)
        }
        let preset = CapturePreset(captionLanguage: params.language, voiceprints: params.voiceprints)
        // The whole file may arrive at once (speed 0), so the inbox must hold all of it.
        let captioner = LiveCaptioner(
            sessionId: sessionId, preset: preset, microphone: nil,
            micRate: origin == .microphone ? rate : nil, systemRate: origin == .system ? rate : nil,
            maxBacklogSeconds: seconds + 5, notify: sink)
        guard let inbox = origin == .microphone ? captioner.microphone : captioner.system else {
            throw RPCError(code: -32000, message: "\(method): no caption stream for origin \(origin.rawValue)")
        }
        log("\(method) \(sessionId): \(params.path) as \(origin.rawValue), \(String(format: "%.1f", seconds)) s at speed \(speed)")
        captioner.start()
        guard await captioner.waitUntilSettled() else {
            await captioner.stop()
            throw RPCError(code: -32000, message: "\(method): caption models unavailable; see the engine log")
        }

        // 100 ms buffers, like a capture callback; timestamps are file time.
        let chunk = Int(rate / 10)
        var offset = 0
        while offset < audio.count {
            let end = min(audio.count, offset + chunk)
            inbox.append(mono: Array(audio[offset..<end]), pair: nil, endMs: Double(end) / rate * 1000)
            offset = end
            if speed > 0 { try? await Task.sleep(for: .milliseconds(Int(100 / speed))) }
        }
        await captioner.stop()
        let result = collected.get()
        return SimulateFileResult(sessionId: sessionId, origin: origin, durationMs: Int(seconds * 1000),
                                  finals: result.finals, relabels: result.relabels)
    }

    /// Finals and relabels seen so far; relabels rewrite earlier finals like the app would.
    struct Collected: Sendable {
        var finals: [CaptionFinalEvent] = []
        var relabels = 0

        /// Records one notification.
        /// - Parameters:
        ///   - method: Notification name.
        ///   - payload: Its params.
        mutating func record(_ method: String, _ payload: JSONValue) {
            switch method {
            case Methods.Meeting.nCaptionFinal:
                if let event = try? payload.decode(CaptionFinalEvent.self) { finals.append(event) }
            case Methods.Meeting.nSpeakerRelabel:
                guard let event = try? payload.decode(SpeakerRelabelEvent.self) else { return }
                relabels += 1
                for i in finals.indices where finals[i].speaker == event.from { finals[i].speaker = event.to }
            default:
                break
            }
        }
    }
}
