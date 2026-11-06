/// Identities belong to immutable planned query frames. Copying a plan for a
/// driver preserves its identity; a separately resolved query gets a new one.
internal final class QueryProfileIdentity {}

internal final class ExecutionProfile {
    enum Stage: Int, CaseIterable { case source, folded, filter, aggregate, window, sort, distinct, output, total }
    private struct Key: Hashable { let owner: ObjectIdentifier; let stage: Stage }
    private var costs: [Key: Int64] = [:]
    func add(_ owner: AnyObject, _ stage: Stage, _ units: Int64) {
        precondition(units >= 0)
        let key = Key(owner: ObjectIdentifier(owner), stage: stage)
        costs[key] = saturatingAdd(costs[key] ?? 0, units)
    }
    func value(_ owner: AnyObject, _ stage: Stage) -> Int64 {
        costs[Key(owner: ObjectIdentifier(owner), stage: stage)] ?? 0
    }
    func cumulative(_ owner: AnyObject, through stage: Stage) -> Int64 {
        Stage.allCases.filter { $0.rawValue <= stage.rawValue && $0 != .total }.reduce(0) { saturatingAdd($0, value(owner, $1)) }
    }
    func measure<T>(_ owner: AnyObject, _ stage: Stage, meter: CostMeter, _ body: () throws -> T) rethrows -> T {
        let start = meter.accrued
        defer { add(owner, stage, max(0, meter.accrued - start)) }
        return try body()
    }
    func measure<T>(_ owner: AnyObject, _ stage: Stage, meter: CostMeter, excluding: [Stage], _ body: () throws -> T) rethrows -> T {
        let start = meter.accrued, before = excluding.reduce(Int64(0)) { saturatingAdd($0, value(owner, $1)) }
        defer {
            let after = excluding.reduce(Int64(0)) { saturatingAdd($0, value(owner, $1)) }
            add(owner, stage, max(0, meter.accrued - start - (after - before)))
        }
        return try body()
    }
}

extension ExpressionContext {
    func profile<T>(_ owner: AnyObject, _ stage: ExecutionProfile.Stage, _ body: () throws -> T) rethrows -> T {
        guard let executionProfile else { return try body() }
        return try executionProfile.measure(owner, stage, meter: meter, body)
    }
    func profile<T>(_ owner: AnyObject, _ stage: ExecutionProfile.Stage, excluding: [ExecutionProfile.Stage], _ body: () throws -> T) rethrows -> T {
        guard let executionProfile else { return try body() }
        return try executionProfile.measure(owner, stage, meter: meter, excluding: excluding, body)
    }
}
