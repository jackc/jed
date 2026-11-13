/// Counts participate in statement-wide inference and custom planning.
internal enum RowCountBound {
    static func resolve(_ spelling: String?, code: String, resolver: ExpressionResolver,
                        settings: SessionSettings) throws -> Int64? {
        guard let spelling else { return nil }
        let number: Int64
        if spelling.hasPrefix("$"), let index = Int(spelling.dropFirst()) {
            resolver.parameterUses.insert(index)
            let integer = TypeDescriptor(.scalar(.i64))
            if let previous = resolver.parameters[index],
               case .scalar(let scalar) = previous.type, scalar.integerWidth != nil {} else if resolver.parameters[index] != nil {
                throw JedError("42804", "inconsistent types inferred for parameter $\(index)")
            }
            resolver.parameters[index] = integer
            guard let value = PlanningProbe.parameter(index, parameters: settings.planningParameters, probe: settings.planningProbe),
                  !value.isNull else { return nil }
            guard case .integer(let bound) = value else { throw JedError("42804", "row count must be an integer") }
            number = bound
        } else {
            guard let value = try? TypeRules.unsignedLiteral(spelling), value <= UInt64(Int64.max) else {
                throw JedError(code, "invalid row-count bound")
            }
            number = Int64(value)
        }
        guard number >= 0 else { throw JedError(code, "invalid row-count bound") }
        return number
    }
}
