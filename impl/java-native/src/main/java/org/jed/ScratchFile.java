package org.jed;

import java.io.BufferedInputStream;
import java.io.BufferedOutputStream;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.EOFException;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.nio.file.Path;
import java.util.Arrays;
import org.jed.storage.BlockStore;

/** Operator-owned scratch file, with repeatable streams and one bounded shared page buffer. */
final class ScratchFile implements AutoCloseable {
    private static final int PAGE = 4096;
    private final BlockStore device;
    private final byte[] buffer = new byte[PAGE];
    private long length, cached = -1;
    private boolean dirty, closed;
    ScratchFile(Path directory, MemoryAccount account) throws IOException {
        var entropy = account == null ? null : account.scratchEntropy();
        BlockStore opened = null;
        try {
            opened = (account == null ? ScratchHost.NATIVE : account.scratchHost()).create(directory, false);
            device = entropy == null ? opened : new ScratchDevice(opened, entropy);
        } catch (IOException | RuntimeException | Error failure) {
            if (opened != null) try { opened.close(); } catch (IOException cleanup) { failure.addSuppressed(cleanup); }
            throw failure;
        }
    }
    long length() { return length; }
    private void ensureOpen() throws IOException { if (closed) throw new IOException("scratch file closed"); }
    private void flushPage() throws IOException {
        if (!dirty) return;
        int count = (int) Math.min(PAGE, length - cached * PAGE);
        if (count > 0) device.writeAt(cached * PAGE, count == PAGE ? buffer : Arrays.copyOf(buffer, count));
        dirty = false;
    }
    private void page(long index) throws IOException {
        ensureOpen(); if (cached == index) return;
        flushPage(); Arrays.fill(buffer, (byte) 0); cached = -1;
        long start = Math.multiplyExact(index, PAGE), available = device.size() - start;
        if (available > 0) {
            byte[] bytes = device.readAt(start, (int) Math.min(PAGE, available));
            System.arraycopy(bytes, 0, buffer, 0, bytes.length);
        }
        cached = index;
    }
    private void readAt(long offset, byte[] result, int start, int count) throws IOException {
        ensureOpen(); if (BlockStore.checkedEnd(offset, count) > length) throw new EOFException("short scratch read");
        for (int done = 0; done < count;) {
            long at = offset + done; page(at / PAGE); int within = (int) (at % PAGE), n = Math.min(PAGE - within, count - done);
            System.arraycopy(buffer, within, result, start + done, n); done += n;
        }
    }
    private void writeAt(long offset, byte[] bytes, int start, int count) throws IOException {
        ensureOpen(); long end = BlockStore.checkedEnd(offset, count);
        for (int done = 0; done < count;) {
            long at = offset + done; page(at / PAGE); int within = (int) (at % PAGE), n = Math.min(PAGE - within, count - done);
            System.arraycopy(bytes, start + done, buffer, within, n); dirty = true; done += n;
            length = Math.max(length, at + n);
        }
        length = Math.max(length, end);
    }
    void setLength(long value) throws IOException {
        ensureOpen(); if (value < 0) throw new IOException("negative scratch length");
        if (value < length) { flushPage(); device.setSize(value); cached = -1; }
        length = value;
    }
    DataInputStream input() throws IOException {
        ensureOpen(); flushPage(); cached = -1;
        return new DataInputStream(new BufferedInputStream(new InputStream() {
            long at; boolean done;
            @Override public int read() throws IOException { byte[] one = new byte[1]; return read(one, 0, 1) < 0 ? -1 : one[0] & 255; }
            @Override public int read(byte[] bytes, int offset, int count) throws IOException {
                java.util.Objects.checkFromIndexSize(offset, count, bytes.length);
                if (done) throw new IOException("scratch reader closed");
                ensureOpen(); if (count == 0) return 0; if (at == length) return -1;
                int n = (int) Math.min(count, length - at); readAt(at, bytes, offset, n); at += n; return n;
            }
            @Override public void close() { done = true; }
        }));
    }
    DataOutputStream output() throws IOException {
        ensureOpen();
        return new DataOutputStream(new BufferedOutputStream(new OutputStream() {
            long at; boolean done;
            @Override public void write(int value) throws IOException { write(new byte[] {(byte) value}, 0, 1); }
            @Override public void write(byte[] bytes, int offset, int count) throws IOException {
                java.util.Objects.checkFromIndexSize(offset, count, bytes.length);
                if (done) throw new IOException("scratch writer closed");
                writeAt(at, bytes, offset, count); at += count;
            }
            @Override public void flush() throws IOException { ensureOpen(); flushPage(); }
            @Override public void close() throws IOException { if (!done) { flush(); done = true; } }
        }));
    }
    Random cursor() { return new Random(); }
    final class Random {
        private long at;
        private final byte[] word = new byte[8];
        void seek(long offset) throws IOException { ensureOpen(); if (offset < 0) throw new IOException("negative scratch seek"); at = offset; }
        long length() { return ScratchFile.this.length(); }
        void setLength(long value) throws IOException { ScratchFile.this.setLength(value); }
        void skipBytes(int count) throws IOException { seek(Math.addExact(at, count)); }
        long readLong() throws IOException { readAt(at, word, 0, 8); at += 8; return org.jed.storage.PageFormat.u64(word, 0); }
        int readInt() throws IOException { readAt(at, word, 0, 4); at += 4; return (int) org.jed.storage.PageFormat.u32(word, 0); }
        void readFully(byte[] bytes) throws IOException { readAt(at, bytes, 0, bytes.length); at += bytes.length; }
        void writeLong(long value) throws IOException { org.jed.storage.PageFormat.put64(word, 0, value); writeAt(at, word, 0, 8); at += 8; }
        void writeInt(int value) throws IOException { org.jed.storage.PageFormat.put32(word, 0, Integer.toUnsignedLong(value)); writeAt(at, word, 0, 4); at += 4; }
        void write(byte[] bytes) throws IOException { writeAt(at, bytes, 0, bytes.length); at += bytes.length; }
    }
    @Override public void close() throws IOException {
        if (closed) return; closed = true;
        IOException failure = null;
        try { device.close(); } catch (IOException error) { failure = error; }
        Arrays.fill(buffer, (byte) 0);
        if (failure != null) throw failure;
    }
}
