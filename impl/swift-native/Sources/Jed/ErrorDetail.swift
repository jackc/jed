/// Constraint diagnostics are built only after the violation is selected.
/// Rendering has its own scratch allowance and never changes statement work,
/// storage, or the error if a value cannot be rendered.
internal struct ErrorDetail {
    let settings: SessionSettings
    static func literalSlot(_ syntax: SQLExpression) -> Bool {
        switch syntax.kind {
        case .null, .number, .string, .boolean, .parameter: return true
        case .row(let children), .array(let children): return children.allSatisfy(literalSlot)
        default: return false
        }
    }
    func visible(_ table: SQLText) -> Bool { (try? settings.privileges.require(.select, on: table)) != nil }
    private func render(_ value: SQLValue, type: TypeDescriptor) throws -> String {
        let text = try value.isNull ? SQLText("null") : settings.render(value, type: type)
        var count = min(64, text.utf8.count)
        while count > 0 && count < text.utf8.count && text.utf8[count] & 0xc0 == 0x80 { count -= 1 }
        return SQLText(validatedUTF8: Array(text.utf8.prefix(count))).description + (count < text.utf8.count ? "..." : "")
    }
    private func phrase(_ labels: [String], values: [String]) -> String {
        "Key (" + labels.joined(separator: ", ") + ")=(" + values.joined(separator: ", ") + ")"
    }
    private func columnPhrase(_ table: CatalogTable, types: [TypeDescriptor], columns: [Int],
                              value: (Int) throws -> SQLValue) throws -> String {
        let labels = columns.map { TextFunctions.quoteIdentifier(table.columns[$0].name).description }
        let values = try columns.map { try render(value($0), type: types[$0]) }
        return phrase(labels, values: values)
    }
    func row(_ table: CatalogTable, types: [TypeDescriptor], value: (Int) throws -> SQLValue) -> String? {
        guard visible(table.name), let values = try? types.indices.map({ try render(value($0), type: types[$0]) }) else { return nil }
        return "Failing row contains (" + values.joined(separator: ", ") + ")."
    }
    func primary(_ table: CatalogTable, types: [TypeDescriptor], value: (Int) throws -> SQLValue,
                 ending: String = "already exists.") -> String? {
        guard visible(table.name), let key = try? columnPhrase(table, types: types, columns: table.primaryKey, value: value) else { return nil }
        return key + " " + ending
    }
    func index(_ table: CatalogTable, plan: StoredIndexPlan, value: @escaping (Int) throws -> SQLValue,
               ending: String = "already exists.") -> String? {
        guard visible(table.name), plan.index.keys.count == plan.ordered.keys.count else { return nil }
        do {
            var labels: [String] = [], values: [String] = []
            for ordinal in plan.index.keys.indices {
                switch plan.index.keys[ordinal] {
                case .column(let column): labels.append(TextFunctions.quoteIdentifier(table.columns[column].name).description)
                case .expression(let definition): labels.append(definition.text.description)
                }
                // Each expression key has the default independent scalar budget,
                // as the frozen error-detail path's scratch meter does.
                let meter = CostMeter(resources: StatementResources(scalarLimit: GeneratedSpec.defaultScalarBytes))
                let context = StatementServices(settings: settings, meter: meter).context(column: value)
                values.append(try render(context.evaluate(plan.ordered.keys[ordinal]), type: plan.ordered.types[ordinal]))
            }
            return phrase(labels, values: values) + " " + ending
        } catch { return nil }
    }
    func child(_ plan: ForeignKeyValidation.Plan, value: (Int) throws -> SQLValue) -> String? {
        let tail = "is not present in table \"\(plan.parent.name)\"."
        guard visible(plan.child.name) else { return "Key " + tail }
        guard let key = try? columnPhrase(plan.child, types: plan.childTypes, columns: plan.foreign.columns, value: value) else { return nil }
        return key + " " + tail
    }
    func parent(_ plan: ForeignKeyValidation.Plan, tuple: [SQLValue]) -> String? {
        let tail = "is still referenced from table \"\(plan.child.name)\"."
        guard visible(plan.parent.name) else { return "Key " + tail }
        guard tuple.count == plan.foreign.referencedColumns.count else { return nil }
        let labels = plan.foreign.referencedColumns.map { TextFunctions.quoteIdentifier(plan.parent.columns[$0].name).description }
        guard let values = try? tuple.indices.map({ try render(tuple[$0], type: plan.parentTypes[plan.foreign.referencedColumns[$0]]) }) else { return nil }
        return phrase(labels, values: values) + " " + tail
    }
}
internal extension JedError {
    func identity(_ column: SQLText, insert: Bool) -> JedError {
        detailed("Column \"\(column)\" is an identity column defined as GENERATED ALWAYS.")
            .hinted(insert ? "Use OVERRIDING SYSTEM VALUE to override." : nil)
    }
    func inDatabase(_ name: SQLText) -> JedError {
        var result = self
        if result.tableName != nil && result.databaseName == nil { result.databaseName = name.description }
        return result
    }
    func withColumn(_ name: SQLText) -> JedError {
        var result = self; result.columnName = name.description; return result
    }

    func object(table: SQLText, constraint: SQLText? = nil, column: SQLText? = nil, detail: String? = nil) -> JedError {
        var result = self
        result.tableName = table.description; result.constraintName = constraint?.description
        result.columnName = column?.description; result.detail = detail
        return result
    }
    func storing(table: SQLText, column: CatalogColumn, type: TypeDescriptor) -> JedError {
        var result = self; result.tableName = table.description
        if code == "22001" {
            if result.columnName == nil { result.columnName = column.name.description }
            if result.dataTypeName == nil { result.dataTypeName = SystemRelationPlan.typeText(type) }
        }
        return result
    }
}
