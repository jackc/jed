import Foundation
import XCTest
@testable import Jed

final class GinBoundaryTests: XCTestCase {
    private func maintenance(type: CatalogTypeUse = CatalogTypeUse(.array(.scalar(.i32))),
                             collatedKey: KeyEncoding.CollatedKey? = nil) throws -> (StoredIndexPlan, DomainSnapshot) {
        let codec = try PlainPageCodec(pageSize: 4096), root = PageSealer.nullReference(codec)
        let pool = try NodePool(codec: codec, cacheBytes: nil) { _ in throw JedError.corruption("resident index must not read a device") }
        let index = CatalogIndex(name: try SQLText("postings"), keys: [.column(0)], kind: 1, root: root)
        let table = CatalogTable(name: try SQLText("items"), columns: [
            CatalogColumn(name: try SQLText("tags"), type: type)
        ], indexes: [index], root: root)
        var image = CatalogImage(); image.tables = [table]
        let snapshot = try DomainSnapshot(catalogID: 86, txid: 0, catalog: image, pool: pool,
            tables: [:], indexes: [:], gistTrees: [:])
        let settings = SessionSettings(environment: Environment.builder().build(), options: SessionOptions())
        var context = ExpressionContext(meter: CostMeter()); context.collatedKey = collatedKey
        let plan = try StoredIndexPlan.resolve(table: table, index: index, snapshot: snapshot,
            settings: settings, context: context)
        return (plan, snapshot)
    }
    private func bytes(_ hex: String) throws -> [UInt8] {
        let utf8 = Array(hex.utf8)
        return try stride(from: 0, to: utf8.count, by: 2).map {
            try XCTUnwrap(UInt8(String(decoding: utf8[$0..<($0 + 2)], as: UTF8.self), radix: 16))
        }
    }
    private func rows(_ path: String, _ kind: String) throws -> [[String: Any]] {
        let url = try XCTUnwrap(Bundle.module.url(forResource: "SpecSnapshot", withExtension: "json", subdirectory: "Fixtures"))
        let snapshot = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
        let documents = try XCTUnwrap(snapshot["documents"] as? [String: [String: Any]])
        return try XCTUnwrap(documents[path]?[kind] as? [[String: Any]])
    }
    private func key(_ number: Int) -> [UInt8] { var writer = ByteWriter(); writer.u32(UInt32(number)); return writer.bytes }
    private func recordKeys(_ tree: BPlusTree) throws -> [[UInt8]] {
        var cursor = TreeCursor(tree), result: [[UInt8]] = []
        while let record = try cursor.next() { result.append(record.key) }
        return result
    }
    private func candidates(_ result: GinGatherResult) -> [[UInt8]] {
        var keys: [[UInt8]] = []
        while let key = result.candidates.next() { keys.append(key) }
        XCTAssertTrue(result.candidates.isClosed); XCTAssertEqual(result.candidates.remainingCount, 0)
        return keys
    }
    private func expectCorruption(_ body: () throws -> Void, file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertThrowsError(try body(), file: file, line: line) { XCTAssertEqual(($0 as? JedError)?.code, "XX001", file: file, line: line) }
    }

    func testAllSharedPostingRunCodecPackingAndHistoryVectors() throws {
        let path = "spec/encoding/gin_posting_runs.toml"
        for row in try rows(path, "run") {
            let term = try bytes(XCTUnwrap(row["term"] as? String)), keys = try XCTUnwrap(row["keys"] as? [String]).map(bytes)
            let expected = try bytes(XCTUnwrap(row["run"] as? String))
            XCTAssertEqual(try GinPostingRun.encode(term: term, keys: keys), expected)
            XCTAssertEqual(try GinPostingRun.decode(termLength: term.count, bytes: expected), keys)
        }
        for row in try rows(path, "pack") {
            let term = try bytes(XCTUnwrap(row["term"] as? String)), keys = try XCTUnwrap(row["keys"] as? [String]).map(bytes)
            XCTAssertEqual(try GinPostingRun.pack(term: term, keys: keys), try XCTUnwrap(row["runs"] as? [String]).map(bytes))
        }
        for row in try rows(path, "history") {
            let term = try bytes(XCTUnwrap(row["term"] as? String)), codec = try PlainPageCodec(pageSize: 4096)
            var index = try GinIndex(tree: BPlusTree(codec: codec, types: []))
            for operation in try XCTUnwrap(row["ops"] as? [String]) {
                let key = try bytes(String(operation.dropFirst()))
                if operation.first == "+" { try index.insert(term: term, key: key) }
                else { try index.remove(term: term, key: key) }
            }
            XCTAssertEqual(try recordKeys(index.tree), try XCTUnwrap(row["runs"] as? [String]).map(bytes), row["name"] as? String ?? "history")
        }
    }

