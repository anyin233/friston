import Foundation

/// `capture/start` params: what one recording session should capture.
///
/// Wire keys are camelCase and must match `mm_core::CapturePreset` in `crates/mm-core/src/capture.rs`.
public struct CapturePreset: Codable, Sendable, Equatable {
    /// Meeting title, used to name the recording folder.
    public var title: String?
    /// ScreenCaptureKit window to record (window-level capture, the default for meetings).
    public var windowID: UInt32?
    /// Whole display to record instead of a window.
    public var displayID: UInt32?
    /// Core Audio UID of the microphone; every channel is kept for spatial cues.
    public var microphoneUID: String?
    /// Record what the Mac plays (remote participants).
    public var systemAudio: Bool
    /// BCP-47 caption language hint for streaming ASR (`zh`, `en`, …); `nil` means auto-detect.
    public var captionLanguage: String?
    /// Screen frame rate; meetings are mostly static, so the default is low.
    public var fps: Int?
    /// Overrides the recordings root (`~/Movies/Meeting Master`); used by tests and power users.
    public var outputDirectory: String?
    /// Enrolled voiceprints for live speaker naming; samples from another embedding model are ignored.
    public var voiceprints: [VoiceprintSample]?

    /// Creates a preset.
    /// - Parameters: see the stored properties; everything is optional and audio is off by default.
    public init(title: String? = nil, windowID: UInt32? = nil, displayID: UInt32? = nil, microphoneUID: String? = nil,
                systemAudio: Bool = false, captionLanguage: String? = nil, fps: Int? = nil,
                outputDirectory: String? = nil, voiceprints: [VoiceprintSample]? = nil) {
        self.title = title
        self.windowID = windowID
        self.displayID = displayID
        self.microphoneUID = microphoneUID
        self.systemAudio = systemAudio
        self.captionLanguage = captionLanguage
        self.fps = fps
        self.outputDirectory = outputDirectory
        self.voiceprints = voiceprints
    }

    private enum CodingKeys: String, CodingKey {
        case title, windowID, displayID, microphoneUID, systemAudio, captionLanguage, fps, outputDirectory, voiceprints
    }

    /// Decodes a preset, treating a missing `systemAudio` as `false`.
    /// - Parameter decoder: Decoder positioned at the params object.
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        title = try c.decodeIfPresent(String.self, forKey: .title)
        windowID = try c.decodeIfPresent(UInt32.self, forKey: .windowID)
        displayID = try c.decodeIfPresent(UInt32.self, forKey: .displayID)
        microphoneUID = try c.decodeIfPresent(String.self, forKey: .microphoneUID)
        systemAudio = try c.decodeIfPresent(Bool.self, forKey: .systemAudio) ?? false
        captionLanguage = try c.decodeIfPresent(String.self, forKey: .captionLanguage)
        fps = try c.decodeIfPresent(Int.self, forKey: .fps)
        outputDirectory = try c.decodeIfPresent(String.self, forKey: .outputDirectory)
        voiceprints = try c.decodeIfPresent([VoiceprintSample].self, forKey: .voiceprints)
    }
}

/// Why a preset cannot be recorded.
public enum CapturePlanError: Error, Equatable, CustomStringConvertible {
    case nothingToRecord
    case windowAndDisplay
    case unknownMicrophone(String)
    case invalidFPS(Int)

    /// Message sent back as the RPC error text.
    public var description: String {
        switch self {
        case .nothingToRecord: "capture/start needs at least one of windowID, displayID, microphoneUID or systemAudio"
        case .windowAndDisplay: "capture/start takes windowID or displayID, not both"
        case .unknownMicrophone(let uid): "no audio input with uid \(uid); call audio/probeInputs for the current list"
        case .invalidFPS(let fps): "fps must be 1…60, got \(fps)"
        }
    }
}

/// Paths of the files one session writes. Each source gets its own file so that ASR can
/// read the microphone and system streams separately (stream origin is a hard speaker partition).
public struct CaptureFiles: Codable, Sendable, Equatable {
    public var screen: URL?
    public var microphone: URL?
    public var systemAudio: URL?

