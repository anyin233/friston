// swift-tools-version: 6.0
// mm-embedd: EmbeddingGemma 2 multimodal embeddings (text / image / audio / video in one
// 768-d space) on MLX. EmbeddingGemma 2 support exists only on mlx-swift-lm `main` so far,
// hence the revision pin; move to a tagged release once one ships it.
import PackageDescription

let package = Package(
    name: "EmbedEngine",
    platforms: [.macOS("15.0")],
    products: [.executable(name: "mm-embedd", targets: ["mm-embedd"])],
    dependencies: [
        .package(path: "../NativeKit"),
        .package(url: "https://github.com/ml-explore/mlx-swift-lm", revision: "c8dabc63e7018d0b1567b2171f8df5dd8a334a55"),
        .package(url: "https://github.com/huggingface/swift-huggingface", from: "0.9.0"),
        .package(url: "https://github.com/huggingface/swift-transformers", from: "1.3.0"),
    ],
    targets: [
        .executableTarget(
            name: "mm-embedd",
            dependencies: [
                .product(name: "EngineKit", package: "NativeKit"),
                .product(name: "MLXVLM", package: "mlx-swift-lm"),
                .product(name: "MLXLMCommon", package: "mlx-swift-lm"),
                .product(name: "MLXHuggingFace", package: "mlx-swift-lm"),
                .product(name: "HuggingFace", package: "swift-huggingface"),
                .product(name: "Tokenizers", package: "swift-transformers"),
            ]
        )
    ]
)
