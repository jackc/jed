import Foundation

/// Native host waiter only. The foreground and idle paths call the same pager
/// policy; this object owns no database generation or durability decisions.
internal final class BatchFlusher: @unchecked Sendable {
    private weak var core: DatabaseCore?
    private let condition = NSCondition()
    private var stopped = false, finished = false
    private let failed: @Sendable () -> Void
    init(core: DatabaseCore, failed: @escaping @Sendable () -> Void) {
        self.core = core; self.failed = failed
        Thread.detachNewThread { self.run() }
    }
    func stop() {
        condition.lock(); stopped = true; condition.broadcast()
        while !finished { condition.wait() }
        condition.unlock()
    }
    private func wait(_ seconds: Double) -> Bool {
        condition.lock(); defer { condition.unlock() }
        if !stopped { _ = condition.wait(until: Date(timeIntervalSinceNow: seconds)) }
        return !stopped
    }
    private func run() {
        var lastPager: ObjectIdentifier?, generation: UInt64 = 0
        var wake: ContinuousClock.Instant?
        var failedStorage = false
        while wait(0.01) {
            guard let core else { break }
            let storage = core.storage, pager = storage.pager, facts = pager.idleFlushState
            guard facts.deadline != nil, facts.interval > 0 else { wake = nil; continue }
            if lastPager != ObjectIdentifier(pager) || generation != facts.generation || wake == nil {
                lastPager = ObjectIdentifier(pager); generation = facts.generation
                // Starting a batch waits the full interval without reading its
                // clock a second time, including on an advancing clock source.
                wake = ContinuousClock.now.advanced(by: .microseconds(facts.interval))
            }
            guard ContinuousClock.now >= wake! else { continue }
            do {
                if let remaining = try core.idleBatchFlush(expected: storage) {
                    wake = ContinuousClock.now.advanced(by: .microseconds(max(1000, remaining)))
                } else { wake = nil }
            } catch let error as JedError where error.code == "55006" {
                wake = ContinuousClock.now.advanced(by: .milliseconds(50))
            } catch {
                failedStorage = pager.isPoisoned
                break
            }
        }
        // Mark exit before invoking the host callback: disposing a failed role
        // may call stop() on this very worker.
        condition.lock(); finished = true; condition.broadcast(); condition.unlock()
        if failedStorage { failed() }
    }
}
