/// Catalog mutations use a private working snapshot. Publication follows every
/// validation, including dependent-object checks across a multi-object DROP.
internal enum CatalogDDL {
    static func execute(_ statement: SQLStatement, source: SQLText, snapshot: inout DomainSnapshot,
                        settings: SessionSettings, context: ExpressionContext, meter: CostMeter) throws -> CommandTag? {
        do {
            let command: String
            switch statement {
            case .createType: command = "CREATE TYPE"
            case .createTable: command = "CREATE TABLE"
            case .createIndex: command = "CREATE INDEX"
            case .createSequence: command = "CREATE SEQUENCE"
            case .createView: command = "CREATE VIEW"
            case .alterView: command = "ALTER VIEW"
            case .alterSequence: command = "ALTER SEQUENCE"
            case .alterTable: command = "ALTER TABLE"
            case .drop(let drop) where ["type", "table", "index", "sequence", "view"].contains(drop.object): command = "DROP " + drop.object.uppercased()
            default: return nil
            }
            try CatalogDomain.requireDDL(settings, statement: statement)
            var work = snapshot
            if settings.targetDomain == DomainNames.temporary, let main = settings.domains?.snapshots[DomainNames.main] { try work.borrow(main) }
            switch statement {
            case .createType(let definition): try createType(definition, snapshot: &work, settings: settings)
            case .createTable(let definition):
                try createTable(definition, source: source, snapshot: &work, settings: settings, context: context)
            case .createIndex(let definition):
                try createIndex(definition, source: source, snapshot: &work, settings: settings, context: context)
            case .createSequence(let definition): try createSequence(definition, snapshot: &work, settings: settings)
            case .createView(let definition): try createView(definition, source: source, snapshot: &work, settings: settings, context: context)
            case .alterView(let definition): try alterView(definition, source: source, snapshot: &work, settings: settings, context: context)
            case .alterSequence(let definition): try alterSequence(definition, snapshot: &work, settings: settings)
            case .alterTable(let definition): try alterTable(definition, source: source, snapshot: &work, settings: settings, context: context)
            case .drop(let definition) where definition.object == "index": try dropIndexes(definition, snapshot: &work, settings: settings)
            case .drop(let definition) where definition.object == "sequence": try dropSequences(definition, snapshot: &work, settings: settings)
            case .drop(let definition) where definition.object == "view": try dropViews(definition, snapshot: &work, settings: settings)
            case .drop(let definition): try drop(definition, snapshot: &work, settings: settings)
            default: break
            }
            snapshot = work
            return CommandTag(command: command, rowsAffected: 0, cost: meter.accrued)
        } catch let error as JedError { throw error.inDatabase(settings.targetDomain) }
    }

    static func nameInMain(_ name: SQLName) throws -> SQLText {
        guard name.database == nil || name.database?.utf8 == [109, 97, 105, 110] else {
            throw JedError("42P01", "database \"\(name.database!)\" is not attached")
        }
        return name.name
    }
    private static func compositeDepth(_ type: SQLType, memo: inout [UInt32: Int]) -> Int {
        switch type {
        case .composite(let composite):
            if let depth = memo[composite.id] { return depth }
            let depth = 1 + (composite.fields.map { compositeDepth($0.descriptor.type, memo: &memo) }.max() ?? 0)
            memo[composite.id] = depth; return depth
        case .array(let element): return compositeDepth(element, memo: &memo)
        default: return 0
        }
    }
    private static func createType(_ definition: SQLCreateType, snapshot: inout DomainSnapshot, settings: SessionSettings) throws {
        guard settings.targetDomain == DomainNames.main else { throw JedError("0A000", "composite types must be created in the main database") }
        let name = try CatalogDomain.name(definition.name, settings: settings)
        try CatalogSQL.reserved(name, kind: "type")
        guard snapshot.types[name] == nil, TypeRules.scalar(named: name) == nil,
              !GeneratedSpec.ranges.contains(where: { $0.id == name.description || $0.aliases.contains(name.description) }) else {
            throw JedError("42710", "type \(name) already exists")
        }
        var names: Set<SQLText> = [], fields: [CatalogColumn] = [], depths: [UInt32: Int] = [:]
        for field in definition.fields {
            guard names.insert(field.name).inserted else { throw JedError("42701", "column \(field.name) specified more than once") }
            let type = try TypeRules.resolve(field.type, composites: snapshot.types)
            guard compositeDepth(type.type, memo: &depths) < 32 else { throw JedError("54001", "composite type nesting exceeds maximum depth of 32") }
            var column = CatalogColumn(name: field.name, type: try CatalogSQL.typeUse(type))
            for constraint in field.constraints {
                if case .notNull = constraint.kind { column.notNull = true }
                else { throw JedError("0A000", "constraint is not supported on a composite field") }
            }
            fields.append(column)
        }
        guard !fields.isEmpty else { throw JedError("42601", "a composite type requires at least one field") }
        var image = snapshot.catalog
        image.composites.append(CatalogComposite(name: name, fields: fields))
        try snapshot.updateCatalog(image, schemaChanged: true)
    }

