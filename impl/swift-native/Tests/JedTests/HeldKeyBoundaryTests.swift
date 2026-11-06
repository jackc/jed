import XCTest
import Synchronization
@testable import Jed

private final class HeldKeyFailingReadStore: BlockStore {
    let memory: MemoryBlockStore
    private let reads = Mutex(0)
    init(_ memory: MemoryBlockStore) { self.memory = memory }
    func read(at offset: UInt64, count: Int) throws -> [UInt8] {
        if offset == 1024 && count == 1024 {
            let ordinal = reads.withLock { $0 += 1; return $0 }
            if ordinal == 2 { throw JedError("58030", "classification read fault") }
        }
        return try memory.read(at: offset, count: count)
    }
    func write(at offset: UInt64, bytes: [UInt8]) throws { try memory.write(at: offset, bytes: bytes) }
    func size() throws -> UInt64 { try memory.size() }
    func setSize(_ bytes: UInt64) throws { try memory.setSize(bytes) }
    func sync() throws { try memory.sync() }
}

final class HeldKeyBoundaryTests: XCTestCase {
    private func fails(_ code: String, _ body: () throws -> Void) {
        XCTAssertThrowsError(try body()) { XCTAssertEqual(($0 as? JedError)?.code, code) }
    }
    private func encrypted(_ seed: UInt64) throws -> (MemoryBlockStore, StorageDomain, KeyEntropy, Credential) {
        let store = MemoryBlockStore(), random = SeededRandomSource(seed: seed)
        let entropy: KeyEntropy = { try random.bytes(count: $0) }
        let credential = try Credential.hostKey([UInt8](repeating: 0x11, count: 32))
        let domain = try StorageDomain.create(store: store, kind: .file, catalogID: seed, pageSize: 1024,
            credential: credential, entropy: entropy)
        return (store, domain, entropy, credential)
    }
    func testHeldPromotionKeepsKeyAcrossCredentialChangeWithoutRepairingPair() throws {
        let (store, domain, entropy, old) = try encrypted(1910)
        let codec = try XCTUnwrap(domain.pager.codec as? EncryptedPageCodec), block = try XCTUnwrap(domain.pager.selectedKeyBlock)
        let changed = try codec.rewrap(block, credential: .hostKey([UInt8](repeating: 0x22, count: 32)), parameters: .default, entropy: entropy)
        // A complete newer copy plus a complete older copy is a valid crash
        // boundary. A promoting holder must adopt it without repairing it.
        try store.write(at: 3072, bytes: changed.encode(pageSize: 1024))
        let before = try store.read(at: 2048, count: 2048)
        let opened = try Pager.open(store: store, credential: old, writable: true,
            unlockedCodec: codec, heldSessionKey: true)
        XCTAssertEqual(opened.selectedKeyBlock, changed)
        XCTAssertEqual(try store.read(at: 2048, count: 2048), before)
        fails("28P01") { _ = try Pager.open(store: store, credential: old) }
    }
    func testRekeyAndPlaintextAreStaleWhileCorruptionStaysCorruption() throws {
        let (_, previous, _, _) = try encrypted(1911)
        let held = try XCTUnwrap(previous.pager.codec as? EncryptedPageCodec)
        let (changed, _, _, credential) = try encrypted(1912)
        fails("28P04") { _ = try Pager.open(store: changed, unlockedCodec: held, heldSessionKey: true) }
        // Maintenance uses the same held-codec primitive but does not label
        // damage as a client-session key transition.
        fails("XX001") { _ = try Pager.open(store: changed, unlockedCodec: held) }
        let plain = MemoryBlockStore()
        _ = try StorageDomain.create(store: plain, catalogID: 1913, pageSize: 256)
        fails("28P04") { _ = try Pager.open(store: plain, unlockedCodec: held, heldSessionKey: true) }
        try changed.write(at: 0, bytes: [0]); try changed.write(at: 1024, bytes: [0])
        fails("XX001") { _ = try Pager.open(store: changed, credential: credential, unlockedCodec: held, heldSessionKey: true) }
    }
    func testClassificationReadFailureIsNotReportedAsAKeyChange() throws {
        let (_, previous, _, _) = try encrypted(1915)
        let held = try XCTUnwrap(previous.pager.codec as? EncryptedPageCodec)
        let (changed, _, _, _) = try encrypted(1916)
        let faulty = HeldKeyFailingReadStore(changed)
        fails("58030") { _ = try Pager.open(store: faulty, unlockedCodec: held, heldSessionKey: true) }
    }
    func testOneAuthenticatingOlderSlotPreservesSelectedAuthenticationFailure() throws {
        let (store, domain, entropy, credential) = try encrypted(1914)
        let codec = try XCTUnwrap(domain.pager.codec as? EncryptedPageCodec), core = DatabaseCore(storage: domain)
        let writer = try core.acquireWriter()
        var snapshot = writer.snapshot, image = snapshot.catalog
        image.sequences.append(CatalogSequence(name: try SQLText("newer")))
        try snapshot.updateCatalog(image, schemaChanged: true)
        try writer.persist(XCTUnwrap(writer.prepare(snapshot, entropy: entropy))); writer.close()
        let selected = domain.pager.selected
        var damaged = selected.bytes
        damaged[damaged.count - 1] ^= 1
        try PageEnvelope.putU32(PageEnvelope.checksum(damaged, at: 32), into: &damaged, at: 32)
        try store.write(at: UInt64(selected.slot) * 1024, bytes: damaged)
        fails("XX001") { _ = try Pager.open(store: store, credential: credential, unlockedCodec: codec, heldSessionKey: true) }
    }
}
