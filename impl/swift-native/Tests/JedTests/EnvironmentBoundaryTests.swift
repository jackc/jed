import Foundation
import Synchronization
import XCTest
@testable import Jed

final class EnvironmentBoundaryTests: XCTestCase {
    private func fixture(_ name: String) throws -> [UInt8] {
        let url = try XCTUnwrap(Bundle.module.url(forResource: name, withExtension: nil, subdirectory: "Fixtures"))
        return Array(try Data(contentsOf: url))
    }
    private func integer(_ value: SQLValue, file: StaticString = #filePath, line: UInt = #line) throws -> Int64 {
        guard case .integer(let number) = value else {
            XCTFail("integer expected", file: file, line: line)
            throw JedError("XX000", "test expected integer")
        }
        return number
    }
    private func fails(_ code: String, _ body: () throws -> Void, file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertThrowsError(try body(), file: file, line: line) { XCTAssertEqual(($0 as? JedError)?.code, code, file: file, line: line) }
    }

    func testBuilderDerivationIsolatesLoadsAndRegistrations() throws {
        var builder = Environment.builder()
        let bare = builder.build()
        try builder.loadUnicodeData(fixture("unicode.jucd"))
        try builder.loadTimeZoneData(fixture("tzdata.jtz"))
        try builder.register(HostFunction(name: SQLText("first"), arguments: [], result: .i64, kernel: { _ in .integer(1) }))
        let original = builder.build()
        XCTAssertTrue(bare.unicode.loadedCollations.isEmpty)
        XCTAssertTrue(bare.timezones.zones.isEmpty)
        XCTAssertTrue(bare.signatures.isEmpty)
        XCTAssertNil(try bare.unicode.resolve(SQLText("C")))
        XCTAssertEqual(try bare.timezones.resolve(SQLText("UTC")).offset(at: 0).seconds, 0)
        XCTAssertFalse(original.timezones.zones.isEmpty)
        XCTAssertEqual(original.unicode.upper(try SQLText("éß")).description, "ÉSS")

        var derived = original.toBuilder()
        try derived.register(HostFunction(name: SQLText("second"), arguments: [], result: .i64, kernel: { _ in .integer(2) }))
        let child = derived.build()
        XCTAssertEqual(original.signatures.count, 1)
        XCTAssertEqual(child.signatures.count, 2)
        XCTAssertEqual(child.unicode.loadedCollations.map(\.contentHash), original.unicode.loadedCollations.map(\.contentHash))
        XCTAssertEqual(child.timezones.zones.map(\.contentHash), original.timezones.zones.map(\.contentHash))
        XCTAssertEqual(try integer(child.call(0, arguments: [])), 1)
        XCTAssertEqual(try integer(child.call(1, arguments: [])), 2)
        fails("42883") { _ = try original.call(1, arguments: []) }
        fails("XX001") { try derived.loadUnicodeData([0]) }
        fails("XX001") { try derived.loadTimeZoneData([0]) }
        XCTAssertEqual(derived.build().signatures.count, 2)
        XCTAssertEqual(derived.build().unicode.loadedCollations.map(\.contentHash), child.unicode.loadedCollations.map(\.contentHash))
        XCTAssertEqual(derived.build().timezones.zones.map(\.contentHash), child.timezones.zones.map(\.contentHash))
    }

