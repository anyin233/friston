import Foundation

/// Which physical stream speech came from. Mirrors `mm_core::AudioOrigin`.
public enum AudioOrigin: String, Codable, Sendable {
    /// Local microphone: people in the room.
    case microphone
    /// System / app audio: remote participants.
    case system
}

/// Evidence about one short stretch of single-speaker speech.
public struct SpeechObservation: Sendable {
    /// L2-normalised speaker embedding (WeSpeaker / CAM++ / ReDimNet).
    public var embedding: [Float]
    public var origin: AudioOrigin
    /// Where the speech came from, when the input has ≥ 2 real channels.
    /// Usually relative (``StereoCueAnalyzer``), so no mic geometry is needed.
    public var spatial: SpatialCue?
    /// The Sortformer slot that dominates this speech (stable within the stream's session), or
    /// `nil` when Sortformer is unavailable or undecided.
    public var turnSlot: Int?

    /// Creates an observation.
    /// - Parameters:
    ///   - embedding: L2-normalised speaker embedding.
    ///   - origin: Microphone or system audio.
    ///   - spatial: Position cue, or `nil` for mono input.
    ///   - turnSlot: Dominant Sortformer slot, from ``SortformerTimeline/dominantSlot(start:end:)``.
    public init(embedding: [Float], origin: AudioOrigin, spatial: SpatialCue? = nil, turnSlot: Int? = nil) {
        self.embedding = embedding
        self.origin = origin
        self.spatial = spatial
        self.turnSlot = turnSlot
    }
}

/// Voiceprints defined ahead of time (recorded or clipped by the user).
public struct VoiceprintBank: Sendable {
    private var voices: [String: [[Float]]] = [:]

    public init() {}

    /// Builds a bank from wire samples, keeping only vectors produced by `model`.
    ///
    /// Vectors from a different embedding model live in a different space, so comparing them
    /// would produce meaningless scores; they are skipped rather than rejected so an app that
    /// stores several models' vectors can always send everything it has.
    /// - Parameters:
    ///   - samples: Voiceprints from `capture/start` or elsewhere.
    ///   - model: The engine's speaker-embedding model id.
    public init(samples: [VoiceprintSample], model: String) {
        for sample in samples where sample.model == model && !sample.vector.isEmpty {
            add(personId: sample.personId, vector: normalized(sample.vector))
        }
    }

    /// Number of distinct people with at least one sample.
    public var personCount: Int { voices.count }

    /// Adds a voiceprint sample for a person (several samples per person are encouraged).
    /// - Parameters:
    ///   - personId: Stable person identifier.
    ///   - vector: L2-normalised speaker embedding.
    public mutating func add(personId: String, vector: [Float]) {
        voices[personId, default: []].append(vector)
    }

    /// Finds the best-matching enrolled person.
    /// - Parameter embedding: L2-normalised query embedding.
    /// - Returns: The person and their best cosine score, or `nil` if the bank is empty.
    public func bestMatch(_ embedding: [Float]) -> (personId: String, score: Float)? {
        voices
            .map { (id, vectors) in (id, vectors.map { cosine($0, embedding) }.max() ?? -1) }
            .max { $0.1 < $1.1 }
            .map { (personId: $0.0, score: $0.1) }
    }

    /// Scores an embedding against one person's samples.
    /// - Parameters:
    ///   - personId: Enrolled person.
    ///   - embedding: L2-normalised query embedding.
    /// - Returns: The best cosine over that person's samples, or `nil` if the person is unknown.
    public func score(personId: String, _ embedding: [Float]) -> Float? {
        voices[personId].map { $0.map { cosine($0, embedding) }.max() ?? -1 }
    }
}

/// Result of assigning an observation to a speaker. Encodes like `mm_core::SpeakerLabel`:
/// `{"kind":"cluster","index":N}`, `{"kind":"known","person_id":…,"score":…}` or `{"kind":"unknown"}`.
public enum SpeakerLabel: Equatable, Sendable, Codable {
    case known(personId: String, score: Float)
    case cluster(index: Int)
    /// Not enough evidence yet (very short utterance, no embedding model).
    case unknown

    private enum CodingKeys: String, CodingKey {
        case kind, index, score
        case personId = "person_id"
    }

