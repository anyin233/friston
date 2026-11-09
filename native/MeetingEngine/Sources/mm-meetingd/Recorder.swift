import AVFoundation
import AudioToolbox
import CoreMedia
import EngineKit
import Foundation
import MeetingCore
import QuartzCore
import ScreenCaptureKit

/// Sends one JSON-RPC notification to the app.
typealias Notify = @Sendable (String, JSONValue) -> Void

/// Errors raised while setting up or tearing down a recording.
enum RecorderError: Error, CustomStringConvertible {
    case windowNotFound(UInt32)
    case displayNotFound(UInt32?)
    case microphoneNotFound(String)
    case microphoneSelect(String, OSStatus)
    case microphoneTap(String, String)
    case writer(String)

    /// Message sent back as the RPC error text.
    var description: String {
        switch self {
        case .windowNotFound(let id): "window \(id) is no longer shareable; call capture/listSources again"
        case .displayNotFound(let id): "display \(id.map(String.init) ?? "(main)") is not available"
        case .microphoneNotFound(let uid): "audio input \(uid) disappeared before capture started"
        case .microphoneSelect(let uid, let status): "could not select audio input \(uid) (OSStatus \(status))"
        case .microphoneTap(let device, let detail): "could not record from audio input \(device) (\(detail))"
        case .writer(let detail): detail
        }
    }
}

/// One recording session: ScreenCaptureKit for the window/display and system audio,
/// AVAudioEngine for the microphone with every hardware channel kept (spatial cues need them).
///
/// Thread safety: ScreenCaptureKit callbacks and all writer state live on `queue`;
/// the microphone tap runs on the audio I/O thread and only touches `micFile` / `micThrottle`,
/// which are guarded by `micLock`.
final class CaptureSession: NSObject, SCStreamOutput, SCStreamDelegate, @unchecked Sendable {
    let id: String
    let plan: CapturePlan
    let startedAt: Date
    private let notify: Notify
    private let queue = DispatchQueue(label: "mm.capture.session")

    private var stream: SCStream?
    private var videoWriter: AVAssetWriter?
    private var videoInput: AVAssetWriterInput?
    private var videoSessionStarted = false
    private var systemFile: AVAudioFile?
    private var systemThrottle = Throttle(interval: 0.1)
    private var lastWriteError: String?

    private var micEngine: AVAudioEngine?
    private let micLock = NSLock()
    private var micFile: AVAudioFile?
    private var micThrottle = Throttle(interval: 0.1)
    private(set) var microphoneChannels: Int?
    /// Live captions; fed from the capture threads, does its model work elsewhere.
    /// Replaced once in `startMicrophone` (before any audio flows) when the device's live rate
    /// differs from the probed one.
    private var captioner: LiveCaptioner
    private let systemRate: Double?

    /// Creates a session; nothing is captured until `start()`.
    /// - Parameters:
    ///   - plan: Validated sources and output paths.
    ///   - notify: Notification sink (`audio/levels`, `capture/state`).
    init(plan: CapturePlan, notify: @escaping Notify) {
        id = UUID().uuidString.lowercased()
        self.plan = plan
        self.notify = notify
        startedAt = .now
        // ScreenCaptureKit is configured for 48 kHz below; the mic runs at its nominal rate.
        systemRate = plan.preset.systemAudio ? 48_000 : nil
        captioner = LiveCaptioner(sessionId: id, plan: plan, micRate: plan.microphone?.sampleRate,
                                  systemRate: systemRate, notify: notify)
    }

    /// Creates the output folder and starts every requested source.
    /// - Throws: `RecorderError` or the framework error, after undoing whatever already started.
    func start() async throws {
        try FileManager.default.createDirectory(at: plan.directory, withIntermediateDirectories: true)
        do {
            if plan.microphone != nil { try startMicrophone() }
            if plan.files.screen != nil || plan.preset.systemAudio { try await startStream() }
            captioner.start()
        } catch {
            _ = await stop()
            // Leave no empty "2026-… 会议" folders behind after a permission failure.
            if (try? FileManager.default.contentsOfDirectory(atPath: plan.directory.path))?.isEmpty == true {
                try? FileManager.default.removeItem(at: plan.directory)
            }
            throw error
        }
    }

    /// Stops every source and finalizes the files.
    /// - Returns: The files that actually contain data (an unused screen file is removed).
    func stop() async -> CaptureFiles {
        stopMicrophone()
        if let stream {
            do { try await stream.stopCapture() } catch {
                // Already stopped (window closed, delegate error); finalizing below still works.
                log("capture \(id): stopCapture: \(error.localizedDescription)")
            }
        }
        stream = nil
        // After the sources stop, so the last words reach the captioner before it flushes.
        await captioner.stop()
        return await finishWriters()
    }

