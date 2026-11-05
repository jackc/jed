import Foundation
import XCTest
@testable import Jed

/// Arithmetic representation invariants and shared byte contracts. SQL behavior
/// belongs to the shared conformance runner once that engine path is available.
final class DecimalTests: XCTestCase {
    private func decimal(_ text: String) throws -> SQLDecimal { try SQLDecimal.parse(text) }

    func testExactIEEEExpansionRoundTripsAndEvenRounding() throws {
        var state: UInt64 = 0xa917_e321_8426_5823
        for _ in 0..<1000 {
            state = state &* 6_364_136_223_846_793_005 &+ 1
            let value = Double(bitPattern: state)
            if value.isFinite && value != 0 {
                let expanded = try BinaryFloatDecimal.exact(bits64: state)
                XCTAssertEqual(Double(expanded.description)?.bitPattern, state)
            }
            let narrowBits = UInt32(truncatingIfNeeded: state)
            let narrow = Float(bitPattern: narrowBits)
            if narrow.isFinite && narrow != 0 {
                let expanded = try BinaryFloatDecimal.exact(bits32: narrowBits)
                XCTAssertEqual(Float(expanded.description)?.bitPattern, narrowBits)
            }
        }
        for (input, expected) in [("1.25", "1.2"), ("1.35", "1.4"), ("-1.25", "-1.2"), ("-1.35", "-1.4")] {
            XCTAssertEqual(BinaryFloatDecimal.roundedEven(try decimal(input), scale: 1).description, expected)
        }
        for value in [Double.infinity, -.infinity, .nan] {
            assertCode("22003") { _ = try BinaryFloatDecimal.exact(bits64: value.bitPattern) }
        }
    }

    func testRomanProcessorRoundTripAndFormattingOutputMeter() throws {
        let meter = CostMeter(), template = try SQLText("FMRN")
        for value: Int64 in 1...3999 {
            let rendered = try NumericFormat.toChar(.integer(value), type: .i32, template: template)
            guard case .text(let text) = rendered else { return XCTFail("expected text") }
            let parsed = try NumericFormat.toNumber(text, template: template, meter: meter)
            guard case .decimal(let number) = parsed else { return XCTFail("expected decimal") }
            XCTAssertEqual(number, SQLDecimal(value))
        }
        XCTAssertEqual(meter.accrued, 0)
        let rendered = try NumericFormat.toChar(.decimal(decimal("1.25")), type: .decimal, template: SQLText("\u{FEFF}FM00.00"), meter: meter)
        guard case .text(let text) = rendered else { return XCTFail("expected text") }
        XCTAssertEqual(meter.accrued, Int64(text.utf8.count))
        XCTAssertEqual(text.utf8.prefix(3), [239, 187, 191])
    }

    private func assertCode(_ code: String, _ operation: () throws -> Void,
                            file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertThrowsError(try operation(), file: file, line: line) {
            XCTAssertEqual(($0 as? JedError)?.code, code, file: file, line: line)
        }
    }

    func testMagnitudeUInt64Oracle() {
        var state: UInt64 = 0xc7e4_2138_1982_71a5
        func next() -> UInt64 {
            state = state &* 6_364_136_223_846_793_005 &+ 1
            return state
        }
        for _ in 0..<2_000 {
            let a = next()
            let b = next() | 1
            let left = BigMagnitude(a)
            let right = BigMagnitude(b)
            let division = left.divided(by: right)
            XCTAssertEqual(division.quotient.description, String(a / b))
            XCTAssertEqual(division.remainder.description, String(a % b))
            XCTAssertEqual(left.description, String(a))
            XCTAssertEqual(left.adding(right).subtracting(right), left)
            let small = a % 1_000_000_000
            let factor = b % 1_000_000_000
            XCTAssertEqual(BigMagnitude(small).multiplying(BigMagnitude(factor)).description, String(small * factor))
        }
    }

    func testMagnitudeLongDivisionEuclideanIdentity() {
        var state: UInt64 = 0x8bc2_d37a_714f_29e1
        func limb() -> UInt32 {
            state = state &* 2_862_933_555_777_941_757 &+ 3_037_000_493
            return UInt32(state % BigMagnitude.base)
        }
        for size in 2...24 {
            for _ in 0..<12 {
                let divisor = BigMagnitude((0..<size).map { _ in limb() })
                let quotient = BigMagnitude((0..<(size + 2)).map { _ in limb() })
                let remainder = BigMagnitude((0..<(size - 1)).map { _ in limb() })
                let dividend = quotient.multiplying(divisor).adding(remainder)
                let result = dividend.divided(by: divisor)
                XCTAssertEqual(result.quotient, quotient)
                XCTAssertEqual(result.remainder, remainder)
                XCTAssertLessThan(result.remainder.compare(to: divisor), 0)
            }
        }
        // Carry/borrow boundaries and a very small normalized leading limb.
        for limbs: [UInt32] in [[999_999_999, 999_999_999], [1, 0, 1], [0, 999_999_999, 1]] {
            let divisor = BigMagnitude(limbs)
            let dividend = divisor.multiplying(divisor).subtracting(BigMagnitude(1))
            let result = dividend.divided(by: divisor)
            XCTAssertEqual(result.quotient, divisor.subtracting(BigMagnitude(1)))
            XCTAssertEqual(result.remainder, divisor.subtracting(BigMagnitude(1)))
        }
    }

