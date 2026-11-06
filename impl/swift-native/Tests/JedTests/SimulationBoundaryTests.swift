import Foundation
import XCTest
@testable import Jed

/// Fault-model invariants and production host seams, beyond the shared trace
/// language's observability. SQL behavior belongs to the full trace/corpus run.
final class SimulationBoundaryTests: XCTestCase {
    private func fails(_ code: String, _ body: () throws -> Void, file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertThrowsError(try body(), file: file, line: line) { XCTAssertEqual(($0 as? JedError)?.code, code, file: file, line: line) }
    }
    func testEverySharedSimulationDecisionVector() throws {
        let url = try XCTUnwrap(Bundle.module.url(forResource: "SpecSnapshot", withExtension: "json", subdirectory: "Fixtures"))
        let snapshot = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
        let documents = try XCTUnwrap(snapshot["documents"] as? [String: [String: Any]])
        let vectors = try XCTUnwrap(documents["spec/encoding/prng.toml"]?["sim_decide"] as? [[String: Any]])
        XCTAssertEqual(vectors.count, 5)
        for vector in vectors {
            func word(_ name: String) throws -> UInt64 { try XCTUnwrap(vector[name] as? NSNumber).uint64Value }
            let value = try SimDecision.value(seed: word("seed"), lane: word("lane"), step: word("step"), offset: word("offset"), version: word("version"))
            XCTAssertEqual(String(format: "%016llx", value), vector["value"] as? String)
        }
        XCTAssertEqual(SimDecision.fileKey("sim.jed"), 0)
        XCTAssertNotEqual(SimDecision.fileKey("a.jed"), SimDecision.fileKey("b.jed"))
    }
    func testProcessSeesUnissuedVersionsUntilDeathAndGrowthStabilizesThem() throws {
        let fs = try SimFilesystem(seed: 19, pageSize: 256), file = try fs.create("sim.jed")
        try file.setSize(256); try fs.syncDirectory(); try file.write(at: 0, bytes: [1, 2, 3]); try file.sync()
        fs.beginStep(1, fault: SimFault(kind: .processCrash, issue: 0))
        try file.write(at: 0, bytes: [4, 5, 6]); try file.write(at: 0, bytes: [7, 8, 9])
        XCTAssertEqual(try file.read(at: 0, count: 3), [7, 8, 9])
        fs.processDeath(); XCTAssertEqual(try file.read(at: 0, count: 3), [1, 2, 3])
        fs.beginStep(2, fault: SimFault(kind: .processCrash, issue: 0))
        try file.write(at: 0, bytes: [10, 11, 12]); try file.setSize(512)
        XCTAssertEqual(fs.barriers, 0); XCTAssertTrue(file.facts.barrierComplete)
        fs.processDeath(); XCTAssertEqual(try file.read(at: 0, count: 3), [10, 11, 12])
        fs.powerLoss(step: 3, survive: 0, tear: 0)
        XCTAssertEqual(try fs.open("sim.jed").read(at: 0, count: 3), [10, 11, 12])
    }
    func testPowerLossIsContentKeyedAndDirectoryBarrierIsIndependent() throws {
        func run(_ order: [Int]) throws -> [UInt8] {
            let fs = try SimFilesystem(seed: 81723, sector: 64, pageSize: 256), file = try fs.create("sim.jed")
            try file.setSize(1024); try fs.syncDirectory(); fs.beginStep(42)
            for page in order { try file.write(at: UInt64(page * 256), bytes: Array(repeating: UInt8(page + 1), count: 256)) }
            fs.powerLoss(step: 42, survive: 700, tear: 600)
            return try fs.open("sim.jed").read(at: 0, count: 1024)
        }
        XCTAssertEqual(try run([0, 1, 2, 3]), try run([3, 1, 0, 2]))
        let fs = try SimFilesystem(seed: 1, pageSize: 256)
        fs.beginStep(0, fault: SimFault(kind: .powerLoss))
        try fs.syncDirectory(); XCTAssertEqual(fs.barriers, 0)
        let temporary = try fs.create("sim.jedtmp"); try temporary.setSize(256)
        XCTAssertEqual(fs.barriers, 0); XCTAssertFalse(temporary.facts.barrierComplete)
        try fs.syncDirectory()
        XCTAssertEqual(fs.fired, "barrier"); XCTAssertEqual(fs.barriers, 1)
        XCTAssertFalse(fs.entryDurable("sim.jedtmp"))
        try fs.syncDirectory() // void directory calls cannot acknowledge death
        XCTAssertEqual(fs.barriers, 1)
        fails("58030") { _ = try temporary.read(at: 0, count: 1) }
    }
    func testTemporaryReplacementUsesOneCanonicalDirectoryCreate() throws {
        for seed: UInt64 in 1...16 {
            let fs = try SimFilesystem(seed: seed, pageSize: 256)
            let main = try fs.create("sim.jed")
            try main.setSize(256); try main.write(at: 0, bytes: [1]); try main.sync(); try fs.syncDirectory()
            fs.beginStep(1)
            let stale = try fs.create("sim.jed.jedtmp")
            try stale.setSize(256); try stale.write(at: 0, bytes: [2]); try stale.sync()
            fs.processDeath()
            fs.beginStep(2)
            let host: any FileOperations = fs
            let replacement = try host.createReplacementTemporary("sim.jed.jedtmp", skipFsync: false)
            try replacement.setSize(256); try replacement.write(at: 0, bytes: [3]); try replacement.sync()
            try fs.rename("sim.jed.jedtmp", to: "sim.jed")
            // The pending directory operations are create(stale),
            // create(replacement), rename. No extra deletion may renumber
            // the shared lane-10 ordered-prefix decision.
            let prefix = SimDecision.value(seed: seed, lane: 10, step: 2, offset: 0, version: 0) % 4
            fs.powerLoss(step: 2, survive: 0, tear: 0)
            XCTAssertEqual(try fs.open("sim.jed").read(at: 0, count: 1), [prefix == 3 ? 3 : 1])
            if prefix == 1 { XCTAssertEqual(try fs.open("sim.jed.jedtmp").read(at: 0, count: 1), [2]) }
            if prefix == 2 { XCTAssertEqual(try fs.open("sim.jed.jedtmp").read(at: 0, count: 1), [3]) }
        }
    }

