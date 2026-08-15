// swift-tools-version:5.9
import PackageDescription

// Local Swift package wrapping DataLineFFI.xcframework (the compiled Rust
// engine, universal arm64+x86_64 macOS static lib) and the UniFFI-generated
// Swift bindings on top of it. Add this folder as a local package dependency
// (Xcode: File > Add Package Dependencies > Add Local...) to link DataLine
// from a Swift app. See DataLine/Architecture.md §6/§8 for the API shape.
//
// `.macOS(.v14)` is a placeholder floor, not a DataLine decision — raise it
// (or add `.iOS(...)`) to match whatever the consuming app actually targets.
let package = Package(
    name: "DataLineFFI",
    platforms: [.macOS(.v14)],
    products: [
        .library(name: "DataLineFFI", targets: ["DataLineFFI"])
    ],
    targets: [
        .binaryTarget(name: "DataLineFFIRust", path: "DataLineFFI.xcframework"),
        .target(name: "DataLineFFI", dependencies: ["DataLineFFIRust"], path: "Sources/DataLineFFI"),
        // Smoke test / usage example — drives the real Swift API end-to-end
        // (schema, records, references, blobs), not just a compile check.
        .executableTarget(name: "DataLineSmokeTest", dependencies: ["DataLineFFI"], path: "Sources/DataLineSmokeTest"),
    ]
)
