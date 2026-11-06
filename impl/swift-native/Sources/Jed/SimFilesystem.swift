import Foundation

/// The model serializes byte and directory operations on one recursive lock.
/// Production engine policy is not implemented here: BlockStore calls, rename,
/// and barriers have precisely the fault behavior in simulation-testing.md §6.
internal final class SimFilesystem: @unchecked Sendable {
    let seed: UInt64
    let sector: Int
    let pageSize: Int
    let archiving: Bool
    fileprivate let lock = NSRecursiveLock()
    fileprivate var files: [String: SimFileData] = [:]
    fileprivate var durable: [String: SimFileData] = [:]
    fileprivate struct DirectoryChange {
        let kind: UInt8
        let from: String
        let to: String
        let file: SimFileData
    }
    fileprivate var directoryChanges: [DirectoryChange] = []
    fileprivate var step: UInt64 = 0
    fileprivate var fault: SimFault?
    fileprivate(set) var fired = ""
    fileprivate var dead = false
    fileprivate var oracle = false
    fileprivate(set) var syncs = 0
    fileprivate(set) var barriers = 0
    fileprivate(set) var writes = 0
    fileprivate var spillOperations: UInt64 = 0
    private(set) var operationsHash: UInt64 = 14_695_981_039_346_656_037

    init(seed: UInt64, sector: Int = 64, pageSize: Int, archiving: Bool = false) throws {
        guard sector > 0, pageSize > 0 else { throw JedError("22023", "invalid simulated sector or page size") }
        self.seed = seed; self.sector = sector; self.pageSize = pageSize; self.archiving = archiving
    }
    fileprivate func locked<T>(_ body: () throws -> T) rethrows -> T {
        lock.lock(); defer { lock.unlock() }; return try body()
    }
    static func injected(_ operation: String) -> JedError { JedError("58030", "I/O error: simulated \(operation)") }
    fileprivate func alive(_ operation: String) throws {
        if dead { throw Self.injected("\(operation) after process death") }
    }
    func beginStep(_ index: UInt64, fault: SimFault? = nil) {
        locked {
            step = index; self.fault = fault; fired = ""; dead = false
            syncs = 0; barriers = 0; writes = 0; spillOperations = 0
            for file in allFiles() {
                file.unissued = []; file.barrierComplete = false; file.writes = 0; file.pages = []
            }
        }
    }
    func withOracle<T>(_ body: () throws -> T) rethrows -> T {
        try locked { let old = oracle; oracle = true; defer { oracle = old }; return try body() }
    }
    fileprivate func allFiles() -> [SimFileData] {
        var seen: Set<ObjectIdentifier> = [], result: [SimFileData] = []
        func append(_ file: SimFileData) {
            if seen.insert(ObjectIdentifier(file)).inserted { result.append(file) }
        }
        for directory in [files, durable] {
            for name in directory.keys.sorted(by: Self.byteOrder) { append(directory[name]!) }
        }
        for change in directoryChanges { append(change.file) }
        return result
    }
    private static func byteOrder(_ a: String, _ b: String) -> Bool { a.utf8.lexicographicallyPrecedes(b.utf8) }
    fileprivate func fold(_ kind: UInt8, key: UInt64 = 0, offset: UInt64 = 0, bytes: [UInt8] = []) {
        var hash: UInt64 = 14_695_981_039_346_656_037
        func add(_ byte: UInt8) { hash = (hash ^ UInt64(byte)) &* 1_099_511_628_211 }
        func word(_ value: UInt64) { for shift in stride(from: 56, through: 0, by: -8) { add(UInt8(truncatingIfNeeded: value >> shift)) } }
        word(operationsHash); add(kind); word(key); word(offset)
        for byte in bytes { add(byte) }; operationsHash = hash
    }
    fileprivate func barrier() throws {
        barriers += 1
        if let fault, fired.isEmpty, fault.kind == .powerLoss || fault.kind == .processCrash,
           barriers == max(1, fault.nth) {
            fired = "barrier"; dead = true; throw Self.injected("crash at a durability barrier")
        }
    }
    func exists(_ path: String) throws -> Bool { try locked { try alive("stat"); return files[path] != nil } }
    func create(_ path: String) throws -> SimDisk {
        try locked {
            try alive("create")
            guard files[path] == nil else { throw JedError("58P02", "database file already exists: \(path)") }
            let file = SimFileData(key: SimDecision.fileKey(path))
            files[path] = file
            directoryChanges.append(.init(kind: 99, from: "", to: path, file: file))
            fold(99, key: file.key)
            return SimDisk(filesystem: self, file: file)
        }
    }
    func open(_ path: String) throws -> SimDisk {
        try locked {
            try alive("open")
            guard let file = files[path] else { throw JedError("58P01", "database file does not exist: \(path)") }
            return SimDisk(filesystem: self, file: file)
        }
    }
    func rename(_ source: String, to destination: String, replace: Bool = true) throws {
        try locked {
            try alive("rename")
            guard let file = files[source] else { throw Self.injected("rename of a missing file") }
            guard replace || files[destination] == nil else { throw JedError("58P02", "database file already exists: \(destination)") }
            file.inherit(files[destination]); files[destination] = file; files.removeValue(forKey: source)
            directoryChanges.append(.init(kind: 114, from: source, to: destination, file: file))
            fold(114, key: file.key)
        }
    }
    func remove(_ path: String) throws {
        try locked {
            try alive("remove")
            if let file = files.removeValue(forKey: path) {
                directoryChanges.append(.init(kind: 100, from: path, to: "", file: file)); fold(100, key: file.key)
            }
        }
    }
    func syncDirectory() throws {
        try locked {
            fold(68); try alive("directory sync")
            guard !directoryChanges.isEmpty else { return }
            try barrier(); durable = files; directoryChanges = []
        }
    }
    func entryDurable(_ path: String) -> Bool { locked { files[path] === durable[path] } }
    func powerLoss(step index: UInt64, survive: Int, tear: Int) {
        locked {
            for file in allFiles() {
                for offset in file.pending.keys.sorted() {
                    let versions = file.pending[offset]!
                    func decide(_ lane: UInt64, _ at: UInt64 = UInt64(offset), _ version: UInt64 = 0) -> UInt64 {
                        SimDecision.value(seed: seed, lane: lane, step: index, offset: at &+ file.key, version: version)
                    }
                    guard decide(1) % 1000 < UInt64(max(0, survive)) else { continue }
                    let chosen = decide(2) % UInt64(versions.count)
                    var bytes = versions[Int(chosen)]
                    if decide(3, UInt64(offset), chosen) % 1000 < UInt64(max(0, tear)) {
                        for start in stride(from: 0, to: bytes.count, by: sector) {
                            if decide(4, UInt64(offset + start), chosen) & 1 == 1 { continue }
                            for i in start..<min(start + sector, bytes.count) { bytes[i] = file.durable.indices.contains(offset + i) ? file.durable[offset + i] : 0 }
                        }
                    }
                    file.putDurable(offset, bytes)
                }
                file.pending = [:]; file.cache = file.durable; file.unissued = []
            }
            let keep = Int(SimDecision.value(seed: seed, lane: 10, step: index) % UInt64(directoryChanges.count + 1))
            var directory = durable
            for change in directoryChanges.prefix(keep) {
                if change.kind != 100 { directory[change.to] = change.file }
                if change.kind != 99 { directory.removeValue(forKey: change.from) }
            }
            files = directory; durable = directory; directoryChanges = []; fault = nil; dead = false
        }
    }
    func processDeath() {
        locked {
            for file in allFiles() {
                for write in file.unissued.reversed() {
                    guard var versions = file.pending[write.offset], versions.indices.contains(write.version) else { continue }
                    let count = versions.remove(at: write.version).count
                    if versions.isEmpty { file.pending.removeValue(forKey: write.offset) } else { file.pending[write.offset] = versions }
                    for i in 0..<count where file.cache.indices.contains(write.offset + i) {
                        file.cache[write.offset + i] = versions.last.flatMap { $0.indices.contains(i) ? $0[i] : nil }
                            ?? (file.durable.indices.contains(write.offset + i) ? file.durable[write.offset + i] : 0)
                    }
                }
                file.unissued = []
            }
            fault = nil; dead = false
        }
    }
    fileprivate func spillOperation() throws {
        try alive("spill")
        let ordinal = spillOperations; spillOperations &+= 1
        if let fault, fault.kind == .spill, !oracle,
           SimDecision.value(seed: seed, lane: 11, step: step, offset: ordinal) % 1000 < UInt64(fault.probability) {
            fired = "spill"; throw Self.injected("spill I/O error")
        }
    }
}

