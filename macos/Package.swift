// swift-tools-version: 6.0
import PackageDescription

// AgentsHubCore deliberately depends on nothing: no Ghostty, no AppKit, no GPU. That is
// what lets `swift test` run the ported logic in a couple of seconds, the same way
// tree.rs and input.rs stay pure on the Rust side.
let package = Package(
    name: "AgentsHub",
    platforms: [.macOS(.v14)],
    products: [
        .library(name: "AgentsHubCore", targets: ["AgentsHubCore"]),
        .executable(name: "AgentsHub", targets: ["AgentsHub"]),
    ],
    dependencies: [
        // Pinned exactly: libghostty's embedding API is officially unstable and this
        // package's date-stamped tags land roughly weekly.
        .package(url: "https://github.com/Lakr233/libghostty-spm.git", exact: "1.6.20260909"),
    ],
    targets: [
        .target(
            name: "AgentsHubCore",
            swiftSettings: [.swiftLanguageMode(.v5)]
        ),
        .executableTarget(
            name: "AgentsHub",
            dependencies: [
                "AgentsHubCore",
                .product(name: "GhosttyTerminal", package: "libghostty-spm"),
            ],
            swiftSettings: [.swiftLanguageMode(.v5)]
        ),
        .testTarget(
            name: "AgentsHubCoreTests",
            dependencies: ["AgentsHubCore"],
            swiftSettings: [.swiftLanguageMode(.v5)]
        ),
    ]
)
