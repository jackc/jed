package org.jed;

import java.io.EOFException;
import java.io.IOException;
import java.security.GeneralSecurityException;
import java.util.Arrays;
import java.util.HashMap;
import java.util.HashSet;
import java.util.Map;
import java.util.Set;
import java.util.function.Consumer;
import org.jed.storage.BlockStore;
import org.jed.storage.PageFormat;

/** Handle-private authenticated scratch blocks, including replay-safe overwrite and truncation. */
final class ScratchDevice implements BlockStore {
    static final int BLOCK_BYTES = 4096, STORED_BYTES = 4136;
    private final BlockStore stored;
    private final Crypto.Xaes cipher;
    private final Consumer<byte[]> entropy;
    private final byte[] fileId = new byte[16];
    // encryption.md §6 deliberately keeps replay counters outside logical memory
    // accounting: file limits bound them, and per-core framing must not alter admission.
    private final Map<Long, Long> counters = new HashMap<>();
    private final Set<Long> valid = new HashSet<>();
    private long length;
    private boolean closed, poisoned;
    ScratchDevice(BlockStore stored, Consumer<byte[]> entropy) throws IOException {
        if (stored.size() != 0) throw new IOException("scratch device requires an empty file");
        this.stored = stored; this.entropy = java.util.Objects.requireNonNull(entropy);
        byte[] key = new byte[32]; entropy.accept(key); entropy.accept(fileId); cipher = new Crypto.Xaes(key); Arrays.fill(key, (byte) 0);
    }
    @Override public synchronized byte[] readAt(long offset, int count) throws IOException {
        ensureOpen(); if (BlockStore.checkedEnd(offset, count) > length) throw new EOFException("short scratch read");
        byte[] result = new byte[count];
        for (int done = 0; done < count;) {
            long at = offset + done, block = at / BLOCK_BYTES; int start = (int) (at % BLOCK_BYTES), n = Math.min(BLOCK_BYTES - start, count - done);
            byte[] bytes = readBlock(block); System.arraycopy(bytes, start, result, done, n); done += n;
        }
        return result;
    }
    @Override public synchronized void writeAt(long offset, byte[] bytes) throws IOException {
        ensureOpen(); long end = BlockStore.checkedEnd(offset, bytes.length); if (bytes.length == 0) return;
        try {
            for (int done = 0; done < bytes.length;) {
                long at = offset + done, block = at / BLOCK_BYTES; int start = (int) (at % BLOCK_BYTES), n = Math.min(BLOCK_BYTES - start, bytes.length - done);
                byte[] plain = start == 0 && n == BLOCK_BYTES ? new byte[BLOCK_BYTES] : readBlock(block);
                System.arraycopy(bytes, done, plain, start, n); writeBlock(block, plain); done += n;
            }
            length = Math.max(length, end);
        } catch (IOException | RuntimeException | Error failure) { poisoned = true; throw failure; }
    }
    @Override public synchronized void setSize(long size) throws IOException {
        ensureOpen(); if (size < 0) throw new IOException("negative scratch length");
        if (size >= length) { length = size; return; }
        try {
            long block = size / BLOCK_BYTES; int cut = (int) (size % BLOCK_BYTES);
            if (cut != 0 && valid.contains(block)) {
                byte[] bytes = readBlock(block); Arrays.fill(bytes, cut, bytes.length, (byte) 0); writeBlock(block, bytes);
            }
            long firstRemoved = block + (cut == 0 ? 0 : 1);
            valid.removeIf(id -> id >= firstRemoved); // Counters survive truncation and regrowth.
            long physical = Math.multiplyExact(firstRemoved, STORED_BYTES);
            if (physical < stored.size()) stored.setSize(physical); length = size;
        } catch (IOException | RuntimeException | Error failure) { poisoned = true; throw failure; }
    }
    @Override public synchronized long size() throws IOException { ensureOpen(); return length; }
    @Override public synchronized void sync() throws IOException { ensureOpen(); stored.sync(); }
    @Override public synchronized void close() throws IOException { if (closed) return; closed = true; stored.close(); }
    private byte[] readBlock(long block) throws IOException {
        if (!valid.contains(block)) return new byte[BLOCK_BYTES];
        byte[] bytes = stored.readAt(physicalOffset(block), STORED_BYTES);
        byte[] sealed = Arrays.copyOf(bytes, BLOCK_BYTES + 16); System.arraycopy(bytes, BLOCK_BYTES + 24, sealed, BLOCK_BYTES, 16);
        try { return cipher.open(Arrays.copyOfRange(bytes, BLOCK_BYTES, BLOCK_BYTES + 24), sealed, associated(block, counters.get(block))); }
        catch (GeneralSecurityException failure) { throw new IOException("scratch block authentication failed"); }
    }
    private void writeBlock(long block, byte[] plain) throws IOException {
        long counter = counters.getOrDefault(block, 0L) + 1;
        if (counter == 0) throw new IOException("scratch block write counter exhausted");
        counters.put(block, counter);
        byte[] nonce = new byte[24]; entropy.accept(nonce); byte[] sealed = cipher.seal(nonce, plain, associated(block, counter));
        byte[] bytes = new byte[STORED_BYTES]; System.arraycopy(sealed, 0, bytes, 0, BLOCK_BYTES);
        System.arraycopy(nonce, 0, bytes, BLOCK_BYTES, 24); System.arraycopy(sealed, BLOCK_BYTES, bytes, BLOCK_BYTES + 24, 16);
        stored.writeAt(physicalOffset(block), bytes); valid.add(block);
    }
    private byte[] associated(long block, long counter) {
        byte[] bytes = new byte[38]; bytes[0] = 'J'; bytes[1] = 'E'; bytes[2] = 'D'; bytes[3] = 'B'; bytes[4] = 1; bytes[5] = 3;
        System.arraycopy(fileId, 0, bytes, 6, 16); PageFormat.put64(bytes, 22, block); PageFormat.put64(bytes, 30, counter); return bytes;
    }
    private static long physicalOffset(long block) throws IOException {
        if (block < 0 || block > (Long.MAX_VALUE - STORED_BYTES) / STORED_BYTES) throw new IOException("scratch offset exceeds host range");
        return block * STORED_BYTES;
    }
    private void ensureOpen() throws IOException { if (closed || poisoned) throw new IOException(closed ? "scratch device is closed" : "scratch device failed"); }
}
