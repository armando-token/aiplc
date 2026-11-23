// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "PlcAutoIpad",
    platforms: [
        .iOS(.v17),
        .macOS(.v14)
    ],
    products: [
        .library(name: "PlcAutoCore", targets: ["PlcAutoCore"]),
        .library(name: "PlcAutoUI", targets: ["PlcAutoUI"])
    ],
    targets: [
        .target(
            name: "PlcAutoCore",
            path: "Sources/PlcAutoCore"
        ),
        .target(
            name: "PlcAutoUI",
            dependencies: ["PlcAutoCore"],
            path: "Sources/PlcAutoUI"
        ),
        .testTarget(
            name: "PlcAutoCoreTests",
            dependencies: ["PlcAutoCore"],
            path: "Tests/PlcAutoCoreTests"
        )
    ]
)
