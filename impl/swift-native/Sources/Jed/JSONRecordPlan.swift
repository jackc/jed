/// Record functions bind their complete row shape before execution. The JSON
/// object is data; only declared member names participate in conversion.
internal enum JSONRecordPlan {
    static let names: Set<String> = ["json_to_record", "jsonb_to_record", "json_to_recordset", "jsonb_to_recordset",
        "json_populate_record", "jsonb_populate_record", "json_populate_recordset", "jsonb_populate_recordset"]
    static func resolve(_ input: SQLFrom, call: SQLCall, resolver: ExpressionResolver) throws -> TableFunctionPlan {
        let name = call.name.description, populate = name.contains("populate"), json = TypeDescriptor(.scalar(name.hasPrefix("jsonb") ? .jsonb : .json))
        guard call.arguments.count == (populate ? 2 : 1), call.arguments.allSatisfy({ $0.name == nil }) else {
            throw JedError("42883", "record function does not exist")
        }
        var arguments: [ResolvedExpression] = [], fields: [TypeField]
        if populate {
            let base = try resolver.resolve(call.arguments[0].value)
            guard let type = base.type?.type else { throw JedError("42P18", "could not determine polymorphic type") }
            if case .record = type { throw JedError("0A000", "anonymous record base is not supported") }
            guard case .composite(let composite) = type else { throw JedError("42804", "first argument must have a row type") }
            guard input.columnDefinitions.isEmpty else { throw JedError("42601", "column definition list is redundant for a known composite type") }
            fields = composite.fields; arguments.append(base)
        } else {
            guard !input.columnDefinitions.isEmpty else { throw JedError("42601", "a column definition list is required for functions returning record") }
            var seen: Set<SQLText> = []
            fields = try input.columnDefinitions.map { column in
                guard seen.insert(column.name).inserted else { throw JedError("42701", "column name specified more than once") }
                return TypeField(name: column.name, descriptor: try TypeRules.resolve(column.type, composites: resolver.composites))
            }
        }
        for field in fields {
            guard case .scalar = field.descriptor.type else { throw JedError("0A000", "JSON record fields require supported scalar types") }
        }
        let document = try resolver.resolve(call.arguments[populate ? 1 : 0].value, expected: json)
        guard document.type?.type == nil || document.type?.type == json.type else { throw JedError("42883", "record function JSON argument has incompatible type") }
        arguments.append(document)
        try resolver.checkFunction?(call.name)
        let function = JsonFunctionResolver.make(name, arguments, result: TypeDescriptor(.record(fields)), strict: false)
        let columns = fields.enumerated().map { ExpressionColumn(name: $0.element.name, qualifier: input.alias ?? call.name,
            type: $0.element.descriptor, index: $0.offset) }
        return TableFunctionPlan(function: function, columns: columns, recordFields: fields)
    }
    static func produce(_ plan: TableFunctionPlan, values: [SQLValue], context: ExpressionContext,
                        visit: ([SQLValue]) throws -> Void) throws {
        let populate = plan.function.name.contains("populate"), set = plan.function.name.hasSuffix("recordset")
        let document = values[populate ? 1 : 0], fields = plan.recordFields!
        let base: [SQLValue]
        if populate, case .composite(let row) = values[0] { base = row }
        else { base = Array(repeating: .null, count: fields.count) }
        func emit(_ object: SQLValue) throws {
            var result = base
            if !object.isNull {
                let error = JedError("22023", "cannot call a record function on a non-object")
                if case .json(let text) = object {
                    _ = try JSONDocument(text, decodeStrings: true, expectedRoot: "object", rootError: error)
                } else if try JSONFunctions.typeOf(object).description != "object" { throw error }
                for (index, field) in fields.enumerated() {
                    let value = try JSONFunctions.field(object, key: field.name, constant: true, meter: context.meter)
                    if value.isNull { continue }
                    let node = try JSONFunctions.node(value)
                    if case .null = node { result[index] = .null }
                    else if field.descriptor.type == .scalar(.json), case .json = value { result[index] = value }
                    else { result[index] = try ResolvedJSONExpression.coerce(node, to: field.descriptor, context: context) }
                }
            }
            context.meter.charge(1); try context.meter.checkpoint(); try visit(result)
        }
        if set {
            guard !document.isNull else { return }
            if case .json(let text) = document, text.utf8.first(where: { ![9, 10, 13, 32].contains($0) }) != 91 {
                _ = try JSONDocument(text, decodeStrings: true, expectedRoot: "array",
                    rootError: JedError("22023", "cannot call a recordset function on a non-array"))
            }
            try JSONFunctions.each(document, array: true, meter: context.meter) { _, item in try emit(item); return true }
        } else { try emit(document) }
    }
}
