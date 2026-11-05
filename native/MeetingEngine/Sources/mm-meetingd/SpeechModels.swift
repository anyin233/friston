import AudioCommon
import EngineKit
import Foundation
import MeetingCore
@preconcurrency import Qwen3ASR
@preconcurrency import SpeechVAD

/// Model ids reported in `initialize` and stamped on voiceprints.
enum SpeechModelIDs {
    /// Live captions: small and fast (RTF ≈ 0.012 on Apple silicon).
    static let liveASR = Qwen3ASRModel.defaultModelId
    /// `transcribe/file`, preferred from an existing Hugging Face cache (no second download).
    static let offlineASRHub = "mlx-community/Qwen3-ASR-1.7B-8bit"
    /// Fallback for `transcribe/file` when the hub copy is missing; downloaded by speech-swift.
    static let offlineASRDownload = Qwen3ASRModel.largeModelId
    static let vad = SileroVADModel.defaultModelId
    /// Voiceprints carry this id; vectors from other models are never compared.
    static let speakerEmbedding = WeSpeakerModel.defaultModelId
    static let segmentation = PyannoteVADModel.defaultModelId
    /// Live speaker turns: Sortformer streaming (Core ML, 480 ms chunks, up to 4 speakers).
    static let turnDiarizer = SortformerDiarizer.defaultModelId
    /// Chunk shape of the live Sortformer; its lookahead decides how late turns are confirmed.
    static let turnDiarizerConfig = SortformerConfig.streaming
}

/// Loads a model from the local cache without touching the network, and downloads it only
/// on a cache miss. speech-swift otherwise asks the Hugging Face Hub on every load; on a slow
/// or captive network that stalled `transcribe/file` for minutes while holding ``MLXGate``.
/// - Parameters:
///   - id: Model id, for the log line.
///   - load: Loader taking the `offlineMode` flag.
/// - Returns: The loaded model.
/// - Throws: The online loader's error when the model is neither cached nor downloadable.
func loadCacheFirst<T>(_ id: String, _ load: (Bool) async throws -> T) async throws -> T {
    do {
        return try await load(true)
    } catch {
        log("model \(id): not in the local cache (\(error)); downloading")
        return try await load(false)
    }
}

/// Owns a model that is not `Sendable`. Only ever used while holding ``MLXGate``.
final class Unchecked<T>: @unchecked Sendable {
    let value: T

    /// Wraps a value.
    /// - Parameter value: The model.
    init(_ value: T) { self.value = value }
}

/// Serializes all MLX work in the process.
///
/// MLX has one GPU dispatch queue and its graphs are not meant to be evaluated from several
/// threads at once, while live captions, `transcribe/file` and voiceprint requests all run
/// concurrently. Every model call goes through ``run(_:)``, which waits its turn and then runs
/// the synchronous body on a dedicated thread, so the Swift cooperative pool is never blocked
/// by a multi-second decode.
actor MLXGate {
    static let shared = MLXGate()

    private var busy = false
    private var waiters: [CheckedContinuation<Void, Never>] = []
    private let queue = DispatchQueue(label: "mm.speech.mlx", qos: .userInitiated)

    /// Runs synchronous model work exclusively.
    /// - Parameter body: Work touching MLX models.
    /// - Returns: The body's result.
    /// - Throws: Whatever the body throws.
    func run<T: Sendable>(_ body: @escaping @Sendable () throws -> T) async throws -> T {
        await acquire()
        defer { release() }
        return try await withCheckedThrowingContinuation { continuation in
            queue.async { continuation.resume(with: Result { try body() }) }
        }
    }

    /// Runs async model work (loading) exclusively.
    /// - Parameter body: Work that may await, e.g. `fromPretrained`.
    /// - Returns: The body's result.
    /// - Throws: Whatever the body throws.
    func load<T: Sendable>(_ body: @Sendable () async throws -> T) async throws -> T {
        await acquire()
        defer { release() }
        return try await body()
    }

    private func acquire() async {
        if !busy {
            busy = true
            return
        }
        await withCheckedContinuation { waiters.append($0) }
    }

    private func release() {
        if waiters.isEmpty {
            busy = false
        } else {
            // Hand the gate straight to the next waiter; `busy` stays true.
            waiters.removeFirst().resume()
        }
    }
}

