/// Scalar facts come from the frozen language-neutral tables. Recursive open
/// types remain handwritten: nominal identity and structural comparability are
/// different relations and must never be replaced with host enum equality.
internal enum TypeRules {
    static let scalarFacts: [String: ScalarSpecFact] = Dictionary(uniqueKeysWithValues: GeneratedSpec.scalars.map { ($0.id, $0) })
    private static let scalarNames: [SQLText: ScalarType] = {
        var result: [SQLText: ScalarType] = [:]
        for type in ScalarType.allCases {
            guard let fact = scalarFacts[type.name] else { continue }
            for name in [fact.id] + fact.aliases { result[SQLText(validatedUTF8: Array(name.utf8))] = type }
        }
        return result
    }()
    static func scalar(named name: SQLText) -> ScalarType? { scalarNames[name] }
    static func resolve(_ syntax: SQLTypeSyntax, composites: [SQLText: CompositeType]) throws -> TypeDescriptor {
        var result: TypeDescriptor
        if let scalar = scalar(named: syntax.name) { result = TypeDescriptor(.scalar(scalar)) }
        else if let range = GeneratedSpec.ranges.first(where: { fact in
            ([fact.id] + fact.aliases).contains { Array($0.utf8) == syntax.name.utf8 }
        }), let element = scalarNames[SQLText(validatedUTF8: Array(range.element.utf8))] {
            result = TypeDescriptor(.range(.scalar(element)))
        } else if let composite = composites[syntax.name] { result = TypeDescriptor(.composite(composite)) }
        else { throw JedError("42704", "type does not exist: \(syntax.name)").at(syntax.span.position) }
        if !syntax.modifiers.isEmpty || !syntax.intervalFields.isEmpty {
            let modifiers = try syntax.modifiers.map(unsignedLiteral)
            switch result.type {
            case .scalar(.decimal):
                let precision = modifiers.first ?? 0, scale = modifiers.count > 1 ? modifiers[1] : 0
                guard (1...1000).contains(precision) else { throw JedError("22023", "NUMERIC precision \(precision) must be between 1 and 1000") }
                guard scale <= precision else { throw JedError("22023", "NUMERIC scale \(scale) must be between 0 and precision \(precision)") }
                result.decimal = try DecimalTypmod(precision: Int(precision), scale: Int(scale))
            case .scalar(.text):
                guard modifiers.count == 1 else { throw JedError("42601", "varchar takes exactly one type modifier (a length)") }
                guard modifiers[0] >= 1 else { throw JedError("22023", "length for type varchar must be at least 1") }
                guard modifiers[0] <= 10_485_760 else { throw JedError("22023", "length for type varchar cannot exceed 10485760") }
                result.textLimit = Int(modifiers[0])
            case .scalar(.timestamp), .scalar(.timestamptz), .scalar(.interval):
                guard modifiers.count <= 1 else { throw JedError("42601", "a timestamp or interval takes at most one precision") }
                var fields: UInt8 = 0
                if !syntax.intervalFields.isEmpty {
                    let name = syntax.intervalFields.joined(separator: " to ")
                    for row in rows(GeneratedSpec.datetimeUnits, "interval_qualifier") {
                        if row["name"] == .text(name), case .integer(let code) = row["code"] { fields = UInt8(code); break }
                    }
                    guard fields != 0 else { throw JedError("42601", "invalid interval qualifier: \(name)") }
                }
                result.temporal = try TemporalTypmod(fields: fields, precision: modifiers.first.map { UInt8(min($0, 6)) })
            default: throw JedError("0A000", "a type modifier is not supported for type \(result.type.name)")
            }
        }
        if syntax.arrayDimensions != 0 {
            guard result.decimal == nil, result.textLimit == nil, result.temporal == nil else {
                throw JedError("0A000", "array element type modifiers are not supported")
            }
            _ = try ValueCodec.arrayElement(result.type)
            result = TypeDescriptor(.array(result.type))
        }
        return result
    }

