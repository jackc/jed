import Foundation
import XCTest
import JedCrypto
@testable import Jed

final class EncryptionTests: XCTestCase {
    private func vector(_ name: String) throws -> [String: Any] {
        let url = try XCTUnwrap(Bundle.module.url(forResource: "SpecSnapshot", withExtension: "json", subdirectory: "Fixtures"))
        let snapshot = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
        let documents = try XCTUnwrap(snapshot["documents"] as? [String: [String: Any]])
        let vectors = try XCTUnwrap(documents["spec/fileformat/encryption_vectors.toml"])
        return try XCTUnwrap(vectors[name] as? [String: Any])
    }

    private func hex(_ text: String) -> [UInt8] {
        stride(from: 0, to: text.utf8.count, by: 2).map { index in
            let start = text.index(text.startIndex, offsetBy: index)
            return UInt8(text[start..<text.index(start, offsetBy: 2)], radix: 16)!
        }
    }

    private func bytes(_ row: [String: Any], _ name: String) throws -> [UInt8] {
        hex(try XCTUnwrap(row[name] as? String))
    }

    private func expect(_ code: String, file: StaticString = #filePath, line: UInt = #line,
                        _ body: () throws -> Void) {
        XCTAssertThrowsError(try body(), file: file, line: line) { error in
            XCTAssertEqual((error as? JedError)?.code, code, "\(error)", file: file, line: line)
        }
    }

    private func padded(_ record: [UInt8]) -> [UInt8] { record + [UInt8](repeating: 0, count: 1024 - record.count) }

    private func checksumKeyBlock(_ page: inout [UInt8]) throws {
        try PageEnvelope.putU32(Checksums.crc32(page.prefix(124)), into: &page, at: 124)
    }

    func testCompleteKeyBlockAndRewrapSharedVectors() throws {
        let initial = try vector("key_block"), replacement = try vector("rewrap")
        let host = try Credential.hostKey(bytes(vector("host_kek"), "key_hex"))
        let source = SeededKeyBytes(seed: try XCTUnwrap(initial["seed"] as? NSNumber).uint64Value)
        let created = try KeyBlock.create(credential: host, entropy: source.fill)
        XCTAssertEqual(created.block.record, try bytes(initial, "block_hex"))
        XCTAssertEqual(created.dataKey, try bytes(initial, "dek_hex"))
        XCTAssertEqual(source.drawSizes, [32, 32, 12])
        let encoded = try created.block.encode(pageSize: 1024)
        let parsed = try KeyBlock.parse(encoded, pageSize: 1024)
        XCTAssertEqual(try parsed.unlock(host), created.dataKey)
        let selected = try KeyBlock.select(page2: encoded, page3: encoded, pageSize: 1024)
        XCTAssertEqual(selected.selectedPage, 2)
        XCTAssertEqual(selected.unselectedPage, 3)
        XCTAssertFalse(selected.needsRepair)

        let secondKey = try Credential.hostKey(bytes(replacement, "key_hex"))
        let secondSource = SeededKeyBytes(seed: try XCTUnwrap(replacement["seed"] as? NSNumber).uint64Value)
        let rewrapped = try parsed.rewrap(dataKey: created.dataKey, credential: secondKey, entropy: secondSource.fill)
        XCTAssertEqual(rewrapped.record, try bytes(replacement, "block_hex"))
        XCTAssertEqual(secondSource.drawSizes, [32, 12])
        XCTAssertEqual(try rewrapped.unlock(secondKey), created.dataKey)
        try selected.validateReplacement(rewrapped)
        expect("55006") { try selected.validateReplacement(parsed) }

        let advanced = try KeyBlock.select(page2: encoded, page3: rewrapped.encode(pageSize: 1024), pageSize: 1024)
        XCTAssertEqual(advanced.selectedPage, 3)
        XCTAssertTrue(advanced.needsRepair)
        expect("28P01") { _ = try advanced.block.unlock(host) }
        XCTAssertEqual(try advanced.block.unlock(secondKey), created.dataKey)
        let reverse = try KeyBlock.select(page2: rewrapped.encode(pageSize: 1024), page3: encoded, pageSize: 1024)
        XCTAssertEqual(reverse.selectedPage, 2)
    }