/// Errors loading or using speech models.
enum SpeechModelError: Error, CustomStringConvertible {
    case load(model: String, underlying: Error)

    /// Message for logs and RPC errors.
    var description: String {
        switch self {
        case .load(let model, let underlying): "could not load speech model \(model): \(underlying)"
        }
    }
}

/// Lazily loaded, process-wide speech models.
///
/// Live models stay resident once loaded (captions restart instantly on the next capture).
/// The 1.7B offline model is loaded per `transcribe/file` request and released afterwards,
/// because it needs ~2.5 GB that the embedding engine may want back.
actor SpeechModels {
    static let shared = SpeechModels()

    private let live = LazyModel<Qwen3ASRModel>(id: SpeechModelIDs.liveASR) {
        try await loadCacheFirst(SpeechModelIDs.liveASR) { offline in
            try await Qwen3ASRModel.fromPretrained(modelId: SpeechModelIDs.liveASR, offlineMode: offline,
                                                   progressHandler: SpeechModels.progress(SpeechModelIDs.liveASR))
        }
    }
    private let silero = LazyModel<SileroVADModel>(id: SpeechModelIDs.vad) {
        try await loadCacheFirst(SpeechModelIDs.vad) { offline in
            try await SileroVADModel.fromPretrained(modelId: SpeechModelIDs.vad, offlineMode: offline,
                                                    progressHandler: SpeechModels.progress(SpeechModelIDs.vad))
        }
    }
    private let wespeaker = LazyModel<WeSpeakerModel>(id: SpeechModelIDs.speakerEmbedding) {
        try await loadCacheFirst(SpeechModelIDs.speakerEmbedding) { offline in
            try await WeSpeakerModel.fromPretrained(modelId: SpeechModelIDs.speakerEmbedding, offlineMode: offline,
                                                    progressHandler: SpeechModels.progress(SpeechModelIDs.speakerEmbedding))
        }
    }

    private let sortformer = LazyModel<SortformerDiarizer>(id: SpeechModelIDs.turnDiarizer) {
        try await loadCacheFirst(SpeechModelIDs.turnDiarizer) { offline in
            try await SortformerDiarizer.fromPretrained(modelId: SpeechModelIDs.turnDiarizer, offlineMode: offline,
                                                        config: SpeechModelIDs.turnDiarizerConfig,
                                                        progressHandler: SpeechModels.progress(SpeechModelIDs.turnDiarizer))
        }
    }

    /// Logs download/load progress at most every 10 %, on stderr.
    /// - Parameter model: Model id for the log line.
    /// - Returns: A progress handler for `fromPretrained`.
    nonisolated static func progress(_ model: String) -> (Double, String) -> Void {
        let last = LockedValue(-1)
        return { fraction, status in
            let step = Int(fraction * 10)
            if last.exchange(step) != step { log("model \(model): \(Int(fraction * 100))% \(status)") }
        }
    }

    /// Qwen3-ASR 0.6B for live captions.
    /// - Returns: The shared model.
    /// - Throws: ``SpeechModelError`` when download or loading fails.
    func liveASR() async throws -> Unchecked<Qwen3ASRModel> { try await live.get() }

    /// Silero VAD; callers take `makeIndependentStream()` for per-stream state.
    /// - Returns: The shared model (weights only; do not run it directly from two streams).
    /// - Throws: ``SpeechModelError``.
    func vad() async throws -> Unchecked<SileroVADModel> { try await silero.get() }

    /// WeSpeaker ResNet34 (256-d) speaker embeddings.
    /// - Returns: The shared model.
    /// - Throws: ``SpeechModelError``.
    func speaker() async throws -> Unchecked<WeSpeakerModel> { try await wespeaker.get() }

    /// Sortformer with the streaming chunk shape; callers open one
    /// `makeStreamingSession()` per audio stream (sessions share the Core ML model).
    /// - Returns: The shared diarizer.
    /// - Throws: ``SpeechModelError``.
    func turnDiarizer() async throws -> Unchecked<SortformerDiarizer> { try await sortformer.get() }

    /// Loads Qwen3-ASR 1.7B, preferring an existing `huggingface_hub` copy.
    /// - Returns: A fresh model owned by the caller, and the id actually used.
    /// - Throws: ``SpeechModelError``.
    func offlineASR() async throws -> (Unchecked<Qwen3ASRModel>, String) {
        let hub = HubCacheLocator.snapshot(repo: SpeechModelIDs.offlineASRHub)
        let model = hub != nil ? SpeechModelIDs.offlineASRHub : SpeechModelIDs.offlineASRDownload
        do {
            let loaded = try await MLXGate.shared.load {
                Unchecked(try await Qwen3ASRModel.fromPretrained(
                    modelId: model, cacheDir: hub, offlineMode: hub != nil, progressHandler: Self.progress(model)))
            }
            log("model \(model) loaded from \(hub?.path ?? "speech-swift cache")")
            return (loaded, model)
        } catch {
            throw SpeechModelError.load(model: model, underlying: error)
        }
    }

    /// Pyannote segmentation + WeSpeaker clustering with a Silero pre-filter, for `transcribe/file`.
    /// - Returns: A fresh pipeline owned by the caller.
    /// - Throws: ``SpeechModelError``.
    func diarizer() async throws -> Unchecked<PyannoteDiarizationPipeline> {
        do {
            return try await MLXGate.shared.load {
                Unchecked(try await loadCacheFirst(SpeechModelIDs.segmentation) { offline in
                    try await PyannoteDiarizationPipeline.fromPretrained(
                        useVADFilter: true, offlineMode: offline, progressHandler: Self.progress(SpeechModelIDs.segmentation))
                })
            }
        } catch {
            throw SpeechModelError.load(model: SpeechModelIDs.segmentation, underlying: error)
        }
    }
}