    func testDerivedSourcesShareStateUntilExplicitlyReplaced() throws {
        let random = SeededRandomSource(seed: 0), clock = ManualClock(microseconds: 10)
        var builder = Environment.builder()
        builder.setRandomSource(random); builder.setClockSource(clock)
        let original = builder.build(), shared = original.toBuilder().build()
        XCTAssertTrue(original.randomSource === shared.randomSource)
        XCTAssertTrue(original.clockSource === shared.clockSource)
        XCTAssertEqual(try original.randomSource.bytes(count: 3), [0xe2, 0x20, 0xa8])
        XCTAssertEqual(try shared.randomSource.bytes(count: 5), [0x6e, 0x78, 0x9e, 0x6a, 0xa1])
        random.reseed(0)
        XCTAssertEqual(try shared.randomSource.bytes(count: 8), [0xe2, 0x20, 0xa8, 0x39, 0x7b, 0x1d, 0xcd, 0xaf])
        try clock.advance(7)
        XCTAssertEqual(try shared.clockSource.microseconds(), 17)
        clock.set(-4)
        XCTAssertEqual(try original.clockSource.microseconds(), -4)

        let replacement = AdvancingClock(start: 20, step: 3)
        var derived = original.toBuilder()
        derived.setClockSource(replacement); derived.setRandomSource(SeededRandomSource(seed: 0))
        let isolated = derived.build()
        XCTAssertFalse(isolated.randomSource === original.randomSource)
        XCTAssertEqual(try isolated.clockSource.microseconds(), 20)
        XCTAssertEqual(try isolated.clockSource.microseconds(), 23)
        replacement.reset(start: -1, step: -2)
        XCTAssertEqual(try isolated.clockSource.microseconds(), -1)
        XCTAssertEqual(try original.clockSource.microseconds(), -4)
        XCTAssertEqual(try isolated.randomSource.bytes(count: 8), [0xe2, 0x20, 0xa8, 0x39, 0x7b, 0x1d, 0xcd, 0xaf])
        fails("22023") { _ = try random.bytes(count: -1) }
        clock.set(.max)
        fails("22008") { try clock.advance(1) }
        XCTAssertEqual(clock.microseconds(), .max)
    }

    func testKeyStreamsRestartPerDatabaseAndIgnoreSQLReseeding() throws {
        let sql = SeededRandomSource(seed: 0)
        var builder = Environment.builder()
        builder.setRandomSource(sql); builder.useInsecureKeyEntropy(seed: 0)
        let environment = builder.build(), first = environment.keyEntropy(), second = environment.keyEntropy()
        let initial: [UInt8] = [0xe2, 0x20, 0xa8, 0x39, 0x7b, 0x1d, 0xcd, 0xaf]
        XCTAssertEqual(try first(8), initial)
        _ = try sql.bytes(count: 31); sql.reseed(999)
        XCTAssertEqual(try second(8), initial)
        XCTAssertEqual(try first(8), try second(8))
        XCTAssertEqual(try environment.toBuilder().build().keyEntropy()(8), initial)
        // Key draws also leave a newly reseeded SQL stream at its first draw.
        sql.reseed(0); _ = try first(19)
        XCTAssertEqual(try sql.bytes(count: 8), initial)
    }

    func testRegistrationRequiresOneKernelAndSignatureUniqueness() throws {
        let name = try SQLText("probe")
        var builder = Environment.builder()
        fails("22023") { try builder.register(HostFunction(name: name, arguments: [], result: .i64)) }
        fails("22023") { try builder.register(HostFunction(name: name, arguments: [], result: .i64, cost: -1, kernel: { _ in .integer(1) })) }
        fails("22023") { try builder.register(HostFunction(name: name, arguments: [], result: .i64, kernel: { _ in .integer(1) }, batchKernel: { _, output in output.append(.integer(1)) })) }
        var registered = HostFunction(name: name, arguments: [.i64], result: .i64, cost: 7, volatility: .stable, kernel: { $0[0] })
        try builder.register(registered)
        registered.cost = 99
        fails("42723") { try builder.register(HostFunction(name: name, arguments: [.i64], result: .text, kernel: { _ in .null })) }
        try builder.register(HostFunction(name: name, arguments: [.text], result: .text, kernel: { $0[0] }))
        let environment = builder.build()
        XCTAssertEqual(environment.signatures.count, 2)
        XCTAssertEqual(environment.signatures[0].cost, 7)
        XCTAssertEqual(environment.signatures[0].volatility, "stable")
        XCTAssertEqual(try integer(environment.call(0, arguments: [.integer(6)])), 6)
    }