/// An inode belongs to the filesystem, while SimDisk is one closable handle.
/// Every mutable field is accessed under its filesystem's lock.
fileprivate final class SimFileData {
    let key: UInt64
    var durable: [UInt8] = [], cache: [UInt8] = []
    var pending: [Int: [[UInt8]]] = [:]
    struct Unissued { let offset: Int; let version: Int }
    var unissued: [Unissued] = []
    var barrierComplete = false
    var writes = 0
    var pages: Set<Int> = []
    var archive: [Int: [[UInt8]]] = [:]
    init(key: UInt64) { self.key = key }
    func putDurable(_ offset: Int, _ bytes: [UInt8]) {
        if offset + bytes.count > durable.count { durable += repeatElement(0, count: offset + bytes.count - durable.count) }
        durable.replaceSubrange(offset..<offset + bytes.count, with: bytes)
    }
    func archiveImage(_ page: Int, _ bytes: [UInt8]) {
        var versions = archive[page] ?? []
        guard !versions.contains(bytes) else { return }; versions.append(bytes)
        if versions.count > 32 { versions.removeFirst(versions.count - 32) }; archive[page] = versions
    }
    func inherit(_ previous: SimFileData?) {
        guard let previous, previous !== self else { return }
        let own = archive; archive = [:]
        for entries in [previous.archive, own] {
            for page in entries.keys.sorted() { for bytes in entries[page]! { archiveImage(page, bytes) } }
        }
    }
}

