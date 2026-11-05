import Foundation

/// One stored voiceprint as sent by the app (`capture/start.voiceprints`).
public struct VoiceprintSample: Codable, Sendable, Equatable {
    public var personId: String
    /// Embedding model that produced `vector`; only vectors of the engine's model are used.
    public var model: String
    public var vector: [Float]

    /// Creates a sample.
    /// - Parameters: see the stored properties.
    public init(personId: String, model: String, vector: [Float]) {
        self.personId = personId
        self.model = model
        self.vector = vector
    }
}

/// Quality indicators for a voiceprint, so the app can ask for a better sample.
public struct VoiceprintQuality: Codable, Sendable, Equatable {
    /// `false` when parts of the clip sound like a different person.
    public var singleSpeaker: Bool
    /// Speech-to-background ratio; `nil` when the clip has no non-speech part to measure noise on.
    public var snrDb: Float?

    /// Creates the indicators.
    /// - Parameters: see the stored properties.
    public init(singleSpeaker: Bool, snrDb: Float?) {
        self.singleSpeaker = singleSpeaker
        self.snrDb = snrDb
    }
}

/// `voiceprint/fromClip` and `voiceprint/enroll` result.
public struct VoiceprintResult: Codable, Sendable, Equatable {
    public var model: String
    /// L2-normalised embedding of the clip's speech.
    public var vector: [Float]
    /// Net speech used, in ms.
    public var speechMs: Int
    public var quality: VoiceprintQuality

    /// Creates the result.
    /// - Parameters: see the stored properties.
    public init(model: String, vector: [Float], speechMs: Int, quality: VoiceprintQuality) {
        self.model = model
        self.vector = vector
        self.speechMs = speechMs
        self.quality = quality
    }
}

/// Invalid clip bounds.
public enum VoiceprintAnalysisError: Error, Equatable, CustomStringConvertible {
    case emptyRange(startMs: Int?, endMs: Int?, durationMs: Int)

    /// Message sent back as the RPC error text.
    public var description: String {
        switch self {
        case .emptyRange(let s, let e, let d):
            "clip range \(s.map(String.init) ?? "start")…\(e.map(String.init) ?? "end") ms is empty (audio is \(d) ms)"
        }
    }
}

/// Model-free checks behind voiceprint quality reporting.
public enum VoiceprintAnalysis {
    /// Converts optional clip bounds to a sample range, clamped to the audio.
    /// - Parameters:
    ///   - totalSamples: Audio length.
    ///   - sampleRate: Audio rate.
    ///   - startMs: Clip start, or the beginning.
    ///   - endMs: Clip end, or the end of the audio.
    /// - Returns: A non-empty sample range.
    /// - Throws: ``VoiceprintAnalysisError/emptyRange(startMs:endMs:durationMs:)`` when nothing remains.
    public static func sampleRange(totalSamples: Int, sampleRate: Int, startMs: Int?, endMs: Int?) throws -> Range<Int> {
        let toSample = { (ms: Int) in Int((Double(ms) * Double(sampleRate) / 1000).rounded()) }
        let lo = max(0, min(totalSamples, startMs.map(toSample) ?? 0))
        let hi = max(0, min(totalSamples, endMs.map(toSample) ?? totalSamples))
        guard hi > lo else {
            throw VoiceprintAnalysisError.emptyRange(startMs: startMs, endMs: endMs,
                                                    durationMs: totalSamples * 1000 / max(sampleRate, 1))
        }
        return lo..<hi
    }

    /// Estimates signal-to-noise from VAD speech regions versus everything else.
    /// - Parameters:
    ///   - samples: The clip.
    ///   - speech: Speech regions (sample ranges within `samples`).
    /// - Returns: dB, or `nil` when either speech or non-speech is missing.
    public static func snrDb(samples: [Float], speech: [Range<Int>]) -> Float? {
        var isSpeech = [Bool](repeating: false, count: samples.count)
        for r in speech {
            for i in r.clamped(to: 0..<samples.count) { isSpeech[i] = true }
        }
        var (se, sn, ne, nn): (Double, Int, Double, Int) = (0, 0, 0, 0)
        for (i, s) in samples.enumerated() {
            if isSpeech[i] { se += Double(s * s); sn += 1 } else { ne += Double(s * s); nn += 1 }
        }
        // Under ~50 ms of either part, the estimate is noise itself.
        guard sn > 0, nn >= 800 || (nn > 0 && nn * 20 >= samples.count) else { return nil }
        let speechPower = se / Double(sn), noisePower = max(ne / Double(nn), 1e-12)
        guard speechPower > 0 else { return nil }
        return Float(10 * log10(speechPower / noisePower))
    }

