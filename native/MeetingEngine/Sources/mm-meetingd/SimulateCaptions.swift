import AVFoundation
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
    /// Same as `capture/start`: mirror direction cues of a 2-channel microphone file.
    var spatialFlip: Bool?
    /// Same as `capture/start`: mic spacing of a 2-channel file, for `azimuthDeg`.
    var spatialSpacingCm: Double?
    /// Optional cloud live ASR backend configuration (`openai` / `gemini`).
    var liveAsr: LiveAsrConfig?
}

/// `caption/simulateFile` result: what the app would have shown, after all relabels.
struct SimulateFileResult: Encodable {
    var sessionId: String
    var origin: AudioOrigin
    var durationMs: Int
    /// Every `caption/final` sent, in order, with the speaker as finally relabelled.
    var finals: [CaptionFinalEvent]
    var relabels: Int
    /// Every `spatial/cue` sent (2-channel microphone files only), speakers relabelled like `finals`.
    var spatialCues: [SpatialCueEvent]
    /// Every `caption/usage` event emitted while streaming in cloud live mode.
    var usage: [CaptionUsageEvent]
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
        let origin = params.origin ?? .microphone
        let sessionId = "sim-\(UUID().uuidString.prefix(8).lowercased())"
        // A 2-channel microphone file keeps its channels at its own rate, exactly as a stereo
        // capture device would deliver them, so direction cues and `spatial/cue` run too.
        let file = try loadChannels(path: params.path, method: method, keepChannels: origin == .microphone)
        let rate = file.sampleRate
        let frames = file.channels.first?.count ?? 0
        let seconds = Double(frames) / rate
        let device = file.channels.count >= 2
            ? AudioInputInfo(uid: "simulate", name: params.path, channels: file.channels.count, sampleRate: rate,
                             isVirtual: false)
            : nil

        let collected = LockedValue(Collected())
        let sink: Notify = { method, payload in
            collected.mutate { $0.record(method, payload) }
            notify(method, payload)
        }
        let preset = CapturePreset(captionLanguage: params.language, voiceprints: params.voiceprints,
                                   spatialFlip: params.spatialFlip ?? false, spatialSpacingCm: params.spatialSpacingCm,
                                   liveAsr: params.liveAsr)
        // The whole file may arrive at once (speed 0), so the inbox must hold all of it.
        let captioner = LiveCaptioner(
            sessionId: sessionId, preset: preset, microphone: device,
            micRate: origin == .microphone ? rate : nil, systemRate: origin == .system ? rate : nil,
            maxBacklogSeconds: seconds + 5, notify: sink)
        guard let inbox = origin == .microphone ? captioner.microphone : captioner.system else {
            throw RPCError(code: -32000, message: "\(method): no caption stream for origin \(origin.rawValue)")
        }
        let pair = captioner.microphoneSpatial
        log("\(method) \(sessionId): \(params.path) as \(origin.rawValue), \(file.channels.count) ch @ \(Int(rate)) Hz, "
            + "\(String(format: "%.1f", seconds)) s at speed \(speed), direction \(pair == nil ? "off" : "on")")
        captioner.start()
        guard await captioner.waitUntilSettled() else {
            await captioner.stop()
            throw RPCError(code: -32000, message: "\(method): caption models unavailable; see the engine log")
        }

