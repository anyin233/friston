import Foundation
import Testing
@testable import MeetingCore

// MARK: Resampling

/// A sine wave sampled at `rate`.
private func sine(_ hz: Double, rate: Double, seconds: Double) -> [Float] {
    (0..<Int(rate * seconds)).map { Float(sin(2 * Double.pi * hz * Double($0) / rate)) }
}

/// Zero crossings per second, a cheap frequency estimate.
private func crossingsPerSecond(_ x: [Float], rate: Double) -> Double {
    var n = 0
    for i in 1..<x.count where (x[i - 1] < 0) != (x[i] < 0) { n += 1 }
    return Double(n) / (Double(x.count) / rate) / 2
}

@Test func resamplerKeepsLengthAndPitchWhenDownsampling() {
    var r = StreamingResampler(inputRate: 48_000, outputRate: 16_000)
    let out = r.process(sine(440, rate: 48_000, seconds: 1))
    #expect(abs(out.count - 16_000) <= 2)
    #expect(abs(crossingsPerSecond(out, rate: 16_000) - 440) < 5)
}

@Test func resamplerRemovesContentAboveTheNewNyquist() {
    var r = StreamingResampler(inputRate: 48_000, outputRate: 16_000)
    // 12 kHz cannot be represented at 16 kHz; without a low-pass it would alias to 4 kHz at full level.
    let out = r.process(sine(12_000, rate: 48_000, seconds: 0.5))
    let rms = (out.dropFirst(200).reduce(0) { $0 + $1 * $1 } / Float(out.count - 200)).squareRoot()
    #expect(rms < 0.05)
}

@Test func resamplerChunkedMatchesOneShot() {
    let x = sine(300, rate: 44_100, seconds: 0.3)
    var whole = StreamingResampler(inputRate: 44_100, outputRate: 16_000)
    let a = whole.process(x)
    var chunked = StreamingResampler(inputRate: 44_100, outputRate: 16_000)
    var b: [Float] = []
    var i = 0
    for size in [1, 7, 512, 33, 4096, 100_000] where i < x.count {
        let end = min(x.count, i + size)
        b += chunked.process(Array(x[i..<end]))
        i = end
    }
    #expect(a.count == b.count)
    #expect(zip(a, b).allSatisfy { abs($0 - $1) < 1e-5 })
}

@Test func resamplerPassesThroughAtEqualRates() {
    var r = StreamingResampler(inputRate: 16_000, outputRate: 16_000)
    let x: [Float] = [0.1, -0.2, 0.3]
    #expect(r.process(x) == x)
}

@Test func downmixAveragesChannels() {
    #expect(StreamingResampler.downmix([[1, 0], [0, 1]]) == [0.5, 0.5])
    #expect(StreamingResampler.downmix(interleaved: [1, 0, 0, 1], channelCount: 2) == [0.5, 0.5])
}

// MARK: Utterance segmentation

private let sr = 16_000

@Test func utteranceGetsPartialsThenOneFinalWithTheSameId() {
    let ids = UtteranceIDAllocator()
    var seg = UtteranceSegmenter(config: .init(sampleRate: sr, partialInterval: 1, firstPartialDelay: 1, maxDuration: 20), ids: ids)
    #expect(seg.speechStarted(atSample: 8_000).isEmpty)
    #expect(seg.advance(to: 16_000).isEmpty) // only 0.5 s of speech so far
    #expect(seg.advance(to: 24_000) == [.partial(id: 1, start: 8_000, end: 24_000)])
    #expect(seg.advance(to: 30_000).isEmpty) // < 1 s since the last partial
    #expect(seg.advance(to: 40_000) == [.partial(id: 1, start: 8_000, end: 40_000)])
    #expect(seg.speechEnded(endSample: 44_000) == [.final(id: 1, start: 8_000, end: 44_000, hadPartial: true)])
    #expect(seg.activeStart == nil)
    #expect(seg.advance(to: 80_000).isEmpty)
}

