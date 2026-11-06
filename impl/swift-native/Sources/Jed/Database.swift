import Synchronization

/// Shared storage identity. Convenience queries create a fresh session; callers
/// that need variables, budgets, or transaction state keep an explicit Session.
public final class Database: Sendable {
    private static let nextCatalog = Atomic<UInt64>(1)
    internal let core: DatabaseCore
    internal let domains = DatabaseDomains()
    internal let host: (any DatabaseHostLifetime)?
    public let environment: Environment
    internal let flushIntervalMilliseconds: UInt64
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
                  keySource: (any RandomSource)? = nil, host: (any DatabaseHostLifetime)? = nil,
                  flushIntervalMilliseconds: UInt64 = 1000) {
        self.core = core; self.environment = environment; self.keySource = Mutex(keySource ?? environment.newKeySource()); self.host = host
        self.flushIntervalMilliseconds = flushIntervalMilliseconds
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
        try checkOpen(); return try Session(database: self, options: options, readOnly: isReadOnly)
    }
    public func readSession(options: SessionOptions = SessionOptions()) throws -> Session {
        try checkOpen(); return try Session(database: self, options: options, readOnly: true)
    }
    public func writeSession(options: SessionOptions = SessionOptions()) throws -> Session {
        if isReadOnly { return try readSession(options: options) }
        let result = try session(options: options)
        do { try result.begin(writable: true); return result }
        catch { result.close(); throw error }
    }
    public func query(_ sql: String, parameters: [SQLValue] = [], cancellation: CancellationToken? = nil) throws -> Rows {
        try session().query(sql, parameters: parameters, cancellation: cancellation)
    }
    public func prepare(_ sql: String) throws -> PreparedStatement {
        let session = try session(); defer { session.close() }; return try session.prepare(sql)
    }
    public func queryPrepared(_ statement: PreparedStatement, parameters: [SQLValue] = [],
                              cancellation: CancellationToken? = nil) throws -> Rows {
        try session().queryPrepared(statement, parameters: parameters, cancellation: cancellation)
    }
    @discardableResult public func executePrepared(_ statement: PreparedStatement, parameters: [SQLValue] = [],
                                                   cancellation: CancellationToken? = nil) throws -> CommandTag {
        try queryPrepared(statement, parameters: parameters, cancellation: cancellation).finish()
    }
    @discardableResult public func execute(_ sql: String, parameters: [SQLValue] = [], cancellation: CancellationToken? = nil) throws -> CommandTag {
        try session().execute(sql, parameters: parameters, cancellation: cancellation)
    }
    public func flush() throws { try checkOpen(); try core.maintenance { try core.storage.pager.flush() } }
    public func attach(_ name: SQLText, source: AttachSource, readOnly: Bool) throws {
        try checkOpen(); try domains.attach(name, source: source, readOnly: readOnly, main: self)
    }
    public func detach(_ name: SQLText) throws { try checkOpen(); try domains.detach(name) }
    public func setMaxStorageBytes(_ name: SQLText, _ limit: Int64) throws {
        try checkOpen()
        guard limit >= 0 else { throw JedError("22023", "negative storage limit") }
        if name == DomainNames.main { maxStorageBytes = limit }
        else { try domains.handle(name).maxStorageBytes = limit }
    }
    public func close() throws {
        try domains.withClose(main: self) { try closePrimary() }
    }
    private func closePrimary() throws {
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
    struct Savepoint {
        let name: SQLText; let snapshot: QueryDomains; let sequences: SequenceHistory
        let temporaryActions: [SQLText: String]
    }
    private final class CursorReference {
        weak var rows: Rows?
        init(_ rows: Rows) { self.rows = rows }
    }
    var working: QueryDomains
    let writer: DomainWriteBundle?
    let pin: DomainReadBundle?
    let temporaryWriter: WriterLease
    var temporaryActions: [SQLText: String]
    var failed = false
    var savepoints: [Savepoint] = []
    var savedSequences = SequenceHistory()
    private var cursors: [CursorReference] = []
    init(writer: DomainWriteBundle, temporary: WriterLease, actions: [SQLText: String]) {
        self.writer = writer; pin = nil; working = writer.initial
        temporaryWriter = temporary; temporaryActions = actions
        working.snapshots[DomainNames.temporary] = temporary.snapshot
    }
    init(pin: DomainReadBundle, temporary: WriterLease, actions: [SQLText: String]) {
        writer = nil; self.pin = pin; working = pin.domains
        temporaryWriter = temporary; temporaryActions = actions
        working.snapshots[DomainNames.temporary] = temporary.snapshot
    }
    func checkRole() throws { try writer?.checkRole(); try pin?.checkRole(); try temporaryWriter.checkRole() }
    func cursorPin() throws -> DomainReadBundle {
        let result = try writer?.pin(working, borrowingWriter: true) ?? pin!.retain()
        do {
            result.install(try temporaryWriter.pin(working.snapshot(DomainNames.temporary), borrowingWriter: true), for: DomainNames.temporary)
            return result
        } catch { result.close(); throw error }
    }
    var writableDomains: Set<SQLText> { Set(writer?.leases.keys.map { $0 } ?? []).union([DomainNames.temporary]) }
    func requireWritable(_ name: SQLText) throws {
        if name == DomainNames.temporary { return }
        guard let writer else { throw JedError("25006", "cannot execute a write in a read-only transaction") }
        try writer.requireWritable(name)
    }
    func own(_ rows: Rows) -> Rows {
        cursors.removeAll { $0.rows == nil || $0.rows?.isClosed == true }
        cursors.append(CursorReference(rows))
        return rows
    }
    func close() {
        let opened = cursors; cursors = []
        for cursor in opened { cursor.rows?.close() }
        temporaryWriter.close(); writer?.close(); pin?.close()
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
    private var temporary: TemporaryDomain?
    private var temporaryActions: [SQLText: String] = [:]
    private let readPin: DomainReadBundle?
    private var closed = false
    private var scoped = false
    internal init(database: Database, options: SessionOptions, readOnly: Bool) throws {
        self.database = database
        settings = SessionSettings(environment: database.environment, options: options)
        settings.scratchFileHost = database.scratchFileHost
        readPin = try readOnly ? database.domains.read(main: database) : nil
        temporary = try TemporaryDomain(database: database, bufferBytes: options.temporaryBufferBytes)
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
    /// Host catalog APIs share the session snapshot and publication machinery.
    /// Their validation errors leave an existing SQL block usable.
    internal func withHostCatalogRead<T>(_ body: (DomainSnapshot) throws -> T) throws -> T {
        try checkOpen(); try checkStatementRole()
        let pin = try readBundle(); defer { pin.close() }
        return try body(pin.domains.snapshot(DomainNames.main))
    }
    internal func withHostCatalogWrite<T>(_ body: (inout DomainSnapshot) throws -> T) throws -> T {
        try checkOpen(); try checkStatementRole(); try requireClean()
        guard readPin == nil, !database.isReadOnly, transaction == nil || transaction?.writer != nil else {
            throw JedError("25006", "cannot modify a catalog in a read-only transaction")
        }
        let active = transaction, frame = try writeFrame(shared: true, cancellation: nil)
        defer { if active == nil { frame.close() } }
        var working = frame.working, snapshot = try frame.working.snapshot(DomainNames.main)
        let original = snapshot
        let result = try body(&snapshot)
        working.snapshots[DomainNames.main] = snapshot
        frame.working = working
        if active == nil, snapshot.hasChanges(comparedTo: original), let writer = frame.writer {
            // Host catalog maintenance has no SQL temporary-table commit actions.
            try writer.commit(working, through: database.domains, synchronous: database.synchronous)
        }
        return result
    }
    /// Internal verification retains the same bundle that ordinary reads use.
    internal func withToolingDomains<T>(_ body: (QueryDomains) throws -> T) throws -> T {
        try checkOpen(); try checkStatementRole()
        let pin = try readBundle(); defer { pin.close() }
        return try body(pin.domains)
    }
    private func readBundle() throws -> DomainReadBundle {
        if let transaction { return try transaction.cursorPin() }
        let result = try readPin?.retain() ?? database.domains.read(main: database)
        do {
            result.install(try temporary!.core.beginRead(), for: DomainNames.temporary)
            return result
        } catch { result.close(); throw error }
    }
    private func writeFrame(shared: Bool, cancellation: CancellationToken?) throws -> SessionTransaction {
        if let transaction { return transaction }
        let privateWriter = try temporary!.core.acquireWriter(cancellation: cancellation)
        do {
            if shared {
                guard readPin == nil, !database.isReadOnly else { throw JedError("25006", "cannot execute a write in a read-only transaction") }
                let writer = try database.domains.acquire(main: database, cancellation: cancellation,
                    timeoutMilliseconds: settings.options.lockTimeoutMilliseconds)
                return SessionTransaction(writer: writer, temporary: privateWriter, actions: temporaryActions)
            }
            return SessionTransaction(pin: try readPin?.retain() ?? database.domains.read(main: database),
                temporary: privateWriter, actions: temporaryActions)
        } catch { privateWriter.close(); throw error }
    }
    private func publish(_ frame: SessionTransaction) throws {
        var working = frame.working, actions = frame.temporaryActions
        var temp = try working.snapshot(DomainNames.temporary)
        var commitSettings = settings; commitSettings.domains = working; commitSettings.targetDomain = DomainNames.temporary
        if try TemporaryCommitActions.apply(snapshot: &temp, actions: &actions, settings: commitSettings) {
            try temporary!.checkBudget(settings.options.maxTemporaryStorageBytes)
        }
        working.snapshots[DomainNames.temporary] = temp
        try temporary!.configure(bufferBytes: settings.options.temporaryBufferBytes)
        let participant = DomainCommitParticipant(name: DomainNames.temporary, lease: frame.temporaryWriter, working: temp)
        if let writer = frame.writer {
            try writer.commit(working, through: database.domains, synchronous: database.synchronous, additional: [participant])
        } else {
            let group = try DomainCommitGroup.prepare([participant])
            _ = try group.commit()
        }
        temporaryActions = actions
    }
    public func begin(writable: Bool = true, cancellation: CancellationToken? = nil) throws {
        try checkOpen(); try checkStatementRole(); try requireClean()
        guard transaction == nil else { throw JedError("25001", "there is already a transaction in progress") }
        if writable {
            guard readPin == nil else { throw JedError("25006", "cannot set transaction read-write mode on a read-only session") }
        }
        transaction = try writeFrame(shared: writable, cancellation: cancellation)
        transaction?.savedSequences = sequenceHistory
    }
    internal func admitScript(cancellation: CancellationToken?) throws -> Bool {
        try checkOpen(); try checkStatementRole()
        guard transaction == nil else { return false }
        try begin(writable: readPin == nil && !database.isReadOnly, cancellation: cancellation)
        return true
    }
    public func commit() throws {
        try checkOpen()
        guard !scoped else { throw JedError("2D000", "cannot end a scoped transaction") }
        guard let active = transaction else { return }
        transaction = nil; defer { active.close() }
        do { try active.checkRole() } catch { sequenceHistory = active.savedSequences; throw error }
        if active.failed || active.writer == nil { sequenceHistory = active.savedSequences }
        if !active.failed, active.writer != nil {
            do {
                try publish(active)
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
    public func prepare(_ sql: String) throws -> PreparedStatement {
        try checkOpen()
        do {
            let text = try SQLText(sql); try settings.checkSQLLength(text)
            var statement = try SQLParser(utf8: text.utf8).parse()
            if case .with = statement, let query = statement.queryBody { statement = .query(query) }
            statement = CreateTableAs.normalize(statement)
            return PreparedStatement(source: text, statement: statement)
        } catch { transaction?.failed = true; throw error }
    }
    public func query(_ sql: String, parameters: [SQLValue] = [], cancellation: CancellationToken? = nil) throws -> Rows {
        try queryPrepared(prepare(sql), parameters: parameters, cancellation: cancellation)
    }
    public func queryPrepared(_ prepared: PreparedStatement, parameters: [SQLValue] = [],
                              cancellation: CancellationToken? = nil) throws -> Rows {
        try checkOpen()
        do {
            let text = prepared.source, statement = prepared.statement
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
            if case .reindex(let options) = statement, let active = transaction,
               options.concurrently || options.object == "database" || options.object == "system" {
                active.failed = true
                let form = options.concurrently ? "CONCURRENTLY" : options.object.uppercased()
                throw JedError("25001", "REINDEX \(form) cannot run inside a transaction block")
            }
            let meter = try settings.newMeter(pendingBytes: transaction?.working.pendingBytes ?? 0, cancellation: cancellation)
            defer { meter.resources.releaseStatementRows() }
            switch statement {
            case .begin(let writable):
                try begin(writable: writable ?? (readPin == nil && !database.isReadOnly), cancellation: cancellation)
                return emptyRows("BEGIN", meter: meter)
            case .savepoint(let name):
                guard let transaction else { throw JedError("25P01", "SAVEPOINT can only be used in transaction blocks") }
                transaction.savepoints.append(.init(name: name, snapshot: transaction.working, sequences: sequenceHistory,
                    temporaryActions: transaction.temporaryActions))
                transaction.working.forgetPendingKeys()
                return emptyRows("SAVEPOINT", meter: meter)
            case .release(let name):
                guard let transaction else { throw JedError("25P01", "RELEASE can only be used in transaction blocks") }
                guard let index = transaction.savepoints.lastIndex(where: { $0.name == name }) else { throw JedError("3B001", "savepoint does not exist: \(name)") }
                transaction.savepoints.removeSubrange(index...)
                return emptyRows("RELEASE", meter: meter)
            case .explain(let options, let body):
                return try explain(options, statement: body, parameters: parameters, meter: meter, cancellation: cancellation)
            case .query(let query):
                if WritableCTEPlan.containsWrite(statement) {
                    return try write(statement, source: text, parameters: parameters, meter: meter, cancellation: cancellation)
                }
                let pin = try readBundle()
                do {
                    var planning = settings; planning.domains = pin.domains; planning.planningParameters = parameters
                    let snapshot = try pin.domains.snapshot(DomainNames.main)
                    try StatementPrivileges.check(statement, snapshot: snapshot, settings: planning)
                    let services = StatementServices(settings: planning, meter: meter)
                    var context = services.context()
                    let committed = transaction?.writer?.initial ?? transaction?.pin?.domains ?? pin.domains
                    let (plan, admittedParameters, requirements) = try prepared.select(query, snapshot: snapshot, settings: planning, context: context,
                        supplied: parameters, core: database.core.planIdentity, committed: committed)
                    if requirements?.mutatesSequences ?? SequenceExecution.mutates(plan.expressions) {
                        pin.close()
                        return try writeQuery(query, parameters: parameters, meter: meter, cancellation: cancellation)
                    }
                    let sequences = SequenceExecution(domains: pin.domains, history: sequenceHistory, meter: meter, writable: false)
                    sequences.install(into: &context)
                    context.parameters = try admittedParameters ?? ParameterBinding.bind(parameters, types: plan.parameters, timezone: settings.timeZone)
                    let execution = SelectExecution(plan: plan, context: context, preparedRequirements: requirements), active = transaction
                    try execution.openCursor()
                    let rows = Rows(columns: plan.columns, meter: meter, guardCostOnPull: false,
                                    pull: { try pin.checkRole(); return try execution.next() },
                                    release: { execution.close(); pin.close() }, failure: { _ in active?.failed = true })
                    return active?.own(rows) ?? rows
                } catch { pin.close(); throw error }
            default: return try write(statement, source: text, parameters: parameters, meter: meter,
                                      cancellation: cancellation, prepared: prepared)
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
    @discardableResult public func executePrepared(_ statement: PreparedStatement, parameters: [SQLValue] = [],
                                                   cancellation: CancellationToken? = nil) throws -> CommandTag {
        try queryPrepared(statement, parameters: parameters, cancellation: cancellation).finish()
    }
    private func emptyRows(_ command: String, meter: CostMeter = CostMeter()) -> Rows {
        Rows(columns: [], meter: meter, command: command, pull: { nil })
    }
    private func explain(_ options: SQLExplain, statement: SQLStatement, parameters: [SQLValue],
                         meter: CostMeter, cancellation: CancellationToken?) throws -> Rows {
        let pin = try readBundle(), active = transaction
        do {
            var planning = settings; planning.domains = pin.domains
            let snapshot = try pin.domains.snapshot(DomainNames.main)
            try StatementPrivileges.check(statement, snapshot: snapshot, settings: planning)
            let services = StatementServices(settings: planning, meter: meter)
            let plan = try ExplainPlan.resolve(options, statement: statement, snapshot: snapshot,
                settings: planning, context: services.context(), parameters: parameters)
            if options.analyze && plan.writes {
                pin.close()
                return try explainWriting(options, statement: statement, parameters: parameters,
                    meter: meter, cancellation: cancellation)
            }
            var cost: Int64 = 0, count: Int64 = 0
            var analyzedSubqueries: SubqueryExecution?
            // Folded constants belong to the whole EXPLAIN statement, so
            // their rows overlap display-row construction after ANALYZE.
            defer { withExtendedLifetime(analyzedSubqueries) {} }
            if options.analyze {
                let inner = CostMeter(limit: meter.limit, lifetime: meter.lifetime,
                    resources: meter.resources, cancellation: meter.cancellation)
                let executionServices = StatementServices(settings: planning, meter: inner)
                var context = executionServices.context(), working = pin.domains
                analyzedSubqueries = context.subqueries
                let sequences = SequenceExecution(domains: working, targetDomain: plan.targetDomain,
                    history: sequenceHistory, meter: inner, writable: false)
                sequences.install(into: &context)
                count = try plan.analyze(working: &working, settings: planning, context: context,
                    requireWritable: { _ in throw JedError("25006", "cannot write in a read-only transaction") }, checkRole: pin.checkRole)
                try inner.checkpoint(); cost = inner.accrued
            }
            let result = try plan.render(meter: meter, analyzedCost: cost, analyzedRows: count)
            let rows = Rows(columns: result.columns, meter: meter, command: "EXPLAIN", guardCostOnPull: false,
                pull: { try pin.checkRole(); return result.next() }, release: { result.close(); pin.close() },
                failure: { _ in active?.failed = true })
            return active?.own(rows) ?? rows
        } catch { pin.close(); throw error }
    }
    private func explainWriting(_ options: SQLExplain, statement: SQLStatement, parameters: [SQLValue],
                                meter: CostMeter, cancellation: CancellationToken?) throws -> Rows {
        let statement = CreateTableAs.normalize(statement)
        let active = transaction
        var admissionDomains = try active?.working ?? readPin?.domains ?? database.domains.routing(main: database)
        if active == nil { admissionDomains.snapshots[DomainNames.temporary] = temporary!.core.routingSnapshot }
        var admission = settings; admission.domains = admissionDomains
        try StatementPrivileges.check(statement, snapshot: admissionDomains.snapshot(DomainNames.main), settings: admission)
        let targets = try StatementDomain.targets(statement, in: admissionDomains)
        let sequenceWrites = StatementSequenceWrites.requiresSharedWriter(statement, domains: admissionDomains, settings: admission)
        let frame = try writeFrame(shared: sequenceWrites || targets.contains { $0 != DomainNames.temporary }, cancellation: cancellation)
        defer { if active == nil { frame.close() } }
        var working = frame.working, planning = settings
        if var temp = working.snapshots[DomainNames.temporary], let main = working.snapshots[DomainNames.main] {
            try temp.borrowTypes(main.types); working.snapshots[DomainNames.temporary] = temp
        }
        planning.domains = working
        let snapshot = try working.snapshot(DomainNames.main)
        try StatementPrivileges.check(statement, snapshot: snapshot, settings: planning)
        let services = StatementServices(settings: planning, meter: meter)
        let plan = try ExplainPlan.resolve(options, statement: statement, snapshot: snapshot,
            settings: planning, context: services.context(), parameters: parameters)
        let inner = CostMeter(limit: meter.limit, lifetime: meter.lifetime,
            resources: meter.resources, cancellation: meter.cancellation)
        let executionServices = StatementServices(settings: planning, meter: inner)
        var context = executionServices.context()
        defer { withExtendedLifetime(context.subqueries) {} }
        let sequences = SequenceExecution(domains: working, targetDomain: plan.targetDomain, history: sequenceHistory,
            meter: inner, writable: true, writableDomains: frame.writableDomains)
        sequences.install(into: &context)
        let count = try plan.analyze(working: &working, settings: planning, context: context,
            requireWritable: frame.requireWritable, checkRole: frame.checkRole)
        try inner.checkpoint()
        try sequences.apply(to: &working)
        let result = try plan.render(meter: meter, analyzedCost: inner.accrued, analyzedRows: count)
        try finishWrite(working, statement: statement, frame: frame, active: active, meter: meter, guardCost: false)
        sequenceHistory = sequences.history
        let rows = result.cursor(guardCostOnPull: false, failure: { _ in active?.failed = true })
        return active?.own(rows) ?? rows
    }
    private func writeQuery(_ query: SQLQuery, parameters: [SQLValue], meter: CostMeter,
                            cancellation: CancellationToken?) throws -> Rows {
        let active = transaction
        var admissionDomains = try active?.working ?? readPin?.domains ?? database.domains.routing(main: database)
        if active == nil { admissionDomains.snapshots[DomainNames.temporary] = temporary!.core.routingSnapshot }
        var admission = settings; admission.domains = admissionDomains
        let shared = StatementSequenceWrites.requiresSharedWriter(.query(query), domains: admissionDomains, settings: admission)
        // A mutating view is admitted as a write before any source row is
        // materialized. Literal references to private sequences remain legal
        // in a read-only shared snapshot.
        if shared, let active { try active.requireWritable(DomainNames.main) }
        let frame = try writeFrame(shared: shared, cancellation: cancellation)
        defer { if active == nil { frame.close() } }
        var working = frame.working
        let snapshot = try working.snapshot(DomainNames.main)
        var planning = settings; planning.domains = working; planning.planningParameters = parameters
        let services = StatementServices(settings: planning, meter: meter)
        var context = services.context()
        let plan = try SelectPlan.resolve(query, snapshot: snapshot, settings: planning, scalar: context.scalar)
        context.parameters = try ParameterBinding.bind(parameters, types: plan.parameters, timezone: settings.timeZone)
        let sequences = SequenceExecution(domains: working, history: sequenceHistory, meter: meter,
            writable: true, writableDomains: frame.writableDomains)
        sequences.install(into: &context)
        let execution = SelectExecution(plan: plan, context: context)
        defer { execution.close() }
        let result = MutationResult(command: "SELECT", columns: plan.columns, meter: meter)
        while let row = try execution.next() { try result.append(row); result.affected += 1 }
        execution.close()
        try sequences.apply(to: &working)
        try finishWrite(working, statement: .query(query), frame: frame, active: active, meter: meter)
        sequenceHistory = sequences.history
        let rows = result.cursor(failure: { _ in active?.failed = true })
        return active?.own(rows) ?? rows
    }
    private func write(_ statement: SQLStatement, source: SQLText, parameters: [SQLValue],
                       meter: CostMeter, cancellation: CancellationToken?, prepared: PreparedStatement? = nil) throws -> Rows {
        let active = transaction
        var admissionDomains = try active?.working ?? readPin?.domains ?? database.domains.routing(main: database)
        if active == nil { admissionDomains.snapshots[DomainNames.temporary] = temporary!.core.routingSnapshot }
        let targets: [SQLText]
        var admission = settings; admission.domains = admissionDomains
        try StatementPrivileges.check(statement, snapshot: admissionDomains.snapshot(DomainNames.main), settings: admission)
        targets = try StatementDomain.targets(statement, in: admissionDomains)
        let maintenance: Bool
        switch statement { case .reindex, .analyze: maintenance = true; default: maintenance = false }
        if maintenance, let active, active.writer == nil {
            throw JedError("25006", "cannot execute maintenance in a read-only transaction")
        }
        let sequenceWrites = StatementSequenceWrites.requiresSharedWriter(statement, domains: admissionDomains, settings: admission)
        let frame = try writeFrame(shared: maintenance || sequenceWrites || targets.contains { $0 != DomainNames.temporary }, cancellation: cancellation)
        defer { if active == nil { frame.close() } }
        var working = frame.working
        if var temp = working.snapshots[DomainNames.temporary], let main = working.snapshots[DomainNames.main] {
            try temp.borrowTypes(main.types); working.snapshots[DomainNames.temporary] = temp
        }
        let target = try StatementDomain.target(statement, in: working)
        var executionSettings = settings
        executionSettings.domains = working; executionSettings.targetDomain = target
        executionSettings.planningParameters = parameters
        let services = StatementServices(settings: executionSettings, meter: meter)
        var context = services.context()
        let sequences = SequenceExecution(domains: working, targetDomain: target, history: sequenceHistory,
            meter: meter, writable: true, writableDomains: frame.writableDomains)
        sequences.install(into: &context)
        let result: MutationResult
        var insertCandidate: PreparedInsertPlan?
        try StatementPrivileges.check(statement, snapshot: working.snapshot(target), settings: executionSettings)
        defer { meter.pendingDomainBase = 0; meter.pendingDomainExcluded = false }
        if case .reindex(let syntax) = statement {
            guard parameters.isEmpty else { throw JedError("42601", "REINDEX takes no bind parameters") }
            let tag = try ReindexExecution.execute(syntax, working: &working, settings: executionSettings,
                context: context, requireWritable: frame.requireWritable)
            result = MutationResult(command: tag.command, meter: meter)
        } else if case .analyze(let name, let columns) = statement {
            guard parameters.isEmpty else { throw JedError("42601", "ANALYZE takes no bind parameters") }
            try frame.requireWritable(target)
            var snapshot = try working.snapshot(target)
            let tag = try AnalyzeExecution.execute(name, columns: columns, snapshot: &snapshot,
                settings: executionSettings, context: context)
            working.snapshots[target] = snapshot
            result = MutationResult(command: tag.command, hasAffectedRows: true, meter: meter)
        } else if case .createTable(let definition) = statement, definition.query != nil {
            result = try CreateTableAs.execute(definition, working: &working, settings: executionSettings,
                context: context, parameters: parameters, requireWritable: frame.requireWritable)
        } else if let mutation = try WritableCTEExecution.execute(statement, working: &working, settings: executionSettings,
            context: context, parameters: parameters, requireWritable: frame.requireWritable) { result = mutation }
        else {
            try frame.requireWritable(target)
            var snapshot = try working.snapshot(target)
            meter.pendingDomainExcluded = target == DomainNames.temporary
            meter.pendingDomainBase = working.pendingBytes - (meter.pendingDomainExcluded ? 0 : snapshot.pendingBytes)
            let mutation: MutationResult?
            if let prepared, case .insert(let syntax) = statement,
               PreparedInsertPlan.eligible(syntax, domain: target) {
                let signature = PreparedSchemaSignature(core: database.core.planIdentity, snapshot: snapshot, name: syntax.table.name)
                let plan: MutationExecutor.InsertPreparation
                if let cached = prepared.insertCache, cached.signature == signature {
                    prepared.insertHit()
                    plan = try cached.bind(syntax, snapshot: snapshot, settings: executionSettings, context: context)
                } else {
                    prepared.insertMiss()
                    plan = try MutationExecutor.prepareInsert(syntax, snapshot: snapshot, settings: executionSettings, context: context)
                    if let base = frame.writer?.initial.snapshots[target],
                       signature == PreparedSchemaSignature(core: database.core.planIdentity, snapshot: base, name: syntax.table.name) {
                        insertCandidate = PreparedInsertPlan(plan, signature: signature)
                    }
                }
                mutation = try MutationExecutor.executeInsert(plan, snapshot: &snapshot, context: context, parameters: parameters)
            } else {
                mutation = try MutationExecutor.execute(statement, snapshot: &snapshot, settings: executionSettings,
                    context: context, parameters: parameters)
            }
            if let mutation {
                result = mutation; working.snapshots[target] = snapshot
            } else {
                meter.pendingDomainBase = 0; meter.pendingDomainExcluded = false
                guard let tag = try CatalogDDL.execute(statement, source: source, working: &working, settings: executionSettings,
                    context: context, meter: meter, requireWritable: frame.requireWritable) else {
                    throw JedError("0A000", "statement execution is not yet connected")
                }
                guard parameters.isEmpty else { throw JedError("42601", "DDL takes no bind parameters") }
                result = MutationResult(command: tag.command, meter: meter); result.affected = tag.rowsAffected
            }
        }
        try sequences.apply(to: &working)
        meter.pendingDomainBase = 0; meter.pendingDomainExcluded = false
        try finishWrite(working, statement: statement, frame: frame, active: active, meter: meter)
        if let insertCandidate { prepared?.insertCache = insertCandidate }
        sequenceHistory = sequences.history
        let rows = result.cursor(failure: { _ in active?.failed = true })
        return active?.own(rows) ?? rows
    }
    private func finishWrite(_ working: QueryDomains, statement: SQLStatement, frame: SessionTransaction,
                             active: SessionTransaction?, meter: CostMeter, guardCost: Bool = true) throws {
        let before = try frame.working.snapshot(DomainNames.temporary)
        let after = try working.snapshot(DomainNames.temporary)
        var actions = frame.temporaryActions
        TemporaryCommitActions.record(statement, before: before.catalog, after: after.catalog, actions: &actions)
        if after.hasChanges(comparedTo: frame.temporaryWriter.snapshot) {
            try temporary!.checkBudget(settings.options.maxTemporaryStorageBytes)
        }
        if guardCost { try meter.checkpoint() } else { try meter.poll() }
        try meter.checkPending(working.pendingBytes)
        frame.working = working; frame.temporaryActions = actions
        if active == nil {
            try publish(frame)
            try meter.syncPending(0)
        }
    }
    private func restoreSavepoint(_ name: SQLText) throws {
        guard let transaction else { throw JedError("25P01", "ROLLBACK TO can only be used in transaction blocks") }
        guard let index = transaction.savepoints.lastIndex(where: { $0.name == name }) else { throw JedError("3B001", "savepoint does not exist: \(name)") }
        transaction.working = transaction.savepoints[index].snapshot
        transaction.temporaryActions = transaction.savepoints[index].temporaryActions
        sequenceHistory = transaction.savepoints[index].sequences
        transaction.working.forgetPendingKeys()
        transaction.savepoints.removeSubrange((index + 1)...)
        transaction.failed = false
    }
    public func close() {
        if closed { return }; closed = true
        transaction?.close(); transaction = nil; readPin?.close(); temporary = nil
    }
    deinit { close() }
}
