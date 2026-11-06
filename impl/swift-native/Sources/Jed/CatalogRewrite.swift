/// Rename only resolved syntactic name positions. Function names, type names,
/// string values, and record-field selectors are not column references.
internal enum CatalogRewrite {
    static func expression(_ expression: CatalogExpression, table: SQLText,
                           column: (old: SQLText, new: SQLText)? = nil,
                           renamedTable: SQLText? = nil) throws -> CatalogExpression {
        guard let root = expression.parsed else { throw JedError("XX003", "stored expression cannot be parsed") }
        var replacements: [(SQLSourceSpan, [UInt8])] = [], work = [root]
        func quoted(_ name: SQLText) -> [UInt8] { [34] + name.utf8.flatMap { $0 == 34 ? [34, 34] : [$0] } + [34] }
        while let node = work.popLast() {
            if case .column(let qualifier, let name) = node.kind, qualifier == nil || qualifier == table {
                if let column, name == column.old {
                    let prefix = qualifier.map { quoted($0) + [UInt8(46)] } ?? []
                    replacements.append((node.span, prefix + quoted(column.new)))
                } else if let renamedTable, qualifier == table {
                    replacements.append((node.span, quoted(renamedTable) + [46] + quoted(name)))
                }
            }
            work += CatalogExpressionPolicy.children(node)
        }
        guard !replacements.isEmpty else { return expression }
        replacements.sort { $0.0.byteStart < $1.0.byteStart }
        var bytes: [UInt8] = [], cursor = 0
        for (span, replacement) in replacements {
            bytes += expression.text.utf8[cursor..<span.byteStart]; bytes += replacement; cursor = span.byteEnd
        }
        bytes += expression.text.utf8[cursor...]
        return CatalogExpression(try CatalogSQL.render(bytes))
    }
    static func tableExpressions(_ table: inout CatalogTable, column: (old: SQLText, new: SQLText)? = nil,
                                 renamedTable: SQLText? = nil) throws {
        let name = table.name
        for index in table.columns.indices {
            if case .expression(let old) = table.columns[index].defaultValue {
                table.columns[index].defaultValue = .expression(try expression(old, table: name, column: column, renamedTable: renamedTable))
            }
        }
        for index in table.checks.indices {
            table.checks[index].expression = try expression(table.checks[index].expression, table: name, column: column, renamedTable: renamedTable)
        }
        for index in table.indexes.indices {
            for key in table.indexes[index].keys.indices {
                if case .expression(let old) = table.indexes[index].keys[key] {
                    table.indexes[index].keys[key] = .expression(try expression(old, table: name, column: column, renamedTable: renamedTable))
                }
            }
            if let old = table.indexes[index].predicate {
                table.indexes[index].predicate = try expression(old, table: name, column: column, renamedTable: renamedTable)
            }
        }
    }
}
