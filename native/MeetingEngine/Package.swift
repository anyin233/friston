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
                // AudioFileLoader (any AVAudioFile format → 16 kHz mono) for transcribe/file and clips.
                .product(name: "AudioCommon", package: "speech-swift"),
            ],
            linkerSettings: [
                .linkedFramework("ScreenCaptureKit"),
                .linkedFramework("EventKit"),
                .linkedFramework("AVFoundation"),
            ]
        )
    ]
)
