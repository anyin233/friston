import Foundation

/// One stretch of speech that Sortformer attributes to one of its speaker slots.
///
/// Positions are stream sample indices (16 kHz in the live pipeline), the same coordinates the
/// utterance segmenter uses, so turns and utterances can be intersected directly.
public struct SortformerTurn: Equatable, Sendable {
    public var start: Int
    public var end: Int
    /// Speaker slot. In a snapshot this is the library's (compacted) id; inside
    /// ``SortformerTimeline`` it is the stable slot for the whole session.
    public var slot: Int

    /// Creates a turn.
    /// - Parameters:
    ///   - start: First sample.
    ///   - end: Sample just after the turn.
    ///   - slot: Speaker slot.
    public init(start: Int, end: Int, slot: Int) {
        self.start = start
        self.end = end
        self.slot = slot
    }

    /// Length in samples.
    public var length: Int { max(0, end - start) }
}

/// Sortformer streaming output for one audio stream, with slot ids that never change.
///
/// speech-swift's `SortformerStreamingSession` returns a whole-stream snapshot each time and
/// compacts the slot ids it reports (used slots sorted, renumbered 0…n−1). Its cache slots are
/// arrival-ordered, so compaction is almost always the identity, but a slot that first becomes
/// audible after a higher one would shift every later id by one and silently swap speakers.
/// The timeline re-identifies slots by the start of turns it has already seen (confirmed frames
/// never change), and answers the two questions the caption loop asks: who dominates an
/// utterance, and where inside it the speaker changes.
public struct SortformerTimeline: Sendable {
    /// Query thresholds, in seconds.
    public struct Config: Sendable {
        public var sampleRate: Int
        /// Speech by one slot needed before ``dominantSlot(start:end:)`` answers.
        public var minDominantSpeech: Double
        /// Share of the utterance's attributed speech the dominant slot must have.
        public var minDominantShare: Double
        /// Speech each side of a change needs before ``splitPoints(start:end:)`` cuts there;
        /// shorter turns are backchannels or Sortformer jitter, not a new caption.
        public var minSplitRun: Double

        /// Creates a config.
        /// - Parameters: see the stored properties.
        public init(sampleRate: Int = 16_000, minDominantSpeech: Double = 0.5, minDominantShare: Double = 0.6,
                    minSplitRun: Double = 1.0) {
            self.sampleRate = sampleRate
            self.minDominantSpeech = minDominantSpeech
            self.minDominantShare = minDominantShare
            self.minSplitRun = minSplitRun
        }
    }

    public let config: Config
    /// Turns kept for queries, with stable slots, sorted by start.
    private var turns: [SortformerTurn] = []
    /// Start sample of every turn ever seen → its stable slot. Survives ``trim(before:)``,
    /// because re-identification needs the old turns that the library still reports.
    private var slotByStart: [Int: Int] = [:]
    /// Snapshot id → stable slot, as of the last update.
    private var stableByReported: [Int: Int] = [:]
    private var nextSlot = 0
    private var floor = 0
    /// Samples up to which Sortformer output is final.
    public private(set) var confirmedUntil = 0

    /// Creates an empty timeline.
    /// - Parameter config: Query thresholds.
    public init(config: Config = .init()) {
        self.config = config
    }

    private func samples(_ seconds: Double) -> Int { Int(seconds * Double(config.sampleRate)) }

