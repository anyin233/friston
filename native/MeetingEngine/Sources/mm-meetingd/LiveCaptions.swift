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
    /// `nil` when Sortformer failed to load: speakers then come from embeddings (and direction) only.
    let turns: SortformerDiarizer?
}

/// A caption work item for ``LiveCaptioner``, after Sortformer has had its say.
enum CaptionWork: Equatable {
    /// Transcribe `start..<end` as a replaceable caption.
    case partial(id: Int, start: Int, end: Int)
    /// Transcribe `start..<end` as the stable text of `id`. `cuts` are the Sortformer speaker
    /// changes inside it (ascending, usually empty); fusion decides whether each cut stands.
    case final(id: Int, start: Int, end: Int, hadPartial: Bool, cuts: [Int])
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
    private var loggedJumps = 0

    // Sortformer speaker turns: a second timbre/turn signal next to the per-utterance embeddings.
    private let ids: UtteranceIDAllocator
    private var turnSession: SortformerStreamingSession?
    private var turnsFailed = false
    private var timeline = SortformerTimeline()
    /// Last snapshot shape, to skip re-mapping when no new frames were confirmed.
    private var lastSnapshot: (count: Int, end: Float) = (0, 0)
    /// Finals waiting for Sortformer to confirm their audio, so their speaker and split points
    /// use the turn signal. Partials are never held.
    private var held: [UtteranceSegmenter.Action] = []

    static let rate = 16_000
    /// 16 kHz samples Sortformer needs beyond a frame before that frame is confirmed: one
    /// 480 ms core chunk plus its 560 ms lookahead and the mel extraction margin.
    static let turnLag: Int = {
        let c = SpeechModelIDs.turnDiarizerConfig
        return (Int(c.chunkLenSeconds) + Int(c.rightContextSeconds)) * c.subsamplingFactor * c.hopLength + 2 * c.hopLength
    }()
    /// Longest a final waits for Sortformer; beyond this the caption is sent with whatever is confirmed.
    static let maxHold = rate * 5 / 2

