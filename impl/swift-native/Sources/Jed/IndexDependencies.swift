/// Admission follows operations, never a blanket ban on a mentioned type.
/// Children are always visited, including unselected CASE arms and NULL tests.
internal enum IndexDependencies {
    struct Facts {
        var hosts: [CatalogHostDependency] = []
        var zones: [CatalogZoneDependency] = []
        var dynamicZones = false
    }
    static func containsInstant(_ type: SQLType?) -> Bool {
        switch type {
        case .scalar(.timestamptz): true
        case .array(let element), .range(let element): containsInstant(element)
        case .composite(let value): value.fields.contains { containsInstant($0.descriptor.type) }
        case .record(let fields): fields.contains { containsInstant($0.descriptor.type) }
        default: false
        }
    }
    static func resolve(_ expressions: [ResolvedExpression], environment: Environment) throws -> Facts {
        var facts = Facts(), zones: [SQLText: CatalogZoneDependency] = [:]
        func nonimmutable() -> JedError { JedError("42P17", "functions in index expression must be marked IMMUTABLE") }
        func pin(_ zone: SQLTimeZone) throws {
            guard let version = zone.dataVersion, let checksum = zone.contentHash else { return }
            guard !version.utf8.isEmpty else { throw JedError("42P17", "named timezone data has no pinnable version") }
            guard version.utf8.count <= UInt16.max, zone.name.utf8.count <= UInt16.max else { throw JedError("54000", "timezone dependency exceeds catalog length limit") }
            zones[zone.name] = CatalogZoneDependency(name: zone.name, version: version, checksum: checksum)
        }
        func explicitZone(_ value: ResolvedExpression, constructor: Bool = false) throws {
            switch value.node {
            case .constant(.null), .constant(.interval): return
            case .constant(.text(let name)):
                try pin(constructor ? environment.timezones.resolveConstructor(name) : environment.timezones.resolve(name))
            default:
                facts.dynamicZones = true
                for zone in environment.timezones.zones { try pin(zone) }
            }
        }
        try StoredExpressionDependencies.walk(expressions) { expression in
            if expression.nonimmutable { throw nonimmutable() }
            switch expression.node {
            case .currentTimestamp, .currentDate, .dateInput, .subquery, .outerColumn: throw nonimmutable()
            case .cast(let source, let target, _), .convert(let source, let target):
                let from = source.type?.type, to = target.type
                if from == .scalar(.text) && ([.scalar(.date), .scalar(.timestamp), .scalar(.interval)].contains(to) || containsInstant(to)) { throw nonimmutable() }
                if to == .scalar(.text) && containsInstant(from) { throw nonimmutable() }
                if from == .scalar(.timestamptz) && [.scalar(.date), .scalar(.timestamp)].contains(to)
                    || to == .scalar(.timestamptz) && [.scalar(.date), .scalar(.timestamp)].contains(from) { throw nonimmutable() }
            case .binary(let op, let a, let b, _) where ["+", "-"].contains(op):
                if a.type?.type == .scalar(.timestamptz) && b.type?.type == .scalar(.interval)
                    || b.type?.type == .scalar(.timestamptz) && a.type?.type == .scalar(.interval) {
                    let interval = a.type?.type == .scalar(.interval) ? a : b
                    guard case .constant(.interval(let value)) = interval.node, value.months == 0, value.days == 0 else { throw nonimmutable() }
                }
            case .call(let call):
                if call.hostID != nil { break } // declaration + component gate below
                let args = call.arguments
                if ["extract", "date_part"].contains(call.name), args.count == 2, args[1].type?.type == .scalar(.timestamptz) {
                    guard case .constant(.text(let field)) = args[0].node, field.asciiLowercased.description == "epoch" else { throw nonimmutable() }
                } else if call.name == "timezone", args.count == 2 { try explicitZone(args[0]) }
                else if call.name == "date_trunc", args.count == 3 { try explicitZone(args[2]) }
                else if call.name == "make_timestamptz", args.count == 7 { try explicitZone(args[6], constructor: true) }
                else if FunctionResolver.text(call.metadata.metadata, "volatility").map({ $0 != "immutable" }) ?? false { throw nonimmutable() }
                if ["to_json", "to_jsonb", "array_to_json", "row_to_json", "json_scalar", "json_build_array", "jsonb_build_array", "json_build_object", "jsonb_build_object"].contains(call.name),
                   args.contains(where: { containsInstant($0.type?.type) }) { throw nonimmutable() }
            case .json(let value):
                if ["json_scalar", "json_array", "json_object"].contains(value.name),
                   StoredExpressionDependencies.children(expression).contains(where: { containsInstant($0.type?.type) }) { throw nonimmutable() }
            default: break
            }
        }
        facts.hosts = try StoredExpressionDependencies.hosts(expressions, environment: environment, immutable: true)
        guard zones.count <= UInt16.max else { throw JedError("54000", "too many timezone dependencies") }
        facts.zones = zones.values.sorted { $0.name < $1.name }
        return facts
    }
    static func validateZones(_ index: CatalogIndex, environment: Environment) throws {
        let loaded = environment.timezones.zones
        if index.dynamicZones && loaded.count != index.zones.count { throw mismatch() }
        for dependency in index.zones {
            guard let zone = loaded.first(where: { $0.name == dependency.name }), zone.dataVersion == dependency.version,
                  zone.contentHash == dependency.checksum else { throw mismatch() }
        }
    }
    private static func mismatch() -> JedError { JedError("XX002", "stored index timezone dependency does not match the environment") }
}
