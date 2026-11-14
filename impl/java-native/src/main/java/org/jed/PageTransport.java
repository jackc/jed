package org.jed;

import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;

/** Host-owned duplex transport factory, invoked again at a disconnected transaction boundary. */
@FunctionalInterface
public interface PageTransport {
    Connection open() throws IOException;
    record Connection(InputStream input, OutputStream output, AutoCloseable owner) implements AutoCloseable {
        public Connection { java.util.Objects.requireNonNull(input); java.util.Objects.requireNonNull(output); java.util.Objects.requireNonNull(owner); }
        @Override public void close() throws IOException {
            try { owner.close(); }
            catch (IOException failure) { throw failure; }
            catch (Exception failure) { throw new IOException("page transport close failed", failure); }
        }
    }
}
