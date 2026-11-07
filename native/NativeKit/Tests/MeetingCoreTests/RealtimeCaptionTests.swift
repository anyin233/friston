import Foundation
import Testing
@testable import MeetingCore

private let sr = 16_000

/// Speech-like noise at `level` for `seconds`, then optional silence; deterministic.
private func tone(_ seconds: Double, level: Float = 0.3) -> [Float] {
    // Carrier with a slow envelope that never reaches zero, so the only dips are the ones a test adds.
    (0..<Int(seconds * Double(sr))).map { level * Float(sin(Double($0) * 0.31) * (0.7 + 0.3 * sin(Double($0) * 0.0017))) }
}

private func silence(_ seconds: Double, level: Float = 0) -> [Float] {
    (0..<Int(seconds * Double(sr))).map { _ in level }
}

// MARK: Partial pacing

@Test func pacerStartsAtTheConfiguredIntervalAndFollowsDecodeTime() {
    var pacer = PartialPacer(initial: 0.5, minInterval: 0.3, maxInterval: 1.5, dutyFactor: 2.5)
    #expect(pacer.interval == 0.5)
    // A fast model refreshes as often as the floor allows.
    pacer.record(decodeSeconds: 0.05)
    #expect(pacer.interval == 0.3)
    // A slow decode stretches the interval so partials never take more than 1/dutyFactor of the GPU.
    for _ in 0..<20 { pacer.record(decodeSeconds: 0.4) }
    #expect(abs(pacer.interval - 1.0) < 0.01)
    for _ in 0..<20 { pacer.record(decodeSeconds: 2) }
    #expect(pacer.interval == 1.5)
}

@Test func pacerIgnoresNonsenseSamples() {
    var pacer = PartialPacer(initial: 0.5)
    pacer.record(decodeSeconds: -1)
    pacer.record(decodeSeconds: .nan)
    #expect(pacer.interval == 0.5)
}

@Test func segmenterUsesTheNewPartialInterval() {
    let ids = UtteranceIDAllocator()
    var seg = UtteranceSegmenter(config: .init(sampleRate: sr, partialInterval: 1, firstPartialDelay: 0.5), ids: ids)
    _ = seg.speechStarted(atSample: 0)
    #expect(seg.advance(to: 8_000) == [.partial(id: 1, start: 0, end: 8_000)])
    seg.setPartialInterval(0.3)
    #expect(seg.advance(to: 12_000).isEmpty) // 0.25 s since the last partial
    #expect(seg.advance(to: 12_800) == [.partial(id: 1, start: 0, end: 12_800)])
}

// MARK: Pause finding

@Test func pauseFinderPrefersTheLongestClosedPause() throws {
    // A sentence gap (0.4 s) followed by a comma-length pause (0.25 s, near-silent).
    let audio = tone(1.5) + silence(0.4) + tone(1) + silence(0.25, level: 0.001) + tone(0.5)
    let pause = try #require(PauseFinder.longestPause(in: audio, sampleRate: sr, minPause: 0.2))
    // First pause: 1.5 s … 1.9 s, give or take one 20 ms frame.
    #expect(abs(pause.lowerBound - Int(1.5 * Double(sr))) <= 320)
    #expect(abs(pause.upperBound - Int(1.9 * Double(sr))) <= 320)
    // Equal pauses: the later one wins, so the final keeps more text.
    let even = tone(1) + silence(0.3) + tone(1) + silence(0.3) + tone(0.5)
    let later = try #require(PauseFinder.longestPause(in: even, sampleRate: sr, minPause: 0.2))
    #expect(abs(later.lowerBound - Int(2.3 * Double(sr))) <= 320)
}

@Test func pauseFinderNeedsSpeechAfterThePause() {
    // Trailing silence is the VAD's business (end of utterance), not a split point.
    #expect(PauseFinder.longestPause(in: tone(2) + silence(0.6), sampleRate: sr, minPause: 0.2) == nil)
    // Breath-length gaps are not pauses.
    #expect(PauseFinder.longestPause(in: tone(1) + silence(0.1) + tone(1), sampleRate: sr, minPause: 0.2) == nil)
}

@Test func quietestPointPrefersTheDip() {
    let audio = tone(1) + tone(0.1, level: 0.02) + tone(0.9)
    let cut = PauseFinder.quietestPoint(in: audio, sampleRate: sr)
    #expect(abs(cut - Int(1.05 * Double(sr))) <= 480)
}

// MARK: Pause-aware splitting

@Test func longSpeechIsSplitAtAPauseAfterSplitAfter() {
    let ids = UtteranceIDAllocator()
    var seg = UtteranceSegmenter(config: .init(sampleRate: sr, partialInterval: 100, firstPartialDelay: 100,
                                               maxDuration: 12, splitAfter: 4), ids: ids)
    // 3 s speech, 0.3 s pause, then speech that goes on.
    let audio = tone(3) + silence(0.3) + tone(5)
    let read: (Range<Int>) -> [Float] = { Array(audio[max(0, $0.lowerBound)..<min(audio.count, $0.upperBound)]) }
    _ = seg.speechStarted(atSample: 0)
    // Before splitAfter nothing happens, even though a pause is there.
    #expect(seg.advance(to: 3 * sr + 8_000, audio: read).isEmpty)
    let actions = seg.advance(to: 4 * sr + 1_000, audio: read)
    guard case .final(1, 0, let cut, false) = actions.first else {
        Issue.record("expected a final for utterance 1, got \(actions)")
        return
    }
    // The cut lands inside the pause (3.0 … 3.3 s), not mid-word.
    #expect(cut > 3 * sr && cut < 3 * sr + 4_800)
    #expect(seg.activeStart == cut)
    #expect(seg.speechEnded(endSample: 6 * sr) == [.final(id: 2, start: cut, end: 6 * sr, hadPartial: false)])
}

