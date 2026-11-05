import Foundation
import Testing
@testable import MeetingCore

/// 16 kHz samples for `s` seconds.
private func sec(_ s: Double) -> Int { Int(s * 16_000) }

private func turn(_ a: Double, _ b: Double, _ slot: Int) -> SortformerTurn {
    SortformerTurn(start: sec(a), end: sec(b), slot: slot)
}

private func basis(_ i: Int, dims: Int = 8) -> [Float] {
    var v = [Float](repeating: 0, count: dims)
    v[i] = 1
    return v
}

/// A vector with cosine `c` to basis(0), leaning towards basis(j).
private func near0(_ c: Float, towards j: Int) -> [Float] {
    normalized(zip(basis(0), basis(j)).map { c * $0 + (1 - c * c).squareRoot() * $1 })
}

// MARK: Timeline

@Test func timelineKeepsSlotIdsStableWhenSnapshotIdsAreCompacted() {
    var timeline = SortformerTimeline()
    // Raw slots 0 and 2 were active; the library compacts them to 0 and 1.
    timeline.update(snapshot: [turn(0, 2, 0), turn(2.5, 4, 1)], confirmedUntil: sec(4))
    #expect(timeline.dominantSlot(start: sec(2.5), end: sec(4)) == 1)
    // Raw slot 1 appears: the old "1" is now reported as 2 and the newcomer as 1.
    timeline.update(snapshot: [turn(0, 2, 0), turn(2.5, 4, 2), turn(4.5, 6, 1)], confirmedUntil: sec(6))
    #expect(timeline.dominantSlot(start: sec(2.5), end: sec(4)) == 1, "a settled turn keeps its slot")
    #expect(timeline.dominantSlot(start: sec(4.5), end: sec(6)) == 2, "the newcomer gets a fresh slot")
}

@Test func dominantSlotNeedsEnoughConfirmedSpeechAndAClearMajority() {
    var timeline = SortformerTimeline()
    timeline.update(snapshot: [turn(0, 3, 0), turn(3, 3.4, 1), turn(5, 6, 0), turn(6, 7, 1)],
                    confirmedUntil: sec(7))
    #expect(timeline.dominantSlot(start: 0, end: sec(3.4)) == 0)
    #expect(timeline.dominantSlot(start: sec(5), end: sec(7)) == nil, "a 50/50 split has no dominant slot")
    #expect(timeline.dominantSlot(start: sec(3), end: sec(3.3)) == nil, "too little speech to judge")
    #expect(timeline.dominantSlot(start: sec(8), end: sec(10)) == nil, "not confirmed yet")
}

@Test func splitPointsFindSpeakerChangesInsideAnUtterance() {
    var timeline = SortformerTimeline()
    timeline.update(snapshot: [turn(0.2, 3.1, 0), turn(3.2, 6, 1), turn(6.1, 6.4, 0)], confirmedUntil: sec(7))
    // One change long enough on both sides; the trailing 0.3 s blip is ignored.
    #expect(timeline.splitPoints(start: 0, end: sec(6.5)) == [sec(3.2)])
    // Too short a first part: no split.
    #expect(timeline.splitPoints(start: sec(2.6), end: sec(6)) == [])
    // A single speaker: no split.
    #expect(timeline.splitPoints(start: sec(3.2), end: sec(6)) == [])
}

@Test func timelineForgetsTurnsBeforeTheTrimPoint() {
    var timeline = SortformerTimeline()
    timeline.update(snapshot: [turn(0, 2, 0), turn(3, 5, 1)], confirmedUntil: sec(5))
    timeline.trim(before: sec(2.5))
    #expect(timeline.dominantSlot(start: 0, end: sec(2)) == nil)
    #expect(timeline.dominantSlot(start: sec(3), end: sec(5)) == 1)
    // Settled turns that were trimmed still keep their slot numbering on the next snapshot.
    timeline.update(snapshot: [turn(0, 2, 0), turn(3, 5, 1), turn(5.5, 7, 0)], confirmedUntil: sec(7))
    #expect(timeline.dominantSlot(start: sec(5.5), end: sec(7)) == 0)
}

// MARK: Slot mapper

@Test func slotMapperFollowsTheMajorityVoteAndMerges() {
    var mapper = SortformerSlotMapper()
    #expect(mapper.cluster(for: 0, origin: .microphone) == nil)
    mapper.vote(slot: 0, origin: .microphone, cluster: 3)
    mapper.vote(slot: 0, origin: .microphone, cluster: 3)
    mapper.vote(slot: 0, origin: .microphone, cluster: 5)
    #expect(mapper.cluster(for: 0, origin: .microphone) == 3)
    #expect(mapper.cluster(for: 0, origin: .system) == nil, "origins never share slots")
    mapper.merge(cluster: 3, into: 5)
    #expect(mapper.cluster(for: 0, origin: .microphone) == 5)
    #expect(mapper.agreement(slot: 0, origin: .microphone, cluster: 5) == 1)
    mapper.vote(slot: 1, origin: .microphone, cluster: 7)
    #expect(mapper.agreement(slot: 0, origin: .microphone, cluster: 7) < 0, "cluster 7 belongs to another slot")
    #expect(mapper.agreement(slot: 2, origin: .microphone, cluster: 9) == 0, "no evidence either way")
}