    /// Creates a file set.
    /// - Parameters: optional path per source.
    public init(screen: URL? = nil, microphone: URL? = nil, systemAudio: URL? = nil) {
        self.screen = screen
        self.microphone = microphone
        self.systemAudio = systemAudio
    }

    private enum CodingKeys: String, CodingKey { case screen, microphone, systemAudio }

    /// Encodes plain filesystem paths (not `file://` URLs) so the Rust side gets `PathBuf`s.
    /// - Parameter encoder: Target encoder.
    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encodeIfPresent(screen?.path, forKey: .screen)
        try c.encodeIfPresent(microphone?.path, forKey: .microphone)
        try c.encodeIfPresent(systemAudio?.path, forKey: .systemAudio)
    }

    /// Decodes plain filesystem paths.
    /// - Parameter decoder: Decoder positioned at the files object.
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        screen = try c.decodeIfPresent(String.self, forKey: .screen).map { URL(filePath: $0) }
        microphone = try c.decodeIfPresent(String.self, forKey: .microphone).map { URL(filePath: $0) }
        systemAudio = try c.decodeIfPresent(String.self, forKey: .systemAudio).map { URL(filePath: $0) }
    }
}

/// A validated preset resolved against the current devices, plus where its files go.
public struct CapturePlan: Sendable, Equatable {
    public static let defaultFPS = 15

    public var preset: CapturePreset
    public var microphone: AudioInputInfo?
    public var fps: Int
    public var directory: URL
    public var files: CaptureFiles

    /// The default recordings root. Media stays outside the Obsidian vault; notes link to it.
    public static var defaultBaseDirectory: URL {
        FileManager.default.homeDirectoryForCurrentUser.appending(path: "Movies/Meeting Master", directoryHint: .isDirectory)
    }

    /// Validates a preset and lays out its output files. Does not touch the filesystem.
    /// - Parameters:
    ///   - preset: Requested sources.
    ///   - inputs: Current audio inputs (from `AudioInputProbe.inputs()`).
    ///   - baseDirectory: Recordings root; `preset.outputDirectory` wins when set.
    ///   - start: Session start, used in the folder name.
    ///   - timeZone: Zone for the folder name (the user's local time).
    /// - Returns: The plan.
    /// - Throws: `CapturePlanError` describing the first problem found.
    public static func make(preset: CapturePreset, inputs: [AudioInputInfo], baseDirectory: URL = defaultBaseDirectory,
                            start: Date = .now, timeZone: TimeZone = .current) throws -> CapturePlan {
        let records = preset.windowID != nil || preset.displayID != nil || preset.microphoneUID != nil || preset.systemAudio
        guard records else { throw CapturePlanError.nothingToRecord }
        guard preset.windowID == nil || preset.displayID == nil else { throw CapturePlanError.windowAndDisplay }
        let fps = preset.fps ?? defaultFPS
        guard (1...60).contains(fps) else { throw CapturePlanError.invalidFPS(fps) }

        var microphone: AudioInputInfo?
        if let uid = preset.microphoneUID {
            guard let match = inputs.first(where: { $0.uid == uid }) else { throw CapturePlanError.unknownMicrophone(uid) }
            microphone = match
        }

        let root = preset.outputDirectory.map { URL(filePath: $0, directoryHint: .isDirectory) } ?? baseDirectory
        let directory = root.appending(path: folderName(title: preset.title, start: start, timeZone: timeZone),
                                       directoryHint: .isDirectory)
        let recordsScreen = preset.windowID != nil || preset.displayID != nil
        let files = CaptureFiles(
            screen: recordsScreen ? directory.appending(path: "screen.mov") : nil,
            microphone: microphone != nil ? directory.appending(path: "mic.caf") : nil,
            systemAudio: preset.systemAudio ? directory.appending(path: "system.caf") : nil)
        return CapturePlan(preset: preset, microphone: microphone, fps: fps, directory: directory, files: files)
    }

