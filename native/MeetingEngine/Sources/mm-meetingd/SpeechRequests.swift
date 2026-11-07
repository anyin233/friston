import AVFoundation
import AudioCommon
import EngineKit
import Foundation
import MeetingCore
import MLX
@preconcurrency import Qwen3ASR
@preconcurrency import SpeechVAD

/// `voiceprint/fromClip` params.
struct VoiceprintFromClipParams: Decodable {
    var path: String
    var startMs: Int?
    var endMs: Int?
}

/// `voiceprint/enroll` params.
struct VoiceprintEnrollParams: Decodable {
    var microphoneUID: String?
    var seconds: Double?
}

/// `transcribe/file` params. `voiceprints` is optional and names matching speakers.
struct TranscribeFileParams: Decodable {
    var path: String
    var language: String?
    var voiceprints: [VoiceprintSample]?
    /// Qwen3-ASR checkpoint for this pass (hub id or absolute folder); default 1.7B 8bit.
    var model: String?
    /// Decoder hint: meeting title, attendee names, terms. Improves names and jargon.
    var context: String?
}

/// Offline speech requests: voiceprints and whole-file transcription.
enum SpeechRequests {
    static let rate = 16_000
    /// Below this, a WeSpeaker embedding is too noisy to identify anyone.
    static let minVoiceprintSpeechMs = 1_000

    /// Loads an audio file as 16 kHz mono.
    /// - Parameters:
    ///   - path: Absolute path (WAV, CAF, M4A… anything AVAudioFile reads).
    ///   - method: RPC method, for error context.
    /// - Returns: Samples.
    /// - Throws: `RPCError` -32602 when the file is missing, -32000 when it cannot be decoded.
    static func loadAudio(path: String, method: String) throws -> [Float] {
        guard FileManager.default.fileExists(atPath: path) else {
            throw RPCError.invalidParams("\(method): no file at \(path)")
        }
        do {
            return try AudioFileLoader.load(url: URL(filePath: path), targetSampleRate: rate)
        } catch {
            throw RPCError(code: -32000, message: "\(method): cannot decode audio \(path): \(error)")
        }
    }

    /// Computes a voiceprint and its quality from 16 kHz audio.
    /// - Parameters:
    ///   - samples: Mono 16 kHz audio.
    ///   - context: Where the audio came from, for error messages.
    /// - Returns: The voiceprint.
    /// - Throws: `RPCError` when models are missing or there is too little speech.
    static func voiceprint(from samples: [Float], context: String) async throws -> VoiceprintResult {
        let vadBox: Unchecked<SileroVADModel>, speakerBox: Unchecked<WeSpeakerModel>
        do {
            vadBox = try await SpeechModels.shared.vad()
            speakerBox = try await SpeechModels.shared.speaker()
        } catch {
            throw RPCError(code: -32000, message: "\(context): \(error)")
        }
        let rate = rate
        return try await MLXGate.shared.run {
            // A separate stream: detectSpeech resets LSTM state that live captions may be using.
            let vad = vadBox.value.makeIndependentStream()
            let regions = vad.detectSpeech(audio: samples, sampleRate: rate).map {
                max(0, Int($0.startTime * Float(rate)))..<min(samples.count, Int($0.endTime * Float(rate)))
            }.filter { !$0.isEmpty }
            let speech = regions.flatMap { samples[$0] }
            let speechMs = speech.count * 1000 / rate
            guard speechMs >= minVoiceprintSpeechMs else {
                throw RPCError(code: -32000, message:
                    "\(context): only \(speechMs) ms of speech found, need at least \(minVoiceprintSpeechMs) ms")
            }
            let speaker = speakerBox.value
            let vector = normalized(speaker.embed(audio: speech, sampleRate: rate))
            // Embed 3 s windows separately: a second voice shows up as an outlier window.
            let window = 3 * rate
            let windows = speech.count >= 2 * window
                ? stride(from: 0, to: speech.count - window / 2, by: window).map {
                    normalized(speaker.embed(audio: Array(speech[$0..<min(speech.count, $0 + window)]), sampleRate: rate))
                }
                : []
            let quality = VoiceprintQuality(
                singleSpeaker: VoiceprintAnalysis.isSingleSpeaker(windowEmbeddings: windows),
                snrDb: VoiceprintAnalysis.snrDb(samples: samples, speech: regions).map { ($0 * 10).rounded() / 10 })
            return VoiceprintResult(model: SpeechModelIDs.speakerEmbedding, vector: vector, speechMs: speechMs,
                                    quality: quality)
        }
    }