    func testMagnitudeDecimalSplitsAndNormalization() {
        let digits = Array("123456789012345678901234567890123456789".utf8)
        let value = BigMagnitude(decimalDigits: digits[...])
        for cut in 0...45 {
            let parts = value.splitDecimal(cut)
            XCTAssertEqual(parts.quotient.shiftedDecimal(cut).adding(parts.remainder), value)
            XCTAssertLessThan(parts.remainder.compare(to: BigMagnitude(1).shiftedDecimal(cut)), 0)
        }
        XCTAssertEqual(BigMagnitude([0, 0, 0]), BigMagnitude(0))
        XCTAssertEqual(BigMagnitude([1, 0, 0]), BigMagnitude(1))
    }

    func testRepresentationPreservesScaleWithoutNegativeZero() throws {
        for (input, output) in [("-0.000", "0.000"), ("+001.50", "1.50"),
                                (".5", "0.5"), ("1.", "1"), ("1.50e1", "15.0"),
                                ("1.50e-3", "0.00150"), ("\u{b}-0x_Ff\t", "-255"),
                                ("0o_17", "15"), ("0b_101", "5"), ("1_0.0_1e1_0", "100100000000"),
                                ("0e999999999999999999999", "0")] {
            XCTAssertEqual(try decimal(input).description, output, input)
        }
        XCTAssertEqual(SQLDecimal(Int64.min).description, String(Int64.min))
        XCTAssertEqual(try decimal("1.50"), try decimal("1.5"))
        XCTAssertEqual(try decimal("-0.4").rounded(to: 0).negative, false)
        XCTAssertEqual(try decimal("1.0").subtracting(decimal("1.0")).description, "0.0")
        XCTAssertEqual(try decimal("1.0000000000000000001").compare(to: decimal("1.0000000000000000000")), 1)
    }

    func testInvalidAndOversizedInputsAreStructured() {
        for input in ["", " ", "+", ".", "1__0", "1_", "1._0", "1_.0", "0x", "0x_", "0x__1",
                      "0b2", "0o8", "1e", "1e+", "1e_1", "1e1_", "1e1x", "NaN", "Infinity",
                      "1 2", "\u{a0}1", "1\u{0}"] {
            assertCode("22P02") { _ = try decimal(input) }
        }
        for input in ["1e131072", "1e-16384", "1e999999999999999999999", "1e-999999999999999999999"] {
            assertCode("22003") { _ = try decimal(input) }
        }
    }

    func testExactScaleArithmeticAndRoundingBoundaries() throws {
        XCTAssertEqual(try decimal("999999999.999999999").adding(decimal("0.000000001")).description, "1000000000.000000000")
        XCTAssertEqual(try decimal("1000000000.000000000").subtracting(decimal("0.000000001")).description, "999999999.999999999")
        XCTAssertEqual(try decimal("1.50").multiplying(decimal("1.5")).description, "2.250")
        for (input, scale, output) in [("0.125", 2, "0.13"), ("-0.125", 2, "-0.13"),
                                      ("-0.124999", 2, "-0.12"), ("999.5", 0, "1000"),
                                      ("-15", -1, "-20"), ("14.999", -1, "10"),
                                      ("0.1", 4, "0.1000")] {
            XCTAssertEqual(try decimal(input).rounded(to: scale).description, output)
        }
        XCTAssertEqual(try decimal("123").rounded(to: Int.min).description, "0")
        XCTAssertEqual(try decimal("-0.999").truncated(to: 2).description, "-0.99")
        XCTAssertEqual(try decimal("1.245").coerced(precision: 4, scale: 2).description, "1.25")
        assertCode("22003") { _ = try decimal("9.995").coerced(precision: 3, scale: 2) }
        assertCode("22023") { _ = try decimal("1").coerced(precision: 0, scale: 0) }
        assertCode("22023") { _ = try decimal("1").coerced(precision: 2, scale: 3) }
        XCTAssertEqual(try decimal("-9223372036854775808.4").integerValue(), Int64.min)
        assertCode("22003") { _ = try decimal("9223372036854775807.5").integerValue() }
    }