    /// Encodes the tagged wire form.
    /// - Parameter encoder: Target encoder.
    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .known(let personId, let score):
            try c.encode("known", forKey: .kind)
            try c.encode(personId, forKey: .personId)
            try c.encode(score, forKey: .score)
        case .cluster(let index):
            try c.encode("cluster", forKey: .kind)
            try c.encode(index, forKey: .index)
        case .unknown:
            try c.encode("unknown", forKey: .kind)
        }
    }

    /// Decodes the tagged wire form.
    /// - Parameter decoder: Decoder positioned at the label object.
    /// - Throws: `DecodingError` for an unknown `kind`.
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        switch try c.decode(String.self, forKey: .kind) {
        case "known": self = .known(personId: try c.decode(String.self, forKey: .personId),
                                    score: try c.decode(Float.self, forKey: .score))
        case "cluster": self = .cluster(index: try c.decode(Int.self, forKey: .index))
        case "unknown": self = .unknown
        case let other:
            throw DecodingError.dataCorruptedError(forKey: .kind, in: c, debugDescription: "unknown speaker kind \(other)")
        }
    }
}

/// A retroactive change of attribution: every caption labelled `from` should now read `to`.
public struct SpeakerRelabel: Equatable, Sendable {
    public var from: SpeakerLabel
    public var to: SpeakerLabel

    /// Creates a relabel.
    /// - Parameters:
    ///   - from: The label previously sent.
    ///   - to: The label that replaces it.
    public init(from: SpeakerLabel, to: SpeakerLabel) {
        self.from = from
        self.to = to
    }
}

/// Tunables for ``OnlineSpeakerTracker``.
public struct SpeakerFusionConfig: Sendable {
    /// Cosine needed to name an enrolled person.
    public var knownThreshold: Float = 0.62
    /// Fused score needed to join an existing anonymous cluster.
    public var clusterThreshold: Float = 0.55
    /// Fused centroid-to-centroid score at which two clusters are judged to be one person.
    /// Higher than `clusterThreshold`, because a merge rewrites captions already shown.
    public var mergeThreshold: Float = 0.75
    /// Clusters with fewer utterances than this are "small": their centroid comes from one or
    /// two utterances and is too noisy to hold against the strict merge threshold.
    public var smallClusterCount: Int = 3
    /// Merge threshold when either cluster is small, so a stray utterance's cluster folds back
    /// into the speaker it belongs to instead of lingering as "说话人 9". Tuned on AMI ES2004a (4 real speakers, live path): 0.55 → 10 clusters, 0.45 → 7, 0.35 → 6 with 4.6 % one-to-one confusion and no real speakers merged.
    public var smallMergeThreshold: Float = 0.35
    /// Utterances an anonymous cluster needs before its centroid may be matched to a voiceprint.
    public var minCountForRename: Int = 2
    /// Maximum influence of direction on the fused score (± this value).
    public var spatialWeight: Float = 0.25
    /// Distance in ``SpatialCue`` units at which spatial similarity has fallen to 1/e.
    /// Cues span about −1…1, so 0.33 is roughly 30° for a calibrated array.
    public var spatialScale: Float = 0.33
    /// Spatial evidence below this confidence is ignored.
    public var minSpatialConfidence: Float = 0.3
    /// Maximum influence of the Sortformer slot on the fused score: +this value for the cluster
    /// the slot maps to, −half of it for a cluster another slot maps to. Small enough that a
    /// clearly different timbre (or a voiceprint match) still wins over Sortformer.
    public var turnWeight: Float = 0.2

    public init() {}

    /// Defaults with `MM_FUSION_*` environment overrides, used only for threshold sweeps
    /// (`MM_FUSION_CLUSTER`, `MM_FUSION_MERGE`, `MM_FUSION_SMALL_MERGE`, `MM_FUSION_SMALL_COUNT`).
    /// - Parameter env: Environment to read (injectable for tests).
    /// - Returns: The configuration.
    public static func fromEnvironment(_ env: [String: String] = ProcessInfo.processInfo.environment) -> SpeakerFusionConfig {
        var c = SpeakerFusionConfig()
        if let v = env["MM_FUSION_CLUSTER"].flatMap(Float.init) { c.clusterThreshold = v }
        if let v = env["MM_FUSION_MERGE"].flatMap(Float.init) { c.mergeThreshold = v }
        if let v = env["MM_FUSION_SMALL_MERGE"].flatMap(Float.init) { c.smallMergeThreshold = v }
        if let v = env["MM_FUSION_SMALL_COUNT"].flatMap(Int.init) { c.smallClusterCount = v }
        return c
    }
}

/// Online speaker attribution fusing timbre (speaker embeddings), position
/// (direction of arrival) and stream origin, anchored by enrolled voiceprints.
///
/// * Origin is a hard partition: a remote voice never merges with a room voice.
/// * Timbre is the primary signal; direction adds up to ±`spatialWeight`, which
///   separates similar-sounding people sitting apart and stabilises short utterances.
/// * Direction is per-meeting only (people move), so it never enters voiceprints.
/// * As centroids sharpen, an anonymous cluster can be renamed to an enrolled person or merged
///   into another cluster; those changes are reported as ``SpeakerRelabel``s.
public final class OnlineSpeakerTracker {
    private struct Cluster {
        var index: Int
        var origin: AudioOrigin
        var centroid: [Float]
        var count: Int
        var position: [Float]?
        var personId: String?
    }