    func testScalarAndBatchResultsEnforceDeclaredKindAndIntegerWidth() throws {
        let name = try SQLText("result")
        for batched in [false, true] {
            func function(_ type: ScalarType, _ value: SQLValue) -> HostFunction {
                if batched { return HostFunction(name: name, arguments: [], result: type, batchKernel: { _, output in output.append(value) }) }
                return HostFunction(name: name, arguments: [], result: type, kernel: { _ in value })
            }
            for (type, minimum, maximum): (ScalarType, Int64, Int64) in [(.i16, -32_768, 32_767), (.i32, -2_147_483_648, 2_147_483_647), (.i64, .min, .max)] {
                XCTAssertEqual(try integer(function(type, .integer(minimum)).evaluate([])), minimum)
                XCTAssertEqual(try integer(function(type, .integer(maximum)).evaluate([])), maximum)
                if type != .i64 {
                    for number in [minimum - 1, maximum + 1] {
                        XCTAssertThrowsError(try function(type, .integer(number)).evaluate([])) {
                            XCTAssertEqual(($0 as? JedError)?.code, "22003")
                            XCTAssertEqual(($0 as? JedError)?.message, "value out of range for type \(type.name)")
                        }
                    }
                }
                XCTAssertTrue(try function(type, .null).evaluate([]).isNull)
                fails("22000") { _ = try function(type, .text(SQLText("wrong"))).evaluate([]) }
            }
            fails("22000") { _ = try function(.text, .integer(1)).evaluate([]) }
        }
    }

    func testBatchOfOneStrictnessColumnShapeAndErrorPrefix() throws {
        let calls = Mutex(0), name = try SQLText("batch")
        let batch = HostFunction(name: name, arguments: [.i64, .i64], result: .i64, batchKernel: { columns, output in
            calls.withLock { $0 += 1 }
            XCTAssertEqual(columns.count, 2); XCTAssertEqual(columns.map(\.count), [1, 1])
            guard case .integer(let first) = columns[0][0], case .integer(let second) = columns[1][0] else { throw JedError("XX000", "test expected integer") }
            output.append(.integer(first + second))
        })
        XCTAssertTrue(try batch.evaluate([.integer(1), .null]).isNull)
        XCTAssertEqual(calls.withLock { $0 }, 0)
        XCTAssertEqual(try integer(batch.evaluate([.integer(2), .integer(3)])), 5)
        XCTAssertEqual(calls.withLock { $0 }, 1)
        fails("22000") { _ = try batch.evaluate([.null]) }
        XCTAssertEqual(calls.withLock { $0 }, 1)
        let zero = HostFunction(name: name, arguments: [], result: .i64, batchKernel: { columns, output in
            XCTAssertTrue(columns.isEmpty); output.append(.integer(9))
        })
        XCTAssertEqual(try integer(zero.evaluate([])), 9)
        let short = HostFunction(name: name, arguments: [], result: .i64, batchKernel: { _, _ in })
        let long = HostFunction(name: name, arguments: [], result: .i64, batchKernel: { _, output in output += [.integer(1), .integer(2)] })
        let failed = HostFunction(name: name, arguments: [], result: .i64, batchKernel: { _, _ in throw JedError("22012", "host failure") })
        let late = HostFunction(name: name, arguments: [], result: .i64, batchKernel: { _, output in output.append(.integer(1)); throw JedError("22012", "host failure") })
        fails("22000") { _ = try short.evaluate([]) }
        fails("22000") { _ = try long.evaluate([]) }
        fails("22012") { _ = try failed.evaluate([]) }
        fails("22000") { _ = try late.evaluate([]) }
        let scalar = HostFunction(name: name, arguments: [.i64], result: .i64, kernel: { _ in throw JedError("22012", "host failure") })
        XCTAssertTrue(try scalar.evaluate([.null]).isNull)
        fails("22012") { _ = try scalar.evaluate([.integer(1)]) }
    }
}