    private struct Constraint {
        let syntax: SQLConstraint
        let column: Int?
    }
    private static func createTable(_ definition: SQLCreateTable, source: SQLText, snapshot: inout DomainSnapshot,
                                    settings: SessionSettings, context: ExpressionContext) throws {
        let name = try CatalogDomain.name(definition.name, settings: settings)
        try CatalogSQL.reserved(name, kind: "table")
        let relations = CatalogDomain.relations(snapshot.catalog, settings: settings)
        if relations.contains(name) {
            if definition.ifNotExists { return }
            throw JedError("42P07", "relation \(name) already exists")
        }
        guard definition.onCommit == nil || settings.targetDomain == DomainNames.temporary else { throw JedError("42P16", "ON COMMIT can only be used on temporary tables") }
        guard definition.query == nil else { throw JedError("0A000", "CREATE TABLE AS execution is not yet connected") }
        let root = PageSealer.nullReference(snapshot.pool.codec)
        var table = CatalogTable(name: name, columns: [], root: root)
        var sequences: [CatalogSequence] = [], sequenceNames = relations.union([name])
        var constraints: [Constraint] = [], names: Set<SQLText> = [], columnPrimary: Set<Int> = []
        let emptyResolver = try resolver(snapshot: snapshot, settings: settings, context: context)
        for (index, column) in definition.columns.enumerated() {
            guard names.insert(column.name).inserted else { throw JedError("42701", "column \(column.name) specified more than once") }
            let serial = SequenceDefinition.serialType(column.type)
            var descriptor = try serial.map { TypeDescriptor(.scalar($0)) } ?? TypeRules.resolve(column.type, composites: snapshot.types)
            let identities = column.constraints.compactMap { constraint -> SQLIdentity? in
                if case .identity(let identity) = constraint.kind { identity } else { nil }
            }
            let hasDefault = column.constraints.contains { if case .default = $0.kind { true } else { false } }
            guard identities.count <= 1 else { throw JedError("42601", "multiple identity specifications for column \(column.name)") }
            guard !hasDefault || serial == nil && identities.isEmpty else { throw JedError("42601", "multiple default values or both default and identity specified for column \(column.name)") }
            guard serial == nil || identities.isEmpty else { throw JedError("42601", "both default and identity specified for column \(column.name)") }
            if ExpressionCollation.collatable(descriptor.type) {
                descriptor.collation = (settings.domains?.snapshots[DomainNames.main]?.catalog ?? snapshot.catalog).collations.first(where: \.isDefault)?.name
            }
            var output = CatalogColumn(name: column.name, type: try CatalogSQL.typeUse(descriptor))
            for constraint in column.constraints {
                switch constraint.kind {
                case .notNull: output.notNull = true
                case .primaryKey:
                    if columnPrimary.insert(index).inserted { constraints.append(Constraint(syntax: constraint, column: index)) }
                case .collate(let collation):
                    guard ExpressionCollation.collatable(descriptor.type) else { throw JedError("42804", "collations are not supported by this type") }
                    _ = try settings.environment.unicode.resolve(collation)
                    descriptor.collation = collation; output.type.collation = collation
                case .default(let expression):
                    output.defaultValue = try defaultValue(expression, target: descriptor, source: source,
                                                           resolver: emptyResolver, context: context)
                case .identity: break // synthesized after the complete column definition
                default: constraints.append(Constraint(syntax: constraint, column: index))
                }
            }
            if serial != nil || !identities.isEmpty {
                guard case .scalar(let scalar) = descriptor.type, scalar.integerWidth != nil else {
                    throw JedError("22023", "identity column type must be smallint, integer, or bigint")
                }
                let identity = identities.first
                let sequence = try SequenceDefinition.owned(table: name, column: column.name, ordinal: index,
                    type: scalar, options: identity?.options ?? [], taken: sequenceNames, composites: snapshot.types)
                sequences.append(sequence); sequenceNames.insert(sequence.name)
                output.notNull = true; output.identity = identity?.always
                output.defaultValue = .expression(SequenceName.defaultExpression(sequence.name))
            }
            table.columns.append(output)
        }
        constraints += definition.constraints.map { Constraint(syntax: $0, column: nil) }
        constraints.sort { $0.syntax.span.byteStart < $1.syntax.span.byteStart }
        var primarySeen = false
        for item in constraints {
            guard case .primaryKey(let columns) = item.syntax.kind else { continue }
            guard !primarySeen else { throw JedError("42P16", "multiple primary keys for table \(name) are not allowed") }
            primarySeen = true
            table.primaryKey = try keyColumns(columns, own: item.column, table: table, kind: "primary key", keyable: true, types: snapshot.types)
            for column in table.primaryKey { table.columns[column].notNull = true }
        }
        struct Unique { var columns: [Int]; var name: SQLText? }
        var uniques: [Unique] = []
        for item in constraints {
            guard case .unique(let columns) = item.syntax.kind else { continue }
            let keys = try keyColumns(columns, own: item.column, table: table, kind: "unique", keyable: true, types: snapshot.types)
            if keys == table.primaryKey { continue }
            if let previous = uniques.firstIndex(where: { $0.columns == keys }) {
                if uniques[previous].name == nil { uniques[previous].name = item.syntax.name }
            } else { uniques.append(Unique(columns: keys, name: item.syntax.name)) }
        }
        let checkResolver = try resolver(snapshot: snapshot, settings: settings, context: context, table: table)
        var checks: [(SQLConstraint, CatalogExpression, [Int], [CatalogHostDependency])] = []
        for item in constraints {
            guard case .check(let expression) = item.syntax.kind else { continue }
            try CatalogExpressionPolicy.validate(expression, isDefault: false)
            checkResolver.referencedColumns = []
            let resolved = try checkResolver.resolve(expression, expected: TypeDescriptor(.scalar(.boolean)))
            guard resolved.type == nil || resolved.type?.type == .scalar(.boolean) else { throw JedError("42804", "argument of CHECK must be boolean").at(expression.expressionLocation) }
            let references = checkResolver.referencedColumns.sorted()
            let hosts = try StoredExpressionDependencies.hosts([resolved], environment: settings.environment)
            checks.append((item.syntax, try CatalogSQL.expression(expression, source: source), references, hosts))
        }
        var schemaConstraints = CatalogDomain.constraints(snapshot.catalog, settings: settings)
        var localConstraints: Set<SQLText> = [], usedRelations = sequenceNames
        for (syntax, expression, referenced, hosts) in checks {
            let checkName = syntax.name ?? CatalogSQL.objectName(name,
                columns: referenced.count == 1 ? [table.columns[referenced[0]].name] : [], label: "check", taken: schemaConstraints)
            try addConstraintName(checkName, table: name, local: &localConstraints, schema: &schemaConstraints)
            table.checks.append(CatalogCheck(name: checkName, expression: expression, hosts: hosts))
        }
        for unique in uniques {
            let indexName: SQLText
            if let explicit = unique.name {
                try CatalogSQL.reserved(explicit, kind: "constraint")
                guard !usedRelations.contains(explicit) else { throw JedError("42P07", "relation \(explicit) already exists") }
                indexName = explicit
            } else {
                indexName = CatalogSQL.objectName(name, columns: unique.columns.map { table.columns[$0].name },
                                                   label: "key", taken: schemaConstraints.union(usedRelations))
            }
            try addConstraintName(indexName, table: name, local: &localConstraints, schema: &schemaConstraints)
            usedRelations.insert(indexName)
            table.indexes.append(CatalogIndex(name: indexName, keys: unique.columns.map(CatalogIndexKey.column),
                                             unique: true, isConstraint: true, root: root))
        }
        for item in constraints {
            switch item.syntax.kind {
            case .foreignKey(let columns, let reference):
                let local = try keyColumns(columns, own: item.column, table: table, kind: "foreign key", keyable: false, types: snapshot.types)
                let parent = try CatalogDomain.foreignParent(reference.table, child: table, image: snapshot.catalog,
                    snapshot: snapshot, settings: settings)
                let parentName = parent.name
                let remote: [Int]
                if reference.columns.isEmpty {
                    guard !parent.primaryKey.isEmpty else { throw JedError("42704", "there is no primary key for referenced table \(parentName)") }
                    remote = parent.primaryKey
                } else { remote = try keyColumns(reference.columns, own: nil, table: parent, kind: "foreign key", keyable: false, types: snapshot.types) }
                guard local.count == remote.count else { throw JedError("42830", "number of referencing and referenced columns for foreign key disagree") }
                guard uniqueCover(remote, table: parent) else { throw JedError("42830", "there is no unique constraint matching given keys for referenced table \(parentName)") }
                for (a, b) in zip(local, remote) {
                    let left = try table.columns[a].type.resolve(snapshot.types).type, right = try parent.columns[b].type.resolve(snapshot.types).type
                    guard foreignKeyPair(left, right) else { throw JedError("42804", "foreign key constraint cannot be implemented: key columns \(table.columns[a].name) and \(parent.columns[b].name) are of incompatible types") }
                }
                let foreignName = item.syntax.name ?? CatalogSQL.objectName(name, columns: local.map { table.columns[$0].name }, label: "fkey", taken: schemaConstraints)
                try addConstraintName(foreignName, table: name, local: &localConstraints, schema: &schemaConstraints)
                let actions = ["no action": UInt8(0), "restrict": 1, "cascade": 2, "set null": 3, "set default": 4]
                guard let onDelete = actions[reference.onDelete], let onUpdate = actions[reference.onUpdate] else { throw JedError("XX000", "unresolved foreign-key action") }
                table.foreignKeys.append(CatalogForeignKey(name: foreignName, columns: local, referencedTable: parentName,
                    referencedColumns: remote, onDelete: onDelete, onUpdate: onUpdate, matchFull: reference.match == "full"))
            default: break
            }
        }
        for item in constraints {
            guard case .exclude(let method, let elements, let predicate) = item.syntax.kind else { continue }
            let definition = try exclusionDefinition(method: method, elements: elements, predicate: predicate,
                table: table, source: source, snapshot: snapshot, settings: settings, context: context)
            let chosen: SQLText
            if let explicit = item.syntax.name {
                try CatalogSQL.reserved(explicit, kind: "constraint")
                guard !usedRelations.contains(explicit) else { throw JedError("42P07", "relation \(explicit) already exists") }
                chosen = explicit
            } else {
                chosen = CatalogSQL.objectName(name, columns: definition.columns.map { table.columns[$0].name },
                    label: "excl", taken: schemaConstraints.union(usedRelations))
            }
            try addConstraintName(chosen, table: name, local: &localConstraints, schema: &schemaConstraints)
            usedRelations.insert(chosen)
            table.indexes.append(definition.index(chosen, root: root))
            table.exclusions.append(CatalogExclusion(name: chosen, index: chosen, elements: definition.elements))
        }
        try CatalogDomain.validateTable(table, sequences: sequences, settings: settings, types: snapshot.types)
        table.checks.sort { $0.name < $1.name }; table.indexes.sort { $0.name < $1.name }; table.foreignKeys.sort { $0.name < $1.name }
        table.exclusions.sort { $0.name < $1.name }
        var image = snapshot.catalog; image.tables.append(table); image.sequences += sequences
        for column in table.columns {
            if let name = column.type.collation, !image.collations.contains(where: { $0.name == name }),
               let collation = try settings.environment.unicode.resolve(name) {
                image.collations.append(CatalogCollation(name: name, unicode: collation.unicodeVersion,
                    cldr: collation.cldrVersion, description: collation.description))
            }
        }
        try snapshot.updateCatalog(image, schemaChanged: true)
        snapshot.setTableStore(name, snapshot.emptyStore(types: try table.columns.map { try $0.type.resolve(snapshot.types) }, primaryKey: table.primaryKey))
        for index in table.indexes {
            let store = snapshot.emptyStore(types: [])
            snapshot.setIndexStore(index.name, store)
            if index.kind == 2 {
                let plan = try StoredIndexPlan.resolve(table: table, index: index, snapshot: snapshot, settings: settings, context: context)
                try plan.rebuild(store, snapshot: &snapshot)
            }
        }
    }