    func testReadFaultOracleExemptionAndFailedWriteLandNothing() throws {
        let fs = try SimFilesystem(seed: 7, pageSize: 256), file = try fs.create("sim.jed")
        try file.setSize(256); try fs.syncDirectory()
        fs.beginStep(1, fault: SimFault(kind: .write, probability: 1000))
        fails("58030") { try file.write(at: 0, bytes: [99]) }
        XCTAssertEqual(file.facts.writes, 0); XCTAssertTrue(file.facts.pendingOffsets.isEmpty)
        fs.beginStep(2, fault: SimFault(kind: .read, probability: 1000))
        fails("58030") { _ = try file.read(at: 0, count: 1) }
        XCTAssertEqual(try fs.withOracle { try file.read(at: 0, count: 1) }, [0])
        fails("58030") { _ = try file.read(at: 0, count: 1) }
    }
    func testTraceBoundaryPreservesSQLAndRejectsUnknownSteps() throws {
        let trace = try SimTrace("# format: simulation\n# seed: 17\nopen r read p2\non r SELECT 'α  β', 'x:y'\nfault power_loss survive=500 tear=100\ncommit r\n")
        XCTAssertEqual(trace.steps.count, 4); XCTAssertEqual(trace.steps[0].process, 2)
        XCTAssertEqual(trace.steps[1].sql, "SELECT 'α  β', 'x:y'")
        XCTAssertEqual(try trace.steps[2].fault().survive, 500)
        fails("22023") { _ = try SimTrace("# format: simulation\nunsupported\n") }
        fails("22023") { _ = try SimTrace("# format: simulation\nfault power_loss survive=1001\n") }
        fails("22023") { _ = try SimTrace("# format: simulation\nopen x read p-1\n") }
    }