/// A model loaded once on first use and then shared; a failed load is retried on the next call.
actor LazyModel<T> {
    let id: String
    private let make: @Sendable () async throws -> T
    private var task: Task<Unchecked<T>, Error>?

    /// Creates the slot; nothing loads yet.
    /// - Parameters:
    ///   - id: Model id for logs and errors.
    ///   - make: Downloads (if needed) and loads the model.
    init(id: String, make: @escaping @Sendable () async throws -> T) {
        self.id = id
        self.make = make
    }

    /// Returns the model, loading it under ``MLXGate`` on first use.
    /// - Returns: The shared model.
    /// - Throws: ``SpeechModelError/load(model:underlying:)``.
    func get() async throws -> Unchecked<T> {
        if let task { return try await task.value }
        let (id, make) = (id, make)
        let task = Task<Unchecked<T>, Error> {
            do {
                let started = Date()
                let value = try await MLXGate.shared.load { Unchecked(try await make()) }
                log("model \(id) ready in \(String(format: "%.1f", Date().timeIntervalSince(started))) s")
                return value
            } catch {
                throw SpeechModelError.load(model: id, underlying: error)
            }
        }
        self.task = task
        do {
            return try await task.value
        } catch {
            self.task = nil
            throw error
        }
    }
}

/// A tiny lock-protected value for callbacks that must be `Sendable`.
final class LockedValue<T: Sendable>: @unchecked Sendable {
    private let lock = NSLock()
    private var value: T

    /// Creates the box.
    /// - Parameter value: Initial value.
    init(_ value: T) { self.value = value }

    /// Stores a new value.
    /// - Parameter new: Value to store.
    /// - Returns: The previous value.
    func exchange(_ new: T) -> T {
        lock.withLock {
            let old = value
            value = new
            return old
        }
    }

    /// Reads the value.
    /// - Returns: The current value.
    func get() -> T { lock.withLock { value } }

    /// Changes the value in place under the lock.
    /// - Parameter change: Mutation to apply.
    func mutate(_ change: (inout T) -> Void) { lock.withLock { change(&value) } }
}
