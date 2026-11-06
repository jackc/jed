import XCTest
@testable import Jed

/// The shared varwidth-span corpus owns SQL shapes and results. These tests
/// exercise representation boundaries it cannot express: corrupt borrowed
/// spans, deferred bodies, private charges, and decoder-free evaluation.
final class PackedSpanBoundaryTests: XCTestCase {
    private let boolean = TypeDescriptor(.scalar(.boolean))
    private let text = TypeDescriptor(.scalar(.text))
    private let bytea = TypeDescriptor(.scalar(.bytea))

    private func column(_ type: TypeDescriptor, _ index: Int = 0) -> ResolvedExpression {
        ResolvedExpression(type: type, node: .column(index), position: 0)
    }
    private func constant(_ value: SQLValue, _ type: TypeDescriptor) -> ResolvedExpression {
        ResolvedExpression(type: type, node: .constant(value), position: 0)
    }
    private func nullTest(_ type: TypeDescriptor, _ index: Int = 0, negated: Bool = false) -> ResolvedExpression {
        ResolvedExpression(type: boolean, node: .isNull(column(type, index), negated), position: 0)
    }
    private func equal(_ type: TypeDescriptor, _ value: SQLValue, negated: Bool = false,
                       reversed: Bool = false, index: Int = 0) -> ResolvedExpression {
        let a = column(type, index), b = constant(value, type)
        return ResolvedExpression(type: boolean,
            node: .binary(negated ? "<>" : "=", reversed ? b : a, reversed ? a : b, type), position: 0)
    }
    private func arrayOperation(_ name: String, _ type: TypeDescriptor, _ operand: ResolvedExpression,
                                reversed: Bool = false) -> ResolvedExpression {
        let a = column(type), args = reversed ? [operand, a] : [a, operand]
        let fact = FunctionSpecFact(category: "array", name: name, kind: "containment",
            metadata: ["result": .text("boolean"), "null": .text("propagates")])
        let function = ResolvedFunction(metadata: fact, arguments: args, resultType: boolean,
            evaluationOrder: [0,1], constantArguments: args.map(FunctionResolver.isConstant))
        return ResolvedExpression(type: boolean, node: .call(function), position: 0)
    }
    private func cell(_ value: SQLValue, _ type: TypeDescriptor) throws -> StoredCell {
        if value.isNull { return StoredCell(bytes: nil, type: type) }
        let bytes: [UInt8]
        if ValueCodec.fixedWidth(type.type) == nil { bytes = try ValueCodec.encode(value, type: type) }
        else { bytes = try ValueCodec.encodeBody(value, type: type) }
        return StoredCell(bytes: bytes[...], type: type)
    }
    private func frame(_ cells: [StoredCell], pending: [Int: PendingOverflow] = [:], packed: Bool = true) throws -> PackedLaneRow {
        let types = cells.map(\.type), codec = try PlainPageCodec(pageSize: 8192)
        let record = PackedRecord(key: [1], cells: cells, overflows: pending)
        let position: TreePosition
        if packed {
            // Keep encoded-path probes on an actual packed page even when
            // future writer representations retain immutable record vectors.
            let payload = try PackedLeaf.encodePayload([record], types: types, capacity: codec.capacity)
            let sealed = try PageSealer.seal(codec: codec, index: codec.firstBodyPage, type: 2, count: 1, payload: payload)
            let leaf = try PackedLeaf(bytes: sealed.page.bytes, types: types, codec: codec)
            position = TreePosition(node: TreeNode(leaf: leaf, pending: pending.isEmpty ? [:] : [0:pending]), row: 0)
        } else {
            var tree = BPlusTree(codec: codec, types: types); try tree.insert(record)
            var cursor = TreeCursor(tree)
            position = try XCTUnwrap(cursor.nextPosition()); cursor.close()
        }
        let row = PackedLaneRow(width: cells.count, touched: Array(cells.indices), pager: nil)
        row.begin(position)
        return row
    }
    private func assertEquivalent(_ expression: ResolvedExpression, cells: [StoredCell],
                                  parameters: [SQLValue] = [], pending: [Int: PendingOverflow] = [:],
                                  expectEncoded: Bool = true, requireSpan: Bool = true, packed: Bool = true,
                                  file: StaticString = #filePath, line: UInt = #line) throws {
        let row = try frame(cells, pending: pending, packed: packed); defer { row.close() }
        let meter = CostMeter()
        var current = ExpressionContext(meter: meter, parameters: parameters)
        var fallbackReads = 0
        current.column = { index in
            fallbackReads += 1
            if requireSpan { throw JedError("XX000", "encoded predicate used the value evaluator") }
            return try row.value(index)
        }
        let filter = PackedLaneFilter(predicate: expression, types: cells.map(\.type),
            touched: Array(cells.indices), context: current)
        XCTAssertEqual(filter.hasEncodedPredicates, expectEncoded, file: file, line: line)
        XCTAssertEqual(meter.accrued, 0, "compilation must not charge the statement", file: file, line: line)
        let keep = try filter.keep(row, context: current)
        if !requireSpan && expectEncoded { XCTAssertGreaterThan(fallbackReads, 0, file: file, line: line) }
        let ordinaryMeter = CostMeter()
        let ordinary = ExpressionContext(meter: ordinaryMeter, parameters: parameters,
            column: { try row.value($0) })
        try row.prepare(Array(cells.indices))
        XCTAssertEqual(keep, try ordinary.matches(expression), file: file, line: line)
        XCTAssertEqual(meter.accrued, ordinaryMeter.accrued, file: file, line: line)
    }

