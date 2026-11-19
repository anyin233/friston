@preconcurrency import AudioCommon
import AVFoundation
import EngineKit
import Foundation
import MeetingCore
@preconcurrency import Qwen3ASR
@preconcurrency import SpeechVAD

/// Wraps `SileroVADModel.makeIndependentStream()` so every 32 ms chunk reports its speech
/// probability and RMS amplitude without running Silero inference a second time.
private final class SpeechProbabilitySpyVAD: StreamingVADProvider {
    private let inner: any StreamingVADProvider
    private let onChunk: (_ endSample: Int, _ prob: Float, _ rms: Float) -> Void
    private var samplesFed = 0

    var inputSampleRate: Int { inner.inputSampleRate }
    var chunkSize: Int { inner.chunkSize }

    init(
        inner: any StreamingVADProvider,
        onChunk: @escaping (_ endSample: Int, _ prob: Float, _ rms: Float) -> Void
    ) {
        self.inner = inner
        self.onChunk = onChunk
    }

    func processChunk(_ samples: [Float]) -> Float {
        let prob = inner.processChunk(samples)
        samplesFed += samples.count
        onChunk(samplesFed, prob, CloudUploadGate.rms(samples))
        return prob
    }

    func resetState() {
        samplesFed = 0
        inner.resetState()
    }
}

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
    var asr: Qwen3ASRModel?
    let vad: SileroVADModel
    /// `nil` when the embedding model failed to load: captions continue with `unknown` speakers.
    let speaker: WeSpeakerModel?
    /// `nil` when Sortformer failed to load: speakers then come from embeddings (and direction) only.
    let turns: SortformerDiarizer?
    /// Prefix-rollback decoding for partials; `nil` falls back to re-transcribing from scratch.
    let prefix: PrefixDecoder?
    /// Streaming Zipformer draft captions (Chinese only), used per ``DraftPolicy``: when a
    /// stream switches to it, partials come from it and Qwen3-ASR only produces finals.
    let draft: DraftModel?
    /// How streams choose between the draft and Qwen3-ASR partials.
    let draftMode: DraftPolicy.Mode
}

/// A caption work item for ``LiveCaptioner``, after Sortformer has had its say.
enum CaptionWork: Equatable {
    /// Transcribe `start..<end` as a replaceable caption.
    case partial(id: Int, start: Int, end: Int)
    /// Show `text` from the streaming draft model as the replaceable caption of `id`.
    case draft(id: Int, start: Int, end: Int, text: String, stableChars: Int)
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
    /// Adapts the partial refresh to this machine's decode speed. The floor is lower than the
    /// pacer's default because prefix-rollback partials only decode the tail of the sentence.
    private var pacer = PartialPacer(initial: CaptionStream.segmenterConfig.partialInterval, minInterval: 0.2)
    /// Tokens of the latest partial per open utterance, forced (minus the rollback) on the next one.
    private var hypotheses: [Int: [Int32]] = [:]
    /// Speaker guess per open utterance, taken once so partials do not re-run the embedding model.
    private var partialSpeakers: [Int: SpeakerLabel] = [:]
    /// Draft decoding of the utterance in progress.
    private var draft: (id: Int, stream: DraftStream, fedUntil: Int, text: String)?
    /// Latest non-empty draft text per utterance, preserved until finalized as an offline backup.
    private var lastDraftText: [Int: String] = [:]
    /// Utterances that showed a draft, so their final retires it even when ASR hears nothing.
    private var drafted: Set<Int> = []
    /// Local VAD + RMS cloud upload gate and metered sample counter.
    private var cloudGate = CloudUploadGate()
    /// Peak per-32 ms RMS observed while each utterance was active in VAD.
    private var utterancePeakRMS: [Int: Float] = [:]
    /// RMS of the most recently processed 32 ms VAD chunk (seeds newly started utterances).
    private var lastChunkRMS: Float = 0
    /// Audio fed to the draft before the VAD's onset, like ``slice(_:_:)``'s pre-roll.
    static let draftPreRoll = rate / 10

    /// Action requested by ``takeCloudStreamingAction()``.
    enum CloudStreamAction {
        case idle
        case abort(id: Int)
        case upload(id: Int, start: Int, end: Int, startMs: Int, samples: [Float])
    }

    /// Result of closing cloud accounting for one finalized utterance in ``finalizeCloudUtterance(id:start:end:)``.
    enum CloudFinalAudio {
        case skip
        case commit(tailAudio: [Float], fullAudio: [Float], uploadedAudioMs: Int, wasOpened: Bool)
    }

    /// Whether any VAD utterance or Sortformer hold is still pending on this stream.
    var hasPendingCloudWork: Bool { !held.isEmpty || segmenter.activeID != nil }

    /// Latest non-empty Zipformer draft text captured for `id`, if any.
    func draftText(for id: Int) -> String { lastDraftText[id] ?? "" }

    /// Extracts incremental VAD-gated 16 kHz audio for cloud streaming, or aborts an orphaned blip.
    func takeCloudStreamingAction() -> CloudStreamAction {
        let heldIds = Set(held.compactMap { action -> Int? in
            if case .final(let id, _, _, _) = action { return id } else { return nil }
        })
        let step = cloudGate.nextStreamingStep(
            activeId: segmenter.activeID,
            activeStart: segmenter.activeStart,
            processed: processed,
            audioBase: audioBase,
            heldFinalIds: heldIds
        )
        switch step {
        case .idle:
            return .idle
        case .abort(let id):
            return .abort(id: id)
        case .upload(let id, let range, _):
            guard let start = segmenter.activeStart else { return .idle }
            return .upload(id: id, start: start, end: range.upperBound, startMs: ms(start), samples: exactSlice(range))
        }
    }

