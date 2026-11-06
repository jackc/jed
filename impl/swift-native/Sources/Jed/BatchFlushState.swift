/// Per-file deadline policy, protected by the pager's state lock. All readings
/// use the sanctioned clock source; wall time is used only by the host waiter.
internal struct BatchFlushState: Sendable {
    var clock: (any ClockSource)?
    var interval: Int64 = 0
    var deadline: Int64?
    var preparedReading: Int64?
    var preparedEmpty = false
    var generation: UInt64 = 0
    var enabled: Bool { interval > 0 && clock != nil }
    mutating func configure(clock: any ClockSource, milliseconds: UInt64) {
        self.clock = clock
        interval = milliseconds > UInt64(Int64.max / 1000) ? Int64.max : Int64(milliseconds) * 1000
        clear()
    }
    mutating func clear() { deadline = nil; preparedReading = nil; preparedEmpty = false }
    mutating func begin(at reading: Int64) {
        let (sum, overflow) = reading.addingReportingOverflow(interval)
        deadline = overflow ? Int64.max : sum; generation &+= 1
    }
}
