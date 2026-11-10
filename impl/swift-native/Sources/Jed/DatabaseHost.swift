/// Lifetime operations at the file-host boundary. The coordinator calls close
/// only after the writer gate is acquired and pending durable work is flushed;
/// the implementation closes file access before releasing process ownership.
internal protocol DatabaseHostLifetime: Sendable {
    var path: String { get }
    func checkOpen() throws
    /// `checkOpen` without the opener-process check, for admitting a
    /// statement. A handle inherited across fork is refused where it could
    /// write: at writer admission and at every file and lock operation
    /// (locking.md §3). A read served from memory needs no system call.
    func checkAdmitted() throws
    func close() throws
}
extension DatabaseHostLifetime {
    func checkAdmitted() throws { try checkOpen() }
}
