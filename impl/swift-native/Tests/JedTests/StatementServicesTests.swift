import Foundation
import Synchronization
import XCTest
@testable import Jed

private final class ServiceClockProbe: ClockSource {
    private struct State { var next: Int64; var reads = 0 }
    private let state: Mutex<State>
    init(_ first: Int64 = 0) { state = Mutex(State(next: first)) }
    var reads: Int { state.withLock { $0.reads } }
    func microseconds() -> Int64 {
        state.withLock { state in
            let value = state.next; state.next += 1000; state.reads += 1
            return value
        }
    }
}
private final class ServiceRandomProbe: RandomSource {
    private let requests = Mutex<[Int]>([])
    var counts: [Int] { requests.withLock { $0 } }
    func bytes(count: Int) -> [UInt8] {
        requests.withLock { $0.append(count) }
        return Array(repeating: 0xff, count: count)
    }
}
private final class ShortRandomSource: RandomSource {
    func bytes(count: Int) -> [UInt8] { [] }
}

final class StatementServicesTests: XCTestCase {
    private func settings(clock: any ClockSource, random: any RandomSource = ServiceRandomProbe()) -> SessionSettings {
        var builder = Environment.builder()
        builder.setClockSource(clock); builder.setRandomSource(random)
        return SessionSettings(environment: builder.build(), options: SessionOptions())
    }
    private func uuid(_ value: SQLValue) throws -> [UInt8] {
        guard case .uuid(let uuid) = value else { throw JedError("XX000", "test expected UUID") }
        return uuid.bytes
    }

    func testAllSharedEntropyStreamAndUUIDByteVectors() throws {
        let url = try XCTUnwrap(Bundle.module.url(forResource: "SpecSnapshot", withExtension: "json", subdirectory: "Fixtures"))
        let snapshot = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
        let documents = try XCTUnwrap(snapshot["documents"] as? [String: [String: Any]])
        let vectors = try XCTUnwrap(documents["spec/encoding/prng.toml"])
        for row in try XCTUnwrap(vectors["stream"] as? [[String: Any]]) {
            let random = SeededRandomSource(seed: try XCTUnwrap(row["seed"] as? NSNumber).uint64Value)
            for output in try XCTUnwrap(row["outputs"] as? [String]) {
                XCTAssertEqual(try random.bytes(count: 8), try ScalarCasts.decodeHex(Array(output.utf8)))
            }
        }
        for version in ["uuidv4", "uuidv7"] {
            for row in try XCTUnwrap(vectors[version] as? [[String: Any]]) {
                let random = SeededRandomSource(seed: try XCTUnwrap(row["seed"] as? NSNumber).uint64Value)
                let clock = ManualClock(microseconds: (row["clock_micros"] as? NSNumber)?.int64Value ?? 0)
                let services = StatementServices(settings: settings(clock: clock, random: random), meter: CostMeter())
                let expected = version == "uuidv4" ? [try XCTUnwrap(row["uuid"] as? String)] : try XCTUnwrap(row["uuids"] as? [String])
                for spelling in expected {
                    XCTAssertEqual(try uuid(services.call(version, [])), try ScalarCasts.uuid(SQLText(spelling)))
                }
            }
        }
    }

    func testLazyClockSharingVolatileReadsAndQueryBlockReset() throws {
        let clock = ServiceClockProbe(10_000), random = ServiceRandomProbe()
        let settings = settings(clock: clock, random: random)
        let services = StatementServices(settings: settings, meter: CostMeter())
        let context = services.context(), copy = context
        XCTAssertEqual(clock.reads, 0); XCTAssertTrue(random.counts.isEmpty)
        _ = try ScalarCasts.input(SQLText("2001-02-03"), to: TypeDescriptor(.scalar(.date)), context: context.scalar)
        XCTAssertEqual(clock.reads, 0)
        XCTAssertEqual(try context.now(), 10_000)
        XCTAssertEqual(try copy.now(), 10_000)
        guard case .timestamptz(let fresh) = try services.call("clock_timestamp", []) else { return XCTFail("timestamp expected") }
        XCTAssertEqual(fresh, 11_000)
        XCTAssertEqual(try context.now(), 10_000)
        XCTAssertEqual(clock.reads, 2)
        let nextBlock = StatementServices(settings: settings, meter: CostMeter())
        XCTAssertEqual(clock.reads, 2)
        XCTAssertEqual(try nextBlock.now(), 12_000)
        XCTAssertEqual(clock.reads, 3); XCTAssertTrue(random.counts.isEmpty)
    }