@Test func firstPartialComesEarlierThanTheRefreshInterval() {
    // Default: the first partial after 0.5 s of speech, later ones every 1 s, so a caption
    // appears about half a second sooner without transcribing more often.
    let ids = UtteranceIDAllocator()
    var seg = UtteranceSegmenter(config: .init(sampleRate: sr), ids: ids)
    #expect(seg.speechStarted(atSample: 0).isEmpty)
    #expect(seg.advance(to: 7_000).isEmpty)
    #expect(seg.advance(to: 8_000) == [.partial(id: 1, start: 0, end: 8_000)])
    #expect(seg.advance(to: 20_000).isEmpty) // 0.75 s since the first partial
    #expect(seg.advance(to: 24_000) == [.partial(id: 1, start: 0, end: 24_000)])
}

@Test func longSpeechIsForceSplitIntoNewUtterances() {
    let ids = UtteranceIDAllocator()
    var seg = UtteranceSegmenter(config: .init(sampleRate: sr, partialInterval: 100, maxDuration: 2), ids: ids)
    _ = seg.speechStarted(atSample: 0)
    #expect(seg.advance(to: 32_000) == [.final(id: 1, start: 0, end: 32_000, hadPartial: false)])
    #expect(seg.activeStart == 32_000)
    #expect(seg.speechEnded(endSample: 40_000) == [.final(id: 2, start: 32_000, end: 40_000, hadPartial: false)])
}

@Test func blipsWithoutPartialsAreDropped() {
    let ids = UtteranceIDAllocator()
    var seg = UtteranceSegmenter(config: .init(sampleRate: sr, minFinalDuration: 0.3), ids: ids)
    _ = seg.speechStarted(atSample: 0)
    #expect(seg.speechEnded(endSample: 3_200).isEmpty) // 0.2 s
    #expect(seg.speechEnded(endSample: 9_000).isEmpty) // not active
}

@Test func utteranceIdsAreSharedAcrossStreams() {
    let ids = UtteranceIDAllocator()
    var mic = UtteranceSegmenter(config: .init(sampleRate: sr), ids: ids)
    var system = UtteranceSegmenter(config: .init(sampleRate: sr), ids: ids)
    _ = mic.speechStarted(atSample: 0)
    _ = system.speechStarted(atSample: 0)
    #expect(mic.speechEnded(endSample: 16_000) == [.final(id: 1, start: 0, end: 16_000, hadPartial: false)])
    #expect(system.speechEnded(endSample: 16_000) == [.final(id: 2, start: 0, end: 16_000, hadPartial: false)])
}

// MARK: Notification encoding

/// Encodes a value and parses it back as a plain JSON object.
private func json<T: Encodable>(_ value: T) throws -> [String: Any] {
    let data = try JSONEncoder().encode(value)
    return try #require(JSONSerialization.jsonObject(with: data) as? [String: Any])
}

@Test func speakerLabelsUseTheRustTaggedShape() throws {
    let known = try json(SpeakerLabel.known(personId: "p-1", score: 0.75))
    #expect(known["kind"] as? String == "known")
    #expect(known["person_id"] as? String == "p-1")
    #expect((known["score"] as? Double).map { abs($0 - 0.75) < 1e-6 } == true)
    let cluster = try json(SpeakerLabel.cluster(index: 3))
    #expect(cluster["kind"] as? String == "cluster" && cluster["index"] as? Int == 3)
    #expect(try json(SpeakerLabel.unknown) as NSDictionary == ["kind": "unknown"])
    for label in [SpeakerLabel.known(personId: "x", score: 0.5), .cluster(index: 0), .unknown] {
        #expect(try JSONDecoder().decode(SpeakerLabel.self, from: JSONEncoder().encode(label)) == label)
    }
}

