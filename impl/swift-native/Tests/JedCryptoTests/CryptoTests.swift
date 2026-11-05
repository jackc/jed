import Foundation
import XCTest
import JedCrypto

final class CryptoTests: XCTestCase {
    private func hex(_ text: String) throws -> [UInt8] {
        let input = Array(text.utf8)
        guard input.count % 2 == 0 else { throw FixtureError.invalidHex }
        return try stride(from: 0, to: input.count, by: 2).map { index in
            guard let value = UInt8(String(decoding: input[index...index + 1], as: UTF8.self), radix: 16) else {
                throw FixtureError.invalidHex
            }
            return value
        }
    }

    private enum FixtureError: Error { case invalidHex, invalidLine, missingValue }

    // Minimal reader for this single shared, flat string/integer fixture. This
    // does not generate or implement engine behavior, and rejects unknown syntax.
    private func vectors() throws -> [(String, [String: String])] {
        let url = try XCTUnwrap(Bundle.module.url(
            forResource: "encryption_vectors", withExtension: "toml", subdirectory: "Fixtures"
        ))
        let source = try String(contentsOf: url, encoding: .utf8)
        var sections: [(String, [String: String])] = []
        for raw in source.split(separator: "\n") {
            let line = raw.trimmingCharacters(in: .whitespaces)
            if line.isEmpty || line.hasPrefix("#") || line == "schema_version = 1" { continue }
            if line.hasPrefix("[") {
                sections.append((line.trimmingCharacters(in: CharacterSet(charactersIn: "[]")), [:]))
                continue
            }
            guard let separator = line.firstIndex(of: "="), !sections.isEmpty else { throw FixtureError.invalidLine }
            let name = line[..<separator].trimmingCharacters(in: .whitespaces)
            var value = line[line.index(after: separator)...].trimmingCharacters(in: .whitespaces)
            if value.hasPrefix("\"") && value.hasSuffix("\"") { value.removeFirst(); value.removeLast() }
            sections[sections.count - 1].1[name] = value
        }
        return sections
    }

    private func bytes(_ values: [String: String], _ name: String) throws -> [UInt8] {
        guard let value = values[name] else { throw FixtureError.missingValue }
        return try hex(value)
    }

    func testSharedEncryptionVectors() throws {
        let sections = try vectors()
        let subkeys = try XCTUnwrap(sections.first { $0.0 == "subkeys" }?.1)
        let dek = try bytes(subkeys, "dek_hex")
        for role in ["page", "meta"] {
            XCTAssertEqual(try JedCrypto.hkdfSHA256(inputKeyMaterial: dek, salt: [], info: Array("jed/v1/\(role)".utf8)),
                           try bytes(subkeys, "\(role)_key_hex"))
        }
        let host = try XCTUnwrap(sections.first { $0.0 == "host_kek" }?.1)
        XCTAssertEqual(try JedCrypto.hkdfSHA256(inputKeyMaterial: bytes(host, "key_hex"), salt: bytes(host, "kdf_salt_hex"),
                                               info: Array("jed/v1/kek".utf8)), try bytes(host, "kek_hex"))
        let xaes = sections.filter { $0.0 == "xaes" }
        XCTAssertEqual(xaes.count, 3)
        for (_, values) in xaes {
            let codec = try XAES256GCM(key: bytes(values, "key_hex"))
            let nonce = try bytes(values, "nonce_hex"), plaintext = try bytes(values, "plaintext_hex")
            let aad = try bytes(values, "aad_hex"), expected = try bytes(values, "sealed_hex")
            XCTAssertEqual(try codec.seal(nonce: nonce, plaintext: plaintext, aad: aad), expected, values["name"] ?? "")
            XCTAssertEqual(try codec.open(nonce: nonce, sealed: expected, aad: aad), plaintext)
            var corrupt = expected
            corrupt[corrupt.count - 1] ^= 1
            XCTAssertThrowsError(try codec.open(nonce: nonce, sealed: corrupt, aad: aad)) { error in
                XCTAssertEqual(error as? JedCryptoError, .authenticationFailed)
            }
            XCTAssertThrowsError(try codec.open(nonce: nonce, sealed: expected, aad: aad + [0]))
            var wrongNonce = nonce
            wrongNonce[0] ^= 1
            XCTAssertThrowsError(try codec.open(nonce: wrongNonce, sealed: expected, aad: aad))
        }
        let argon = try XCTUnwrap(sections.first { $0.0 == "argon2id" }?.1)
        XCTAssertEqual(try JedCrypto.argon2id(
            password: Array(try XCTUnwrap(argon["password"]).utf8), salt: Array(try XCTUnwrap(argon["salt"]).utf8),
            memoryKiB: UInt32(try XCTUnwrap(argon["memory_kib"]))!,
            iterations: UInt32(try XCTUnwrap(argon["iterations"]))!,
            parallelism: UInt32(try XCTUnwrap(argon["parallelism"]))!
        ), try bytes(argon, "key_hex"))

        let initial = try XCTUnwrap(sections.first { $0.0 == "key_block" }?.1)
        let rewrap = try XCTUnwrap(sections.first { $0.0 == "rewrap" }?.1)
        let wrappedKey = try bytes(initial, "dek_hex")
        for (fields, credential) in [(initial, try bytes(host, "key_hex")), (rewrap, try bytes(rewrap, "key_hex"))] {
            let block = try bytes(fields, "block_hex")
            let kek = try JedCrypto.hkdfSHA256(inputKeyMaterial: credential, salt: Array(block[32..<64]), info: Array("jed/v1/kek".utf8))
            let sealed = try JedCrypto.gcmSeal(key: kek, nonce: Array(block[64..<76]), plaintext: wrappedKey, aad: Array(block[..<76]))
            XCTAssertEqual(sealed, Array(block[76..<124]))
            XCTAssertEqual(try JedCrypto.gcmOpen(key: kek, nonce: Array(block[64..<76]), sealed: sealed, aad: Array(block[..<76])), wrappedKey)
        }
    }

