// swift-tools-version: 5.9

import PackageDescription

let package = Package(
    name: "TalkViewer",
    platforms: [
        .macOS(.v13),
    ],
    products: [
        .executable(name: "TalkViewer", targets: ["TalkViewer"]),
    ],
    targets: [
        .executableTarget(
            name: "TalkViewer",
            linkerSettings: [
                .linkedFramework("AppKit"),
                .linkedFramework("AVFoundation"),
                .linkedFramework("WebKit"),
            ]
        ),
        .testTarget(
            name: "TalkViewerTests",
            dependencies: ["TalkViewer"]
        ),
    ]
)
