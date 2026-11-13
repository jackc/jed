/// Column identities survive ordinal edits; conversion expressions themselves
/// retain the action-visible row layout in which they were resolved.
internal enum AlterRowSource {
    case original(Int)
    case added(CatalogColumn)
}
internal enum AlterRowStep {
    case append(CatalogColumn, TypeDescriptor, ResolvedExpression)
    case remove(Int)
    case convert(Int, ResolvedExpression)
}
internal enum AlterRowRewrite {
    struct Row { let key: [UInt8]; let values: [SQLValue]; let record: PackedRecord }
    static func append(_ column: CatalogColumn, snapshot: DomainSnapshot, settings: SessionSettings,
                       context: ExpressionContext) throws -> AlterRowStep {
        let type = try column.type.resolve(snapshot.types)
        let expression: ResolvedExpression
        switch column.defaultValue {
        case .none: expression = ResolvedExpression(type: type, node: .constant(.null), position: 0)
        case .constant(let value): expression = ResolvedExpression(type: type, node: .constant(value), position: 0)
        case .expression(let definition):
            guard let syntax = definition.parsed else { throw JedError("XX003", "stored default expression cannot be parsed") }
            let resolver = try CatalogDDL.resolver(snapshot: snapshot, settings: settings, context: context)
            expression = try AssignmentValues.resolve(syntax, to: type, resolver: resolver)
        }
        return .append(column, type, expression)
    }
    /// Conversion and compression admission precede the separate NOT NULL scan
    /// and final CHECK/index/FK/exclusion validation. Validation reuses these
    /// rows without charging another read of the rewritten table.
    static func materialize(table: CatalogTable, steps: [AlterRowStep], original: TableStore,
                            rekey: Bool, snapshot: DomainSnapshot, context: ExpressionContext) throws -> (rows: [Row], nextID: Int64) {
        let types = try table.columns.map { try $0.type.resolve(snapshot.types) }
        var allocation = TableStore(tree: BPlusTree(codec: original.tree.codec, types: types, pool: original.tree.pool, layout: .table(types: types, primaryKey: table.primaryKey)),
                                    nextRowID: rekey && table.primaryKey.isEmpty ? 0 : original.nextRowID)
        var rows: [Row] = [], keys: Set<[UInt8]> = []
        let compression = CostMeter(resources: context.meter.resources, cancellation: context.meter.cancellation)
        try ScanCost.charge(original, touched: Array(original.tree.types.indices), meter: context.meter)
        var cursor = original.cursor(); defer { cursor.close() }
        while let record = try cursor.next() {
            try context.meter.step(); context.meter.charge(1)
            let source = ScanRecord(record, types: original.tree.types, pager: original.tree.pool?.pager)
            var values = try original.tree.types.indices.map { try source.value($0) }
            for step in steps {
                switch step {
                case .append(let column, let type, let expression):
                    let value = try AssignmentValues.store(context.evaluate(expression), expression: expression, to: type, context: context)
                    if column.notNull && value.isNull { throw JedError("23502", "column \(column.name) contains null values").object(table: table.name, column: column.name) }
                    values.append(value)
                case .remove(let ordinal): values.remove(at: ordinal)
                case .convert(let ordinal, let expression):
                    var execution = context; let stable = values
                    execution.column = { stable[$0] }
                    values[ordinal] = try execution.evaluate(expression)
                }
            }
            for index in table.columns.indices where table.columns[index].notNull && values[index].isNull {
                throw JedError("23502", "column \(table.columns[index].name) contains null values").object(table: table.name, column: table.columns[index].name)
            }
            let key: [UInt8]
            if !rekey { key = record.key }
            else if table.primaryKey.isEmpty { key = try KeyEncoding.encode(.integer(allocation.allocateRowID()), type: TypeDescriptor(.scalar(.i64))) }
            else {
                var bytes: [UInt8] = []
                for column in table.primaryKey { bytes += try KeyEncoding.encode(values[column], type: types[column], collatedKey: context.collatedKey) }
                key = bytes
            }
            guard keys.insert(key).inserted else { throw JedError("23505", "duplicate key value violates primary key of \(table.name)")
                .object(table: table.name, constraint: CatalogSQL.objectName(table.name, label: "pkey"),
                    detail: context.errorDetail?.primary(table, types: types, value: { values[$0] }, ending: "is duplicated.")) }
            let rewritten = try RecordDisposition.plan(key: key, values: values.map(RecordInput.value), types: types,
                codec: original.tree.codec, layout: .table(types: types, primaryKey: table.primaryKey), meter: compression, pager: original.tree.pool?.pager)
            rows.append(Row(key: key, values: values, record: rewritten))
        }
        context.meter.charge(compression.accrued); try context.meter.step()
        return (rows, allocation.nextRowID)
    }
    static func publish(_ rows: [Row], nextID: Int64, types: [TypeDescriptor], original: TableStore,
                        settings: SessionSettings, primaryKey: [Int]) throws -> TableStore {
        var result = TableStore(tree: BPlusTree(codec: original.tree.codec, types: types, pool: original.tree.pool, layout: .table(types: types, primaryKey: primaryKey)), nextRowID: nextID)
        for row in rows.sorted(by: { $0.key.lexicographicallyPrecedes($1.key) }) {
            // Frozen ALTER rewrites retain a cumulative pending balance. Later
            // DML starts tracking new versions without reclaiming these rows.
            try result.put(row.record, netStaging: false)
        }
        return result
    }
}
