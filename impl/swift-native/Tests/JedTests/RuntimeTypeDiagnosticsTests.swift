import XCTest
@testable import Jed

/// Structured error fields are an embedding contract; the shared SQL corpus
/// separately owns each operation's value, SQLSTATE and cost behavior.
final class RuntimeTypeDiagnosticsTests: XCTestCase {
    func testTypedOverflowAndGenericDomainErrorsRemainDistinct() throws {
        var builder = Environment.builder()
        try builder.register(HostFunction(name: SQLText("too_wide"), arguments: [], result: .i16,
            kernel: { _ in .integer(100_000) }))
        let database = try Database(environment: builder.build()); defer { try? database.close() }
        struct Probe {
            let sql: String
            var parameters: [SQLValue] = []
            let type: String?
        }
        let probes = [
            Probe(sql: "SELECT $1::i16 + 1::i16", parameters: [.integer(32_767)], type: "i16"),
            Probe(sql: "SELECT -($1::i64)", parameters: [.integer(.min)], type: "i64"),
            Probe(sql: "SELECT $1::i64 + 1::i64",
                parameters: [.integer(.max)], type: "i64"),
            Probe(sql: "SELECT exp(1000000::decimal)", type: "decimal"),
            Probe(sql: "SELECT '[1,2147483647]'::i32range", type: "i32"),
            Probe(sql: "SELECT too_wide()", type: "i16"),
            Probe(sql: "SELECT '1e-9999'::f32", type: "f32"),
            Probe(sql: "SELECT 'NaN'::f64::i32", type: "i32"),
            Probe(sql: "SELECT '3e38'::f32 + '3e38'::f32", type: "f32"),
            Probe(sql: "SELECT exp(710::f32)", type: "f64"),
            Probe(sql: "SELECT sum(x) FROM (VALUES ('3e38'::f32), ('3e38'::f32)) AS v(x)", type: "f32"),
            Probe(sql: "SELECT var_pop(x) FROM (VALUES ('1e200'::f64), ('-1e200'::f64)) AS v(x)", type: "f64"),
            Probe(sql: "SELECT '1e100'::f64::f32", type: "f32"),
            Probe(sql: "SELECT '1e400'::decimal::f64", type: "f64"),
            Probe(sql: "SELECT 'Infinity'::f64::decimal", type: "decimal"),
            Probe(sql: "SELECT '1e-30'::f32 * '1e-30'::f32", type: nil),
            Probe(sql: "SELECT '1e-100'::f64::f32", type: nil),
            Probe(sql: "SELECT '1e-400'::decimal::f64", type: nil),
            Probe(sql: "SELECT asin(2::f64)", type: nil),
            Probe(sql: "SELECT factorial(-1)", type: nil),
        ]
        for probe in probes {
            var failure: JedError?
            do {
                let rows = try database.query(probe.sql, parameters: probe.parameters)
                defer { rows.close() }
                while try rows.next() != nil {}
            } catch let error as JedError { failure = error }
            let error = try XCTUnwrap(failure, probe.sql)
            XCTAssertEqual(error.code, "22003", probe.sql)
            XCTAssertEqual(error.dataTypeName, probe.type, probe.sql)
            XCTAssertNil(error.tableName, probe.sql)
            XCTAssertNil(error.columnName, probe.sql)
            XCTAssertNil(error.databaseName, probe.sql)
        }
    }
}
