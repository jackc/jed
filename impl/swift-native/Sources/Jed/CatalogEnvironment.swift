/// Opening preserves stored definitions and stale index pins. Only an entirely
/// absent referenced collation prevents opening; dependency skew is enforced at
/// the operation that would use or maintain the affected object.
internal enum CatalogEnvironment {
    static func validate(_ image: CatalogImage, environment: Environment) throws {
        for pin in image.collations.sorted(by: { $0.name < $1.name }) { _ = try verdict(pin, environment: environment) }
    }

    /// False is a readable but stale pin. Missing loaded data is the stronger
    /// whole-open refusal and must be diagnosed before statistics key checks.
    static func verdict(_ pin: CatalogCollation, environment: Environment) throws -> Bool {
        guard let loaded = environment.unicode.loadedCollations.first(where: { $0.name == pin.name }) else {
            throw JedError("XX002", "referenced collation \"\(pin.name)\" is not loaded")
        }
        return pin.unicode == loaded.unicodeVersion && pin.cldr == loaded.cldrVersion
    }

    static func collationMatches(_ name: SQLText?, image: CatalogImage, environment: Environment) -> Bool {
        guard let name, name.utf8 != [67] else { return true }
        guard let pin = image.collations.first(where: { $0.name == name }),
              let loaded = environment.unicode.loadedCollations.first(where: { $0.name == name }) else { return false }
        return pin.unicode == loaded.unicodeVersion && pin.cldr == loaded.cldrVersion
    }

    private static func checkCollation(_ name: SQLText?, image: CatalogImage, environment: Environment) throws {
        guard !collationMatches(name, image: image, environment: environment) else { return }
        let pin = image.collations.first { $0.name == name }
        let loaded = environment.unicode.loadedCollations.first { $0.name == name }
        throw JedError("XX002", "collation \"\(name?.description ?? "")\" version mismatch: stored \(pin?.unicode.description ?? "missing")/\(pin?.cldr.description ?? "missing"), loaded \(loaded?.unicodeVersion.description ?? "missing")/\(loaded?.cldrVersion.description ?? "missing")")
    }

    /// CHECK dependencies deliberately do not belong here: DELETE never
    /// evaluates checks, and a missing CHECK function has ordinary 42883.
    static func checkTableCollations(_ table: CatalogTable, image: CatalogImage, environment: Environment) throws {
        for column in table.columns { try checkCollation(column.type.collation, image: image, environment: environment) }
    }

    static func checkWritable(_ table: CatalogTable, image: CatalogImage, environment: Environment) throws {
        try checkTableCollations(table, image: image, environment: environment)
        for index in table.indexes.sorted(by: { $0.name < $1.name }) {
            try checkIndex(index, table: table, image: image, environment: environment)
        }
    }

    static func checkIndex(_ index: CatalogIndex, table: CatalogTable, image: CatalogImage, environment: Environment) throws {
        try IndexDependencies.validateZones(index, environment: environment)
        // Resolve only definition metadata; this never evaluates a stored row or
        // calls a host kernel. Fresh nominal IDs cannot escape this check.
        let types = try image.resolvedTypes(catalogID: 0)
        let columns = try table.columns.enumerated().map {
            ExpressionColumn(name: $0.element.name, qualifier: table.name, type: try $0.element.type.resolve(types), index: $0.offset)
        }
        let resolver = ExpressionResolver(columns: columns, composites: types)
        resolver.hostFunctions = environment.signatures; resolver.unicode = environment.unicode; resolver.storageCollation = true
        var expressions: [ResolvedExpression] = []
        for key in index.keys {
            switch key {
            case .column(let ordinal):
                guard columns.indices.contains(ordinal) else { throw JedError.corruption("invalid index key column") }
                try checkCollation(columns[ordinal].type.collation, image: image, environment: environment)
            case .expression(let definition):
                guard let syntax = definition.parsed else { throw JedError("XX003", "stored index expression cannot be parsed") }
                let expression = try resolver.resolve(syntax)
                try checkCollation(ExpressionCollation.derive(expression, storage: true).orderingName(), image: image, environment: environment)
                expressions.append(expression)
            }
        }
        if let definition = index.predicate {
            guard let syntax = definition.parsed else { throw JedError("XX003", "stored index predicate cannot be parsed") }
            expressions.append(try resolver.resolve(syntax, expected: TypeDescriptor(.scalar(.boolean))))
        }
        try StoredExpressionDependencies.walk(expressions) { expression in
            try checkCollation(expression.collation?.name, image: image, environment: environment)
        }
        try StoredExpressionDependencies.validateHosts(index.hosts, expressions: expressions, environment: environment)
    }
}