    func testSharedPostingRunsThroughStoredMaintenanceAndNetPendingAccount() throws {
        let (plan, snapshot) = try maintenance(), path = "spec/encoding/gin_posting_runs.toml"
        for row in try rows(path, "pack") {
            let term = try bytes(XCTUnwrap(row["term"] as? String)), keys = try XCTUnwrap(row["keys"] as? [String]).map(bytes)
            let entries = keys.reversed().map { StoredIndexPlan.Entry(bytes: term + $0, storageKeyLength: $0.count) }
            let store = try plan.build(entries, snapshot: snapshot, netStaging: true)
            let expected = try XCTUnwrap(row["runs"] as? [String]).map(bytes)
            XCTAssertEqual(try recordKeys(store.tree), expected)
            XCTAssertEqual(store.pendingBytes, Int64(expected.reduce(0) { $0 + $1.count }))
        }
        for row in try rows(path, "history") {
            let term = try bytes(XCTUnwrap(row["term"] as? String))
            var store = snapshot.emptyStore(types: [])
            for operation in try XCTUnwrap(row["ops"] as? [String]) {
                let key = try bytes(String(operation.dropFirst()))
                let entry = StoredIndexPlan.Entry(bytes: term + key, storageKeyLength: key.count)
                let pin = store, before = try recordKeys(pin.tree)
                if operation.first == "+" { try plan.insert(entry, into: &store, netStaging: true) }
                else { try plan.remove(entry, from: &store, netStaging: true) }
                let actual = try recordKeys(store.tree)
                XCTAssertEqual(store.pendingBytes, Int64(actual.reduce(0) { $0 + $1.count }))
                XCTAssertEqual(try recordKeys(pin.tree), before, "posting-run edits must preserve a pinned root")
            }
            XCTAssertEqual(try recordKeys(store.tree), try XCTUnwrap(row["runs"] as? [String]).map(bytes))
        }
    }

    func testUnchangedPostingSetRetainsPhysicalRunsAndPendingVersions() throws {
        let (plan, snapshot) = try maintenance()
        let values = [SQLValue.array(try SQLArray(elements: [.integer(2), .integer(2), .integer(3)]))]
        let entries = try plan.entries({ values[$0] }, storageKey: key(7))
        XCTAssertEqual(entries.count, 2)
        var store = try plan.build(entries, snapshot: snapshot, netStaging: true)
        let revision = store.revision, pending = store.pendingBytes, before = try recordKeys(store.tree)
        let unchanged = StoredIndexPlan.Change(before: entries, after: entries)
        XCTAssertTrue(unchanged.removed.isEmpty); XCTAssertTrue(unchanged.inserted.isEmpty)
        try plan.apply([unchanged], to: &store, twoPass: false, netStaging: true)
        XCTAssertTrue(store.revision === revision); XCTAssertEqual(store.pendingBytes, pending)
        XCTAssertEqual(try recordKeys(store.tree), before)
        let rekeyed = try plan.entries({ values[$0] }, storageKey: key(9))
        let moved = StoredIndexPlan.Change(before: entries, after: rekeyed)
        XCTAssertEqual(moved.removed.count, 2); XCTAssertEqual(moved.inserted.count, 2)
        try plan.apply([moved], to: &store, twoPass: true, netStaging: true)
        for record in try recordKeys(store.tree) {
            XCTAssertEqual(try GinIndex.decodeRun(record, opclasses: XCTUnwrap(plan.ginOpclasses)).keys, [key(9)])
        }
        XCTAssertEqual(store.pendingBytes, Int64(try recordKeys(store.tree).reduce(0) { $0 + $1.count }))
    }