    func testTextByteaAndNullnessUseBorrowedSpansWithScalarCosts() throws {
        for value in [SQLValue.text(try SQLText("")), .text(try SQLText("hé🙂")), .text(try SQLText("different")), .null] {
            for negated in [false, true] {
                for reversed in [false, true] {
                    try assertEquivalent(equal(text, .text(try SQLText("hé🙂")), negated: negated, reversed: reversed),
                        cells: [try cell(value, text)])
                }
                try assertEquivalent(nullTest(text, negated: negated), cells: [try cell(value, text)])
            }
        }
        for value in [SQLValue.bytea([]), .bytea([0,255,0,42]), .bytea([0,254,0,42]), .null] {
            for negated in [false, true] {
                try assertEquivalent(equal(bytea, .bytea([0,255,0,42]), negated: negated), cells: [try cell(value, bytea)])
            }
        }
        let malformed = StoredCell(bytes: [UInt8(0),0,2,41][...], type: text)
        // IS NULL has no value-decoding requirement, even for an invalid body.
        let row = try frame([malformed]); defer { row.close() }
        let meter = CostMeter(), context = ExpressionContext(meter: CostMeter())
        let filter = PackedLaneFilter(predicate: nullTest(text, negated: true), types: [text], touched: [0], context: context)
        XCTAssertTrue(try filter.keep(row, context: ExpressionContext(meter: meter)))
        XCTAssertEqual(meter.accrued, 1)
    }

    func testArraySpansHandleWidthsNullsDimensionsAndReversedOperands() throws {
        let cases: [(ScalarType, SQLValue, SQLValue)] = [
            (.i16, .integer(-32768), .integer(32767)),
            (.i32, .integer(-2147483648), .integer(2147483647)),
            (.i64, .integer(Int64.min), .integer(Int64.max)),
            (.text, .text(try SQLText("é🙂")), .text(try SQLText(""))),
            (.bytea, .bytea([0,255]), .bytea([])),
        ]
        for (scalar, a, b) in cases {
            let type = TypeDescriptor(.array(.scalar(scalar)))
            let shaped = SQLValue.array(try SQLArray(dimensions: [2,2], lowerBounds: [-3,7], elements: [a,b,.null,a]))
            let plain = SQLValue.array(try SQLArray(elements: [a,b,a]))
            let empty = SQLValue.array(try SQLArray(elements: []))
            let constants = [empty, .array(try SQLArray(elements: [a,a])),
                .array(try SQLArray(elements: [b,.null])), shaped]
            for value in [shaped, plain, empty, .null] {
                for expected in constants {
                    for name in ["@>", "<@", "&&"] {
                        for reversed in [false, true] {
                            try assertEquivalent(arrayOperation(name, type, constant(expected, type), reversed: reversed),
                                cells: [try cell(value, type)])
                        }
                    }
                }
            }
        }
    }

