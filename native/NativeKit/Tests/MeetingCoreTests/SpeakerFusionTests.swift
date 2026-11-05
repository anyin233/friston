import Testing
@testable import MeetingCore

private func basis(_ i: Int, dims: Int = 8) -> [Float] {
    var v = [Float](repeating: 0, count: dims)
    v[i] = 1
    return v
}

/// A vector with cosine `c` to basis(0), leaning towards basis(j).
private func near0(_ c: Float, towards j: Int) -> [Float] {
    normalized(zip(basis(0), basis(j)).map { c * $0 + (1 - c * c).squareRoot() * $1 })
}

@Test func distinctVoicesGetStableDistinctClusters() {
    let tracker = OnlineSpeakerTracker(bank: VoiceprintBank())
    let a1 = tracker.assign(.init(embedding: basis(0), origin: .microphone))
    let b1 = tracker.assign(.init(embedding: basis(1), origin: .microphone))
    let a2 = tracker.assign(.init(embedding: near0(0.9, towards: 2), origin: .microphone))
    #expect(a1 == .cluster(index: 0))
    #expect(b1 == .cluster(index: 1))
    #expect(a2 == a1)
}

@Test func directionSeparatesSimilarVoicesSittingApart() {
    let similar = near0(0.6, towards: 3) // ambiguous timbre: cosine 0.6 ≥ cluster threshold 0.55
    let withoutSpace = OnlineSpeakerTracker(bank: VoiceprintBank())
    _ = withoutSpace.assign(.init(embedding: basis(0), origin: .microphone))
    #expect(withoutSpace.assign(.init(embedding: similar, origin: .microphone)) == .cluster(index: 0))

    let withSpace = OnlineSpeakerTracker(bank: VoiceprintBank())
    _ = withSpace.assign(.init(embedding: basis(0), origin: .microphone, azimuthDeg: -60, spatialConfidence: 0.9))
    let other = withSpace.assign(.init(embedding: similar, origin: .microphone, azimuthDeg: 60, spatialConfidence: 0.9))
    #expect(other == .cluster(index: 1))
}

@Test func enrolledVoicesAreNamedAndOriginIsAHardPartition() {
    var bank = VoiceprintBank()
    bank.add(personId: "alice", vector: basis(4))
    let tracker = OnlineSpeakerTracker(bank: bank)
    #expect(tracker.assign(.init(embedding: near0(0.1, towards: 4), origin: .microphone)) ==
        .known(personId: "alice", score: cosine(basis(4), near0(0.1, towards: 4))))

    _ = tracker.assign(.init(embedding: basis(1), origin: .microphone))
    let remote = tracker.assign(.init(embedding: basis(1), origin: .system))
    #expect(remote != .cluster(index: 1), "identical timbre from system audio must not join a room cluster")
}
