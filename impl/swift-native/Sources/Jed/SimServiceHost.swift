#if os(Linux)
import Foundation

/// Resident coordination devices shared by simulated processes. Presence owns
/// a file; the serialized owner record only locates its endpoint.
internal final class SimServiceWorld: @unchecked Sendable {
    let filesystem: SimFilesystem, clock: ManualClock, network = SimNetwork()
    fileprivate let lock = NSRecursiveLock()
    fileprivate struct ClaimKey: Hashable { let process: Int; let path: String }
    fileprivate var claims: [ClaimKey: SimPathClaim] = [:]
    fileprivate var owners: [String: SimOwnership] = [:]
    fileprivate var bundles: [ObjectIdentifier: SimOwnership] = [:]
    init(filesystem: SimFilesystem, clock: ManualClock) { self.filesystem = filesystem; self.clock = clock }
    func owner(_ path: String) -> Int? { lock.lock(); defer { lock.unlock() }; return owners[path]?.process }
    func kill(_ process: Int) {
        network.kill(process)
        lock.lock(); defer { lock.unlock() }
        for bundle in Array(bundles.values) where bundle.process == process { bundle.close() }
        for claim in Array(claims.values) where claim.process == process { claim.close() }
    }
    func pause() {
        // Engine deadlines use this same monotonic simulated clock. Yield only
        // lets already-started protocol readers react; it does not add time.
        lock.lock()
        let (next, overflow) = clock.microseconds().addingReportingOverflow(2000)
        clock.set(overflow ? Int64.max : next)
        lock.unlock()
        Thread.sleep(forTimeInterval: 0.00001)
    }
}

internal final class SimServicePlatform: ServiceHostPlatform, @unchecked Sendable {
    let world: SimServiceWorld, process: Int
    private let scratchRandom: any RandomSource
    private let coordinationRandom: any RandomSource
    init(world: SimServiceWorld, process: Int, scratchSeed: UInt64) {
        self.world = world; self.process = process; scratchRandom = SeededRandomSource(seed: scratchSeed)
        coordinationRandom = SeededRandomSource(seed: scratchSeed ^ 0x5345525649434500)
    }
    var operations: any FileOperations { world.filesystem }
    var uid: UInt32 { 1000 }
    var pid: UInt32 { UInt32(process) }
    var backgroundElection: Bool { false }
    var ownerTimers: Bool { false }
    func incarnation() throws -> [UInt8] { try coordinationRandom.bytes(count: 16) }
    func dispatch(_ body: @escaping @Sendable () -> Void) { world.network.dispatch(body) }
    func claim(_ path: String) throws -> any ServicePathClaim {
        world.lock.lock(); defer { world.lock.unlock() }
        let key = SimServiceWorld.ClaimKey(process: process, path: path)
        guard world.claims[key] == nil else { throw JedError("55006", "database is already open in simulated process") }
        let claim = SimPathClaim(world: world, path: path, process: process)
        world.claims[key] = claim; return claim
    }
    func prepare(path: String, timeout: Int64, claim: any ServicePathClaim) throws -> any ServiceOwnership {
        guard timeout >= 0 else { throw JedError("22023", "negative file lock timeout") }
        try claim.checkOpen()
        guard let claim = claim as? SimPathClaim, claim.world === world, claim.path == path, claim.process == process else {
            throw JedError("55006", "simulated claim does not match ownership bundle")
        }
        let bundle = SimOwnership(world: world, path: path, process: process)
        world.lock.lock(); world.bundles[ObjectIdentifier(bundle)] = bundle; world.lock.unlock()
        return bundle
    }
    func openFile(path: String, options: OpenOptions, catalogID: UInt64, bundle: any ServiceOwnership,
                  unlockedCodec: EncryptedPageCodec?, structural: Bool, random: any RandomSource) throws -> Database {
        try FileDatabaseHost.openResolved(path: path, options: options, catalogID: catalogID, operations: world.filesystem,
            scratch: scratch(sealed: unlockedCodec != nil || options.credential != nil || options.scratchEncryption == .on, random: random),
            bundle: bundle, unlockedCodec: unlockedCodec, structural: structural)
    }
    func owner(_ path: String) -> ServiceOwnerRecord? { world.network.owner(path) }
    func endpoint(path: String, incarnation: [UInt8]) -> String? {
        path + ".p\(process)." + incarnation.map { String(format: "%02x", $0) }.joined()
    }
    func listen(_ endpoint: String) throws -> any ServiceListener { try world.network.listen(endpoint, process: process) }
    func connect(_ endpoint: String, deadline: ContinuousClock.Instant) throws -> any ServiceFrameTransport {
        try world.network.connect(endpoint, process: process)
    }
    func remove(_ endpoint: String) { world.network.remove(endpoint) }
    func probeWritable(_ path: String) throws { guard world.filesystem.facts(path) != nil else { throw JedError("58P01", "simulated database is missing") } }
    func now() -> Int64 { world.clock.microseconds() }
    func pause() { world.pause() }
    func scratch(sealed: Bool, random: any RandomSource) -> any ScratchFileHost {
        SimScratchFileHost(filesystem: world.filesystem, sealed: sealed, random: scratchRandom)
    }
}

fileprivate final class SimPathClaim: ServicePathClaim, @unchecked Sendable {
    weak var world: SimServiceWorld?
    let path: String, process: Int
    var closed = false
    init(world: SimServiceWorld, path: String, process: Int) { self.world = world; self.path = path; self.process = process }
    func checkOpen() throws {
        guard let world else { throw JedError("55006", "simulated claim ended") }
        world.lock.lock(); defer { world.lock.unlock() }
        guard !closed else { throw JedError("55006", "simulated claim is closed") }
    }
    func close() {
        guard let world else { return }; world.lock.lock(); defer { world.lock.unlock() }
        guard !closed else { return }; closed = true
        world.claims.removeValue(forKey: SimServiceWorld.ClaimKey(process: process, path: path))
    }
}

fileprivate final class SimOwnership: ServiceOwnership, @unchecked Sendable {
    weak var world: SimServiceWorld?
    let path: String, process: Int
    var closed = false, owns = false
    init(world: SimServiceWorld, path: String, process: Int) { self.world = world; self.path = path; self.process = process }
    func checkOpen() throws {
        guard let world else { throw JedError("55006", "simulated ownership ended") }
        world.lock.lock(); defer { world.lock.unlock() }
        guard !closed else { throw JedError("55000", "simulated ownership is closed") }
    }
    func tryOwnership() throws -> Bool {
        guard let world else { throw JedError("55006", "simulated ownership ended") }
        world.lock.lock(); defer { world.lock.unlock() }
        guard !closed else { throw JedError("55006", "simulated ownership is closed") }
        if owns { return true }
        guard world.owners[path] == nil else { return false }
        world.owners[path] = self; owns = true; return true
    }
    func writeOwner(_ record: ServiceOwnerRecord) throws {
        guard let world else { throw JedError("55006", "simulated ownership ended") }
        world.lock.lock(); defer { world.lock.unlock() }
        guard !closed, owns else { throw JedError("55006", "owner record requires simulated ownership") }
        try world.network.writeOwner(path, record)
    }
    func close() {
        guard let world else { return }; world.lock.lock(); defer { world.lock.unlock() }
        guard !closed else { return }; closed = true
        if world.owners[path] === self { world.owners.removeValue(forKey: path) }
        world.bundles.removeValue(forKey: ObjectIdentifier(self))
    }
}
#endif