    /// Creates a stream.
    /// - Parameters:
    ///   - origin: Microphone or system audio.
    ///   - inbox: Where the capture thread drops audio.
    ///   - ids: Session-wide utterance IDs.
    ///   - spatial: Mic pair for direction cues, or `nil` for mono/virtual inputs.
    init(origin: AudioOrigin, inbox: CaptionInbox, ids: UtteranceIDAllocator, spatial: SpatialArrayProfile?) {
        self.origin = origin
        self.inbox = inbox
        self.ids = ids
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
    func ingest(_ batch: CaptionInbox.Batch, models: LiveModels) -> [CaptionWork] {
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
        pushTurns(samples, models: models)
        return releaseFinals(Self.coalesce(actions), flushing: false)
    }

    /// Ends the stream: closes any open utterance.
    /// - Returns: The last final, if any.
    func flush() -> [CaptionWork] {
        guard let vad else { return [] }
        var actions: [UtteranceSegmenter.Action] = []
        for case .speechEnded(let segment) in vad.flush() {
            actions += segmenter.speechEnded(endSample: Int(segment.endTime * Float(Self.rate)))
        }
        // The VAD may have closed already while our segmenter still holds an utterance.
        actions += segmenter.speechEnded(endSample: processed)
        if let session = turnSession {
            do { applySnapshot(try session.finish(), confirmedUntil: processed) } catch {
                log("captions \(origin.rawValue): Sortformer finish failed: \(error)")
            }
        }
        return releaseFinals(Self.coalesce(actions), flushing: true)
    }

    // MARK: Speaker turns (Sortformer)

    /// Feeds the 16 kHz samples the VAD just saw to this stream's Sortformer session.
    /// - Parameters:
    ///   - samples: New resampled audio, contiguous with everything pushed before.
    ///   - models: Loaded models; Sortformer is optional.
    private func pushTurns(_ samples: [Float], models: LiveModels) {
        guard !turnsFailed, let diarizer = models.turns else { return }
        if turnSession == nil { turnSession = diarizer.makeStreamingSession() }
        guard let session = turnSession else { return }
        do {
            let snapshot = try session.push(audio: samples)
            applySnapshot(snapshot, confirmedUntil: max(0, processed - Self.turnLag))
        } catch {
            // Degrade to embeddings only; held finals are released on this pass.
            log("captions \(origin.rawValue): Sortformer disabled after error: \(error)")
            turnsFailed = true
            turnSession = nil
        }
    }

    /// Converts a whole-stream Sortformer snapshot into stream samples and updates the timeline.
    private func applySnapshot(_ result: DiarizationResult, confirmedUntil: Int) {
        let shape = (result.segments.count, result.segments.last?.endTime ?? 0)
        guard shape != lastSnapshot || confirmedUntil > timeline.confirmedUntil else { return }
        lastSnapshot = shape
        let turns = result.segments.map {
            SortformerTurn(start: Int(($0.startTime * Float(Self.rate)).rounded()),
                           end: Int(($0.endTime * Float(Self.rate)).rounded()), slot: $0.speakerId)
        }
        timeline.update(snapshot: turns, confirmedUntil: confirmedUntil)
    }

    /// Whether Sortformer output can still arrive for this stream.
    private var turnsActive: Bool { turnSession != nil && !turnsFailed }

    /// Holds finals until Sortformer has confirmed their audio, then splits them at speaker changes.
    /// - Parameters:
    ///   - actions: New work items in order.
    ///   - flushing: Release everything (end of stream).
    /// - Returns: Partials as they come, and the finals that are ready.
    private func releaseFinals(_ actions: [UtteranceSegmenter.Action], flushing: Bool) -> [CaptionWork] {
        var out: [CaptionWork] = []
        for action in actions {
            switch action {
            case .final: held.append(action)
            case .partial(let id, let start, let end): out.append(.partial(id: id, start: start, end: end))
            }
        }
        while let first = held.first, case .final(let id, let start, let end, let hadPartial) = first {
            let confirmed = timeline.confirmedUntil >= end
            guard flushing || !turnsActive || confirmed || processed - end >= Self.maxHold else { break }
            held.removeFirst()
            out.append(.final(id: id, start: start, end: end, hadPartial: hadPartial,
                              cuts: timeline.splitPoints(start: start, end: end)))
        }
        return out
    }

    /// A fresh utterance id, for the later pieces of a split final.
    func nextUtteranceId() -> Int { ids.next() }

    /// The Sortformer slot that dominates `start..<end`, if Sortformer is running and decided.
    func turnSlot(_ start: Int, _ end: Int) -> Int? {
        timeline.dominantSlot(start: start, end: end)
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
        // Held finals still need their audio.
        let heldStart = held.compactMap { action -> Int? in
            if case .final(_, let start, _, _) = action { return start } else { return nil }
        }.min() ?? processed
        let keepFrom = min(segmenter.activeStart ?? processed, processed - Self.rate, heldStart) - Self.rate / 10
        let drop = min(keepFrom - audioBase, audio.count)
        if drop > 0 {
            audio.removeFirst(drop)
            audioBase += drop
        }
        let cueFloor = min(segmenter.activeStart ?? processed, heldStart) - Self.rate
        frameCues.removeAll { $0.sample < cueFloor }
        timeline.trim(before: cueFloor)
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
        logDelayJumps()
    }

    /// Summarises direction over an utterance.
    /// - Returns: The cue for fusion, `caption/final.spatial` and `spatial/cue`, or `nil` for
    ///   mono input / no usable frames.
    func direction(_ start: Int, _ end: Int) -> UtteranceDirection? {
        guard let stereo, let spatial else { return nil }
        let frames = frameCues.filter { $0.sample >= start && $0.sample < end }.map(\.cue)
        guard let cue = stereo.segmentCue(frames) else { return nil }
        let delays = frames.filter { $0.confidence >= stereo.config.minConfidence }.map(\.delaySamples).sorted()
        let median = delays.isEmpty ? 0 : delays[delays.count / 2]
        let azimuth = spatial.azimuthDeg(delaySamples: median)
        return UtteranceDirection(
            fusion: cue,
            caption: CaptionSpatialCue(azimuthDeg: azimuth, tdoaSamples: median, confidence: cue.confidence),
            position: stereo.reportedPosition(of: cue), azimuthDeg: azimuth)
    }

    /// Logs once per detected inter-channel delay jump, so field logs show when a wireless
    /// link re-paired and positions were re-learned.
    /// Logs the learned stereo statistics (calibration aid); no-op for mono streams.
    func logStereoSummary() {
        if let stereo { log("captions \(origin.rawValue): stereo \(stereo.summary)") }
    }

    private func logDelayJumps() {
        guard let stereo, stereo.delayJumps > loggedJumps else { return }
        loggedJumps = stereo.delayJumps
        log("captions \(origin.rawValue): stereo delay jump #\(loggedJumps) detected; direction statistics restarted")
    }
}

/// Direction of one utterance in the three shapes the engine needs.
struct UtteranceDirection {
    /// Relative cue for ``SpeakerFusion``.
    let fusion: SpatialCue
    /// `caption/final.spatial`.
    let caption: CaptionSpatialCue
    /// `spatial/cue` coordinates (screen convention, see ``StereoCueAnalyzer/reportedPosition(of:)``).
    let position: [Float]
    /// Degrees, positive to the right; only with a known mic spacing.
    let azimuthDeg: Float?
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
    /// Set once model loading has either succeeded or failed.
    private let settled = LockedValue(false)
    private var loop: Task<Void, Never>?

