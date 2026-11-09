import AudioCommon
import CSherpaOnnx
import EngineKit
import Foundation

/// Streaming Zipformer (sherpa-onnx, CPU) used as the live caption *draft* layer.
///
/// Like Foresight's streaming encoder, it emits text every 320 ms chunk and almost never
/// rewrites what it already showed (0.1–0.3 % of characters on AISHELL-1 / WenetSpeech
/// meetings), so captions grow while people speak. It is less accurate than Qwen3-ASR
/// (WenetSpeech meeting CER 9.85 % against 6.19 %), so finals still come from Qwen3-ASR and
/// replace the draft (docs/asr-model-survey.md §9). Runs on the CPU, leaving the GPU to MLX.
///
/// Weights are shared; each caption stream opens its own ``DraftStream``. Not thread-safe:
/// call it from the caption loop only.
final class DraftModel: @unchecked Sendable {
    /// Hugging Face repo of the int8 model (Apache-2.0, from icefall `multi_zh-hans` zipformer-large).
    static let modelId = "csukuangfj/sherpa-onnx-streaming-zipformer-zh-int8-2025-06-30"
    static let files = ["encoder.int8.onnx", "decoder.onnx", "joiner.int8.onnx", "tokens.txt"]
    /// Two threads keep a 100 ms step under ~20 ms on an M5 Pro without starving capture.
    static let threads: Int32 = 2

    private let recognizer: OpaquePointer

    /// Loads the recognizer from a directory holding ``files``.
    /// - Parameter directory: Model directory.
    /// - Throws: ``DraftModelError`` when a file is missing or sherpa-onnx rejects the model.
    init(directory: URL) throws {
        for name in Self.files where !FileManager.default.fileExists(atPath: directory.appendingPathComponent(name).path) {
            throw DraftModelError.missingFile(directory.appendingPathComponent(name).path)
        }
        // sherpa-onnx copies every string into its C++ config, so these only need to live
        // until SherpaOnnxCreateOnlineRecognizer returns.
        var owned: [UnsafeMutablePointer<CChar>] = []
        defer { owned.forEach { free($0) } }
        func c(_ s: String) -> UnsafePointer<CChar> {
            let p = strdup(s)!
            owned.append(p)
            return UnsafePointer(p)
        }
        func path(_ name: String) -> UnsafePointer<CChar> { c(directory.appendingPathComponent(name).path) }

        var config = SherpaOnnxOnlineRecognizerConfig()
        config.feat_config.sample_rate = 16_000
        config.feat_config.feature_dim = 80
        config.model_config.transducer.encoder = path("encoder.int8.onnx")
        config.model_config.transducer.decoder = path("decoder.onnx")
        config.model_config.transducer.joiner = path("joiner.int8.onnx")
        config.model_config.tokens = path("tokens.txt")
        config.model_config.num_threads = Self.threads
        config.model_config.provider = c("cpu")
        config.decoding_method = c("greedy_search")
        // Utterance boundaries come from Silero VAD and the segmenter, not sherpa's endpointing.
        config.enable_endpoint = 0
        guard let recognizer = SherpaOnnxCreateOnlineRecognizer(&config) else {
            throw DraftModelError.rejected(directory.path)
        }
        self.recognizer = recognizer
    }

    deinit { SherpaOnnxDestroyOnlineRecognizer(recognizer) }

    /// Opens a decoding stream (one per utterance; cheap).
    /// - Returns: The stream.
    func makeStream() -> DraftStream { DraftStream(recognizer: recognizer) }

    /// Downloads the model on a cache miss and loads it.
    /// - Returns: The model.
    /// - Throws: Download errors from speech-swift, or ``DraftModelError``.
    static func load() async throws -> DraftModel {
        // MM_DRAFT_DIR points at a local model folder with the same four files, for comparing
        // other streaming transducers (e.g. X-ASR) without changing the default.
        if let dir = ProcessInfo.processInfo.environment["MM_DRAFT_DIR"], !dir.isEmpty {
            log("draft model: loading from MM_DRAFT_DIR \(dir)")
            return try DraftModel(directory: URL(fileURLWithPath: dir))
        }
        let directory = try HuggingFaceDownloader.getCacheDirectory(for: modelId)
        try await loadCacheFirst(modelId) { offline in
            try await HuggingFaceDownloader.downloadFiles(modelId: modelId, to: directory, files: files,
                                                          offlineMode: offline)
        }
        return try DraftModel(directory: directory)
    }
}

/// One utterance being decoded by the draft model.
final class DraftStream {
    private let recognizer: OpaquePointer
    private let stream: OpaquePointer

    /// Creates a stream on `recognizer` (owned by the ``DraftModel``, which must outlive it).
    fileprivate init(recognizer: OpaquePointer) {
        self.recognizer = recognizer
        stream = SherpaOnnxCreateOnlineStream(recognizer)
    }

    deinit { SherpaOnnxDestroyOnlineStream(stream) }

    /// Feeds 16 kHz audio and decodes every chunk that became ready.
    /// - Parameter samples: New mono samples, contiguous with earlier ones.
    /// - Returns: The text so far.
    func accept(_ samples: [Float]) -> String {
        guard !samples.isEmpty else { return text }
        samples.withUnsafeBufferPointer { buf in
            SherpaOnnxOnlineStreamAcceptWaveform(stream, 16_000, buf.baseAddress, Int32(buf.count))
        }
        while SherpaOnnxIsOnlineStreamReady(recognizer, stream) != 0 {
            SherpaOnnxDecodeOnlineStream(recognizer, stream)
        }
        return text
    }

    /// The decoded text so far.
    var text: String {
        guard let result = SherpaOnnxGetOnlineStreamResult(recognizer, stream) else { return "" }
        defer { SherpaOnnxDestroyOnlineRecognizerResult(result) }
        guard let cText = result.pointee.text else { return "" }
        return String(cString: cText).trimmingCharacters(in: .whitespacesAndNewlines)
    }
}

/// Why the draft model could not be loaded.
enum DraftModelError: Error, CustomStringConvertible {
    case missingFile(String)
    case rejected(String)

    var description: String {
        switch self {
        case .missingFile(let path): "draft model file missing: \(path)"
        case .rejected(let dir): "sherpa-onnx could not load the draft model in \(dir)"
        }
    }
}