    func testCollatedArrayTermsNeverDemandCollationSortKeys() throws {
        var type = CatalogTypeUse(.array(.scalar(.text))); type.collation = try SQLText("unicode")
        let (plan, _) = try maintenance(type: type, collatedKey: { _, _ in
            XCTFail("GIN byte-identity terms must not demand a collation key")
            throw JedError("0A000", "unexpected collation demand")
        })
        let value = SQLValue.array(try SQLArray(elements: [.text(SQLText("é")), .text(SQLText("e"))]))
        let actual = try plan.entries({ _ in value }, storageKey: key(1)).map(\.term)
        XCTAssertEqual(actual, [GinTerms.terminated(Array("e".utf8)), GinTerms.terminated(Array("é".utf8))])
    }

    func testAllSharedJSONTermVectorsAndFraming() throws {
        let path = "spec/encoding/gin_jsonb_terms.toml", op = try GinOpclass(type: TypeDescriptor(.scalar(.jsonb)))
        for row in try rows(path, "index") {
            let value = try SQLJSON.parse(text: SQLText(XCTUnwrap(row["doc"] as? String)))
            let expected = try XCTUnwrap(row["terms"] as? [String]).map(bytes), terms = try op.indexTerms(value)
            XCTAssertEqual(terms, expected)
            for term in terms { XCTAssertEqual(try op.termLength(in: term + [99, 88]), term.count) }
        }
        for row in try rows(path, "key") {
            let term = GinTerms.jsonKey(try SQLText(XCTUnwrap(row["key"] as? String)))
            XCTAssertEqual(term, try bytes(XCTUnwrap(row["term"] as? String)))
        }
    }

    func testFrozenGINIndexPagesRebuildExactlyIncludingEncryption() throws {
        let names = ["gin_array_table", "gin_decimal_bytea_table", "gin_float_table", "gin_multi_table", "gin_uuid_table",
                     "gin_text_table", "gin_interval_table", "gin_jsonb_table", "encrypted_index_table"]
        var checked = 0
        for name in names {
            let url = try XCTUnwrap(Bundle.module.url(forResource: name, withExtension: "jed", subdirectory: "Fixtures"))
            let bytes = [UInt8](try Data(contentsOf: url)), store = MemoryBlockStore(), encrypted = name.hasPrefix("encrypted_")
            try store.write(at: 0, bytes: bytes)
            let pager = try Pager.open(store: store, credential: encrypted ? .hostKey([UInt8](repeating: 0x4a, count: 32)) : nil)
            let catalog = try CatalogChain.read(root: pager.selected.meta.root, pageCount: pager.selected.meta.pageCount,
                codec: pager.codec, read: pager.readBody)
            let image = try CatalogImage.collect(catalog.entries, catalogID: 1), types = try image.resolvedTypes(catalogID: 1)
            for table in image.tables {
                for definition in table.indexes where definition.kind == 1 {
                    checked += 1
                    let ops = try definition.keys.map { item -> GinOpclass in
                        guard case .column(let column) = item else { throw JedError.corruption("unexpected GIN expression golden") }
                        return try GinOpclass(type: table.columns[column].type.resolve(types))
                    }
                    let pool = try NodePool(pager: pager, cacheBytes: pager.codec.pageSize)
                    let source = try BPlusTree.open(root: definition.root, pageCount: pager.selected.meta.pageCount, types: [], pool: pool)
                    var cursor = TreeCursor(source), postings: [GinPosting] = []
                    while let record = try cursor.next() {
                        let run = try GinIndex.decodeRun(record.key, opclasses: ops)
                        postings.append(contentsOf: run.keys.map { GinPosting(term: run.term, storageKey: $0) })
                    }
                    var pageIDs: [UInt32] = []
                    func visit(_ child: TreeChild) throws {
                        let node = try source.resolve(child)
                        for child in node.children { try visit(child) }
                        pageIDs.append(try XCTUnwrap(child.reference).index)
                    }
                    if let root = source.root { try visit(root) }
                    let snapshot = try DomainSnapshot(catalogID: 1, txid: 0, catalog: image, pool: pool,
                        tables: [:], indexes: [:], gistTrees: [:])
                    let settings = SessionSettings(environment: Environment.builder().build(), options: SessionOptions())
                    let plan = try StoredIndexPlan.resolve(table: table, index: definition, snapshot: snapshot,
                        settings: settings, context: ExpressionContext(meter: CostMeter()))
                    let rebuilt = try plan.build(postings.map {
                        StoredIndexPlan.Entry(bytes: $0.key, storageKeyLength: $0.storageKey.count)
                    }, snapshot: snapshot, netStaging: true)
                    XCTAssertEqual(try recordKeys(rebuilt.tree), try recordKeys(source), name)
                    var allocator = try PageAllocator(highWater: pager.selected.meta.pageCount, free: pageIDs.sorted(), firstBody: pager.codec.firstBodyPage)
                    var salts = pageIDs.map { page in
                        let end = (Int(page) + 1) * pager.codec.pageSize
                        return Array(bytes[(end - 40)..<(end - 16)])
                    }.makeIterator()
                    let written = try rebuilt.tree.serialize(allocator: &allocator, fromScratch: true,
                        entropy: encrypted ? { count in XCTAssertEqual(count, 24); return try XCTUnwrap(salts.next()) } : nil)
                    XCTAssertEqual(written.root, definition.root, name)
                    for page in written.pages {
                        let start = Int(page.index) * pager.codec.pageSize
                        XCTAssertEqual(page.bytes, Array(bytes[start..<(start + pager.codec.pageSize)]), "\(name):\(page.index)")
                    }
                }
            }
        }
        XCTAssertGreaterThanOrEqual(checked, 9)
    }