    func testKeyBlockCorruptionSelectionAndCredentialFailures() throws {
        let valid = padded(try bytes(vector("key_block"), "block_hex"))
        let key = try Credential.hostKey(bytes(vector("host_kek"), "key_hex"))
        let parsed = try KeyBlock.parse(valid, pageSize: 1024)
        expect("28P02") { _ = try parsed.unlock(nil) }
        expect("28P01") { _ = try parsed.unlock(.passphrase("wrong kind")) }
        expect("28P01") { _ = try parsed.unlock(Credential.hostKey([UInt8](repeating: 0, count: 32))) }

        // Recompute CRC to prove each structural check is independent of it.
        for offset in [0, 4, 6, 7, 16, 20, 24, 28, 128, 1023] {
            var bad = valid; bad[offset] ^= 0x80
            try checksumKeyBlock(&bad)
            expect("XX001") { _ = try KeyBlock.parse(bad, pageSize: 1024) }
            let selected = try KeyBlock.select(page2: bad, page3: valid, pageSize: 1024)
            XCTAssertEqual(selected.selectedPage, 3)
            XCTAssertTrue(selected.needsRepair)
            expect("XX001") { _ = try KeyBlock.select(page2: bad, page3: bad, pageSize: 1024) }
        }
        var zeroGeneration = valid
        zeroGeneration.replaceSubrange(8..<16, with: repeatElement(UInt8(0), count: 8))
        try checksumKeyBlock(&zeroGeneration)
        expect("XX001") { _ = try KeyBlock.parse(zeroGeneration, pageSize: 1024) }
        expect("XX001") { _ = try KeyBlock.parse(Array(valid.dropLast()), pageSize: 1024) }
        expect("XX001") { _ = try KeyBlock.parse(Array(valid.prefix(512)), pageSize: 512) }

        var conflict = valid; conflict[32] ^= 1; try checksumKeyBlock(&conflict)
        expect("XX001") { _ = try KeyBlock.select(page2: valid, page3: conflict, pageSize: 1024) }
        var damagedWrap = valid; damagedWrap[76] ^= 1; try checksumKeyBlock(&damagedWrap)
        expect("28P01") { _ = try KeyBlock.parse(damagedWrap, pageSize: 1024).unlock(key) }
        var badCRC = valid; badCRC[124] ^= 1
        XCTAssertEqual(try KeyBlock.select(page2: valid, page3: badCRC, pageSize: 1024).selectedPage, 2)

        var exhausted = valid
        exhausted.replaceSubrange(8..<16, with: repeatElement(UInt8(255), count: 8))
        try checksumKeyBlock(&exhausted)
        expect("XX001") {
            _ = try KeyBlock.parse(exhausted, pageSize: 1024).rewrap(dataKey: Array(repeating: 1, count: 32),
                credential: key, entropy: { _ in XCTFail("must reject before entropy"); return [] })
        }
    }

    func testPassphraseBytesAndKDFBounds() throws {
        let parameters = try Argon2Parameters(memoryKiB: 32, iterations: 1, parallelism: 4)
        for password in ["", "a\0b", "\u{00e9}", "e\u{0301}"] {
            let source = SeededKeyBytes(seed: 99)
            let credential = Credential.passphrase(password)
            let created = try KeyBlock.create(credential: credential, parameters: parameters, entropy: source.fill)
            let block = try KeyBlock.parse(created.block.encode(pageSize: 1024), pageSize: 1024)
            XCTAssertEqual(try block.unlock(credential), created.dataKey)
            expect("28P01") { _ = try block.unlock(.passphrase(password + "x")) }
        }
        let composed = try KeyBlock.create(credential: .passphrase("\u{00e9}"), parameters: parameters,
                                           entropy: SeededKeyBytes(seed: 99).fill)
        expect("28P01") { _ = try composed.block.unlock(.passphrase("e\u{0301}")) }
        let invalidParameters: [(UInt32, UInt32, UInt32)] = [(31, 1, 4), (32, 0, 4), (32, 17, 4), (32, 1, 0), (32, 1, 17), (1_048_577, 1, 1)]
        for (m, t, p) in invalidParameters {
            expect("22023") { _ = try Argon2Parameters(memoryKiB: m, iterations: t, parallelism: p) }
        }
        // A hostile passphrase header never reaches the KDF allocation path.
        var hostile = padded(try bytes(vector("key_block"), "block_hex"))
        hostile[7] = 1
        try PageEnvelope.putU32(.max, into: &hostile, at: 16)
        try PageEnvelope.putU32(1, into: &hostile, at: 20)
        try PageEnvelope.putU32(1, into: &hostile, at: 24)
        try checksumKeyBlock(&hostile)
        expect("XX001") { _ = try KeyBlock.parse(hostile, pageSize: 1024) }
    }