    /// Lexer-admitted SQL integer spellings (including radix and separators).
    /// Type input from text uses its separate, narrower input grammar.
    static func unsignedLiteral(_ spelling: String) throws -> UInt64 {
        let bytes = Array(spelling.utf8).filter { $0 != 95 }
        var radix: UInt64 = 10, offset = 0
        if bytes.count > 2, bytes[0] == 48 {
            switch bytes[1] | 32 {
            case 120: radix = 16; offset = 2
            case 111: radix = 8; offset = 2
            case 98: radix = 2; offset = 2
            default: break
            }
        }
        var result: UInt64 = 0
        for byte in bytes.dropFirst(offset) {
            let digit = byte >= 48 && byte <= 57 ? UInt64(byte - 48) : byte | 32 >= 97 && byte | 32 <= 102 ? UInt64((byte | 32) - 87) : UInt64.max
            guard digit < radix else { throw JedError("42601", "invalid integer literal") }
            let product = result.multipliedReportingOverflow(by: radix), sum = product.partialValue.addingReportingOverflow(digit)
            guard !product.overflow, !sum.overflow else { throw JedError("22003", "integer literal out of range") }
            result = sum.partialValue
        }
        return result
    }
    static func family(_ scalar: ScalarType) -> String { scalarFacts[scalar.name]!.family }
    static func fields(_ type: SQLType) -> [TypeField]? {
        switch type { case .composite(let value): value.fields; case .record(let fields): fields; default: nil }
    }
    private static func rank(_ scalar: ScalarType) -> Int64 {
        if case .integer(let rank) = scalarFacts[scalar.name]?.metadata["rank"] { return rank }
        return 0
    }
    static func rows(_ table: [String: SpecDataValue], _ key: String) -> [[String: SpecDataValue]] {
        guard case .array(let values) = table[key] else { return [] }
        return values.compactMap { if case .table(let row) = $0 { row } else { nil } }
    }
    static func scalarComparison(_ left: ScalarType, _ right: ScalarType) -> String? {
        let l = family(left), r = family(right)
        for row in rows(GeneratedSpec.comparison, "comparable") {
            if row["left_family"] == .text(l), row["right_family"] == .text(r), case .text(let via) = row["via"] { return via }
        }
        return nil
    }
    static func comparable(_ left: SQLType?, _ right: SQLType?) throws {
        for type in [left, right] {
            if type == .scalar(.json) || type == .scalar(.jsonpath) {
                throw JedError("42883", "operator does not exist: \(type!.name) is not comparable")
            }
        }
        guard let left, let right else { return }
        if let a = fields(left), let b = fields(right) {
            guard a.count == b.count else { throw JedError("42804", "cannot compare rows of different sizes").hinted(nil) }
            for (a, b) in zip(a, b) { try comparable(a.descriptor.type, b.descriptor.type) }
            return
        }
        switch (left, right) {
        case (.scalar(.jsonb), .scalar(.jsonb)): return
        case (.scalar(let a), .scalar(let b)) where scalarComparison(a, b) != nil: return
        case (.array(let a), .array(let b)):
            let integers: Bool
            if case .scalar(let a) = a, case .scalar(let b) = b { integers = a.integerWidth != nil && b.integerWidth != nil }
            else { integers = false }
            guard a == b || integers else { throw JedError("42804", "cannot compare arrays of different element types") }
            try comparable(a, b); return
        case (.range(let a), .range(let b)) where a == b: return
        default: throw JedError("42804", "cannot compare \(left.name) and \(right.name)")
        }
    }
    static func common(_ left: SQLType?, _ right: SQLType?) throws -> SQLType? {
        guard let left else { return right }
        guard let right else { return left }
        if left == right { return left }
        switch (left, right) {
        case (.scalar(let a), .scalar(let b)):
            switch scalarComparison(a, b) {
            case "promote": return .scalar(rank(a) >= rank(b) ? a : b)
            case "promote-to-decimal": return .scalar(.decimal)
            default: break
            }
        case (.array(let a), .array(let b)): return .array(try commonElement(a, b))
        case (.record(let a), .record(let b)): return .record(try commonFields(a, b))
        default: break
        }
        throw JedError("42804", "cannot match types \(left.name) and \(right.name)")
    }
    private static func commonElement(_ left: SQLType, _ right: SQLType) throws -> SQLType {
        if left == right { return left }
        switch (left, right) {
        case (.scalar(let a), .scalar(let b)) where a.integerWidth != nil && b.integerWidth != nil:
            return .scalar(rank(a) >= rank(b) ? a : b)
        case (.array(let a), .array(let b)): return .array(try commonElement(a, b))
        case (.record(let a), .record(let b)): return .record(try commonFields(a, b))
        default: throw JedError("42804", "cannot match container elements \(left.name) and \(right.name)")
        }
    }
    private static func commonFields(_ left: [TypeField], _ right: [TypeField]) throws -> [TypeField] {
        guard left.count == right.count else { throw JedError("42804", "cannot match records of different sizes") }
        return try zip(left, right).map { a, b in
            guard a.name == b.name else { throw JedError("42804", "cannot match records with different fields") }
            return TypeField(name: a.name, descriptor: TypeDescriptor(try commonElement(a.descriptor.type, b.descriptor.type)))
        }
    }
    enum CastMode: Int { case implicitCast = 0, assignment = 1, explicit = 2 }
    static func admitsCast(from: SQLType, to: SQLType, mode: CastMode) -> Bool {
        if from == to { return true }
        switch (from, to) {
        // J3's semantic cast table (json.md §6.1) is in addition to the
        // scalar catalog rows; jsonb -> json alone admits assignment.
        case (.scalar(.jsonb), .scalar(.json)): return mode.rawValue >= CastMode.assignment.rawValue
        case (.scalar(.json), .scalar(.jsonb)), (.scalar(.text), .scalar(.json)), (.scalar(.text), .scalar(.jsonb)):
            return mode == .explicit
        case (.scalar(let a), .scalar(let b)):
            return rows(GeneratedSpec.casts, "cast").contains { row in
                guard row["from"] == .text(a.name), row["to"] == .text(b.name), case .text(let spelling) = row["mode"] else { return false }
                let required = spelling == "implicit" ? 0 : spelling == "assignment" ? 1 : 2
                return mode.rawValue >= required
            }
        case (.array(.scalar(let a)), .array(.scalar(let b))):
            if [.json, .jsonb, .jsonpath].contains(a) || [.json, .jsonb, .jsonpath].contains(b) { return false }
            return mode == .explicit && admitsCast(from: .scalar(a), to: .scalar(b), mode: mode)
        case (.scalar(.text), .array), (.array, .scalar(.text)), (.scalar(.text), .composite), (.composite, .scalar(.text)), (.record, .scalar(.text)):
            return mode == .explicit
        default: return false
        }
    }
    static func argumentCoercion(from source: ScalarType, to target: ScalarType, literal: String? = nil) -> Int? {
        if source == target { return 0 }
        return rows(GeneratedSpec.comparison, "arg_coercion").compactMap { row -> Int? in
            guard (row["from"] == .text(source.name) || literal.map({ row["from"] == .text($0) }) == true),
                  row["to"] == .text(target.name), case .integer(let cost) = row["cost"] else { return nil }
            return Int(cost)
        }.min()
    }
}
