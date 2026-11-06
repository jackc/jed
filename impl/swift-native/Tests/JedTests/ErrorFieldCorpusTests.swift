import Foundation
import XCTest
@testable import Jed

/// Structured diagnostics are shared host-API fixtures, outside sqllogictest's
/// SQLSTATE-only error assertions. Every case runs through a public session.
final class ErrorFieldCorpusTests: XCTestCase {
    private struct Case {
        let name: String
        var fields: [String: String] = [:]
        var setup: [String] = []
        var attach: [String] = []
        var grants: [String] = []
        var revokes: [String] = []
    }
    private func cases(_ file: String, position: Bool) throws -> [Case] {
        let url = try XCTUnwrap(Bundle.module.url(forResource: file, withExtension: "txt", subdirectory: "Fixtures"))
        let source = try String(contentsOf: url, encoding: .utf8)
        var result: [Case] = []
        let singular: Set<String> = position
            ? ["run", "script", "code", "position", "no-position", "hint", "detail", "database", "load-unicode"]
            : ["run", "code", "detail", "no-detail", "message", "default_privileges"]
        for raw in source.components(separatedBy: "\n") {
            if raw.trimmingCharacters(in: .whitespaces).isEmpty || raw.hasPrefix("#") { continue }
            if raw.hasPrefix("case: ") {
                result.append(Case(name: String(raw.dropFirst(6)))); continue
            }
            guard !result.isEmpty else { throw SQLCorpusError("error-field directive precedes first case") }
            let delimiter = raw.range(of: ": ")
            let key = delimiter.map { String(raw[..<$0.lowerBound]) } ?? raw
            let value = delimiter.map { String(raw[$0.upperBound...]) } ?? ""
            let i = result.count - 1
            switch key {
            case "setup": result[i].setup.append(value)
            case "attach" where position: result[i].attach.append(value)
            case "grant" where !position: result[i].grants.append(value)
            case "revoke" where !position: result[i].revokes.append(value)
            default:
                guard singular.contains(key), result[i].fields[key] == nil else {
                    throw SQLCorpusError("unknown/duplicate error-field directive: \(raw)")
                }
                result[i].fields[key] = value
            }
        }
        guard Set(result.map(\.name)).count == result.count else { throw SQLCorpusError("duplicate error-field case name") }
        for item in result {
            guard item.fields["code"]?.utf8.count == 5,
                  (item.fields["run"] != nil) != (item.fields["script"] != nil) else {
                throw SQLCorpusError("incomplete error-field case: \(item.name)")
            }
            if position {
                guard (item.fields["position"] != nil) != (item.fields["no-position"] != nil),
                      item.fields["position"].map({ (Int($0) ?? 0) > 0 }) ?? true else {
                    throw SQLCorpusError("invalid expected Position: \(item.name)")
                }
            } else if (item.fields["detail"] != nil) == (item.fields["no-detail"] != nil) {
                throw SQLCorpusError("missing/conflicting expected Detail: \(item.name)")
            }
        }
        return result
    }
    private func privileges(_ text: String) throws -> PrivilegeSet {
        if text == "-" { return [] }
        var result: PrivilegeSet = []
        for name in text.split(separator: ",", omittingEmptySubsequences: false) {
            switch name.uppercased() {
            case "SELECT": result.insert(.select)
            case "INSERT": result.insert(.insert)
            case "UPDATE": result.insert(.update)
            case "DELETE": result.insert(.delete)
            case "EXECUTE": result.insert(.execute)
            default: throw SQLCorpusError("unknown error-field privilege: \(name)")
            }
        }
        return result
    }
    private func sameText(_ actual: String?, _ expected: String?, _ label: String) {
        XCTAssertTrue(actual.map { Array($0.utf8) } == expected.map { Array($0.utf8) },
                      "\(label): got \(String(reflecting: actual)), expected \(String(reflecting: expected))")
    }
    private func drain(_ sql: String, session: Session) throws {
        let rows = try session.query(sql); defer { rows.close() }
        while try rows.next() != nil {}
    }
    private func run(_ item: Case, position: Bool) throws {
        var builder = Environment.builder()
        if item.fields["load-unicode"] != nil {
            let url = try XCTUnwrap(Bundle.module.url(forResource: "unicode", withExtension: "jucd", subdirectory: "Fixtures"))
            try builder.loadUnicodeData(Array(Data(contentsOf: url)))
        }
        let database = try Database(environment: builder.build()); defer { try? database.close() }
        for name in item.attach { try database.attach(SQLText(name), source: .memory, readOnly: false) }
        let session = try database.session(); defer { session.close() }
        for sql in item.setup { try drain(sql, session: session) }
        if let value = item.fields["default_privileges"] { session.defaultPrivileges = try privileges(value) }
        for (granting, entries) in [(true, item.grants), (false, item.revokes)] {
            for entry in entries {
                guard let space = entry.firstIndex(of: " ") else { throw SQLCorpusError("invalid privilege/object directive") }
                let set = try privileges(String(entry[..<space])), name = try SQLText(String(entry[entry.index(after: space)...]))
                if granting { session.grant(set, on: name) } else { session.revoke(set, on: name) }
            }
        }
        var failure: JedError?
        do {
            if let script = item.fields["script"] { _ = try session.executeScript(script) }
            else { try drain(try XCTUnwrap(item.fields["run"]), session: session) }
        } catch let error as JedError { failure = error }
        let error = try XCTUnwrap(failure, "\(item.name): expected failure, statement succeeded")
        XCTAssertEqual(error.code, item.fields["code"], item.name)
        sameText(error.detail, item.fields["detail"], "\(item.name): Detail")
        if position {
            XCTAssertEqual(error.position, item.fields["position"].flatMap(Int.init), "\(item.name): Position")
            sameText(error.hint, item.fields["hint"], "\(item.name): Hint")
            sameText(error.databaseName, item.fields["database"], "\(item.name): DatabaseName")
        } else if let expected = item.fields["message"] {
            sameText(error.message, expected, "\(item.name): Message")
        }
    }
    private func check(_ file: String, count: Int, position: Bool) throws {
        let inputs = try cases(file, position: position)
        XCTAssertEqual(inputs.count, count)
        for item in inputs {
            do { try run(item, position: position) }
            catch { XCTFail("\(file)/\(item.name): \(error)") }
        }
    }
    func testStructuredObjectDiagnosticFields() throws {
        struct Diagnostic {
            let name: String
            let setup: [String]
            let sql: String
            let code: String
            var constraint: String? = nil
            var table: String? = nil
            var column: String? = nil
            var type: String? = nil
        }
        let cases = [
            Diagnostic(name: "primary key", setup: ["CREATE TABLE t(id i32 PRIMARY KEY)", "INSERT INTO t VALUES(1)"],
                sql: "INSERT INTO t VALUES(1)", code: "23505", constraint: "t_pkey", table: "t"),
            Diagnostic(name: "unique index", setup: ["CREATE TABLE t(id i32 PRIMARY KEY,email text)",
                "CREATE UNIQUE INDEX t_email_key ON t(email)", "INSERT INTO t VALUES(1,'a')"],
                sql: "INSERT INTO t VALUES(2,'a')", code: "23505", constraint: "t_email_key", table: "t"),
            Diagnostic(name: "check", setup: ["CREATE TABLE t(id i32 PRIMARY KEY,n i32 CONSTRAINT n_pos CHECK(n>0))"],
                sql: "INSERT INTO t VALUES(1,-1)", code: "23514", constraint: "n_pos", table: "t"),
            Diagnostic(name: "foreign key child", setup: ["CREATE TABLE p(id i32 PRIMARY KEY)",
                "CREATE TABLE c(id i32 PRIMARY KEY,pid i32 CONSTRAINT c_pid_fk REFERENCES p)"],
                sql: "INSERT INTO c VALUES(1,99)", code: "23503", constraint: "c_pid_fk", table: "c"),
            Diagnostic(name: "foreign key parent", setup: ["CREATE TABLE p(id i32 PRIMARY KEY)",
                "CREATE TABLE c(id i32 PRIMARY KEY,pid i32 CONSTRAINT c_pid_fk REFERENCES p)",
                "INSERT INTO p VALUES(1)", "INSERT INTO c VALUES(1,1)"],
                sql: "DELETE FROM p WHERE id=1", code: "23503", constraint: "c_pid_fk", table: "p"),
            Diagnostic(name: "exclusion", setup: ["CREATE TABLE t(id i32 PRIMARY KEY,r i32range,CONSTRAINT t_r_excl EXCLUDE USING gist(r WITH &&))",
                "INSERT INTO t VALUES(1,'[1,5)')"], sql: "INSERT INTO t VALUES(2,'[3,8)')",
                code: "23P01", constraint: "t_r_excl", table: "t"),
            Diagnostic(name: "not null", setup: ["CREATE TABLE t(id i32 PRIMARY KEY,n i32 NOT NULL)"],
                sql: "INSERT INTO t VALUES(1,NULL)", code: "23502", table: "t", column: "n"),
            Diagnostic(name: "integer overflow", setup: ["CREATE TABLE t(id i32 PRIMARY KEY,n i16)"],
                sql: "INSERT INTO t VALUES(1,99999)", code: "22003", table: "t", type: "i16"),
            Diagnostic(name: "varchar length", setup: ["CREATE TABLE t(id i32 PRIMARY KEY,s varchar(3))"],
                sql: "INSERT INTO t VALUES(1,'abcd')", code: "22001", table: "t", column: "s", type: "varchar(3)"),
            Diagnostic(name: "unrelated resolution error", setup: ["CREATE TABLE t(id i32 PRIMARY KEY)"],
                sql: "SELECT nonesuch FROM t", code: "42703"),
        ]
        for item in cases {
            do {
                let database = try Database(); defer { try? database.close() }
                let session = try database.session(); defer { session.close() }
                for sql in item.setup { try drain(sql, session: session) }
                var failure: JedError?
                do { try drain(item.sql, session: session) } catch let error as JedError { failure = error }
                let error = try XCTUnwrap(failure, item.name)
                XCTAssertEqual(error.code, item.code, item.name)
                sameText(error.constraintName, item.constraint, "\(item.name): ConstraintName")
                sameText(error.tableName, item.table, "\(item.name): TableName")
                // The frozen overflow case promises its type/table, without
                // requiring a column label on an expression conversion error.
                if item.code != "22003" { sameText(error.columnName, item.column, "\(item.name): ColumnName") }
                sameText(error.dataTypeName, item.type, "\(item.name): DataTypeName")
            } catch { XCTFail("object diagnostic/\(item.name): \(error)") }
        }
    }
    func testAllSharedDetailCases() throws { try check("detail_cases", count: 61, position: false) }
    func testAllSharedPositionHintCases() throws { try check("position_cases", count: 346, position: true) }
}
