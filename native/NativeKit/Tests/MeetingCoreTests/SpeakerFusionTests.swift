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
    _ = withSpace.assign(.init(embedding: basis(0), origin: .microphone,
                               spatial: SpatialCue(position: [-0.7, 0], confidence: 0.9)))
    let other = withSpace.assign(.init(embedding: similar, origin: .microphone,
                                       spatial: SpatialCue(position: [0.7, 0], confidence: 0.9)))
    #expect(other == .cluster(index: 1))
}

@Test func azimuthWithKnownSpacingMapsOntoTheSameScale() {
    // −60° and +60° from a calibrated array must be as far apart as relative cues ±0.67.
    let tracker = OnlineSpeakerTracker(bank: VoiceprintBank())
    let similar = near0(0.6, towards: 3)
    _ = tracker.assign(.init(embedding: basis(0), origin: .microphone, spatial: .init(azimuthDeg: -60, confidence: 0.9)))
    #expect(tracker.assign(.init(embedding: similar, origin: .microphone,
                                 spatial: .init(azimuthDeg: 60, confidence: 0.9))) == .cluster(index: 1))
}

@Test func spatialCuesOfDifferentShapeAreIgnored() {
    let tracker = OnlineSpeakerTracker(bank: VoiceprintBank())
    let similar = near0(0.6, towards: 3)
    _ = tracker.assign(.init(embedding: basis(0), origin: .microphone, spatial: .init(position: [-0.7], confidence: 0.9)))
    let other = tracker.assign(.init(embedding: similar, origin: .microphone,
                                     spatial: .init(position: [0.7, 0], confidence: 0.9)))
    #expect(other == .cluster(index: 0), "a 1-D and a 2-D cue are not comparable, so timbre decides")
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

@Test func strayUtteranceClusterIsAbsorbedOnceTheMainSpeakerTalksAgain() {
    // One off-timbre utterance (cosine 0.5 < cluster threshold) opens a cluster of its own;
    // a one-utterance centroid is too noisy to keep apart at the strict merge threshold,
    // so once the main speaker is established, their next utterance folds it back
    // (with a relabel for the UI).
    let tracker = OnlineSpeakerTracker(bank: VoiceprintBank())
    for _ in 0..<3 {
        #expect(tracker.assign(.init(embedding: basis(0), origin: .microphone)) == .cluster(index: 0))
    }
    let stray = tracker.assignReportingRelabels(.init(embedding: near0(0.5, towards: 2), origin: .microphone))
    #expect(stray.label == .cluster(index: 1))
    let back = tracker.assignReportingRelabels(.init(embedding: basis(0), origin: .microphone))
    #expect(back.label == .cluster(index: 0))
    #expect(back.relabels == [SpeakerRelabel(from: .cluster(index: 1), to: .cluster(index: 0))])
}

@Test func establishedDifferentSpeakersStayApart() {
    // Two speakers with several utterances each and a cross cosine of 0.3 must not merge.
    let tracker = OnlineSpeakerTracker(bank: VoiceprintBank())
    let b = near0(0.3, towards: 2)
    for _ in 0..<3 {
        _ = tracker.assign(.init(embedding: basis(0), origin: .microphone))
        _ = tracker.assign(.init(embedding: b, origin: .microphone))
    }
    #expect(tracker.assign(.init(embedding: b, origin: .microphone)) == .cluster(index: 1))
}
