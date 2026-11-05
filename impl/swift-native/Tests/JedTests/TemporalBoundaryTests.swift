import Foundation
import XCTest
@testable import Jed

final class TemporalBoundaryTests: XCTestCase {
    private func document(_ path: String) throws -> [String: Any] {
        let url = try XCTUnwrap(Bundle.module.url(forResource: "SpecSnapshot", withExtension: "json", subdirectory: "Fixtures"))
        let root = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
        let documents = try XCTUnwrap(root["documents"] as? [String: [String: Any]])
        return try XCTUnwrap(documents[path])
    }
    private func fixture(_ name: String, extension suffix: String) throws -> [UInt8] {
        let url = try XCTUnwrap(Bundle.module.url(forResource: name, withExtension: suffix, subdirectory: "Fixtures"))
        return [UInt8](try Data(contentsOf: url))
    }
    private func value(_ entry: [String: Any], type: ScalarType) throws -> SQLValue {
        if type == .date { return .date(try XCTUnwrap(entry["days"] as? NSNumber).int32Value) }
        let micros = try XCTUnwrap(entry["micros"] as? NSNumber).int64Value
        if type == .interval {
            return .interval(SQLInterval(months: try XCTUnwrap(entry["months"] as? NSNumber).int32Value,
                                         days: try XCTUnwrap(entry["days"] as? NSNumber).int32Value, microseconds: micros))
        }
        return type == .timestamptz ? .timestamptz(micros) : .timestamp(micros)
    }
    private func same(_ a: SQLValue, _ b: SQLValue, _ context: String) {
        switch (a, b) {
        case (.date(let x), .date(let y)): XCTAssertEqual(x, y, context)
        case (.timestamp(let x), .timestamp(let y)), (.timestamptz(let x), .timestamptz(let y)): XCTAssertEqual(x, y, context)
        case (.interval(let x), .interval(let y)): XCTAssertEqual(x, y, context)
        default: XCTFail("mismatched temporal types: \(context)")
        }
    }
    private func checkVectors(_ name: String, defaultType: ScalarType) throws {
        let vectors = try document("spec/encoding/\(name).toml")
        for category in ["parse", "parse_error", "render"] {
            for group in try XCTUnwrap(vectors[category] as? [[String: Any]]) {
                let type = (group["type"] as? String).flatMap { name in ScalarType.allCases.first(where: { $0.name == name }) } ?? defaultType
                for entry in try XCTUnwrap(group["cases"] as? [[String: Any]]) {
                    if category == "render" {
                        XCTAssertEqual(try Temporal.render(value(entry, type: type)).description, entry["text"] as? String)
                    } else {
                        let input = try XCTUnwrap(entry["input"] as? String)
                        if category == "parse" {
                            do { same(try Temporal.parse(text: SQLText(input), type: type), try value(entry, type: type), input) }
                            catch { XCTFail("\(input): \(error)") }
                        } else {
                            XCTAssertThrowsError(try Temporal.parse(text: SQLText(input), type: type), input) {
                                XCTAssertEqual(($0 as? JedError)?.code, entry["error"] as? String, input)
                            }
                        }
                    }
                }
            }
        }
    }
    func testSharedDateVectors() throws { try checkVectors("dates", defaultType: .date) }
    func testSharedTimestampVectors() throws { try checkVectors("timestamps", defaultType: .timestamp) }
    func testSharedIntervalVectors() throws { try checkVectors("intervals", defaultType: .interval) }

    func testCalendarInversionAcrossEntireFiniteDomains() throws {
        for day in stride(from: Int64(Int32.min) + 1, through: Int64(Int32.max) - 1, by: 4_294_967) {
            let civil = Temporal.civilFromDays(day)
            XCTAssertEqual(Temporal.daysFromCivil(year: civil.year, month: civil.month, day: civil.day), day)
        }
        for micros: Int64 in [.min + 1, -62_167_219_200_000_000, -86_400_000_001, -1, 0, 1, .max - 1] {
            XCTAssertEqual(try Temporal.microsFromCivil(Temporal.civilFromMicros(micros)), micros)
        }
    }

    func testSharedTimezoneBundleAndReaderVectors() throws {
        let bytes = try fixture("tzdata", extension: "jtz")
        let bundle = try TimeZoneBundle(bytes)
        XCTAssertEqual(bundle.encoded(), bytes)
        let manifest = try XCTUnwrap((document("spec/tz/vectors/bundle.toml")["bundle"] as? [[String: Any]])?.first)
        XCTAssertEqual(bundle.version.description, manifest["tzdata_version"] as? String)
        XCTAssertEqual(bundle.zoneNames.map(\.description), manifest["zones"] as? [String])
        XCTAssertEqual(bundle.links.map { "\($0.0)=\($0.1)" }, manifest["links"] as? [String])
        var database = TimeZoneDatabase(); try database.load(bytes)
        let vectors = try XCTUnwrap(document("spec/tz/vectors/tzif.toml")["case"] as? [[String: Any]])
        for row in vectors {
            let name = try XCTUnwrap(row["zone"] as? String)
            let zone = try database.resolve(SQLText(name))
            let offset = zone.offset(at: try XCTUnwrap(row["instant_micros"] as? NSNumber).int64Value)
            XCTAssertEqual(offset.seconds, try XCTUnwrap(row["utoff_secs"] as? NSNumber).int32Value, name)
            XCTAssertEqual(offset.abbreviation, row["abbrev"] as? String, name)
            XCTAssertEqual(offset.isDST, row["is_dst"] as? Bool, name)
        }
        try database.load(bytes)
        XCTAssertEqual(database.zones.count, bundle.zones.count)
        XCTAssertEqual(try database.resolve(SQLText("AMERICA/NEW_YORK")).name.description, "America/New_York")
    }

