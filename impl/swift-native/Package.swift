// swift-tools-version: 6.4
import PackageDescription

let package = Package(
    name: "JedSwiftExperiment",
    products: [
        .library(name: "Jed", targets: ["Jed"]),
        .library(name: "JedCrypto", targets: ["JedCrypto"]),
        .executable(name: "jed-unicode", targets: ["JedUnicode"]),
    ],
    dependencies: [
        .package(url: "https://github.com/apple/swift-crypto.git", exact: "5.1.1"),
        // CryptoExtras requires this cryptographic serialization dependency.
        // An exact constraint keeps fresh resolutions reproducible.
        .package(url: "https://github.com/apple/swift-asn1.git", exact: "1.7.4"),
    ],
    targets: [
        .target(name: "CJedHost"),
        .target(name: "Jed", dependencies: ["JedCrypto", "CJedHost"]),
        .target(name: "JedUnicodeCompiler", dependencies: ["Jed"]),
        .executableTarget(name: "JedUnicode", dependencies: ["JedUnicodeCompiler"]),
        .target(
            name: "JedCrypto",
            dependencies: [
                "CArgon2",
                .product(name: "Crypto", package: "swift-crypto"),
                .product(name: "CryptoExtras", package: "swift-crypto"),
            ]
        ),
        .target(
            name: "CArgon2",
            path: "Vendor/argon2",
            exclude: ["LICENSE", "PROVENANCE.json"],
            sources: [
                "src/argon2.c", "src/core.c", "src/encoding.c", "src/ref.c",
                "src/blake2/blake2b.c",
            ],
            publicHeadersPath: "include",
            cSettings: [.define("ARGON2_NO_THREADS"), .headerSearchPath("src")]
        ),
        .testTarget(name: "JedTests", dependencies: ["Jed", "JedUnicodeCompiler"], resources: [.copy("Fixtures")]),
        .testTarget(name: "JedCryptoTests", dependencies: ["JedCrypto"], resources: [.copy("Fixtures")]),
    ],
    swiftLanguageModes: [.v6]
)