    /// Replaces the timeline with a new whole-stream snapshot.
    /// - Parameters:
    ///   - snapshot: Every turn from the stream start, with the library's slot ids.
    ///   - confirmedUntil: Samples whose diarization is final (the rest is lookahead).
    public mutating func update(snapshot: [SortformerTurn], confirmedUntil: Int) {
        self.confirmedUntil = max(self.confirmedUntil, confirmedUntil)
        // Vote: which stable slot did each reported id's already-known turns have?
        var votes: [Int: [Int: Int]] = [:]
        for t in snapshot {
            if let stable = slotByStart[t.start] { votes[t.slot, default: [:]][stable, default: 0] += 1 }
        }
        var map: [Int: Int] = [:]
        var taken = Set<Int>()
        for (reported, counts) in votes.sorted(by: { $0.key < $1.key }) {
            if let best = counts.max(by: { $0.value < $1.value || ($0.value == $1.value && $0.key > $1.key) }),
               !taken.contains(best.key) {
                map[reported] = best.key
                taken.insert(best.key)
            }
        }
        // No evidence (all its turns are new): keep the previous mapping if still free, else a new slot.
        for reported in Set(snapshot.map(\.slot)).sorted() where map[reported] == nil {
            if let previous = stableByReported[reported], !taken.contains(previous) {
                map[reported] = previous
            } else {
                map[reported] = nextSlot
            }
            taken.insert(map[reported]!)
            nextSlot = max(nextSlot, map[reported]! + 1)
        }
        stableByReported = map
        turns = snapshot.compactMap { t in
            guard t.end > floor, let slot = map[t.slot] else { return nil }
            return SortformerTurn(start: t.start, end: t.end, slot: slot)
        }.sorted { $0.start < $1.start }
        // Confirmed frames never change, so a turn's start never moves (the last turn may still
        // grow at its end). The first slot seen for a start is therefore the right one to keep.
        for t in snapshot where slotByStart[t.start] == nil { slotByStart[t.start] = map[t.slot] }
    }

    /// Drops turns that end before `sample` (audio the caption loop no longer holds).
    /// - Parameter sample: Earliest sample still of interest.
    public mutating func trim(before sample: Int) {
        floor = max(floor, sample)
        turns.removeAll { $0.end <= floor }
    }

    /// Per-slot speech inside `start..<end`.
    private func overlap(_ start: Int, _ end: Int) -> [Int: Int] {
        var bySlot: [Int: Int] = [:]
        for t in turns where t.end > start && t.start < end {
            bySlot[t.slot, default: 0] += min(t.end, end) - max(t.start, start)
        }
        return bySlot
    }

    /// The slot that speaks most of `start..<end`.
    /// - Parameters:
    ///   - start: Utterance start sample.
    ///   - end: Utterance end sample.
    /// - Returns: The stable slot, or `nil` with too little confirmed speech or no clear majority.
    public func dominantSlot(start: Int, end: Int) -> Int? {
        let bySlot = overlap(start, end)
        let total = bySlot.values.reduce(0, +)
        guard let best = bySlot.max(by: { $0.value < $1.value || ($0.value == $1.value && $0.key > $1.key) }),
              best.value >= samples(config.minDominantSpeech),
              Double(best.value) >= config.minDominantShare * Double(total)
        else { return nil }
        return best.key
    }

    /// Where the speaker changes inside an utterance.
    /// - Parameters:
    ///   - start: Utterance start sample.
    ///   - end: Utterance end sample.
    /// - Returns: Ascending cut positions (the start of each new speaker's turn); empty when one
    ///   speaker holds the floor or the changes are too short to caption separately.
    public func splitPoints(start: Int, end: Int) -> [Int] {
        // Consecutive turns of one slot form a run; short runs of another slot do not end it.
        var runs: [(slot: Int, start: Int, speech: Int)] = []
        for t in turns where t.end > start && t.start < end {
            let s = max(t.start, start), e = min(t.end, end)
            if let last = runs.last, last.slot == t.slot {
                runs[runs.count - 1].speech += e - s
            } else {
                runs.append((t.slot, s, e - s))
            }
        }
        let minRun = samples(config.minSplitRun)
        var current: Int?
        var cuts: [Int] = []
        var pieceStart = start
        for run in runs where run.speech >= minRun {
            defer { current = run.slot }
            guard let current, current != run.slot, run.start - pieceStart >= minRun, run.start > start else { continue }
            cuts.append(run.start)
            pieceStart = run.start
        }
        return cuts
    }