    func testConstantConstructorChargesReplayPerRowAndOnNullOperands() throws {
        let integer = TypeDescriptor(.scalar(.i32)), array = TypeDescriptor(.array(.scalar(.i32)))
        let parameter = ResolvedExpression(type: integer, node: .parameter(1), position: 0)
        let constructor = ResolvedExpression(type: array,
            node: .array([parameter, constant(.integer(7), integer)], nested: false), position: 0)
        let expression = arrayOperation("@>", array, constructor)
        for value in [SQLValue.array(try SQLArray(elements: [.integer(3),.integer(7)])), .null] {
            try assertEquivalent(expression, cells: [try cell(value, array)], parameters: [.integer(3)])
        }
        let row = try frame([try cell(.array(try SQLArray(elements: [.integer(3),.integer(7)])), array)])
        defer { row.close() }
        let meter = CostMeter(), context = ExpressionContext(meter: CostMeter(), parameters: [.integer(3)])
        let filter = PackedLaneFilter(predicate: expression, types: [array], touched: [0], context: context)
        XCTAssertEqual(context.meter.accrued, 0)
        let execution = ExpressionContext(meter: meter, parameters: [.integer(3)])
        XCTAssertTrue(try filter.keep(row, context: execution)); XCTAssertEqual(meter.accrued, 2)
        XCTAssertTrue(try filter.keep(row, context: execution)); XCTAssertEqual(meter.accrued, 4)
    }

    func testMalformedBodiesDeclineBeforeChargingAndUseDecoderErrors() throws {
        let array = TypeDescriptor(.array(.scalar(.text)))
        let validArray = try ValueCodec.encode(.array(SQLArray(elements: [.text(try SQLText("a"))])), type: array)
        var badFlags = validArray; badFlags[2] = 2
        var badShape = validArray; badShape[3...6] = [0,0,0,0]
        var badUTF8 = validArray; badUTF8[badUTF8.count - 1] = 0xff
        var badBitmap = try ValueCodec.encode(.array(SQLArray(elements: [.null])), type: array)
        badBitmap[badBitmap.count - 1] |= 1
        let textBodies: [[UInt8]] = [[0,0,2,41], [0,0,1,0], [0,0,1,0xff], [0,0,2,0xc0,0x80], [0,0,1,65,0]]
        let textExpression = equal(text, .text(try SQLText("a")))
        let arrayExpression = arrayOperation("&&", array, constant(.array(try SQLArray(elements: [])), array))
        let probes = textBodies.map { (text, $0, textExpression) }
            + [badFlags, badShape, badUTF8, badBitmap, validArray + [0]].map { (array, $0, arrayExpression) }
        for (type, bytes, expression) in probes {
          for packed in [false, true] {
            let row = try frame([StoredCell(bytes: bytes[...], type: type)], packed: packed); defer { row.close() }
            let meter = CostMeter(), context = ExpressionContext(meter: CostMeter())
            let filter = PackedLaneFilter(predicate: expression, types: [type], touched: [0], context: context)
            XCTAssertTrue(filter.hasEncodedPredicates)
            let execution = ExpressionContext(meter: meter, column: { try row.value($0) })
            XCTAssertThrowsError(try filter.keep(row, context: execution)) { XCTAssertEqual(($0 as? JedError)?.code, "XX001") }
            XCTAssertEqual(meter.accrued, 0, "declined atoms must not precharge before decoder failure")
          }
        }
    }

    func testAndShortCircuitDoesNotDecodeAnUnreachedMalformedCell() throws {
        let left = nullTest(text), right = equal(bytea, .bytea([1]), index: 1)
        let expression = ResolvedExpression(type: boolean, node: .binary("and", left, right, boolean), position: 0)
        let row = try frame([try cell(.text(SQLText("not null")), text), StoredCell(bytes: [UInt8(0),0,2,1][...], type: bytea)])
        defer { row.close() }
        let meter = CostMeter(), context = ExpressionContext(meter: meter, column: { try row.value($0) })
        let filter = PackedLaneFilter(predicate: expression, types: [text,bytea], touched: [0,1], context: context)
        XCTAssertFalse(try filter.keep(row, context: context)); XCTAssertEqual(meter.accrued, 2)
    }

    func testMixedAndPreservesGenericErrorsAndReadsOnlyReachedColumns() throws {
        let integer = TypeDescriptor(.scalar(.i32))
        let division = ResolvedExpression(type: integer,
            node: .binary("/", constant(.integer(1), integer), column(integer, 1), integer), position: 0)
        let comparison = ResolvedExpression(type: boolean,
            node: .binary(">", division, constant(.integer(0), integer), integer), position: 0)
        let expression = ResolvedExpression(type: boolean,
            node: .binary("and", nullTest(text), comparison, boolean), position: 0)
        for value in [SQLValue.text(try SQLText("not null")), .null] {
            let row = try frame([try cell(value, text), try cell(.integer(0), integer)]); defer { row.close() }
            let meter = CostMeter(), context = ExpressionContext(meter: meter, column: { try row.value($0) })
            let filter = PackedLaneFilter(predicate: expression, types: [text,integer], touched: [0,1], context: context)
            if value.isNull {
                XCTAssertThrowsError(try filter.keep(row, context: context)) { XCTAssertEqual(($0 as? JedError)?.code, "22012") }
                XCTAssertEqual(meter.accrued, 4)
            } else {
                XCTAssertFalse(try filter.keep(row, context: context)); XCTAssertEqual(meter.accrued, 2)
            }
        }
    }

