import Synchronization

internal protocol DatabaseRoleCoordinator: AnyObject, Sendable {
    func awaitAdmission() throws
    func awaitWriteAdmission(timeoutMilliseconds: UInt64, cancellation: CancellationToken?) throws
}
internal final class WeakRoleCoordinator: Sendable {
    private struct State { weak var value: (any DatabaseRoleCoordinator)? }
    private let state = Mutex(State())
    func install(_ value: any DatabaseRoleCoordinator) { state.withLock { $0.value = value } }
    var value: (any DatabaseRoleCoordinator)? { state.withLock { $0.value } }
}

internal protocol CoordinatedDatabaseClose: DatabaseHostLifetime {
    var isClosed: Bool { get }
    func closeDatabase(_ core: DatabaseCore) throws
}