    /// Creates the captioner; nothing runs until ``start()``.
    /// - Parameters:
    ///   - sessionId: Capture session id for the notifications.
    ///   - plan: The capture plan (sources, language, voiceprints, mic device).
    ///   - micRate: Microphone device rate, when recording the mic.
    ///   - systemRate: System-audio rate, when recording it.
    ///   - notify: Notification sink.
    convenience init(sessionId: String, plan: CapturePlan, micRate: Double?, systemRate: Double?,
                     notify: @escaping Notify) {
        self.init(sessionId: sessionId, preset: plan.preset, microphone: plan.microphone, micRate: micRate,
                  systemRate: systemRate, notify: notify)
    }

    /// Creates the captioner from its parts (also used by `caption/simulateFile`, which has no plan).
    /// - Parameters:
    ///   - sessionId: Session id for the notifications.
    ///   - preset: Language and voiceprints.
    ///   - microphone: The mic device, for direction cues; `nil` when unknown (no direction).
    ///   - micRate: Rate of the microphone stream, or `nil` for none.
    ///   - systemRate: Rate of the system-audio stream, or `nil` for none.
    ///   - maxBacklogSeconds: Audio an inbox keeps while the loop is busy or models load.
    ///   - notify: Notification sink.
    init(sessionId: String, preset: CapturePreset, microphone mic: AudioInputInfo?, micRate: Double?,
         systemRate: Double?, maxBacklogSeconds: Double = 60, notify: @escaping Notify) {
        self.sessionId = sessionId
        self.notify = notify
        let ids = UtteranceIDAllocator()
        var streams: [CaptionStream] = []
        var micSpatial: SpatialArrayProfile?
        if let micRate {
            let inbox = CaptionInbox(sampleRate: micRate, maxSeconds: maxBacklogSeconds)
            // Virtual/aggregate devices and multichannel devices without a preset give no pair.
            // Test rigs (loopback device fed a synthetic stereo signal) opt in by UID; the app never sets it.
            let testUID = ProcessInfo.processInfo.environment["MM_SPATIAL_TEST_DEVICE_UID"]
            let spatial = (mic.flatMap { try? SpatialArrayProfile.resolve(for: $0, testDeviceUID: testUID) } ?? nil)?
                .adjusted(flip: preset.spatialFlip, spacingCm: preset.spatialSpacingCm)
            micSpatial = spatial
            streams.append(CaptionStream(origin: .microphone, inbox: inbox, ids: ids, spatial: spatial))
            microphone = inbox
        } else {
            microphone = nil
        }
        if let systemRate {
            let inbox = CaptionInbox(sampleRate: systemRate, maxSeconds: maxBacklogSeconds)
            streams.append(CaptionStream(origin: .system, inbox: inbox, ids: ids, spatial: nil))
            system = inbox
        } else {
            system = nil
        }
        self.streams = streams
        microphoneSpatial = micSpatial
        let samples = preset.voiceprints ?? []
        let bank = VoiceprintBank(samples: samples, model: SpeechModelIDs.speakerEmbedding)
        if !samples.isEmpty {
            log("captions \(sessionId): \(bank.personCount) enrolled people (\(samples.count) samples, model \(SpeechModelIDs.speakerEmbedding))")
        }
        tracker = OnlineSpeakerTracker(bank: bank, config: .fromEnvironment())
        languageName = ASRLanguage.qwenName(for: preset.captionLanguage)
        languageCode = ASRLanguage.primaryCode(for: preset.captionLanguage)
    }

