/// Read-only catalog relations use the statement's pinned snapshot. They have
/// no store or access path; generated rows enter the ordinary relation account.
internal struct SystemRelationPlan {
    enum Kind: String {
        case tables = "jed_tables", columns = "jed_columns", indexes = "jed_indexes"
        case constraints = "jed_constraints", statistics = "jed_statistics", sequences = "jed_sequences"
        case types = "jed_types", fields = "jed_type_fields", views = "jed_views"
    }
    let kind: Kind
    let snapshot: DomainSnapshot
    let sourceDomain: SQLText
    let settings: SessionSettings
    let columns: [ExpressionColumn]

    static func contains(_ name: SQLText) -> Bool { Kind(rawValue: name.description) != nil }
    static func rejectTarget(_ name: SQLText, dropping: Bool = false) throws {
        if contains(name) { throw JedError("42809", "cannot \(dropping ? "drop" : "modify") system relation \"\(name)\"") }
    }
    static func resolve(_ name: SQLName, alias: SQLText?, snapshot: DomainSnapshot,
                        settings: SessionSettings) throws -> SystemRelationPlan? {
        guard let kind = Kind(rawValue: name.name.description) else { return nil }
        let domains = QueryDomains.statement(snapshot: snapshot, settings: settings)
        let sourceDomain = name.database ?? domains.implicitDomain
        let owner = try domains.snapshot(sourceDomain)
        try settings.privileges.require(.select, on: name.name)
        let text = SQLType.scalar(.text), bool = SQLType.scalar(.boolean)
        let i32 = SQLType.scalar(.i32), i64 = SQLType.scalar(.i64)
        let definition: [(String, SQLType)]
        switch kind {
        case .tables: definition = [("name", text)]
        case .columns: definition = [("table_name", text), ("name", text), ("ordinal", i32), ("type", text),
            ("not_null", bool), ("pk_ordinal", i32), ("column_default", text), ("identity_generation", text)]
        case .indexes: definition = [("name", text), ("table_name", text), ("columns", .array(text)), ("is_unique", bool),
            ("method", text), ("predicate", text), ("descending", .array(bool)), ("nulls_first", .array(bool))]
        case .constraints: definition = [("name", text), ("table_name", text), ("type", text), ("columns", .array(text)),
            ("expression", text), ("ref_table", text), ("ref_columns", .array(text)), ("operators", .array(text)), ("match_type", text)]
        case .statistics: definition = [("table_name", text), ("column_name", text), ("analyzed_rows", i64), ("is_stale", bool),
            ("null_count", i64), ("distinct_count", i64), ("sample_rows", i64), ("average_width", i64),
            ("mcv_count", i32), ("histogram_count", i32), ("term_count", i32)]
        case .sequences: definition = [("name", text), ("start_value", i64), ("min_value", i64), ("max_value", i64),
            ("increment_by", i64), ("cycle", bool), ("cache_size", i64), ("last_value", i64),
            ("owned_by_table", text), ("owned_by_column", text)]
        case .types: definition = [("name", text), ("kind", text)]
        case .fields: definition = [("type_name", text), ("name", text), ("ordinal", i32), ("type", text), ("not_null", bool)]
        case .views: definition = [("name", text), ("definition", text), ("columns", .array(text)), ("check_option", text),
            ("security_barrier", bool), ("security_invoker", bool)]
        }
        return SystemRelationPlan(kind: kind, snapshot: owner, sourceDomain: sourceDomain, settings: settings,
            columns: definition.enumerated().map { ExpressionColumn(name: sqlText($0.element.0), qualifier: alias ?? name.name,
                type: TypeDescriptor($0.element.1), index: $0.offset) })
    }

