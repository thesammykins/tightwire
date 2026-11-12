// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "Tightwire",
    platforms: [.macOS(.v14)],
    products: [
        .executable(name: "Tightwire", targets: ["Tightwire"]),
        .executable(name: "tightwire-send", targets: ["tightwire-send"]),
    ],
    targets: [
        // The CoreAudio IO callback lives in C so the real-time path provably
        // avoids Swift runtime calls (retain/release, allocation, exclusivity checks).
        .target(name: "TightwireRT", linkerSettings: [.linkedFramework("CoreAudio")]),
        .target(name: "TightwireCore", dependencies: ["TightwireRT"]),
        .executableTarget(name: "Tightwire", dependencies: ["TightwireCore"]),
        .executableTarget(name: "tightwire-send", dependencies: ["TightwireCore"]),
        .testTarget(name: "TightwireCoreTests", dependencies: ["TightwireCore", "TightwireRT"], exclude: ["Fixtures"]),
    ]
)