    func testDigestHostAPI() throws {
        // Verifies the independent raw-byte host boundary, including SHA-224's C
        // bridge. SQL overloads, NULL behavior, text encoding, and costs belong
        // to the shared hash_functions.test corpus.
        let abc = Array("abc".utf8)
        XCTAssertEqual(JedCrypto.md5(abc), try hex("900150983cd24fb0d6963f7d28e17f72"))
        XCTAssertEqual(JedCrypto.sha224(abc), try hex("23097d223405d8228642a477bda255b32aadbce4bda0b3f7e36c9da7"))
        XCTAssertEqual(JedCrypto.sha256(abc), try hex("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"))
        XCTAssertEqual(JedCrypto.sha384(abc), try hex("cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7"))
        XCTAssertEqual(JedCrypto.sha512(abc), try hex("ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"))
        XCTAssertEqual(JedCrypto.sha224([]), try hex("d14a028c2a3a2bc9476102bb288234c415a2b01f828ea62ac5b3e42f"))
        XCTAssertNotEqual(JedCrypto.sha224([0]), JedCrypto.sha224([]))
    }

    func testAESBlockKnownAnswer() throws {
        XCTAssertEqual(try JedCrypto.aesBlock(key: hex("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"),
                                              block: hex("00112233445566778899aabbccddeeff")),
                       try hex("8ea2b7ca516745bfeafc49904b496089"))
    }

    func testEmptyAndEmbeddedZeroInputs() throws {
        let key = [UInt8](repeating: 4, count: 32), nonce = [UInt8](repeating: 3, count: 12)
        let sealed = try JedCrypto.gcmSeal(key: key, nonce: nonce, plaintext: [], aad: [])
        XCTAssertEqual(sealed.count, 16)
        XCTAssertEqual(try JedCrypto.gcmOpen(key: key, nonce: nonce, sealed: sealed, aad: []), [])
        let salt = Array("somesalt".utf8)
        let empty = try JedCrypto.argon2id(password: [], salt: salt, memoryKiB: 32, iterations: 1, parallelism: 4)
        let zero = try JedCrypto.argon2id(password: [0], salt: salt, memoryKiB: 32, iterations: 1, parallelism: 4)
        XCTAssertEqual(empty.count, 32)
        XCTAssertNotEqual(empty, zero)
        XCTAssertNotEqual(try JedCrypto.argon2id(password: [0, 1], salt: salt, memoryKiB: 32, iterations: 1, parallelism: 4), zero)
    }

    func testSerialArgon2BackendPreservesMultipleLanes() throws {
        // Official upstream 20190702 src/test.c Argon2id v19 vector: the
        // ARGON2_NO_THREADS build must preserve p=2, not silently force p=1.
        XCTAssertEqual(try JedCrypto.argon2id(
            password: Array("password".utf8), salt: Array("somesalt".utf8),
            memoryKiB: 256, iterations: 2, parallelism: 2
        ), try hex("6d093c501fd5999645e0ea3bf620d7b8be7fd2db59c20d9fff9539da2bf57037"))
    }

    func testRejectedParametersDoNotReachBackend() throws {
        let key = [UInt8](repeating: 0, count: 32)
        XCTAssertThrowsError(try JedCrypto.aesBlock(key: [], block: []))
        XCTAssertThrowsError(try JedCrypto.aesBlock(key: key, block: [0]))
        XCTAssertThrowsError(try JedCrypto.gcmSeal(key: key, nonce: [], plaintext: [], aad: []))
        XCTAssertThrowsError(try JedCrypto.gcmOpen(key: key, nonce: [UInt8](repeating: 0, count: 12), sealed: [], aad: []))
        XCTAssertThrowsError(try XAES256GCM(key: []).seal(nonce: [], plaintext: [], aad: []))
        XCTAssertThrowsError(try XAES256GCM(key: key).seal(nonce: [], plaintext: [], aad: []))
        for count in [Int.min, -1, 0, 8161, Int.max] {
            XCTAssertThrowsError(try JedCrypto.hkdfSHA256(inputKeyMaterial: key, salt: [], info: [], outputByteCount: count))
        }
        let salt = [UInt8](repeating: 0, count: 32)
        for (m, t, p): (UInt32, UInt32, UInt32) in [(0, 1, 1), (8, 0, 1), (8, 1, 0), (1_048_577, 1, 1), (128, 17, 1), (128, 1, 17), (UInt32.max, UInt32.max, UInt32.max)] {
            XCTAssertThrowsError(try JedCrypto.argon2id(password: [], salt: salt, memoryKiB: m, iterations: t, parallelism: p))
        }
        XCTAssertThrowsError(try JedCrypto.argon2id(password: [], salt: [], memoryKiB: 8, iterations: 1, parallelism: 1))
    }
}