    /// Re-joins neighbouring pieces of a split utterance that fusion attributes to one speaker.
    ///
    /// Sortformer sometimes moves one voice to a new slot mid-sentence; cutting there would
    /// break the caption (and the ASR context) for nothing. Fusion has the final say.
    /// - Parameters:
    ///   - bounds: Utterance start, the cuts, and the end (ascending).
    ///   - labels: Speaker of each piece (`bounds.count − 1` labels).
    /// - Returns: The pieces to caption; equal non-`unknown` neighbours are merged.
    public static func joinPieces(bounds: [Int], labels: [SpeakerLabel])
        -> [(start: Int, end: Int, label: SpeakerLabel)] {
        var pieces: [(start: Int, end: Int, label: SpeakerLabel)] = []
        for (i, label) in labels.enumerated() where i + 1 < bounds.count {
            if let last = pieces.last, last.label == label, label != .unknown {
                pieces[pieces.count - 1].end = bounds[i + 1]
            } else {
                pieces.append((bounds[i], bounds[i + 1], label))
            }
        }
        return pieces
    }
}

/// Learns which fusion cluster each Sortformer slot is, by voting.
///
/// Sortformer slots are stable within a stream but anonymous and limited to four speakers;
/// fusion clusters carry identity (voiceprints, display index) across the whole meeting. Each
/// assigned utterance votes slot → cluster; a slot's cluster is its majority vote. Slots are
/// per origin, because microphone and system audio each run their own Sortformer session.
public struct SortformerSlotMapper: Sendable {
    private struct Key: Hashable, Sendable {
        var origin: AudioOrigin
        var slot: Int
    }

    private var votes: [Key: [Int: Float]] = [:]

    /// Creates an empty mapper.
    public init() {}

    /// Records that an utterance dominated by `slot` was attributed to `cluster`.
    /// - Parameters:
    ///   - slot: Stable Sortformer slot.
    ///   - origin: Stream the slot belongs to.
    ///   - cluster: Fusion cluster display index.
    ///   - weight: Vote weight (1 per utterance).
    public mutating func vote(slot: Int, origin: AudioOrigin, cluster: Int, weight: Float = 1) {
        votes[Key(origin: origin, slot: slot), default: [:]][cluster, default: 0] += weight
    }

    /// The cluster a slot maps to.
    /// - Parameters:
    ///   - slot: Stable Sortformer slot.
    ///   - origin: Stream the slot belongs to.
    /// - Returns: The majority cluster, or `nil` before any vote.
    public func cluster(for slot: Int, origin: AudioOrigin) -> Int? {
        votes[Key(origin: origin, slot: slot)]?
            .max { $0.value < $1.value || ($0.value == $1.value && $0.key > $1.key) }?.key
    }

    /// Moves every vote for `source` to `target` (the tracker merged two clusters).
    /// - Parameters:
    ///   - source: Cluster that disappeared.
    ///   - target: Cluster that absorbed it.
    public mutating func merge(cluster source: Int, into target: Int) {
        for key in votes.keys {
            if let moved = votes[key]?.removeValue(forKey: source) { votes[key]![target, default: 0] += moved }
        }
    }

    /// How much Sortformer agrees that an utterance of `slot` belongs to `cluster`.
    /// - Parameters:
    ///   - slot: The utterance's dominant slot.
    ///   - origin: Stream of the utterance.
    ///   - cluster: Candidate cluster display index.
    /// - Returns: +1 when the slot maps to the cluster, −0.5 when another slot of this stream
    ///   maps to it (Sortformer heard a different person there), 0 without evidence. The negative
    ///   side is weaker because Sortformer occasionally splits one person over two slots.
    public func agreement(slot: Int, origin: AudioOrigin, cluster: Int) -> Float {
        if self.cluster(for: slot, origin: origin) == cluster { return 1 }
        let claimed = votes.keys.contains { key in
            key.origin == origin && key.slot != slot && self.cluster(for: key.slot, origin: origin) == cluster
        }
        return claimed ? -0.5 : 0
    }
}