    /// Closes cloud accounting for a finalized utterance, filtering sub-floor noise and computing
    /// exact uploaded duration (`uploadedAudioMs`) excluding un-uploaded trailing silence.
    func finalizeCloudUtterance(id: Int, start: Int, end: Int) -> CloudFinalAudio {
        let fullAudio = slice(start, end)
        let peakRMS = max(utterancePeakRMS[id] ?? 0, CloudUploadGate.rms(fullAudio))
        switch cloudGate.finalize(id: id, start: start, end: end, audioBase: audioBase, maxRMS: peakRMS) {
        case .skip:
            return .skip
        case .commit(let tailRange, let totalSamples, let wasOpened):
            let tailAudio = tailRange.map { exactSlice($0) } ?? []
            let uploadedMs = Int((Double(max(0, totalSamples)) / Double(Self.rate) * 1000).rounded())
            return .commit(tailAudio: tailAudio, fullAudio: fullAudio, uploadedAudioMs: uploadedMs, wasOpened: wasOpened)
        }
    }
    /// Draft or Qwen3-ASR partials; set by the captioner once models are loaded.
    var draftPolicy = DraftPolicy(mode: .auto)
    /// Decode timings for the end-of-session log line.
    private(set) var stats = DecodeStats()
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
        segmenter = UtteranceSegmenter(config: Self.segmenterConfig, ids: ids)
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
            vad = StreamingVADProcessor(
                provider: SpeechProbabilitySpyVAD(inner: models.vad.makeIndependentStream()) { [weak self] endSample, prob, rms in
                    guard let self else { return }
                    self.lastChunkRMS = rms
                    self.cloudGate.observeVADChunk(endSample: endSample, speechProb: prob, rms: rms)
                    if let id = self.segmenter.activeID {
                        self.utterancePeakRMS[id] = max(self.utterancePeakRMS[id] ?? 0, rms)
                    }
                },
                config: Self.vadConfig
            )
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
                    if let id = segmenter.activeID {
                        utterancePeakRMS[id] = max(utterancePeakRMS[id] ?? 0, lastChunkRMS)
                    }
                case .speechEnded(let segment):
                    actions += segmenter.speechEnded(endSample: Int(segment.endTime * Float(Self.rate)))
                }
            }
            processed += end - offset
            actions += segmenter.advance(to: processed) { [self] range in exactSlice(range) }
            offset = end
        }
        pushTurns(samples, models: models)
        return releaseFinals(Self.coalesce(actions), flushing: false) + advanceDraft(models: models)
    }

    /// Feeds the utterance in progress to the streaming draft model.
    ///
    /// A new utterance (VAD onset or a split) gets a fresh stream, primed with its audio since
    /// the onset; afterwards only new samples are fed, so each call costs one or two 320 ms
    /// chunks at most.
    /// - Parameter models: Loaded models; without a draft model nothing happens.
    /// - Returns: A draft caption when its text changed.
    private func advanceDraft(models: LiveModels) -> [CaptionWork] {
        guard let model = models.draft, let start = segmenter.activeStart, let id = segmenter.activeID else {
            draft = nil
            return []
        }
        if draft?.id != id {
            // Decided per utterance so one caption never mixes the two sources.
            guard draftPolicy.useDraft else {
                draft = nil
                return []
            }
            draft = (id, model.makeStream(), max(audioBase, start - Self.draftPreRoll), "")
        }
        guard var d = draft, processed > d.fedUntil else { return [] }
        let text = d.stream.accept(exactSlice(d.fedUntil..<processed))
        d.fedUntil = processed
        defer { draft = d }
        guard !text.isEmpty else { return [] }
        lastDraftText[id] = text
        guard text != d.text else { return [] }
        d.text = text
        drafted.insert(id)
        return [.draft(id: id, start: start, end: processed, text: text, stableChars: DraftCaption.stableScalars(text))]
    }

    /// Whether `id` showed a draft caption that its final must retire.
    func hadDraft(_ id: Int) -> Bool { drafted.contains(id) }

    /// Whether the draft model owns the partials of `id`.
    func draftOwns(_ id: Int) -> Bool { draft?.id == id }

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

    /// Segmentation: first partial after 0.25 s of confirmed speech, then as often as
    /// ``PartialPacer`` allows (starting at 0.5 s) and at every word gap of ≥ 0.15 s; utterances
    /// past 8 s are cut at their longest pause, past 12 s at the quietest point. See
    /// `docs/engine-protocol.md` › 实时字幕.
    static let segmenterConfig = UtteranceSegmenter.Config(
        sampleRate: rate, partialInterval: 0.5, firstPartialDelay: 0.25, maxDuration: 12, splitAfter: 8, minPause: 0.2,
        pauseTrigger: 0.15, minPauseAdvance: 0.1)

    /// Tokens of the previous partial of `id`, for prefix-rollback decoding.
    func hypothesis(_ id: Int) -> [Int32] { hypotheses[id] ?? [] }

    /// Stores the newest partial tokens of `id`.
    func setHypothesis(_ id: Int, _ tokens: [Int32]) { hypotheses[id] = tokens }

    /// The cached partial speaker guess of `id`, computing it once with `make` when absent.
    /// - Parameters:
    ///   - id: Utterance.
    ///   - make: Computes the guess; returning `nil` (too little audio) leaves the cache empty.
    /// - Returns: The guess, or `unknown`.
    func partialSpeaker(_ id: Int, _ make: () -> SpeakerLabel?) -> SpeakerLabel {
        if let known = partialSpeakers[id] { return known }
        guard let label = make() else { return .unknown }
        partialSpeakers[id] = label
        return label
    }

    /// Drops per-utterance partial state once the utterance is final.
    func forget(_ id: Int) {
        hypotheses[id] = nil
        partialSpeakers[id] = nil
        drafted.remove(id)
        utterancePeakRMS[id] = nil
        lastDraftText[id] = nil
    }

    /// Records how long a partial took and retunes the refresh interval.
    /// - Parameter seconds: Wall time of the partial's transcription.
    func recordPartialDecode(seconds: Double) {
        stats.partials.append(seconds)
        pacer.record(decodeSeconds: seconds)
        segmenter.setPartialInterval(pacer.interval)
        if draftPolicy.observe(partialInterval: pacer.interval) {
            log("captions \(origin.rawValue): Qwen3-ASR partials refresh every \(Int(pacer.interval * 1000)) ms; "
                + "switching to draft captions")
        }
    }

    /// Records how long a final took to transcribe.
    /// - Parameter seconds: Wall time of the final's transcription.
    func recordFinalDecode(seconds: Double) { stats.finals.append(seconds) }

    /// Counts a partial dropped because the loop was behind real time.
    func recordSkippedPartial() { stats.skippedPartials += 1 }

    /// The partial refresh interval in use, for the summary log.
    var partialInterval: Double { pacer.interval }

    /// Audio for exactly `range` (16 kHz stream samples), clamped to what is buffered; used by
    /// the segmenter's pause search, which needs positions to line up with the range start.
    private func exactSlice(_ range: Range<Int>) -> [Float] {
        let lo = max(audioBase, range.lowerBound) - audioBase
        let hi = min(audioBase + audio.count, range.upperBound) - audioBase
        // A range starting before the buffer would shift every position; return nothing instead.
        guard range.lowerBound >= audioBase, hi > lo else { return [] }
        return Array(audio[lo..<hi])
    }

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
    private let rawLanguageHint: String?
    private let cloudConfig: LiveAsrConfig?
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
        rawLanguageHint = preset.captionLanguage
        cloudConfig = preset.liveAsr
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

    private struct CloudActiveSlice: Sendable {
        let id: Int
        let startMs: Int
        let speaker: SpeakerLabel
        let samples: [Float]
    }

    private struct CloudRetraction: Sendable {
        let id: Int
        let origin: AudioOrigin
        let startMs: Int
        let endMs: Int
    }

    private struct CloudFinalJob: Sendable {
        let id: Int
        let origin: AudioOrigin
        let startMs: Int
        let endMs: Int
        let audioMs: Int
        let hadPartial: Bool
        let tailAudio: [Float]
        let fullAudio: [Float]
        let speaker: SpeakerLabel
        let captionSpatial: CaptionSpatialCue?
        let cueEvent: SpatialCueEvent?
    }

    private struct CloudTickWork: Sendable {
        let abortId: Int?
        let retracts: [CloudRetraction]
        let active: CloudActiveSlice?
        let finals: [CloudFinalJob]
    }

    private struct UtteranceJob: Sendable {
        let id: Int
        let origin: AudioOrigin
        let startMs: Int
        let endMs: Int
        let audioMs: Int
        let hadPartial: Bool
        let draftText: String
        let fullAudio: [Float]
        let speaker: SpeakerLabel
        let captionSpatial: CaptionSpatialCue?
        let cueEvent: SpatialCueEvent?
    }

    private enum UtteranceOutcome: Sendable {
        case success(UtteranceJob, CloudUtteranceTranscript)
        case failure(UtteranceJob, Error)

        var job: UtteranceJob {
            switch self {
            case .success(let j, _), .failure(let j, _): j
            }
        }
    }

    private struct UtterancePartialNotice: Sendable {
        let id: Int
        let origin: AudioOrigin
        let startMs: Int
        let text: String
        let speaker: SpeakerLabel
        let stableChars: Int
    }

    private struct UtteranceTickWork: Sendable {
        let partials: [UtterancePartialNotice]
        let retracts: [CloudRetraction]
        let jobs: [UtteranceJob]
    }

    /// Loads local prefix & draft helpers when running local Qwen3-ASR captions (or draft-only for cloud utterance mode).
    private func loadLocalExtras(loadPrefix: Bool = true) async -> (PrefixDecoder?, DraftModel?, DraftPolicy.Mode) {
        var prefix: PrefixDecoder?
        if loadPrefix {
            // MM_PARTIAL_PREFIX=0 restores whole-utterance re-decoding, for A/B runs of eval-captions.
            if ProcessInfo.processInfo.environment["MM_PARTIAL_PREFIX"] == "0" {
                log("captions \(sessionId): prefix-rollback partials off (MM_PARTIAL_PREFIX=0)")
            } else {
                do { prefix = try await SpeechModels.shared.livePrefixDecoder().value } catch {
                    log("captions \(sessionId): prefix-rollback partials disabled, re-decoding from scratch: \(error)")
                }
            }
        }
        var draft: DraftModel?
        // MM_DRAFT=0 turns the draft layer off and MM_DRAFT=1 forces it, for A/B runs of
        // eval-captions; the default switches to it only when Qwen3-ASR partials get slow.
        // The model is Mandarin-only, so other caption languages keep Qwen3-ASR partials.
        let draftSetting = ProcessInfo.processInfo.environment["MM_DRAFT"]
        let draftMode: DraftPolicy.Mode = draftSetting == "1" ? .always : .auto
        if languageCode != "zh" {
            log("captions \(sessionId): draft captions off (language \(languageCode ?? "auto"), model is Mandarin-only)")
        } else if draftSetting == "0" {
            log("captions \(sessionId): draft captions off (MM_DRAFT=0)")
        } else {
            do { draft = try await SpeechModels.shared.draft().value } catch {
                log("captions \(sessionId): draft captions unavailable, using Qwen3-ASR partials: \(error)")
            }
        }
        return (prefix, draft, draftMode)
    }

    /// Loads local Qwen3-ASR + prefix/draft onto existing VAD/speaker/turns models.
    private func attachLocalASR(to base: LiveModels) async throws -> LiveModels {
        let asr = try await SpeechModels.shared.liveASR()
        let (prefix, draft, draftMode) = await loadLocalExtras(loadPrefix: true)
        // Without a draft model a stream must never switch away from Qwen3-ASR partials.
        let policy = draft == nil ? DraftPolicy(mode: .auto, switchInterval: .infinity) : DraftPolicy(mode: draftMode)
        for stream in streams { stream.draftPolicy = policy }
        return LiveModels(asr: asr.value, vad: base.vad, speaker: base.speaker, turns: base.turns,
                          prefix: prefix, draft: draft, draftMode: draftMode)
    }

    /// Creates lazy per-stream WebSocket clients for cloud streaming mode without connecting up front.
    private func makeCloudStreamClients(_ cfg: LiveAsrConfig) throws -> [AudioOrigin: CloudLiveStreamClient] {
        var clients: [AudioOrigin: CloudLiveStreamClient] = [:]
        for stream in streams {
            clients[stream.origin] = try CloudLiveStreamClient(config: cfg, language: rawLanguageHint, origin: stream.origin)
        }
        return clients
    }

    /// Whether a cloud failure should trigger immediate local Qwen3-ASR fallback instead of waiting 3 errors.
    private static func isImmediateCloudFallback(_ error: Error, everSucceeded: Bool) -> Bool {
        if let uttErr = error as? CloudUtteranceError, uttErr.isAuthOrQuotaError { return true }
        if let liveErr = error as? CloudLiveClientError, liveErr.isAuthOrQuotaError { return true }
        if CloudAuthErrorDetector.isAuthOrQuotaFailure(statusCode: nil, message: String(describing: error)) {
            return true
        }
        return !everSucceeded
    }

    /// Loads models (or prepares lazy cloud clients / utterance HTTP client), then processes audio every 50 ms.
    private func run() async {
        var models: LiveModels
        var cloudClients: [AudioOrigin: CloudLiveStreamClient] = [:]
        var utteranceClient: CloudUtteranceClient?
        do {
            async let vad = SpeechModels.shared.vad()
            async let speaker = try? SpeechModels.shared.speaker()
            async let turns = Self.loadTurnDiarizer(sessionId)
            let base = LiveModels(asr: nil, vad: try await vad.value, speaker: await speaker?.value,
                                  turns: await turns?.value, prefix: nil, draft: nil, draftMode: .auto)
            if base.speaker == nil { log("captions \(sessionId): speaker embeddings unavailable; speakers will be unknown") }

            if let cfg = cloudConfig {
                do {
                    switch cfg.effectiveMode {
                    case .streaming:
                        cloudClients = try makeCloudStreamClients(cfg)
                        models = base
                    case .utterance:
                        utteranceClient = try CloudUtteranceClient(config: cfg, language: rawLanguageHint)
                        let (_, draft, _) = await loadLocalExtras(loadPrefix: false)
                        let policy = draft == nil
                            ? DraftPolicy(mode: .auto, switchInterval: .infinity)
                            : DraftPolicy(mode: .always)
                        for stream in streams { stream.draftPolicy = policy }
                        models = LiveModels(asr: nil, vad: base.vad, speaker: base.speaker, turns: base.turns,
                                            prefix: nil, draft: draft, draftMode: .always)
                    }
                } catch {
                    log("captions \(sessionId): cloud live setup failed (\(cfg.provider)), falling back to local Qwen3-ASR: \(error)")
                    models = try await attachLocalASR(to: base)
                }
            } else {
                models = try await attachLocalASR(to: base)
            }
        } catch {
            log("captions \(sessionId): disabled, models unavailable: \(error)")
            microphone?.close()
            system?.close()
            _ = settled.exchange(true)
            return
        }
        _ = ready.exchange(true)
        _ = settled.exchange(true)
        if let cfg = cloudConfig, utteranceClient != nil {
            log("captions \(sessionId): running cloud utterance HTTP (\(cfg.provider) · \(cfg.model), "
                + "draft \(models.draft != nil ? "on" : "off"), language \(rawLanguageHint ?? "auto"))")
        } else if let cfg = cloudConfig, !cloudClients.isEmpty {
            log("captions \(sessionId): running cloud live streaming (\(cfg.provider) · \(cfg.model), "
                + "VAD-gated/lazy WS, language \(rawLanguageHint ?? "auto"))")
        } else {
            log("captions \(sessionId): running (language \(languageName ?? "auto"))")
        }

        let emittedCloudPartials = LockedValue<Set<Int>>([])
        var consecutiveCloudErrors = 0
        var cloudEverSucceeded = false

        var pendingUtteranceUploads: [UtteranceJob] = []
        var inFlightUtteranceTasks: [Int: Task<Void, Never>] = [:]
        let completedUtteranceUploads = LockedValue<[UtteranceOutcome]>([])
        let maxConcurrentUtteranceUploads = 4

        /// Dispatches queued utterance HTTP jobs up to `maxConcurrentUtteranceUploads`.
        func dispatchPendingUtterances(client: CloudUtteranceClient) {
            while inFlightUtteranceTasks.count < maxConcurrentUtteranceUploads,
                  !pendingUtteranceUploads.isEmpty {
                let job = pendingUtteranceUploads.removeFirst()
                inFlightUtteranceTasks[job.id] = Task {
                    do {
                        let transcript = try await client.transcribe(samples16k: job.fullAudio)
                        completedUtteranceUploads.mutate { $0.append(.success(job, transcript)) }
                    } catch {
                        completedUtteranceUploads.mutate { $0.append(.failure(job, error)) }
                    }
                }
            }
        }

        /// Drains finished utterance HTTP jobs, emitting finals + usage or falling back locally.
        func drainUtteranceOutcomes(cfg: LiveAsrConfig) async {
            var batch: [UtteranceOutcome] = []
            completedUtteranceUploads.mutate {
                batch = $0
                $0.removeAll()
            }
            if !batch.isEmpty {
                batch.sort {
                    let a = $0.job, b = $1.job
                    return (a.startMs, a.id) < (b.startMs, b.id)
                }
                for outcome in batch {
                    inFlightUtteranceTasks.removeValue(forKey: outcome.job.id)
                    switch outcome {
                    case .success(let job, let res):
                        consecutiveCloudErrors = 0
                        cloudEverSucceeded = true
                        if !res.text.isEmpty || job.hadPartial {
                            emitFinal(id: job.id, origin: job.origin, startMs: job.startMs, endMs: job.endMs,
                                      text: res.text, speaker: res.text.isEmpty ? .unknown : job.speaker,
                                      captionSpatial: job.captionSpatial, cueEvent: job.cueEvent)
                        }
                        let meteredMs = res.billedAudioMs ?? job.audioMs
                        if meteredMs > 0 || res.inputTokens > 0 || res.outputTokens > 0 {
                            send(Methods.Meeting.nCaptionUsage, CaptionUsageEvent(
                                sessionId: sessionId, provider: cfg.provider, model: cfg.model,
                                origin: job.origin, audioMs: meteredMs,
                                inputTokens: res.inputTokens, outputTokens: res.outputTokens, mode: .utterance))
                        }
                    case .failure(let job, let error):
                        consecutiveCloudErrors += 1
                        let fatal = Self.isImmediateCloudFallback(error, everSucceeded: cloudEverSucceeded)
                            || consecutiveCloudErrors >= 3
                        log("captions \(sessionId) \(job.origin.rawValue): cloud utterance \(job.id) failed (\(consecutiveCloudErrors)/3): \(error)")
                        if fatal {
                            utteranceClient = nil
                            if models.asr == nil {
                                do {
                                    models = try await attachLocalASR(to: models)
                                } catch {
                                    log("captions \(sessionId): local Qwen3-ASR fallback failed: \(error)")
                                }
                            }
                            if models.asr != nil {
                                let activeModels = models
                                let localText = (try? await MLXGate.shared.run { self.transcribe(job.fullAudio, activeModels) }) ?? ""
                                let resolvedText = localText.isEmpty ? job.draftText : localText
                                if !resolvedText.isEmpty || job.hadPartial {
                                    emitFinal(id: job.id, origin: job.origin, startMs: job.startMs, endMs: job.endMs,
                                              text: resolvedText, speaker: resolvedText.isEmpty ? .unknown : job.speaker,
                                              captionSpatial: job.captionSpatial, cueEvent: job.cueEvent)
                                }
                            } else if !job.draftText.isEmpty || job.hadPartial {
                                let fallbackText = job.draftText
                                emitFinal(id: job.id, origin: job.origin, startMs: job.startMs, endMs: job.endMs,
                                          text: fallbackText, speaker: fallbackText.isEmpty ? .unknown : job.speaker,
                                          captionSpatial: job.captionSpatial, cueEvent: job.cueEvent)
                            }
                        } else if !job.draftText.isEmpty || job.hadPartial {
                            let fallbackText = job.draftText
                            emitFinal(id: job.id, origin: job.origin, startMs: job.startMs, endMs: job.endMs,
                                      text: fallbackText, speaker: fallbackText.isEmpty ? .unknown : job.speaker,
                                      captionSpatial: job.captionSpatial, cueEvent: job.cueEvent)
                        }
                    }
                }
            }

            // If fatal fallback deactivated the cloud utterance client, drain remaining queued jobs locally.
            if utteranceClient == nil && !pendingUtteranceUploads.isEmpty {
                let queued = pendingUtteranceUploads
                pendingUtteranceUploads.removeAll()
                if models.asr == nil {
                    models = (try? await attachLocalASR(to: models)) ?? models
                }
                for q in queued {
                    let localText: String
                    if models.asr != nil {
                        let activeModels = models
                        localText = (try? await MLXGate.shared.run { self.transcribe(q.fullAudio, activeModels) }) ?? ""
                    } else {
                        localText = ""
                    }
                    let resolved = localText.isEmpty ? q.draftText : localText
                    if !resolved.isEmpty || q.hadPartial {
                        emitFinal(id: q.id, origin: q.origin, startMs: q.startMs, endMs: q.endMs,
                                  text: resolved, speaker: resolved.isEmpty ? .unknown : q.speaker,
                                  captionSpatial: q.captionSpatial, cueEvent: q.cueEvent)
                    }
                }
            }
        }

        while true {
            let last = stopping.get()
            let currentModels = models
            for stream in streams {
                let batch = stream.inbox.take()

                // Branch 1: Solution A — VAD-gated realtime WebSocket streaming.
                if let client = cloudClients[stream.origin], let cfg = cloudConfig {
                    if batch.mono.isEmpty && !last && !stream.hasPendingCloudWork {
                        await client.disconnectIfIdle(after: 60)
                        continue
                    }
                    let tick: CloudTickWork
                    do {
                        tick = try await MLXGate.shared.run { [self, currentModels] in
                            self.collectCloudTick(batch: batch, last: last, on: stream, models: currentModels)
                        }
                    } catch {
                        log("captions \(sessionId) \(stream.origin.rawValue): \(error)")
                        continue
                    }

                    if let abortId = tick.abortId {
                        emittedCloudPartials.mutate { _ = $0.remove(abortId) }
                        await client.abortUtterance(id: abortId)
                    }
                    for r in tick.retracts {
                        var cloudPartial = false
                        emittedCloudPartials.mutate { cloudPartial = ($0.remove(r.id) != nil) }
                        if cloudPartial {
                            emitFinal(id: r.id, origin: r.origin, startMs: r.startMs, endMs: r.endMs,
                                      text: "", speaker: .unknown, captionSpatial: nil, cueEvent: nil)
                        }
                    }

                    // 1. Finalize completed VAD utterances first.
                    var fellBackOnFinal = false
                    for (idx, job) in tick.finals.enumerated() {
                        var cloudPartial = false
                        emittedCloudPartials.mutate { cloudPartial = ($0.remove(job.id) != nil) }
                        let hadPartial = job.hadPartial || cloudPartial
                        do {
                            if await client.currentUtteranceId != job.id {
                                if await client.currentUtteranceId != nil {
                                    _ = try? await client.finishUtterance()
                                }
                                try await client.beginUtterance(id: job.id) { _ in }
                            }
                            if !job.tailAudio.isEmpty {
                                try await client.pushAudio16k(job.tailAudio)
                            }
                            let res = try await client.finishUtterance()
                            consecutiveCloudErrors = 0
                            cloudEverSucceeded = true
                            if !res.text.isEmpty || hadPartial {
                                emitFinal(id: job.id, origin: job.origin, startMs: job.startMs, endMs: job.endMs,
                                          text: res.text, speaker: res.text.isEmpty ? .unknown : job.speaker,
                                          captionSpatial: job.captionSpatial, cueEvent: job.cueEvent)
                            }
                            if job.audioMs > 0 || res.inputTokens > 0 || res.outputTokens > 0 {
                                send(Methods.Meeting.nCaptionUsage, CaptionUsageEvent(
                                    sessionId: sessionId, provider: cfg.provider, model: cfg.model,
                                    origin: job.origin, audioMs: job.audioMs,
                                    inputTokens: res.inputTokens, outputTokens: res.outputTokens, mode: .streaming))
                            }
                        } catch {
                            consecutiveCloudErrors += 1
                            let fatal = Self.isImmediateCloudFallback(error, everSucceeded: cloudEverSucceeded)
                                || consecutiveCloudErrors >= 3
                            log("captions \(sessionId) \(job.origin.rawValue): cloud utterance \(job.id) failed (\(consecutiveCloudErrors)/3): \(error)")
                            if fatal {
                                fellBackOnFinal = true
                                for c in cloudClients.values { await c.close() }
                                cloudClients.removeAll()
                                do {
                                    models = try await attachLocalASR(to: models)
                                } catch {
                                    log("captions \(sessionId): local Qwen3-ASR fallback failed: \(error)")
                                }
                                let activeModels = models
                                for remIdx in idx..<tick.finals.count {
                                    let rem = tick.finals[remIdx]
                                    var remPartial = hadPartial
                                    if remIdx != idx {
                                        emittedCloudPartials.mutate { remPartial = rem.hadPartial || ($0.remove(rem.id) != nil) }
                                    }
                                    let text = (try? await MLXGate.shared.run { self.transcribe(rem.fullAudio, activeModels) }) ?? ""
                                    if !text.isEmpty || remPartial {
                                        emitFinal(id: rem.id, origin: rem.origin, startMs: rem.startMs, endMs: rem.endMs,
                                                  text: text, speaker: text.isEmpty ? .unknown : rem.speaker,
                                                  captionSpatial: rem.captionSpatial, cueEvent: rem.cueEvent)
                                    }
                                }
                                break
                            }
                            if hadPartial {
                                emitFinal(id: job.id, origin: job.origin, startMs: job.startMs, endMs: job.endMs,
                                          text: "", speaker: .unknown, captionSpatial: nil, cueEvent: nil)
                            }
                        }
                    }
                    if fellBackOnFinal {
                        continue
                    }

                    // 2. Stream incremental audio for the currently active utterance.
                    if let active = tick.active {
                        do {
                            if await client.currentUtteranceId != active.id {
                                if await client.currentUtteranceId != nil {
                                    _ = try? await client.finishUtterance()
                                }
                                let uid = active.id
                                let origin = stream.origin
                                let startMs = active.startMs
                                let speaker = active.speaker
                                let sid = sessionId
                                let sink = notify
                                try await client.beginUtterance(id: uid) { text in
                                    emittedCloudPartials.mutate { _ = $0.insert(uid) }
                                    let event = CaptionPartialEvent(
                                        sessionId: sid, utteranceId: uid, origin: origin,
                                        startMs: startMs, text: text, speaker: speaker, stableChars: 0)
                                    if let encoded = try? JSONValue.from(event) {
                                        sink(Methods.Meeting.nCaptionPartial, encoded)
                                    }
                                }
                            }
                            try await client.pushAudio16k(active.samples)
                            consecutiveCloudErrors = 0
                        } catch {
                            consecutiveCloudErrors += 1
                            let fatal = Self.isImmediateCloudFallback(error, everSucceeded: cloudEverSucceeded)
                                || consecutiveCloudErrors >= 3
                            log("captions \(sessionId) \(stream.origin.rawValue): cloud stream failed (\(consecutiveCloudErrors)/3): \(error)")
                            if fatal {
                                log("captions \(sessionId): cloud live failed, switching to local Qwen3-ASR")
                                for c in cloudClients.values { await c.close() }
                                cloudClients.removeAll()
                                do {
                                    models = try await attachLocalASR(to: models)
                                } catch {
                                    log("captions \(sessionId): local Qwen3-ASR fallback failed: \(error)")
                                }
                            }
                        }
                    } else if tick.finals.isEmpty {
                        await client.disconnectIfIdle(after: 60)
                    }
                    continue
                }

                // Branch 2: Solution B — VAD + local CPU draft + per-utterance HTTP upload.
                if let uClient = utteranceClient {
                    if !batch.mono.isEmpty || last || stream.hasPendingCloudWork {
                        let uttTick: UtteranceTickWork
                        do {
                            uttTick = try await MLXGate.shared.run { [self, currentModels] in
                                self.collectUtteranceTick(batch: batch, last: last, on: stream, models: currentModels)
                            }
                            for p in uttTick.partials {
                                send(Methods.Meeting.nCaptionPartial, CaptionPartialEvent(
                                    sessionId: sessionId, utteranceId: p.id, origin: p.origin,
                                    startMs: p.startMs, text: p.text, speaker: p.speaker, stableChars: p.stableChars))
                            }
                            for r in uttTick.retracts {
                                emitFinal(id: r.id, origin: r.origin, startMs: r.startMs, endMs: r.endMs,
                                          text: "", speaker: .unknown, captionSpatial: nil, cueEvent: nil)
                            }
                            pendingUtteranceUploads.append(contentsOf: uttTick.jobs)
                            dispatchPendingUtterances(client: uClient)
                        } catch {
                            log("captions \(sessionId) \(stream.origin.rawValue): \(error)")
                        }
                    }
                    continue
                }

                // Branch 3: Local Qwen3-ASR (+ optional Zipformer draft).
                let behind = Double(batch.mono.count) / stream.inbox.sampleRate > 1.5
                do {
                    try await MLXGate.shared.run { [self, currentModels] in
                        var work = stream.ingest(batch, models: currentModels)
                        if last { work += stream.flush() }
                        for item in work { self.perform(item, on: stream, models: currentModels, skipPartials: behind) }
                        stream.trim()
                    }
                } catch {
                    log("captions \(sessionId) \(stream.origin.rawValue): \(error)")
                }
            }
            if let cfg = cloudConfig, cfg.effectiveMode == .utterance {
                if let uClient = utteranceClient {
                    dispatchPendingUtterances(client: uClient)
                }
                await drainUtteranceOutcomes(cfg: cfg)
            }
            if last { break }
            // 50 ms keeps the loop's own wait well under the 0.2 s partial floor.
            try? await Task.sleep(for: .milliseconds(50))
        }

        // Wait for all in-flight per-utterance HTTP requests before ending `stop()`.
        if let cfg = cloudConfig, cfg.effectiveMode == .utterance {
            while !pendingUtteranceUploads.isEmpty || !inFlightUtteranceTasks.isEmpty {
                if let uClient = utteranceClient {
                    dispatchPendingUtterances(client: uClient)
                }
                await drainUtteranceOutcomes(cfg: cfg)
                if let nextTask = inFlightUtteranceTasks.values.first {
                    await nextTask.value
                    await drainUtteranceOutcomes(cfg: cfg)
                }
            }
        }
        for client in cloudClients.values { await client.close() }
        for stream in streams {
            stream.logStereoSummary()
            log("captions \(sessionId) \(stream.origin.rawValue): \(stream.stats.summary), "
                + "partial interval \(Int(stream.partialInterval * 1000)) ms")
        }
        log("captions \(sessionId): stopped")
    }

    /// Runs synchronous VAD, Sortformer, WeSpeaker, and GCC-PHAT work inside ``MLXGate`` for cloud streaming mode.
    private func collectCloudTick(batch: CaptionInbox.Batch, last: Bool,
                                  on stream: CaptionStream, models: LiveModels) -> CloudTickWork {
        var work = stream.ingest(batch, models: models)
        if last { work += stream.flush() }

        var finals: [CloudFinalJob] = []
        var retracts: [CloudRetraction] = []
        for item in work {
            guard case .final(let id, let start, let end, let hadPartial, _) = item else { continue }
            let finalAudio = stream.finalizeCloudUtterance(id: id, start: start, end: end)
            stream.forget(id)
            let (startMs, endMs) = (stream.ms(start), stream.ms(end))
            switch finalAudio {
            case .skip:
                if hadPartial {
                    retracts.append(CloudRetraction(id: id, origin: stream.origin, startMs: startMs, endMs: endMs))
                }
            case .commit(let tailAudio, let fullAudio, let uploadedMs, _):
                let speaker = attribute(fullAudio, start, end, on: stream, models: models)
                let direction = stream.direction(start, end)
                let cueEvent = direction.map {
                    SpatialCueEvent(
                        sessionId: sessionId, utteranceId: id, origin: stream.origin, startMs: startMs, endMs: endMs,
                        position: $0.position, confidence: $0.fusion.confidence, azimuthDeg: $0.azimuthDeg, speaker: speaker)
                }
                finals.append(CloudFinalJob(
                    id: id, origin: stream.origin, startMs: startMs, endMs: endMs, audioMs: uploadedMs,
                    hadPartial: hadPartial, tailAudio: tailAudio, fullAudio: fullAudio, speaker: speaker,
                    captionSpatial: direction?.caption, cueEvent: cueEvent))
            }
        }

        var abortId: Int?
        var activeSlice: CloudActiveSlice?
        if !last {
            switch stream.takeCloudStreamingAction() {
            case .idle:
                break
            case .abort(let id):
                abortId = id
            case .upload(let id, let start, let end, let startMs, let samples):
                let speaker = stream.partialSpeaker(id) {
                    let full = stream.slice(start, end)
                    guard full.count >= CaptionStream.rate * 3 / 2, let embedding = embed(full, models) else { return nil }
                    return tracker.peek(SpeechObservation(embedding: embedding, origin: stream.origin,
                                                          turnSlot: stream.turnSlot(start, end)))
                }
                activeSlice = CloudActiveSlice(id: id, startMs: startMs, speaker: speaker, samples: samples)
            }
        }

        stream.trim()
        return CloudTickWork(abortId: abortId, retracts: retracts, active: activeSlice, finals: finals)
    }

    /// Runs synchronous VAD, Zipformer draft, Sortformer, WeSpeaker, and GCC-PHAT work for Solution B utterance mode.
    private func collectUtteranceTick(batch: CaptionInbox.Batch, last: Bool,
                                      on stream: CaptionStream, models: LiveModels) -> UtteranceTickWork {
        var work = stream.ingest(batch, models: models)
        if last { work += stream.flush() }

        var partials: [UtterancePartialNotice] = []
        var retracts: [CloudRetraction] = []
        var jobs: [UtteranceJob] = []

        for item in work {
            switch item {
            case .draft(let id, let start, let end, let text, let stable):
                let speaker = stream.partialSpeaker(id) {
                    let audio = stream.slice(start, end)
                    guard audio.count >= CaptionStream.rate * 3 / 2, let embedding = embed(audio, models) else { return nil }
                    return tracker.peek(SpeechObservation(embedding: embedding, origin: stream.origin,
                                                          turnSlot: stream.turnSlot(start, end)))
                }
                partials.append(UtterancePartialNotice(
                    id: id, origin: stream.origin, startMs: stream.ms(start),
                    text: text, speaker: speaker, stableChars: stable))
            case .partial:
                break
            case .final(let id, let start, let end, let hadPartial, _):
                let showedPartial = hadPartial || stream.hadDraft(id)
                let draftBackup = stream.draftText(for: id)
                let finalAudio = stream.finalizeCloudUtterance(id: id, start: start, end: end)
                stream.forget(id)
                let (startMs, endMs) = (stream.ms(start), stream.ms(end))
                switch finalAudio {
                case .skip:
                    if showedPartial {
                        retracts.append(CloudRetraction(id: id, origin: stream.origin, startMs: startMs, endMs: endMs))
                    }
                case .commit(_, let fullAudio, let uploadedMs, _):
                    let speaker = attribute(fullAudio, start, end, on: stream, models: models)
                    let direction = stream.direction(start, end)
                    let cueEvent = direction.map {
                        SpatialCueEvent(
                            sessionId: sessionId, utteranceId: id, origin: stream.origin, startMs: startMs, endMs: endMs,
                            position: $0.position, confidence: $0.fusion.confidence, azimuthDeg: $0.azimuthDeg, speaker: speaker)
                    }
                    jobs.append(UtteranceJob(
                        id: id, origin: stream.origin, startMs: startMs, endMs: endMs, audioMs: uploadedMs,
                        hadPartial: showedPartial, draftText: draftBackup, fullAudio: fullAudio,
                        speaker: speaker, captionSpatial: direction?.caption, cueEvent: cueEvent))
                }
            }
        }

        stream.trim()
        return UtteranceTickWork(partials: partials, retracts: retracts, jobs: jobs)
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
        case .draft(let id, let start, let end, let text, let stable):
            let speaker = stream.partialSpeaker(id) {
                let audio = stream.slice(start, end)
                guard audio.count >= CaptionStream.rate * 3 / 2, let embedding = embed(audio, models) else { return nil }
                return tracker.peek(SpeechObservation(embedding: embedding, origin: stream.origin,
                                                      turnSlot: stream.turnSlot(start, end)))
            }
            send(Methods.Meeting.nCaptionPartial, CaptionPartialEvent(
                sessionId: sessionId, utteranceId: id, origin: stream.origin, startMs: stream.ms(start),
                text: text, speaker: speaker, stableChars: stable))

        case .partial(let id, let start, let end):
            // The draft model already shows this utterance; Qwen3-ASR only finalizes it.
            guard !stream.draftOwns(id) else { return }
            guard !skipPartials else {
                stream.recordSkippedPartial()
                return
            }
            let audio = stream.slice(start, end)
            let began = DispatchTime.now()
            let (text, stable) = transcribePartial(id: id, audio, on: stream, models)
            stream.recordPartialDecode(seconds: Self.seconds(since: began))
            guard !text.isEmpty else { return }
            let speaker = stream.partialSpeaker(id) {
                guard audio.count >= CaptionStream.rate * 3 / 2, let embedding = embed(audio, models) else { return nil }
                return tracker.peek(SpeechObservation(embedding: embedding, origin: stream.origin,
                                                      turnSlot: stream.turnSlot(start, end)))
            }
            send(Methods.Meeting.nCaptionPartial, CaptionPartialEvent(
                sessionId: sessionId, utteranceId: id, origin: stream.origin, startMs: stream.ms(start),
                text: text, speaker: speaker, stableChars: stable))

        case .final(let id, let start, let end, var hadPartial, let cuts) where cuts.isEmpty:
            hadPartial = hadPartial || stream.hadDraft(id)
            stream.forget(id)
            let audio = stream.slice(start, end)
            let began = DispatchTime.now()
            let text = transcribe(audio, models)
            stream.recordFinalDecode(seconds: Self.seconds(since: began))
            guard !text.isEmpty || hadPartial else { return }
            let speaker = text.isEmpty ? SpeakerLabel.unknown : attribute(audio, start, end, on: stream, models: models)
            sendFinal(id: id, start, end, text: text, speaker: speaker, on: stream)

        case .final(let id, let start, let end, var hadPartial, let cuts):
            // Attribute each Sortformer piece first: where fusion hears the same person on both
            // sides of a cut, the cut is Sortformer slot jitter and the caption stays whole.
            let bounds = [start] + cuts + [end]
            let labels = (0..<bounds.count - 1).map { i in
                attribute(stream.slice(bounds[i], bounds[i + 1]), bounds[i], bounds[i + 1], on: stream, models: models)
            }
            hadPartial = hadPartial || stream.hadDraft(id)
            stream.forget(id)
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
        let cueEvent = direction.map {
            SpatialCueEvent(
                sessionId: sessionId, utteranceId: id, origin: stream.origin, startMs: startMs, endMs: endMs,
                position: $0.position, confidence: $0.fusion.confidence,
                azimuthDeg: $0.azimuthDeg, speaker: speaker)
        }
        emitFinal(id: id, origin: stream.origin, startMs: startMs, endMs: endMs, text: text,
                  speaker: speaker, captionSpatial: direction?.caption, cueEvent: cueEvent)
    }

    /// Emits `caption/final` followed by `spatial/cue` when present.
    private func emitFinal(id: Int, origin: AudioOrigin, startMs: Int, endMs: Int, text: String,
                           speaker: SpeakerLabel, captionSpatial: CaptionSpatialCue?,
                           cueEvent: SpatialCueEvent?) {
        send(Methods.Meeting.nCaptionFinal, CaptionFinalEvent(
            sessionId: sessionId, utteranceId: id, origin: origin, startMs: startMs,
            endMs: endMs, text: text, language: languageCode ?? ASRLanguage.guess(fromText: text),
            speaker: speaker, spatial: captionSpatial))
        // An empty final only retracts a partial: no utterance on screen to place.
        guard let cueEvent, !text.isEmpty else { return }
        send(Methods.Meeting.nSpatialCue, cueEvent)
    }

    /// Seconds elapsed since `start` on the monotonic clock.
    private static func seconds(since start: DispatchTime) -> Double {
        Double(DispatchTime.now().uptimeNanoseconds - start.uptimeNanoseconds) / 1e9
    }

    /// Transcribes a partial, forcing most of the previous partial of the same utterance.
    /// - Parameters:
    ///   - id: Utterance.
    ///   - audio: 16 kHz audio of the utterance so far.
    ///   - stream: Holds the previous hypothesis.
    ///   - models: Loaded models; without a prefix decoder this re-transcribes from scratch.
    /// - Returns: Caption text and how many of its leading Unicode scalars are stable.
    private func transcribePartial(id: Int, _ audio: [Float], on stream: CaptionStream,
                                   _ models: LiveModels) -> (String, Int) {
        guard let prefix = models.prefix else { return (transcribe(audio, models), 0) }
        guard audio.count >= CaptionStream.rate / 5 else { return ("", 0) }
        let seconds = Double(audio.count) / Double(CaptionStream.rate)
        let h = prefix.decode(audio: audio, language: languageName, previous: stream.hypothesis(id),
                              maxTokens: ASRLanguage.maxTokens(forSeconds: seconds))
        stream.setHypothesis(id, h.tokens)
        return (h.text, h.stableChars)
    }

    /// Runs Qwen3-ASR on 16 kHz audio.
    private func transcribe(_ audio: [Float], _ models: LiveModels) -> String {
        guard let asr = models.asr, audio.count >= CaptionStream.rate / 5 else { return "" }
        let seconds = Double(audio.count) / Double(CaptionStream.rate)
        return asr.transcribe(audio: audio, sampleRate: CaptionStream.rate, language: languageName,
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
