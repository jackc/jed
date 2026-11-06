/// Body and relation pushdown are mutually recursive in the logical plan.
/// Retain their continuations on the heap so expanded views do not consume a
/// native stack frame for every derived body and predicate level.
internal final class PushdownWork {
    typealias Step = (PushdownWork) throws -> Void
    private var pending: [Step] = []
    let parameters: [SQLValue]?
    let planningProbe: PlanningProbe?
    init(parameters: [SQLValue]?, planningProbe: PlanningProbe? = nil) {
        self.parameters = parameters; self.planningProbe = planningProbe
    }
    func append(_ step: @escaping Step) { pending.append(step) }
    func run() throws {
        defer { pending.removeAll() }
        while let step = pending.popLast() { try step(self) }
    }
}
internal final class PushdownValue<Value> {
    var value: Value
    init(_ value: Value) { self.value = value }
}
