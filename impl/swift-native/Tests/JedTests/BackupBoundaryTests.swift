import Foundation
import XCTest
@testable import Jed

final class BackupBoundaryTests: XCTestCase {
    private func fixture(_ name: String) throws -> [UInt8] {
        Array(try Data(contentsOf: XCTUnwrap(Bundle.module.url(forResource: name, withExtension: "jed", subdirectory: "Fixtures"))))
    }
    private func fails(_ code: String, _ body: () throws -> Void, file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertThrowsError(try body(), file: file, line: line) { XCTAssertEqual(($0 as? JedError)?.code, code, file: file, line: line) }
    }
    private func copy(_ source: PinnedBackup, form: BackupForm = .pageIdentical, entropy: KeyEntropy? = nil) throws -> MemoryBlockStore {
        let target = MemoryBlockStore()
        let image = try BackupImage.write(source, form: form, entropy: entropy) { page in
            XCTAssertEqual(page.bytes.count, source.selected.meta.pageSize)
            try target.write(at: UInt64(page.index) * UInt64(page.bytes.count), bytes: page.bytes)
        }
        try target.setSize(UInt64(image.pageCount) * UInt64(source.selected.meta.pageSize))
        return target
    }
    private func directory() throws -> URL {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("jed-swift-backup-" + UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: false)
        addTeardownBlock { try FileManager.default.removeItem(at: root) }; return root
    }
    private func sequence(_ core: DatabaseCore, _ name: String, synchronous: Bool = true) throws {
        let writer = try core.acquireWriter(); defer { writer.close() }
        var snapshot = writer.snapshot, image = snapshot.catalog
        image.sequences.append(CatalogSequence(name: try SQLText(name)))
        try snapshot.updateCatalog(image, schemaChanged: true)
        try writer.persist(XCTUnwrap(writer.prepare(snapshot)), synchronous: synchronous)
    }

    func testEveryFrozenOrdinaryImageCopiesOriginalBodyAndRepairsBootstrapMetadata() throws {
        let paths = GeneratedSpec.sourceHashes.keys.filter {
            $0.hasPrefix("spec/fileformat/fixtures/") && !$0.contains("/cow_") && !$0.contains("/malformed_")
        }.sorted()
        XCTAssertEqual(paths.count, 87)
        for path in paths {
            let name = String(path.split(separator: "/").last!.dropLast(4))
            do {
                let bytes = try fixture(name), store = MemoryBlockStore(), encrypted = name.hasPrefix("encrypted_")
                try store.write(at: 0, bytes: bytes)
                let credential: Credential? = try encrypted ? .hostKey([UInt8](repeating: 0x4a, count: 32)) : nil
                let domain = try StorageDomain.open(store: store, kind: .file, catalogID: 1801, cacheBytes: 1024,
                    credential: credential, validateCatalog: { _ in })
                let core = DatabaseCore(storage: domain), source = try core.beginBackup()
                defer { source.pin.close() }
                let random = SeededRandomSource(seed: 18)
                let target = try copy(source, entropy: { try random.bytes(count: $0) })
                let first = Int(domain.pager.codec.firstBodyPage) * domain.pager.pageSize
                XCTAssertEqual(try target.read(at: UInt64(first), count: bytes.count - first), Array(bytes[first...]), name)
                if encrypted {
                    let size = domain.pager.pageSize
                    XCTAssertEqual(try target.read(at: UInt64(2 * size), count: 2 * size), Array(bytes[(2 * size)..<(4 * size)]), name)
                }
                let opened = try StorageDomain.open(store: target, catalogID: 1802, credential: credential, validateCatalog: { _ in })
                XCTAssertEqual(opened.committed.txid, source.selected.meta.txid, name)
                XCTAssertEqual(opened.pager.selected.meta.pageCount, source.selected.meta.pageCount, name)
                XCTAssertEqual(opened.livePages, source.selected.meta.livePages, name)
                XCTAssertEqual(opened.committed.catalog.tables.map(\.rowCount), domain.committed.catalog.tables.map(\.rowCount), name)
            } catch { XCTFail("\(name): \(error)") }
        }
    }

