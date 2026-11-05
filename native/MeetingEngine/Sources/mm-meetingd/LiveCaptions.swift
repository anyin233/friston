import AVFoundation
import EngineKit
import Foundation
import MeetingCore
@preconcurrency import Qwen3ASR
@preconcurrency import SpeechVAD

/// Audio handed from a capture thread to the caption loop, at the device rate.
///
/// The capture threads only append under a short lock; everything expensive happens in the
/// caption loop, so a slow model can never make the recorder drop audio.
final class CaptionInbox: @unchecked Sendable {
    struct Batch {
        var mono: [Float] = []
        /// The spatial pair (channels a and b), when the stream has one.
        var pairA: [Float] = []
        var pairB: [Float] = []
        /// Capture-relative time of `mono[0]`, in ms.
        var firstMs: Double?
    }

    private let lock = NSLock()
    private var batch = Batch()
    private var closed = false
    let sampleRate: Double
    /// Bounded so a stream whose models never load cannot grow without limit.
    private let maxSamples: Int

    /// Creates an inbox.
    /// - Parameters:
    ///   - sampleRate: Device rate of the appended samples.
    ///   - maxSeconds: Backlog kept while models load; older audio is dropped.
    init(sampleRate: Double, maxSeconds: Double = 60) {
        self.sampleRate = sampleRate
        maxSamples = Int(sampleRate * maxSeconds)
    }

    /// Appends one capture buffer (capture thread).
    /// - Parameters:
    ///   - mono: Mono mix.
    ///   - pair: Spatial pair, same length as `mono`, or `nil`.
    ///   - endMs: Capture-relative time just after the last sample.
    func append(mono: [Float], pair: ([Float], [Float])?, endMs: Double) {
        lock.withLock {
            guard !closed else { return }
            if batch.firstMs == nil {
                batch.firstMs = endMs - Double(mono.count) / sampleRate * 1000
            }
            batch.mono += mono
            if let pair {
                batch.pairA += pair.0
                batch.pairB += pair.1
            }
            let excess = batch.mono.count - maxSamples
            if excess > 0 {
                batch.mono.removeFirst(excess)
                if batch.pairA.count >= excess {
                    batch.pairA.removeFirst(excess)
                    batch.pairB.removeFirst(excess)
                }
                batch.firstMs = batch.firstMs.map { $0 + Double(excess) / sampleRate * 1000 }
            }
        }
    }

    /// Takes everything appended so far.
    /// - Returns: The pending batch (empty when nothing arrived).
    func take() -> Batch {
        lock.withLock {
            defer { batch = Batch() }
            return batch
        }
    }

    /// Stops accepting audio (captions disabled or session over).
    func close() {
        lock.withLock {
            closed = true
            batch = Batch()
        }
    }
}

/// Models the caption loop uses, loaded together so a stream never runs half-equipped.
struct LiveModels: @unchecked Sendable {
    let asr: Qwen3ASRModel
    let vad: SileroVADModel
    /// `nil` when the embedding model failed to load: captions continue with `unknown` speakers.
    let speaker: WeSpeakerModel?
}

/// Per-stream caption state. Only touched by ``LiveCaptioner``'s loop, inside ``MLXGate``.
final class CaptionStream: @unchecked Sendable {
    let origin: AudioOrigin
    let inbox: CaptionInbox
    private var resampler: StreamingResampler
    private var vad: StreamingVADProcessor?
    private var segmenter: UtteranceSegmenter
    private var clock: CaptionClock?
    /// 16 kHz audio; `audio[0]` is stream sample `audioBase`.
    private var audio: [Float] = []
    private var audioBase = 0
    /// 16 kHz samples fed to the VAD so far.
    private var processed = 0

    private let spatial: SpatialArrayProfile?
    private var stereo: StereoCueAnalyzer?
    private var pairA: [Float] = []
    private var pairB: [Float] = []
    /// Device-rate frames consumed by the stereo analyzer so far.
    private var stereoFrames = 0
    private var frameCues: [(sample: Int, cue: StereoFrameCue)] = []

