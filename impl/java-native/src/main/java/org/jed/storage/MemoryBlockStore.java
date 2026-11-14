package org.jed.storage;

import java.io.EOFException;
import java.io.IOException;
import java.nio.channels.ClosedChannelException;
import java.util.Arrays;
import java.util.HashMap;
import java.util.Map;

/** Segmented byte device; growing it never copies a complete database image. */
public final class MemoryBlockStore implements BlockStore {
    private final int blockSize;
    // Map iteration is used only for truncation, never for observable storage ordering.
    private final Map<Long, byte[]> blocks = new HashMap<>();
    private long length;
    private boolean closed;

    public MemoryBlockStore(int blockSize) {
        if (blockSize < 256 || blockSize > 65536 || (blockSize & (blockSize - 1)) != 0) {
            throw new IllegalArgumentException("invalid memory block size");
        }
        this.blockSize = blockSize;
    }

    public MemoryBlockStore() { this(8192); }

    @Override public synchronized byte[] readAt(long offset, int count) throws IOException {
        ensureOpen();
        if (BlockStore.checkedEnd(offset, count) > length) {
            throw new EOFException("short byte-device read");
        }
        byte[] out = new byte[count];
        for (int done = 0; done < count;) {
            long at = offset + done;
            int local = (int) (at % blockSize);
            int n = Math.min(blockSize - local, count - done);
            byte[] block = blocks.get(at / blockSize);
            if (block != null) System.arraycopy(block, local, out, done, n);
            done += n;
        }
        return out;
    }

    @Override public synchronized void writeAt(long offset, byte[] bytes) throws IOException {
        ensureOpen();
        long end = BlockStore.checkedEnd(offset, bytes.length);
        // A zero-length write does not extend a file.
        if (bytes.length == 0) return;
        for (int done = 0; done < bytes.length;) {
            long at = offset + done;
            int local = (int) (at % blockSize);
            int n = Math.min(blockSize - local, bytes.length - done);
            long index = at / blockSize;
            byte[] block = blocks.computeIfAbsent(index, ignored -> new byte[blockSize]);
            System.arraycopy(bytes, done, block, local, n);
            done += n;
        }
        length = Math.max(length, end);
    }

    @Override public synchronized long size() throws IOException { ensureOpen(); return length; }

    @Override public synchronized void setSize(long newLength) throws IOException {
        ensureOpen();
        if (newLength < 0) throw new IOException("negative byte-device size");
        if (newLength < length) {
            long last = newLength / blockSize;
            int tail = (int) (newLength % blockSize);
            blocks.keySet().removeIf(index -> index > last || (index == last && tail == 0));
            byte[] kept = blocks.get(last);
            if (kept != null) Arrays.fill(kept, tail, blockSize, (byte) 0);
        }
        length = newLength;
    }

    @Override public synchronized void sync() throws IOException { ensureOpen(); }

    @Override public synchronized void close() {
        blocks.clear();
        length = 0;
        closed = true;
    }

    private void ensureOpen() throws ClosedChannelException {
        if (closed) throw new ClosedChannelException();
    }
}