    static func resolver(snapshot: DomainSnapshot, settings: SessionSettings, context: ExpressionContext,
                         table: CatalogTable? = nil) throws -> ExpressionResolver {
        let scope: [ExpressionColumn] = try table.map { table in
            let types = try snapshot.tableTypes(table)
            return table.columns.enumerated().map { index, column in
                ExpressionColumn(name: column.name, qualifier: table.name, type: types[index], index: index)
            }
        } ?? []
        var scalar = context.scalar; scalar.meter = nil
        let result = ExpressionResolver(columns: scope, composites: snapshot.types, scalar: scalar)
        result.hostFunctions = settings.environment.signatures; result.unicode = settings.environment.unicode
        result.checkFunction = { try settings.privileges.require(.execute, on: $0, function: true) }
        return result
    }
    static func defaultValue(_ expression: SQLExpression, target: TypeDescriptor, source: SQLText,
                             resolver: ExpressionResolver, context: ExpressionContext) throws -> CatalogDefault? {
        try CatalogExpressionPolicy.validate(expression, isDefault: true)
        var literal = expression
        while case .parenthesized(let inner) = literal.kind { literal = inner }
        if case .null = literal.kind { return nil }
        if case .array = target.type { throw JedError("0A000", "array columns support only DEFAULT NULL") }
        if case .range = target.type { throw JedError("0A000", "range columns support only DEFAULT NULL") }
        let resolved = try AssignmentValues.resolve(expression, to: target, resolver: resolver, assignmentPosition: 0)
        if case .composite = target.type { return .expression(try CatalogSQL.expression(expression, source: source)) }
        switch literal.kind {
        case .string(let text):
            if case .dateInput = resolved.node {
                return .expression(try CatalogSQL.expression(expression, source: source))
            }
            return .constant(try ScalarCasts.input(text, to: target, explicit: false, context: resolver.scalar))
        case .number, .boolean:
            return .constant(try AssignmentValues.store(context.evaluate(resolved), expression: resolved, to: target, context: context))
        default: return .expression(try CatalogSQL.expression(expression, source: source))
        }
    }
    static func keyColumns(_ names: [SQLText], own: Int?, table: CatalogTable, kind: String,
                           keyable: Bool, types: [SQLText: CompositeType]) throws -> [Int] {
        let columns = names.isEmpty ? own.map { [table.columns[$0].name] } ?? [] : names
        var result: [Int] = []
        for name in columns {
            guard let index = table.columns.firstIndex(where: { $0.name == name }) else { throw JedError("42703", "column \(name) named in key does not exist") }
            guard !result.contains(index) else { throw JedError("42701", "column \(name) appears twice in \(kind) constraint") }
            if keyable { try KeyEncoding.validateType(table.columns[index].type.resolve(types).type) }
            result.append(index)
        }
        return result
    }
    static func uniqueCover(_ columns: [Int], table: CatalogTable) -> Bool {
        let wanted = Set(columns)
        if !columns.isEmpty, wanted == Set(table.primaryKey) { return true }
        return table.indexes.contains { index in
            guard index.unique, index.predicate == nil, index.kind == 0 else { return false }
            let keys = index.keys.compactMap { if case .column(let column) = $0 { column } else { nil } }
            return keys.count == index.keys.count && Set(keys) == wanted
        }
    }
    static func foreignKeyPair(_ left: SQLType, _ right: SQLType) -> Bool {
        if left == right { return true }
        guard case .scalar(let a) = left, case .scalar(let b) = right else { return false }
        return a.integerWidth != nil && (b.integerWidth != nil || b == .decimal)
            || (a == .f32 && b == .f64) || (a == .f64 && b == .f32)
    }
    private static func addConstraintName(_ name: SQLText, table: SQLText, local: inout Set<SQLText>, schema: inout Set<SQLText>) throws {
        guard local.insert(name).inserted else { throw JedError("42710", "constraint \(name) for relation \(table) already exists") }
        schema.insert(name)
    }
    private static func drop(_ definition: SQLDrop, snapshot: inout DomainSnapshot, settings: SessionSettings) throws {
        var targets: Set<SQLText> = []
        let relations = CatalogSQL.relationNames(snapshot.catalog)
        for input in definition.names {
            if definition.object == "table" { try SystemRelationPlan.rejectTarget(input.name, dropping: true) }
            let name = try CatalogDomain.name(input, settings: settings)
            let exists = definition.object == "type" ? snapshot.types[name] != nil : snapshot.catalog.tables.contains { $0.name == name }
            if exists { targets.insert(name) }
            else if definition.object == "table" && relations.contains(name) { throw JedError("42809", "\(name) is not a table") }
            else if !definition.ifExists { throw JedError(definition.object == "type" ? "42704" : "42P01", "\(definition.object) does not exist: \(name)") }
        }
        if targets.isEmpty { return }
        var image = snapshot.catalog
        func references(_ type: CatalogType) -> Bool {
            switch type { case .composite(let name): targets.contains(name); case .array(let element): references(element); default: false }
        }
        if definition.object == "type" {
            let used = image.composites.contains { !targets.contains($0.name) && $0.fields.contains { references($0.type.type) } }
                || image.tables.contains { $0.columns.contains { references($0.type.type) } }
            let temporaryUse = settings.domains?.snapshots[DomainNames.temporary]?.catalog.tables.contains {
                $0.columns.contains { references($0.type.type) }
            } ?? false
            if used || temporaryUse { throw JedError(definition.cascade ? "0A000" : "2BP01", "cannot drop type because other objects depend on it") }
        } else {
            let dependent = image.tables.contains { !targets.contains($0.name) && $0.foreignKeys.contains { targets.contains($0.referencedTable) } }
            if dependent && !definition.cascade { throw JedError("2BP01", "cannot drop table because other objects depend on it").hinted(DependencyDiagnostics.cascade) }
        }
        let dependencyKind: UInt8 = definition.object == "table" ? 0 : 3
        var views: Set<SQLText> = []
        var changed = true
        while changed {
            changed = false
            for view in image.views where !views.contains(view.name) {
                if view.dependencies.contains(where: { $0.kind == dependencyKind && targets.contains($0.name) || $0.kind == 1 && views.contains($0.name) }) {
                    guard definition.cascade else { throw JedError("2BP01", "cannot drop \(definition.object) because other objects depend on it").hinted(DependencyDiagnostics.cascade) }
                    views.insert(view.name); changed = true
                }
            }
        }
        image.views.removeAll { views.contains($0.name) }
        if definition.object == "type" { image.composites.removeAll { targets.contains($0.name) } }
        else {
            for table in image.tables where targets.contains(table.name) {
                snapshot.setTableStore(table.name, nil)
                for index in table.indexes { snapshot.setIndexStore(index.name, nil); snapshot.setGistTree(index.name, nil) }
            }
            image.tables.removeAll { targets.contains($0.name) }
            for index in image.tables.indices { image.tables[index].foreignKeys.removeAll { targets.contains($0.referencedTable) } }
            image.sequences.removeAll { $0.owner.map { targets.contains($0.table) } ?? false }
            image.statistics.removeAll { targets.contains($0.table) }
        }
        try snapshot.updateCatalog(image, schemaChanged: true)
    }
}
