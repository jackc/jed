/// Bounded, deterministic column facts. Catalog publication happens only after
/// every authored column scan has completed successfully.
internal enum AnalyzeExecution {
    static func limit(_ name: String) -> Int {
        guard case .integer(let value) = GeneratedSpec.estimator["statistics_" + name] else {
            preconditionFailure("missing canonical statistics limit")
        }
        return Int(value)
    }
    static func execute(_ name: SQLName, columns names: [SQLText], snapshot: inout DomainSnapshot,
                        settings: SessionSettings, context: ExpressionContext) throws -> CommandTag {
        try SystemRelationPlan.rejectTarget(name.name)
        guard let table = snapshot.catalog.tables.first(where: { $0.name == name.name }),
              let store = snapshot.tables[name.name] else {
            if snapshot.catalog.views.contains(where: { $0.name == name.name }) {
                return CommandTag(command: "ANALYZE", rowsAffected: 0, cost: context.meter.accrued)
            }
            throw JedError("42P01", "table does not exist: \(name.name)")
        }
        try CatalogEnvironment.checkTableCollations(table, image: snapshot.catalog, environment: settings.environment)
        var columns: [Int] = [], seen: Set<SQLText> = []
        if names.isEmpty { columns = Array(table.columns.indices) }
        else {
            for name in names {
                guard seen.insert(name).inserted else { throw JedError("42701", "column appears more than once: \(name)") }
                guard let ordinal = table.columns.firstIndex(where: { $0.name == name }) else {
                    throw JedError("42703", "column does not exist: \(name)")
                }
                columns.append(ordinal)
            }
        }
        let types = try snapshot.tableTypes(table)
        var facts: [CatalogStatistics] = []
        for column in columns {
            facts.append(try collect(table: table.name, column: column, type: types[column], store: store, context: context))
        }
        var image = snapshot.catalog
        image.statistics.removeAll { $0.table == table.name && columns.contains($0.column) }
        image.statistics += facts
        image.statistics.sort { $0.table == $1.table ? $0.column < $1.column : $0.table < $1.table }
        try snapshot.updateCatalog(image, schemaChanged: false)
        return CommandTag(command: "ANALYZE", rowsAffected: 0, cost: context.meter.accrued)
    }