    func testDivisionScalesAndDeepScaleShifts() throws {
        for (left, right, result) in [("1", "3", "0.33333333333333333333"),
                                      ("2", "3", "0.66666666666666666667"),
                                      ("10.0", "4.0", "2.5000000000000000"),
                                      ("1.50", "1.5", "1.00000000000000000000"),
                                      ("-1", "8", "-0.12500000000000000000")] {
            XCTAssertEqual(try decimal(left).dividing(decimal(right)).description, result)
        }
        let deepZero = try decimal("1e-16383").dividing(SQLDecimal(1))
        XCTAssertTrue(deepZero.isZero)
        XCTAssertEqual(deepZero.scale, 1000)
        XCTAssertEqual(try decimal("5e-1001").dividing(SQLDecimal(1)), try decimal("1e-1000"))
        XCTAssertEqual(try decimal("4e-1001").dividing(SQLDecimal(1)), SQLDecimal(0))
        XCTAssertEqual(try decimal("1.5e-1001").dividing(decimal("1e-1001")), try decimal("1.5"))
        XCTAssertEqual(try decimal("-5.50").modulo(decimal("-2")).description, "-1.50")
        assertCode("22012") { _ = try decimal("1e-16383").dividing(SQLDecimal(0)) }
        assertCode("22012") { _ = try decimal("1").modulo(SQLDecimal(0)) }
    }

    func testValueCapsAndUncappedAggregateIntermediate() throws {
        let max = try decimal(String(repeating: "9", count: SQLDecimal.maximumIntegerDigits))
        let beyond = max.addingUncapped(SQLDecimal(1))
        assertCode("22003") { _ = try beyond.checked() }
        XCTAssertEqual(try beyond.addingUncapped(SQLDecimal(-1)).checked(), max)
        assertCode("22003") { _ = try max.rounded(to: -1) }
        let tinyProduct = try decimal("1e-10000").multiplying(decimal("1e-10000"))
        XCTAssertTrue(tinyProduct.isZero)
        XCTAssertEqual(tinyProduct.scale, SQLDecimal.maximumScale)
    }

    func testSharedDecimalKeyVectors() throws {
        let url = try XCTUnwrap(Bundle.module.url(forResource: "SpecSnapshot", withExtension: "json", subdirectory: "Fixtures"))
        let snapshot = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
        let documents = try XCTUnwrap(snapshot["documents"] as? [String: [String: Any]])
        let vectors = try XCTUnwrap(documents["spec/encoding/decimal.toml"])
        var count = 0
        for kind in ["bare", "nullable", "descending"] {
            let groups = try XCTUnwrap(vectors[kind] as? [[String: Any]])
            for group in groups {
                let cases = try XCTUnwrap(group["cases"] as? [[String: Any]])
                var previous: [UInt8]?
                for entry in cases {
                    let expected = try XCTUnwrap(entry["bytes"] as? String)
                    var actual: [UInt8]
                    if let value = entry["value"] as? String {
                        actual = try decimal(value).keyBytes()
                        if kind != "bare" { actual.insert(0, at: 0) }
                    } else { actual = [1] }
                    if kind == "descending" { actual = actual.map { ~$0 } }
                    XCTAssertEqual(actual.map { String(format: "%02x", $0) }.joined(), expected)
                    if let previous { XCTAssertTrue(previous.lexicographicallyPrecedes(actual)) }
                    previous = actual
                    count += 1
                }
            }
        }
        XCTAssertEqual(count, 41)
        XCTAssertEqual(try decimal("1.5").keyBytes(), try decimal("1.50000").keyBytes())
        XCTAssertEqual(try decimal("0.000").keyBytes(), [4])
    }

    func testDecimalBodyByteBoundaryAndCorruption() throws {
        XCTAssertEqual(try decimal("1.50").encodedBody(), [0, 0, 2, 0, 1, 0, 150])
        XCTAssertEqual(try decimal("-12345.6789").encodedBody(), [1, 0, 4, 0, 3, 0, 1, 9, 41, 26, 133])
        for input in ["0", "0.00", "-1.50", "123456789012345678901234567890.000001", "1e-16383"] {
            let value = try decimal(input)
            let bytes = try value.encodedBody()
            let decoded = try SQLDecimal.decodeBody(bytes)
            XCTAssertEqual(decoded.description, value.description)
            XCTAssertEqual(decoded.canonicalDigitGroupCount, (bytes.count - 5) / 2)
            for end in bytes.indices { assertCode("XX001") { _ = try SQLDecimal.decodeBody(Array(bytes.prefix(end))) } }
        }
        for bytes: [UInt8] in [[2, 0, 0, 0, 0], [1, 0, 0, 0, 0], [0, 64, 0, 0, 0],
                               [0, 0, 0, 0, 1, 0, 0], [0, 0, 0, 0, 1, 39, 16], [0, 0, 0, 0, 0, 1]] {
            assertCode("XX001") { _ = try SQLDecimal.decodeBody(bytes) }
        }
    }

