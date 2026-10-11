// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "TeraRelayFileProvider",
    platforms: [.macOS(.v12)],
    products: [
        .executable(name: "TeraRelayFileProvider", targets: ["TeraRelayFileProvider"]),
    ],
    targets: [
        .executableTarget(
            name: "TeraRelayFileProvider",
            path: "Sources/TeraRelayFileProvider",
            swiftSettings: [
                .swiftLanguageMode(.v5),
                .unsafeFlags(["-application-extension"]),
            ],
            linkerSettings: [
                .linkedFramework("FileProvider"),
                .linkedFramework("UniformTypeIdentifiers"),
                .unsafeFlags([
                    "-Xlinker", "-e", "-Xlinker", "_NSExtensionMain",
                    "-Xlinker", "-application_extension",
                ]),
            ]
        ),
    ]
)