    /// Builds `yyyy-MM-dd HHmm <title>`, sortable in Finder and safe as one path component.
    /// - Parameters:
    ///   - title: Meeting title; `会议` when missing or blank.
    ///   - start: Session start.
    ///   - timeZone: Zone used for the timestamp.
    /// - Returns: The folder name.
    static func folderName(title: String?, start: Date, timeZone: TimeZone) -> String {
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = timeZone
        let c = calendar.dateComponents([.year, .month, .day, .hour, .minute], from: start)
        let stamp = String(format: "%04d-%02d-%02d %02d%02d", c.year ?? 0, c.month ?? 0, c.day ?? 0, c.hour ?? 0, c.minute ?? 0)
        // "/" would nest folders and ":" shows as "/" in Finder, so both become "-".
        let cleaned = (title ?? "")
            .replacingOccurrences(of: "/", with: "-")
            .replacingOccurrences(of: ":", with: "-")
            .trimmingCharacters(in: .whitespacesAndNewlines)
        return "\(stamp) \(cleaned.isEmpty ? "会议" : cleaned)"
    }
}

/// Per-channel loudness for the `audio/levels` notification.
public struct ChannelLevels: Codable, Sendable, Equatable {
    public var rmsDb: [Float]
    public var peakDb: [Float]
}

/// Computes dBFS levels for level meters in the UI.
public enum LevelMeter {
    /// Reported for silence, so the UI never receives `-inf` (which JSON cannot encode).
    public static let floorDb: Float = -96

    /// Measures deinterleaved channels.
    /// - Parameter channels: One sample array per channel, full scale = ±1.
    /// - Returns: RMS and peak in dBFS per channel, clamped to `floorDb`.
    public static func measure(channels: [[Float]]) -> ChannelLevels {
        var rms: [Float] = []
        var peak: [Float] = []
        for samples in channels {
            var sumSquares: Float = 0
            var maxAbs: Float = 0
            for s in samples {
                sumSquares += s * s
                maxAbs = max(maxAbs, abs(s))
            }
            let meanSquare = samples.isEmpty ? 0 : sumSquares / Float(samples.count)
            rms.append(decibels(meanSquare.squareRoot()))
            peak.append(decibels(maxAbs))
        }
        return ChannelLevels(rmsDb: rms, peakDb: peak)
    }

    /// Measures interleaved samples (the layout ScreenCaptureKit and most converters produce).
    /// - Parameters:
    ///   - interleaved: Frames laid out `c0 c1 … c0 c1 …`.
    ///   - channelCount: Channels per frame; must be ≥ 1.
    /// - Returns: Levels per channel.
    public static func measure(interleaved: [Float], channelCount: Int) -> ChannelLevels {
        let count = max(channelCount, 1)
        var channels = [[Float]](repeating: [], count: count)
        for (i, s) in interleaved.enumerated() { channels[i % count].append(s) }
        return measure(channels: channels)
    }

    /// Converts a linear amplitude to dBFS, clamped to `floorDb`.
    /// - Parameter amplitude: Linear amplitude, 1 = full scale.
    /// - Returns: Level in dBFS.
    static func decibels(_ amplitude: Float) -> Float {
        guard amplitude > 0 else { return floorDb }
        return max(20 * log10(amplitude), floorDb)
    }
}

/// Rate limiter for high-frequency notifications such as `audio/levels`.
public struct Throttle: Sendable {
    public let interval: TimeInterval
    private var last: TimeInterval?

    /// Creates a throttle.
    /// - Parameter interval: Minimum seconds between two firings.
    public init(interval: TimeInterval) {
        self.interval = interval
    }

    /// Reports whether to fire now, and records the firing if so.
    /// - Parameter time: Current time in seconds (any monotonic clock).
    /// - Returns: `true` at most once per `interval`.
    public mutating func shouldFire(at time: TimeInterval) -> Bool {
        // A tiny epsilon keeps 10.00 → 10.10 firing despite binary rounding.
        if let last, time - last < interval - 1e-9 { return false }
        last = time
        return true
    }
}