    func testClockWordsRemainLazyAndDataInputCannotReadThem() throws {
        let clock = ServiceClockProbe(), services = StatementServices(settings: settings(clock: clock), meter: CostMeter())
        let context = services.context(), date = TypeDescriptor(.scalar(.date))
        let word = SQLValue.text(try SQLText("today"))
        for mode: TypeRules.CastMode in [.assignment, .implicitCast] {
            XCTAssertThrowsError(try ScalarCasts.cast(word, from: .text, to: date, mode: mode, context: context.scalar)) {
                XCTAssertEqual(($0 as? JedError)?.code, "42804")
            }
        }
        XCTAssertThrowsError(try ContainerText.input(SQLText("{today}"), to: TypeDescriptor(.array(.scalar(.date))), context: context.scalar)) {
            XCTAssertEqual(($0 as? JedError)?.code, "22007")
        }
        XCTAssertEqual(clock.reads, 0)
        _ = try ScalarCasts.cast(word, from: .text, to: date, context: context.scalar)
        _ = try context.evaluate(ResolvedExpression(type: date, node: .dateInput(SQLText("tomorrow")), position: 0))
        XCTAssertEqual(clock.reads, 1)
        // An already-resolved literal clock override keeps the test-adapter seam.
        var fixed = context; fixed.scalar.statementMicros = 90_000
        XCTAssertEqual(try fixed.now(), 90_000)
        XCTAssertEqual(clock.reads, 1)
    }

    func testExpressionCostGuardRunsBeforeAnyClockOrEntropyRead() throws {
        let clock = ServiceClockProbe(), random = ServiceRandomProbe()
        let services = StatementServices(settings: settings(clock: clock, random: random), meter: CostMeter(limit: 1))
        let context = services.context()
        let expression = ResolvedExpression(type: TypeDescriptor(.scalar(.timestamptz)), node: .currentTimestamp, position: 0)
        XCTAssertThrowsError(try context.evaluate(expression)) { XCTAssertEqual(($0 as? JedError)?.code, "54P01") }
        XCTAssertEqual(clock.reads, 0); XCTAssertTrue(random.counts.isEmpty)
    }

    func testUUIDDrawLengthsCounterWrapAndBlockReset() throws {
        let clock = ServiceClockProbe(1_000_000), random = ServiceRandomProbe()
        let settings = settings(clock: clock, random: random)
        let services = StatementServices(settings: settings, meter: CostMeter())
        let v4 = try uuid(services.call("uuidv4", []))
        XCTAssertEqual(v4[6], 0x4f); XCTAssertEqual(v4[8], 0xbf)
        XCTAssertEqual(clock.reads, 0); XCTAssertEqual(random.counts, [16])
        let first = try uuid(services.call("uuidv7", []))
        XCTAssertEqual(Array(first.prefix(8)), [0, 0, 0, 0, 3, 0xe8, 0x70, 0])
        for _ in 1..<4096 { _ = try services.call("uuidv7", []) }
        let wrapped = try uuid(services.call("uuidv7", []))
        XCTAssertEqual(Array(wrapped.prefix(8)), Array(first.prefix(8)))
        XCTAssertEqual(clock.reads, 1)
        XCTAssertEqual(random.counts.count, 4098)
        XCTAssertTrue(random.counts.dropFirst().allSatisfy { $0 == 8 })
        let second = StatementServices(settings: settings, meter: CostMeter())
        let reset = try uuid(second.call("uuidv7", []))
        XCTAssertEqual(Array(reset[6..<8]), [0x70, 0]); XCTAssertEqual(clock.reads, 2)
    }

    func testUUIDRejectedRangeAndHostSourceShapeDoNotTrap() throws {
        for value: Int64 in [-1, 281_474_976_710_656_000] {
            let clock = ManualClock(microseconds: value), random = ServiceRandomProbe()
            let services = StatementServices(settings: settings(clock: clock, random: random), meter: CostMeter())
            XCTAssertThrowsError(try services.call("uuidv7", [])) { XCTAssertEqual(($0 as? JedError)?.code, "22008") }
            XCTAssertTrue(random.counts.isEmpty)
        }
        let short = StatementServices(settings: settings(clock: ManualClock(microseconds: 0), random: ShortRandomSource()), meter: CostMeter())
        XCTAssertThrowsError(try short.call("uuidv4", [])) { XCTAssertEqual(($0 as? JedError)?.code, "22000") }
        XCTAssertThrowsError(try short.call("uuidv7", [])) { XCTAssertEqual(($0 as? JedError)?.code, "22000") }
    }

    func testSettingsSnapshotFallbackAndNullDoNotReadSources() throws {
        let clock = ServiceClockProbe(), random = ServiceRandomProbe()
        var settings = settings(clock: clock, random: random)
        try settings.setVariable(SQLText("app.tenant"), value: SQLText("first"))
        let services = StatementServices(settings: settings, meter: CostMeter())
        try settings.setVariable(SQLText("app.tenant"), value: SQLText("later"))
        guard case .text(let value) = try services.call("current_setting", [.text(SQLText("app.tenant"))]) else { return XCTFail("text expected") }
        XCTAssertEqual(value.description, "first")
        XCTAssertTrue(try services.call("current_setting", [.text(SQLText("missing")), .boolean(true)]).isNull)
        XCTAssertThrowsError(try services.call("current_setting", [.text(SQLText("missing"))])) { XCTAssertEqual(($0 as? JedError)?.code, "42704") }
        XCTAssertTrue(try services.call("uuidv7", [.null]).isNull)
        var context = ExpressionContext(meter: CostMeter())
        context.functions.stateCall = { name, _ in XCTAssertEqual(name, "nextval"); return .integer(17) }
        services.install(into: &context)
        guard case .integer(17) = try context.functions.stateCall?("nextval", []) else { return XCTFail("fallback result expected") }
        XCTAssertEqual(clock.reads, 0); XCTAssertTrue(random.counts.isEmpty)
    }
}
