#if os(Linux)
import Foundation
import Glibc

/// Coordination devices supplied by a host. Database ownership and protocol
/// decisions remain in ServiceDatabaseHost and PageServiceOwner.
internal protocol ServicePathClaim: AnyObject, Sendable {
    var path: String { get }
    func checkOpen() throws
    /// `checkOpen` without the opener-process check.
    func checkAdmitted() throws
    func close()
}
extension ServicePathClaim {
    func checkAdmitted() throws { try checkOpen() }
}
internal protocol ServiceOwnership: FileOwnership {
    func tryOwnership() throws -> Bool
    func writeOwner(_ record: ServiceOwnerRecord) throws
}
internal protocol ServiceListener: AnyObject, Sendable {
    func acceptConnection() throws -> any ServiceFrameTransport
    func close()
}
internal protocol ServiceHostPlatform: AnyObject, Sendable {
    var operations: any FileOperations { get }
    var uid: UInt32 { get }
    var pid: UInt32 { get }
    var backgroundElection: Bool { get }
    var ownerTimers: Bool { get }
    func incarnation() throws -> [UInt8]
    func dispatch(_ body: @escaping @Sendable () -> Void)
    func claim(_ path: String) throws -> any ServicePathClaim
    func prepare(path: String, timeout: Int64, claim: any ServicePathClaim) throws -> any ServiceOwnership
    func openFile(path: String, options: OpenOptions, catalogID: UInt64, bundle: any ServiceOwnership,
                  unlockedCodec: EncryptedPageCodec?, structural: Bool, random: any RandomSource) throws -> Database
    func owner(_ path: String) -> ServiceOwnerRecord?
    func endpoint(path: String, incarnation: [UInt8]) -> String?
    func listen(_ endpoint: String) throws -> any ServiceListener
    func connect(_ endpoint: String, deadline: ContinuousClock.Instant) throws -> any ServiceFrameTransport
    func remove(_ endpoint: String)
    func probeWritable(_ path: String) throws
    func now() -> Int64
    func pause()
    func scratch(sealed: Bool, random: any RandomSource) -> any ScratchFileHost
}

extension FilePathClaim: ServicePathClaim {}
extension FileLockBundle: ServiceOwnership {}
extension UnixServiceSocket: ServiceListener {
    func acceptConnection() throws -> any ServiceFrameTransport { try accept() }
}

internal final class NativeServiceListener: ServiceListener, Sendable {
    let socket: UnixServiceSocket
    init(_ socket: UnixServiceSocket) { self.socket = socket }
    func acceptConnection() throws -> any ServiceFrameTransport { try socket.accept() }
    func close() { socket.close() }
}

internal final class NativeServiceHostPlatform: ServiceHostPlatform, Sendable {
    let operations: any FileOperations = NativeFileOperations()
    var uid: UInt32 { UInt32(getuid()) }
    var pid: UInt32 { UInt32(getpid()) }
    var backgroundElection: Bool { true }
    var ownerTimers: Bool { true }
    func incarnation() throws -> [UInt8] { try OSRandomSource().bytes(count: 16) }
    func dispatch(_ body: @escaping @Sendable () -> Void) { Thread.detachNewThread(body) }
    func claim(_ path: String) throws -> any ServicePathClaim { try FilePathClaim.acquire(path) }
    func prepare(path: String, timeout: Int64, claim: any ServicePathClaim) throws -> any ServiceOwnership {
        guard let claim = claim as? FilePathClaim else { throw JedError("58030", "invalid native path claim") }
        return try FileLockBundle.prepare(path: path, timeoutMilliseconds: timeout, claim: claim)
    }
    func openFile(path: String, options: OpenOptions, catalogID: UInt64, bundle: any ServiceOwnership,
                  unlockedCodec: EncryptedPageCodec?, structural: Bool, random: any RandomSource) throws -> Database {
        guard let bundle = bundle as? FileLockBundle else { throw JedError("58030", "invalid native ownership bundle") }
        return try FileDatabaseHost.open(path: path, options: options, catalogID: catalogID, bundle: bundle,
            unlockedCodec: unlockedCodec, structural: structural)
    }
    func owner(_ path: String) -> ServiceOwnerRecord? { FileLockBundle.readOwner(path) }
    func endpoint(path: String, incarnation: [UInt8]) -> String? { PageServiceEndpoint.choose(database: path, incarnation: incarnation) }
    func listen(_ endpoint: String) throws -> any ServiceListener { NativeServiceListener(try UnixServiceSocket.listen(path: endpoint)) }
    func connect(_ endpoint: String, deadline: ContinuousClock.Instant) throws -> any ServiceFrameTransport {
        try UnixServiceSocket.connect(path: endpoint, deadline: deadline)
    }
    func remove(_ endpoint: String) { PageServiceEndpoint.remove(endpoint) }
    func probeWritable(_ path: String) throws {
        let fd = path.withCString { Glibc.open($0, O_RDWR | O_CLOEXEC) }
        guard fd >= 0 else { throw JedError("25006", "database file cannot be opened read-write") }
        _ = Glibc.close(fd)
    }
    func now() -> Int64 { Int64(clamping: DispatchTime.now().uptimeNanoseconds / 1000) }
    func pause() { Thread.sleep(forTimeInterval: 0.002) }
    func scratch(sealed: Bool, random: any RandomSource) -> any ScratchFileHost { NativeScratchFileHost(sealed: sealed) }
}

internal enum ServiceHostTime {
    static func deadline(_ now: Int64, milliseconds: UInt64) -> Int64 {
        let interval = milliseconds > UInt64(Int64.max / 1000) ? Int64.max : Int64(milliseconds) * 1000
        let (sum, overflow) = now.addingReportingOverflow(interval)
        return overflow ? Int64.max : sum
    }
}
#endif