    static let rate = 16_000

    /// Creates a stream.
    /// - Parameters:
    ///   - origin: Microphone or system audio.
    ///   - inbox: Where the capture thread drops audio.
    ///   - ids: Session-wide utterance IDs.
    ///   - spatial: Mic pair for direction cues, or `nil` for mono/virtual inputs.
    init(origin: AudioOrigin, inbox: CaptionInbox, ids: UtteranceIDAllocator, spatial: SpatialArrayProfile?) {
        self.origin = origin
        self.inbox = inbox
        resampler = StreamingResampler(inputRate: inbox.sampleRate, outputRate: Double(Self.rate))
        segmenter = UtteranceSegmenter(config: .init(sampleRate: Self.rate), ids: ids)
        self.spatial = spatial
        if let spatial {
            do { stereo = try StereoCueAnalyzer(config: spatial.analyzerConfig) } catch {
                log("captions \(origin.rawValue): stereo cues disabled: \(error)")
            }
        }
    }

    /// Feeds pending audio through resampling, VAD and segmentation.
    /// - Parameters:
    ///   - batch: Audio taken from the inbox.
    ///   - models: Loaded models (for the VAD).
    /// - Returns: Work items, at most one partial per utterance.
    func ingest(_ batch: CaptionInbox.Batch, models: LiveModels) -> [UtteranceSegmenter.Action] {
        guard !batch.mono.isEmpty else { return [] }
        if clock == nil, let first = batch.firstMs {
            clock = CaptionClock(sampleRate: Self.rate, offsetMs: Int(first.rounded()))
        }
        if vad == nil {
            // Silero VAD is a stateful LSTM: each stream needs its own state over shared weights.
            vad = StreamingVADProcessor(model: models.vad.makeIndependentStream(), config: Self.vadConfig)
        }
        ingestStereo(batch)
        let samples = resampler.process(batch.mono)
        audio += samples
        var actions: [UtteranceSegmenter.Action] = []
        var offset = 0
        // Feed in VAD-sized steps so long monologues are split on time, not per batch.
        while offset < samples.count, let vad {
            let end = min(samples.count, offset + SileroVADModel.chunkSize)
            for event in vad.process(samples: Array(samples[offset..<end])) {
                switch event {
                case .speechStarted(let time):
                    actions += segmenter.speechStarted(atSample: Int(time * Float(Self.rate)))
                case .speechEnded(let segment):
                    actions += segmenter.speechEnded(endSample: Int(segment.endTime * Float(Self.rate)))
                }
            }
            processed += end - offset
            actions += segmenter.advance(to: processed)
            offset = end
        }
        return Self.coalesce(actions)
    }

    /// Ends the stream: closes any open utterance.
    /// - Returns: The last final, if any.
    func flush() -> [UtteranceSegmenter.Action] {
        guard let vad else { return [] }
        var actions: [UtteranceSegmenter.Action] = []
        for case .speechEnded(let segment) in vad.flush() {
            actions += segmenter.speechEnded(endSample: Int(segment.endTime * Float(Self.rate)))
        }
        // The VAD may have closed already while our segmenter still holds an utterance.
        actions += segmenter.speechEnded(endSample: processed)
        return Self.coalesce(actions)
    }

    /// Keeps only the newest partial per utterance, and none for utterances already final:
    /// transcribing stale partials while behind real time would only delay the finals.
    static func coalesce(_ actions: [UtteranceSegmenter.Action]) -> [UtteranceSegmenter.Action] {
        var finals = Set<Int>()
        var newestPartial: [Int: Int] = [:]
        for (i, a) in actions.enumerated() {
            switch a {
            case .final(let id, _, _, _): finals.insert(id)
            case .partial(let id, _, _): newestPartial[id] = i
            }
        }
        return actions.enumerated().compactMap { i, a in
            if case .partial(let id, _, _) = a, finals.contains(id) || newestPartial[id] != i { return nil }
            return a
        }
    }