        // 100 ms buffers, like a capture callback; timestamps are file time.
        let chunk = Int(rate / 10)
        var offset = 0
        // Pace against a clock, not a fixed sleep per buffer: the per-buffer overhead otherwise
        // accumulates (measured ~4 %, 17 s behind after 7 min), so latency measurements drift.
        let clock = ContinuousClock()
        let started = clock.now
        while offset < frames {
            let end = min(frames, offset + chunk)
            let slices = file.channels.map { Array($0[offset..<end]) }
            let stereo = pair.map { (slices[$0.channelA], slices[$0.channelB]) }
            inbox.append(mono: StreamingResampler.downmix(slices), pair: stereo, endMs: Double(end) / rate * 1000)
            offset = end
            if speed > 0 { try? await Task.sleep(until: started + .seconds(Double(end) / rate / speed), clock: clock) }
        }
        await captioner.stop()
        let result = collected.get()
        return SimulateFileResult(sessionId: sessionId, origin: origin, durationMs: Int(seconds * 1000),
                                  finals: result.finals, relabels: result.relabels, spatialCues: result.spatialCues,
                                  usage: result.usage)
    }

    /// Decoded audio: one array per channel at `sampleRate`.
    struct ChannelAudio {
        var channels: [[Float]]
        var sampleRate: Double
    }

    /// Loads a file for simulation.
    /// - Parameters:
    ///   - path: Audio file.
    ///   - method: RPC method, for error messages.
    ///   - keepChannels: Keep a stereo file's two channels at the file rate (microphone origin);
    ///     otherwise decode to mono 16 kHz like `transcribe/file`.
    /// - Returns: The channels and their rate.
    /// - Throws: `RPCError` -32602 for a missing file, -32000 when it cannot be decoded.
    static func loadChannels(path: String, method: String, keepChannels: Bool) throws -> ChannelAudio {
        guard keepChannels, FileManager.default.fileExists(atPath: path) else {
            return ChannelAudio(channels: [try SpeechRequests.loadAudio(path: path, method: method)],
                                sampleRate: Double(SpeechRequests.rate))
        }
        do {
            let file = try AVAudioFile(forReading: URL(filePath: path), commonFormat: .pcmFormatFloat32,
                                       interleaved: false)
            let format = file.processingFormat
            // Only a plain stereo file maps onto a capture device with a spatial pair; anything
            // else would be downmixed anyway, so use the shared mono loader.
            guard format.channelCount == 2 else {
                return ChannelAudio(channels: [try SpeechRequests.loadAudio(path: path, method: method)],
                                    sampleRate: Double(SpeechRequests.rate))
            }
            let count = AVAudioFrameCount(file.length)
            guard let buffer = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: count) else {
                throw RPCError(code: -32000, message: "\(method): cannot allocate \(count) frames for \(path)")
            }
            try file.read(into: buffer)
            guard let data = buffer.floatChannelData else {
                throw RPCError(code: -32000, message: "\(method): \(path) did not decode to Float32")
            }
            let frames = Int(buffer.frameLength)
            let channels = (0..<Int(format.channelCount)).map { Array(UnsafeBufferPointer(start: data[$0], count: frames)) }
            return ChannelAudio(channels: channels, sampleRate: format.sampleRate)
        } catch let error as RPCError {
            throw error
        } catch {
            throw RPCError(code: -32000, message: "\(method): cannot decode audio \(path): \(error)")
        }
    }

    /// Finals and relabels seen so far; relabels rewrite earlier finals like the app would.
    struct Collected: Sendable {
        var finals: [CaptionFinalEvent] = []
        var relabels = 0
        var spatialCues: [SpatialCueEvent] = []
        var usage: [CaptionUsageEvent] = []

        /// Records one notification.
        /// - Parameters:
        ///   - method: Notification name.
        ///   - payload: Its params.
        mutating func record(_ method: String, _ payload: JSONValue) {
            switch method {
            case Methods.Meeting.nCaptionFinal:
                if let event = try? payload.decode(CaptionFinalEvent.self) { finals.append(event) }
            case Methods.Meeting.nSpatialCue:
                if let event = try? payload.decode(SpatialCueEvent.self) { spatialCues.append(event) }
            case Methods.Meeting.nCaptionUsage:
                if let event = try? payload.decode(CaptionUsageEvent.self) { usage.append(event) }
            case Methods.Meeting.nSpeakerRelabel:
                guard let event = try? payload.decode(SpeakerRelabelEvent.self) else { return }
                relabels += 1
                for i in finals.indices where finals[i].speaker == event.from { finals[i].speaker = event.to }
                for i in spatialCues.indices where spatialCues[i].speaker == event.from { spatialCues[i].speaker = event.to }
            default:
                break
            }
        }
    }
}