    /// The mic channels the direction analysis uses; the recorder copies them out of each buffer.
    let microphoneSpatial: SpatialArrayProfile?

    /// Starts loading models and the caption loop in the background.
    func start() {
        guard !streams.isEmpty else { return }
        loop = Task.detached(priority: .userInitiated) { [self] in await run() }
    }

    /// Waits until model loading has finished, successfully or not.
    /// - Returns: Whether captions are running.
    func waitUntilSettled() async -> Bool {
        while !settled.get() { try? await Task.sleep(for: .milliseconds(20)) }
        return ready.get()
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
            async let turns = Self.loadTurnDiarizer(sessionId)
            let loaded = (try await asr, try await vad, await speaker, await turns)
            if loaded.2 == nil { log("captions \(sessionId): speaker embeddings unavailable; speakers will be unknown") }
            models = LiveModels(asr: loaded.0.value, vad: loaded.1.value, speaker: loaded.2?.value, turns: loaded.3?.value)
        } catch {
            log("captions \(sessionId): disabled, models unavailable: \(error)")
            microphone?.close()
            system?.close()
            _ = settled.exchange(true)
            return
        }
        _ = ready.exchange(true)
        _ = settled.exchange(true)
        log("captions \(sessionId): running (language \(languageName ?? "auto"))")
        while true {
            let last = stopping.get()
            for stream in streams {
                let batch = stream.inbox.take()
                let behind = Double(batch.mono.count) / stream.inbox.sampleRate > 1.5
                do {
                    try await MLXGate.shared.run { [self] in
                        var work = stream.ingest(batch, models: models)
                        if last { work += stream.flush() }
                        for item in work { self.perform(item, on: stream, models: models, skipPartials: behind) }
                        stream.trim()
                    }
                } catch {
                    log("captions \(sessionId) \(stream.origin.rawValue): \(error)")
                }
            }
            if last { break }
            try? await Task.sleep(for: .milliseconds(100))
        }
        for stream in streams { stream.logStereoSummary() }
        log("captions \(sessionId): stopped")
    }

    /// Loads Sortformer, logging instead of failing: captions work without it.
    /// - Parameter sessionId: For the log line.
    /// - Returns: The diarizer, or `nil` when it cannot be downloaded or loaded.
    private static func loadTurnDiarizer(_ sessionId: String) async -> Unchecked<SortformerDiarizer>? {
        do { return try await SpeechModels.shared.turnDiarizer() } catch {
            log("captions \(sessionId): Sortformer unavailable, speakers from embeddings only: \(error)")
            return nil
        }
    }

