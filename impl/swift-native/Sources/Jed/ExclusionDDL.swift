extension CatalogDDL {
    struct ExclusionDefinition {
        let kind: UInt8
        let elements: [CatalogExclusion.Element]
        let predicate: CatalogExpression?
        let dependencies: IndexDependencies.Facts
        var columns: [Int] { elements.map(\.column) }
        func index(_ name: SQLText, root: PageReference) -> CatalogIndex {
            var index = CatalogIndex(name: name, keys: columns.map(CatalogIndexKey.column), kind: kind, root: root, predicate: predicate)
            index.hosts = dependencies.hosts; index.zones = dependencies.zones; index.dynamicZones = dependencies.dynamicZones
            return index
        }
    }
    static func exclusionDefinition(method: SQLText?, elements: [SQLExcludeElement], predicate: SQLExpression?,
                                    table: CatalogTable, source: SQLText, snapshot: DomainSnapshot,
                                    settings: SessionSettings, context: ExpressionContext) throws -> ExclusionDefinition {
        let kind: UInt8
        switch method?.description ?? "btree" {
        case "btree": kind = 0
        case "gist": kind = 2
        case "gin": throw JedError("0A000", "access method gin does not support exclusion constraints")
        default: throw JedError("42704", "access method does not exist: \(method!)")
        }
        var resolved: [CatalogExclusion.Element] = []
        for element in elements {
            guard case .column(let qualifier, let name) = element.expression.kind, qualifier == nil else {
                throw JedError("0A000", "exclusion expressions must be bare columns")
            }
            guard let column = table.columns.firstIndex(where: { $0.name == name }) else { throw JedError("42703", "column \(name) named in key does not exist") }
            if kind == 2, resolved.contains(where: { $0.column == column }) { throw JedError("42701", "column \(name) appears twice in exclusion constraint") }
            let type = try table.columns[column].type.resolve(snapshot.types)
            let strategy: UInt8
            if kind == 0 {
                try KeyEncoding.validateType(type.type)
                guard element.op == "=" else { throw JedError("42809", "operator \(element.op) is not a commutative member of the btree operator family") }
                strategy = 1
            } else {
                switch element.op {
                case "&&":
                    guard case .range = type.type else { throw JedError("42704", "type \(type.type.name) has no GiST operator class accepting &&") }
                    _ = try GistOpclass(type: type); strategy = 0
                case "=":
                    if case .range = type.type { throw JedError("42704", "range has no GiST operator class accepting =") }
                    _ = try GistOpclass(type: type); strategy = 1
                default: throw JedError("0A000", "exclusion constraint operator \(element.op) is not supported yet")
                }
            }
            resolved.append(.init(column: column, strategy: strategy))
        }
        var stored: CatalogExpression?, dependencies = IndexDependencies.Facts()
        if let predicate {
            try CatalogExpressionPolicy.validate(predicate, isDefault: false)
            let resolver = try resolver(snapshot: snapshot, settings: settings, context: context, table: table)
            resolver.storageCollation = true
            let expression = try resolver.resolve(predicate, expected: TypeDescriptor(.scalar(.boolean)))
            guard expression.type == nil || expression.type?.type == .scalar(.boolean) else { throw JedError("42804", "argument of WHERE must be boolean") }
            dependencies = try IndexDependencies.resolve([expression], environment: settings.environment)
            stored = try CatalogSQL.expression(predicate, source: source)
        }
        return ExclusionDefinition(kind: kind, elements: resolved, predicate: stored, dependencies: dependencies)
    }
}
