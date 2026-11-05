import EngineKit
import Foundation
import MLXGuidedGeneration
import MLXHuggingFace
import MLXLLM
import MLXLMCommon
import Tokenizers

/// `llm/generate` params.
struct GenerateRequest: Decodable {
    var system: String?
    var prompt: String
    /// JSON Schema (as a JSON value) that constrains the output; free text when absent.
    var jsonSchema: EngineKit.JSONValue?
    var maxTokens: Int?
}

/// On-device text generation for offline minutes. Lives in mm-embedd because it needs
/// the same `mlx-swift-lm` revision, and so the embedding model and the LLM share one
/// MLX process that the app can unload as a whole when captions need the GPU.
actor LocalLLM {
    private var container: ModelContainer?
    private var directory: URL?

    /// Loads an MLX language model from a local folder (e.g. a snapshot of an
    /// `mlx-community/*-4bit` instruct model).
    /// - Parameter directory: Folder with `config.json`, weights and tokenizer files.
    /// - Throws: `RPCError.invalidParams` naming the folder when it is not a model.
    func load(directory: URL) async throws {
        guard FileManager.default.fileExists(atPath: directory.appendingPathComponent("config.json").path) else {
            throw RPCError.invalidParams("no config.json in \(directory.path)")
        }
        container = try await LLMModelFactory.shared.loadContainer(
            from: directory, using: #huggingFaceTokenizerLoader())
        self.directory = directory
        log("loaded language model from \(directory.path)")
    }

    /// Releases the model.
    func unload() {
        container = nil
        directory = nil
    }

    /// Reports whether a model is loaded.
    func status() -> EngineKit.JSONValue {
        .object(["loaded": .bool(container != nil), "directory": .string(directory?.path ?? "")])
    }

    /// Generates a reply.
    /// - Parameter request: System/user text, optional JSON Schema, token limit.
    /// - Returns: The generated text (valid JSON when a schema was given).
    /// - Throws: `RPCError` -32021 when no model is loaded; generation errors with context.
    func generate(_ request: GenerateRequest) async throws -> String {
        guard let container else {
            throw RPCError(code: -32021, message: "language model not loaded; call llm/load first")
        }
        let maxTokens = request.maxTokens ?? 4096
        let system = request.system ?? ""
        let prompt = request.prompt

        guard let schemaValue = request.jsonSchema else {
            let session = ChatSession(container, instructions: request.system)
            return try await session.respond(to: request.prompt)
        }
        let schemaData = try JSONEncoder().encode(schemaValue)
        let schema = String(decoding: schemaData, as: UTF8.self)
        // Guided generation masks logits token by token, so the reply always parses.
        return try await container.perform { context in
            let tokenizer = context.tokenizer
            let vocab = TokenizerVocabExtractor.extractForGrammar(from: tokenizer)
            let grammarTokenizer = try GrammarTokenizer(
                vocab: vocab.vocab, vocabType: vocab.vocabType, eosTokenId: Int32(tokenizer.eosTokenId ?? 0))
            let constraint = try GrammarConstraint(
                tokenizer: grammarTokenizer, jsonSchema: schema, fastForward: true, hostTokenizer: tokenizer)
            // UserInput is not Sendable, so it is built inside the isolated closure from plain strings.
            var messages: [Chat.Message] = []
            if !system.isEmpty { messages.append(.system(system)) }
            messages.append(.user(prompt))
            let prepared = try await context.processor.prepare(input: UserInput(chat: messages))
            var output = ""
            do {
                try GuidedGenerationLoop.run(
                    input: prepared, context: context, constraint: constraint,
                    maxTokens: maxTokens, vocabSize: grammarTokenizer.vocabSize
                ) { delta in
                    output += delta
                    return true
                }
            } catch {
                throw RPCError(code: -32000, message: "guided generation failed after \(output.count) chars: \(error)")
            }
            return output
        }
    }
}
