/// Repeatable source-order rows. The first reader seals the append phase;
/// subsequent readers independently replay the same immutable stage.
internal final class RowSpool {
    private let config: SpillConfig
    private let charge: SpillCharge
    private var rows: [[SQLValue]] = []
    private var file: SpillRowFile?
    private var sealed = false, closed = false, failed = false, transferred = false
    private var readers = 0
    private(set) var count: Int64 = 0
    private(set) var residentBytes: Int64 = 0
    var isSpilled: Bool { file != nil }
    init(config: SpillConfig) { self.config = config; charge = SpillCharge(meter: config.meter) }
    func append(_ row: [SQLValue]) throws {
        guard !sealed, !closed, !failed else { throw JedError("58030", "row spool no longer accepts rows") }
        guard count < Int64.max else { throw JedError("58030", "row spool is too large") }
        do {
            if let file { try file.append(row) }
            else {
                let bytes = MemorySizes.row(row)
                let admitted: Bool
                if config.canSpill { admitted = charge.tryReserve(bytes) }
                else { try charge.reserve(bytes); admitted = true }
                let nextBytes = saturatingAdd(residentBytes, bytes)
                if config.canSpill && (!admitted || nextBytes > config.workMemoryBytes) {
                    let target = try SpillRowFile(host: config.host!, kind: "rows")
                    do {
                        for old in rows { try target.append(old) }
                        try target.append(row)
                    } catch { try? target.close(); throw error }
                    file = target; rows = []; residentBytes = 0; charge.releaseAll()
                } else { rows.append(row); residentBytes = nextBytes }
            }
            count += 1
        } catch { failed = true; throw error }
    }
    func cursor() throws -> RowSpoolCursor {
        guard !closed, !failed, !transferred else { throw JedError("58030", "row spool is closed or transferred") }
        sealed = true; readers += 1
        return RowSpoolCursor(owner: self, file: file?.cursor(), owning: false)
    }
    func takeOutput() throws -> RowSpoolCursor {
        guard !closed, !failed, !transferred, readers == 0 else {
            throw JedError("58030", "row spool output already borrowed or transferred")
        }
        sealed = true; transferred = true; readers += 1
        return RowSpoolCursor(owner: self, file: file?.cursor(), owning: true)
    }
    fileprivate func row(at position: Int64) throws -> [SQLValue]? {
        guard !closed, !failed else { throw JedError("58030", "row spool is closed or unusable") }
        guard position < count else { return nil }
        return rows[Int(position)]
    }
    fileprivate func checkOpen() throws {
        guard !closed, !failed else { throw JedError("58030", "row spool is closed or unusable") }
    }
    fileprivate func releaseReader() { precondition(readers > 0); readers -= 1 }
    func releaseCharge() { sealed = true; charge.releaseAll() }
    func close() {
        guard !closed else { return }; closed = true
        try? file?.close(); file = nil; rows = []; residentBytes = 0; charge.releaseAll()
    }
    deinit { close() }
}

internal final class RowSpoolCursor {
    private var owner: RowSpool?
    private var file: SpillRowFile.Cursor?
    private let owning: Bool
    private var position: Int64 = 0
    fileprivate init(owner: RowSpool, file: SpillRowFile.Cursor?, owning: Bool) {
        self.owner = owner; self.file = file; self.owning = owning
    }
    func next() throws -> [SQLValue]? {
        guard let owner else { return nil }
        do {
            try owner.checkOpen()
            let row: [SQLValue]?
            if let file { row = try file.next() } else { row = try owner.row(at: position) }
            if let row { position += 1; return row }
            close(); return nil
        } catch { close(); throw error }
    }
    func close() {
        file?.close(); file = nil
        let prior = owner; owner = nil
        prior?.releaseReader()
        if owning { prior?.close() }
    }
    deinit { close() }
}