// MARK: Fusion

@Test func sameSortformerSlotPullsAnAmbiguousUtteranceIntoItsCluster() {
    let ambiguous = near0(0.45, towards: 3) // below the 0.55 cluster threshold on timbre alone
    let plain = OnlineSpeakerTracker(bank: VoiceprintBank())
    _ = plain.assign(.init(embedding: basis(0), origin: .microphone))
    #expect(plain.assign(.init(embedding: ambiguous, origin: .microphone)) == .cluster(index: 1))

    let withTurns = OnlineSpeakerTracker(bank: VoiceprintBank())
    _ = withTurns.assign(.init(embedding: basis(0), origin: .microphone, turnSlot: 0))
    #expect(withTurns.assign(.init(embedding: ambiguous, origin: .microphone, turnSlot: 0)) == .cluster(index: 0))
}

@Test func differentSortformerSlotsSeparateSimilarVoices() {
    let similar = near0(0.6, towards: 3) // would join cluster 0 on timbre alone
    let tracker = OnlineSpeakerTracker(bank: VoiceprintBank())
    _ = tracker.assign(.init(embedding: basis(0), origin: .microphone, turnSlot: 0))
    #expect(tracker.assign(.init(embedding: similar, origin: .microphone, turnSlot: 1)) == .cluster(index: 1))
    // The new slot is now mapped, so its next utterance lands there even with similar timbre.
    #expect(tracker.assign(.init(embedding: near0(0.62, towards: 4), origin: .microphone, turnSlot: 1))
        == .cluster(index: 1))
}

@Test func strongTimbreMatchOverridesTheSortformerSlot() {
    let tracker = OnlineSpeakerTracker(bank: VoiceprintBank())
    _ = tracker.assign(.init(embedding: basis(0), origin: .microphone, turnSlot: 0))
    _ = tracker.assign(.init(embedding: basis(1), origin: .microphone, turnSlot: 1))
    // Sortformer says slot 0, but the voice is clearly cluster 1's.
    #expect(tracker.assign(.init(embedding: normalized([0.1, 0.95, 0, 0, 0, 0, 0, 0]), origin: .microphone,
                                 turnSlot: 0)) == .cluster(index: 1))
}

@Test func voiceprintMatchOverridesTheSortformerSlot() {
    var bank = VoiceprintBank()
    bank.add(personId: "alice", vector: basis(4))
    let tracker = OnlineSpeakerTracker(bank: bank)
    _ = tracker.assign(.init(embedding: basis(0), origin: .microphone, turnSlot: 0))
    let label = tracker.assign(.init(embedding: basis(4), origin: .microphone, turnSlot: 0))
    #expect(label == .known(personId: "alice", score: 1))
}

// MARK: Default microphone

@Test func defaultMicrophoneResolvesToTheSystemInput() throws {
    let builtIn = AudioInputInfo(uid: "BuiltIn", name: "MacBook Pro Microphone", channels: 1, sampleRate: 48_000,
                                 isVirtual: false)
    let plan = try CapturePlan.make(preset: CapturePreset(microphoneUID: CapturePreset.defaultMicrophoneUID),
                                    inputs: [builtIn], defaultInputUID: "BuiltIn")
    #expect(plan.microphone == builtIn)
    #expect(plan.files.microphone?.lastPathComponent == "mic.caf")
    #expect(throws: CapturePlanError.noDefaultMicrophone) {
        try CapturePlan.make(preset: CapturePreset(microphoneUID: "default"), inputs: [builtIn], defaultInputUID: nil)
    }
}

// MARK: Split pieces

@Test func piecesThatFusionGivesTheSameSpeakerAreJoinedAgain() {
    let a = SpeakerLabel.cluster(index: 0), b = SpeakerLabel.cluster(index: 1)
    // Sortformer cut twice, but fusion says the last two pieces are the same person.
    let pieces = SortformerTimeline.joinPieces(bounds: [0, 100, 200, 300], labels: [a, b, b])
    #expect(pieces.map(\.start) == [0, 100])
    #expect(pieces.map(\.end) == [100, 300])
    #expect(pieces.map(\.label) == [a, b])
    // Unknown speakers are not evidence of sameness, so Sortformer's cut stands.
    #expect(SortformerTimeline.joinPieces(bounds: [0, 100, 200], labels: [.unknown, .unknown]).count == 2)
}
