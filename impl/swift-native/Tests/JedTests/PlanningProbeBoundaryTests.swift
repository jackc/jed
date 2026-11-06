import XCTest
@testable import Jed

final class PlanningProbeBoundaryTests: XCTestCase {
    private let integer = TypeDescriptor(.scalar(.i32))
    private let boolean = TypeDescriptor(.scalar(.boolean))
    private func parameter(_ number: Int) -> ResolvedExpression {
        ResolvedExpression(type: integer, node: .parameter(number), position: 0)
    }
    private func column(_ number: Int) -> ResolvedExpression {
        ResolvedExpression(type: integer, node: .column(number), position: 0)
    }
    private func comparison(_ operation: String, _ left: ResolvedExpression, _ right: ResolvedExpression) -> ResolvedExpression {
        ResolvedExpression(type: boolean, node: .binary(operation, left, right, integer), position: 0)
    }
    private func fixture(mcv: Bool = false, statistics: Bool = false) throws -> (CatalogTable, DomainSnapshot, TableStore) {
        let codec = try PlainPageCodec(pageSize: 4096)
        let pool = try NodePool(codec: codec, cacheBytes: nil) { _ in
            throw JedError("XX000", "planning must not read a page")
        }
        let table = CatalogTable(name: try SQLText("probe"), columns: [
            CatalogColumn(name: try SQLText("id"), type: CatalogTypeUse(.scalar(.i32)), notNull: true)
        ], primaryKey: [0], root: PageSealer.nullReference(codec), rowCount: 100)
        var image = CatalogImage(); image.tables = [table]
        if statistics {
            var facts = CatalogStatistics(table: table.name, column: 0, stale: false,
                distributionEligible: true, termEligible: false, analyzed: 100, nulls: 0,
                widthSum: 400, distinct: 100, sampleRows: 32, sampleNonnull: 32)
            if mcv { facts.mcv = [.init(frequency: 8, bytes: try ValueCodec.encode(.integer(7), type: integer))] }
            image.statistics = [facts]
        }
        let store = TableStore(tree: BPlusTree(codec: codec, types: [integer], pool: pool))
        let snapshot = try DomainSnapshot(catalogID: 1, txid: 0, catalog: image, pool: pool,
            tables: [table.name: store], indexes: [:], gistTrees: [:])
        return (table, snapshot, store)
    }

    func testProbeObservationsAndEndedFrames() {
        let probe = PlanningProbe.probe()
        XCTAssertFalse(probe.isNull(1)); XCTAssertFalse(probe.sensitive); XCTAssertTrue(probe.nullRead)
        XCTAssertFalse(probe.needsCustom([.integer(9)])); XCTAssertTrue(probe.needsCustom([.null]))
        XCTAssertNil(probe.value(1)); XCTAssertTrue(probe.sensitive)
        XCTAssertTrue(probe.needsCustom([.integer(9)]))
        let untouched = PlanningProbe.probe(); untouched.end()
        XCTAssertNil(untouched.value(1)); XCTAssertFalse(untouched.isNull(1))
        XCTAssertFalse(untouched.sensitive); XCTAssertFalse(untouched.nullRead)
        let custom = PlanningProbe.custom([.integer(9), .null])
        if case .integer(9) = custom.value(1) {} else { XCTFail("custom frame lost value") }
        XCTAssertTrue(custom.isNull(2)); XCTAssertFalse(custom.needsCustom([.null]))
        custom.end(); XCTAssertNil(custom.value(1)); XCTAssertFalse(custom.isNull(2))
        XCTAssertFalse(custom.needsCustom([.null]))
        if case .integer(4) = PlanningProbe.known(parameter(1), parameters: [.integer(4)], probe: custom) {} else {
            XCTFail("an ended frame must not hide a later execution's parameters")
        }
        XCTAssertTrue(PlanningProbe.isNull(parameter(1), parameters: [.null], probe: custom))
    }

    func testUniqueAccessAndLogicalEqualityOnlyAssumeNonNull() throws {
        for statistics in [false, true] {
            let (table, snapshot, store) = try fixture(statistics: statistics)
            let predicate = comparison("=", column(0), parameter(1)), probe = PlanningProbe.probe()
            var context = ExpressionContext(meter: CostMeter()); context.planningProbe = probe
            let base = BaseScanPlan(predicate: predicate, keys: [0], types: [integer])
            let candidate = ScanCandidate(path: .primary(base), table: table, store: store, snapshot: snapshot, predicate: predicate)
            XCTAssertEqual(try candidate.estimate(parameters: nil, context: context).rows, 1)
            let model = PlanCardinality(table: table, snapshot: snapshot, planningProbe: probe)
            XCTAssertEqual(try model.rows(predicate, input: 100), 1)
            XCTAssertFalse(probe.sensitive); XCTAssertTrue(probe.nullRead)
            XCTAssertFalse(probe.needsCustom([.integer(7)])); XCTAssertTrue(probe.needsCustom([.null]))
        }
    }

