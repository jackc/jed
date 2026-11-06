import Foundation
import Synchronization
@testable import Jed

/// The shared forced-TEMP lane gives an otherwise in-memory main database a
/// real scratch directory. This test host supplies only that capability.
final class SQLCorpusTemporaryHost: DatabaseHostLifetime, DatabaseScratchProvider, ScratchFileHost {
    let path = ""
    private struct State { var closed = false; var tempFiles = 0 }
    private let state = Mutex(State())
    private let inner: any ScratchFileHost
    var scratchFileHost: any ScratchFileHost { self }
    var sealed: Bool { inner.sealed }
    var tempFiles: Int { state.withLock { $0.tempFiles } }

    init(directory: String, sealed: Bool = false) throws {
        #if os(Linux) || canImport(Darwin)
        inner = NativeScratchFileHost(directory: directory, sealed: sealed)
        #else
        throw SQLCorpusError("forced TEMP spill requires a native scratch-file host")
        #endif
    }
    func checkOpen() throws {
        guard !state.withLock({ $0.closed }) else { throw JedError("55000", "corpus scratch host is closed") }
    }
    func close() { state.withLock { $0.closed = true } }
    func create(kind: String) throws -> any ScratchStore {
        try checkOpen()
        let store = try inner.create(kind: kind)
        if kind == "temp" { state.withLock { $0.tempFiles += 1 } }
        return store
    }
    func database(environment: Environment) throws -> Database {
        let storage = try StorageDomain.create(store: MemoryBlockStore(), catalogID: Database.allocateCatalogID(), pageSize: 8192)
        return Database(core: DatabaseCore(storage: storage), environment: environment, host: self)
    }
}