    /// Handles `voiceprint/fromClip`.
    /// - Parameter params: Clip path and optional bounds.
    /// - Returns: The voiceprint of the clipped speech.
    /// - Throws: `RPCError` for bad paths/ranges or too little speech.
    static func fromClip(_ params: VoiceprintFromClipParams) async throws -> VoiceprintResult {
        let method = Methods.Meeting.voiceprintFromClip
        let all = try loadAudio(path: params.path, method: method)
        let range: Range<Int>
        do {
            range = try VoiceprintAnalysis.sampleRange(totalSamples: all.count, sampleRate: rate,
                                                       startMs: params.startMs, endMs: params.endMs)
        } catch let error as VoiceprintAnalysisError {
            throw RPCError.invalidParams("\(method) \(params.path): \(error)")
        }
        return try await voiceprint(from: Array(all[range]), context: "\(method) \(params.path)")
    }

    /// Handles `voiceprint/enroll`: records the microphone, then computes the voiceprint.
    /// - Parameter params: Device (default input when omitted) and duration (3–120 s, default 15).
    /// - Returns: The voiceprint.
    /// - Throws: `RPCError` -32012 without microphone permission, -32602 for a bad duration.
    static func enroll(_ params: VoiceprintEnrollParams) async throws -> VoiceprintResult {
        let method = Methods.Meeting.voiceprintEnroll
        let seconds = params.seconds ?? 15
        guard (3...120).contains(seconds) else { throw RPCError.invalidParams("\(method): seconds must be 3…120, got \(seconds)") }
        guard await AVCaptureDevice.requestAccess(for: .audio) else {
            throw RPCError(code: -32012, message: "microphone access denied; enable it in System Settings › Privacy & Security › Microphone")
        }
        let engine = AVAudioEngine()
        do {
            if let uid = params.microphoneUID { try selectInput(of: engine, uid: uid) }
        } catch {
            throw RPCError(code: -32000, message: "\(method): \(error)")
        }
        let format = engine.inputNode.inputFormat(forBus: 0)
        let collected = LockedValue<[Float]>([])
        engine.inputNode.installTap(onBus: 0, bufferSize: 4096, format: format) { buffer, _ in
            guard let data = buffer.floatChannelData else { return }
            let frames = Int(buffer.frameLength)
            let channels = (0..<Int(buffer.format.channelCount)).map { Array(UnsafeBufferPointer(start: data[$0], count: frames)) }
            let mono = buffer.format.isInterleaved
                ? StreamingResampler.downmix(interleaved: Array(UnsafeBufferPointer(start: data[0], count: frames * channels.count)),
                                             channelCount: channels.count)
                : StreamingResampler.downmix(channels)
            collected.mutate { $0 += mono }
        }
        engine.prepare()
        do { try engine.start() } catch {
            engine.inputNode.removeTap(onBus: 0)
            throw RPCError(code: -32000, message: "\(method): cannot start \(params.microphoneUID ?? "default input"): \(error)")
        }
        log("\(method): recording \(seconds) s from \(params.microphoneUID ?? "default input") @ \(format.sampleRate) Hz")
        try? await Task.sleep(for: .seconds(seconds))
        engine.inputNode.removeTap(onBus: 0)
        engine.stop()
        var resampler = StreamingResampler(inputRate: format.sampleRate, outputRate: Double(rate))
        let samples = resampler.process(collected.get())
        return try await voiceprint(from: samples, context: "\(method) (\(params.microphoneUID ?? "default input"))")
    }