    private let config: SpeakerFusionConfig
    private let bank: VoiceprintBank
    private var clusters: [Cluster] = []
    private var slots = SortformerSlotMapper()
    /// Display indices are never reused, even after a merge removes a cluster.
    private var nextIndex = 0

    /// Creates a tracker for one meeting.
    /// - Parameters:
    ///   - bank: Enrolled voiceprints.
    ///   - config: Thresholds and weights.
    public init(bank: VoiceprintBank, config: SpeakerFusionConfig = .init()) {
        self.bank = bank
        self.config = config
    }

    /// Assigns an observation to a known person or an anonymous cluster, updating state.
    /// - Parameter obs: The observation.
    /// - Returns: The speaker label.
    public func assign(_ obs: SpeechObservation) -> SpeakerLabel {
        assignReportingRelabels(obs).label
    }

    /// Assigns an observation and reports clusters renamed or merged as a consequence.
    /// - Parameter obs: The observation.
    /// - Returns: The label for this observation and the relabels to broadcast (oldest first).
    public func assignReportingRelabels(_ obs: SpeechObservation) -> (label: SpeakerLabel, relabels: [SpeakerRelabel]) {
        if let match = bank.bestMatch(obs.embedding), match.score >= config.knownThreshold {
            let i = clusters.firstIndex { $0.personId == match.personId && $0.origin == obs.origin }
                ?? newCluster(obs, personId: match.personId)
            update(i, with: obs)
            let (survivor, relabels) = consolidate(i)
            voteSlot(obs, survivor)
            return (.known(personId: match.personId, score: match.score), relabels)
        }
        let i: Int
        if let (best, score) = bestCluster(for: obs), score >= config.clusterThreshold {
            i = best
        } else {
            i = newCluster(obs, personId: nil)
        }
        update(i, with: obs)
        let (survivor, relabels) = consolidate(i)
        voteSlot(obs, survivor)
        if let person = clusters[survivor].personId {
            return (.known(personId: person, score: cosine(clusters[survivor].centroid, obs.embedding)), relabels)
        }
        return (.cluster(index: clusters[survivor].index), relabels)
    }

    /// Classifies an observation without changing any state (for volatile partial captions).
    /// - Parameter obs: The observation.
    /// - Returns: The label `assign` would most likely give, or `.unknown` when it would start a new cluster.
    public func peek(_ obs: SpeechObservation) -> SpeakerLabel {
        if let match = bank.bestMatch(obs.embedding), match.score >= config.knownThreshold {
            return .known(personId: match.personId, score: match.score)
        }
        guard let (best, score) = bestCluster(for: obs), score >= config.clusterThreshold else { return .unknown }
        return label(of: clusters[best])
    }

    /// Teaches the slot mapper that this observation's Sortformer slot is cluster `i`.
    private func voteSlot(_ obs: SpeechObservation, _ i: Int) {
        guard let slot = obs.turnSlot else { return }
        slots.vote(slot: slot, origin: obs.origin, cluster: clusters[i].index)
    }

    private func bestCluster(for obs: SpeechObservation) -> (Int, Float)? {
        clusters.indices
            .filter { clusters[$0].origin == obs.origin }
            .map { ($0, fusedScore(clusters[$0], obs)) }
            .max { $0.1 < $1.1 }
    }

    private func label(of c: Cluster) -> SpeakerLabel {
        if let person = c.personId {
            return .known(personId: person, score: bank.score(personId: person, c.centroid) ?? 0)
        }
        return .cluster(index: c.index)
    }

    /// Renames cluster `i` if its centroid now matches a voiceprint, then merges it with any
    /// same-origin cluster it has converged with.
    /// - Returns: The array position of the cluster that now holds `i`'s speech, and the relabels.
    private func consolidate(_ i: Int) -> (Int, [SpeakerRelabel]) {
        var relabels: [SpeakerRelabel] = []
        var current = i
        if clusters[current].personId == nil, clusters[current].count >= config.minCountForRename,
           let match = bank.bestMatch(clusters[current].centroid), match.score >= config.knownThreshold {
            let before = label(of: clusters[current])
            clusters[current].personId = match.personId
            relabels.append(SpeakerRelabel(from: before, to: label(of: clusters[current])))
        }
        while let j = mergeCandidate(for: current) {
            let a = clusters[current], b = clusters[j]
            // The named cluster survives (its label is the more informative one), otherwise the older one.
            let keepCurrent = a.personId != nil || (b.personId == nil && a.index < b.index)
            let (keep, drop) = keepCurrent ? (current, j) : (j, current)
            let from = label(of: clusters[drop])
            slots.merge(cluster: clusters[drop].index, into: clusters[keep].index)
            merge(clusters[drop], into: keep)
            let to = label(of: clusters[keep])
            if from != to { relabels.append(SpeakerRelabel(from: from, to: to)) }
            clusters.remove(at: drop)
            current = keep > drop ? keep - 1 : keep
        }
        return (current, relabels)
    }

