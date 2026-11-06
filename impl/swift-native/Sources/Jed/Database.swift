import Synchronization

/// Shared storage identity. Convenience queries create a fresh session; callers
/// that need variables, budgets, or transaction state keep an explicit Session.
public final class Database: Sendable {
    private static let nextCatalog = Atomic<UInt64>(1)
    internal let core: DatabaseCore
    internal let host: (any DatabaseHostLifetime)?
    public let environment: Environment
    private let keySource: Mutex<any RandomSource>
    internal var currentKeySource: any RandomSource { keySource.withLock { $0 } }
    internal func installKeySource(_ source: any RandomSource) { keySource.withLock { $0 = source } }
    internal var keyEntropy: KeyEntropy {
        let source = currentKeySource
        return { try source.bytes(count: $0) }
    }
    private struct State { var closed = false; var storageLimit: Int64; var synchronous = true }
    private let state: Mutex<State>

    public convenience init(environment: Environment = Environment.builder().build(), pageSize: Int = 8192,
                            maxStorageBytes: Int64 = 0) throws {
        let catalog = Self.allocateCatalogID()
        let storage = try StorageDomain.create(store: MemoryBlockStore(), catalogID: catalog, pageSize: pageSize)
        self.init(core: DatabaseCore(storage: storage), environment: environment, maxStorageBytes: maxStorageBytes)
    }
    internal static func allocateCatalogID() -> UInt64 { nextCatalog.wrappingAdd(1, ordering: .relaxed).oldValue }
    internal init(core: DatabaseCore, environment: Environment, maxStorageBytes: Int64 = 0,
                  keySource: (any RandomSource)? = nil, host: (any DatabaseHostLifetime)? = nil) {
        self.core = core; self.environment = environment; self.keySource = Mutex(keySource ?? environment.newKeySource()); self.host = host
        state = Mutex(State(storageLimit: maxStorageBytes))
    }
    public var pageSize: Int { core.storage.pager.pageSize }
    public var version: UInt64 { core.version }
    public var storageBytes: UInt64 { core.storage.storageBytes }
    public var isReadOnly: Bool { core.storage.readOnly }
    public var isClosed: Bool { state.withLock { $0.closed } }
    public var maxStorageBytes: Int64 {
        get { state.withLock { $0.storageLimit } }
        set { state.withLock { $0.storageLimit = max(0, newValue) } }
    }
    public var synchronous: Bool {
        get { state.withLock { $0.synchronous } }
        set { state.withLock { $0.synchronous = newValue } }
    }
    internal func checkOpen() throws {
        guard !isClosed else { throw JedError("55000", "database is closed") }
        try host?.checkOpen()
    }
    public func session(options: SessionOptions = SessionOptions()) throws -> Session {
        try checkOpen(); return try Session(database: self, options: options, readOnly: false)
    }
    public func readSession(options: SessionOptions = SessionOptions()) throws -> Session {
        try checkOpen(); return try Session(database: self, options: options, readOnly: true)
    }
    public func writeSession(options: SessionOptions = SessionOptions()) throws -> Session { try session(options: options) }
    public func query(_ sql: String, parameters: [SQLValue] = [], cancellation: CancellationToken? = nil) throws -> Rows {
        try session().query(sql, parameters: parameters, cancellation: cancellation)
    }
    @discardableResult public func execute(_ sql: String, parameters: [SQLValue] = [], cancellation: CancellationToken? = nil) throws -> CommandTag {
        try session().execute(sql, parameters: parameters, cancellation: cancellation)
    }
    public func flush() throws { try checkOpen(); try core.maintenance { try core.storage.pager.flush() } }
    public func close() throws {
        if isClosed { return }
        if let coordinated = host as? any CoordinatedDatabaseClose {
            do { try coordinated.closeDatabase(core); state.withLock { $0.closed = true } }
            catch { if coordinated.isClosed { state.withLock { $0.closed = true } }; throw error }
            return
        }
        try core.maintenance {
            // Once close owns the gate it must release the native descriptor
            // and lock even when a previous storage failure prevents flushing.
            defer { state.withLock { $0.closed = true } }
            do { try core.storage.pager.flush() }
            catch { try? host?.close(); throw error }
            try host?.close()
        }
    }
}