    // MARK: Microphone

    /// Points the engine's input unit at the chosen device and records its native format.
    private func startMicrophone() throws {
        guard let mic = plan.microphone, let url = plan.files.microphone else { return }
        let engine = AVAudioEngine()
        try selectInput(of: engine, uid: mic.uid)
        let input = engine.inputNode

        // The hardware-side format keeps every channel; AVAudioEngine would otherwise mix down.
        // Its rate is read from the device, since a Bluetooth headset may have changed it.
        let format = InputTap.format(of: engine, uid: mic.uid)
        if format.sampleRate != plan.microphone?.sampleRate {
            // Nothing has been fed yet, so the captioner can still take the live rate; a stale one
            // would make it drop every microphone buffer.
            captioner = LiveCaptioner(sessionId: id, plan: plan, micRate: format.sampleRate, systemRate: systemRate,
                                      notify: notify)
        }
        let file = try AVAudioFile(forWriting: url, settings: format.settings, commonFormat: .pcmFormatFloat32,
                                   interleaved: false)
        micLock.withLock { micFile = file }
        microphoneChannels = Int(format.channelCount)
        log("capture \(id): microphone \(mic.name) \(format.channelCount)ch @ \(format.sampleRate) Hz -> \(url.path)")

        try InputTap.install(on: input, format: format, device: mic.name) { [weak self] buffer in
            self?.handleMicrophone(buffer)
        }
        engine.prepare()
        try engine.start()
        micEngine = engine
    }

    /// Writes one microphone buffer and emits throttled levels (audio I/O thread).
    private func handleMicrophone(_ buffer: AVAudioPCMBuffer) {
        let fire: Bool = micLock.withLock {
            do { try micFile?.write(from: buffer) } catch { log("capture \(id): mic write failed: \(error)") }
            return micThrottle.shouldFire(at: CACurrentMediaTime())
        }
        if fire { emitLevels(buffer, source: .microphone) }
        if let inbox = captioner.microphone { feed(inbox, buffer, pair: captioner.microphoneSpatial) }
    }

    /// Hands a buffer to the captioner (capture thread; copies only).
    /// - Parameters:
    ///   - inbox: The stream's inbox.
    ///   - buffer: Float32 capture buffer, interleaved or not.
    ///   - pair: Channels to keep for direction cues, or `nil`.
    private func feed(_ inbox: CaptionInbox, _ buffer: AVAudioPCMBuffer, pair: SpatialArrayProfile?) {
        guard buffer.format.sampleRate == inbox.sampleRate else {
            // Would make every caption time wrong; the device changed rate under us.
            if !rateWarned.exchange(true) {
                log("capture \(id): captions skip \(buffer.format.sampleRate) Hz audio, expected \(inbox.sampleRate) Hz")
            }
            return
        }
        let channels = Self.channels(of: buffer)
        guard !channels.isEmpty else { return }
        var stereoPair: ([Float], [Float])?
        if let pair, pair.channelA < channels.count, pair.channelB < channels.count {
            stereoPair = (channels[pair.channelA], channels[pair.channelB])
        }
        inbox.append(mono: StreamingResampler.downmix(channels), pair: stereoPair,
                     endMs: Date.now.timeIntervalSince(startedAt) * 1000)
    }

    private let rateWarned = LockedValue(false)

    /// Copies a Float32 buffer into one array per channel.
    /// - Parameter buffer: Interleaved or deinterleaved Float32 PCM.
    /// - Returns: Channel arrays, empty for non-float formats.
    private static func channels(of buffer: AVAudioPCMBuffer) -> [[Float]] {
        guard let data = buffer.floatChannelData else { return [] }
        let frames = Int(buffer.frameLength)
        let count = Int(buffer.format.channelCount)
        if buffer.format.isInterleaved {
            let all = UnsafeBufferPointer(start: data[0], count: frames * count)
            return (0..<count).map { c in (0..<frames).map { all[$0 * count + c] } }
        }
        return (0..<count).map { Array(UnsafeBufferPointer(start: data[$0], count: frames)) }
    }

    /// Removes the tap and closes `mic.caf` (AVAudioFile finalizes the header on release).
    private func stopMicrophone() {
        guard let engine = micEngine else { return }
        engine.inputNode.removeTap(onBus: 0)
        engine.stop()
        micEngine = nil
        micLock.withLock { micFile = nil }
    }

