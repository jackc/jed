/// Canonical persisted expression text is a token contract, independent of the
/// expression tree and its eventual resolver. Source offsets are UTF-8 bytes.
internal enum CatalogSQL {
    static func expression(_ syntax: SQLExpression, source: SQLText) throws -> CatalogExpression {
        let span = syntax.span
        guard span.byteStart >= 0, span.byteEnd >= span.byteStart, span.byteEnd <= source.utf8.count else {
            throw JedError("XX000", "expression source span is outside the statement")
        }
        return CatalogExpression(try render(Array(source.utf8[span.byteStart..<span.byteEnd])))
    }
    static func render(_ utf8: [UInt8]) throws -> SQLText {
        let tokens = try SQLLexer(utf8: utf8).tokenize()
        var parts: [String] = []
        for token in tokens {
            switch token.kind {
            case .word(let name, let quoted):
                let escaped = String(decoding: name.utf8.flatMap { $0 == 34 ? [34, 34] : [$0] }, as: UTF8.self)
                parts.append(quoted ? "\"" + escaped + "\"" : name.description)
            case .string(let text):
                let escaped = String(decoding: text.utf8.flatMap { $0 == 39 ? [39, 39] : [$0] }, as: UTF8.self)
                parts.append("'" + escaped + "'")
            case .number(let spelling):
                let plain = spelling.lowercased()
                let decimal = plain.contains(".") || (!plain.hasPrefix("0x") && plain.contains("e"))
                if decimal {
                    _ = try SQLDecimal.parse(spelling)
                    // Token coefficients preserve written leading zeros. Value
                    // normalization would change catalog bytes (0.50 -> .50).
                    let spelling = plain.filter { $0 != "_" }
                    let exponentParts = spelling.split(separator: "e", omittingEmptySubsequences: false)
                    let mantissa = exponentParts[0]
                    let exponent = exponentParts.count == 2 ? Int(exponentParts[1]) ?? 0 : 0
                    let point = mantissa.firstIndex(of: ".")
                    var scale = (point.map { mantissa.distance(from: mantissa.index(after: $0), to: mantissa.endIndex) } ?? 0) - exponent
                    var digits = mantissa.filter { $0 != "." }
                    if scale < 0 { digits += String(repeating: "0", count: -scale); scale = 0 }
                    if scale == 0 { parts.append(digits + ".") }
                    else if digits.count <= scale {
                        parts.append("." + String(repeating: "0", count: scale - digits.count) + digits)
                    } else {
                        let split = digits.index(digits.endIndex, offsetBy: -scale)
                        parts.append(String(digits[..<split]) + "." + String(digits[split...]))
                    }
                } else { parts.append(String(try TypeRules.unsignedLiteral(spelling))) }
            case .parameter(let index): parts.append("$\(index)")
            case .symbol(let symbol): parts.append(symbol == "!=" ? "<>" : symbol)
            case .end: break
            }
        }
        return try SQLText(parts.joined(separator: " "))
    }

