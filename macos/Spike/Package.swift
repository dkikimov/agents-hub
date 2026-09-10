// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "Spike",
    platforms: [.macOS(.v14)],
    dependencies: [
        .package(url: "https://github.com/Lakr233/libghostty-spm.git", exact: "1.6.20260909"),
    ],
    targets: [
        .executableTarget(
            name: "Spike",
            dependencies: [.product(name: "GhosttyTerminal", package: "libghostty-spm")],
            swiftSettings: [.swiftLanguageMode(.v5)]
        ),
    ]
)
