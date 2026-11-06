import Foundation
import XCTest
@testable import Jed

final class StoredDependencyTests: XCTestCase {
    private func expression(_ sql: String, environment: Environment) throws -> ResolvedExpression {
        let resolver = ExpressionResolver()
        resolver.hostFunctions = environment.signatures; resolver.unicode = environment.unicode
        return try resolver.resolve(SQLParser(sql).parseExpressionOnly())
    }
    private func hostEnvironment(component: String? = "component", version: UInt32 = 1,
                                 volatility: HostFunction.Volatility = .immutable) throws -> Environment {
        var builder = Environment.builder()
        try builder.register(HostFunction(name: SQLText("host_value"), arguments: [.i64], result: .i64,
            volatility: volatility, componentID: component.map { SQLText(validatedUTF8: Array($0.utf8)) }, semanticVersion: version,
            kernel: { $0[0] }))
        return builder.build()
    }
    func testHostPinsDeduplicateAndValidateResolvedCalls() throws {
        let environment = try hostEnvironment()
        let value = try expression("coalesce(host_value(1), host_value(2))", environment: environment)
        let dependencies = try StoredExpressionDependencies.hosts([value], environment: environment, immutable: true)
        XCTAssertEqual(dependencies.count, 1)
        try StoredExpressionDependencies.validateHosts(dependencies, environment: environment)
        try StoredExpressionDependencies.validateHosts(dependencies, expressions: [value], environment: environment)
        for altered in [try hostEnvironment(version: 2), try hostEnvironment(component: "different"), Environment.builder().build()] {
            XCTAssertThrowsError(try StoredExpressionDependencies.validateHosts(dependencies, environment: altered)) {
                XCTAssertEqual(($0 as? JedError)?.code, "XX002")
            }
        }
        try StoredExpressionDependencies.validateHosts(dependencies, expressions: [], environment: environment)
        XCTAssertThrowsError(try StoredExpressionDependencies.validateHosts([], expressions: [value], environment: environment)) {
            XCTAssertEqual(($0 as? JedError)?.code, "XX002")
        }
        XCTAssertThrowsError(try expression("host_value(1)", environment: Environment.builder().build())) {
            XCTAssertEqual(($0 as? JedError)?.code, "42883")
        }
        let stable = try hostEnvironment(volatility: .stable), stableValue = try expression("host_value(1)", environment: stable)
        XCTAssertEqual(try StoredExpressionDependencies.hosts([stableValue], environment: stable).count, 1)
        XCTAssertThrowsError(try StoredExpressionDependencies.hosts([stableValue], environment: stable, immutable: true)) {
            XCTAssertEqual(($0 as? JedError)?.code, "42P17")
        }
    }
    func testFoldedContainerInputsRetainSessionDependency() throws {
        let environment = Environment.builder().build()
        let relative = try expression("'{\"2024-01-01 00:00:00\"}'::timestamptz[]", environment: environment)
        XCTAssertTrue(relative.nonimmutable)
        XCTAssertThrowsError(try IndexDependencies.resolve([relative], environment: environment)) {
            XCTAssertEqual(($0 as? JedError)?.code, "42P17")
        }
        let absolute = try expression("'{\"2024-01-01 00:00:00+00\",infinity}'::timestamptz[]", environment: environment)
        XCTAssertFalse(absolute.nonimmutable)
        XCTAssertTrue(try IndexDependencies.resolve([absolute], environment: environment).zones.isEmpty)
    }
    func testNamedTimezonePinsFailClosedInBareEnvironment() throws {
        let path = try XCTUnwrap(Bundle.module.url(forResource: "tzdata", withExtension: "jtz", subdirectory: "Fixtures"))
        var builder = Environment.builder(); try builder.loadTimeZoneData([UInt8](Data(contentsOf: path)))
        let environment = builder.build()
        let value = try expression("timezone('America/New_York', TIMESTAMP '2024-01-01')", environment: environment)
        let facts = try IndexDependencies.resolve([value], environment: environment)
        XCTAssertEqual(facts.zones.map { $0.name.description }, ["America/New_York"])
        let codec = try PlainPageCodec(pageSize: 8192)
        var index = CatalogIndex(name: try SQLText("i"), keys: [], root: PageSealer.nullReference(codec))
        index.zones = facts.zones
        try IndexDependencies.validateZones(index, environment: environment)
        XCTAssertThrowsError(try IndexDependencies.validateZones(index, environment: Environment.builder().build())) {
            XCTAssertEqual(($0 as? JedError)?.code, "XX002")
        }
    }
}