@Test func captionEventsUseCamelCaseWithSnakeCaseSpatial() throws {
    let partial = try json(CaptionPartialEvent(sessionId: "s", utteranceId: 4, origin: .system, startMs: 1200,
                                               text: "大家好", speaker: .unknown))
    #expect(Set(partial.keys) == ["sessionId", "utteranceId", "origin", "startMs", "text", "speaker"])
    #expect(partial["origin"] as? String == "system")

    let final = try json(CaptionFinalEvent(
        sessionId: "s", utteranceId: 4, origin: .microphone, startMs: 1200, endMs: 2500, text: "大家好",
        language: "zh", speaker: .cluster(index: 0),
        spatial: CaptionSpatialCue(azimuthDeg: nil, tdoaSamples: -3.5, confidence: 0.8)))
    #expect(final["endMs"] as? Int == 2500 && final["language"] as? String == "zh")
    let spatial = try #require(final["spatial"] as? [String: Any])
    #expect(Set(spatial.keys) == ["tdoa_samples", "confidence"]) // nil azimuth is omitted
    #expect(spatial["tdoa_samples"] as? Double == -3.5)

    let relabel = try json(SpeakerRelabelEvent(sessionId: "s", from: .cluster(index: 1),
                                               to: .known(personId: "p", score: 0.7)))
    #expect((relabel["to"] as? [String: Any])?["person_id"] as? String == "p")
}

@Test func spatialCueEventUsesCamelCaseAndOmitsUnknownAzimuth() throws {
    // Same JSON keys as `spatial_cue_decodes_the_engine_shape` in crates/mm-core/src/live.rs.
    let event = try json(SpatialCueEvent(
        sessionId: "s", utteranceId: 7, origin: .microphone, startMs: 1200, endMs: 2500,
        position: [-0.8, -0.75, 0.1], confidence: 0.6, azimuthDeg: nil, speaker: .cluster(index: 1)))
    #expect(Set(event.keys) == ["sessionId", "utteranceId", "origin", "startMs", "endMs", "position",
                                "confidence", "speaker"])
    #expect((event["position"] as? [Double])?.count == 3)
    let withAzimuth = try json(SpatialCueEvent(
        sessionId: "s", utteranceId: 7, origin: .microphone, startMs: 0, endMs: 1, position: [0.5],
        confidence: 1, azimuthDeg: 30, speaker: .unknown))
    #expect(withAzimuth["azimuthDeg"] as? Double == 30)
}

@Test func millisecondsFromSamples() {
    #expect(CaptionClock(sampleRate: 16_000, offsetMs: 250).ms(atSample: 16_000) == 1250)
}

// MARK: Voiceprints