    // MARK: ScreenCaptureKit

    /// Starts an SCStream for the window/display and/or system audio.
    private func startStream() async throws {
        let content: SCShareableContent
        do {
            content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: false)
        } catch {
            throw RPCError(code: -32010, message: "ScreenCaptureKit unavailable (Screen Recording permission?): \(error.localizedDescription)")
        }
        let filter: SCContentFilter
        if let windowID = plan.preset.windowID {
            guard let window = content.windows.first(where: { $0.windowID == windowID }) else {
                throw RecorderError.windowNotFound(windowID)
            }
            filter = SCContentFilter(desktopIndependentWindow: window)
        } else {
            // System audio alone still needs a content filter; any display works and no frames are kept.
            let wanted = plan.preset.displayID
            guard let display = content.displays.first(where: { wanted == nil || $0.displayID == wanted }) else {
                throw RecorderError.displayNotFound(wanted)
            }
            filter = SCContentFilter(display: display, excludingWindows: [])
        }

        let config = SCStreamConfiguration()
        let recordsScreen = plan.files.screen != nil
        if recordsScreen {
            let scale = CGFloat(filter.pointPixelScale)
            // HEVC needs even dimensions.
            config.width = max(2, Int(filter.contentRect.width * scale) & ~1)
            config.height = max(2, Int(filter.contentRect.height * scale) & ~1)
            config.minimumFrameInterval = CMTime(value: 1, timescale: CMTimeScale(plan.fps))
            config.pixelFormat = kCVPixelFormatType_32BGRA
            config.showsCursor = true
            try makeVideoWriter(width: config.width, height: config.height)
        } else {
            config.width = 2
            config.height = 2
            config.minimumFrameInterval = CMTime(value: 1, timescale: 1)
        }
        if plan.preset.systemAudio {
            config.capturesAudio = true
            // Our own UI sounds are not part of the meeting.
            config.excludesCurrentProcessAudio = true
            config.sampleRate = 48_000
            config.channelCount = 2
        }