public enum TransactionStatus: Sendable { case idle, open, failed }

internal final class SessionTransaction {
    struct Savepoint { let name: SQLText; let snapshot: DomainSnapshot; let sequences: SequenceHistory }
    private final class CursorReference {
        weak var rows: Rows?
        init(_ rows: Rows) { self.rows = rows }
    }
    var working: DomainSnapshot
    let writer: WriterLease?
    let pin: SnapshotPin?
    var failed = false
    var savepoints: [Savepoint] = []
    var savedSequences = SequenceHistory()
    private var cursors: [CursorReference] = []
    init(writer: WriterLease) { self.writer = writer; pin = nil; working = writer.snapshot }
    init(pin: SnapshotPin) { writer = nil; self.pin = pin; working = pin.snapshot }
    func checkRole() throws { try writer?.checkRole(); try pin?.checkRole() }
    func cursorPin() throws -> SnapshotPin {
        if let writer { return try writer.pin(working) }
        return try pin!.retain()
    }
    func own(_ rows: Rows) -> Rows {
        cursors.removeAll { $0.rows == nil || $0.rows?.isClosed == true }
        cursors.append(CursorReference(rows))
        return rows
    }
    func close() {
        let opened = cursors; cursors = []
        for cursor in opened { cursor.rows?.close() }
        writer?.close(); pin?.close()
    }
    deinit { close() }
}

