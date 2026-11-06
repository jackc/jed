import Synchronization

/// One token per operation, retained by its cursors. Cancellation is best effort;
/// it never changes deterministic costs or substitutes for their guards.
public final class CancellationToken: Sendable {
    private let cancelled = Atomic<Bool>(false)
    public init() {}
    public func cancel() { cancelled.store(true, ordering: .relaxed) }
    public var isCancelled: Bool { cancelled.load(ordering: .relaxed) }
}

/// Shared instrumentation for accounting tests. These are observations, never
/// admission inputs. Engine accounts remain owned by a serialized operation.
public final class MemoryDiagnostics: Sendable {
    public static let shared = MemoryDiagnostics()
    private let underflowCount = Atomic<UInt64>(0)
    private let highestBalance = Atomic<Int64>(0)
    public init() {}
    public var underflows: UInt64 { underflowCount.load(ordering: .relaxed) }
    public var peak: Int64 { highestBalance.load(ordering: .relaxed) }
    public func resetPeak() { highestBalance.store(0, ordering: .relaxed) }
    internal func recordUnderflow() { _ = underflowCount.wrappingAdd(1, ordering: .relaxed) }
    internal func recordBalance(_ value: Int64) {
        var previous = highestBalance.load(ordering: .relaxed)
        while previous < value {
            let result = highestBalance.compareExchange(expected: previous, desired: value, ordering: .relaxed)
            if result.exchanged { return }
            previous = result.original
        }
    }
}

internal final class LifetimeCost {
    var accrued: Int64
    var limit: Int64
    init(limit: Int64 = 0, accrued: Int64 = 0) {
        self.limit = max(0, limit)
        self.accrued = max(0, accrued)
    }
}

internal func saturatingAdd(_ lhs: Int64, _ rhs: Int64) -> Int64 {
    precondition(lhs >= 0 && rhs >= 0, "logical counts cannot be negative")
    return rhs > Int64.max - lhs ? Int64.max : lhs + rhs
}

/// Live logical bytes. Pending writes form a floor across statement-owned
/// releases; a rejected reservation never modifies the balance.
internal final class QueryMemoryAccount {
    let limit: Int64
    let diagnostics: MemoryDiagnostics
    private(set) var used: Int64 = 0
    private(set) var floor: Int64 = 0
    var active: Bool { limit > 0 }

    init(limit: Int64 = 0, diagnostics: MemoryDiagnostics = .shared) {
        self.limit = max(0, limit)
        self.diagnostics = diagnostics
    }

    func tryReserve(_ bytes: Int64) -> Bool {
        precondition(bytes >= 0)
        guard active else { return true }
        guard bytes <= limit - used else { return false }
        used += bytes
        diagnostics.recordBalance(used)
        return true
    }

    func reserve(_ bytes: Int64) throws {
        guard tryReserve(bytes) else {
            throw JedError("54P05", "query memory exceeded the limit of \(limit) bytes")
        }
    }

    func release(_ bytes: Int64) {
        precondition(bytes >= 0)
        guard active else { return }
        if bytes > used - floor {
            diagnostics.recordUnderflow()
            used = floor
        } else {
            used -= bytes
        }
    }

    func syncPending(_ pending: Int64) throws {
        precondition(pending >= 0)
        guard active else { return }
        if pending > floor { try reserve(pending - floor) }
        else { used -= floor - pending }
        floor = pending
    }
}

/// All nested/scratch meters and a frozen cursor share one instance. A new
/// statement creates a fresh instance; it never resets a prior cursor's balance.
internal final class StatementResources {
    let query: QueryMemoryAccount
    let scalarLimit: Int64
    private(set) var scalarUsed: Int64 = 0

    init(queryLimit: Int64 = 0, scalarLimit: Int64 = 0,
         diagnostics: MemoryDiagnostics = .shared) {
        self.query = QueryMemoryAccount(limit: queryLimit, diagnostics: diagnostics)
        self.scalarLimit = scalarLimit > 0 ? scalarLimit : GeneratedSpec.defaultScalarBytes
    }

