/// RETURNING is materialized before publication. Its Q1 reservation transfers
/// to the cursor and is returned one row at a time as ownership reaches the host.
internal final class MutationResult {
    let command: String
    let columns: [ResultColumn]
    var affected: Int64 = 0
    private let meter: CostMeter
    private var rows: [[SQLValue]?] = []
    private var position = 0
    private var reserved: Int64 = 0
    init(command: String, columns: [ResultColumn] = [], meter: CostMeter) {
        self.command = command; self.columns = columns; self.meter = meter
    }
    func append(_ row: [SQLValue]) throws {
        let bytes = MemorySizes.row(row)
        try meter.reserveQuery(bytes); reserved = saturatingAdd(reserved, bytes)
        rows.append(row)
    }
    func next() -> [SQLValue]? {
        guard position < rows.count, let row = rows[position] else { return nil }
        rows[position] = nil; position += 1
        let bytes = MemorySizes.row(row); reserved -= bytes; meter.resources.query.release(bytes)
        return row
    }
    func close() { rows = []; meter.resources.query.release(reserved); reserved = 0 }
    func cursor(failure: @escaping (Error) -> Void = { _ in }) -> Rows {
        Rows(columns: columns, meter: meter, command: command, affected: affected,
             pull: next, release: close, failure: failure)
    }
    deinit { close() }
}
