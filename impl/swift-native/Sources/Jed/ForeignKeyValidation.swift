/// Foreign-key equality is defined in the parent's key space. A conversion may
/// have no exact equal (for example f64 0.1 in f32); that is a missing reference,
/// never a lossy cast or an arithmetic error.
internal enum ForeignKeyValidation {
    struct Candidate {
        let key: [UInt8]
        let value: (Int) throws -> SQLValue
    }
    enum Probe { case exempt, missing, key([UInt8]) }
    struct Plan {
        let child: CatalogTable
        let foreign: CatalogForeignKey
        let parent: CatalogTable
        let childTypes: [TypeDescriptor]
        let parentTypes: [TypeDescriptor]
        let keyColumns: [Int]
        let index: CatalogIndex?

        init(child: CatalogTable, foreign: CatalogForeignKey, snapshot: DomainSnapshot, parent override: CatalogTable? = nil) throws {
            guard let parent = override ?? snapshot.catalog.tables.first(where: { $0.name == foreign.referencedTable }) else {
                throw JedError.corruption("foreign key references a missing table")
            }
            self.child = child; self.foreign = foreign; self.parent = parent
            childTypes = try child.columns.map { try $0.type.resolve(snapshot.types) }
            parentTypes = try parent.columns.map { try $0.type.resolve(snapshot.types) }
            if !parent.primaryKey.isEmpty, Set(parent.primaryKey) == Set(foreign.referencedColumns) {
                keyColumns = parent.primaryKey; index = nil
            } else {
                guard let chosen = parent.indexes.sorted(by: { $0.name < $1.name }).first(where: {
                    $0.kind == 0 && $0.unique && $0.predicate == nil && plainColumns($0).map { Set($0) == Set(foreign.referencedColumns) } == true
                }), let columns = plainColumns(chosen) else { throw JedError.corruption("foreign key has no referenced unique key") }
                keyColumns = columns; index = chosen
            }
        }
        func violation(parentSide: Bool = false, mixedNulls: Bool = false) -> JedError {
            let table = parentSide ? parent.name : child.name
            var error = JedError("23503", "\(parentSide ? "update or delete" : "insert or update") on table \(table) violates foreign key constraint \(foreign.name)")
            error.tableName = table.description; error.constraintName = foreign.name.description
            if mixedNulls { error.detail = "MATCH FULL does not allow mixing of null and nonnull key values." }
            return error
        }
        func childProbe(_ value: (Int) throws -> SQLValue, context: ExpressionContext,
                        enforceMatch: Bool = true) throws -> Probe {
            let tuple = try foreign.columns.map(value), nulls = tuple.filter(\.isNull).count
            if nulls > 0 {
                if enforceMatch, foreign.matchFull, nulls < tuple.count { throw violation(mixedNulls: true) }
                return .exempt
            }
            var converted: [SQLValue] = []
            for position in tuple.indices {
                guard let equal = try exact(tuple[position], to: parentTypes[foreign.referencedColumns[position]].type) else { return .missing }
                converted.append(equal)
            }
            return .key(try encodeParentTuple(converted, context: context))
        }
        /// The tuple is in REFERENCES-list order, regardless of the physical
        /// parent's PK/index ordering. Every member is already a parent value.
        func encodeParentTuple(_ tuple: [SQLValue], context: ExpressionContext) throws -> [UInt8] {
            var result: [UInt8] = []
            for (position, column) in keyColumns.enumerated() {
                guard let paired = foreign.referencedColumns.firstIndex(of: column) else { throw JedError.corruption("foreign key order is inconsistent") }
                if let index {
                    let options = index.keyOptions.isEmpty ? 0 : index.keyOptions[position]
                    result += try KeyEncoding.encodeSlot(tuple[paired], type: parentTypes[column], descending: options & 1 != 0,
                        nullsFirst: options & 2 != 0, collatedKey: context.collatedKey)
                } else { result += try KeyEncoding.encode(tuple[paired], type: parentTypes[column], collatedKey: context.collatedKey) }
            }
            return result
        }
        func parentProbe(_ value: (Int) throws -> SQLValue, context: ExpressionContext) throws -> [UInt8]? {
            let tuple = try foreign.referencedColumns.map(value)
            if tuple.contains(where: \.isNull) { return nil }
            return try encodeParentTuple(tuple, context: context)
        }
        func parentExists(_ key: [UInt8], snapshot: DomainSnapshot, candidates: [Candidate] = [],
                          vacated: Set<[UInt8]> = [], context: ExpressionContext) throws -> Bool {
            for candidate in candidates {
                if try parentProbe(candidate.value, context: context) == key { return true }
            }
            if let index {
                guard let store = snapshot.indexes[index.name] else { throw JedError.corruption("foreign key index is missing") }
                var cursor = store.cursor(lower: TreeBound(key), upper: OrderedIndexPlan.successor(key).map { TreeBound($0, inclusive: false) })
                defer { cursor.close() }
                while let row = try cursor.next() {
                    if !vacated.contains(Array(row.key.dropFirst(key.count))) { return true }
                }
                return false
            }
            guard !vacated.contains(key) else { return false }
            guard let store = snapshot.tables[parent.name] else { throw JedError.corruption("foreign key table is missing") }
            return try store.get(key) != nil
        }
        /// Reverse selection is deterministic and independent of row values:
        /// PK first, then nonpartial plain-prefix indexes in name order.
        func reversePath(snapshot: DomainSnapshot, settings: SessionSettings) -> (columns: [Int], index: CatalogIndex?)? {
            let width = foreign.columns.count, wanted = Set(foreign.columns)
            func leading(_ columns: [Int]) -> Bool {
                guard columns.count >= width, Set(columns.prefix(width)) == wanted else { return false }
                for column in columns.prefix(width) {
                    guard let position = foreign.columns.firstIndex(of: column) else { return false }
                    if ExpressionCollation.collatable(childTypes[column].type) {
                        let childName = childTypes[column].collation ?? SQLText(validatedUTF8: [67])
                        let parentName = parentTypes[foreign.referencedColumns[position]].collation ?? SQLText(validatedUTF8: [67])
                        guard childName == parentName, CatalogEnvironment.collationMatches(childName, image: snapshot.catalog,
                            environment: settings.environment) else { return false }
                    }
                }
                return true
            }
            if leading(child.primaryKey) { return (Array(child.primaryKey.prefix(width)), nil) }
            for index in child.indexes.sorted(by: { $0.name < $1.name }) where index.kind == 0 && index.predicate == nil {
                guard index.keys.count >= width else { continue }
                var columns: [Int] = []
                for key in index.keys {
                    if case .column(let column) = key { columns.append(column) } else { break }
                }
                guard columns.count == index.keys.count, leading(columns),
                      columns.dropFirst(width).allSatisfy({ ValueCodec.fixedWidth(childTypes[$0].type) != nil }) else { continue }
                return (Array(columns.prefix(width)), index)
            }
            return nil
        }
        func reversePrefix(_ tuple: [SQLValue], columns: [Int], index: CatalogIndex?,
                           context: ExpressionContext) throws -> [UInt8]? {
            var key: [UInt8] = []
            for (position, column) in columns.enumerated() {
                let paired = foreign.columns.firstIndex(of: column)!
                guard let value = try exact(tuple[paired], to: childTypes[column].type) else { return nil }
                if let definition = index {
                    let options = definition.keyOptions.isEmpty ? 0 : definition.keyOptions[position]
                    key += try KeyEncoding.encodeSlot(value, type: childTypes[column], descending: options & 1 != 0,
                        nullsFirst: options & 2 != 0, collatedKey: context.collatedKey)
                } else { key += try KeyEncoding.encode(value, type: childTypes[column], collatedKey: context.collatedKey) }
            }
            return key
        }
        func hasChild(_ tuple: [SQLValue], snapshot: DomainSnapshot, settings: SessionSettings,
                      context: ExpressionContext) throws -> Bool {
            guard !tuple.contains(where: \.isNull) else { return false }
            if let path = reversePath(snapshot: snapshot, settings: settings) {
                guard let key = try reversePrefix(tuple, columns: path.columns, index: path.index, context: context) else { return false }
                let selected: TableStore?
                if let index = path.index { selected = snapshot.indexes[index.name] }
                else { selected = snapshot.tables[child.name] }
                guard let store = selected else { throw JedError.corruption("foreign key reverse store is missing") }
                let access = ScanAccess(lower: TreeBound(key), upper: OrderedIndexPlan.successor(key).map { TreeBound($0, inclusive: false) }, isEmpty: false)
                context.meter.charge(access.pages(store.tree)); try context.meter.guardCost()
                var cursor = access.cursor(store); defer { cursor.close() }
                return try cursor.next() != nil
            }
            guard let store = snapshot.tables[child.name] else { throw JedError.corruption("foreign key child is missing") }
            let key = try encodeParentTuple(tuple, context: context)
            try ScanCost.charge(store, touched: foreign.columns, meter: context.meter)
            var cursor = store.cursor(); defer { cursor.close() }
            while let record = try cursor.next() {
                context.meter.charge(1); try context.meter.step()
                let row = ScanRecord(record, pager: store.tree.pool?.pager)
                if case .key(let candidate) = try childProbe(row.value, context: context, enforceMatch: false), candidate == key { return true }
            }
            return false
        }
    }
    static func validateChildren(_ candidates: [Candidate], child: CatalogTable, snapshot: DomainSnapshot,
                                 assigned: Set<Int>? = nil, vacated: Set<[UInt8]> = [], context: ExpressionContext) throws {
        let plans = try child.foreignKeys.sorted { $0.name < $1.name }.filter {
            assigned == nil || !Set($0.columns).isDisjoint(with: assigned!)
        }.map { try Plan(child: child, foreign: $0, snapshot: snapshot) }
        for candidate in candidates {
            for plan in plans {
                switch try plan.childProbe(candidate.value, context: context) {
                case .exempt: continue
                case .missing: throw plan.violation()
                case .key(let key):
                    let selfReference = plan.parent.name == child.name
                    guard try plan.parentExists(key, snapshot: snapshot, candidates: selfReference ? candidates : [],
                        vacated: selfReference ? vacated : [], context: context) else { throw plan.violation() }
                }
            }
        }
    }
    static func plainColumns(_ index: CatalogIndex) -> [Int]? {
        var result: [Int] = []
        for key in index.keys { guard case .column(let column) = key else { return nil }; result.append(column) }
        return result
    }
    static func exact(_ value: SQLValue, to type: SQLType) throws -> SQLValue? {
        switch (value, type) {
        case (.integer(let number), .scalar(let target)) where target.integerWidth != nil:
            guard let integer = try? ScalarCasts.checkedInteger(number, type: target) else { return nil }
            return .integer(integer)
        case (.integer(let number), .scalar(.decimal)): return .decimal(SQLDecimal(number))
        case (.decimal(let number), .scalar(let target)) where target.integerWidth != nil:
            guard let integer = try? number.integerValue(), SQLDecimal(integer).compare(to: number) == 0,
                  let checked = try? ScalarCasts.checkedInteger(integer, type: target) else { return nil }
            return .integer(checked)
        case (.f32(let bits), .scalar(.f64)): return .f64(Double(Float(bitPattern: bits)).bitPattern)
        case (.f64(let bits), .scalar(.f32)):
            let number = Double(bitPattern: bits), narrow = Float(number)
            guard number.isNaN || Double(narrow) == number else { return nil }
            return .f32(narrow.bitPattern)
        default: return value
        }
    }
}
