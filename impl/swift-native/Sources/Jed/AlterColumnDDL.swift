/// Column changes edit the action-visible catalog first. Dense ordinals remain
/// meaningful after every action, including incoming FKs and sequence owners.
internal enum AlterColumnDDL {
    static func add(_ syntax: SQLColumnDefinition, table: inout CatalogTable, image: inout CatalogImage,
                    source: SQLText, snapshot: DomainSnapshot, settings: SessionSettings,
                    context: ExpressionContext, added: inout Set<SQLText>) throws -> CatalogColumn {
        let ordinal = table.columns.count, serial = SequenceDefinition.serialType(syntax.type)
        var descriptor = try serial.map { TypeDescriptor(.scalar($0)) } ?? TypeRules.resolve(syntax.type, composites: snapshot.types)
        if ExpressionCollation.collatable(descriptor.type) { descriptor.collation = image.collations.first(where: \.isDefault)?.name }
        let identities = syntax.constraints.compactMap { constraint -> SQLIdentity? in
            if case .identity(let identity) = constraint.kind { identity } else { nil }
        }
        let hasDefault = syntax.constraints.contains { if case .default = $0.kind { true } else { false } }
        guard identities.count <= 1, !hasDefault || serial == nil && identities.isEmpty, serial == nil || identities.isEmpty else {
            throw JedError("42601", "conflicting default or identity specifications")
        }
        var column = CatalogColumn(name: syntax.name, type: try CatalogSQL.typeUse(descriptor))
        let resolver = try CatalogDDL.resolver(snapshot: snapshot, settings: settings, context: context)
        for constraint in syntax.constraints {
            switch constraint.kind {
            case .notNull, .primaryKey: column.notNull = true
            case .collate(let name):
                guard ExpressionCollation.collatable(descriptor.type) else { throw JedError("42804", "collations are not supported by this type") }
                _ = try settings.environment.unicode.resolve(name); descriptor.collation = name; column.type.collation = name
            case .default(let value):
                column.defaultValue = try CatalogDDL.defaultValue(value, target: descriptor, source: source, resolver: resolver, context: context)
            default: break
            }
        }
        if serial != nil || !identities.isEmpty {
            guard case .scalar(let scalar) = descriptor.type, scalar.integerWidth != nil else { throw JedError("22023", "identity column type must be smallint, integer, or bigint") }
            let reserved = Set(syntax.constraints.compactMap { constraint -> SQLText? in
                if case .unique = constraint.kind { return constraint.name }; return nil
            })
            let sequence = try SequenceDefinition.owned(table: table.name, column: syntax.name, ordinal: ordinal,
                type: scalar, options: identities.first?.options ?? [], taken: CatalogDomain.relations(image, settings: settings).union(reserved), composites: snapshot.types)
            image.sequences.append(sequence); column.notNull = true; column.identity = identities.first?.always
            column.defaultValue = .expression(SequenceName.defaultExpression(sequence.name))
        }
        table.columns.append(column)
        let primaries = syntax.constraints.filter { if case .primaryKey = $0.kind { true } else { false } }
        guard primaries.count <= 1, primaries.isEmpty || table.primaryKey.isEmpty else { throw JedError("42P16", "multiple primary keys are not allowed") }
        if !primaries.isEmpty { try KeyEncoding.validateType(descriptor.type); table.primaryKey = [ordinal] }
        for constraint in syntax.constraints {
            var normalized = constraint
            switch constraint.kind {
            case .primaryKey: break
            case .unique:
                if table.primaryKey == [ordinal] { continue }
                normalized.kind = .unique([column.name])
                added.insert(try CatalogDDL.addAlterConstraint(normalized, table: &table, image: image,
                    source: source, snapshot: snapshot, settings: settings, context: context))
            case .foreignKey(_, let reference):
                normalized.kind = .foreignKey(columns: [column.name], reference)
                added.insert(try CatalogDDL.addAlterConstraint(normalized, table: &table, image: image,
                    source: source, snapshot: snapshot, settings: settings, context: context))
            case .check:
                added.insert(try CatalogDDL.addAlterConstraint(normalized, table: &table, image: image,
                    source: source, snapshot: snapshot, settings: settings, context: context))
            default: break
            }
            if let position = image.tables.firstIndex(where: { $0.name == table.name }) { image.tables[position] = table }
        }
        if let name = descriptor.collation, !image.collations.contains(where: { $0.name == name }),
           let collation = try settings.environment.unicode.resolve(name) {
            image.collations.append(CatalogCollation(name: name, unicode: collation.unicodeVersion,
                cldr: collation.cldrVersion, description: collation.description))
        }
        return column
    }
    static func uses(_ expression: CatalogExpression, column: SQLText, table: SQLText) throws -> Bool {
        guard let root = expression.parsed else { throw JedError("XX003", "stored expression cannot be parsed") }
        var work = [root], found = false
        while let value = work.popLast() {
            if case .column(let qualifier, let name) = value.kind, name == column,
               qualifier == nil || qualifier == table { found = true }
            work += CatalogExpressionPolicy.children(value)
        }
        return found
    }
    static func drop(_ ordinal: Int, table: inout CatalogTable, image: inout CatalogImage,
                     cascade: Bool, removedIndexes: inout Set<SQLText>, added: inout Set<SQLText>) throws {
        guard !table.primaryKey.contains(ordinal) else { throw JedError("0A000", "dropping a primary-key column is not supported yet") }
        let name = table.columns[ordinal].name
        // Every CHECK and index definition must be parsable, including ones
        // that turn out not to depend on this column. Defaults are not read.
        var checks: Set<SQLText> = [], indexes: Set<SQLText> = [], predicateIndexes: Set<SQLText> = []
        for check in table.checks where try uses(check.expression, column: name, table: table.name) { checks.insert(check.name) }
        for index in table.indexes {
            var keyUses = false
            for key in index.keys {
                switch key {
                case .column(let column): keyUses = keyUses || column == ordinal
                case .expression(let expression):
                    let hit = try uses(expression, column: name, table: table.name); keyUses = keyUses || hit
                }
            }
            let predicateUses = try index.predicate.map { try uses($0, column: name, table: table.name) } ?? false
            if keyUses || predicateUses { indexes.insert(index.name) }
            if predicateUses && !keyUses { predicateIndexes.insert(index.name) }
        }
        var views = Set(image.views.filter { view in
            view.dependencies.contains { $0.kind == 0 && $0.name == table.name && $0.columns.contains(name) }
        }.map(\.name))
        let sequences = Set(image.sequences.filter { $0.owner?.table == table.name && $0.owner?.column == ordinal }.map(\.name))
        for view in image.views where view.dependencies.contains(where: { $0.kind == 2 && sequences.contains($0.name) }) { views.insert(view.name) }
        var external = !views.isEmpty || table.exclusions.contains { predicateIndexes.contains($0.index) }
        for child in image.tables {
            for foreign in child.foreignKeys where foreign.referencedTable == table.name && foreign.referencedColumns.contains(ordinal) {
                if child.name != table.name || !foreign.columns.contains(ordinal) { external = true }
            }
        }
        guard cascade || !external else { throw JedError("2BP01", "cannot drop column \(name) because other objects depend on it") }
        var changed = true
        while changed {
            changed = false
            for view in image.views where !views.contains(view.name) {
                if view.dependencies.contains(where: { $0.kind == 1 && views.contains($0.name) }) { views.insert(view.name); changed = true }
            }
        }
        image.views.removeAll { views.contains($0.name) }
        try ViewRewrite.droppingColumn(in: &image, table: table.name, ordinal: ordinal)
        table.checks.removeAll { checks.contains($0.name) }
        table.indexes.removeAll { indexes.contains($0.name) }
        table.exclusions.removeAll { indexes.contains($0.index) }
        removedIndexes.formUnion(indexes); added.subtract(checks); added.subtract(indexes)
        table.foreignKeys.removeAll {
            let removed = $0.columns.contains(ordinal) || ($0.referencedTable == table.name && $0.referencedColumns.contains(ordinal))
            if removed { added.remove($0.name) }
            return removed
        }
        func compact(_ value: Int) -> Int { value > ordinal ? value - 1 : value }
        table.primaryKey = table.primaryKey.map(compact)
        for index in table.indexes.indices {
            table.indexes[index].keys = table.indexes[index].keys.map { key in
                if case .column(let column) = key { return .column(compact(column)) }; return key
            }
        }
        for foreign in table.foreignKeys.indices {
            table.foreignKeys[foreign].columns = table.foreignKeys[foreign].columns.map(compact)
            if table.foreignKeys[foreign].referencedTable == table.name {
                table.foreignKeys[foreign].referencedColumns = table.foreignKeys[foreign].referencedColumns.map(compact)
            }
        }
        for exclusion in table.exclusions.indices {
            for element in table.exclusions[exclusion].elements.indices {
                table.exclusions[exclusion].elements[element].column = compact(table.exclusions[exclusion].elements[element].column)
            }
        }
        for child in image.tables.indices where image.tables[child].name != table.name {
            image.tables[child].foreignKeys.removeAll { $0.referencedTable == table.name && $0.referencedColumns.contains(ordinal) }
            for foreign in image.tables[child].foreignKeys.indices where image.tables[child].foreignKeys[foreign].referencedTable == table.name {
                image.tables[child].foreignKeys[foreign].referencedColumns = image.tables[child].foreignKeys[foreign].referencedColumns.map(compact)
            }
        }
        image.sequences.removeAll { sequences.contains($0.name) }
        for sequence in image.sequences.indices where image.sequences[sequence].owner?.table == table.name {
            let oldOrdinal = image.sequences[sequence].owner!.column
            image.sequences[sequence].owner?.column = compact(oldOrdinal)
        }
        table.columns.remove(at: ordinal)
    }
}
