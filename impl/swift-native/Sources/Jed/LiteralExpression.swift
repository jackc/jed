/// Named composite input is represented by typed ROW construction. Each
/// present nested record owns its normal operator charge; array and range
/// input remain folded values, including composites nested inside arrays.
internal enum LiteralExpression {
    static func make(_ value: SQLValue, type: TypeDescriptor, position: Int,
                     nonimmutable: Bool = false) -> ResolvedExpression {
        let node: ResolvedExpression.Node
        if case .composite(let composite) = type.type, case .composite(let values) = value,
           composite.fields.count == values.count {
            node = .row(zip(values, composite.fields).map {
                make($0, type: $1.descriptor, position: position)
            })
        } else { node = .constant(value) }
        return ResolvedExpression(type: type, node: node, position: position, nonimmutable: nonimmutable)
    }
}