    func testZoneShiftBoundsAndHostDataIsolation() throws {
        let empty = TimeZoneDatabase()
        XCTAssertThrowsError(try empty.resolve(SQLText("America/New_York"))) { XCTAssertEqual(($0 as? JedError)?.code, "22023") }
        for name in ["+168", "+1000000", "+4294967296", "+05:60", "+05:00:61"] {
            XCTAssertThrowsError(try empty.resolve(SQLText(name))) { XCTAssertEqual(($0 as? JedError)?.code, "22023") }
        }
        let east = SQLTimeZone(fixed: 3600), west = SQLTimeZone(fixed: -3600)
        XCTAssertThrowsError(try east.instantToLocal(.max - 1)) { XCTAssertEqual(($0 as? JedError)?.code, "22008") }
        XCTAssertThrowsError(try west.instantToLocal(.min + 1)) { XCTAssertEqual(($0 as? JedError)?.code, "22008") }
        XCTAssertEqual(try east.instantToLocal(.max), .max)
        XCTAssertEqual(try west.localToInstant(.min), .min)
        for value: Int64 in [-1_000_001, -1, 0, 1, 1_000_001] {
            XCTAssertEqual(try east.localToInstant(east.instantToLocal(value)), value)
        }
    }

    func testFormattingByteNodesAndRichErrorBoundaries() throws {
        let nodes = FormatTemplate.parse(try SQLText("\u{FEFF}\"é😀\"FMYYYYTH-MM-FF6"))
        XCTAssertEqual(nodes.filter { $0.keyword == nil }.flatMap(\.bytes), Array("\u{FEFF}é😀--".utf8))
        XCTAssertEqual(nodes.compactMap { $0.keyword?.id }, ["YYYY", "MM", "FF6"])
        XCTAssertEqual(nodes.first { $0.keyword?.id == "YYYY" }?.fill, true)
        XCTAssertEqual(nodes.first { $0.keyword?.id == "YYYY" }?.ordinal, "TH")
        XCTAssertEqual(FormatTemplate.parse(try SQLText("FM")).count, 0)
        for (input, message, detail) in [
            ("20€401", "invalid value \"20�\" for \"YYYY\"", "Field requires 4 characters, but only 2 could be parsed."),
            ("2😀01", "invalid value \"2�\" for \"YYYY\"", "Field requires 4 characters, but only 1 could be parsed.")
        ] {
            XCTAssertThrowsError(try TemporalFormat.parse(SQLText(input), template: SQLText("YYYYMM"))) {
                let error = $0 as? JedError
                XCTAssertEqual(error?.code, "22007")
                XCTAssertEqual(error?.message, message)
                XCTAssertEqual(error?.detail, detail)
                XCTAssertEqual(error?.hint, "If your source string is not fixed-width, try using the \"FM\" modifier.")
            }
        }
    }

    func testFormattingCalendarAgreementAndOutputAccounting() throws {
        for year in stride(from: Int64(-4000), through: 9000, by: 13) {
            for month: Int64 in [1, 2, 3, 12] {
                let day: Int64 = 17
                let julian = FormatCalendar.julian(year, month, day)
                XCTAssertEqual(Int64(julian), Temporal.daysFromCivil(year: year, month: month, day: day) + 2_440_588)
                let restored = FormatCalendar.civil(julian)
                XCTAssertEqual(restored, Temporal.Civil(year: year, month: month, day: day))
            }
        }
        let meter = CostMeter(), template = try SQLText("\u{FEFF}YYYY-MM-DD HH24:MI:SS OF")
        let rendered = try TemporalFormat.toChar(.timestamptz(0), template: template, timezone: SQLTimeZone(fixed: 3600), meter: meter)
        guard case .text(let text) = rendered else { return XCTFail("expected text") }
        XCTAssertEqual(meter.accrued, 1 + Int64(text.utf8.count))
        XCTAssertEqual(text.utf8.prefix(3), [239, 187, 191])
        let ceiling = CostMeter(limit: 3)
        XCTAssertThrowsError(try TemporalFormat.toChar(.timestamp(0), template: template, meter: ceiling)) {
            XCTAssertEqual(($0 as? JedError)?.code, "54P01")
        }
    }
}