    /// Transcribes one work item and sends its notifications (runs inside ``MLXGate``).
    private func perform(_ work: CaptionWork, on stream: CaptionStream, models: LiveModels, skipPartials: Bool) {
        switch work {
        case .partial(let id, let start, let end):
            guard !skipPartials else { return }
            let audio = stream.slice(start, end)
            let text = transcribe(audio, models)
            guard !text.isEmpty else { return }
            var speaker = SpeakerLabel.unknown
            if audio.count >= CaptionStream.rate * 3 / 2, let embedding = embed(audio, models) {
                speaker = tracker.peek(SpeechObservation(embedding: embedding, origin: stream.origin,
                                                         turnSlot: stream.turnSlot(start, end)))
            }
            send(Methods.Meeting.nCaptionPartial, CaptionPartialEvent(
                sessionId: sessionId, utteranceId: id, origin: stream.origin, startMs: stream.ms(start),
                text: text, speaker: speaker))

        case .final(let id, let start, let end, let hadPartial, let cuts) where cuts.isEmpty:
            let audio = stream.slice(start, end)
            let text = transcribe(audio, models)
            guard !text.isEmpty || hadPartial else { return }
            let speaker = text.isEmpty ? SpeakerLabel.unknown : attribute(audio, start, end, on: stream, models: models)
            sendFinal(id: id, start, end, text: text, speaker: speaker, on: stream)

        case .final(let id, let start, let end, let hadPartial, let cuts):
            // Attribute each Sortformer piece first: where fusion hears the same person on both
            // sides of a cut, the cut is Sortformer slot jitter and the caption stays whole.
            let bounds = [start] + cuts + [end]
            let labels = (0..<bounds.count - 1).map { i in
                attribute(stream.slice(bounds[i], bounds[i + 1]), bounds[i], bounds[i + 1], on: stream, models: models)
            }
            let pieces = SortformerTimeline.joinPieces(bounds: bounds, labels: labels)
            if pieces.count > 1 {
                log("captions \(stream.origin.rawValue): utterance \(id) split at "
                    + "\(pieces.dropFirst().map { stream.ms($0.start) }) ms (speaker change)")
            }
            for (k, piece) in pieces.enumerated() {
                let text = transcribe(stream.slice(piece.start, piece.end), models)
                // The first piece keeps the id, so it retires the partial on screen even when empty.
                guard !text.isEmpty || (k == 0 && hadPartial) else { continue }
                sendFinal(id: k == 0 ? id : stream.nextUtteranceId(), piece.start, piece.end, text: text,
                          speaker: text.isEmpty ? .unknown : piece.label, on: stream)
            }
        }
    }

    /// Decides who spoke `start..<end`, updating the tracker and broadcasting relabels.
    /// - Parameters:
    ///   - audio: The 16 kHz audio of the span.
    ///   - start: Span start (stream samples), for direction and Sortformer lookups.
    ///   - end: Span end.
    ///   - stream: The stream it came from.
    ///   - models: Loaded models.
    /// - Returns: The label; `unknown` below 0.4 s or without an embedding model.
    private func attribute(_ audio: [Float], _ start: Int, _ end: Int, on stream: CaptionStream,
                           models: LiveModels) -> SpeakerLabel {
        guard audio.count >= CaptionStream.rate * 2 / 5, let embedding = embed(audio, models) else { return .unknown }
        let observation = SpeechObservation(embedding: embedding, origin: stream.origin,
                                            spatial: stream.direction(start, end)?.fusion,
                                            turnSlot: stream.turnSlot(start, end))
        // Embeddings of short utterances scatter widely; letting them found clusters
        // splits one person into many "说话人 N".
        guard audio.count >= CaptionStream.rate * 3 / 2 else {
            // Too short to shape a cluster centroid, but enough to guess.
            return tracker.peek(observation)
        }
        let (label, relabels) = tracker.assignReportingRelabels(observation)
        for r in relabels {
            send(Methods.Meeting.nSpeakerRelabel, SpeakerRelabelEvent(sessionId: sessionId, from: r.from, to: r.to))
        }
        return label
    }

    /// Sends one `caption/final`, then its `spatial/cue` when the utterance has a direction.
    private func sendFinal(id: Int, _ start: Int, _ end: Int, text: String, speaker: SpeakerLabel,
                           on stream: CaptionStream) {
        let direction = stream.direction(start, end)
        let (startMs, endMs) = (stream.ms(start), stream.ms(end))
        send(Methods.Meeting.nCaptionFinal, CaptionFinalEvent(
            sessionId: sessionId, utteranceId: id, origin: stream.origin, startMs: startMs,
            endMs: endMs, text: text, language: languageCode ?? ASRLanguage.guess(fromText: text),
            speaker: speaker, spatial: direction?.caption))
        // An empty final only retracts a partial: no utterance on screen to place.
        guard let direction, !text.isEmpty else { return }
        send(Methods.Meeting.nSpatialCue, SpatialCueEvent(
            sessionId: sessionId, utteranceId: id, origin: stream.origin, startMs: startMs, endMs: endMs,
            position: direction.position, confidence: direction.fusion.confidence,
            azimuthDeg: direction.azimuthDeg, speaker: speaker))
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
