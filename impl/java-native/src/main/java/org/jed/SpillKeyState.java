package org.jed;

import java.io.IOException;
import java.nio.file.Path;
import java.util.Arrays;
import java.util.Iterator;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/** Exact keyed state. Disk buckets grow by linear hashing while first occurrence uses a disk ordinal vector. */
final class SpillKeyState implements AutoCloseable {
    record Value(byte[] bytes, long ordinal) {}
    record Entry(byte[] key, Value value) {}
    interface Cursor extends AutoCloseable { Entry next(); default void discardCurrent() {} @Override default void close() {} }
    private static final class Key {
        final byte[] bytes;
        final int hash;
        Key(byte[] bytes) { this.bytes = bytes; hash = Arrays.hashCode(bytes); }
        @Override public int hashCode() { return hash; }
        @Override public boolean equals(Object value) { return value instanceof Key key && Arrays.equals(bytes, key.bytes); }
    }
    private record Resident(byte[] bytes, long logical, long ordinal) {}
    private final Path scratch;
    private final long workMem;
    private final MemoryAccount account;
    private final Map<Key, Resident> resident = new LinkedHashMap<>();
    private final ScratchFile[] files = new ScratchFile[3];
    private ScratchFile.Random data, directory, ordinals;
    private long count, held, base = 64, split;
    private boolean closed;
    SpillKeyState(Path scratch, long workMem, MemoryAccount account) {
        if (workMem < 0) throw new IllegalArgumentException("negative work_mem");
        this.scratch = scratch; this.workMem = workMem; this.account = account;
    }
    long size() { return count; }
    long residentBytes() { return held; }
    boolean spilled() { return data != null; }
    long buckets() { return base + split; }
    Value get(byte[] key) {
        ensureOpen();
        if (data == null) {
            Resident item = resident.get(new Key(key));
            return item == null ? null : new Value(item.bytes.clone(), item.ordinal);
        }
        try {
            long pointer = find(key, hash(key));
            if (pointer == 0) return null;
            return value(pointer - 1);
        } catch (IOException failure) { throw fail(failure); }
    }
    boolean contains(byte[] key) {
        ensureOpen();
        if (data == null) return resident.containsKey(new Key(key));
        try { return find(key, hash(key)) != 0; } catch (IOException failure) { throw fail(failure); }
    }
    /** Release a finished lookup owner's resident value. Disk bytes remain reclaimable scratch. */
    void discardResident(byte[] key) {
        ensureOpen(); if (data != null) return;
        Resident removed = resident.remove(new Key(key));
        if (removed != null) { held -= removed.logical; if (account != null) account.release(removed.logical); }
    }
    long put(byte[] key, byte[] value, long logicalBytes) {
        ensureOpen();
        if (logicalBytes < 0) throw new IllegalArgumentException("negative logical state size");
        try {
            if (data == null) {
                Key lookup = new Key(key); Resident prior = resident.get(lookup);
                long delta = logicalBytes - (prior == null ? 0 : prior.logical);
                boolean admitted = delta <= 0 || account == null || account.tryReserve(delta);
                if (!admitted && (scratch == null || workMem == 0)) throw MemoryAccount.exceeded();
                if (admitted) {
                    if (delta < 0 && account != null) account.release(-delta);
                    held = Math.addExact(held, delta);
                    if (scratch == null || workMem == 0 || held <= workMem) {
                        long ordinal = prior == null ? count++ : prior.ordinal;
                        resident.put(prior == null ? new Key(key.clone()) : lookup, new Resident(value.clone(), logicalBytes, ordinal));
                        return ordinal;
                    }
                }
                forceDisk();
            }
            return putDisk(key, value);
        } catch (IOException failure) { throw fail(failure); }
        catch (RuntimeException failure) { cleanup(failure); throw failure; }
    }
    private void forceDisk() throws IOException {
        if (account != null) account.beforeSpill();
        files[0] = new ScratchFile(scratch, account); data = files[0].cursor();
        files[1] = new ScratchFile(scratch, account); directory = files[1].cursor();
        files[2] = new ScratchFile(scratch, account); ordinals = files[2].cursor();
        directory.setLength(base * 8); long expected = count; count = 0;
        for (var item : resident.entrySet()) {
            count = item.getValue().ordinal;
            long ordinal = putDisk(item.getKey().bytes, item.getValue().bytes);
            if (ordinal != item.getValue().ordinal) throw new IllegalStateException("state ordinal changed during spill");
        }
        count = expected; ordinals.setLength(count * 8);
        resident.clear(); release();
    }
    private static long hash(byte[] key) {
        long hash = 0xcbf29ce484222325L;
        for (byte value : key) hash = (hash ^ (value & 255L)) * 0x100000001b3L;
        return hash;
    }
    private long bucket(long hash) { long bucket = hash & (base - 1); return bucket < split ? hash & (2 * base - 1) : bucket; }
    private long pointer(long bucket) throws IOException { directory.seek(bucket * 8); return directory.readLong(); }
    private void pointer(long bucket, long value) throws IOException { directory.seek(bucket * 8); directory.writeLong(value); }
    private long find(byte[] key, long hash) throws IOException {
        long pointer = pointer(bucket(hash));
        while (pointer != 0) {
            data.seek(pointer - 1); long next = data.readLong(), storedHash = data.readLong();
            data.skipBytes(20); int keyLength = data.readInt();
            if (storedHash == hash && keyLength == key.length) {
                byte[] candidate = new byte[keyLength]; data.readFully(candidate);
                if (Arrays.equals(key, candidate)) return pointer;
            }
            pointer = next;
        }
        return 0;
    }
    private Value value(long offset) throws IOException {
        data.seek(offset + 16); long ordinal = data.readLong(), position = data.readLong(); int size = data.readInt();
        data.seek(position); byte[] bytes = new byte[size]; data.readFully(bytes);
        return new Value(bytes, ordinal);
    }
    private long putDisk(byte[] key, byte[] bytes) throws IOException {
        long hash = hash(key), found = find(key, hash), end = data.length();
        if (found != 0) {
            data.seek(found - 1 + 16); long ordinal = data.readLong();
            data.seek(end); data.write(bytes);
            data.seek(found - 1 + 24); data.writeLong(end); data.writeInt(bytes.length);
            return ordinal;
        }
        long bucket = bucket(hash), head = pointer(bucket), ordinal = count;
        data.seek(end); data.writeLong(head); data.writeLong(hash); data.writeLong(ordinal);
        data.writeLong(Math.addExact(end + 40, key.length)); data.writeInt(bytes.length); data.writeInt(key.length);
        data.write(key); data.write(bytes); pointer(bucket, end + 1);
        ordinals.seek(ordinal * 8); ordinals.writeLong(end + 1); count = Math.incrementExact(count);
        if (count > 4 * (base + split)) splitBucket();
        return ordinal;
    }
    private void splitBucket() throws IOException {
        long oldBucket = split, newBucket = base + split;
        directory.setLength((newBucket + 1) * 8);
        long pointer = pointer(oldBucket), oldHead = 0, newHead = 0;
        while (pointer != 0) {
            data.seek(pointer - 1); long next = data.readLong(), hash = data.readLong();
            boolean moved = (hash & (2 * base - 1)) == newBucket;
            data.seek(pointer - 1); data.writeLong(moved ? newHead : oldHead);
            if (moved) newHead = pointer; else oldHead = pointer;
            pointer = next;
        }
        pointer(oldBucket, oldHead); pointer(newBucket, newHead);
        if (++split == base) { base *= 2; split = 0; }
    }
    Cursor cursor() {
        ensureOpen();
        if (data == null) {
            Iterator<Map.Entry<Key, Resident>> iterator = resident.entrySet().iterator();
            return new Cursor() {
                private Resident current;
                public Entry next() { ensureOpen(); if (!iterator.hasNext()) return null; var item = iterator.next(); current = item.getValue(); return new Entry(item.getKey().bytes.clone(), new Value(current.bytes.clone(), current.ordinal)); }
                public void discardCurrent() { if (current == null) throw new IllegalStateException("no current state row"); iterator.remove(); held -= current.logical; if (account != null) account.release(current.logical); current = null; }
            };
        }
        return new Cursor() {
            private long ordinal;
            @Override public Entry next() {
                ensureOpen(); if (ordinal == count) return null;
                try {
                    long pointer;
                    do { if (ordinal == count) return null; ordinals.seek(ordinal++ * 8); pointer = ordinals.readLong(); } while (pointer == 0);
                    long offset = pointer - 1;
                    data.seek(offset + 36); int size = data.readInt(); byte[] key = new byte[size]; data.readFully(key);
                    return new Entry(key, value(offset));
                } catch (IOException failure) { throw fail(failure); }
            }
        };
    }
    private void ensureOpen() { if (closed) throw new IllegalStateException("keyed state closed"); }
    private void release() { if (account != null) account.release(held); held = 0; }
    private SqlException fail(IOException failure) { SqlException error = SpillRows.io(failure); cleanup(error); return error; }
    private void cleanup(RuntimeException failure) { try { close(); } catch (RuntimeException cleanup) { failure.addSuppressed(cleanup); } }
    @Override public void close() {
        if (closed) return;
        closed = true; resident.clear(); release(); IOException failure = null;
        for (ScratchFile file : files) if (file != null) try { file.close(); } catch (IOException error) { failure = error; }
        if (failure != null) throw SpillRows.io(failure);
    }
}
