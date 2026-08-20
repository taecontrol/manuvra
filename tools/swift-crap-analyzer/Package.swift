// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "SwiftCrapAnalyzer",
    platforms: [.macOS(.v15)],
    dependencies: [
        .package(
            url: "https://github.com/swiftlang/swift-syntax.git",
            exact: "603.0.2"
        ),
    ],
    targets: [
        .executableTarget(
            name: "swift-crap-analyzer",
            dependencies: [
                .product(name: "SwiftParser", package: "swift-syntax"),
                .product(name: "SwiftSyntax", package: "swift-syntax"),
            ]
        ),
    ]
)