    func testSurvivorsOwnProjectionMemoryAndCorrelatedFallbackReadsTheFrame() throws {
        let database = try Database(); defer { try? database.close() }
        try database.execute("CREATE TABLE t(id i32 PRIMARY KEY,flag text,payload bytea)")
        try database.execute("INSERT INTO t VALUES(1,'no','rejected'::bytea),(2,'yes','kept'::bytea)")
        let name = try SQLText("t"), base = database.core.routingSnapshot
        var store = try XCTUnwrap(base.tables[name]), cursor = store.cursor()
        let rejected = try XCTUnwrap(cursor.next()), kept = try XCTUnwrap(cursor.next()); cursor.close()
        var invalid = rejected.cells
        invalid[2] = StoredCell(bytes: [UInt8(0),0,2,41][...], type: bytea)
        try store.put(PackedRecord(key: rejected.key, cells: invalid), netStaging: false)
        var snapshot = base; snapshot.setTableStore(name, store)

        func run(_ sql: String, snapshot: DomainSnapshot, memory: Int64 = 0) throws
            -> (rows: [[UInt8]], cost: Int64, peak: Int64, error: String?) {
            let diagnostics = MemoryDiagnostics(), resources = StatementResources(queryLimit: memory, diagnostics: diagnostics)
            let meter = CostMeter(resources: resources)
            let settings = SessionSettings(environment: database.environment, options: SessionOptions())
            let context = StatementServices(settings: settings, meter: meter).context()
            guard case .query(let syntax) = try SQLParser(sql).parse() else { throw JedError("XX000", "expected SELECT") }
            let resolved = try SelectPlan.resolve(syntax, snapshot: snapshot, settings: settings, scalar: context.scalar)
            let plan = try resolved.selectAccess(parameters: [], context: context)
            if let aggregate = plan.aggregation { XCTAssertTrue(ColumnarExecution.packedAggregation(plan, aggregate, context)) }
            else { XCTAssertTrue(ColumnarExecution.projection(plan, context)) }
            let execution = SelectExecution(plan: plan, context: context); defer { execution.close() }
            var rows: [[UInt8]] = [], error: String?
            do { while let row = try execution.next() { rows.append(try SpillRowCodec.encode(row)) } }
            catch let failure { error = (failure as? JedError)?.code ?? String(describing: failure) }
            execution.close(); execution.close()
            XCTAssertEqual(resources.query.used, 0); XCTAssertEqual(diagnostics.underflows, 0)
            return (rows, meter.accrued, diagnostics.peak, error)
        }
        let sql = "SELECT payload FROM t WHERE flag='yes'"
        let value = SQLValue.bytea(Array("kept".utf8)), bytes = MemorySizes.value(value)
        let success = try run(sql, snapshot: snapshot, memory: bytes)
        XCTAssertNil(success.error); XCTAssertEqual(success.rows, [try SpillRowCodec.encode([value])])
        XCTAssertEqual(success.peak, bytes)
        let denied = try run(sql, snapshot: snapshot, memory: bytes - 1)
        XCTAssertEqual(denied.error, "54P05"); XCTAssertTrue(denied.rows.isEmpty)
        XCTAssertEqual(denied.peak, 0); XCTAssertEqual(denied.cost, success.cost - 1)
        let correlated = try run("SELECT payload FROM t WHERE flag='yes' AND EXISTS(SELECT 1 WHERE t.id=2)", snapshot: snapshot)
        XCTAssertNil(correlated.error); XCTAssertEqual(correlated.rows, success.rows)

        // COUNT beside an encoded WHERE consumes nullness even when the
        // surviving body is invalid. MIN remains a value consumer.
        invalid = kept.cells; invalid[2] = StoredCell(bytes: [UInt8(0),0,2,41][...], type: bytea)
        try store.put(PackedRecord(key: kept.key, cells: invalid), netStaging: false)
        snapshot.setTableStore(name, store)
        let counted = try run("SELECT count(payload) FROM t WHERE flag='yes'", snapshot: snapshot)
        XCTAssertNil(counted.error); XCTAssertEqual(counted.rows, [try SpillRowCodec.encode([.integer(1)])])
        let minimum = try run("SELECT min(payload) FROM t WHERE flag='yes'", snapshot: snapshot)
        XCTAssertEqual(minimum.error, "XX001")
    }