    func testIntegerRootFloorInvariant() {
        for value: UInt64 in [0, 1, 2, 3, 4, 15, 16, 17, 999_999_999, UInt64.max] {
            let input = BigMagnitude(value)
            let root = input.squareRoot()
            let successor = root.adding(BigMagnitude(1))
            XCTAssertLessThanOrEqual(root.multiplying(root).compare(to: input), 0)
            XCTAssertGreaterThan(successor.multiplying(successor).compare(to: input), 0)
        }
        let largeRoot = BigMagnitude(decimalDigits: Array("123456789012345678901234567890".utf8)[...])
        XCTAssertEqual(largeRoot.multiplying(largeRoot).squareRoot(), largeRoot)
        XCTAssertEqual(largeRoot.multiplying(largeRoot).subtracting(BigMagnitude(1)).squareRoot(),
                       largeRoot.subtracting(BigMagnitude(1)))
    }

    func testTranscendentalAdmissionUsesLogicalDigitsAndSharedAccount() throws {
        let value = try decimal("123.4500") // precision 7, scale 4
        let resources = StatementResources()
        let meter = CostMeter(resources: resources)
        // D = 32 + 12 + (7 + 4) = 55; ceil(D/4)^2 = 196.
        try DecimalTranscendental.admit(meter, scale: 12, values: [value])
        XCTAssertEqual(meter.accrued, 196)
        XCTAssertEqual(resources.scalarUsed, 440)
        let nested = CostMeter(resources: resources)
        try DecimalTranscendental.admit(nested, scale: -12, squareRoot: true, values: [value])
        XCTAssertEqual(nested.accrued, 196 * 6) // bit_length(55) = 6
        XCTAssertEqual(resources.scalarUsed, 880)

        let both = CostMeter(limit: 1, resources: StatementResources(scalarLimit: 1))
        assertCode("54P01") { _ = try DecimalFunctions.call(name: "sqrt", args: [value], meter: both) }
        XCTAssertEqual(both.resources.scalarUsed, 0)
        let memory = CostMeter(resources: StatementResources(scalarLimit: 1))
        assertCode("54P04") { _ = try DecimalFunctions.call(name: "sqrt", args: [value], meter: memory) }
        XCTAssertGreaterThan(memory.accrued, 0)
        XCTAssertEqual(memory.resources.scalarUsed, 0)
    }

    func testDecimalWorkDependsOnLogicalGroupsAndChecksBeforeWork() throws {
        let left = try decimal("123456789.0000")
        let right = try decimal("1.00000000")
        XCTAssertEqual(DecimalFunctions.work("+", left, right), 5)
        XCTAssertEqual(DecimalFunctions.work("*", left, right), 12)
        XCTAssertEqual(DecimalFunctions.work("%", left, right), 15)
        XCTAssertEqual(DecimalFunctions.work("/", try decimal("1e-16383"), SQLDecimal(1)), 3846)
        let meter = CostMeter(limit: 1)
        assertCode("54P01") { _ = try DecimalFunctions.binary("*", left, right, meter: meter) }
        XCTAssertEqual(meter.accrued, 11)
        // Zero-divisor failure precedes additional arithmetic work admission.
        let divisorMeter = CostMeter(limit: 1)
        divisorMeter.charge(1)
        assertCode("22012") { _ = try DecimalFunctions.binary("/", left, SQLDecimal(0), meter: divisorMeter) }
        XCTAssertEqual(divisorMeter.accrued, 1)
    }

    func testGuardedKernelsKeepFiniteDecimalInvariants() throws {
        for (name, arguments) in [("sqrt", ["2.0"]), ("exp", ["1.0"]), ("ln", ["2.0"]),
                                  ("log", ["2.0", "8.0"]), ("power", ["2.0", "0.5"]),
                                  ("power", ["-1.0", "2147483649"])] {
            let meter = CostMeter()
            let result = try DecimalFunctions.call(name: name, args: arguments.map(decimal), meter: meter)
            XCTAssertEqual(try SQLDecimal.decodeBody(result.encodedBody()).description, result.description)
            XCTAssertLessThanOrEqual(result.scale, 1000)
            XCTAssertGreaterThan(meter.accrued, 0)
            XCTAssertGreaterThan(meter.resources.scalarUsed, 0)
            XCTAssertFalse(result.isZero && result.negative)
        }
        let root = try DecimalFunctions.call(name: "sqrt", args: [decimal("4.0")], meter: CostMeter())
        XCTAssertEqual(try root.multiplying(root), SQLDecimal(4))
        let logarithm = try DecimalFunctions.call(name: "ln", args: [SQLDecimal(1)], meter: CostMeter())
        XCTAssertTrue(logarithm.isZero)
        XCTAssertFalse(logarithm.negative)
    }
}
