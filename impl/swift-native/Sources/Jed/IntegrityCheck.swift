public extension Database {
    /// Inspect one committed main or attached domain. The operation does not
    /// acquire the writer gate or consume a session's SQL cost/memory account.
    func integrityCheck(_ name: String = "main", options: IntegrityCheckOptions = .init(),
                        cancellation: CancellationToken? = nil) throws -> IntegrityReport {
        guard options.maxFindings >= 0 else { throw JedError("22023", "negative integrity finding limit") }
        let name = try SQLText(name)
        for _ in 0...100_000 {
            if cancellation?.isCancelled == true { throw JedError("57014", "integrity check canceled") }
            let (target, pin) = try domains.integrityPin(name, main: self)
            defer { pin.close(); withExtendedLifetime(target) {} }
            let storage = target.core.storage
            try pin.checkRole()
            guard let generation = try storage.pager.integrityGeneration(txid: pin.snapshot.txid,
                memory: storage.kind == .memory) else { continue }
            let checker = IntegrityChecker(database: name.description, snapshot: pin.snapshot,
                generation: generation, pager: storage.pager, environment: target.environment,
                options: options, cancellation: cancellation, checkRole: { try pin.checkRole() })
            return try checker.run()
        }
        throw JedError("55006", "database changed too often while capturing integrity snapshot")
    }
}

/// One bitmap covers physical reachability. Replays retain only a traversal
/// stack and one decoded row; findings are the caller-selected bounded result.
internal final class IntegrityChecker {
    let database: String
    let snapshot: DomainSnapshot
    let generation: IntegrityGeneration
    let pager: Pager
    let environment: Environment
    let options: IntegrityCheckOptions
    let cancellation: CancellationToken?
    let checkRole: () throws -> Void
    var report = IntegrityReport()
    var reached: [UInt8]
    var damaged = false
    var tableUnreadable: Set<SQLText> = []
    var indexClean: [SQLText: Bool] = [:]
    var indexEntries: [SQLText: UInt64] = [:]
    var gistRoots: [SQLText: PageReference] = [:]
    var protected: Set<UInt32>
    var meta: MetaPage { generation.selected.meta }
    var codec: any PageCodec { pager.codec }
    var settings: SessionSettings { SessionSettings(environment: environment, options: .init()) }

    init(database: String, snapshot: DomainSnapshot, generation: IntegrityGeneration,
         pager: Pager, environment: Environment, options: IntegrityCheckOptions,
         cancellation: CancellationToken?, checkRole: @escaping () throws -> Void) {
        self.database = database; self.snapshot = snapshot; self.generation = generation
        self.pager = pager; self.environment = environment; self.options = options
        self.cancellation = cancellation; self.checkRole = checkRole
        reached = [UInt8](repeating: 0, count: (Int(generation.selected.meta.pageCount) + 7) / 8)
        protected = generation.selected.protectedPages
    }
    func poll() throws {
        if cancellation?.isCancelled == true { throw JedError("57014", "integrity check canceled") }
        try checkRole()
    }
    func add(_ kind: String, _ object: SQLText? = nil, page: UInt32? = nil,
             key: [UInt8]? = nil, _ detail: String = "") throws {
        if options.maxFindings > 0 && report.findings.count >= options.maxFindings {
            report.truncated = true; throw IntegrityStop.truncated
        }
        report.findings.append(IntegrityFinding(kind: kind, database: database,
            object: object.map { $0.description }, page: page, key: key, detail: detail))
    }
    func skip(_ object: SQLText, _ reason: String) {
        report.skipped.append(IntegritySkip(object: object.description, reason: reason))
    }
    func freshContext() throws -> ExpressionContext {
        let configuration = settings
        return try StatementServices(settings: configuration, meter: configuration.newMeter(cancellation: cancellation)).context()
    }
    func marked(_ index: UInt32) -> Bool {
        index < meta.pageCount && reached[Int(index / 8)] & (1 << (index % 8)) != 0
    }
    func mark(_ index: UInt32) {
        if !marked(index) { reached[Int(index / 8)] |= 1 << (index % 8); report.pages += 1 }
    }
    func reach(_ reference: PageReference, object: SQLText, key: [UInt8]? = nil) throws -> Bool {
        try poll()
        if reference.index < meta.firstBodyPage || reference.index >= meta.pageCount {
            try add("page_out_of_range", object, page: reference.index, key: key); return false
        }
        if marked(reference.index) {
            try add("page_reached_twice", object, page: reference.index, key: key); return false
        }
        mark(reference.index); return true
    }
    func body(_ reference: PageReference, cached: Bool = true) throws -> [UInt8] {
        try poll()
        let bytes = try cached ? snapshot.pool.integrityBody(reference) : pager.readBody(reference)
        try PageEnvelope.validateHeader(bytes, encrypted: meta.encrypted)
        guard bytes.count == codec.pageSize - codec.reservedBytes,
              try PageEnvelope.readU32(bytes, at: 12) == PageEnvelope.checksum(bytes, at: 12) else {
            throw JedError.corruption("invalid integrity page envelope")
        }
        return bytes
    }
    func run() throws -> IntegrityReport {
        do {
            try poll(); try checkGeneration(); try checkCatalog()
            for sequence in snapshot.catalog.sequences.sorted(by: { $0.name < $1.name }) {
                if sequence.last < sequence.minimum || sequence.last > sequence.maximum {
                    try add("sequence_out_of_range", sequence.name)
                }
            }
            for table in snapshot.catalog.tables.sorted(by: { $0.name < $1.name }) {
                let types = try snapshot.tableTypes(table)
                let state = try walkTree(root: table.root, object: table.name, types: types, table: table)
                if state.unreadable { tableUnreadable.insert(table.name); damaged = true }
                else if state.records != UInt64(table.rowCount) { try add("row_count_mismatch", table.name) }
                for index in table.indexes.sorted(by: { $0.name < $1.name }) {
                    let before = report.findings.count
                    let result: IntegrityTreeResult
                    if index.kind == 2 {
                        result = try walkGist(root: gistRoots[index.name], table: table, index: index)
                    } else {
                        result = try walkTree(root: index.root, object: index.name, types: [], gin: index.kind == 1)
                    }
                    indexClean[index.name] = before == report.findings.count && !result.unreadable
                    indexEntries[index.name] = result.records
                    damaged = damaged || result.unreadable
                    if options.level == .full { try checkAgreement(table, index: index) }
                }
            }
            if options.constraints {
                for table in snapshot.catalog.tables.sorted(by: { $0.name < $1.name }) { try checkConstraints(table) }
            }
            let live = report.pages
            try checkFreeList()
            if !generation.memory && !damaged && live != UInt64(meta.livePages) { try add("live_count_mismatch") }
            try poll()
        } catch IntegrityStop.truncated {}
        try poll()
        return report
    }
}