    func testPinnedRelaxedGenerationSurvivesLaterCommitAndZerosDeadPages() throws {
        let domain = try StorageDomain.create(store: MemoryBlockStore(), kind: .file, catalogID: 1803, pageSize: 256)
        let core = DatabaseCore(storage: domain)
        try sequence(core, "first", synchronous: false)
        let source = try core.beginBackup(); defer { source.pin.close() }
        try sequence(core, "second", synchronous: false)
        XCTAssertGreaterThan(core.version, source.selected.meta.txid)
        let target = try copy(source)
        let reopened = try StorageDomain.open(store: target, catalogID: 1804, validateCatalog: { _ in })
        XCTAssertEqual(reopened.committed.catalog.sequences.map { $0.name.description }, ["first"])
        XCTAssertEqual(reopened.committed.txid, source.selected.meta.txid)
        XCTAssertTrue(reopened.pager.selected.entries.isEmpty)
        let image = try CatalogChain.read(root: source.selected.meta.root, pageCount: source.selected.meta.pageCount,
            codec: domain.pager.codec, read: { try domain.pager.readBody($0) })
        for page in domain.pager.codec.firstBodyPage..<source.selected.meta.pageCount where !image.pages.contains(page) {
            XCTAssertEqual(try target.read(at: UInt64(page) * 256, count: 256), [UInt8](repeating: 0, count: 256))
        }
    }

    func testCurrentRewrappedKeyIsCopiedAndMemoryOnlyAdmitsCompactedForm() throws {
        let random = SeededRandomSource(seed: 19), entropy: KeyEntropy = { try random.bytes(count: $0) }
        let old = try Credential.hostKey([UInt8](repeating: 1, count: 32)), new = try Credential.hostKey([UInt8](repeating: 2, count: 32))
        let domain = try StorageDomain.create(store: MemoryBlockStore(), kind: .file, catalogID: 1805, pageSize: 1024,
            credential: old, entropy: entropy)
        let core = DatabaseCore(storage: domain), source = try core.beginBackup(); defer { source.pin.close() }
        let codec = try XCTUnwrap(domain.pager.codec as? EncryptedPageCodec), block = try XCTUnwrap(domain.pager.selectedKeyBlock)
        let changed = try codec.rewrap(block, credential: new, parameters: .default, entropy: entropy)
        try domain.pager.changeKeyBlock(changed, replacing: block, syncDirectory: {})
        for form in [BackupForm.pageIdentical, .compacted] {
            let target = try copy(source, form: form, entropy: entropy)
            fails("28P01") { _ = try Pager.open(store: target, credential: old) }
            let opened = try Pager.open(store: target, credential: new)
            XCTAssertEqual(opened.selectedKeyBlock, changed)
            XCTAssertEqual(opened.selected.meta.txid, source.selected.meta.txid + (form == .compacted ? 1 : 0))
        }
        let memory = try Database.create(options: CreateOptions(pageSize: 256))
        defer { try? memory.close() }
        let pin = try memory.core.beginBackup(); defer { pin.pin.close() }
        fails("0A000") { _ = try copy(pin) }
        let compacted = try copy(pin, form: .compacted)
        XCTAssertEqual(try Pager.open(store: compacted).selected.meta.txid, memory.version + 1)
    }

    func testRoleRevocationDuringCopyFailsBeforeAnotherPage() throws {
        let domain = try StorageDomain.create(store: MemoryBlockStore(), kind: .file, catalogID: 1806, pageSize: 256)
        let core = DatabaseCore(storage: domain), source = try core.beginBackup(); defer { source.pin.close() }
        var count = 0
        fails("08006") {
            _ = try BackupImage.write(source, form: .pageIdentical, entropy: nil) { _ in
                count += 1
                try core.quiesceRole(flush: false)
            }
        }
        XCTAssertEqual(count, 1)
    }

    #if os(Linux) || canImport(Darwin)
    func testNativePublicationDuplicateRefusalAndFailureCleanupKeepPinThroughRename() throws {
        let root = try directory(), destination = root.appendingPathComponent("copy.jed").path
        let database = try Database.create(options: CreateOptions(pageSize: 256))
        defer { try? database.close() }
        try database.backup(destination, form: .compacted)
        let opened = try Database.open(destination, options: OpenOptions(locking: .exclusive))
        XCTAssertEqual(opened.version, database.version + 1); try opened.close()
        let existing = try Data(contentsOf: URL(fileURLWithPath: destination))
        fails("58P02") { try database.backup(destination, form: .compacted) }
        XCTAssertEqual(try Data(contentsOf: URL(fileURLWithPath: destination)), existing)
        for afterRename in [false, true] {
            let failed = root.appendingPathComponent(afterRename ? "after.jed" : "before.jed").path
            var checks = 0
            fails("08006") {
                try FileBackupHost.write(destination: failed, skipFsync: true, checkRole: {
                    checks += 1
                    if afterRename && checks == 1 { return }
                    throw JedError("08006", "role changed")
                }) { store in try store.write(at: 0, bytes: [1, 2, 3]) }
            }
            XCTAssertFalse(FileManager.default.fileExists(atPath: failed))
            XCTAssertFalse(FileManager.default.fileExists(atPath: failed + ".jedtmp"))
        }
    }
    #endif
}
