// swift-tools-version:6.0
import PackageDescription

let package = Package(
    name: "voice-harness-client",
    platforms: [.macOS(.v13), .iOS(.v17)],
    products: [
        .library(name: "voicekit", targets: ["voicekit"])
    ],
    targets: [
        .target(
            name: "voicekit",
            exclude: [
                "PersonalContext/EventKitProvider.swift",
                "PersonalContext/ContactsProvider.swift",
                "PersonalContext/PhotosProvider.swift",
            ]
        ),
        .executableTarget(
            name: "VoiceHarnessClient",
            dependencies: ["voicekit"],
            path: "Sources",
            exclude: ["voicekit"]
        ),
        .testTarget(name: "voicekitTests", dependencies: ["voicekit"])
    ]
)