    func testPendingAndCompressedBodiesUseOrdinaryEvaluationWithoutDoubleCharge() throws {
        let value = SQLValue.text(try SQLText("hé🙂")), raw = try ValueCodec.encodeRawBody(value, type: text)
        let expression = equal(text, value)
        for compressed in [false, true] {
            let pending = PendingOverflow(bytes: compressed ? LZ4.compress(raw) : raw, rawLength: raw.count, compressed: compressed)
            let stored = StoredCell(bytes: try pending.pointer(PageReference(0))[...], type: text)
            for packed in [false, true] {
                try assertEquivalent(expression, cells: [stored], pending: [0:pending], requireSpan: false, packed: packed)
            }
        }
        let compressed = LZ4.compress(raw)
        var writer = ByteWriter(); writer.u8(3); writer.u32(UInt32(raw.count)); writer.u16(UInt16(compressed.count)); writer.append(compressed)
        for packed in [false, true] {
            try assertEquivalent(expression, cells: [StoredCell(bytes: writer.bytes[...], type: text)], requireSpan: false, packed: packed)
        }
    }

    func testDirtyCellsRetainValidatedEncodedAtoms() throws {
        let value = SQLValue.text(try SQLText("hé🙂"))
        try assertEquivalent(equal(text, value), cells: [try cell(value, text)], packed: false)
        try assertEquivalent(equal(bytea, .bytea([0,255])), cells: [try cell(.bytea([0,255]), bytea)], packed: false)
        let array = TypeDescriptor(.array(.scalar(.text)))
        let values = SQLValue.array(try SQLArray(dimensions: [2,2], lowerBounds: [-1,4],
            elements: [value,.null,value,.text(try SQLText(""))]))
        let expected = constant(.array(try SQLArray(elements: [value])), array)
        for name in ["@>", "<@", "&&"] {
            try assertEquivalent(arrayOperation(name, array, expected), cells: [try cell(values, array)], packed: false)
        }
        // Writer record vectors hold StoredCells, not decoded SQL values. Their
        // validated inline encoding remains eligible for the same span atoms.
        let row = try frame([try cell(value, text)], packed: false); defer { row.close() }
        XCTAssertNotNil(try row.storedCell(0))
    }

    func testMalformedDirtyDeferredBodiesFailBeforeAtomCharges() throws {
        let expression = equal(text, .text(try SQLText("valid")))
        let cases = [PendingOverflow(bytes: [0xff], rawLength: 1, compressed: false),
            PendingOverflow(bytes: [0xff], rawLength: 1, compressed: true)]
        for pending in cases {
            let stored = StoredCell(bytes: try pending.pointer(PageReference(0))[...], type: text)
            let row = try frame([stored], pending: [0:pending], packed: false); defer { row.close() }
            let meter = CostMeter(), context = ExpressionContext(meter: meter, column: { _ in
                throw JedError("XX000", "malformed pending value reached the expression getter")
            })
            let filter = PackedLaneFilter(predicate: expression, types: [text], touched: [0], context: context)
            XCTAssertThrowsError(try filter.keep(row, context: context)) { XCTAssertEqual(($0 as? JedError)?.code, "XX001") }
            XCTAssertEqual(meter.accrued, 0)
        }
        var writer = ByteWriter(); writer.u8(3); writer.u32(1); writer.u16(1); writer.u8(0xff)
        let row = try frame([StoredCell(bytes: writer.bytes[...], type: text)], packed: false); defer { row.close() }
        let meter = CostMeter(), context = ExpressionContext(meter: meter, column: { _ in
            throw JedError("XX000", "malformed compressed value reached the expression getter")
        })
        let filter = PackedLaneFilter(predicate: expression, types: [text], touched: [0], context: context)
        XCTAssertThrowsError(try filter.keep(row, context: context)) { XCTAssertEqual(($0 as? JedError)?.code, "XX001") }
        XCTAssertEqual(meter.accrued, 0)
    }

    func testCompositeNullnessKeepsItsAllFieldsRule() throws {
        let type = TypeDescriptor(.composite(CompositeType(catalogID: 1, id: 1, name: try SQLText("pair"),
            fields: [TypeField(name: try SQLText("a"), descriptor: text), TypeField(name: try SQLText("b"), descriptor: bytea)])))
        for value in [SQLValue.composite([.null,.null]), .composite([.text(try SQLText("a")),.null]), .null] {
            for negated in [false, true] {
                try assertEquivalent(nullTest(type, negated: negated), cells: [try cell(value, type)],
                    expectEncoded: false, requireSpan: false)
            }
        }
    }
}