    func produce(context: ExpressionContext, emit: ([SQLValue]) throws -> Void) throws {
        let catalog = snapshot.catalog
        let tables = catalog.tables.sorted { $0.name < $1.name }
        func row(_ values: @autoclosure () throws -> [SQLValue]) throws {
            try context.meter.step(); context.meter.charge(1)
            try emit(values())
        }
        func array(_ values: [SQLValue]) throws -> SQLValue { .array(try SQLArray(elements: values)) }
        func names(_ values: [SQLText]) throws -> SQLValue { try array(values.map(SQLValue.text)) }
        func optional(_ value: SQLText?) -> SQLValue { value.map(SQLValue.text) ?? .null }
        func keys(_ index: CatalogIndex, _ table: CatalogTable) -> [SQLText] {
            index.keys.map { switch $0 { case .column(let i): table.columns[i].name; case .expression(let e): e.text } }
        }
        switch kind {
        case .tables:
            for table in tables { try row([.text(table.name)]) }
        case .columns:
            for table in tables {
                let types = try snapshot.tableTypes(table)
                for (index, column) in table.columns.enumerated() {
                    let pk = table.primaryKey.firstIndex(of: index)
                    try row([.text(table.name), .text(column.name), .integer(Int64(index + 1)),
                        Self.text(Self.typeText(types[index])), .boolean(column.notNull || pk != nil),
                        pk.map { .integer(Int64($0 + 1)) } ?? .null, try defaultText(column, type: types[index]),
                        column.identity.map { Self.text($0 ? "ALWAYS" : "BY DEFAULT") } ?? .null])
                }
            }
            for view in catalog.views.sorted(by: { $0.name < $1.name }) {
                var unrestricted = settings
                unrestricted.domains = try QueryDomains.statement(snapshot: snapshot, settings: settings).viewScope(owner: sourceDomain)
                unrestricted.targetDomain = sourceDomain
                unrestricted.privileges = SessionPrivileges(defaults: .allTables)
                let body = try ViewDefinition.parsed(view)
                var scalar = context.scalar; scalar.meter = nil
                let resolver = ExpressionResolver(composites: snapshot.types, scalar: scalar)
                let plan = try resolver.viewExpansion.withBody(view.name, referenceDepth: 0, offset: 0, bodyDepth: body.depth) {
                    try SelectPlan.resolve(body.query, snapshot: snapshot, settings: unrestricted, scalar: scalar, using: resolver)
                }
                guard plan.columns.count == view.columns.count else { throw JedError("XX003", "stored view output changed") }
                for (index, column) in view.columns.enumerated() {
                    var type = plan.columns[index].type ?? TypeDescriptor(.scalar(.text))
                    if case .array = type.type { type.decimal = nil; type.textLimit = nil; type.temporal = nil }
                    try row([.text(view.name), .text(column.name), .integer(Int64(index + 1)), Self.text(Self.typeText(type)),
                        .boolean(false), .null, optional(column.defaultText), .null])
                }
            }
        case .indexes:
            for table in tables {
                for index in table.indexes.sorted(by: { $0.name < $1.name }) {
                    let options = index.keys.indices.map { index.keyOptions.isEmpty ? UInt8(0) : index.keyOptions[$0] }
                    try row([.text(index.name), .text(table.name), try names(keys(index, table)), .boolean(index.unique),
                        Self.text(index.kind == 1 ? "gin" : index.kind == 2 ? "gist" : "btree"), optional(index.predicate?.text),
                        try array(options.map { .boolean($0 & 1 != 0) }), try array(options.map { .boolean($0 & 2 != 0) })])
                }
            }
        case .constraints:
            for table in tables {
                for check in table.checks.sorted(by: { $0.name < $1.name }) {
                    try row([.text(check.name), .text(table.name), Self.text("check"), .null, .text(check.expression.text), .null, .null, .null, .null])
                }
                for index in table.indexes.sorted(by: { $0.name < $1.name }) where index.unique && index.isConstraint {
                    try row([.text(index.name), .text(table.name), Self.text("unique"), try names(keys(index, table)), .null, .null, .null, .null, .null])
                }
                for foreign in table.foreignKeys.sorted(by: { $0.name < $1.name }) {
                    guard let parent = catalog.tables.first(where: { $0.name == foreign.referencedTable }) else {
                        throw JedError.corruption("foreign key parent is missing")
                    }
                    try row([.text(foreign.name), .text(table.name), Self.text("foreign_key"),
                        try names(foreign.columns.map { table.columns[$0].name }), .null, .text(parent.name),
                        try names(foreign.referencedColumns.map { parent.columns[$0].name }), .null, Self.text(foreign.matchFull ? "full" : "simple")])
                }
                for exclusion in table.exclusions.sorted(by: { $0.name < $1.name }) {
                    let predicate = table.indexes.first { $0.name == exclusion.index }?.predicate?.text
                    try row([.text(exclusion.name), .text(table.name), Self.text("exclude"),
                        try names(exclusion.elements.map { table.columns[$0.column].name }), optional(predicate), .null, .null,
                        try array(exclusion.elements.map { Self.text($0.strategy == 0 ? "&&" : "=") }), .null])
                }
            }
        case .statistics:
            for stat in catalog.statistics.sorted(by: { $0.table == $1.table ? $0.column < $1.column : $0.table < $1.table }) {
                guard let table = catalog.tables.first(where: { $0.name == stat.table }), stat.column < table.columns.count else { continue }
                let nonnull = stat.analyzed - stat.nulls
                let width: SQLValue = nonnull > 0 ? .integer(stat.widthSum / nonnull + (stat.widthSum % nonnull == 0 ? 0 : 1)) : .null
                try row([.text(table.name), .text(table.columns[stat.column].name), .integer(stat.analyzed), .boolean(stat.stale),
                    .integer(stat.nulls), stat.distributionEligible ? .integer(stat.distinct) : .null, .integer(Int64(stat.sampleRows)), width,
                    .integer(Int64(stat.mcv.count)), .integer(Int64(stat.histogram.count)), stat.termEligible ? .integer(Int64(stat.terms.count)) : .null])
            }
        case .sequences:
            for sequence in catalog.sequences.sorted(by: { $0.name < $1.name }) {
                let owner = sequence.owner
                let table = owner.flatMap { owner in catalog.tables.first { $0.name == owner.table } }
                let column = owner.flatMap { owner in table.map { $0.columns[owner.column].name } }
                try row([.text(sequence.name), .integer(sequence.start), .integer(sequence.minimum), .integer(sequence.maximum),
                    .integer(sequence.increment), .boolean(sequence.cycle), .integer(sequence.cache), sequence.called ? .integer(sequence.last) : .null,
                    optional(owner?.table), optional(column)])
            }
        case .types:
            for type in catalog.composites.sorted(by: { $0.name < $1.name }) { try row([.text(type.name), Self.text("composite")]) }
        case .fields:
            for type in catalog.composites.sorted(by: { $0.name < $1.name }) {
                for (index, field) in type.fields.enumerated() {
                    try row([.text(type.name), .text(field.name), .integer(Int64(index + 1)),
                        Self.text(Self.typeText(try field.type.resolve(snapshot.types))), .boolean(field.notNull)])
                }
            }
        case .views:
            for view in catalog.views.sorted(by: { $0.name < $1.name }) {
                try row([.text(view.name), .text(view.definition), try names(view.columns.map(\.name)),
                    Self.text(view.checkOption == 1 ? "LOCAL" : view.checkOption == 2 ? "CASCADED" : "NONE"),
                    .boolean(view.securityBarrier), .boolean(view.securityInvoker)])
            }
        }
    }

