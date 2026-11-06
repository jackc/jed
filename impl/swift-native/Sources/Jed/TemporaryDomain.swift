import Synchronization

/// A session's private page space. Cursors retain the core and its byte device;
/// closing a session drops ownership without invalidating surviving cursors.
internal final class TemporaryDomain {
    private final class Budget: Sendable {
        let value: Mutex<Int64>
        init(_ bytes: Int64) { value = Mutex(bytes) }
    }
    let core: DatabaseCore
    let store: SpillableScratchStore
    private let budget: Budget

    init(database: Database, bufferBytes: Int64) throws {
        let store = try SpillableScratchStore(budget: max(0, bufferBytes), host: database.scratchFileHost)
        let storage = try StorageDomain.create(store: store, catalogID: Database.allocateCatalogID(), pageSize: database.pageSize)
        let budget = Budget(max(0, bufferBytes)), pool = storage.pool, pageSize = storage.pager.pageSize
        self.store = store; self.budget = budget
        core = DatabaseCore(storage: storage)
        storage.installMemoryStageHook { pages in
            let bytes = budget.value.withLock { $0 }
            try store.spillIfNeeded(committedBytes: UInt64(pages) * UInt64(pageSize), budget: bytes)
            if store.spilled { try pool.resizeCache(bytes: Int(clamping: bytes)) }
        }
    }
    func configure(bufferBytes: Int64) throws {
        budget.value.withLock { $0 = max(0, bufferBytes) }
        if store.spilled { try core.storage.pool.resizeCache(bytes: Int(clamping: max(0, bufferBytes))) }
    }
    func checkBudget(_ limit: Int64) throws {
        guard limit <= 0 || core.storage.storageBytes <= UInt64(limit) else {
            throw JedError("54P03", "temporary storage limit exceeded")
        }
    }
}