    /// Handles `transcribe/file`: diarization, then Qwen3-ASR 1.7B per speaker turn.
    /// - Parameter params: File, language hint and optional voiceprints for naming speakers.
    /// - Returns: Segments in time order.
    /// - Throws: `RPCError` for missing files or when the ASR model cannot load.
    static func transcribeFile(_ params: TranscribeFileParams) async throws -> FileTranscript {
        let method = Methods.Meeting.transcribeFile
        let samples = try loadAudio(path: params.path, method: method)
        // Validate the model before the minutes-long diarization, so a typo fails fast.
        let source: ASRModelSource?
        do {
            source = try params.model.map {
                try ASRModelChoice.resolve($0, hubSnapshot: { HubCacheLocator.snapshot(repo: $0) },
                                           isDirectory: { path in
                                               var dir: ObjCBool = false
                                               return FileManager.default.fileExists(atPath: path, isDirectory: &dir) && dir.boolValue
                                           })
            }
        } catch {
            throw RPCError.invalidParams("\(method) \(params.path): \(error)")
        }
        let started = Date()
        let rate = rate
        guard !samples.isEmpty else { return FileTranscript(language: ASRLanguage.primaryCode(for: params.language), segments: []) }

        // Diarize first so the 1.7B model is not resident at the same time as the pipeline load.
        var turns: [SpeakerTurn] = []
        var centroids: [[Float]] = []
        do {
            let diarizer = try await SpeechModels.shared.diarizer()
            let result = try await MLXGate.shared.run {
                let r = diarizer.value.diarize(audio: samples, sampleRate: rate, config: .default)
                return (r.segments.map { SpeakerTurn(start: $0.startTime, end: $0.endTime, speaker: $0.speakerId) },
                        r.speakerEmbeddings)
            }
            (turns, centroids) = result
        } catch {
            log("\(method) \(params.path): diarization unavailable, speakers will be unknown: \(error)")
        }
        if turns.isEmpty {
            // No diarizer (or it found nothing): plain VAD segments, speaker unknown.
            let vadBox = try await loadOrFail(method) { try await SpeechModels.shared.vad() }
            turns = try await MLXGate.shared.run {
                vadBox.value.makeIndependentStream().detectSpeech(audio: samples, sampleRate: rate)
                    .map { SpeakerTurn(start: $0.startTime, end: $0.endTime, speaker: -1) }
            }
        }
        let windows = TurnPlanner.windows(from: turns, maxGap: 0.8, maxDuration: 25)
        let labels = speakerLabels(centroids: centroids, voiceprints: params.voiceprints ?? [])

        let (segments, model) = try await transcribeWindows(samples, windows: windows, labels: labels, params: params,
                                                            source: source)
        // The 1.7B model is gone once transcribeWindows returns, but MLX keeps its buffers in a
        // cache (~1.3 GB measured); clearing it is what actually returns the memory.
        let freed = try? await MLXGate.shared.run { () -> String in
            let before = MLX.Memory.cacheMemory
            MLX.Memory.clearCache()
            return "MLX active \(MLX.Memory.activeMemory >> 20) MB, cache \(before >> 20) → \(MLX.Memory.cacheMemory >> 20) MB"
        }
        log("\(method): \(freed ?? "MLX cache not cleared")")
        let language = ASRLanguage.primaryCode(for: params.language) ?? majorityLanguage(segments)
        log("\(method) \(params.path): \(segments.count) segments, \(Set(windows.map(\.speaker)).count) speakers, "
            + "\(model), context \(ASRModelChoice.context(params.context)?.count ?? 0) chars, "
            + "\(String(format: "%.1f", Date().timeIntervalSince(started))) s for "
            + "\(String(format: "%.1f", Double(samples.count) / Double(rate))) s of audio")
        return FileTranscript(language: language, segments: segments, model: model)
    }

