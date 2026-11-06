internal extension CatalogImage {
    func entries() -> [CatalogEntry] {
        var output = composites.sorted { $0.name < $1.name }.map(CatalogEntry.composite)
        output += sequences.sorted { $0.name < $1.name }.map(CatalogEntry.sequence)
        output += collations.sorted { $0.name < $1.name }.map(CatalogEntry.collation)
        output += tables.sorted { $0.name < $1.name }.map { value in
            var table = value
            table.checks.sort { $0.name < $1.name }; table.indexes.sort { $0.name < $1.name }
            table.foreignKeys.sort { $0.name < $1.name }; table.exclusions.sort { $0.name < $1.name }
            return .table(table)
        }
        for stats in statistics.sorted(by: { $0.table == $1.table ? $0.column < $1.column : $0.table < $1.table }) {
            output.append(.statistic(CatalogStatisticEntry(table: stats.table, column: stats.column,
                body: .summary(stats, mcv: stats.mcv.count, histogram: stats.histogram.count, terms: stats.terms.count))))
            for (index, item) in stats.mcv.enumerated() { output.append(.statistic(.init(table: stats.table, column: stats.column, body: .mcv(index, item)))) }
            for (index, item) in stats.histogram.enumerated() { output.append(.statistic(.init(table: stats.table, column: stats.column, body: .histogram(index, item)))) }
            for (index, item) in stats.terms.enumerated() { output.append(.statistic(.init(table: stats.table, column: stats.column, body: .term(index, item)))) }
        }
        output += views.sorted { $0.name < $1.name }.map { value in
            var view = value
            view.dependencies.sort { $0.name == $1.name ? $0.kind < $1.kind : $0.name < $1.name }
            for index in view.dependencies.indices { view.dependencies[index].columns.sort() }
            return .view(view)
        }
        return output
    }

    static func collect(_ entries: [CatalogEntry], catalogID: UInt64,
                        collatedKey: KeyEncoding.CollatedKey? = nil,
                        collationVerdict: ((CatalogCollation) throws -> Bool)? = nil) throws -> CatalogImage {
        var image = CatalogImage(), rawStats: [CatalogStatisticEntry] = []
        for entry in entries {
            switch entry {
            case .composite(let value): image.composites.append(value)
            case .sequence(let value): image.sequences.append(value)
            case .collation(let value): image.collations.append(value)
            case .table(let value): image.tables.append(value)
            case .view(let value): image.views.append(value)
            case .statistic(let value): rawStats.append(value)
            }
        }
        try image.validateReferences(catalogID: catalogID)
        var skewedCollations: Set<SQLText> = []
        if let collationVerdict {
            for pin in image.collations.sorted(by: { $0.name < $1.name }) where try !collationVerdict(pin) { skewedCollations.insert(pin.name) }
        }
        let types = try image.resolvedTypes(catalogID: catalogID)
        let tables = Dictionary(uniqueKeysWithValues: image.tables.map { ($0.name, $0) })
        var position = 0, previous: (SQLText, Int)?
        while position < rawStats.count {
            let raw = rawStats[position]
            guard case .summary(var stats, let mcvs, let histograms, let terms) = raw.body,
                  let table = tables[raw.table], table.columns.indices.contains(raw.column) else {
                throw JedError.corruption("statistics entry has no valid table, column, or summary")
            }
            if let previous {
                guard previous.0 < raw.table || (previous.0 == raw.table && previous.1 < raw.column) else {
                    throw JedError.corruption("duplicate or unordered statistics summaries")
                }
            }
            stats.mcv = []; stats.histogram = []; stats.terms = []
            previous = (raw.table, raw.column); position += 1
            let type = try table.columns[raw.column].type.resolve(types)
            let skewed = type.collation.map { skewedCollations.contains($0) } ?? false
            let distribution: Bool
            switch type.type {
            case .range: distribution = true
            case .scalar(.json), .scalar(.jsonb), .scalar(.jsonpath): distribution = false
            case .scalar: distribution = true
            default: distribution = false
            }
            let termEligible: Bool
            switch type.type {
            case .scalar(.jsonb): termEligible = true
            case .array(.scalar(let scalar)): termEligible = scalar != .json && scalar != .jsonb && scalar != .jsonpath
            default: termEligible = false
            }
            guard stats.distributionEligible == distribution, stats.termEligible == termEligible else {
                throw JedError.corruption("statistics eligibility disagrees with column type")
            }
            func next() throws -> CatalogStatisticEntry.Body {
                guard position < rawStats.count, rawStats[position].table == raw.table,
                      rawStats[position].column == raw.column else { throw JedError.corruption("missing statistics item") }
                defer { position += 1 }; return rawStats[position].body
            }
            func key(_ bytes: [UInt8]) throws -> [UInt8] {
                let value = try ValueCodec.decode(bytes, type: type)
                guard bytes.first == 0, !value.isNull, try ValueCodec.encode(value, type: type) == bytes else {
                    throw JedError.corruption("noncanonical statistics value")
                }
                // Under skew the typed bytes remain canonical, but the loaded
                // bundle cannot prove the old comparison-key size or order.
                if skewed { return [] }
                let key: [UInt8]
                do { key = try KeyEncoding.encode(value, type: type, collatedKey: collatedKey) }
                catch let error as JedError where error.code == "XX002" { throw error }
                catch { throw JedError.corruption("statistics value has no valid key") }
                guard key.count <= 128 else { throw JedError.corruption("statistics key is too long") }
                return key
            }
            var lastKey: [UInt8]?, lastFrequency = UInt32.max, seen: Set<[UInt8]> = []
            for index in 0..<mcvs {
                guard case .mcv(let ordinal, let value) = try next(), ordinal == index,
                      value.frequency > 0, value.frequency <= stats.sampleNonnull else { throw JedError.corruption("invalid MCV item") }
                let current = try key(value.bytes)
                guard skewed || (seen.insert(current).inserted && (value.frequency < lastFrequency ||
                      (value.frequency == lastFrequency && (lastKey.map { $0.lexicographicallyPrecedes(current) } ?? true)))) else {
                    throw JedError.corruption("duplicate or unordered MCV item")
                }
                stats.mcv.append(value); lastKey = current; lastFrequency = value.frequency
            }
            lastKey = nil
            for index in 0..<histograms {
                guard case .histogram(let ordinal, let bytes) = try next(), ordinal == index else { throw JedError.corruption("invalid histogram item") }
                let current = try key(bytes)
                guard skewed || lastKey.map({ !$0.lexicographicallyPrecedes(current) && $0 != current }) != true else {
                    throw JedError.corruption("unordered histogram bounds")
                }
                stats.histogram.append(bytes); lastKey = current
            }
            var lastCount = Int64.max
            lastKey = nil; seen.removeAll(keepingCapacity: true)
            for index in 0..<terms {
                guard case .term(let ordinal, let value) = try next(), ordinal == index,
                      value.count > 0, value.count <= stats.analyzed - stats.nulls,
                      seen.insert(value.bytes).inserted,
                      value.count < lastCount || (value.count == lastCount && (lastKey.map { $0.lexicographicallyPrecedes(value.bytes) } ?? true)) else {
                    throw JedError.corruption("invalid, duplicate or unordered GIN statistics item")
                }
                stats.terms.append(value); lastKey = value.bytes; lastCount = value.count
            }
            image.statistics.append(stats)
        }
        return image
    }

    func validateReferences(catalogID: UInt64, borrowedTypes: [SQLText: CompositeType] = [:],
                            borrowedReferences: CatalogBorrowedReferences = .init()) throws {
        func unique(_ names: [SQLText]) throws {
            guard names.allSatisfy({ !$0.utf8.isEmpty }), Set(names).count == names.count else {
                throw JedError.corruption("empty or duplicate catalog name")
            }
        }
        let types = try resolvedTypes(catalogID: catalogID, borrowedTypes: borrowedTypes)
        try unique(tables.map(\.name) + views.map(\.name) + sequences.map(\.name))
        try unique(collations.map(\.name))
        guard collations.filter(\.isDefault).count <= 1 else { throw JedError.corruption("multiple default collations") }
        let tableMap = Dictionary(uniqueKeysWithValues: tables.map { ($0.name, $0) })
        let viewMap = Dictionary(uniqueKeysWithValues: views.map { ($0.name, $0) })
        let sequenceNames = Set(sequences.map(\.name)), collationNames = Set(collations.map(\.name))
        for table in tables {
            try unique(table.columns.map(\.name)); try unique(table.checks.map(\.name))
            try unique(table.indexes.map(\.name)); try unique(table.foreignKeys.map(\.name)); try unique(table.exclusions.map(\.name))
            for column in table.columns {
                _ = try column.type.resolve(types)
                if let collation = column.type.collation, collation.utf8 != [67], !collationNames.contains(collation) {
                    throw JedError.corruption("column references missing collation")
                }
            }
            for foreign in table.foreignKeys {
                guard let parent = tableMap[foreign.referencedTable], !foreign.columns.isEmpty,
                      foreign.columns.count == foreign.referencedColumns.count,
                      foreign.columns.allSatisfy({ table.columns.indices.contains($0) }),
                      foreign.referencedColumns.allSatisfy({ parent.columns.indices.contains($0) }) else {
                    throw JedError.corruption("foreign key references missing table or column")
                }
                for (local, remote) in zip(foreign.columns, foreign.referencedColumns) {
                    let left = try table.columns[local].type.resolve(types).type
                    let right = try parent.columns[remote].type.resolve(types).type
                    var comparable = left == right
                    if case .scalar(let l) = left, case .scalar(let r) = right {
                        comparable = comparable || (l.integerWidth != nil && (r.integerWidth != nil || r == .decimal)) ||
                            ([ScalarType.f32, .f64].contains(l) && [ScalarType.f32, .f64].contains(r))
                    }
                    guard comparable else { throw JedError.corruption("foreign key pairs incomparable types") }
                }
                let referenced = Set(foreign.referencedColumns)
                let uniqueKey = !parent.primaryKey.isEmpty && Set(parent.primaryKey) == referenced || parent.indexes.contains { index in
                    guard index.unique, index.predicate == nil else { return false }
                    var ordinals: [Int] = []
                    for key in index.keys { guard case .column(let ordinal) = key else { return false }; ordinals.append(ordinal) }
                    return Set(ordinals) == referenced
                }
                guard uniqueKey else { throw JedError.corruption("foreign key does not reference a unique key") }
            }
            for exclusion in table.exclusions {
                guard let index = table.indexes.first(where: { $0.name == exclusion.index }), !index.unique,
                      index.kind == 2 || (index.kind == 0 && exclusion.elements.allSatisfy({ $0.strategy == 1 })) else {
                    throw JedError.corruption("exclusion constraint references invalid index")
                }
            }
        }
        for sequence in sequences {
            if let owner = sequence.owner {
                guard let table = tableMap[owner.table], table.columns.indices.contains(owner.column) else {
                    throw JedError.corruption("sequence owner references missing table or column")
                }
            }
        }
        for view in views {
            try unique(view.columns.map(\.name))
            var dependencies: Set<[UInt8]> = []
            for dep in view.dependencies {
                guard dependencies.insert([dep.kind] + dep.name.utf8).inserted else { throw JedError.corruption("duplicate view dependency") }
                try unique(dep.columns)
                switch dep.kind {
                case 0:
                    let columns = tableMap[dep.name].map { Set($0.columns.map(\.name)) } ?? borrowedReferences.tables[dep.name]
                    guard let columns, dep.columns.allSatisfy(columns.contains) else {
                        throw JedError.corruption("view references missing table or column")
                    }
                case 1: guard viewMap[dep.name] != nil || borrowedReferences.views.contains(dep.name) else { throw JedError.corruption("view references missing view") }
                case 2: guard sequenceNames.contains(dep.name) || borrowedReferences.sequences.contains(dep.name) else { throw JedError.corruption("view references missing sequence") }
                case 3: guard types[dep.name] != nil else { throw JedError.corruption("view references missing composite type") }
                default: throw JedError.corruption("invalid view dependency kind")
                }
            }
        }
        var active: Set<SQLText> = [], complete: Set<SQLText> = []
        func visit(_ name: SQLText) throws {
            if complete.contains(name) { return }
            guard active.insert(name).inserted, active.count <= 256 else { throw JedError.corruption("cyclic or excessively nested view graph") }
            for dep in viewMap[name]!.dependencies where dep.kind == 1 && viewMap[dep.name] != nil { try visit(dep.name) }
            active.remove(name); complete.insert(name)
        }
        for name in viewMap.keys.sorted() { try visit(name) }
    }
}