    func testTermCanonicalizationAndQueryDegeneracies() throws {
        let integer = try GinOpclass(type: TypeDescriptor(.array(.scalar(.i32))))
        let decimal = try GinOpclass(type: TypeDescriptor(.array(.scalar(.decimal))))
        XCTAssertEqual(try decimal.queryTerm(.integer(2)), try decimal.queryTerm(.decimal(SQLDecimal.parse("2.00"))))
        XCTAssertEqual(try integer.queryTerm(.decimal(SQLDecimal.parse("2.00"))), try integer.queryTerm(.integer(2)))
        XCTAssertNil(try integer.queryTerm(.decimal(SQLDecimal.parse("2.01"))))
        XCTAssertNil(try integer.queryTerm(.integer(Int64.max)))
        let float = try GinOpclass(type: TypeDescriptor(.array(.scalar(.f32))))
        XCTAssertEqual(try float.queryTerm(.f64((-0.0).bitPattern)), try float.elementTerm(.f32(0)))
        XCTAssertEqual(try float.queryTerm(.f64(Double.nan.bitPattern)), try float.elementTerm(.f32(Float.nan.bitPattern)))
        XCTAssertNil(try float.queryTerm(.f64(0.1.bitPattern)))
        let interval = try GinOpclass(type: TypeDescriptor(.array(.scalar(.interval))))
        XCTAssertEqual(try interval.elementTerm(.interval(SQLInterval(months: 1))), try interval.elementTerm(.interval(SQLInterval(days: 30))))
        let text = try GinOpclass(type: TypeDescriptor(.array(.scalar(.text))))
        let prefix = String(repeating: "é", count: 64)
        XCTAssertEqual(try text.elementTerm(.text(SQLText(prefix))), try text.elementTerm(.text(SQLText(prefix + "z"))))
        let allNull = SQLValue.array(try SQLArray(elements: [.null])), empty = SQLValue.array(try SQLArray(elements: []))
        if case .empty = try integer.query(allNull, strategy: .contains) {} else { XCTFail("NULL-containing @> must be empty") }
        for value in [allNull, empty] {
            if case .fullScan = try integer.query(value, strategy: .equal) {} else { XCTFail("termless equality requires full scan") }
        }
        if case .fullScan = try integer.query(empty, strategy: .contains) {} else { XCTFail("empty containment requires full scan") }
        if case .empty = try integer.query(empty, strategy: .overlaps) {} else { XCTFail("empty overlap matches nothing") }
    }