    func testCredentialPolicyAndEntropyBoundary() throws {
        let credential = Credential.passphrase("secret")
        XCTAssertFalse(String(describing: credential).contains("secret"))
        XCTAssertFalse(String(reflecting: credential).contains("secret"))
        expect("22023") { _ = try Credential.hostKey([]) }
        expect("28P02") { try EncryptionPolicy.checkCredential(encrypted: true, credential: nil) }
        expect("28P03") { try EncryptionPolicy.checkCredential(encrypted: false, credential: credential) }
        expect("55000") { try EncryptionPolicy.checkCredential(encrypted: false, inMemory: true, credential: credential) }
        try EncryptionPolicy.checkCredential(encrypted: false, credential: nil)
        try EncryptionPolicy.checkCredential(encrypted: true, credential: credential)
        expect("58030") { _ = try KeyBlock.create(credential: credential, entropy: { _ in [] }) }
        expect("0A000") { _ = try EncryptedPageCodec(pageSize: 512, dataKey: Array(repeating: 1, count: 32)) }
        expect("XX001") { _ = try EncryptedPageCodec(pageSize: 512, dataKey: Array(repeating: 1, count: 32), opening: true) }
    }

    func testBodyAuthenticationBoundsAndReplay() throws {
        let key = Array(UInt8(0)...UInt8(31))
        let codec = try EncryptedPageCodec(pageSize: 1024, dataKey: key)
        XCTAssertEqual(codec.capacity, 968)
        var clear = [UInt8](repeating: 0, count: 984); clear[0] = 2; clear[16] = 90
        let nonce = Array(UInt8(0)...UInt8(23))
        let first = try codec.sealBody(pageIndex: 4, plaintext: clear, salt: nonce)
        let opened = try codec.openBody(pageIndex: 4, sealed: first.bytes, expectedTag: first.tag)
        XCTAssertEqual(Array(opened.prefix(12)), Array(clear.prefix(12)))
        XCTAssertEqual(Array(opened.dropFirst(16)), Array(clear.dropFirst(16)))
        XCTAssertEqual(try PageEnvelope.readU32(opened, at: 12), try PageEnvelope.checksum(opened, at: 12))
        XCTAssertEqual(try codec.sealBody(pageIndex: 4, plaintext: opened, salt: nonce).bytes, first.bytes)
        let newer = try codec.sealBody(pageIndex: 4, plaintext: clear, salt: Array(repeating: 4, count: 24))
        expect("XX001") { _ = try codec.openBody(pageIndex: 4, sealed: first.bytes, expectedTag: newer.tag) }
        expect("XX001") { _ = try codec.openBody(pageIndex: 5, sealed: first.bytes) }
        expect("XX001") { _ = try codec.openBody(pageIndex: 4, sealed: first.bytes, expectedTag: []) }
        expect("XX001") { _ = try codec.openBody(pageIndex: 4, sealed: Array(first.bytes.dropLast())) }
        for offset in [0, 983, 984, 1007, 1008, 1023] {
            var damaged = first.bytes; damaged[offset] ^= 1
            expect("XX001") { _ = try codec.openBody(pageIndex: 4, sealed: damaged) }
        }
        let pageKey = try JedCrypto.hkdfSHA256(inputKeyMaterial: key, salt: [], info: Array("jed/v1/page".utf8))
        for offset in [1, 2, 3, 8, 12] {
            var invalid = clear; invalid[offset] = 1
            let sealed = try JedCrypto.xaesSeal(key: pageKey, nonce: nonce, plaintext: invalid,
                                                aad: hex("4a454442010100000004"))
            let stored = Array(sealed.dropLast(16)) + nonce + sealed.suffix(16)
            expect("XX001") { _ = try codec.openBody(pageIndex: 4, sealed: stored) }
        }
        clear[0] = 8
        expect("XX001") { _ = try codec.sealBody(pageIndex: 4, plaintext: clear, salt: nonce) }
    }

    func testPlainPageChecksumBoundary() throws {
        let codec = try PlainPageCodec(pageSize: 256)
        var page = [UInt8](repeating: 0, count: 256); page[0] = 2
        let sealed = try codec.sealBody(pageIndex: 2, plaintext: page, salt: [])
        XCTAssertEqual(codec.capacity, 240)
        XCTAssertTrue(sealed.tag.isEmpty)
        XCTAssertEqual(try codec.openBody(pageIndex: 2, sealed: sealed.bytes), sealed.bytes)
        var damaged = sealed.bytes; damaged[255] ^= 1
        expect("XX001") { _ = try codec.openBody(pageIndex: 2, sealed: damaged) }
        expect("XX001") { _ = try codec.openBody(pageIndex: 2, sealed: sealed.bytes, expectedTag: [0]) }
    }