@Test func presetDecodesVoiceprintsAndBankIgnoresOtherModels() throws {
    let preset = try JSONDecoder().decode(CapturePreset.self, from: Data(#"""
    {"microphoneUID":"m","voiceprints":[
      {"personId":"a","model":"wespeaker","vector":[1,0]},
      {"personId":"b","model":"campplus","vector":[0,1]},
      {"personId":"c","model":"wespeaker","vector":[0,2]}]}
    """#.utf8))
    let samples = try #require(preset.voiceprints)
    let bank = VoiceprintBank(samples: samples, model: "wespeaker")
    #expect(bank.personCount == 2)
    let match = try #require(bank.bestMatch([0, 1]))
    #expect(match.personId == "c" && abs(match.score - 1) < 1e-6) // vectors are normalised on load
    // Old payloads without the key still decode.
    #expect(try JSONDecoder().decode(CapturePreset.self, from: Data("{}".utf8)).voiceprints == nil)
}

@Test func snrComparesSpeechWithTheRest() throws {
    var x = [Float](repeating: 0.01, count: 2_000)
    for i in 1_000..<2_000 { x[i] = i.isMultiple(of: 2) ? 1 : -1 }
    let snr = try #require(VoiceprintAnalysis.snrDb(samples: x, speech: [1_000..<2_000]))
    #expect(abs(snr - 40) < 0.5)
    #expect(VoiceprintAnalysis.snrDb(samples: x, speech: [0..<2_000]) == nil) // no noise reference
}

@Test func singleSpeakerCheckLooksForOutlierWindows() {
    let a: [Float] = [1, 0, 0], a2: [Float] = normalized([0.95, 0.3, 0]), b: [Float] = [0, 0, 1]
    #expect(VoiceprintAnalysis.isSingleSpeaker(windowEmbeddings: [a, a2, a]))
    #expect(!VoiceprintAnalysis.isSingleSpeaker(windowEmbeddings: [a, a, b]))
    #expect(VoiceprintAnalysis.isSingleSpeaker(windowEmbeddings: [a])) // too little evidence to object
}

@Test func clipRangeClampsAndValidates() throws {
    #expect(try VoiceprintAnalysis.sampleRange(totalSamples: 160_000, sampleRate: 16_000, startMs: 1_000, endMs: 3_000)
            == 16_000..<48_000)
    #expect(try VoiceprintAnalysis.sampleRange(totalSamples: 16_000, sampleRate: 16_000, startMs: nil, endMs: 99_000)
            == 0..<16_000)
    #expect(throws: VoiceprintAnalysisError.self) {
        try VoiceprintAnalysis.sampleRange(totalSamples: 16_000, sampleRate: 16_000, startMs: 2_000, endMs: 1_000)
    }
}

// MARK: Languages

@Test func captionLanguageMapsToQwenNames() {
    #expect(ASRLanguage.qwenName(for: "zh") == "Chinese")
    #expect(ASRLanguage.qwenName(for: "zh-Hans-CN") == "Chinese")
    #expect(ASRLanguage.qwenName(for: "EN-us") == "English")
    #expect(ASRLanguage.qwenName(for: "yue") == "Cantonese")
    #expect(ASRLanguage.qwenName(for: "auto") == nil)
    #expect(ASRLanguage.qwenName(for: nil) == nil)
    #expect(ASRLanguage.qwenName(for: "tlh") == nil) // unsupported: let the model detect
}

@Test func languageGuessFromScript() {
    #expect(ASRLanguage.guess(fromText: "大家好，我们开始开会") == "zh")
    #expect(ASRLanguage.guess(fromText: "こんにちは、会議を始めます") == "ja")
    #expect(ASRLanguage.guess(fromText: "안녕하세요") == "ko")
    #expect(ASRLanguage.guess(fromText: "Let's start the meeting") == "en")
    #expect(ASRLanguage.guess(fromText: "123 …") == nil)
}

// MARK: Offline turns

@Test func turnsMergeSameSpeakerAcrossShortGapsAndSplitWhenLong() {
    let turns = TurnPlanner.windows(from: [
        .init(start: 0, end: 2, speaker: 0), .init(start: 2.3, end: 4, speaker: 0), // merged (gap 0.3)
        .init(start: 4.1, end: 5, speaker: 1),                                      // speaker change
        .init(start: 7, end: 8, speaker: 1),                                        // gap 2 s
        .init(start: 10, end: 40, speaker: 2),                                      // too long: split
    ], maxGap: 0.6, maxDuration: 12)
    #expect(turns.map(\.speaker) == [0, 1, 1, 2, 2, 2])
    #expect(turns[0].start == 0 && turns[0].end == 4)
    #expect(turns.suffix(3).allSatisfy { $0.end - $0.start <= 12 })
    #expect(turns.last?.end == 40)
}

/// Regression: the Silero pre-filter cut refine coverage of far-field meeting speech to ~67%
/// (AISHELL-4, docs/asr-model-survey.md §7.6), doubling CER. Pyannote alone covers ~97%.
@Test func refineSegmentationDoesNotUseSileroPrefilter() {
    #expect(TurnPlanner.sileroPrefilter == false)
}

// MARK: Fusion relabels

@Test func anonymousClusterIsRenamedOnceItMatchesAVoiceprint() {
    var bank = VoiceprintBank()
    bank.add(personId: "alice", vector: [1, 0, 0])
    let tracker = OnlineSpeakerTracker(bank: bank)
    // Each utterance alone is too far from Alice (cosine 0.6 < 0.62), so an anonymous cluster starts…
    let first = tracker.assignReportingRelabels(.init(embedding: [0.6, 0.8, 0], origin: .microphone))
    #expect(first.label == .cluster(index: 0) && first.relabels.isEmpty)
    // …but the averaged centroid of two utterances (noise cancels out) reaches 0.65.
    let second = tracker.assignReportingRelabels(.init(embedding: normalized([0.6, 0.4, 0.693]), origin: .microphone))
    #expect(second.relabels.count == 1)
    #expect(second.relabels.first?.from == .cluster(index: 0))
    if case .known(let person, _)? = second.relabels.first?.to { #expect(person == "alice") } else { Issue.record("not known") }
    if case .known(let person, _) = second.label { #expect(person == "alice") } else { Issue.record("label not known") }
}

@Test func clustersThatConvergeAreMerged() {
    var config = SpeakerFusionConfig()
    config.clusterThreshold = 0.8
    config.mergeThreshold = 0.8
    let tracker = OnlineSpeakerTracker(bank: VoiceprintBank(), config: config)
    _ = tracker.assignReportingRelabels(.init(embedding: [1, 0], origin: .microphone))
    let b = tracker.assignReportingRelabels(.init(embedding: normalized([0.7, 0.714]), origin: .microphone)) // cos 0.7
    #expect(b.label == .cluster(index: 1))
    // Halfway between: joins one cluster and drags its centroid next to the other.
    let c = tracker.assignReportingRelabels(.init(embedding: normalized([0.92, 0.39]), origin: .microphone))
    #expect(c.relabels == [SpeakerRelabel(from: .cluster(index: 1), to: .cluster(index: 0))])
    #expect(c.label == .cluster(index: 0))
}

@Test func peekDoesNotCreateClusters() {
    let tracker = OnlineSpeakerTracker(bank: VoiceprintBank())
    #expect(tracker.peek(.init(embedding: [1, 0], origin: .microphone)) == .unknown)
    _ = tracker.assign(.init(embedding: [1, 0], origin: .microphone))
    #expect(tracker.peek(.init(embedding: [0.99, 0.1], origin: .microphone)) == .cluster(index: 0))
    #expect(tracker.peek(.init(embedding: [1, 0], origin: .system)) == .unknown)
    #expect(tracker.assign(.init(embedding: [0, 1], origin: .microphone)) == .cluster(index: 1))
}

// MARK: Model location

@Test func hubSnapshotFollowsEnvironmentAndRefs() {
    let files: Set<String> = [
        "/hf/hub/models--mlx-community--Qwen3-ASR-1.7B-8bit/refs/main",
        "/hf/hub/models--mlx-community--Qwen3-ASR-1.7B-8bit/snapshots/abc/model.safetensors",
    ]
    let snapshot = HubCacheLocator.snapshot(
        repo: "mlx-community/Qwen3-ASR-1.7B-8bit", environment: ["HF_HOME": "/hf"], home: "/Users/me",
        readFile: { $0.hasSuffix("refs/main") ? "abc\n" : nil },
        fileExists: { files.contains($0) })
    #expect(snapshot?.path == "/hf/hub/models--mlx-community--Qwen3-ASR-1.7B-8bit/snapshots/abc")
    #expect(HubCacheLocator.cacheRoots(environment: [:], home: "/Users/me") == ["/Users/me/.cache/huggingface/hub"])
    #expect(HubCacheLocator.cacheRoots(environment: ["HF_HUB_CACHE": "/x", "HF_HOME": "/y"], home: "/h")
            == ["/x", "/y/hub", "/h/.cache/huggingface/hub"])
    #expect(HubCacheLocator.snapshot(repo: "a/b", environment: [:], home: "/h", readFile: { _ in nil },
                                     fileExists: { _ in false }) == nil)
}

@Test func tokenBudgetScalesWithDurationToStopDecoderLoops() {
    #expect(ASRLanguage.maxTokens(forSeconds: 0.5) == 24)
    #expect(ASRLanguage.maxTokens(forSeconds: 6) == 106)
    #expect(ASRLanguage.maxTokens(forSeconds: 60) == 448) // model cap
}