    private func defaultText(_ column: CatalogColumn, type: TypeDescriptor) throws -> SQLValue {
        switch column.defaultValue {
        case nil: return .null
        case .expression(let expression): return .text(expression.text)
        case .constant(let value):
            if value.isNull { return .null }
            let text = try ContainerText.render(value, type: type).description
            switch value {
            case .integer, .decimal, .boolean: return Self.text(text)
            default:
                var quoted: [UInt8] = [39]
                for byte in text.utf8 { quoted.append(byte); if byte == 39 { quoted.append(39) } }
                quoted.append(39); return .text(SQLText(validatedUTF8: quoted))
            }
        }
    }
    static func typeText(_ type: TypeDescriptor) -> String {
        switch type.type {
        case .array(let element): return typeText(ContainerText.elementDescriptor(type, element: element)) + "[]"
        case .scalar(.text) where type.textLimit != nil: return "varchar(\(type.textLimit!))"
        case .scalar(.decimal) where type.decimal != nil: return "decimal(\(type.decimal!.precision),\(type.decimal!.scale))"
        default: break
        }
        var result = type.type.name
        if let temporal = type.temporal {
            if case .array(let qualifiers) = GeneratedSpec.datetimeUnits["interval_qualifier"] {
                for case .table(let entry) in qualifiers {
                    if case .integer(let code) = entry["code"], code == temporal.fields,
                       case .text(let name) = entry["name"] { result += " " + name; break }
                }
            }
            if let precision = temporal.precision { result += "(\(precision))" }
        }
        return result
    }
    private static func sqlText(_ text: String) -> SQLText { SQLText(validatedUTF8: Array(text.utf8)) }
    private static func text(_ text: String) -> SQLValue { .text(sqlText(text)) }
}