/// Lifecycle reported by the `capture/state` notification.
public enum CaptureState: String, Codable, Sendable, Equatable {
    case idle, starting, recording, stopping, stopped, failed
}

/// An illegal lifecycle step, e.g. a second `capture/start` while recording.
public enum CaptureStateError: Error, Equatable, CustomStringConvertible {
    case invalidTransition(from: CaptureState, to: CaptureState)

    /// Message sent back as the RPC error text.
    public var description: String {
        switch self {
        case .invalidTransition(let from, let to): "capture cannot go from \(from.rawValue) to \(to.rawValue)"
        }
    }
}

/// Guards the one-session-at-a-time capture lifecycle.
public struct CaptureStateMachine: Sendable {
    public private(set) var state: CaptureState = .idle

    /// Creates a machine in `idle`.
    public init() {}

    /// Moves to `next` if the lifecycle allows it.
    /// - Parameter next: Target state.
    /// - Throws: `CaptureStateError.invalidTransition` otherwise; the state is unchanged.
    public mutating func transition(to next: CaptureState) throws {
        let allowed: Bool = switch (state, next) {
        case (.idle, .starting), (.stopped, .starting), (.failed, .starting): true
        case (.starting, .recording), (.recording, .stopping), (.stopping, .stopped): true
        case (.starting, .failed), (.recording, .failed), (.stopping, .failed): true
        default: false
        }
        guard allowed else { throw CaptureStateError.invalidTransition(from: state, to: next) }
        state = next
    }
}

/// `capture/start` result.
public struct CaptureStarted: Codable, Sendable, Equatable {
    public var sessionId: String
    public var directory: String
    public var files: CaptureFiles
    /// Channels kept in `mic.caf`; ≥ 2 real channels enable spatial cues.
    public var microphoneChannels: Int?
    public var startedAt: Date

    /// Creates the result.
    /// - Parameters: see the stored properties.
    public init(sessionId: String, directory: String, files: CaptureFiles, microphoneChannels: Int?, startedAt: Date) {
        self.sessionId = sessionId
        self.directory = directory
        self.files = files
        self.microphoneChannels = microphoneChannels
        self.startedAt = startedAt
    }
}

/// `capture/stop` result. `files` lists only files that were actually written.
public struct CaptureStopped: Codable, Sendable, Equatable {
    public var sessionId: String
    public var durationMs: UInt64
    public var files: CaptureFiles

    /// Creates the result.
    /// - Parameters: see the stored properties.
    public init(sessionId: String, durationMs: UInt64, files: CaptureFiles) {
        self.sessionId = sessionId
        self.durationMs = durationMs
        self.files = files
    }
}

/// `capture/state` notification payload.
public struct CaptureStateEvent: Codable, Sendable, Equatable {
    public var sessionId: String
    public var state: CaptureState
    /// Set when `state` is `failed`.
    public var error: String?

    /// Creates the payload.
    /// - Parameters: see the stored properties.
    public init(sessionId: String, state: CaptureState, error: String? = nil) {
        self.sessionId = sessionId
        self.state = state
        self.error = error
    }
}

/// Which stream an `audio/levels` notification measures.
public enum AudioSource: String, Codable, Sendable, Equatable {
    case microphone, system
}

/// `audio/levels` notification payload, sent at most every 100 ms per source.
public struct AudioLevelsEvent: Codable, Sendable, Equatable {
    public var sessionId: String
    public var source: AudioSource
    public var rmsDb: [Float]
    public var peakDb: [Float]

    /// Creates the payload.
    /// - Parameters:
    ///   - sessionId: Capture session.
    ///   - source: Measured stream.
    ///   - levels: Per-channel levels from `LevelMeter`.
    public init(sessionId: String, source: AudioSource, levels: ChannelLevels) {
        self.sessionId = sessionId
        self.source = source
        rmsDb = levels.rmsDb
        peakDb = levels.peakDb
    }
}
