#if os(Linux)
import Foundation
import Glibc
import Synchronization

internal enum PageStreamServer {
    /// The first frame alone is interpreted here. After credential removal,
    /// both directions relay complete bounded raw frames unchanged.
    static func serve(path: String, readOnly: Bool, stream: PageFrameStream) throws {
        defer { stream.close() }
        var request: UInt32 = 1
        var owned: Database?
        defer { try? owned?.close() }
        do {
            guard let first = try stream.readRaw() else { return }
            if first.count >= 9 { var header = ByteReader(first); _ = try header.take(5); request = try header.u32() }
            let frame = try ServiceFrame.decode(first, pageSize: 0)
            guard request != 0, case .hello(var hello) = frame.message else {
                throw PageServiceProtocol.violation("first stream request is not hello")
            }
            let credential = hello.credential; hello.credential = nil
            if readOnly { hello.flags |= 1 }
            let deadline = ContinuousClock.now.advanced(by: .seconds(5))
            let normalized = try FileLockBundle.normalize(path, creating: false)
            while true {
                if let owner = FileLockBundle.readOwner(normalized), owner.serving, owner.uid == UInt32(getuid()),
                   let socket = try? UnixServiceSocket.connect(path: owner.endpoint, deadline: deadline) {
                    try socket.send(ServiceFrame(request: request, message: .hello(hello)), pageSize: 0, deadline: deadline)
                    relay(stream, socket: socket)
                    return
                }
                if ContinuousClock.now >= deadline { throw JedError("08006", "could not reach a page-service owner") }
                if owned != nil { Thread.sleep(forTimeInterval: 0.002); continue }
                let options = OpenOptions(credential: credential, readOnly: readOnly,
                    pageService: PageServiceOptions(ownership: .yield))
                let database = try ServiceDatabaseHost.open(path: normalized, options: options,
                    catalogID: Database.allocateCatalogID(), structuralOnly: true)
                if (database.host as? ServiceDatabaseHost)?.toolingRole == "owner" { owned = database }
                else { try database.close() }
                // A competing election may have won between inspection and
                // open. Either way, the next iteration dials the selected owner.
            }
        } catch {
            let error = error as? JedError ?? JedError("58030", "stream server failed")
            try? stream.send(ServiceFrame(request: request, message: .error(code: error.code,
                message: error.message, detail: error.detail ?? "", flags: 1)), pageSize: 0)
        }
    }
    private static func relay(_ stream: PageFrameStream, socket: UnixServiceSocket) {
        let inbound = DispatchGroup(); inbound.enter()
        Thread.detachNewThread {
            defer { socket.close(); stream.close(); inbound.leave() }
            do { while let bytes = try stream.readRaw() { try socket.sendRaw(bytes) } }
            catch { }
        }
        do { while let bytes = try socket.readRaw() { try stream.sendRaw(bytes) } }
        catch { }
        socket.close(); stream.close(); inbound.wait()
    }
}

extension DatabaseTooling {
    @_spi(ExperimentalTools) public static func servePageStream(path: String, readOnly: Bool = false) throws {
        let input = dup(STDIN_FILENO), output = dup(STDOUT_FILENO)
        guard input >= 0, output >= 0 else {
            if input >= 0 { _ = Glibc.close(input) }; if output >= 0 { _ = Glibc.close(output) }
            throw JedError("08006", "could not retain page stream descriptors")
        }
        let stream = try DescriptorPageTransport(input: input, output: output)
        try PageStreamServer.serve(path: path, readOnly: readOnly, stream: PageFrameStream(stream))
    }
}
#endif
