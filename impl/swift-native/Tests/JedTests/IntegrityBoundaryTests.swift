import Foundation
import Synchronization
import XCTest
@testable import Jed

private final class IntegrityFaultDevice: BlockStore {
    let backing = MemoryBlockStore()
    let readFailure = Mutex<UInt64?>(nil)
    func read(at offset: UInt64, count: Int) throws -> [UInt8] {
        if readFailure.withLock({ $0 == offset }) { throw JedError("58030", "injected integrity read error") }
        return try backing.read(at: offset, count: count)
    }
    func write(at offset: UInt64, bytes: [UInt8]) throws { try backing.write(at: offset, bytes: bytes) }
    func size() throws -> UInt64 { try backing.size() }
    func setSize(_ bytes: UInt64) throws { try backing.setSize(bytes) }
    func sync() throws { try backing.sync() }
}

final class IntegrityBoundaryTests: XCTestCase {
    private func fixture(_ name: String, ext: String = "jed", subdirectory: String = "Fixtures") throws -> [UInt8] {
        Array(try Data(contentsOf: XCTUnwrap(Bundle.module.url(forResource: name, withExtension: ext, subdirectory: subdirectory))))
    }
    private func environment() throws -> Environment {
        var builder = Environment.builder()
        try builder.loadUnicodeData(fixture("unicode", ext: "jucd"))
        try builder.loadTimeZoneData(fixture("tzdata", ext: "jtz"))
        return builder.build()
    }
    private func directory() throws -> URL {
        let url = FileManager.default.temporaryDirectory.appendingPathComponent("jed-integrity-" + UUID().uuidString)
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: false)
        addTeardownBlock { try FileManager.default.removeItem(at: url) }; return url
    }
    private func contract(_ report: IntegrityReport) -> [String: Any] {
        ["findings": report.findings.map { finding -> [String: Any] in
            ["kind": finding.kind, "database": finding.database, "object": finding.object as Any? ?? NSNull(),
             "page": finding.page as Any? ?? NSNull(), "key": finding.key.map { $0.map { String(format: "%02x", $0) }.joined() } as Any? ?? NSNull()]
        }, "truncated": report.truncated, "skipped": report.skipped.map { ["object": $0.object, "reason": $0.reason] },
         "pages": report.pages, "rows": report.rows]
    }
    private func json(_ value: Any) throws -> String {
        String(decoding: try JSONSerialization.data(withJSONObject: value, options: [.sortedKeys]), as: UTF8.self)
    }
    func testEveryFrozenFindingFixtureThroughPublicFileAPI() throws {
        let directory = try directory(), environment = try environment()
        let root = try XCTUnwrap(Bundle.module.url(forResource: "Integrity", withExtension: nil, subdirectory: "Fixtures"))
        let expected = try FileManager.default.contentsOfDirectory(at: root, includingPropertiesForKeys: nil)
            .filter { $0.pathExtension == "json" }.sorted { $0.lastPathComponent < $1.lastPathComponent }
        XCTAssertEqual(expected.count, 24)
        var runs = 0
        for file in expected {
            do {
                let document = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: file)) as? [String: Any])
                let name = try XCTUnwrap(document["fixture"] as? String), path = directory.appendingPathComponent(name)
                try FileManager.default.copyItem(at: root.appendingPathComponent(name), to: path)
                let database = try Database.open(path.path, options: OpenOptions(environment: environment, cacheBytes: 1024,
                    readOnly: true, locking: .exclusive))
                defer { try? database.close() }
                for run in try XCTUnwrap(document["runs"] as? [[String: Any]]) {
                    let level = try XCTUnwrap(IntegrityLevel(rawValue: XCTUnwrap(run["level"] as? String)))
                    let maximum = try XCTUnwrap(run["max_findings"] as? Int)
                    let report = try database.integrityCheck(options: .init(level: level,
                        constraints: run["constraints"] as? Bool ?? false, maxFindings: maximum))
                    XCTAssertEqual(try json(contract(report)), try json(XCTUnwrap(run["report"])), "\(name) \(level) max=\(maximum)")
                    runs += 1
                }
            } catch { XCTFail("\(file.lastPathComponent): \(error)") }
        }
        XCTAssertEqual(runs, 50)
    }
    func testAllFrozenGoldenImagesThroughPublicIntegrityAPI() throws {
        let environment = try environment(), directory = try directory()
        let root = try XCTUnwrap(Bundle.module.url(forResource: "Fixtures", withExtension: nil))
        let images = try FileManager.default.contentsOfDirectory(at: root, includingPropertiesForKeys: nil)
            .filter { $0.pathExtension == "jed" }.sorted { $0.lastPathComponent < $1.lastPathComponent }
        XCTAssertEqual(images.count, 112)
        let skips: [String: [IntegritySkip]] = [
            "collation_skew_corrupt.jed": [.init(object: "t_name_idx", reason: "missing_loaded_data")],
            "hostfunc_index_table.jed": [.init(object: "t_geo_idx", reason: "missing_host_function")],
            "hostfunc_check_table.jed": [.init(object: "t_a_check1", reason: "missing_host_function")],
            "unparsable_definitions.jed": [.init(object: "e_r_excl", reason: "unparsable_definition"),
                .init(object: "p_future_idx", reason: "unparsable_definition"), .init(object: "x_future_idx", reason: "unparsable_definition"),
                .init(object: "c_v_check", reason: "unparsable_definition")]
        ]
        var checked = 0, rejected = 0
        for image in images {
            let name = image.lastPathComponent, path = directory.appendingPathComponent(name)
            try FileManager.default.copyItem(at: image, to: path)
            let database: Database
            do {
                let credential = try name.hasPrefix("encrypted_") ? Credential.hostKey(Array(repeating: 0x4a, count: 32)) : nil
                database = try Database.open(path.path, options: OpenOptions(environment: environment, cacheBytes: 1024,
                    credential: credential, readOnly: true, locking: .exclusive))
            } catch {
                XCTAssertEqual((error as? JedError)?.code, "XX001", name)
                XCTAssertTrue(["cow_invalid_free_list.jed", "malformed_fk_type.jed", "malformed_missing_type.jed", "malformed_interior_cycle.jed"].contains(name), "unexpected open refusal \(name): \(error)")
                rejected += 1; continue
            }
            defer { try? database.close() }
            for level in [IntegrityLevel.quick, .full] {
                do {
                    let report = try database.integrityCheck(options: .init(level: level, constraints: level == .full, maxFindings: 0))
                    if name.hasPrefix("malformed_") { XCTAssertFalse(report.ok, name) }
                    else {
                        XCTAssertTrue(report.ok, "\(name) \(level): \(report.findings)")
                        XCTAssertEqual(report.skipped, level == .full ? skips[name] ?? [] : [], name)
                    }
                } catch { XCTFail("\(name) \(level): \(error)") }
            }
            checked += 1
        }
        XCTAssertEqual(checked, 108); XCTAssertEqual(rejected, 4)
    }
    func testDefaultsNamedDomainsCancellationAndConcurrentWriter() throws {
        let database = try Database(); defer { try? database.close() }
        XCTAssertTrue(try database.integrityCheck().ok)
        let writer = try database.writeSession(); defer { writer.close() }
        try writer.execute("CREATE TABLE pending (id i32 PRIMARY KEY)")
        XCTAssertTrue(try database.integrityCheck(options: .init(constraints: true)).ok)
        XCTAssertEqual(try database.integrityCheck().rows, 0)
        for name in ["temp", "MAIN", "missing", ""] {
            XCTAssertThrowsError(try database.integrityCheck(name)) { XCTAssertEqual(($0 as? JedError)?.code, "42704") }
        }
        let cancellation = CancellationToken(); cancellation.cancel()
        XCTAssertThrowsError(try database.integrityCheck(cancellation: cancellation)) { XCTAssertEqual(($0 as? JedError)?.code, "57014") }
        try writer.rollback(); writer.close()
        try database.attach(SQLText("attached"), source: .memory, readOnly: false)
        XCTAssertTrue(try database.integrityCheck("attached").ok)
        try database.detach(SQLText("attached"))
    }
    func testCancellationInsideFinalCheckReturnsNoReportEvenAtFindingLimit() throws {
        let token = CancellationToken(), armed = Mutex(false)
        var builder = Environment.builder()
        try builder.register(HostFunction(name: SQLText("check_hook"), arguments: [.i32], result: .boolean,
            volatility: .immutable, componentID: SQLText("integrity-test"), semanticVersion: 1, kernel: { _ in
                if armed.withLock({ $0 }) { token.cancel(); throw JedError("57014", "canceled from host") }
                return .boolean(true)
            }))
        let database = try Database(environment: builder.build()); defer { try? database.close() }
        try database.execute("CREATE TABLE checked (id i32 PRIMARY KEY CHECK(check_hook(id)))")
        try database.execute("INSERT INTO checked VALUES (1)")
        armed.withLock { $0 = true }
        XCTAssertThrowsError(try database.integrityCheck(options: .init(constraints: true, maxFindings: 1), cancellation: token)) {
            XCTAssertEqual(($0 as? JedError)?.code, "57014")
        }
        XCTAssertEqual(database.core.livePinCount, 0)
    }

    func testLiveMetaManifestCatalogAndIOFailuresAreDistinct() throws {
        let device = IntegrityFaultDevice()
        let storage = try StorageDomain.create(store: device, kind: .file, catalogID: 801, pageSize: 1024)
        let database = Database(core: DatabaseCore(storage: storage), environment: Environment.builder().build())
        defer { try? database.close() }
        try database.execute("CREATE TABLE t (id i32 PRIMARY KEY)")
        try database.execute("INSERT INTO t VALUES (1)")
        XCTAssertTrue(try database.integrityCheck().ok)
        let selected = storage.pager.selected, offset = UInt64(selected.slot) * 1024
        var metaBytes = selected.bytes; metaBytes[0] ^= 1
        try device.write(at: offset, bytes: metaBytes)
        XCTAssertEqual(try database.integrityCheck().findings.first?.kind, "meta_invalid")
        try device.write(at: offset, bytes: selected.bytes)
        let root = selected.meta.root, original = try storage.pager.readStored(root.index)
        var broken = original; broken[12] ^= 1
        try device.write(at: UInt64(root.index) * 1024, bytes: broken)
        let damaged = try database.integrityCheck(options: .init(maxFindings: 0))
        XCTAssertTrue(damaged.findings.contains { $0.kind == "manifest_page_mismatch" && $0.page == root.index })
        XCTAssertTrue(damaged.findings.contains { $0.kind == "catalog_page_corrupt" && $0.page == root.index })
        XCTAssertFalse(damaged.findings.contains { $0.kind == "live_count_mismatch" })
        try device.write(at: UInt64(root.index) * 1024, bytes: original)
        device.readFailure.withLock { $0 = UInt64(root.index) * 1024 }
        XCTAssertThrowsError(try database.integrityCheck()) { XCTAssertEqual(($0 as? JedError)?.code, "58030") }
        device.readFailure.withLock { $0 = nil }
        XCTAssertEqual(database.core.livePinCount, 0)
        XCTAssertTrue(try database.integrityCheck().ok)
    }

    func testRegistryAndDefaultsAreFrozenMechanicalFacts() throws {
        guard case .array(let findings) = GeneratedSpec.integrity["finding"],
              case .array(let skips) = GeneratedSpec.integrity["skip"] else { return XCTFail("missing integrity registry") }
        let names = findings.compactMap { value -> String? in
            guard case .table(let row) = value, case .text(let kind) = row["kind"] else { return nil }; return kind
        }
        XCTAssertEqual(names.count, Set(names).count)
        XCTAssertEqual(names.count, 32)
        XCTAssertEqual(skips.count, 5)
        let options = IntegrityCheckOptions()
        XCTAssertEqual(options.level, .full); XCTAssertFalse(options.constraints); XCTAssertEqual(options.maxFindings, 100)
    }

}