    private func mergeCandidate(for i: Int) -> Int? {
        let c = clusters[i]
        return clusters.indices
            .filter { j in
                j != i && clusters[j].origin == c.origin
                    // Two different enrolled people are never one speaker.
                    && (c.personId == nil || clusters[j].personId == nil || c.personId == clusters[j].personId)
            }
            .map { j in (j, centroidScore(c, clusters[j])) }
            .filter { j, score in
                // Only an established speaker absorbs a small cluster, and only when it is the
                // one being updated: a cluster just opened for an off-timbre utterance must not
                // fold back in the same step, or the cluster threshold would never apply.
                let absorbs = c.count >= config.smallClusterCount && clusters[j].count < config.smallClusterCount
                return score >= (absorbs ? config.smallMergeThreshold : config.mergeThreshold)
            }
            .max { $0.1 < $1.1 }?.0
    }

    private func centroidScore(_ a: Cluster, _ b: Cluster) -> Float {
        let timbre = cosine(a.centroid, b.centroid)
        guard let pa = a.position, let pb = b.position, pa.count == pb.count else { return timbre }
        return timbre + config.spatialWeight * spatialTerm(pa, pb)
    }

    private func merge(_ source: Cluster, into k: Int) {
        var c = clusters[k]
        let (n, m) = (Float(c.count), Float(source.count))
        c.centroid = normalized(zip(c.centroid, source.centroid).map { ($0 * n + $1 * m) / max(n + m, 1) })
        if c.position == nil { c.position = source.position }
        c.personId = c.personId ?? source.personId
        c.count += source.count
        clusters[k] = c
    }

    private func fusedScore(_ c: Cluster, _ obs: SpeechObservation) -> Float {
        // Timbre, plus the turn evidence: Sortformer tracks who holds the floor even when a short
        // utterance's embedding is too noisy to place on its own.
        var score = cosine(c.centroid, obs.embedding)
        if let slot = obs.turnSlot {
            score += config.turnWeight * slots.agreement(slot: slot, origin: obs.origin, cluster: c.index)
        }
        guard let a = c.position, let cue = obs.spatial, cue.confidence >= config.minSpatialConfidence,
              a.count == cue.position.count
        else { return score }
        return score + config.spatialWeight * cue.confidence * spatialTerm(a, cue.position)
    }

    /// Spatial agreement in −1…1: +1 at the same spot, −1 far apart.
    private func spatialTerm(_ a: [Float], _ b: [Float]) -> Float {
        let distance = zip(a, b).reduce(0) { $0 + ($1.0 - $1.1) * ($1.0 - $1.1) }.squareRoot()
        let delta = distance / config.spatialScale
        return 2 * exp(-delta * delta) - 1
    }

    private func newCluster(_ obs: SpeechObservation, personId: String?) -> Int {
        clusters.append(Cluster(index: nextIndex, origin: obs.origin, centroid: obs.embedding, count: 0,
                                position: nil, personId: personId))
        nextIndex += 1
        return clusters.count - 1
    }

    private func update(_ i: Int, with obs: SpeechObservation) {
        var c = clusters[i]
        let n = Float(c.count)
        c.centroid = normalized(zip(c.centroid, obs.embedding).map { ($0 * n + $1) / (n + 1) })
        if let cue = obs.spatial, cue.confidence >= config.minSpatialConfidence {
            // Exponential moving average: follows a person who shifts in their seat, and slow
            // drift of the cue scale. A cue of a different shape (input changed) restarts it.
            if let old = c.position, old.count == cue.position.count {
                c.position = zip(old, cue.position).map { 0.8 * $0 + 0.2 * $1 }
            } else {
                c.position = cue.position
            }
        }
        c.count += 1
        clusters[i] = c
    }
}

/// Cosine similarity of two equal-length vectors.
/// - Returns: −1…1, or 0 when either vector is zero.
public func cosine(_ a: [Float], _ b: [Float]) -> Float {
    var dot: Float = 0, na: Float = 0, nb: Float = 0
    for (x, y) in zip(a, b) { dot += x * y; na += x * x; nb += y * y }
    let denom = (na * nb).squareRoot()
    return denom > 0 ? dot / denom : 0
}

/// Scales a vector to unit length (zero vectors are returned unchanged).
public func normalized(_ v: [Float]) -> [Float] {
    let n = v.reduce(0) { $0 + $1 * $1 }.squareRoot()
    return n > 0 ? v.map { $0 / n } : v
}
