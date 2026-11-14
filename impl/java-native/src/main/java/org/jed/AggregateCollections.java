package org.jed;

import java.nio.ByteBuffer;
import java.nio.file.Path;
import java.util.List;
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.ByteArrayInputStream;
import java.io.DataInputStream;
import java.io.IOException;

/** Growing grouped inputs are independent replayable rows, never part of the finite group record. */
final class AggregateCollections implements AutoCloseable {
    private final SpillKeyState rows, uniqueKeys;
    private final boolean bounded;
    private record Owner(long group, int fold) {}
    private final java.util.Map<Owner, java.util.List<byte[]>> residentUnique = new java.util.HashMap<>();
    AggregateCollections(Path scratch, long workMem, MemoryAccount account) { bounded = scratch != null && workMem != 0; rows = new SpillKeyState(scratch, workMem, account); uniqueKeys = new SpillKeyState(scratch, workMem, account); }
    boolean bounded() { return bounded; }
    private static byte[] key(long group, int fold, long ordinal) { return ByteBuffer.allocate(20).putLong(group).putInt(fold).putLong(ordinal).array(); }
    void put(long group, int fold, long ordinal, List<SqlValue> values, boolean tuple) { put(group, fold, ordinal, values, tuple, -1); }
    void put(long group, int fold, long ordinal, List<SqlValue> values, boolean tuple, long residentBytes) {
        put(group, fold, ordinal, values, tuple, residentBytes, DistinctState.keyBytes(values));
    }
    void put(long group, int fold, long ordinal, List<SqlValue> values, boolean tuple, long residentBytes, long boundedPayload) {
        try {
            var bytes = new ByteArrayOutputStream(); var out = new DataOutputStream(bytes); out.writeInt(values.size()); SpillRows.write(out, values); out.flush();
            long size = residentBytes >= 0 ? residentBytes : tuple ? MemoryAccount.row(values) : DistinctState.keyBytes(values);
            if (bounded) size = MemoryAccount.add(MemorySchedule.ENTRY + MemorySchedule.ROW, boundedPayload);
            rows.put(key(group, fold, ordinal), bytes.toByteArray(), size);
        } catch (IOException failure) { throw SpillRows.io(failure); }
    }
    List<SqlValue> get(long group, int fold, long ordinal) {
        SpillKeyState.Value stored = rows.get(key(group, fold, ordinal));
        if (stored == null) throw new IllegalStateException("missing retained aggregate input");
        try { var in = new DataInputStream(new ByteArrayInputStream(stored.bytes())); return SpillRows.read(in, in.readInt()); }
        catch (IOException failure) { throw SpillRows.io(failure); }
    }
    boolean unique(long group, int fold, String key) {
        byte[] text = SqlText.encode(key); byte[] encoded = ByteBuffer.allocate(12 + text.length).putLong(group).putInt(fold).put(text).array();
        if (uniqueKeys.contains(encoded)) return false;
        uniqueKeys.put(encoded, new byte[0], DistinctState.entryBytes(bounded ? List.of(SqlValue.ofInteger(group), SqlValue.ofInteger(fold), SqlValue.ofText(key)) : List.of(SqlValue.ofText(key))));
        if (uniqueKeys.spilled()) residentUnique.clear();
        else residentUnique.computeIfAbsent(new Owner(group, fold), ignored -> new java.util.ArrayList<>()).add(encoded);
        return true;
    }
    void discard(long group, int fold, long count) { discard(group, fold, count, false); }
    void discard(long group, int fold, long count, boolean unique) {
        if (unique) {
            var keys = residentUnique.remove(new Owner(group, fold));
            if (keys != null) for (byte[] key : keys) uniqueKeys.discardResident(key);
        }
        for (long i = 0; i < count; i++) rows.discardResident(key(group, fold, i));
    }
    @Override public void close() { rows.close(); uniqueKeys.close(); residentUnique.clear(); }
}
