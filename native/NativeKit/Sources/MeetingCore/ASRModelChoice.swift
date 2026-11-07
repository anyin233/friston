import Foundation

/// Where to load a Qwen3-ASR checkpoint from.
public struct ASRModelSource: Equatable, Sendable {
    /// Id speech-swift uses to pick the architecture (0.6B/1.7B) and quantization from its name,
    /// and to name its download cache.
    public var modelId: String
    /// Local checkpoint folder to load offline, or `nil` to let speech-swift use its cache or download.
    public var directory: URL?

    /// Creates a source.
    /// - Parameters: see the stored properties.
    public init(modelId: String, directory: URL?) {
        self.modelId = modelId
        self.directory = directory
    }
}

/// Why a requested refine model cannot be used.
public struct ASRModelChoiceError: Error, CustomStringConvertible, Equatable {
    public let description: String
}

/// Validates the model a caller asks `transcribe/file` to use, and its context hint.
///
/// The post-meeting pass may use a different (stronger) checkpoint than live captions. Only
/// Qwen3-ASR MLX checkpoints load, and speech-swift infers the architecture and bit width from
/// the model id alone, so a name without `0.6B`/`1.7B` would be loaded with the wrong shapes.
public enum ASRModelChoice {
    /// Longest context hint passed to the decoder. The hint is part of every window's prompt,
    /// so an unbounded attendee list would slow down every decode for no gain.
    public static let maxContextCharacters = 600

    /// Resolves a hub repo id (`owner/name`) or an absolute checkpoint folder.
    /// - Parameters:
    ///   - requested: What the caller sent.
    ///   - hubSnapshot: Cached `huggingface_hub` snapshot of a repo id, if any.
    ///   - isDirectory: Whether a path is an existing folder.
    /// - Returns: The source to load.
    /// - Throws: ``ASRModelChoiceError`` naming the problem and the value.
    public static func resolve(_ requested: String, hubSnapshot: (String) -> URL?,
                               isDirectory: (String) -> Bool) throws(ASRModelChoiceError) -> ASRModelSource {
        let value = requested.trimmingCharacters(in: .whitespacesAndNewlines)
        if value.hasPrefix("/") {
            guard isDirectory(value) else {
                throw ASRModelChoiceError(description: "model folder \(value) does not exist")
            }
            let id = idOfFolder(value)
            try check(id, requested: value)
            return ASRModelSource(modelId: id, directory: URL(filePath: value, directoryHint: .isDirectory))
        }
        let parts = value.split(separator: "/", omittingEmptySubsequences: false)
        guard parts.count == 2, parts.allSatisfy({ !$0.isEmpty }) else {
            throw ASRModelChoiceError(description:
                "model \"\(value)\" is neither a Hugging Face repo id (owner/name) nor an absolute folder")
        }
        try check(value, requested: value)
        return ASRModelSource(modelId: value, directory: hubSnapshot(value))
    }

    /// Normalizes the decoder context hint.
    /// - Parameter context: Caller text (meeting title, attendee names, terms).
    /// - Returns: Trimmed text of at most ``maxContextCharacters`` characters, or `nil` when blank.
    public static func context(_ context: String?) -> String? {
        guard let text = context?.trimmingCharacters(in: .whitespacesAndNewlines), !text.isEmpty else { return nil }
        return String(text.prefix(maxContextCharacters))
    }

    /// The id a checkpoint folder stands for: `owner/name` for a hub snapshot, else the folder name.
    private static func idOfFolder(_ path: String) -> String {
        let components = URL(filePath: path).pathComponents
        if let repo = components.last(where: { $0.hasPrefix("models--") }) {
            let parts = repo.dropFirst("models--".count).components(separatedBy: "--")
            if parts.count == 2 { return parts.joined(separator: "/") }
        }
        return components.last ?? path
    }

    /// Rejects ids speech-swift's Qwen3-ASR loader would misread.
    private static func check(_ id: String, requested: String) throws(ASRModelChoiceError) {
        let lower = id.lowercased()
        guard lower.contains("qwen3-asr") else {
            throw ASRModelChoiceError(description: "model \(requested) is not a Qwen3-ASR checkpoint")
        }
        guard lower.contains("0.6b") || lower.contains("1.7b") else {
            throw ASRModelChoiceError(description:
                "model \(requested) does not say 0.6B or 1.7B in its name; the loader needs it to pick the architecture")
        }
    }
}
