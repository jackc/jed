public struct ResultColumn: Sendable {
    public let name: SQLText
    /// nil is SQL's unresolved all-NULL type (reported as `unknown`).
    public let type: TypeDescriptor?
}

public struct CommandTag: Sendable {
    public let command: String
    public let rowsAffected: Int64
    public let cost: Int64
}

/// A serialized pull cursor. Each successful pull transfers one result row to
/// the caller. Closing, exhaustion, or a terminal error releases every pin.
public final class Rows {
    public let columns: [ResultColumn]
    private let meter: CostMeter
    private let guardCostOnPull: Bool
    private var pull: (() throws -> [SQLValue]?)?
    private var release: (() -> Void)?
    private var failure: ((Error) -> Void)?
    private let command: String
    private let affected: Int64?
    private var rowCount: Int64 = 0
    public private(set) var error: JedError?
    public var cost: Int64 { meter.accrued }
    public var isClosed: Bool { pull == nil }
    public var commandTag: CommandTag { CommandTag(command: command, rowsAffected: affected ?? rowCount, cost: cost) }

    internal init(columns: [ResultColumn], meter: CostMeter, command: String = "SELECT", affected: Int64? = nil,
                  guardCostOnPull: Bool = true,
                  pull: @escaping () throws -> [SQLValue]?, release: @escaping () -> Void = {},
                  failure: @escaping (Error) -> Void = { _ in }) {
        self.columns = columns; self.meter = meter; self.command = command; self.affected = affected
        self.guardCostOnPull = guardCostOnPull
        self.pull = pull; self.release = release
        self.failure = failure
    }
    public func next() throws -> [SQLValue]? {
        guard let pull else { return nil }
        do {
            if guardCostOnPull { try meter.checkpoint() } else { try meter.poll() }
            guard let row = try pull() else { close(); return nil }
            rowCount = saturatingAdd(rowCount, 1)
            return row
        } catch {
            terminate(error); throw error
        }
    }
    /// Engine-owned collection participates in Q1. Rows already returned by
    /// next() are host-owned and are outside the engine account.
    public func collect() throws -> [[SQLValue]] {
        var result: [[SQLValue]] = [], reserved: Int64 = 0
        defer { meter.resources.query.release(reserved) }
        do {
            while let row = try next() {
                let bytes = MemorySizes.row(row)
                try meter.reserveQuery(bytes); reserved = saturatingAdd(reserved, bytes)
                result.append(row)
            }
            return result
        } catch { terminate(error); throw error }
    }
    public func finish() throws -> CommandTag {
        while try next() != nil {}
        return commandTag
    }
    public func close() {
        pull = nil
        failure = nil
        let cleanup = release; release = nil; cleanup?()
    }
    private func terminate(_ error: Error) {
        // next() may already have terminated the cursor before collect() sees
        // its error. The callback is consumed by close and runs at most once.
        self.error = error as? JedError
        failure?(error)
        close()
    }
    deinit { close() }
}
