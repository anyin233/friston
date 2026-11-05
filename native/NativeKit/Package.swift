// swift-tools-version: 6.0
// NativeKit: dependency-free building blocks shared by the native engines.
//  - EngineKit:   newline-delimited JSON-RPC 2.0 server over stdio (same framing as Codex app-server).
//  - MeetingCore: DSP and speaker logic that must stay testable without MLX (GCC-PHAT DOA,
//                 voiceprint bank, spatial + timbre speaker fusion, audio input probing).
import PackageDescription

let package = Package(
    name: "NativeKit",
    platforms: [.macOS("15.0")],
    products: [
        .library(name: "EngineKit", targets: ["EngineKit"]),
        .library(name: "MeetingCore", targets: ["MeetingCore"]),
    ],
    targets: [
        .target(name: "EngineKit"),
        .target(name: "MeetingCore", linkerSettings: [.linkedFramework("Accelerate"), .linkedFramework("CoreAudio")]),
        .testTarget(name: "EngineKitTests", dependencies: ["EngineKit"]),
        .testTarget(name: "MeetingCoreTests", dependencies: ["MeetingCore"]),
    ]
)
