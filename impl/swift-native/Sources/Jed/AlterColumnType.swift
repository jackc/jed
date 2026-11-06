internal enum AlterColumnType {
    static func apply(_ ordinal: Int, target syntax: SQLTypeSyntax, using: SQLExpression?, table: inout CatalogTable,
                      image: inout CatalogImage, source: SQLText, snapshot: DomainSnapshot, settings: SessionSettings,
                      context: ExpressionContext, preparationCost: inout Int64) throws -> ResolvedExpression {
        let old = table.columns[ordinal]
        var target = try TypeRules.resolve(syntax, composites: snapshot.types)
        if ExpressionCollation.collatable(target.type) { target.collation = image.collations.first(where: \.isDefault)?.name }
        if table.primaryKey.contains(ordinal) { try KeyEncoding.validateType(target.type) }
        if old.identity != nil {
            guard case .scalar(let scalar) = target.type, scalar.integerWidth != nil else {
                throw JedError("22023", "identity column type must be smallint, integer, or bigint")
            }
        }
        let column = SQLExpression(kind: .column(qualifier: nil, name: old.name), span: syntax.span, position: syntax.span.position)
        let conversion = SQLExpression(kind: .cast(using ?? column, syntax), span: syntax.span, position: syntax.span.position)
        let resolver = try CatalogDDL.resolver(snapshot: snapshot, settings: settings, context: context, table: table)
        let expression = try resolver.resolve(conversion)
        var replacement = old; replacement.type = try CatalogSQL.typeUse(target)
        switch old.defaultValue {
        case .none: break
        case .constant(let value):
            // Resolve through a typed column, preserving the ordinary explicit
            // cast charge even though the saved default value is a constant.
            let cast = try resolver.resolve(SQLExpression(kind: .cast(column, syntax), span: syntax.span, position: syntax.span.position))
            let meter = CostMeter(limit: context.meter.limit, lifetime: context.meter.lifetime,
                resources: context.meter.resources, cancellation: context.meter.cancellation)
            var execution = ExpressionContext(meter: meter, timezone: context.scalar.timezone, column: { _ in value })
            execution.scalar = context.scalar; execution.scalar.meter = meter; execution.collatedKey = context.collatedKey
            replacement.defaultValue = .constant(try execution.evaluate(cast))
            preparationCost = saturatingAdd(preparationCost, meter.accrued)
        case .expression(let stored):
            guard let parsed = stored.parsed else { throw JedError("XX003", "stored default expression cannot be parsed") }
            let empty = try CatalogDDL.resolver(snapshot: snapshot, settings: settings, context: context)
            _ = try empty.resolve(SQLExpression(kind: .cast(parsed, syntax), span: syntax.span, position: syntax.span.position))
            let typeText = try CatalogSQL.render(Array(source.utf8[syntax.span.byteStart..<syntax.span.byteEnd]))
            replacement.defaultValue = .expression(CatalogExpression(SQLText(validatedUTF8:
                Array("CAST ( ".utf8) + stored.text.utf8 + Array(" AS ".utf8) + typeText.utf8 + Array(" )".utf8))))
        }
        if old.identity != nil {
            guard case .scalar(let before) = old.type.type, case .scalar(let after) = target.type,
                  case .expression(let defaultValue) = old.defaultValue,
                  let slot = image.sequences.firstIndex(where: {
                      $0.owner?.table == table.name && $0.owner?.column == ordinal && SequenceReferences.contains($0.name, in: defaultValue.text)
                  }) else { throw JedError.corruption("identity column has no owned sequence") }
            image.sequences[slot] = try retype(image.sequences[slot], from: before, to: after)
        }
        table.columns[ordinal] = replacement
        return expression
    }
    private static func retype(_ original: CatalogSequence, from: ScalarType, to: ScalarType) throws -> CatalogSequence {
        func bounds(_ type: ScalarType) throws -> (Int64, Int64) {
            switch type {
            case .i16: return (Int64(Int16.min), Int64(Int16.max))
            case .i32: return (Int64(Int32.min), Int64(Int32.max))
            case .i64: return (.min, .max)
            default: throw JedError("22023", "identity column type must be smallint, integer, or bigint")
            }
        }
        let old = try bounds(from), new = try bounds(to)
        var result = original
        if original.increment > 0 {
            if result.maximum == old.1 { result.maximum = new.1 }
        } else if result.minimum == old.0 { result.minimum = new.0 }
        guard result.minimum >= new.0, result.maximum <= new.1, result.minimum < result.maximum,
              result.start >= result.minimum, result.start <= result.maximum,
              result.last >= result.minimum, result.last <= result.maximum else {
            throw JedError("22023", "sequence values are outside the new identity type bounds")
        }
        return result
    }
    static func validateEndState(table: CatalogTable, changed: Set<Int>, image: CatalogImage,
                                 snapshot: DomainSnapshot) throws {
        for index in table.indexes {
            for key in index.keys {
                guard case .column(let ordinal) = key, changed.contains(ordinal) else { continue }
                let type = try table.columns[ordinal].type.resolve(snapshot.types)
                switch index.kind {
                case 0: try KeyEncoding.validateType(type.type)
                case 1: _ = try GinOpclass(type: type)
                case 2: _ = try GistOpclass(type: type)
                default: throw JedError.corruption("invalid index kind")
                }
            }
        }
        for child in image.tables {
            for foreign in child.foreignKeys where child.name == table.name || foreign.referencedTable == table.name {
                guard let parent = image.tables.first(where: { $0.name == foreign.referencedTable }) else { throw JedError.corruption("foreign key parent is missing") }
                for (local, remote) in zip(foreign.columns, foreign.referencedColumns) {
                    guard try CatalogDDL.foreignKeyPair(child.columns[local].type.resolve(snapshot.types).type,
                                                        parent.columns[remote].type.resolve(snapshot.types).type) else {
                        throw JedError("42804", "foreign key constraint cannot be implemented: incompatible key column types")
                    }
                }
            }
        }
    }
}