    func reserveScalar(_ bytes: Int64) throws {
        precondition(bytes >= 0)
        guard bytes <= scalarLimit - scalarUsed else {
            throw JedError("54P04", "scalar allocations exceeded the limit of \(scalarLimit) bytes")
        }
        scalarUsed += bytes
    }
}

internal final class CostMeter {
    private enum Ceiling { case statement, lifetime }
    private var firstCeiling: Ceiling?
    private var cancellationSteps: UInt8 = 0
    private(set) var accrued: Int64 = 0
    let limit: Int64
    let lifetime: LifetimeCost?
    let lifetimeLimit: Int64
    let resources: StatementResources
    let cancellation: CancellationToken?
    /// Other domains' pending writes stay resident while one target mutation
    /// updates its own reservation. The statement coordinator resets this to
    /// zero before synchronizing the complete working-root set.
    var pendingDomainBase: Int64 = 0
    var pendingDomainExcluded = false

    init(limit: Int64 = 0, lifetime: LifetimeCost? = nil,
         resources: StatementResources = StatementResources(), cancellation: CancellationToken? = nil) {
        self.limit = max(0, limit)
        self.lifetime = lifetime
        self.lifetimeLimit = max(0, lifetime?.limit ?? 0)
        self.resources = resources
        self.cancellation = cancellation
    }

    func charge(_ units: Int64) {
        precondition(units >= 0)
        if firstCeiling == nil {
            let statementRemaining = limit > 0 ? limit - accrued : Int64.max
            let lifetimeRemaining = lifetime.map { lifetimeLimit > 0 ? lifetimeLimit - $0.accrued : Int64.max } ?? Int64.max
            let statementHit = limit > 0 && units >= statementRemaining
            let lifetimeHit = lifetime != nil && lifetimeLimit > 0 && units >= lifetimeRemaining
            if statementHit { firstCeiling = .statement }
            if lifetimeHit && (!statementHit || lifetimeRemaining < statementRemaining) { firstCeiling = .lifetime }
        }
        accrued = saturatingAdd(accrued, units)
        if let lifetime { lifetime.accrued = saturatingAdd(lifetime.accrued, units) }
    }

    func guardCost() throws {
        let statementOver = limit > 0 && accrued >= limit
        let lifetimeOver = lifetime != nil && lifetimeLimit > 0 && lifetime!.accrued >= lifetimeLimit
        guard statementOver || lifetimeOver else { return }
        var useLifetime = lifetimeOver
        if statementOver && lifetimeOver, let lifetime {
            useLifetime = lifetime.accrued - lifetimeLimit > accrued - limit
        }
        if let firstCeiling { useLifetime = firstCeiling == .lifetime }
        if useLifetime, let lifetime {
            throw JedError("54P02", "session exceeded the lifetime cost limit of \(lifetimeLimit) (accrued \(lifetime.accrued))")
        }
        throw JedError("54P01", "query exceeded the cost limit of \(limit) (accrued \(accrued))")
    }

    func poll() throws {
        if cancellation?.isCancelled == true { throw JedError("57014", "query canceled") }
    }

    func checkpoint() throws { try poll(); try guardCost() }

    func pollStep() throws {
        if cancellation != nil {
            let sample = cancellationSteps == 0
            cancellationSteps &+= 1
            if sample { try poll() }
        }
    }

    func step() throws { try pollStep(); try guardCost() }

    func reserveScalar(_ bytes: Int64) throws {
        // Amplifier callers charge and guard cost before allocation admission.
        try resources.reserveScalar(bytes)
    }

    func reserveQuery(_ bytes: Int64) throws {
        if !resources.query.tryReserve(bytes) {
            try guardCost()
            throw JedError("54P05", "query memory exceeded the limit of \(resources.query.limit) bytes")
        }
    }

    func syncPending(_ pending: Int64) throws {
        do { try resources.query.syncPending(saturatingAdd(pendingDomainBase, pendingDomainExcluded ? 0 : pending)) }
        catch { try guardCost(); throw error }
    }
}
