/// An index definition resolves once; its per-row keys are trusted maintenance
/// work, unmetered for cost but sharing scalar allocation and cancellation seams.
internal struct OrderedIndexPlan {
    struct Prefix { let bytes: [UInt8]; let hasNull: Bool }
    let index: CatalogIndex
    let keys: [ResolvedExpression]
    let types: [TypeDescriptor]
    let predicate: ResolvedExpression?
    let touched: [Int]
    let context: ExpressionContext

    static func resolve(table: CatalogTable, index: CatalogIndex, snapshot: DomainSnapshot,
                        settings: SessionSettings, context: ExpressionContext) throws -> OrderedIndexPlan {
        guard index.kind == 0 else { throw JedError("0A000", "inverted and GiST index maintenance is not yet connected") }
        try IndexDependencies.validateZones(index, environment: settings.environment)
        let columns = try table.columns.enumerated().map {
            ExpressionColumn(name: $0.element.name, qualifier: table.name, type: try $0.element.type.resolve(snapshot.types), index: $0.offset)
        }
        let resolver = ExpressionResolver(columns: columns, composites: snapshot.types, scalar: context.scalar)
        resolver.hostFunctions = settings.environment.signatures; resolver.unicode = settings.environment.unicode
        resolver.storageCollation = true
        let keys = try index.keys.map { key -> ResolvedExpression in
            switch key {
            case .column(let index):
                resolver.referencedColumns.insert(index)
                return ResolvedExpression(type: columns[index].type, node: .column(index), position: 0,
                    collation: ExpressionCollation.collatable(columns[index].type.type) ? .implicit(columns[index].type.collation ?? SQLText(validatedUTF8: [67])) : nil)
            case .expression(let definition):
                guard let syntax = definition.parsed else { throw JedError("XX003", "stored index expression cannot be parsed") }
                return try resolver.resolve(syntax)
            }
        }
        let types = try keys.map { key -> TypeDescriptor in
            guard var type = key.type else { throw JedError("42P18", "could not determine index key type") }
            type.collation = try ExpressionCollation.derive(key, storage: true).orderingName()
            try KeyEncoding.validateType(type.type); return type
        }
        let predicate = try index.predicate.map { definition in
            guard let syntax = definition.parsed else { throw JedError("XX003", "stored index predicate cannot be parsed") }
            return try resolver.resolve(syntax, expected: TypeDescriptor(.scalar(.boolean)))
        }
        try StoredExpressionDependencies.validateHosts(index.hosts, expressions: keys + (predicate.map { [$0] } ?? []), environment: settings.environment)
        let meter = CostMeter(resources: context.meter.resources, cancellation: context.meter.cancellation)
        var unmetered = ExpressionContext(meter: meter, timezone: context.scalar.timezone)
        unmetered.scalar = context.scalar; unmetered.scalar.meter = meter
        unmetered.collatedKey = context.collatedKey; unmetered.functions = context.functions
        return OrderedIndexPlan(index: index, keys: keys, types: types, predicate: predicate,
                                touched: resolver.referencedColumns.sorted(), context: unmetered)
    }
    func prefix(_ value: @escaping (Int) throws -> SQLValue) throws -> Prefix? {
        var row = context; row.column = value
        if let predicate, try !row.matches(predicate) { return nil }
        var bytes: [UInt8] = [], null = false
        for ordinal in keys.indices {
            let value = try row.evaluate(keys[ordinal]), option = index.keyOptions.isEmpty ? 0 : index.keyOptions[ordinal]
            null = null || value.isNull
            bytes += try KeyEncoding.encodeSlot(value, type: types[ordinal], descending: option & 1 != 0,
                nullsFirst: option & 2 != 0, collatedKey: context.collatedKey)
        }
        return Prefix(bytes: bytes, hasNull: null)
    }
    static func successor(_ bytes: [UInt8]) -> [UInt8]? {
        var result = bytes
        while let last = result.popLast() {
            if last < 255 { result.append(last + 1); return result }
        }
        return nil
    }
    func conflicts(_ prefix: Prefix, store: TableStore, vacated: Set<[UInt8]> = []) throws -> Bool {
        guard index.unique, !prefix.hasNull else { return false }
        var cursor = store.cursor(lower: TreeBound(prefix.bytes), upper: Self.successor(prefix.bytes).map { TreeBound($0, inclusive: false) })
        defer { cursor.close() }
        while let record = try cursor.next() {
            if !vacated.contains(Array(record.key.dropFirst(prefix.bytes.count))) { return true }
        }
        return false
    }
}
