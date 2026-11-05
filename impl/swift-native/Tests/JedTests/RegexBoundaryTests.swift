import Foundation
import XCTest
@testable import Jed

/// Shared NFA listings and step costs are not expressible through SQL; the spec
/// explicitly requires these cross-core bytecode/capture fixtures in each core.
final class RegexBoundaryTests: XCTestCase {
    private func vectors(_ name: String) throws -> [[String: Any]] {
        let url = try XCTUnwrap(Bundle.module.url(forResource: "SpecSnapshot", withExtension: "json", subdirectory: "Fixtures"))
        let root = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
        let documents = try XCTUnwrap(root["documents"] as? [String: [String: Any]])
        return try XCTUnwrap(documents["spec/regex/\(name)_vectors.toml"]?["case"] as? [[String: Any]])
    }
    private func compile(_ row: [String: Any]) throws -> RegexProgram {
        let flags = row["flags"] as? String ?? ""
        return try RegexProgram.compile(pattern: SQLText(XCTUnwrap(row["pattern"] as? String)), options: RegexOptions(ignoreCase: flags.contains("i"), lineAnchors: flags.contains("m")))
    }
    func testSharedProgramListingsAndClassOrder() throws {
        for row in try vectors("program") {
            let program = try compile(row)
            let name = row["pattern"] as? String ?? ""
            XCTAssertEqual(program.listing, try XCTUnwrap(row["prog"] as? [String]), name)
            XCTAssertEqual(program.classListing, row["classes"] as? [String] ?? [], name)
            XCTAssertEqual(program.instructionCount, row["count"] as? Int, name)
            let meter = CostMeter(); try program.chargeCompile(meter)
            XCTAssertEqual(meter.accrued, Int64(program.instructionCount), name)
        }
    }
    func testSharedCapturesAndExactStepCounts() throws {
        for row in try vectors("match") {
            let program = try compile(row), input = try SQLText(XCTUnwrap(row["input"] as? String))
            let context = "\(row["pattern"] ?? "") / \(input.description)"
            let scratch = RegexScratch()
            for _ in 0..<2 {
                let meter = CostMeter(), match = try program.search(input, meter: meter, scratch: scratch)
                XCTAssertEqual(match != nil, row["matched"] as? Bool, context)
                XCTAssertEqual(match?.captureSpans ?? [], row["caps"] as? [[Int]] ?? [], context)
                XCTAssertEqual(meter.accrued, try XCTUnwrap(row["steps"] as? NSNumber).int64Value, context)
            }
        }
    }
    func testSharedSimilarTranslationProgramsAndRuns() throws {
        for row in try vectors("similar") {
            let pattern = try SQLText(XCTUnwrap(row["pattern"] as? String))
            let escape = try (row["escape"] as? String).map(SQLText.init)
            if let error = row["error"] as? String {
                XCTAssertThrowsError(try Similar.translate(pattern, escape: escape)) { XCTAssertEqual(($0 as? JedError)?.code, error) }
                continue
            }
            let translated = try Similar.translate(pattern, escape: escape)
            XCTAssertEqual(translated.text.description, row["translation"] as? String, pattern.description)
            let input = try SQLText(XCTUnwrap(row["input"] as? String))
            let program = try RegexProgram.compile(pattern: translated.text, options: RegexOptions(dotAll: true))
            XCTAssertEqual(program.instructionCount, row["similar_compile"] as? Int, pattern.description)
            let meter = CostMeter()
            XCTAssertEqual(try program.isMatch(input, meter: meter), row["similar"] as? Bool, pattern.description)
            XCTAssertEqual(meter.accrued, try XCTUnwrap(row["similar_steps"] as? NSNumber).int64Value, pattern.description)
            if let error = row["substring_error"] as? String {
                XCTAssertThrowsError(try Similar.compileSubstring(translated)) { XCTAssertEqual(($0 as? JedError)?.code, error) }
                continue
            }
            let substring = try Similar.compileSubstring(translated), substringMeter = CostMeter()
            XCTAssertEqual(substring.instructionCount, row["substring_compile"] as? Int, pattern.description)
            XCTAssertEqual(try substring.match(input, meter: substringMeter)?.description, row["substring"] as? String, pattern.description)
            XCTAssertEqual(substringMeter.accrued, try XCTUnwrap(row["substring_steps"] as? NSNumber).int64Value, pattern.description)
        }
    }
    func testStructuralAdmissionAndPersistentCaptureBranches() throws {
        XCTAssertEqual(try RegexProgram.compile(pattern: SQLText("a{32762}")).instructionCount, 32768)
        for pattern in ["a{32763}", "a{999999999999999999999999999999}", "((ab){10000}){10000}"] {
            XCTAssertThrowsError(try RegexProgram.compile(pattern: SQLText(pattern))) { XCTAssertEqual(($0 as? JedError)?.code, "54001") }
        }
        XCTAssertThrowsError(try RegexProgram.compile(pattern: SQLText("a{999999999999999999999,2}"))) { XCTAssertEqual(($0 as? JedError)?.code, "2201B") }
        let pattern = String(repeating: "()", count: 4000)
        let program = try RegexProgram.compile(pattern: SQLText(pattern))
        let matched = try XCTUnwrap(program.search(SQLText(""), meter: CostMeter()))
        XCTAssertEqual(matched.groupCount, 4000)
        XCTAssertEqual(matched.captureSpans, Array(repeating: [0, 0], count: 4001))
        let nested = try RegexProgram.compile(pattern: SQLText(String(repeating: "(?:", count: 10_000) + "x" + String(repeating: ")", count: 10_000)))
        XCTAssertEqual(nested.instructionCount, 7)
        XCTAssertTrue(try nested.isMatch(SQLText("x"), meter: CostMeter()))
        let hostile = try RegexProgram.compile(pattern: SQLText("(a+)+b"))
        XCTAssertThrowsError(try hostile.isMatch(SQLText(String(repeating: "a", count: 1000)), meter: CostMeter(limit: 100))) { XCTAssertEqual(($0 as? JedError)?.code, "54P01") }
    }