internal final class SimDisk: FileBlockStore, @unchecked Sendable {
    fileprivate let filesystem: SimFilesystem
    fileprivate var file: SimFileData
    private var closed = false
    fileprivate var readOnly = false
    fileprivate var skipFsync = false
    fileprivate init(filesystem: SimFilesystem, file: SimFileData) { self.filesystem = filesystem; self.file = file }
    private func check(_ operation: String) throws {
        guard !closed else { throw SimFilesystem.injected("operation on closed file") }
        try filesystem.alive(operation)
    }
    private func bounds(_ offset: UInt64, _ count: Int) throws -> Int {
        guard count >= 0, offset <= UInt64(Int.max), UInt64(count) <= UInt64(Int.max) - offset else {
            throw SimFilesystem.injected("invalid byte range")
        }; return Int(offset)
    }
    private func decide(_ lane: UInt64, offset: UInt64, version: UInt64 = 0) -> UInt64 {
        SimDecision.value(seed: filesystem.seed, lane: lane, step: filesystem.step, offset: offset &+ file.key, version: version)
    }
    func read(at offset: UInt64, count: Int) throws -> [UInt8] {
        try filesystem.locked {
            try check("read"); let start = try bounds(offset, count)
            if let fault = filesystem.fault, fault.kind == .read, !filesystem.oracle,
               decide(9, offset: offset) % 1000 < UInt64(fault.probability) {
                filesystem.fired = "read"; throw SimFilesystem.injected("read error")
            }
            guard start + count <= file.cache.count else { throw SimFilesystem.injected("short read") }
            return Array(file.cache[start..<start + count])
        }
    }
    func write(at offset: UInt64, bytes: [UInt8]) throws {
        try filesystem.locked {
            filesystem.fold(119, key: file.key, offset: offset, bytes: bytes); try check("write")
            guard !readOnly else { throw SimFilesystem.injected("write to read-only file") }
            let start = try bounds(offset, bytes.count), version = file.pending[start]?.count ?? 0
            if let fault = filesystem.fault {
                if fault.kind == .processCrash, decide(6, offset: offset, version: UInt64(version)) % 1000 >= UInt64(fault.issue) {
                    file.unissued.append(.init(offset: start, version: version))
                }
                if fault.kind == .write, filesystem.fired.isEmpty,
                   decide(5, offset: offset, version: UInt64(version)) % 1000 < UInt64(fault.probability) {
                    filesystem.fired = "write"; throw SimFilesystem.injected("write error")
                }
            }
            filesystem.writes += 1; file.writes += 1
            if !bytes.isEmpty { for page in start / filesystem.pageSize...(start + bytes.count - 1) / filesystem.pageSize { file.pages.insert(page) } }
            if start + bytes.count > file.cache.count { file.cache += repeatElement(0, count: start + bytes.count - file.cache.count) }
            file.cache.replaceSubrange(start..<start + bytes.count, with: bytes); file.pending[start, default: []].append(bytes)
        }
    }
    private func flush() {
        for offset in file.pending.keys.sorted() {
            let bytes = file.pending[offset]!.last!; file.putDurable(offset, bytes)
            if filesystem.archiving, offset.isMultiple(of: filesystem.pageSize) {
                for index in 0..<(bytes.count / filesystem.pageSize) {
                    let page = offset / filesystem.pageSize + index
                    if page >= 4 { file.archiveImage(page, Array(bytes[index * filesystem.pageSize..<(index + 1) * filesystem.pageSize])) }
                }
            }
        }
        if file.durable.count < file.cache.count { file.durable += repeatElement(0, count: file.cache.count - file.durable.count) }
        file.pending = [:]; file.unissued = []
        if filesystem.directoryChanges.isEmpty { file.barrierComplete = true }
    }
    func sync() throws {
        try filesystem.locked {
            if skipFsync { try check("sync"); return }
            filesystem.fold(115, key: file.key); try check("sync"); filesystem.syncs += 1; try filesystem.barrier()
            if let fault = filesystem.fault, fault.kind == .sync, filesystem.fired.isEmpty, filesystem.syncs == fault.nth {
                filesystem.fired = "sync"
                for offset in file.pending.keys.sorted() {
                    let versions = file.pending[offset]!, newest = versions.last!
                    if decide(7, offset: UInt64(offset), version: UInt64(versions.count - 1)) & 1 == 0 { file.putDurable(offset, newest) }
                    else { for i in newest.indices { file.cache[offset + i] = file.durable.indices.contains(offset + i) ? file.durable[offset + i] : 0 } }
                }
                file.pending = [:]; throw SimFilesystem.injected("sync error")
            }
            flush()
        }
    }
    func size() throws -> UInt64 { try filesystem.locked { try check("size"); return UInt64(file.cache.count) } }
    func setSize(_ bytes: UInt64) throws {
        try filesystem.locked {
            filesystem.fold(122, key: file.key, offset: bytes); try check("set_size"); let count = try bounds(bytes, 0)
            guard !readOnly else { throw SimFilesystem.injected("resize of read-only file") }
            if let fault = filesystem.fault, fault.kind == .noSpace, count > file.cache.count, filesystem.fired.isEmpty {
                filesystem.fired = "enospc"; throw SimFilesystem.injected("no space left on device")
            }
            if count > file.cache.count { file.cache += repeatElement(0, count: count - file.cache.count); flush() }
            else if count < file.cache.count {
                file.cache.removeLast(file.cache.count - count)
                if file.durable.count > count { file.durable.removeLast(file.durable.count - count) }
                for offset in file.pending.keys.filter({ $0 >= count }) { file.pending.removeValue(forKey: offset) }
            }
        }
    }
    func close() { filesystem.locked { closed = true } }
    func reopen(path: String) throws {
        try filesystem.locked {
            try filesystem.alive("reopen")
            guard let replacement = filesystem.files[path] else { throw JedError("58P01", "database file does not exist: \(path)") }
            file = replacement; closed = false
        }
    }
    struct Facts {
        let cache: [UInt8]
        let durable: [UInt8]
        let pendingOffsets: [Int]
        let barrierComplete: Bool
        let writes: Int
        let pages: Set<Int>
    }
    var facts: Facts { filesystem.locked { Facts(cache: file.cache, durable: file.durable, pendingOffsets: file.pending.keys.sorted(), barrierComplete: file.barrierComplete, writes: file.writes, pages: file.pages) } }
}

extension SimFilesystem: FileOperations {
    func resolve(_ path: String, creating: Bool) throws -> String { path }
    func temporaryCreatePath(_ path: String) throws -> String { path + ".jedtmp" }
    func create(_ path: String, skipFsync: Bool) throws -> any FileBlockStore {
        try locked { let disk = try create(path); disk.skipFsync = skipFsync; return disk }
    }
    func open(_ path: String, readOnly: Bool, skipFsync: Bool) throws -> any FileBlockStore {
        try locked { let disk = try open(path); disk.readOnly = readOnly; disk.skipFsync = skipFsync; return disk }
    }
    func copyPermissions(from source: any FileBlockStore, to target: any FileBlockStore) throws {
        guard let source = source as? SimDisk, let target = target as? SimDisk,
              source.filesystem === self, target.filesystem === self else { throw Self.injected("foreign simulated byte handle") }
    }
    func syncDirectory(_ path: String, skipFsync: Bool) throws { if !skipFsync { try syncDirectory() } }
}