    func testSkipScanCandidateSetsChargesAndCollectorLifetime() throws {
        let codec = try PlainPageCodec(pageSize: 256), terms = [key(1), key(2), key(3)]
        var postings: [GinPosting] = []
        for number in 0..<500 {
            postings.append(GinPosting(term: terms[0], storageKey: key(number)))
            if number % 7 == 0 { postings.append(GinPosting(term: terms[1], storageKey: key(number))) }
            if number % 11 == 0 { postings.append(GinPosting(term: terms[2], storageKey: key(number))) }
        }
        let index = try GinIndex.build(postings: postings, codec: codec)
        let meter = CostMeter(resources: StatementResources(queryLimit: 1))
        let query = GinQueryTree.all(terms.map(GinQueryTree.term))
        let result = try GinGather.gather([.gather(query)], index: index, meter: meter)
        XCTAssertEqual(candidates(result), stride(from: 0, to: 500, by: 77).map(key))
        XCTAssertEqual(meter.accrued, Int64(result.ginEntries))
        XCTAssertEqual(meter.resources.query.used, 0, "frozen Q1/Q2 schedule excludes posting keys")
        XCTAssertLessThan(result.ginEntries, postings.count)
        XCTAssertGreaterThan(result.pageReads, 0)
        let any = try GinGather.gather([.gather(.any([.term(terms[1]), .term(terms[2])]))], index: index)
        XCTAssertEqual(candidates(any), (0..<500).filter { $0 % 7 == 0 || $0 % 11 == 0 }.map(key))
        XCTAssertEqual(any.ginEntries, (0..<500).filter { $0 % 7 == 0 }.count + (0..<500).filter { $0 % 11 == 0 }.count)
        let twice = try GinGather.gather([.gather(.term(terms[1])), .gather(.term(terms[1]))], index: index)
        XCTAssertEqual(twice.ginEntries, 2 * (0..<500).filter { $0 % 7 == 0 }.count)
        twice.candidates.close(); XCTAssertTrue(twice.candidates.isClosed); XCTAssertNil(twice.candidates.next())
        let full = try GinGather.gather([.gather(query), .fullScan], index: index)
        XCTAssertTrue(full.requiresFullScan); XCTAssertEqual(full.pageReads, 0); XCTAssertEqual(full.ginEntries, 0)
        let canceled = CancellationToken(); canceled.cancel()
        XCTAssertThrowsError(try GinGather.gather([.gather(query)], index: index, meter: CostMeter(cancellation: canceled))) {
            XCTAssertEqual(($0 as? JedError)?.code, "57014")
        }
    }

    func testMalformedRunsAndAtomicSerializationFailure() throws {
        for malformed: [UInt8] in [[], [1, 0, 1], [1, 2, 0xff, 0xff], [1, 2, 0x80, 0x80, 0x80, 0, 3],
                                   [1, 2, 3, 0, 0, 2], [1, 2, 0, 0, 0, 2]] {
            expectCorruption { _ = try GinPostingRun.decode(termLength: 1, bytes: malformed) }
        }
        let term = key(1), codec = try PlainPageCodec(pageSize: 256)
        let index = try GinIndex.build(postings: [GinPosting(term: term, storageKey: key(1))], codec: codec)
        let before = try recordKeys(index.tree)
        // Pending key-only nodes retain their staged bytes until commit
        // preflight checks page fit, just like the other index trees.
        var pending = index
        XCTAssertTrue(try pending.insert(term: term, key: [UInt8](repeating: 200, count: 300)))
        var allocator = try PageAllocator(highWater: 4, free: [2, 3], firstBody: codec.firstBodyPage)
        XCTAssertThrowsError(try pending.tree.serialize(allocator: &allocator, fromScratch: true)) {
            XCTAssertEqual(($0 as? JedError)?.code, "0A000")
        }
        XCTAssertEqual(allocator.highWater, 4)
        XCTAssertEqual(Array(allocator.remaining), [2, 3])
        XCTAssertEqual(try recordKeys(index.tree), before)
        let op = try GinOpclass(type: TypeDescriptor(.scalar(.jsonb)))
        for term in [[UInt8(0)], [0x12], [0x13, 0], [3, 2], [1, 0, 3]] {
            expectCorruption { _ = try op.termLength(in: term) }
        }
    }
}