        let stream = SCStream(filter: filter, configuration: config, delegate: self)
        if recordsScreen { try stream.addStreamOutput(self, type: .screen, sampleHandlerQueue: queue) }
        if plan.preset.systemAudio { try stream.addStreamOutput(self, type: .audio, sampleHandlerQueue: queue) }
        do {
            try await stream.startCapture()
        } catch {
            throw RPCError(code: -32010, message: "could not start screen capture (Screen Recording permission?): \(error.localizedDescription)")
        }
        self.stream = stream
    }

    /// Prepares the HEVC writer for `screen.mov`.
    private func makeVideoWriter(width: Int, height: Int) throws {
        guard let url = plan.files.screen else { return }
        let writer = try AVAssetWriter(outputURL: url, fileType: .mov)
        let input = AVAssetWriterInput(mediaType: .video, outputSettings: [
            AVVideoCodecKey: AVVideoCodecType.hevc, AVVideoWidthKey: width, AVVideoHeightKey: height,
        ])
        input.expectsMediaDataInRealTime = true
        guard writer.canAdd(input) else { throw RecorderError.writer("cannot add a \(width)x\(height) HEVC track to \(url.path)") }
        writer.add(input)
        queue.sync {
            videoWriter = writer
            videoInput = input
        }
        log("capture \(id): screen \(width)x\(height) @ \(plan.fps) fps -> \(url.path)")
    }

    /// Receives screen frames and system audio on `queue`.
    func stream(_ stream: SCStream, didOutputSampleBuffer sampleBuffer: CMSampleBuffer, of type: SCStreamOutputType) {
        guard sampleBuffer.isValid else { return }
        switch type {
        case .screen: handleScreen(sampleBuffer)
        case .audio: handleSystemAudio(sampleBuffer)
        default: break
        }
    }

    /// Appends complete frames; idle/blank frames carry no pixels and are skipped.
    private func handleScreen(_ sampleBuffer: CMSampleBuffer) {
        guard let writer = videoWriter, let input = videoInput else { return }
        guard let attachments = CMSampleBufferGetSampleAttachmentsArray(sampleBuffer, createIfNecessary: false)
                as? [[SCStreamFrameInfo: Any]],
              let rawStatus = attachments.first?[.status] as? Int,
              SCFrameStatus(rawValue: rawStatus) == .complete
        else { return }
        if !videoSessionStarted {
            guard writer.startWriting() else {
                lastWriteError = "screen writer failed to start: \(writer.error?.localizedDescription ?? "unknown")"
                log("capture \(id): \(lastWriteError!)")
                videoWriter = nil
                return
            }
            writer.startSession(atSourceTime: sampleBuffer.presentationTimeStamp)
            videoSessionStarted = true
        }
        if input.isReadyForMoreMediaData, !input.append(sampleBuffer) {
            log("capture \(id): dropped frame: \(writer.error?.localizedDescription ?? "unknown")")
        }
    }

    /// Writes system audio to `system.caf` and emits throttled levels.
    private func handleSystemAudio(_ sampleBuffer: CMSampleBuffer) {
        guard let url = plan.files.systemAudio, let buffer = Self.pcmBuffer(from: sampleBuffer) else { return }
        do {
            if systemFile == nil {
                systemFile = try AVAudioFile(forWriting: url, settings: buffer.format.settings,
                                             commonFormat: .pcmFormatFloat32, interleaved: buffer.format.isInterleaved)
            }
            try systemFile?.write(from: buffer)
        } catch {
            log("capture \(id): system audio write failed: \(error)")
        }
        if systemThrottle.shouldFire(at: CACurrentMediaTime()) { emitLevels(buffer, source: .system) }
        if let inbox = captioner.system { feed(inbox, buffer, pair: nil) }
    }

    /// Wraps a ScreenCaptureKit audio sample buffer as a PCM buffer without copying.
    /// - Parameter sampleBuffer: An `.audio` sample buffer.
    /// - Returns: The PCM view, or `nil` when the buffer has no audio format.
    private static func pcmBuffer(from sampleBuffer: CMSampleBuffer) -> AVAudioPCMBuffer? {
        guard let description = sampleBuffer.formatDescription,
              let asbd = description.audioStreamBasicDescription else { return nil }
        var streamDescription = asbd
        guard let format = AVAudioFormat(streamDescription: &streamDescription) else { return nil }
        var result: AVAudioPCMBuffer?
        try? sampleBuffer.withAudioBufferList { list, _ in
            // Copy, because the block buffer is only valid inside this closure.
            guard let view = AVAudioPCMBuffer(pcmFormat: format, bufferListNoCopy: list.unsafePointer),
                  let copy = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: view.frameLength) else { return }
            copy.frameLength = view.frameLength
            let src = UnsafeMutableAudioBufferListPointer(UnsafeMutablePointer(mutating: view.audioBufferList))
            let dst = UnsafeMutableAudioBufferListPointer(copy.mutableAudioBufferList)
            for (s, d) in zip(src, dst) {
                if let from = s.mData, let to = d.mData { memcpy(to, from, Int(min(s.mDataByteSize, d.mDataByteSize))) }
            }
            result = copy
        }
        return result
    }

    /// Reports a fatal stream error (window closed, permission revoked) as a failed state.
    func stream(_ stream: SCStream, didStopWithError error: Error) {
        let message = "screen capture stopped: \(error.localizedDescription)"
        log("capture \(id): \(message)")
        if let payload = try? JSONValue.from(CaptureStateEvent(sessionId: id, state: .failed, error: message)) {
            notify(Methods.Meeting.nCaptureState, payload)
        }
    }

    /// Finalizes `screen.mov` and `system.caf`; drops files that never received data.
    private func finishWriters() async -> CaptureFiles {
        let (writer, input, started) = queue.sync { () -> (AVAssetWriter?, AVAssetWriterInput?, Bool) in
            defer {
                videoWriter = nil
                videoInput = nil
                systemFile = nil
            }
            return (videoWriter, videoInput, videoSessionStarted)
        }
        var files = plan.files
        if let writer, let input, started {
            input.markAsFinished()
            await writer.finishWriting()
            if writer.status != .completed {
                log("capture \(id): screen.mov not finalized: \(writer.error?.localizedDescription ?? "status \(writer.status.rawValue)")")
            }
        } else if let url = files.screen {
            // No complete frame arrived (e.g. the window was minimized the whole time).
            try? FileManager.default.removeItem(at: url)
            files.screen = nil
        }
        for keyPath in [\CaptureFiles.microphone, \CaptureFiles.systemAudio] {
            if let url = files[keyPath: keyPath], !FileManager.default.fileExists(atPath: url.path) {
                files[keyPath: keyPath] = nil
            }
        }
        return files
    }

    // MARK: Levels

    /// Measures a buffer and sends `audio/levels`.
    private func emitLevels(_ buffer: AVAudioPCMBuffer, source: AudioSource) {
        guard let data = buffer.floatChannelData else { return }
        let frames = Int(buffer.frameLength)
        let channels = Int(buffer.format.channelCount)
        let levels: ChannelLevels
        if buffer.format.isInterleaved {
            levels = LevelMeter.measure(interleaved: Array(UnsafeBufferPointer(start: data[0], count: frames * channels)),
                                        channelCount: channels)
        } else {
            levels = LevelMeter.measure(channels: (0..<channels).map { Array(UnsafeBufferPointer(start: data[$0], count: frames)) })
        }
        if let payload = try? JSONValue.from(AudioLevelsEvent(sessionId: id, source: source, levels: levels)) {
            notify(Methods.Meeting.nAudioLevels, payload)
        }
    }
}