    private struct Sample {
        let priority: UInt64
        let ordinal: Int64
        let nonnull: Bool
        let oversized: Bool
        let key: [UInt8]
        let bytes: [UInt8]
        static func less(_ left: Sample, _ right: Sample) -> Bool {
            left.priority == right.priority ? left.ordinal < right.ordinal : left.priority < right.priority
        }
    }
    private static func hash(_ bytes: [UInt8]) -> UInt64 {
        bytes.reduce(UInt64(14_695_981_039_346_656_037)) { ($0 ^ UInt64($1)) &* 1_099_511_628_211 }
    }
    private static func eligible(_ type: SQLType) -> Bool {
        switch type {
        case .scalar(.json), .scalar(.jsonb), .scalar(.jsonpath): false
        case .scalar, .range: true
        default: false
        }
    }
    private static func bodyWidth(_ value: SQLValue, type: TypeDescriptor) throws -> Int {
        switch value {
        // Large top-level string bodies reside in overflow chains; the ordinary
        // canonical body still counts its two-byte length field.
        case .text(let text), .json(let text), .jsonpath(let text): return text.utf8.count + 2
        case .bytea(let bytes): return bytes.count + 2
        default: return try ValueCodec.encodeBody(value, type: type).count
        }
    }
    private static func collect(table: SQLText, column: Int, type: TypeDescriptor, store: TableStore,
                                context: ExpressionContext) throws -> CatalogStatistics {
        let meter = context.meter, distribution = eligible(type.type), opclass = try? GinOpclass(type: type)
        var result = CatalogStatistics(table: table, column: column, stale: false,
            distributionEligible: distribution, termEligible: opclass != nil, analyzed: 0, nulls: 0,
            widthSum: 0, distinct: 0, sampleRows: 0, sampleNonnull: 0)
        let sampleLimit = limit("sample_rows"), kmvLimit = limit("kmv_hashes"), maximumBytes = limit("max_value_bytes")
        var samples = StatisticsMaxHeap<Sample>(less: Sample.less)
        var kmv = StatisticsMaxHeap<UInt64>(less: <), hashes: Set<UInt64> = [], overflow = false
        var terms = StatisticsTermCounter()
        meter.charge(Int64(store.tree.nodeCount))
        var cursor = store.cursor(); defer { cursor.close() }
        while let record = try cursor.next() {
            try meter.step()
            var units: Int64 = 1
            if ValueCodec.fixedWidth(type.type) == nil,
               let facts = try record.cells[column].largeFacts(encrypted: store.tree.codec.reservedBytes != 0) {
                let slab = store.tree.codec.capacity
                let chain = slab - (store.tree.codec.reservedBytes == 0 ? 0 : 16)
                if facts.external != nil { units = saturatingAdd(units, Int64((facts.stored + chain - 1) / chain)) }
                if facts.compressed { units = saturatingAdd(units, Int64((facts.raw + slab - 1) / slab)) }
            }
            meter.charge(units)
            let value = try ScanRecord(record, pager: store.tree.pool?.pager).value(column)
            let ordinal = result.analyzed
            result.analyzed = saturatingAdd(result.analyzed, 1)
            var key: [UInt8] = [], bytes: [UInt8] = [], oversized = false
            if value.isNull {
                result.nulls = saturatingAdd(result.nulls, 1); meter.charge(1); try meter.guardCost()
            } else {
                let body = try bodyWidth(value, type: type)
                if distribution { key = try KeyEncoding.encode(value, type: type, collatedKey: context.collatedKey) }
                let width = distribution ? key.count : body
                result.widthSum = saturatingAdd(result.widthSum, Int64(width))
                meter.charge(Int64(max(1, width))); try meter.guardCost()
                if distribution {
                    let fingerprint = hash(key)
                    if !hashes.contains(fingerprint) {
                        if hashes.count < kmvLimit { hashes.insert(fingerprint); kmv.insert(fingerprint) }
                        else {
                            overflow = true
                            if fingerprint < kmv.values[0] {
                                hashes.remove(kmv.replaceMaximum(fingerprint)); hashes.insert(fingerprint)
                            }
                        }
                    }
                }
                if let opclass {
                    for term in try opclass.indexTerms(value) {
                        meter.charge(Int64(max(1, term.count))); try meter.guardCost(); terms.add(term)
                    }
                }
                oversized = body > maximumBytes || (distribution && key.count > maximumBytes)
                if distribution && !oversized { bytes = try ValueCodec.encode(value, type: type) }
                else { key = [] }
            }
            let sample = Sample(priority: hash(record.key), ordinal: ordinal, nonnull: !value.isNull,
                                oversized: oversized, key: key, bytes: bytes)
            if samples.values.count < sampleLimit { samples.insert(sample) }
            else if Sample.less(sample, samples.values[0]) { _ = samples.replaceMaximum(sample) }
        }
        try meter.step()
        result.sampleRows = UInt32(samples.values.count)
        result.sampleNonnull = UInt32(samples.values.filter(\.nonnull).count)
        if distribution {
            result.distinct = Int64(hashes.count)
            if overflow {
                let numerator = UInt128(kmvLimit - 1) << 64, denominator = UInt128(kmv.values[0]) + 1
                let estimate = (numerator + denominator - 1) / denominator
                result.distinct = Int64(min(UInt128(result.analyzed - result.nulls), max(UInt128(kmvLimit + 1), estimate)))
            }
            finish(samples.values, result: &result)
        }
        terms.finish(into: &result)
        return result
    }