    /// Decides whether windowed embeddings of a clip all come from one voice.
    /// - Parameters:
    ///   - windowEmbeddings: Embeddings of consecutive ~2–3 s windows of speech.
    ///   - threshold: Minimum cosine of every window to the mean.
    /// - Returns: `true` with fewer than two windows (no evidence against).
    public static func isSingleSpeaker(windowEmbeddings: [[Float]], threshold: Float = 0.5) -> Bool {
        guard windowEmbeddings.count >= 2, let dims = windowEmbeddings.first?.count else { return true }
        var mean = [Float](repeating: 0, count: dims)
        for e in windowEmbeddings { for i in 0..<min(dims, e.count) { mean[i] += e[i] } }
        let centre = normalized(mean)
        return windowEmbeddings.allSatisfy { cosine($0, centre) >= threshold }
    }
}

/// One speaker turn from offline diarization, in seconds.
public struct SpeakerTurn: Sendable, Equatable {
    public var start: Float
    public var end: Float
    public var speaker: Int

    /// Creates a turn.
    /// - Parameters: see the stored properties.
    public init(start: Float, end: Float, speaker: Int) {
        self.start = start
        self.end = end
        self.speaker = speaker
    }
}

/// Plans offline transcription windows from diarized turns.
public enum TurnPlanner {
    /// Merges a speaker's consecutive turns across short gaps and splits overly long ones.
    ///
    /// Diarizers emit many short fragments; transcribing each separately loses context and
    /// punctuation, while very long windows make Qwen3-ASR slower and more prone to loops.
    /// - Parameters:
    ///   - turns: Diarized turns, any order.
    ///   - maxGap: Largest pause (s) bridged within one speaker's window.
    ///   - maxDuration: Longest window (s); longer ones are cut into equal pieces.
    /// - Returns: Windows in time order.
    public static func windows(from turns: [SpeakerTurn], maxGap: Float = 0.6, maxDuration: Float = 25) -> [SpeakerTurn] {
        var merged: [SpeakerTurn] = []
        for t in turns.sorted(by: { $0.start < $1.start }) where t.end > t.start {
            if var last = merged.last, last.speaker == t.speaker, t.start - last.end <= maxGap,
               t.end - last.start <= maxDuration {
                last.end = max(last.end, t.end)
                merged[merged.count - 1] = last
            } else {
                merged.append(t)
            }
        }
        return merged.flatMap { t -> [SpeakerTurn] in
            let length = t.end - t.start
            guard length > maxDuration else { return [t] }
            let pieces = Int((length / maxDuration).rounded(.up))
            let size = length / Float(pieces)
            return (0..<pieces).map { k in
                SpeakerTurn(start: t.start + Float(k) * size,
                            end: k == pieces - 1 ? t.end : t.start + Float(k + 1) * size, speaker: t.speaker)
            }
        }
    }
}

/// One segment of a `transcribe/file` result.
public struct FileTranscriptSegment: Codable, Sendable, Equatable {
    public var startMs: Int
    public var endMs: Int
    public var text: String
    public var speaker: SpeakerLabel
    /// Not known for a single mixed file; reserved for per-stream transcription.
    public var origin: AudioOrigin?

    /// Creates a segment.
    /// - Parameters: see the stored properties.
    public init(startMs: Int, endMs: Int, text: String, speaker: SpeakerLabel, origin: AudioOrigin? = nil) {
        self.startMs = startMs
        self.endMs = endMs
        self.text = text
        self.speaker = speaker
        self.origin = origin
    }
}

/// `transcribe/file` result.
public struct FileTranscript: Codable, Sendable, Equatable {
    /// BCP-47 code: the requested language, or the majority guess over segments.
    public var language: String?
    public var segments: [FileTranscriptSegment]

    /// Creates the result.
    /// - Parameters: see the stored properties.
    public init(language: String?, segments: [FileTranscriptSegment]) {
        self.language = language
        self.segments = segments
    }
}
