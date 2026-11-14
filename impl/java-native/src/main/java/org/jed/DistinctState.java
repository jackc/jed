package org.jed;

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.nio.file.Path;
import java.util.List;

/** First-occurrence DISTINCT membership with bounded, value-canonical exact keys. */
final class DistinctState implements AutoCloseable {
    private final SpillKeyState state;
    DistinctState(Path scratch, long workMem, MemoryAccount account) { state = new SpillKeyState(scratch, workMem, account); }
    boolean add(List<SqlValue> values) { return add(values, entryBytes(values)); }
    boolean add(List<SqlValue> values, long bytes) {
        byte[] key = key(values);
        if (state.contains(key)) return false;
        state.put(key, new byte[0], bytes); return true;
    }
    void discard(List<SqlValue> values) { state.discardResident(key(values)); }
    boolean contains(List<SqlValue> values) { return state.contains(key(values)); }
    boolean spilled() { return state.spilled(); }
    long size() { return state.size(); }
    static long keyBytes(List<SqlValue> values) {
        long bytes = 0; for (SqlValue value : values) bytes = MemoryAccount.add(bytes, MemoryAccount.value(value)); return bytes;
    }
    static long entryBytes(List<SqlValue> values) { return MemoryAccount.add(MemorySchedule.ENTRY, keyBytes(values)); }
    static byte[] key(List<SqlValue> values) {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        try (DataOutputStream output = new DataOutputStream(bytes)) {
            for (SqlValue value : values) {
                output.writeByte(value.type().kind().ordinal());
                byte[] key = Encoding.key(value, true, false);
                output.writeInt(key.length); output.write(key);
            }
        } catch (IOException impossible) { throw new AssertionError(impossible); }
        return bytes.toByteArray();
    }
    @Override public void close() { state.close(); }
}