/// A session and its cursors are driven by one caller at a time. Separate
/// sessions run independently over the thread-safe database core.
public final class Session {
    internal let database: Database
    internal var settings: SessionSettings
    internal var transaction: SessionTransaction?
    private var sequenceHistory = SequenceHistory()
    private let readPin: SnapshotPin?
    private var closed = false
    private var scoped = false
    internal init(database: Database, options: SessionOptions, readOnly: Bool) throws {
        self.database = database
        settings = SessionSettings(environment: database.environment, options: options)
        readPin = try readOnly ? database.core.beginRead() : nil
    }
    public var status: TransactionStatus { transaction.map { $0.failed ? .failed : .open } ?? .idle }
    public var isClosed: Bool { closed }
    public var lifetimeCost: Int64 { settings.lifetime.accrued }
    public var maxCost: Int64 {
        get { settings.options.maxCost }
        set { settings.options.maxCost = max(0, newValue) }
    }
    public var lifetimeMaxCost: Int64 {
        get { settings.lifetime.limit }
        set { settings.lifetime.limit = max(0, newValue) }
    }
    public var maxScalarBytes: Int64 {
        get { settings.options.maxScalarBytes }
        set { settings.options.maxScalarBytes = newValue > 0 ? newValue : GeneratedSpec.defaultScalarBytes }
    }
    public var maxQueryMemoryBytes: Int64 {
        get { settings.options.maxQueryMemoryBytes }
        set { settings.options.maxQueryMemoryBytes = max(0, newValue) }
    }
    public var timeZone: SQLText { settings.timeZoneName }
    public func setTimeZone(_ name: SQLText) throws { try checkOpen(); try settings.setTimeZone(name) }
    public func setVariable(_ name: SQLText, value: SQLText) throws { try checkOpen(); try settings.setVariable(name, value: value) }
    public func resetVariable(_ name: SQLText) throws { try checkOpen(); try settings.setVariable(name, value: nil) }
    public func variable(_ name: SQLText) -> SQLText? { settings.variables[name.asciiLowercased] }
    public func grant(_ privileges: PrivilegeSet, on name: SQLText, function: Bool = false) {
        settings.privileges.grant(privileges, on: name, function: function)
    }
    public func revoke(_ privileges: PrivilegeSet, on name: SQLText, function: Bool = false) {
        settings.privileges.revoke(privileges, on: name, function: function)
    }
    public func render(_ value: SQLValue, type: TypeDescriptor) throws -> SQLText { try settings.render(value, type: type) }
    internal func checkOpen() throws {
        guard !closed else { throw JedError("55000", "session is closed") }
        try database.checkOpen()
    }
    private func checkStatementRole() throws { try readPin?.checkRole(); try transaction?.checkRole() }
    private func requireClean() throws {
        if transaction?.failed == true { throw JedError("25P02", "current transaction is aborted, commands ignored until end of transaction block") }
    }
    public func begin(writable: Bool = true, cancellation: CancellationToken? = nil) throws {
        try checkOpen(); try checkStatementRole(); try requireClean()
        guard transaction == nil else { throw JedError("25001", "there is already a transaction in progress") }
        if writable {
            guard readPin == nil else { throw JedError("25006", "cannot set transaction read-write mode on a read-only session") }
            transaction = SessionTransaction(writer: try database.core.acquireWriter(cancellation: cancellation, relaxed: !database.synchronous, timeoutMilliseconds: settings.options.lockTimeoutMilliseconds))
        } else { transaction = SessionTransaction(pin: try readPin?.retain() ?? database.core.beginRead()) }
        transaction?.savedSequences = sequenceHistory
    }
    public func commit() throws {
        try checkOpen()
        guard !scoped else { throw JedError("2D000", "cannot end a scoped transaction") }
        guard let active = transaction else { return }
        transaction = nil; defer { active.close() }
        do { try active.checkRole() } catch { sequenceHistory = active.savedSequences; throw error }
        if active.failed { sequenceHistory = active.savedSequences }
        if !active.failed, let writer = active.writer {
            do {
                var policy = DomainCommitPolicy(); policy.storageLimit = database.maxStorageBytes
                if let plan = try writer.prepare(active.working, policy: policy, entropy: database.keyEntropy) {
                    try writer.persist(plan, synchronous: database.synchronous)
                }
            } catch { sequenceHistory = active.savedSequences; throw error }
        }
    }
    public func rollback() throws {
        try checkOpen()
        guard !scoped else { throw JedError("2D000", "cannot end a scoped transaction") }
        if let active = transaction { sequenceHistory = active.savedSequences; active.close() }
        transaction = nil
    }
    public func view<T>(_ body: (Session) throws -> T) throws -> T { try withTransaction(writable: false, body) }
    public func update<T>(_ body: (Session) throws -> T) throws -> T { try withTransaction(writable: true, body) }
    private func withTransaction<T>(writable: Bool, _ body: (Session) throws -> T) throws -> T {
        try begin(writable: writable); scoped = true
        do {
            let value = try body(self)
            scoped = false; try commit(); return value
        } catch {
            scoped = false
            if let active = transaction { sequenceHistory = active.savedSequences; active.close() }
            transaction = nil; throw error
        }
    }
    public func query(_ sql: String, parameters: [SQLValue] = [], cancellation: CancellationToken? = nil) throws -> Rows {
        try checkOpen()
        do {
            let text = try SQLText(sql); try settings.checkSQLLength(text)
            var statement = try SQLParser(utf8: text.utf8).parse()
            if case .with = statement, let query = statement.queryBody { statement = .query(query) }
            statement = CreateTableAs.normalize(statement)
            switch statement {
            case .begin, .commit, .rollback, .savepoint, .release, .rollbackTo:
                guard parameters.isEmpty else { throw JedError("42601", "transaction control takes no bind parameters") }
            default: break
            }
            switch statement {
            case .commit: try commit(); return emptyRows("COMMIT")
            case .rollback: try rollback(); return emptyRows("ROLLBACK")
            case .rollbackTo(let name): try checkStatementRole(); try restoreSavepoint(name); return emptyRows("ROLLBACK")
            default: try checkStatementRole(); try requireClean()
            }
            let meter = try settings.newMeter(pendingBytes: transaction?.working.pendingBytes ?? 0, cancellation: cancellation)
            switch statement {
            case .begin(let writable):
                try begin(writable: writable ?? (readPin == nil && !database.isReadOnly), cancellation: cancellation)
                return emptyRows("BEGIN", meter: meter)
            case .savepoint(let name):
                guard let transaction else { throw JedError("25P01", "SAVEPOINT can only be used in transaction blocks") }
                transaction.savepoints.append(.init(name: name, snapshot: transaction.working, sequences: sequenceHistory))
                transaction.working.forgetPendingKeys()
                return emptyRows("SAVEPOINT", meter: meter)
            case .release(let name):
                guard let transaction else { throw JedError("25P01", "RELEASE can only be used in transaction blocks") }
                guard let index = transaction.savepoints.lastIndex(where: { $0.name == name }) else { throw JedError("3B001", "savepoint does not exist: \(name)") }
                transaction.savepoints.removeSubrange(index...)
                return emptyRows("RELEASE", meter: meter)
            case .query(let query):
                let pin = try transaction?.cursorPin() ?? readPin?.retain() ?? database.core.beginRead()
                do {
                    try StatementPrivileges.check(statement, snapshot: pin.snapshot, settings: settings)
                    let services = StatementServices(settings: settings, meter: meter)
                    var context = services.context()
                    let plan = try SelectPlan.resolve(query, snapshot: pin.snapshot, settings: settings, scalar: context.scalar)
                    if SequenceExecution.mutates(plan.expressions) {
                        pin.close()
                        return try writeQuery(query, parameters: parameters, meter: meter, cancellation: cancellation)
                    }
                    let sequences = SequenceExecution(snapshot: pin.snapshot, history: sequenceHistory, meter: meter, writable: false)
                    sequences.install(into: &context)
                    context.parameters = try ParameterBinding.bind(parameters, types: plan.parameters, timezone: settings.timeZone)
                    let execution = SelectExecution(plan: plan, context: context), active = transaction
                    let rows = Rows(columns: plan.columns, meter: meter, pull: execution.next,
                                    release: { execution.close(); pin.close() }, failure: { _ in active?.failed = true })
                    return active?.own(rows) ?? rows
                } catch { pin.close(); throw error }
            default: return try write(statement, source: text, parameters: parameters, meter: meter, cancellation: cancellation)
            }
        } catch {
            // Rejecting a nested BEGIN is transaction control admission, not a
            // failed SQL statement; the original block remains usable.
            if (error as? JedError)?.code != "25001" { transaction?.failed = true }
            throw error
        }
    }
    @discardableResult public func execute(_ sql: String, parameters: [SQLValue] = [], cancellation: CancellationToken? = nil) throws -> CommandTag {
        try query(sql, parameters: parameters, cancellation: cancellation).finish()
    }
    private func emptyRows(_ command: String, meter: CostMeter = CostMeter()) -> Rows {
        Rows(columns: [], meter: meter, command: command, pull: { nil })
    }
    private func writeQuery(_ query: SQLQuery, parameters: [SQLValue], meter: CostMeter,
                            cancellation: CancellationToken?) throws -> Rows {
        guard readPin == nil, !database.isReadOnly, transaction == nil || transaction?.writer != nil else {
            throw JedError("25006", "cannot advance a sequence in a read-only transaction")
        }
        let active = transaction
        let writer = try active?.writer ?? database.core.acquireWriter(cancellation: cancellation, relaxed: !database.synchronous, timeoutMilliseconds: settings.options.lockTimeoutMilliseconds)
        defer { if active == nil { writer.close() } }
        var working = active?.working ?? writer.snapshot
        let services = StatementServices(settings: settings, meter: meter)
        var context = services.context()
        let plan = try SelectPlan.resolve(query, snapshot: working, settings: settings, scalar: context.scalar)
        context.parameters = try ParameterBinding.bind(parameters, types: plan.parameters, timezone: settings.timeZone)
        let sequences = SequenceExecution(snapshot: working, history: sequenceHistory, meter: meter, writable: true)
        sequences.install(into: &context)
        let execution = SelectExecution(plan: plan, context: context)
        defer { execution.close() }
        let result = MutationResult(command: "SELECT", columns: plan.columns, meter: meter)
        while let row = try execution.next() { try result.append(row); result.affected += 1 }
        execution.close()
        try sequences.apply(to: &working)
        try meter.checkpoint(); try meter.syncPending(working.pendingBytes)
        if let active { active.working = working }
        else {
            var policy = DomainCommitPolicy(); policy.storageLimit = database.maxStorageBytes
            if let plan = try writer.prepare(working, policy: policy, entropy: database.keyEntropy) {
                try writer.persist(plan, synchronous: database.synchronous)
            }
            try meter.syncPending(0)
        }
        sequenceHistory = sequences.history
        let rows = result.cursor(failure: { _ in active?.failed = true })
        return active?.own(rows) ?? rows
    }
    private func write(_ statement: SQLStatement, source: SQLText, parameters: [SQLValue],
                       meter: CostMeter, cancellation: CancellationToken?) throws -> Rows {
        guard readPin == nil, !database.isReadOnly, transaction == nil || transaction?.writer != nil else {
            throw JedError("25006", "cannot execute a write in a read-only transaction")
        }
        let active = transaction
        let writer = try active?.writer ?? database.core.acquireWriter(cancellation: cancellation, relaxed: !database.synchronous, timeoutMilliseconds: settings.options.lockTimeoutMilliseconds)
        defer { if active == nil { writer.close() } }
        var working = active?.working ?? writer.snapshot
        let services = StatementServices(settings: settings, meter: meter)
        var context = services.context()
        let sequences = SequenceExecution(snapshot: working, history: sequenceHistory, meter: meter, writable: true)
        sequences.install(into: &context)
        let result: MutationResult
        try StatementPrivileges.check(statement, snapshot: working, settings: settings)
        if case .createTable(let definition) = statement, definition.query != nil {
            result = try CreateTableAs.execute(definition, snapshot: &working, settings: settings,
                                              context: context, parameters: parameters)
        } else if let mutation = try MutationExecutor.execute(statement, snapshot: &working, settings: settings,
                                                        context: context, parameters: parameters) { result = mutation }
        else if let tag = try CatalogDDL.execute(statement, source: source, snapshot: &working, settings: settings,
                                                context: context, meter: meter) {
            guard parameters.isEmpty else { throw JedError("42601", "DDL takes no bind parameters") }
            result = MutationResult(command: tag.command, meter: meter); result.affected = tag.rowsAffected
        } else { throw JedError("0A000", "statement execution is not yet connected") }
        try sequences.apply(to: &working)
        try meter.checkpoint(); try meter.syncPending(working.pendingBytes)
        if let active { active.working = working }
        else {
            var policy = DomainCommitPolicy(); policy.storageLimit = database.maxStorageBytes
            if let plan = try writer.prepare(working, policy: policy, entropy: database.keyEntropy) {
                try writer.persist(plan, synchronous: database.synchronous)
            }
            try meter.syncPending(0)
        }
        sequenceHistory = sequences.history
        let rows = result.cursor(failure: { _ in active?.failed = true })
        return active?.own(rows) ?? rows
    }
    private func restoreSavepoint(_ name: SQLText) throws {
        guard let transaction else { throw JedError("25P01", "ROLLBACK TO can only be used in transaction blocks") }
        guard let index = transaction.savepoints.lastIndex(where: { $0.name == name }) else { throw JedError("3B001", "savepoint does not exist: \(name)") }
        transaction.working = transaction.savepoints[index].snapshot
        sequenceHistory = transaction.savepoints[index].sequences
        transaction.working.forgetPendingKeys()
        transaction.savepoints.removeSubrange((index + 1)...)
        transaction.failed = false
    }
    public func close() {
        if closed { return }; closed = true
        transaction?.close(); transaction = nil; readPin?.close()
    }
    deinit { close() }
}