    func testMetaAuthenticationBindsSlotAndOrderedManifest() throws {
        let codec = try EncryptedPageCodec(pageSize: 1024, dataKey: Array(repeating: 7, count: 32))
        var meta = [UInt8](repeating: 0, count: 1024)
        meta.replaceSubrange(0..<8, with: [0x4a, 0x45, 0x44, 0x42, 0, 52, 1, 40])
        try PageEnvelope.putU32(1024, into: &meta, at: 8)
        let manifest = [ManifestPageBytes(pageIndex: 7, bytes: Array(repeating: 7, count: 1024)),
                        ManifestPageBytes(pageIndex: 5, bytes: Array(repeating: 5, count: 1024))]
        let sealed = try codec.sealMeta(meta, slot: 1, manifest: manifest, nonce: Array(repeating: 9, count: 24))
        try codec.verifyMeta(sealed, slot: 1, manifest: manifest)
        try codec.verifyServedMeta(sealed, manifest: manifest, staleHeldKey: true)
        expect("XX001") { try codec.verifyMeta(sealed, slot: 0, manifest: manifest) }
        expect("XX001") { try codec.verifyMeta(sealed, slot: 1, manifest: Array(manifest.reversed())) }
        expect("XX001") { try codec.verifyMeta(sealed, slot: 1) }
        var altered = sealed; altered[100] ^= 1
        try PageEnvelope.putU32(PageEnvelope.checksum(altered, at: 32), into: &altered, at: 32)
        expect("XX001") { try codec.verifyMeta(altered, slot: 1, manifest: manifest) }
        expect("28P04") { try codec.verifyServedMeta(altered, manifest: manifest, staleHeldKey: true) }
        expect("08P01") { try codec.verifyServedMeta(altered, manifest: manifest, staleHeldKey: false) }
        let wrong = try EncryptedPageCodec(pageSize: 1024, dataKey: Array(repeating: 8, count: 32))
        expect("XX001") { try wrong.verifyMeta(sealed, slot: 1, manifest: manifest) }
    }

    func testFrozenEncryptedGoldenPages() throws {
        let credential = try Credential.hostKey(bytes(vector("host_kek"), "key_hex"))
        for name in ["encrypted_empty_db", "encrypted_pk_table"] {
            let url = try XCTUnwrap(Bundle.module.url(forResource: name, withExtension: "jed", subdirectory: "Fixtures"))
            let file = [UInt8](try Data(contentsOf: url))
            let pageSize = Int(try PageEnvelope.readU32(file, at: 8))
            XCTAssertEqual(file.count % pageSize, 0)
            func page(_ index: Int) -> [UInt8] { Array(file[(index * pageSize)..<((index + 1) * pageSize)]) }
            let selected = try KeyBlock.select(page2: page(2), page3: page(3), pageSize: pageSize)
            let codec = try EncryptedPageCodec(pageSize: pageSize, dataKey: selected.block.unlock(credential), opening: true)
            for slot in 0...1 {
                let meta = page(slot)
                try codec.verifyMeta(meta, slot: UInt32(slot))
                XCTAssertEqual(try codec.sealMeta(meta, slot: UInt32(slot), nonce: Array(meta[(pageSize - 40)..<(pageSize - 16)])), meta)
                let root = try PageEnvelope.readU32(meta, at: 20)
                _ = try codec.openBody(pageIndex: root, sealed: page(Int(root)), expectedTag: Array(meta[64..<80]))
            }
            for index in 4..<(file.count / pageSize) {
                let stored = page(index)
                let clear = try codec.openBody(pageIndex: UInt32(index), sealed: stored)
                XCTAssertEqual(try codec.sealBody(pageIndex: UInt32(index), plaintext: clear,
                                                salt: Array(stored[(pageSize - 40)..<(pageSize - 16)])).bytes, stored)
            }
        }
    }
}

/// Explicitly insecure deterministic fixture source, using entropy.md's fill
/// contract. It is private to tests and can never become production entropy.
private final class SeededKeyBytes {
    private var state: UInt64
    private(set) var drawSizes: [Int] = []
    init(seed: UInt64) { state = seed }
    func fill(_ count: Int) -> [UInt8] {
        drawSizes.append(count)
        var result: [UInt8] = []
        while result.count < count {
            state &+= 0x9e3779b97f4a7c15
            var x = state
            x = (x ^ (x >> 30)) &* 0xbf58476d1ce4e5b9
            x = (x ^ (x >> 27)) &* 0x94d049bb133111eb
            x ^= x >> 31
            var writer = ByteWriter(); writer.u64(x)
            result.append(contentsOf: writer.bytes.prefix(count - result.count))
        }
        return result
    }
}
