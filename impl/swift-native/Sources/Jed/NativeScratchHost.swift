#if os(Linux) || canImport(Darwin)
import Foundation

internal final class NativeScratchFileHost: ScratchFileHost {
    let sealed: Bool
    let directory: String
    private let names: any RandomSource
    private let keys: any RandomSource
    init(directory: String = FileManager.default.temporaryDirectory.path, sealed: Bool,
         names: any RandomSource = OSRandomSource(), keys: any RandomSource = OSRandomSource()) {
        self.directory = directory; self.sealed = sealed; self.names = names; self.keys = keys
    }
    func create(kind: String) throws -> any ScratchStore {
        let store = try UnixBlockStore.scratch(directory: directory, random: names)
        let file = NativeScratchStore(store)
        guard sealed else { return file }
        do { return try SealedScratchStore(inner: file, random: keys) }
        catch { try? file.close(); throw error }
    }
}

private final class NativeScratchStore: ScratchStore {
    private let store: UnixBlockStore
    init(_ store: UnixBlockStore) { self.store = store }
    func read(at offset: UInt64, count: Int) throws -> [UInt8] { try store.read(at: offset, count: count) }
    func write(at offset: UInt64, bytes: [UInt8]) throws { try store.write(at: offset, bytes: bytes) }
    func sync() throws { try store.sync() }
    func size() throws -> UInt64 { try store.size() }
    func setSize(_ bytes: UInt64) throws { try store.setSize(bytes) }
    func close() throws { try store.close() }
}
#endif