    /// VAD thresholds: Silero defaults, but a longer pause before ending an utterance so
    /// captions break at phrase boundaries instead of every breath.
    static let vadConfig = VADConfig(onset: 0.5, offset: 0.35, minSpeechDuration: 0.25, minSilenceDuration: 0.5,
                                     windowDuration: 0.032, stepRatio: 1.0)

    /// Audio for `start..<end` (16 kHz stream samples), with a little pre-roll for the first phoneme.
    func slice(_ start: Int, _ end: Int) -> [Float] {
        let lo = max(audioBase, start - Self.rate / 10) - audioBase
        let hi = min(audioBase + audio.count, end) - audioBase
        return hi > lo ? Array(audio[lo..<hi]) : []
    }

    /// Drops audio no utterance can need any more.
    func trim() {
        // Keep 1 s before the VAD position: speech onset is confirmed ~0.25 s after it starts.
        let keepFrom = min(segmenter.activeStart ?? processed, processed - Self.rate) - Self.rate / 10
        let drop = min(keepFrom - audioBase, audio.count)
        if drop > 0 {
            audio.removeFirst(drop)
            audioBase += drop
        }
        let cueFloor = (segmenter.activeStart ?? processed) - Self.rate
        frameCues.removeAll { $0.sample < cueFloor }
    }

    /// Capture-relative milliseconds of a 16 kHz stream sample.
    func ms(_ sample: Int) -> Int { clock?.ms(atSample: sample) ?? 0 }

    // MARK: Direction

    /// Runs the stereo analyzer on whole frames of the spatial pair.
    private func ingestStereo(_ batch: CaptionInbox.Batch) {
        guard let stereo, !batch.pairA.isEmpty else { return }
        pairA += batch.pairA
        pairB += batch.pairB
        let n = stereo.config.frameCount
        while pairA.count >= n {
            let a = Array(pairA.prefix(n)), b = Array(pairB.prefix(n))
            pairA.removeFirst(n)
            pairB.removeFirst(n)
            // Frame centre in 16 kHz samples, to match utterance bounds.
            let centre = Int(Double(stereoFrames + n / 2) * Double(Self.rate) / inbox.sampleRate)
            stereoFrames += n
            if let cue = stereo.analyze(a, b) { frameCues.append((centre, cue)) }
        }
    }

    /// Summarises direction over an utterance.
    /// - Returns: The fusion cue and the wire cue, or `nil`s for mono input / no usable frames.
    func spatialCue(_ start: Int, _ end: Int) -> (SpatialCue?, CaptionSpatialCue?) {
        guard let stereo else { return (nil, nil) }
        let frames = frameCues.filter { $0.sample >= start && $0.sample < end }.map(\.cue)
        guard let cue = stereo.segmentCue(frames) else { return (nil, nil) }
        let delays = frames.filter { $0.confidence >= stereo.config.minConfidence }.map(\.delaySamples).sorted()
        let median = delays.isEmpty ? 0 : delays[delays.count / 2]
        return (cue, CaptionSpatialCue(azimuthDeg: nil, tdoaSamples: median, confidence: cue.confidence))
    }
}

/// Live captions for one capture session: two independent streams (origin is a hard speaker
/// partition), one speaker tracker, one loop that does all model work through ``MLXGate``.
final class LiveCaptioner: @unchecked Sendable {
    let sessionId: String
    let microphone: CaptionInbox?
    let system: CaptionInbox?
    private let streams: [CaptionStream]
    private let tracker: OnlineSpeakerTracker
    private let languageName: String?
    private let languageCode: String?
    private let notify: Notify
    private let stopping = LockedValue(false)
    private let ready = LockedValue(false)
    private var loop: Task<Void, Never>?

