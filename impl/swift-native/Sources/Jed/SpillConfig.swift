/// One operator's frozen host policy and the statement's shared query account.
/// Scratch I/O never contributes new SQL cost units.
internal struct SpillConfig {
    let workMemoryBytes: Int64
    let host: (any ScratchFileHost)?
    let meter: CostMeter
    var canSpill: Bool { host != nil && workMemoryBytes > 0 }
    init(workMemoryBytes: Int64, host: (any ScratchFileHost)?, meter: CostMeter) {
        self.workMemoryBytes = max(0, workMemoryBytes); self.host = host; self.meter = meter
    }
}

/// Explicit ownership of successful reservations only. A rejected spillable
/// element is written with the resident prefix but never charged.
internal final class SpillCharge {
    private let meter: CostMeter
    private(set) var bytes: Int64 = 0
    init(meter: CostMeter) { self.meter = meter }
    func tryReserve(_ amount: Int64) -> Bool {
        guard meter.resources.query.tryReserve(amount) else { return false }
        bytes = saturatingAdd(bytes, amount); return true
    }
    func reserve(_ amount: Int64) throws {
        try meter.reserveQuery(amount); bytes = saturatingAdd(bytes, amount)
    }
    func release(_ amount: Int64) {
        precondition(amount >= 0 && amount <= bytes)
        meter.resources.query.release(amount); bytes -= amount
    }
    func releaseAll() { meter.resources.query.release(bytes); bytes = 0 }
    deinit { releaseAll() }
}
