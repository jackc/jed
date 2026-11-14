package org.jed;

import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.UnixDomainSocketAddress;
import java.nio.channels.SocketChannel;
import java.nio.file.Path;
import org.jed.host.ExclusiveFileLock;
import org.jed.host.PageConnection;
import org.jed.host.PageProtocol;
import org.jed.storage.PageFormat;
import static org.jed.host.PageProtocol.*;

/** Explicit host stream-serving seam: relay to an owner, or hold a yielding local owner. */
public final class PageRelay {
    private PageRelay() { }
    public static void serve(Path path, InputStream input, OutputStream output) throws IOException {
        serve(path, input, output, false);
    }
    public static void serve(Path path, InputStream input, OutputStream output, boolean readOnly) throws IOException {
        Frame first = PageProtocol.read(input, 0);
        if (first == null) return;
        Database database = null;
        try (var claim = ExclusiveFileLock.claim(path, false)) {
            if (!(first.message() instanceof Hello hello)) throw new Violation("first stream message must be hello");
            if (readOnly) hello = new Hello(hello.minimum(), hello.maximum(), hello.format(), hello.flags() | 1, hello.client(), hello.credential());
            if (hello.minimum() > VERSION || hello.maximum() < VERSION || hello.format() != PageFormat.VERSION)
                throw new SqlException("0A000", "unsupported page-service protocol or file format");
            var owner = claim.ownerRecord(); Path endpoint;
            if (owner != null) {
                if (owner.uid() != ExclusiveFileLock.uid() || !owner.serving()) throw new SqlException("55006", "file owner cannot serve this transport");
                endpoint = Path.of(owner.endpoint());
            } else {
                claim.close();
                Credential credential = switch (hello.credential().kind()) {
                    case 0 -> null;
                    case 1 -> Credential.passphrase(hello.credential().passphrase());
                    case 2 -> Credential.hostKey(hello.credential().key());
                    default -> throw new Violation("invalid stream credential");
                };
                database = Database.open(path, FileOptions.builder().ownership(FileOptions.Ownership.YIELD)
                    .credential(credential).readOnly((hello.flags() & 1) != 0).shutdownGraceMillis(0).build());
                endpoint = database.relayEndpoint();
            }
            try (SocketChannel channel = SocketChannel.open(java.net.StandardProtocolFamily.UNIX)) {
                channel.connect(UnixDomainSocketAddress.of(endpoint));
                OutputStream upstream = PageConnection.output(channel);
                PageProtocol.write(upstream, new Frame(first.requestId(), new Hello(hello.minimum(), hello.maximum(), hello.format(), hello.flags(), hello.client(), WireCredential.none())), 0);
                Thread sender = Thread.ofVirtual().name("jed-stream-relay").start(() -> {
                    try { input.transferTo(upstream); }
                    catch (IOException ignored) { }
                    finally { try { channel.close(); } catch (IOException ignored) { } }
                });
                try { PageConnection.input(channel).transferTo(output); output.flush(); }
                finally { channel.close(); sender.interrupt(); }
            }
        } catch (SqlException failure) {
            PageProtocol.write(output, new Frame(first.requestId(), new PageProtocol.Error(failure.sqlState(), failure.getMessage(), "", 1)), 0);
        } finally { if (database != null) database.close(); }
    }
}
