// swift-tools-version:6.0
import PackageDescription

let package = Package(
    name: "voice-harness-client",
    platforms: [.macOS(.v13)],
    targets: [
        .target(name: "voicekit"),
        .executableTarget(
            name: "VoiceHarnessClient",
            dependencies: ["voicekit"],
            path: "Sources",
            exclude: ["voicekit"]
        ),
        .testTarget(name: "voicekitTests", dependencies: ["voicekit"])
    ]
)