    static func reserved(_ name: SQLText, kind: String) throws {
        guard !name.utf8.starts(with: [106, 101, 100, 95]) else {
            throw JedError("42939", "\(kind) name \(name) is reserved (the jed_ prefix is reserved for system objects)")
        }
    }
    static func objectName(_ table: SQLText, columns: [SQLText] = [], label: String, taken: Set<SQLText> = []) -> SQLText {
        var addition: [UInt8] = []
        for column in columns {
            if !addition.isEmpty { addition.append(95) }
            addition += column.utf8
            if addition.count >= 64 { break }
        }
        var suffix = 0
        while true {
            let tail = Array((label + (suffix == 0 ? "" : String(suffix))).utf8)
            let budget = 63 - tail.count - (addition.isEmpty ? 1 : 2)
            var a = table.utf8.count, b = addition.count
            while a + b > budget { if a > b { a -= 1 } else { b -= 1 } }
            while a > 0 && a < table.utf8.count && table.utf8[a] & 0xc0 == 0x80 { a -= 1 }
            while b > 0 && b < addition.count && addition[b] & 0xc0 == 0x80 { b -= 1 }
            var bytes = Array(table.utf8.prefix(a))
            if !addition.isEmpty { bytes.append(95); bytes += addition.prefix(b) }
            bytes.append(95); bytes += tail
            let name = SQLText(validatedUTF8: bytes)
            if !taken.contains(name) { return name }
            suffix += 1
        }
    }
    static func typeUse(_ descriptor: TypeDescriptor) throws -> CatalogTypeUse {
        func stored(_ type: SQLType) throws -> CatalogType {
            switch type {
            case .scalar(let type): return .scalar(type)
            case .composite(let type): return .composite(type.name)
            case .array(let element): return .array(try stored(element))
            case .range(let element):
                _ = try ValueCodec.rangeElement(element)
                guard case .scalar(let scalar) = element else { throw JedError("0A000", "unsupported range element type") }
                return .range(scalar)
            case .record: throw JedError("42P16", "anonymous record is not a stored type")
            }
        }
        var result = CatalogTypeUse(try stored(descriptor.type))
        result.decimal = descriptor.decimal; result.textLimit = descriptor.textLimit
        result.temporal = descriptor.temporal; result.collation = descriptor.collation
        return result
    }
    static func relationNames(_ catalog: CatalogImage) -> Set<SQLText> {
        Set(catalog.tables.map(\.name) + catalog.tables.flatMap { $0.indexes.map(\.name) }
            + catalog.views.map(\.name) + catalog.sequences.map(\.name))
    }
    static func constraints(_ table: CatalogTable) -> Set<SQLText> {
        Set(table.checks.map(\.name) + table.indexes.filter(\.isConstraint).map(\.name)
            + table.foreignKeys.map(\.name) + table.exclusions.map(\.name))
    }
}

/// Structural exclusions precede name/type resolution. A work stack keeps this
/// walk bounded by the already-admitted parser tree without adding recursion.
internal enum CatalogExpressionPolicy {
    static func validate(_ expression: SQLExpression, isDefault: Bool) throws {
        var pending = [expression]
        while let expression = pending.popLast() {
            switch expression.kind {
            case .column where isDefault: throw JedError("0A000", "cannot use column reference in DEFAULT expression")
            case .parameter(let index): throw JedError("42P02", "there is no parameter $\(index)")
            case .scalarQuery, .exists, .memberQuery, .quantifiedQuery:
                throw JedError("0A000", isDefault ? "cannot use subquery in DEFAULT expression" : "cannot use subquery in check constraint")
            case .call(let call):
                if call.over != nil || call.overName != nil {
                    throw JedError("42P20", isDefault ? "window functions are not allowed in DEFAULT expressions" : "window functions are not allowed in check constraints")
                }
                if GeneratedSpec.functions.contains(where: { $0.name == call.name.description && $0.kind == "aggregate" }) {
                    throw JedError("42803", isDefault ? "aggregate functions are not allowed in DEFAULT expressions" : "aggregate functions are not allowed in check constraints")
                }
            default: break
            }
            pending.append(contentsOf: children(expression).reversed())
        }
    }
    static func children(_ expression: SQLExpression) -> [SQLExpression] {
        switch expression.kind {
        case .unary(_, let value), .cast(let value, _), .collate(let value, _), .field(let value, _), .parenthesized(let value),
             .isNull(let value, _), .isJSON(let value, _, _, _), .extract(_, let value): return [value]
        case .binary(_, let a, let b), .isDistinct(let a, let b, _), .quantified(_, let a, _, let b): return [a, b]
        case .row(let items), .array(let items), .conditionalFunction(_, let items): return items
        case .member(let value, let items, _): return [value] + items
        case .between(let value, let lower, let upper, _): return [value, lower, upper]
        case .pattern(_, let value, let pattern, let escape, _): return [value, pattern] + (escape.map { [$0] } ?? [])
        case .subscripted(let value, let bounds): return [value] + bounds.flatMap { [$0.lower, $0.upper].compactMap { $0 } }
        case .conditional(let operand, let arms, let fallback):
            return (operand.map { [$0] } ?? []) + arms.flatMap { [$0.condition, $0.result] } + (fallback.map { [$0] } ?? [])
        case .call(let call): return call.arguments.map(\.value) + call.orderBy.map(\.expression)
            + call.withinGroup.map(\.expression) + (call.filter.map { [$0] } ?? [])
        case .json(let value): return [value.context] + (value.path.map { [$0] } ?? []) + value.passing.map(\.value)
            + [value.onEmpty?.value, value.onError?.value].compactMap { $0 }
        default: return []
        }
    }
}
