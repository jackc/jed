/// Sequence options are exact signed integers. The selected data type constrains
/// creation; the persisted tuple intentionally contains only the resulting bounds.
internal enum SequenceDefinition {
    static func serialType(_ syntax: SQLTypeSyntax) -> ScalarType? {
        guard syntax.arrayDimensions == 0, syntax.modifiers.isEmpty else { return nil }
        switch syntax.name.description {
        case "serial", "serial4": return .i32
        case "bigserial", "serial8": return .i64
        case "smallserial", "serial2": return .i16
        default: return nil
        }
    }
    static func owned(table: SQLText, column: SQLText, ordinal: Int, type: ScalarType,
                      options: [SQLSequenceOption], taken: Set<SQLText>,
                      composites: [SQLText: CompositeType] = [:]) throws -> CatalogSequence {
        let names = options.filter { $0.name == "sequence name" }
        guard names.count <= 1 else { throw JedError("42601", "conflicting or redundant sequence options") }
        let name: SQLText
        if let explicit = names.first?.identifier {
            try CatalogSQL.reserved(explicit, kind: "sequence")
            guard !taken.contains(explicit) else { throw JedError("42P07", "relation already exists: \(explicit)") }
            name = explicit
        } else { name = CatalogSQL.objectName(table, columns: [column], label: "seq", taken: taken) }
        var sequence = try create(name: name, options: options.filter { $0.name != "sequence name" }, type: type, composites: composites)
        sequence.owner = (table, ordinal)
        return sequence
    }
    static func create(name: SQLText, options: [SQLSequenceOption], type: ScalarType? = nil,
                       composites: [SQLText: CompositeType] = [:]) throws -> CatalogSequence {
        let selected = try dataType(options, fixed: type, composites: composites)
        return try apply(CatalogSequence(name: name), options: options, type: selected, creating: true)
    }
    static func alter(_ original: CatalogSequence, options: [SQLSequenceOption], type: ScalarType? = nil,
                      composites: [SQLText: CompositeType] = [:]) throws -> CatalogSequence {
        if options.contains(where: { $0.name == "as" }) { throw JedError("0A000", "ALTER SEQUENCE AS is not supported") }
        return try apply(original, options: options, type: type ?? .i64, creating: false)
    }
    private static func dataType(_ options: [SQLSequenceOption], fixed: ScalarType?,
                                 composites: [SQLText: CompositeType]) throws -> ScalarType {
        if let option = options.first(where: { $0.name == "as" }) {
            guard fixed == nil else { throw JedError("42601", "conflicting or redundant sequence options") }
            guard let syntax = option.type, let resolved = try? TypeRules.resolve(syntax, composites: composites),
                  case .scalar(let scalar) = resolved.type, scalar.integerWidth != nil else {
                throw JedError("22023", "sequence type must be smallint, integer, or bigint")
            }
            return scalar
        }
        let result = fixed ?? .i64
        guard result.integerWidth != nil else { throw JedError("22023", "identity column type must be smallint, integer, or bigint") }
        return result
    }
    private static func integer(_ text: String) throws -> Int64 {
        let negative = text.first == "-", raw = negative ? String(text.dropFirst()) : text
        let magnitude = try TypeRules.unsignedLiteral(raw)
        guard magnitude <= UInt64(Int64.max) + (negative ? 1 : 0) else { throw JedError("22003", "sequence option is out of range for bigint") }
        if negative && magnitude == UInt64(Int64.max) + 1 { return .min }
        return negative ? -Int64(magnitude) : Int64(magnitude)
    }
    private static func apply(_ original: CatalogSequence, options: [SQLSequenceOption], type: ScalarType,
                              creating: Bool) throws -> CatalogSequence {
        var seen: Set<String> = [], numbers: [String: Int64] = [:]
        for option in options {
            guard seen.insert(option.name).inserted else { throw JedError("42601", "conflicting or redundant sequence options") }
            guard ["as", "increment", "minvalue", "maxvalue", "start", "cache", "cycle", "restart"].contains(option.name),
                  !creating || option.name != "restart" else { throw JedError("42601", "invalid sequence option \(option.name)") }
            if let raw = option.number { numbers[option.name] = try integer(raw) }
        }
        var result = original
        if let n = numbers["increment"] { result.increment = n }
        if let n = numbers["cache"] { result.cache = n }
        guard result.increment != 0 else { throw JedError("22023", "INCREMENT must not be zero") }
        guard result.cache > 0 else { throw JedError("22023", "CACHE must be greater than zero") }
        let bounds: (Int64, Int64)
        switch type { case .i16: bounds = (Int64(Int16.min), Int64(Int16.max))
        case .i32: bounds = (Int64(Int32.min), Int64(Int32.max))
        case .i64: bounds = (.min, .max)
        default: throw JedError("22023", "sequence type must be smallint, integer, or bigint") }
        let ascending = result.increment > 0
        if let n = numbers["maxvalue"] { result.maximum = n }
        else if creating || seen.contains("maxvalue") { result.maximum = ascending ? bounds.1 : -1 }
        if let n = numbers["minvalue"] { result.minimum = n }
        else if creating || seen.contains("minvalue") { result.minimum = ascending ? 1 : bounds.0 }
        guard result.maximum <= bounds.1 else { throw JedError("22023", "MAXVALUE is out of range for sequence data type") }
        guard result.minimum >= bounds.0 else { throw JedError("22023", "MINVALUE is out of range for sequence data type") }
        guard result.minimum < result.maximum else { throw JedError("22023", "MINVALUE must be less than MAXVALUE") }
        if let n = numbers["start"] { result.start = n }
        else if creating { result.start = ascending ? result.minimum : result.maximum }
        guard result.start >= result.minimum && result.start <= result.maximum else { throw JedError("22023", "START value is outside sequence bounds") }
        if let cycle = options.first(where: { $0.name == "cycle" })?.enabled { result.cycle = cycle }
        if creating || seen.contains("restart") {
            result.last = numbers["restart"] ?? result.start; result.called = false
        }
        guard result.last >= result.minimum && result.last <= result.maximum else { throw JedError("22023", "RESTART value is outside sequence bounds") }
        return result
    }
}