    /// Loads the offline model, transcribes each window and drops the model on return.
    /// - Parameters:
    ///   - samples: 16 kHz mono audio.
    ///   - windows: Speaker-homogeneous windows to decode.
    ///   - labels: Diarized speaker id → label.
    ///   - params: Request params (path, language, context).
    ///   - source: Requested checkpoint, or `nil` for the default refine model.
    /// - Returns: Segments and the model id used.
    /// - Throws: Model load errors with the method name.
    private static func transcribeWindows(
        _ samples: [Float], windows: [SpeakerTurn], labels: [Int: SpeakerLabel], params: TranscribeFileParams,
        source: ASRModelSource?
    ) async throws -> ([FileTranscriptSegment], String) {
        let method = Methods.Meeting.transcribeFile
        let rate = rate
        let (asr, model) = try await loadOrFail(method) { try await SpeechModels.shared.offlineASR(source) }
        let context = ASRModelChoice.context(params.context)
        var segments: [FileTranscriptSegment] = []
        for w in windows {
            let lo = max(0, Int(w.start * Float(rate))), hi = min(samples.count, Int(w.end * Float(rate)))
            guard hi - lo >= rate / 5 else { continue }
            let slice = Array(samples[lo..<hi])
            let options = Qwen3DecodingOptions(
                maxTokens: ASRLanguage.maxTokens(forSeconds: Double(slice.count) / Double(rate)),
                language: ASRLanguage.qwenName(for: params.language), context: context)
            // One gate turn per window, so live captions can interleave with a long file.
            let text = try await MLXGate.shared.run {
                asr.value.transcribe(audio: slice, sampleRate: rate, options: options)
                    .trimmingCharacters(in: .whitespacesAndNewlines)
            }
            guard !text.isEmpty else { continue }
            segments.append(FileTranscriptSegment(
                startMs: lo * 1000 / rate, endMs: hi * 1000 / rate, text: text,
                speaker: w.speaker >= 0 ? labels[w.speaker] ?? .cluster(index: w.speaker) : .unknown))
        }
        return (segments, model)
    }

    /// Names diarized speakers whose centroid matches an enrolled voiceprint.
    /// - Parameters:
    ///   - centroids: Per-speaker embeddings from the diarizer (index = speaker id).
    ///   - voiceprints: Enrolled samples; other models' vectors are ignored.
    /// - Returns: Labels for matched speaker ids; unmatched ids fall back to clusters.
    static func speakerLabels(centroids: [[Float]], voiceprints: [VoiceprintSample]) -> [Int: SpeakerLabel] {
        let bank = VoiceprintBank(samples: voiceprints, model: SpeechModelIDs.speakerEmbedding)
        let threshold = SpeakerFusionConfig().knownThreshold
        var labels: [Int: SpeakerLabel] = [:]
        for (id, centroid) in centroids.enumerated() {
            if let match = bank.bestMatch(normalized(centroid)), match.score >= threshold {
                labels[id] = .known(personId: match.personId, score: match.score)
            }
        }
        return labels
    }

    /// The most common guessed language across segments, weighted by speaking time
    /// (character counts would favour alphabetic scripts over Chinese).
    private static func majorityLanguage(_ segments: [FileTranscriptSegment]) -> String? {
        var weights: [String: Int] = [:]
        for s in segments {
            if let code = ASRLanguage.guess(fromText: s.text) { weights[code, default: 0] += s.endMs - s.startMs }
        }
        return weights.max { $0.value < $1.value }?.key
    }

    /// Loads a model, converting failures into an RPC error naming the method.
    private static func loadOrFail<T>(_ method: String, _ load: () async throws -> T) async throws -> T {
        do { return try await load() } catch {
            throw RPCError(code: -32000, message: "\(method): \(error)")
        }
    }
}
