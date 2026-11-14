package org.jed;

import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.EOFException;
import java.io.IOException;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

/** Ordered hash buckets with fixed disk partitions and record-at-a-time skew replay. */
final class HashRows implements AutoCloseable {
    record Entry(byte[] key, List<SqlValue> row) {}
    interface Cursor extends AutoCloseable { Entry next(); @Override void close(); }
    private record Stored(long hash, Entry entry) {}
    private static final int PARTITIONS = 32;
    private final Path scratch;
    private final long workMem;
    private final MemoryAccount account;
    private final int columns;
    private final List<Stored> resident = new ArrayList<>();
    private final Map<Long, List<Entry>> buckets = new HashMap<>();
    private final ScratchFile[] paths = new ScratchFile[PARTITIONS];
    private final DataOutputStream[] writers = new DataOutputStream[PARTITIONS];
    private final List<Cursor> readers = new ArrayList<>();
    private long held;
    private boolean spilled, frozen, closed;
    HashRows(Path scratch, long workMem, MemoryAccount account, int columns) { this.scratch = scratch; this.workMem = workMem; this.account = account; this.columns = columns; }
    boolean spilled() { return spilled; }
    long residentBytes() { return held; }
    void append(byte[] key, List<SqlValue> row) { append(key, row, -1); }
    void append(byte[] key, List<SqlValue> row, long residentBytes) {
        if (frozen || closed) throw new IllegalStateException("hash rows are frozen");
        Entry entry = new Entry(key.clone(), List.copyOf(row)); long hash = hash(key);
        if (spilled) { write(hash, entry); return; }
        // Hash-row value shape is [bytea key, composite(build row)] plus ENTRY and ROW.
        long bytes = residentBytes >= 0 && (scratch == null || workMem == 0) ? residentBytes : MemoryAccount.add(MemorySchedule.ENTRY + MemorySchedule.ROW + 2 * MemorySchedule.VALUE + key.length, DistinctState.keyBytes(row));
        boolean admitted = account == null || account.tryReserve(bytes);
        if (!admitted && (scratch == null || workMem == 0)) throw new SqlException("54P05", "query memory limit exceeded");
        if (admitted) held = MemoryAccount.add(held, bytes);
        resident.add(new Stored(hash, entry)); buckets.computeIfAbsent(hash, ignored -> new ArrayList<>()).add(entry);
        if (scratch != null && workMem != 0 && (!admitted || held > workMem)) spill();
    }
    private void spill() {
        if (account != null) account.beforeSpill();
        spilled = true;
        for (Stored stored : resident) write(stored.hash, stored.entry);
        resident.clear(); buckets.clear(); release();
    }
    private void write(long hash, Entry entry) {
        int partition = (int) (hash & (PARTITIONS - 1));
        try {
            DataOutputStream out = writers[partition];
            if (out == null) { paths[partition] = new ScratchFile(scratch, account); out = writers[partition] = paths[partition].output(); }
            out.writeLong(hash); out.writeInt(entry.key.length); out.write(entry.key); SpillRows.write(out, entry.row);
        } catch (IOException error) { close(); throw SpillRows.io(error); }
    }
    void freeze() {
        if (frozen) return; frozen = true;
        try { for (int i = 0; i < writers.length; i++) if (writers[i] != null) { writers[i].close(); writers[i] = null; } }
        catch (IOException error) { close(); throw SpillRows.io(error); }
    }
    Cursor probe(byte[] key) {
        freeze(); if (closed) throw new IllegalStateException("hash rows closed");
        long hash = hash(key);
        if (!spilled) {
            List<Entry> entries = buckets.getOrDefault(hash, List.of());
            return new Cursor() { int at; public Entry next() { return at == entries.size() ? null : entries.get(at++); } public void close() { at = entries.size(); } };
        }
        ScratchFile path = paths[(int) (hash & (PARTITIONS - 1))];
        if (path == null) return new Cursor() { public Entry next() { return null; } public void close() {} };
        try {
            DataInputStream input = path.input();
            Cursor cursor = new Cursor() {
                boolean done;
                public Entry next() {
                    if (done) return null;
                    try {
                        while (true) {
                            long stored;
                            // A clean record boundary is the only legal EOF.
                            int first = input.read(); if (first < 0) { close(); return null; }
                            stored = ((long) first << 56); for (int i = 6; i >= 0; i--) stored |= (long) input.readUnsignedByte() << (i * 8);
                            int length = input.readInt(); if (length < 0) throw new IOException("invalid hash key length");
                            byte[] candidate = input.readNBytes(length); if (candidate.length != length) throw new EOFException("truncated hash key");
                            List<SqlValue> row = SpillRows.read(input, columns);
                            if (stored == hash) return new Entry(candidate, row);
                        }
                    } catch (IOException error) { close(); throw SpillRows.io(error); }
                }
                public void close() { if (done) return; done = true; readers.remove(this); try { input.close(); } catch (IOException error) { throw SpillRows.io(error); } }
            };
            readers.add(cursor); return cursor;
        } catch (IOException error) { throw SpillRows.io(error); }
    }
    private static long hash(byte[] bytes) { long hash = 0xcbf29ce484222325L; for (byte value : bytes) hash = (hash ^ (value & 255L)) * 0x100000001b3L; return hash; }
    private void release() { if (account != null) account.release(held); held = 0; }
    @Override public void close() {
        if (closed) return; closed = true;
        RuntimeException failure = null;
        for (Cursor reader : List.copyOf(readers)) try { reader.close(); } catch (RuntimeException error) { failure = error; }
        for (int i = 0; i < paths.length; i++) try {
            if (writers[i] != null) { writers[i].close(); writers[i] = null; }
            if (paths[i] != null) paths[i].close();
        } catch (IOException error) { failure = SpillRows.io(error); }
        resident.clear(); buckets.clear(); release();
        if (failure != null) throw failure;
    }
}