    /// Creates the captioner; nothing runs until ``start()``.
    /// - Parameters:
    ///   - sessionId: Capture session id for the notifications.
    ///   - plan: The capture plan (sources, language, voiceprints, mic device).
    ///   - micRate: Microphone device rate, when recording the mic.
    ///   - systemRate: System-audio rate, when recording it.
    ///   - notify: Notification sink.
    init(sessionId: String, plan: CapturePlan, micRate: Double?, systemRate: Double?, notify: @escaping Notify) {
        self.sessionId = sessionId
        self.notify = notify
        let ids = UtteranceIDAllocator()
        var streams: [CaptionStream] = []
        var micSpatial: SpatialArrayProfile?
        if let micRate {
            let inbox = CaptionInbox(sampleRate: micRate)
            // Virtual/aggregate devices and multichannel devices without a preset give no pair.
            let spatial = plan.microphone.flatMap { try? SpatialArrayProfile.resolve(for: $0) } ?? nil
            micSpatial = spatial
            streams.append(CaptionStream(origin: .microphone, inbox: inbox, ids: ids, spatial: spatial))
            microphone = inbox
        } else {
            microphone = nil
        }
        if let systemRate {
            let inbox = CaptionInbox(sampleRate: systemRate)
            streams.append(CaptionStream(origin: .system, inbox: inbox, ids: ids, spatial: nil))
            system = inbox
        } else {
            system = nil
        }
        self.streams = streams
        microphoneSpatial = micSpatial
        let samples = plan.preset.voiceprints ?? []
        let bank = VoiceprintBank(samples: samples, model: SpeechModelIDs.speakerEmbedding)
        if !samples.isEmpty {
            log("captions \(sessionId): \(bank.personCount) enrolled people (\(samples.count) samples, model \(SpeechModelIDs.speakerEmbedding))")
        }
        tracker = OnlineSpeakerTracker(bank: bank)
        languageName = ASRLanguage.qwenName(for: plan.preset.captionLanguage)
        languageCode = ASRLanguage.primaryCode(for: plan.preset.captionLanguage)
    }

    /// The mic channels the direction analysis uses; the recorder copies them out of each buffer.
    let microphoneSpatial: SpatialArrayProfile?

    /// Starts loading models and the caption loop in the background.
    func start() {
        guard !streams.isEmpty else { return }
        loop = Task.detached(priority: .userInitiated) { [self] in await run() }
    }

    /// Stops captioning, finalizing open utterances when the models are loaded.
    ///
    /// Does not wait for a model that is still downloading: `capture/stop` must stay prompt.
    func stop() async {
        _ = stopping.exchange(true)
        if ready.get() {
            await loop?.value
        } else if loop != nil {
            log("captions \(sessionId): stopped before the models were ready")
        }
        microphone?.close()
        system?.close()
    }

    /// Loads models, then processes audio every 100 ms until stopped.
    private func run() async {
        let models: LiveModels
        do {
            async let asr = SpeechModels.shared.liveASR()
            async let vad = SpeechModels.shared.vad()
            async let speaker = try? SpeechModels.shared.speaker()
            let loaded = (try await asr, try await vad, await speaker)
            if loaded.2 == nil { log("captions \(sessionId): speaker embeddings unavailable; speakers will be unknown") }
            models = LiveModels(asr: loaded.0.value, vad: loaded.1.value, speaker: loaded.2?.value)
        } catch {
            log("captions \(sessionId): disabled, models unavailable: \(error)")
            microphone?.close()
            system?.close()
            return
        }
        _ = ready.exchange(true)
        log("captions \(sessionId): running (language \(languageName ?? "auto"))")
        while true {
            let last = stopping.get()
            for stream in streams {
                let batch = stream.inbox.take()
                let behind = Double(batch.mono.count) / stream.inbox.sampleRate > 1.5
                do {
                    try await MLXGate.shared.run { [self] in
                        var actions = stream.ingest(batch, models: models)
                        if last { actions += stream.flush() }
                        for action in actions { self.perform(action, on: stream, models: models, skipPartials: behind) }
                        stream.trim()
                    }
                } catch {
                    log("captions \(sessionId) \(stream.origin.rawValue): \(error)")
                }
            }
            if last { break }
            try? await Task.sleep(for: .milliseconds(100))
        }
        log("captions \(sessionId): stopped")
    }

