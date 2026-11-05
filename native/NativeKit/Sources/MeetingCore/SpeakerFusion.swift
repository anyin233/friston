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

    /// Creates an observation.
    /// - Parameters:
    ///   - embedding: L2-normalised speaker embedding.
    ///   - origin: Microphone or system audio.
    ///   - spatial: Position cue, or `nil` for mono input.
    public init(embedding: [Float], origin: AudioOrigin, spatial: SpatialCue? = nil) {
        self.embedding = embedding
        self.origin = origin
        self.spatial = spatial
    }
}

/// Voiceprints defined ahead of time (recorded or clipped by the user).
public struct VoiceprintBank: Sendable {
    private var voices: [String: [[Float]]] = [:]

    public init() {}

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
}

/// Result of assigning an observation to a speaker.
public enum SpeakerLabel: Equatable, Sendable {
    case known(personId: String, score: Float)
    case cluster(index: Int)
}

/// Tunables for ``OnlineSpeakerTracker``.
public struct SpeakerFusionConfig: Sendable {
    /// Cosine needed to name an enrolled person.
    public var knownThreshold: Float = 0.62
    /// Fused score needed to join an existing anonymous cluster.
    public var clusterThreshold: Float = 0.55
    /// Maximum influence of direction on the fused score (± this value).
    public var spatialWeight: Float = 0.25
    /// Distance in ``SpatialCue`` units at which spatial similarity has fallen to 1/e.
    /// Cues span about −1…1, so 0.33 is roughly 30° for a calibrated array.
    public var spatialScale: Float = 0.33
    /// Spatial evidence below this confidence is ignored.
    public var minSpatialConfidence: Float = 0.3

    public init() {}
}

/// Online speaker attribution fusing timbre (speaker embeddings), position
/// (direction of arrival) and stream origin, anchored by enrolled voiceprints.
///
/// * Origin is a hard partition: a remote voice never merges with a room voice.
/// * Timbre is the primary signal; direction adds up to ±`spatialWeight`, which
///   separates similar-sounding people sitting apart and stabilises short utterances.
/// * Direction is per-meeting only (people move), so it never enters voiceprints.
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
        if let match = bank.bestMatch(obs.embedding), match.score >= config.knownThreshold {
            let i = clusters.firstIndex { $0.personId == match.personId && $0.origin == obs.origin }
                ?? newCluster(obs, personId: match.personId)
            update(i, with: obs)
            return .known(personId: match.personId, score: match.score)
        }
        let scored = clusters.indices
            .filter { clusters[$0].origin == obs.origin }
            .map { ($0, fusedScore(clusters[$0], obs)) }
            .max { $0.1 < $1.1 }
        let i: Int
        if let (best, score) = scored, score >= config.clusterThreshold {
            i = best
        } else {
            i = newCluster(obs, personId: nil)
        }
        update(i, with: obs)
        if let person = clusters[i].personId {
            return .known(personId: person, score: cosine(clusters[i].centroid, obs.embedding))
        }
        return .cluster(index: clusters[i].index)
    }

    private func fusedScore(_ c: Cluster, _ obs: SpeechObservation) -> Float {
        let timbre = cosine(c.centroid, obs.embedding)
        guard let a = c.position, let cue = obs.spatial, cue.confidence >= config.minSpatialConfidence,
              a.count == cue.position.count
        else { return timbre }
        let distance = zip(a, cue.position).reduce(0) { $0 + ($1.0 - $1.1) * ($1.0 - $1.1) }.squareRoot()
        let delta = distance / config.spatialScale
        let spatialSimilarity = exp(-delta * delta)
        return timbre + config.spatialWeight * cue.confidence * (2 * spatialSimilarity - 1)
    }

    private func newCluster(_ obs: SpeechObservation, personId: String?) -> Int {
        clusters.append(Cluster(index: clusters.count, origin: obs.origin, centroid: obs.embedding, count: 0,
                                position: nil, personId: personId))
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