@Test func withoutPauseTheHardLimitCutsAtTheQuietestPoint() {
    let ids = UtteranceIDAllocator()
    var seg = UtteranceSegmenter(config: .init(sampleRate: sr, partialInterval: 100, firstPartialDelay: 100,
                                               maxDuration: 4, splitAfter: 2), ids: ids)
    // Continuous speech with one soft dip (too short to be a pause) at 3.2 s.
    let audio = tone(3.2) + tone(0.1, level: 0.02) + tone(2)
    let read: (Range<Int>) -> [Float] = { Array(audio[max(0, $0.lowerBound)..<min(audio.count, $0.upperBound)]) }
    _ = seg.speechStarted(atSample: 0)
    var actions: [UtteranceSegmenter.Action] = []
    var t = 0
    while actions.isEmpty, t < 5 * sr {
        t += 512
        actions = seg.advance(to: t, audio: read)
    }
    guard case .final(1, 0, let cut, false) = actions.first else {
        Issue.record("expected a forced final, got \(actions)")
        return
    }
    #expect(t >= 4 * sr)
    #expect(abs(cut - Int(3.25 * Double(sr))) <= 800)
    #expect(seg.activeStart == cut)
}

// MARK: Decode stats

@Test func decodeStatsSummariseForTheLog() {
    var stats = DecodeStats()
    #expect(stats.summary == "partials 0, finals 0")
    stats.partials = [0.1, 0.2, 0.3, 0.4, 1.0]
    stats.skippedPartials = 2
    stats.finals = [0.5]
    #expect(stats.summary == "partials 5 (decode median 300 ms, p90 1000 ms, 2 skipped), finals 1 (decode median 500 ms, p90 500 ms)")
}

// MARK: Refine model choice

@Test func refineModelResolvesHubIdsToTheirCachedSnapshot() throws {
    let cached = URL(filePath: "/hub/models--mlx-community--Qwen3-ASR-1.7B-8bit/snapshots/abc")
    let source = try ASRModelChoice.resolve("mlx-community/Qwen3-ASR-1.7B-8bit",
                                            hubSnapshot: { $0 == "mlx-community/Qwen3-ASR-1.7B-8bit" ? cached : nil },
                                            isDirectory: { _ in false })
    #expect(source == ASRModelSource(modelId: "mlx-community/Qwen3-ASR-1.7B-8bit", directory: cached))
    // Not cached: speech-swift downloads it by id.
    let remote = try ASRModelChoice.resolve(" aufklarer/Qwen3-ASR-1.7B-MLX-8bit ", hubSnapshot: { _ in nil },
                                            isDirectory: { _ in false })
    #expect(remote == ASRModelSource(modelId: "aufklarer/Qwen3-ASR-1.7B-MLX-8bit", directory: nil))
}

@Test func refineModelAcceptsLocalFoldersNamedLikeTheCheckpoint() throws {
    let snap = "/Users/me/.cache/huggingface/hub/models--mlx-community--Qwen3-ASR-1.7B-8bit/snapshots/abc"
    let fromSnapshot = try ASRModelChoice.resolve(snap, hubSnapshot: { _ in nil }, isDirectory: { _ in true })
    // speech-swift reads size and bits from the id, so a snapshot keeps its repo name.
    #expect(fromSnapshot.modelId == "mlx-community/Qwen3-ASR-1.7B-8bit")
    #expect(fromSnapshot.directory?.path == snap)
    let plain = try ASRModelChoice.resolve("/models/Qwen3-ASR-1.7B-5bit/", hubSnapshot: { _ in nil },
                                           isDirectory: { _ in true })
    #expect(plain.modelId == "Qwen3-ASR-1.7B-5bit")
}

@Test func refineModelRejectsWhatQwen3ASRCannotLoad() {
    let none: (String) -> URL? = { _ in nil }
    for bad in ["", "openai/whisper-large-v3", "mlx-community/Qwen3-ASR-bf16", "Qwen3-ASR-1.7B-8bit",
                "a/b/Qwen3-ASR-1.7B-8bit"] {
        #expect(throws: ASRModelChoiceError.self) {
            try ASRModelChoice.resolve(bad, hubSnapshot: none, isDirectory: { _ in false })
        }
    }
    #expect(throws: ASRModelChoiceError.self) {
        try ASRModelChoice.resolve("/missing/Qwen3-ASR-1.7B-8bit", hubSnapshot: none, isDirectory: { _ in false })
    }
}

@Test func refineContextIsTrimmedAndCapped() {
    #expect(ASRModelChoice.context(nil) == nil)
    #expect(ASRModelChoice.context("  \n ") == nil)
    #expect(ASRModelChoice.context(" 会议：周会 ") == "会议：周会")
    let long = String(repeating: "名", count: 2_000)
    #expect(ASRModelChoice.context(long)?.count == ASRModelChoice.maxContextCharacters)
}