/// Points an engine's input unit at a specific Core Audio device.
/// - Parameters:
///   - engine: A fresh, not yet started engine.
///   - uid: Device UID from `audio/probeInputs`.
/// - Throws: ``RecorderError`` when the device is gone or cannot be selected.
func selectInput(of engine: AVAudioEngine, uid: String) throws {
    guard var deviceID = AudioInputProbe.deviceID(forUID: uid) else { throw RecorderError.microphoneNotFound(uid) }
    guard let unit = engine.inputNode.audioUnit else { throw RecorderError.microphoneSelect(uid, -1) }
    let status = AudioUnitSetProperty(unit, kAudioOutputUnitProperty_CurrentDevice, kAudioUnitScope_Global, 0,
                                      &deviceID, UInt32(MemoryLayout<AudioDeviceID>.size))
    guard status == noErr else { throw RecorderError.microphoneSelect(uid, status) }
}

/// Owns the single active capture session and its lifecycle.
actor CaptureController {
    private var machine = CaptureStateMachine()
    private var session: CaptureSession?
    private let notify: Notify

    /// Creates the controller.
    /// - Parameter notify: Notification sink shared with every session.
    init(notify: @escaping Notify) {
        self.notify = notify
    }

    /// Handles `capture/start`.
    /// - Parameter preset: Requested sources.
    /// - Returns: The session ID and planned file paths.
    /// - Throws: `RPCError` -32602 for invalid presets, -32010/-32012 for missing permissions,
    ///   -32000 when a session is already running or a source fails to start.
    func start(_ preset: CapturePreset) async throws -> CaptureStarted {
        let plan: CapturePlan
        do {
            plan = try CapturePlan.make(preset: preset, inputs: AudioInputProbe.inputs(),
                                        defaultInputUID: AudioInputProbe.defaultInputUID())
        } catch let error as CapturePlanError {
            throw RPCError.invalidParams(error.description)
        }
        do { try machine.transition(to: .starting) } catch let error as CaptureStateError {
            throw RPCError(code: -32000, message: "\(error.description); call capture/stop first")
        }
        if plan.microphone != nil, !(await AVCaptureDevice.requestAccess(for: .audio)) {
            try? machine.transition(to: .failed)
            throw RPCError(code: -32012, message: "microphone access denied; enable it in System Settings › Privacy & Security › Microphone")
        }

        let session = CaptureSession(plan: plan, notify: notify)
        emit(session.id, .starting)
        do {
            try await session.start()
        } catch {
            try? machine.transition(to: .failed)
            let message = (error as? RPCError)?.message ?? "capture/start failed: \(error)"
            emit(session.id, .failed, error: message)
            if let rpc = error as? RPCError { throw rpc }
            throw RPCError(code: -32000, message: message)
        }
        try machine.transition(to: .recording)
        self.session = session
        emit(session.id, .recording)
        return CaptureStarted(sessionId: session.id, directory: plan.directory.path, files: plan.files,
                              microphoneChannels: session.microphoneChannels, startedAt: session.startedAt)
    }

    /// Handles `capture/stop`.
    /// - Returns: Duration and the files that were written.
    /// - Throws: `RPCError` -32000 when nothing is recording.
    func stop() async throws -> CaptureStopped {
        guard let session else { throw RPCError(code: -32000, message: "capture/stop: no capture is running") }
        try machine.transition(to: .stopping)
        emit(session.id, .stopping)
        let files = await session.stop()
        self.session = nil
        try machine.transition(to: .stopped)
        emit(session.id, .stopped)
        let duration = UInt64(max(0, Date.now.timeIntervalSince(session.startedAt)) * 1000)
        return CaptureStopped(sessionId: session.id, durationMs: duration, files: files)
    }

    /// Sends `capture/state`.
    private func emit(_ sessionId: String, _ state: CaptureState, error: String? = nil) {
        if let payload = try? JSONValue.from(CaptureStateEvent(sessionId: sessionId, state: state, error: error)) {
            notify(Methods.Meeting.nCaptureState, payload)
        }
    }
}
