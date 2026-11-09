// swift-tools-version: 6.0
// mm-meetingd: capture (ScreenCaptureKit / AVFoundation / Core Audio taps), streaming ASR,
// diarization, voiceprints, spatial cues and calendar. Speech models come from speech-swift
// (MLX + Core ML / Neural Engine). Kept separate from mm-embedd because speech-swift pins
// mlx-swift-lm 3.31.4 while EmbeddingGemma 2 needs mlx-swift-lm main.
import PackageDescription

let package = Package(
    name: "MeetingEngine",
    platforms: [.macOS("15.0")],
    products: [.executable(name: "mm-meetingd", targets: ["mm-meetingd"])],
    dependencies: [
        .package(path: "../NativeKit"),
        .package(url: "https://github.com/soniqo/speech-swift", exact: "0.0.29"),
        // Same version speech-swift resolves; listed only so the engine can free MLX's buffer cache.
        .package(url: "https://github.com/ml-explore/mlx-swift", exact: "0.31.6"),
    ],
    targets: [
        .executableTarget(
            name: "mm-meetingd",
            dependencies: [
                .product(name: "EngineKit", package: "NativeKit"),
                .product(name: "MeetingCore", package: "NativeKit"),
                // Qwen3-ASR (zh/en/ja/ko + 22 Chinese dialects) and Silero / Sortformer / WeSpeaker.
                .product(name: "Qwen3ASR", package: "speech-swift"),
                .product(name: "SpeechVAD", package: "speech-swift"),
                .product(name: "MLX", package: "mlx-swift"),
                // AudioFileLoader (any AVAudioFile format → 16 kHz mono) for transcribe/file and clips.
                .product(name: "AudioCommon", package: "speech-swift"),
                // Streaming Zipformer draft captions (CPU, ONNX Runtime); see docs/asr-model-survey.md §9.
                "CSherpaOnnx",
            ],
            linkerSettings: [
                .linkedFramework("ScreenCaptureKit"),
                .linkedFramework("EventKit"),
                .linkedFramework("AVFoundation"),
                // ONNX Runtime's static library needs these system frameworks.
                .linkedFramework("Foundation"),
                .linkedFramework("CoreML"),
            ]
        ),
        // sherpa-onnx C API as a static xcframework, built by scripts/fetch-sherpa-onnx.sh
        // (pinned version and checksums; not committed).
        .binaryTarget(name: "CSherpaOnnx", path: "Vendor/CSherpaOnnx.xcframework"),
    ]
)
