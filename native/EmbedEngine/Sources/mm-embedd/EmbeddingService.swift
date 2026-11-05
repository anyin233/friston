import EngineKit
import Foundation
import HuggingFace
import MLXHuggingFace
import MLXLMCommon
import MLXVLM
import Tokenizers

/// One item to embed; parts are concatenated in the order text → images → video → audio.
struct EmbedItem: Decodable {
    var text: String?
    var title: String?
    var images: [String]?
    var video: [String]?
    var audio: [String]?
    /// Window inside `audio` / `video`, so whole recordings can be embedded piecewise.
    var startMs: Int?
    var endMs: Int?
}

/// `embed/batch` params.
struct EmbedRequest: Decodable {
    var items: [EmbedItem]
    /// `search_query` for queries, `document` for corpus items (default), or another EmbeddingGemma task.
    var task: String?
    /// Matryoshka truncation: 768 (default), 512, 256 or 128.
    var dimensions: Int?
}

/// Owns the loaded EmbeddingGemma 2 checkpoint. Encoders load lazily on first use.
actor EmbeddingService {
    private var model: (any MultimodalEmbedding)?
    private var directory: URL?

    /// Loads a local checkpoint directory (e.g. a snapshot of `mlx-community/embeddinggemma-2-bf16`).
    /// - Parameter directory: Folder containing `config.json`, weights and tokenizer files.
    func load(directory: URL) async throws {
        guard FileManager.default.fileExists(atPath: directory.appendingPathComponent("config.json").path) else {
            throw RPCError.invalidParams("no config.json in \(directory.path)")
        }
        model = try await MultimodalEmbeddingFactory.load(
            modelDirectory: directory, tokenizerLoader: #huggingFaceTokenizerLoader())
        self.directory = directory
        log("loaded embedding model from \(directory.path)")
    }

    /// Releases the model so the GPU memory goes back to the live caption path.
    func unload() {
        model = nil
        directory = nil
    }

    /// Reports whether a model is loaded and which modalities it supports.
    func status() async -> EngineKit.JSONValue {
        guard let model else { return .object(["loaded": .bool(false)]) }
        let modalities = await model.supportedModalities.map { "\($0)" }.sorted()
        return .object([
            "loaded": .bool(true),
            "directory": .string(directory?.path ?? ""),
            "modalities": .array(modalities.map(EngineKit.JSONValue.string)),
        ])
    }

    /// Embeds a batch.
    /// - Parameter request: Items, task and optional Matryoshka dimension.
    /// - Returns: Unit-length vectors, one per item.
    func embed(_ request: EmbedRequest) async throws -> [[Float]] {
        guard let model else { throw RPCError(code: -32020, message: "embedding model not loaded; call model/load first") }
        let dims = request.dimensions ?? 768
        guard [128, 256, 512, 768].contains(dims) else { throw RPCError.invalidParams("dimensions must be 128/256/512/768") }
        var inputs: [EmbeddingInput] = []
        var clips: [URL] = []
        // Temporary window clips are deleted whether embedding succeeds or not.
        defer { Clips.remove(clips) }
        for item in request.items {
            let file = { (p: String) in URL(fileURLWithPath: p) }
            var parts: [EmbeddingInput.Part] = []
            if let text = item.text, !text.isEmpty { parts.append(.text(text)) }
            parts += (item.images ?? []).map { .image(.url(file($0))) }
            for path in item.video ?? [] {
                if let start = item.startMs, let end = item.endMs {
                    let clip = try await Clips.video(file(path), startMs: start, endMs: end)
                    clips.append(clip)
                    parts.append(.video(.url(clip)))
                } else {
                    parts.append(.video(.url(file(path))))
                }
            }
            for path in item.audio ?? [] {
                if let start = item.startMs, let end = item.endMs {
                    let clip = try Clips.audio(file(path), startMs: start, endMs: end)
                    clips.append(clip)
                    parts.append(.audio(.url(clip)))
                } else {
                    parts.append(.audio(.url(file(path))))
                }
            }
            inputs.append(EmbeddingInput(parts, title: item.title))
        }
        let vectors = try await model.embed(inputs, task: Self.task(request.task))
        // Matryoshka: a prefix of the full vector, re-normalised, is a valid lower-dim embedding.
        return dims == 768 ? vectors : vectors.map { v in
            let prefix = Array(v.prefix(dims))
            let norm = prefix.reduce(0) { $0 + $1 * $1 }.squareRoot()
            return norm > 0 ? prefix.map { $0 / norm } : prefix
        }
    }

    private static func task(_ name: String?) -> EmbeddingTask {
        switch name {
        case "search_query": .searchQuery
        case "clustering": .clustering
        case "classification": .classification
        case "sentence_similarity": .sentenceSimilarity
        case "question_answering": .questionAnswering
        default: .document
        }
    }
}