    private static func finish(_ samples: [Sample], result: inout CatalogStatistics) {
        struct Group { var sample: Sample; var count: UInt32 }
        let oversized = samples.contains { $0.nonnull && $0.oversized }
        let ordered = samples.filter { $0.nonnull && !$0.oversized }.sorted {
            $0.key == $1.key ? $0.ordinal < $1.ordinal : $0.key.lexicographicallyPrecedes($1.key)
        }
        var groups: [Group] = []
        for sample in ordered {
            if let last = groups.last, last.sample.key == sample.key { groups[groups.count - 1].count += 1 }
            else { groups.append(Group(sample: sample, count: 1)) }
        }
        let maximum = limit("mcv_entries")
        let complete = Int64(samples.count) == result.analyzed && !oversized && groups.count <= maximum
        let common = groups.filter { complete || ($0.count >= 2 && UInt128($0.count) * UInt128(result.distinct) > UInt128(result.sampleNonnull)) }
            .sorted { $0.count == $1.count ? $0.sample.key.lexicographicallyPrecedes($1.sample.key) : $0.count > $1.count }
            .prefix(maximum)
        result.mcv = common.map { CatalogStatistics.MCV(frequency: $0.count, bytes: $0.sample.bytes) }
        if oversized { return }
        let selected = Set(common.map { $0.sample.key })
        let remaining = ordered.filter { !selected.contains($0.key) }
        guard remaining.count >= 2 else { return }
        let bounds = min(limit("histogram_bounds"), remaining.count)
        result.histogram = (0..<bounds).map { remaining[$0 * (remaining.count - 1) / (bounds - 1)].bytes }
    }
}

/// Max root, bounded by one canonical statistics limit. Ties in the row sample
/// include the storage ordinal, so heap shape cannot select different rows.
private struct StatisticsMaxHeap<Element> {
    var values: [Element] = []
    let less: (Element, Element) -> Bool
    mutating func insert(_ element: Element) {
        values.append(element)
        var index = values.count - 1
        while index > 0 {
            let parent = (index - 1) / 2
            if !less(values[parent], values[index]) { break }
            values.swapAt(parent, index); index = parent
        }
    }
    mutating func replaceMaximum(_ element: Element) -> Element {
        let previous = values[0]; values[0] = element
        var index = 0
        while index * 2 + 1 < values.count {
            var child = index * 2 + 1
            if child + 1 < values.count && less(values[child], values[child + 1]) { child += 1 }
            if !less(values[index], values[child]) { break }
            values.swapAt(index, child); index = child
        }
        return previous
    }
}

private struct StatisticsTermCounter {
    struct Entry { var count: Int64; let delta: Int64 }
    var entries: [[UInt8]: Entry] = [:]
    var occurrences: Int64 = 0
    var oversized = false
    var pruned = false
    mutating func add(_ term: [UInt8]) {
        occurrences = saturatingAdd(occurrences, 1)
        let width = Int64(AnalyzeExecution.limit("term_bucket_width"))
        let bucket = occurrences / width + (occurrences % width == 0 ? 0 : 1)
        if term.count > AnalyzeExecution.limit("max_value_bytes") { oversized = true }
        else if var entry = entries[term] { entry.count = saturatingAdd(entry.count, 1); entries[term] = entry }
        else { entries[term] = Entry(count: 1, delta: bucket - 1) }
        if occurrences % width == 0 {
            let before = entries.count
            entries = entries.filter { saturatingAdd($0.value.count, $0.value.delta) > bucket }
            pruned = pruned || entries.count != before
        }
    }
    func finish(into result: inout CatalogStatistics) {
        guard result.termEligible else { return }
        let width = Int64(AnalyzeExecution.limit("term_bucket_width")), maximum = AnalyzeExecution.limit("term_entries")
        let ordered = entries.sorted {
            $0.value.count == $1.value.count ? $0.key.lexicographicallyPrecedes($1.key) : $0.value.count > $1.value.count
        }
        result.termOccurrences = occurrences
        result.untrackedTermRows = oversized ? 1 : 0
        if pruned { result.untrackedTermRows = max(result.untrackedTermRows, occurrences / width + (occurrences % width == 0 ? 0 : 1)) }
        if ordered.count > maximum { result.untrackedTermRows = max(result.untrackedTermRows, ordered[maximum].value.count) }
        result.terms = ordered.prefix(maximum).map { CatalogStatistics.Term(count: $0.value.count, bytes: $0.key) }
    }
}