    #if os(Linux) || canImport(Darwin)
    private func sequence(_ database: Database, _ name: String) throws {
        let writer = try database.core.acquireWriter(); defer { writer.close() }
        var snapshot = writer.snapshot, catalog = snapshot.catalog
        catalog.sequences.append(CatalogSequence(name: try SQLText(name)))
        try snapshot.updateCatalog(catalog, schemaChanged: true)
        try writer.persist(XCTUnwrap(writer.prepare(snapshot)), synchronous: true)
    }
    func testSimulatedFilesUseProductionCreateReplacementBackupAndRecovery() throws {
        let fs = try SimFilesystem(seed: 31, pageSize: 256)
        let database = try FileDatabaseHost.createResolved(path: "sim.jed", options: CreateOptions(pageSize: 256, locking: .none),
            catalogID: 9101, random: SeededRandomSource(seed: 1), operations: fs)
        defer { try? database.close() }
        try sequence(database, "first"); let before = database.version
        try database.backup("sim.jed.backup1", form: .pageIdentical)
        let copy = try FileDatabaseHost.openResolved(path: "sim.jed.backup1", options: OpenOptions(locking: .none), catalogID: 9102, operations: fs)
        XCTAssertEqual(copy.version, before); try copy.close()
        let original = database.core.storage
        try database.compact(); XCTAssertEqual(database.version, before + 1)
        XCTAssertFalse(database.core.storage === original)
        fails("58030") { _ = try original.pager.readStored(0) }
        XCTAssertTrue(fs.entryDurable("sim.jed")); XCTAssertFalse(try fs.exists("sim.jed.jedtmp"))
        try database.close(); fs.powerLoss(step: 4, survive: 0, tear: 0)
        let reopened = try FileDatabaseHost.openResolved(path: "sim.jed", options: OpenOptions(locking: .none), catalogID: 9103, operations: fs)
        defer { try? reopened.close() }
        XCTAssertEqual(reopened.version, before + 1)
        XCTAssertEqual(reopened.core.storage.committed.catalog.sequences.map { $0.name.description }, ["first"])
    }
    func testCreateReopensPublishedPathAfterDirectoryBarrier() throws {
        let fs = try SimFilesystem(seed: 2, pageSize: 256)
        fs.beginStep(55, fault: SimFault(kind: .powerLoss, survive: 1000, tear: 128, nth: 2))
        fails("58030") {
            _ = try FileDatabaseHost.createResolved(path: "a1.jed", options: CreateOptions(pageSize: 256, locking: .none),
                catalogID: 9105, random: SeededRandomSource(seed: 1), operations: fs)
        }
        XCTAssertTrue(fs.isDead); XCTAssertEqual(fs.fired, "barrier")
        XCTAssertNotNil(fs.facts("a1.jed"))
        XCTAssertFalse(fs.entryDurable("a1.jed"))
        fs.powerLoss(step: 55, survive: 1000, tear: 128)
        XCTAssertNil(fs.facts("a1.jed"), "a creation that never returned has no durability floor")
    }

    func testReplacementFailureBeforeRenameLeavesOriginalUsable() throws {
        let fs = try SimFilesystem(seed: 32, pageSize: 256)
        let database = try FileDatabaseHost.createResolved(path: "sim.jed", options: CreateOptions(pageSize: 256, locking: .none),
            catalogID: 9104, random: SeededRandomSource(seed: 1), operations: fs)
        defer { try? database.close() }
        try sequence(database, "first"); let original = database.core.storage, version = database.version
        fs.beginStep(2, fault: SimFault(kind: .write, probability: 1000))
        fails("58030") { try database.compact() }
        XCTAssertTrue(database.core.storage === original); XCTAssertEqual(database.version, version)
        try database.checkOpen(); XCTAssertFalse(try fs.exists("sim.jed.jedtmp"))
        fs.beginStep(3)
        try sequence(database, "after_failure"); XCTAssertEqual(database.version, version + 1)
    }
    #endif
}