    /// Transcribes one work item and sends its notifications (runs inside ``MLXGate``).
    private func perform(_ action: UtteranceSegmenter.Action, on stream: CaptionStream, models: LiveModels,
                         skipPartials: Bool) {
        switch action {
        case .partial(let id, let start, let end):
            guard !skipPartials else { return }
            let audio = stream.slice(start, end)
            let text = transcribe(audio, models)
            guard !text.isEmpty else { return }
            var speaker = SpeakerLabel.unknown
            if audio.count >= CaptionStream.rate * 3 / 2, let embedding = embed(audio, models) {
                speaker = tracker.peek(SpeechObservation(embedding: embedding, origin: stream.origin))
            }
            send(Methods.Meeting.nCaptionPartial, CaptionPartialEvent(
                sessionId: sessionId, utteranceId: id, origin: stream.origin, startMs: stream.ms(start),
                text: text, speaker: speaker))

        case .final(let id, let start, let end, let hadPartial):
            let audio = stream.slice(start, end)
            let text = transcribe(audio, models)
            guard !text.isEmpty || hadPartial else { return }
            let (cue, wireCue) = stream.spatialCue(start, end)
            var speaker = SpeakerLabel.unknown
            if !text.isEmpty, let embedding = audio.count >= CaptionStream.rate * 2 / 5 ? embed(audio, models) : nil {
                let observation = SpeechObservation(embedding: embedding, origin: stream.origin, spatial: cue)
                // Embeddings of short utterances scatter widely; letting them found clusters
                // splits one person into many "说话人 N".
                if audio.count >= CaptionStream.rate * 3 / 2 {
                    let (label, relabels) = tracker.assignReportingRelabels(observation)
                    speaker = label
                    for r in relabels {
                        send(Methods.Meeting.nSpeakerRelabel, SpeakerRelabelEvent(sessionId: sessionId, from: r.from, to: r.to))
                    }
                } else {
                    // Too short to shape a cluster centroid, but enough to guess.
                    speaker = tracker.peek(observation)
                }
            }
            send(Methods.Meeting.nCaptionFinal, CaptionFinalEvent(
                sessionId: sessionId, utteranceId: id, origin: stream.origin, startMs: stream.ms(start),
                endMs: stream.ms(end), text: text, language: languageCode ?? ASRLanguage.guess(fromText: text),
                speaker: speaker, spatial: wireCue))
        }
    }

    /// Runs Qwen3-ASR on 16 kHz audio.
    private func transcribe(_ audio: [Float], _ models: LiveModels) -> String {
        guard audio.count >= CaptionStream.rate / 5 else { return "" }
        let seconds = Double(audio.count) / Double(CaptionStream.rate)
        return models.asr.transcribe(audio: audio, sampleRate: CaptionStream.rate, language: languageName,
                                     maxTokens: ASRLanguage.maxTokens(forSeconds: seconds))
            .trimmingCharacters(in: .whitespacesAndNewlines)
    }

    /// Computes a normalised speaker embedding, or `nil` without a speaker model.
    private func embed(_ audio: [Float], _ models: LiveModels) -> [Float]? {
        guard let speaker = models.speaker else { return nil }
        let v = speaker.embed(audio: audio, sampleRate: CaptionStream.rate)
        return v.allSatisfy({ $0 == 0 }) ? nil : normalized(v)
    }

    /// Encodes and sends one notification.
    private func send<T: Encodable>(_ method: String, _ payload: T) {
        do { notify(method, try JSONValue.from(payload)) } catch {
            log("captions \(sessionId): cannot encode \(method): \(error)")
        }
    }
}