    func testCaptureAndReplacementAdmissionBeforeConstruction() throws {
        let source = try SQLText("é"), pattern = try SQLText("(é)(x)?")
        let program = try RegexProgram.compile(pattern: pattern), searchMeter = CostMeter()
        _ = try program.search(source, meter: searchMeter)
        let resources = StatementResources(scalarLimit: 1000), meter = CostMeter(resources: resources)
        let captured = try RegexFunctions.call(name: "regexp_match", args: [.text(source), .text(pattern)], program: program, chargeCompile: false, meter: meter)
        XCTAssertEqual(resources.scalarUsed, MemorySizes.payload(captured))
        XCTAssertEqual(meter.accrued, searchMeter.accrued + MemorySizes.payload(captured))
        let denied = StatementResources(scalarLimit: MemorySizes.payload(captured) - 1)
        XCTAssertThrowsError(try RegexFunctions.call(name: "regexp_match", args: [.text(source), .text(pattern)], program: program, chargeCompile: false, meter: CostMeter(resources: denied))) {
            XCTAssertEqual(($0 as? JedError)?.code, "54P04")
        }
        XCTAssertEqual(denied.scalarUsed, 0)

        let replacement = try SQLText("\\1\\1\\\\"), replacementResources = StatementResources(scalarLimit: 1000)
        let replacementMeter = CostMeter(resources: replacementResources)
        let replaced = try RegexFunctions.call(name: "regexp_replace", args: [.text(source), .text(pattern), .text(replacement)], program: program, chargeCompile: false, meter: replacementMeter)
        XCTAssertEqual(replacementResources.scalarUsed, MemorySizes.payload(replaced))
        XCTAssertEqual(replacementMeter.accrued, searchMeter.accrued + Int64(replacement.utf8.count) + MemorySizes.payload(replaced))
    }
    func testRetainedProducerAdmissionAndPerRowWork() throws {
        let source = try SQLText("abc"), pattern = try SQLText("b*"), global = try SQLText("g")
        let args: [SQLValue] = [.text(source), .text(pattern), .text(global)]
        let resources = StatementResources(scalarLimit: 1000), meter = CostMeter(resources: resources)
        let rows = try RegexFunctions.rows(name: "regexp_matches", args: args, meter: meter)
        let preparedCost = meter.accrued, admitted = resources.scalarUsed
        XCTAssertGreaterThan(admitted, 0)
        var payload: Int64 = 0, count: Int64 = 0
        while let row = try rows.next() {
            count += 1; payload += MemorySizes.payload(row)
            XCTAssertEqual(meter.accrued, preparedCost + count)
            XCTAssertEqual(resources.scalarUsed, admitted)
        }
        XCTAssertEqual(payload, admitted)
        XCTAssertEqual(count, 4)
        XCTAssertNil(try rows.next())
        XCTAssertEqual(meter.accrued, preparedCost + count)

        let limitedMeter = CostMeter(limit: preparedCost + 1)
        let limited = try RegexFunctions.rows(name: "regexp_matches", args: args, meter: limitedMeter)
        XCTAssertThrowsError(try limited.next()) { XCTAssertEqual(($0 as? JedError)?.code, "54P01") }
        XCTAssertNil(try limited.next())
        let denied = StatementResources(scalarLimit: admitted - 1)
        XCTAssertThrowsError(try RegexFunctions.rows(name: "regexp_matches", args: args, meter: CostMeter(resources: denied))) {
            XCTAssertEqual(($0 as? JedError)?.code, "54P04")
        }
        XCTAssertEqual(denied.scalarUsed, 0)
    }

    func testSplitProducerCloseAndPartitionOwnership() throws {
        let source = try SQLText("ébc"), meter = CostMeter()
        let rows = try RegexFunctions.rows(name: "regexp_split_to_table", args: [.text(source), .text(SQLText(""))], meter: meter)
        let searchCost = meter.accrued
        var parts: [UInt8] = [], count: Int64 = 0
        while let row = try rows.next() {
            guard case .text(let text) = row else { return XCTFail("split row has wrong type") }
            parts += text.utf8; count += 1
        }
        XCTAssertEqual(parts, source.utf8)
        XCTAssertEqual(meter.accrued, searchCost + count)
        XCTAssertEqual(meter.resources.scalarUsed, 0)
        let early = try RegexFunctions.rows(name: "regexp_split_to_table", args: [.text(source), .text(SQLText(""))], meter: CostMeter())
        XCTAssertNotNil(try early.next()); early.close(); XCTAssertNil(try early.next())
        let emptyMeter = CostMeter()
        let empty = try RegexFunctions.rows(name: "regexp_matches", args: [.null, .text(SQLText("x"))], meter: emptyMeter)
        XCTAssertNil(try empty.next()); XCTAssertEqual(emptyMeter.accrued, 0)
    }
}
