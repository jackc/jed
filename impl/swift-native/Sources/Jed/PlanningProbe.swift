/// One plan/bind boundary's observations. A probe hides parameter payloads;
/// NULL-only tests remain distinct so ordinary non-NULL key lookups can reuse
/// their generic plan. A custom frame exposes already-admitted values.
internal final class PlanningProbe {
    private var values: [SQLValue]?
    private let isCustom: Bool
    private var ended = false
    private(set) var sensitive = false
    private(set) var nullRead = false
    var isActive: Bool { !ended }

    init() { values = nil; isCustom = false }
    init(values: [SQLValue]) { self.values = values; isCustom = true }
    static func probe() -> PlanningProbe { PlanningProbe() }
    static func custom(_ values: [SQLValue]) -> PlanningProbe { PlanningProbe(values: values) }
    static func custom(values: [SQLValue]) -> PlanningProbe { PlanningProbe(values: values) }

    /// Parameter numbers are one-based, like ResolvedExpression.parameter.
    func value(_ number: Int) -> SQLValue? {
        guard !ended, number > 0 else { return nil }
        guard let values else { sensitive = true; return nil }
        guard values.indices.contains(number - 1) else { return nil }
        let value = values[number - 1]
        if case .composite = value { return nil }
        return value
    }
    func isNull(_ number: Int) -> Bool {
        guard !ended, number > 0 else { return false }
        guard let values else { nullRead = true; return false }
        return values.indices.contains(number - 1) && values[number - 1].isNull
    }
    func needsCustom(_ bound: [SQLValue]) -> Bool {
        !isCustom && (sensitive || nullRead && bound.contains(where: \.isNull))
    }
    func end() {
        ended = true
        // Cached observations never retain an execution's bound values.
        values = nil
    }

    static func parameter(_ number: Int, parameters: [SQLValue]?, probe: PlanningProbe?) -> SQLValue? {
        if let probe, probe.isActive { return probe.value(number) }
        guard let parameters, parameters.indices.contains(number - 1) else { return nil }
        let value = parameters[number - 1]
        if case .composite = value { return nil }
        return value
    }
    /// Shape admission must precede reads in paired proofs. In particular an
    /// outer column, call, or folded subquery never authorizes reading its peer.
    static func literal(_ expression: ResolvedExpression) -> Bool {
        switch expression.node {
        case .constant(.composite): return false
        case .constant, .parameter: return true
        case .convert(let child, let target):
            return child.type?.type == target.type && literal(child)
        default: return false
        }
    }
    static func known(_ expression: ResolvedExpression, parameters: [SQLValue]?, probe: PlanningProbe?) -> SQLValue? {
        switch expression.node {
        case .constant(.composite): return nil
        case .constant(let value): return value
        case .parameter(let number): return parameter(number, parameters: parameters, probe: probe)
        case .convert(let child, let target):
            return child.type?.type == target.type ? known(child, parameters: parameters, probe: probe) : nil
        default: return nil
        }
    }
    static func isNull(_ expression: ResolvedExpression, parameters: [SQLValue]?, probe: PlanningProbe?) -> Bool {
        switch expression.node {
        case .constant(let value): return value.isNull
        case .parameter(let number):
            if let probe, probe.isActive { return probe.isNull(number) }
            return parameters.map { $0.indices.contains(number - 1) && $0[number - 1].isNull } ?? false
        case .convert(let child, let target):
            return child.type?.type == target.type && isNull(child, parameters: parameters, probe: probe)
        default: return false
        }
    }
}