/// Text sequence arguments name exactly one identifier, independently of SQL
/// keyword classification. Quoted names retain bytes; bare names fold ASCII.
internal enum SequenceName {
    static func parse(_ input: SQLText) throws -> SQLText {
        let bytes = Temporal.trimmed(input.utf8)
        func invalid() -> JedError { JedError("42602", "invalid name syntax") }
        guard !bytes.isEmpty else { throw invalid() }
        if bytes[0] == 34 {
            var output: [UInt8] = [], cursor = 1
            while cursor < bytes.count {
                let byte = bytes[cursor]; cursor += 1
                if byte == 34 {
                    if cursor < bytes.count && bytes[cursor] == 34 { output.append(34); cursor += 1 }
                    else {
                        guard cursor == bytes.count else { throw invalid() }
                        return SQLText(validatedUTF8: output)
                    }
                } else { output.append(byte) }
            }
            throw invalid()
        }
        guard SQLLexicalBoundary.isWordStart(bytes[0]), bytes.dropFirst().allSatisfy({
            SQLLexicalBoundary.isWordStart($0) || SQLLexicalBoundary.isDigit($0) || $0 == 36
        }) else { throw invalid() }
        return SQLText(validatedUTF8: bytes.map(Temporal.asciiLower))
    }
    static func quotedReference(_ name: SQLText) -> SQLText {
        if let first = name.utf8.first, (97...122).contains(first) || first == 95,
           name.utf8.dropFirst().allSatisfy({ (97...122).contains($0) || (48...57).contains($0) || $0 == 95 }) { return name }
        return SQLText(validatedUTF8: [34] + name.utf8.flatMap { $0 == 34 ? [34, 34] : [$0] } + [34])
    }
    static func defaultExpression(_ name: SQLText) -> CatalogExpression {
        let quoted = quotedReference(name).utf8.flatMap { $0 == 39 ? [39, 39] : [$0] }
        return CatalogExpression(SQLText(validatedUTF8: Array("nextval ( '".utf8) + quoted + Array("' )".utf8)))
    }
}
