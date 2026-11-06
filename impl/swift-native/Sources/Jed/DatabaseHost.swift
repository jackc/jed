/// Lifetime operations at the file-host boundary. The coordinator calls close
/// only after the writer gate is acquired and pending durable work is flushed;
/// the implementation closes file access before releasing process ownership.
internal protocol DatabaseHostLifetime: Sendable {
    var path: String { get }
    func checkOpen() throws
    func close() throws
}