    func testContradictionReadsOnlyComparablePeers() throws {
        let one = comparison("=", column(0), parameter(1))
        let probe = PlanningProbe.probe()
        XCTAssertFalse(PredicateContradiction.proves(one, planningProbe: probe))
        XCTAssertFalse(probe.sensitive); XCTAssertTrue(probe.nullRead)
        let outer = ResolvedExpression(type: integer, node: .outerColumn(OuterColumnReference(depth: 1, index: 0)), position: 0)
        let runtimePeer = try XCTUnwrap(QueryEstimate.and([one, comparison("=", column(0), outer)]))
        let runtimeProbe = PlanningProbe.probe()
        _ = try PlanCardinality(table: nil, snapshot: nil, planningProbe: runtimeProbe).rows(runtimePeer, input: 100)
        XCTAssertFalse(runtimeProbe.sensitive); XCTAssertTrue(runtimeProbe.nullRead)
        let pair = try XCTUnwrap(QueryEstimate.and([one, comparison("=", column(0), parameter(2))]))
        let paired = PlanningProbe.probe()
        XCTAssertFalse(PredicateContradiction.proves(pair, planningProbe: paired)); XCTAssertTrue(paired.sensitive)
        XCTAssertTrue(PredicateContradiction.proves(pair, planningProbe: .custom([.integer(1), .integer(2)])))
        XCTAssertFalse(PredicateContradiction.proves(pair, planningProbe: .custom([.integer(2), .integer(2)])))
    }

    func testStatisticsPayloadAndLikeProofReadValues() throws {
        let (table, snapshot, _) = try fixture(mcv: true, statistics: true)
        let probe = PlanningProbe.probe()
        _ = try PlanCardinality(table: table, snapshot: snapshot, planningProbe: probe)
            .rows(comparison("=", column(0), parameter(1)), input: 100)
        XCTAssertTrue(probe.sensitive)
        let text = TypeDescriptor(.scalar(.text))
        let pattern = ResolvedPattern(subject: ResolvedExpression(type: text, node: .column(0), position: 0),
            pattern: ResolvedExpression(type: text, node: .parameter(1), position: 0),
            like: true, negated: false, insensitive: false, similar: false, program: nil)
        let expression = ResolvedExpression(type: boolean, node: .pattern(pattern), position: 0)
        let like = PlanningProbe.probe()
        XCTAssertFalse(PushdownSafety.predicate(expression, planningProbe: like)); XCTAssertTrue(like.sensitive)
        XCTAssertTrue(PushdownSafety.predicate(expression, planningProbe: .custom([.text(try SQLText("x%"))])))
        XCTAssertFalse(PushdownSafety.predicate(expression, planningProbe: .custom([.text(try SQLText("x\\"))])))
    }

    func testArrayLengthIsAValueReadButElementNullChecksAreNot() throws {
        let type = TypeDescriptor(.array(integer.type))
        let array = ResolvedExpression(type: type, node: .parameter(1), position: 0)
        let dynamic = ResolvedExpression(type: boolean, node: .quantified("=", all: false, column(0), array, integer), position: 0)
        let probe = PlanningProbe.probe()
        _ = try PlanCardinality(table: nil, snapshot: nil, planningProbe: probe).rows(dynamic, input: 100)
        XCTAssertTrue(probe.sensitive)
        let constructed = ResolvedExpression(type: type, node: .array([parameter(1)], nested: false), position: 0)
        let fixed = ResolvedExpression(type: boolean, node: .quantified("=", all: false, column(0), constructed, integer), position: 0)
        let element = PlanningProbe.probe()
        _ = try PlanCardinality(table: nil, snapshot: nil, planningProbe: element).rows(fixed, input: 100)
        XCTAssertFalse(element.sensitive); XCTAssertTrue(element.nullRead)
    }

    func testDisjunctionDeduplicatesParameterPositionsWithoutReadingValues() throws {
        let term = comparison("=", column(0), parameter(1))
        let expression = ResolvedExpression(type: boolean, node: .binary("or", term, term, boolean), position: 0)
        let probe = PlanningProbe.probe(), model = PlanCardinality(table: nil, snapshot: nil, planningProbe: probe)
        XCTAssertEqual(try model.rows(expression, input: 100), try model.rows(term, input: 100))
        XCTAssertFalse(probe.sensitive); XCTAssertTrue(probe.nullRead)
    }
}
