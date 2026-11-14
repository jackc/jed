package org.jed;

import java.util.List;

/** Statement-scoped logical memory; these charges are not measurements of the Java heap. */
final class MemoryAccount {
    private final long limit;
    private long used;
    private Runnable rejected = () -> {};
    void onRejected(Runnable guard) { rejected = java.util.Objects.requireNonNull(guard); }
    private ScratchHost scratchHost = ScratchHost.NATIVE;
    void scratchHost(ScratchHost host) { scratchHost = java.util.Objects.requireNonNull(host); }
    ScratchHost scratchHost() { return scratchHost; }
    private boolean encrypted;
    void encrypted(boolean value) { encrypted = value; }
    private java.util.function.Consumer<byte[]> scratchEntropy;
    void scratchEntropy(java.util.function.Consumer<byte[]> source) { scratchEntropy = java.util.Objects.requireNonNull(source); }
    java.util.function.Consumer<byte[]> scratchEntropy() { beforeSpill(); return encrypted ? scratchEntropy : null; }
    void beforeSpill() { if (encrypted && scratchEntropy == null) throw new SqlException("0A000", "encrypted temporary storage requires an independent entropy stream"); }
    MemoryAccount(long limit) {
        if (limit < 0) throw new IllegalArgumentException("negative query-memory limit");
        this.limit = limit;
    }
    /** Pending-set physical key bookkeeping is uncharged (memory.md §7.1).
     * Its bounded spool must still use this handle's host and protected scratch. */
    MemoryAccount physicalMetadata() {
        MemoryAccount result = new MemoryAccount(0);
        result.scratchHost = scratchHost; result.encrypted = encrypted; result.scratchEntropy = scratchEntropy;
        return result;
    }
    synchronized long used() { return used; }
    synchronized boolean tryReserve(long bytes) {
        if (bytes < 0) throw new IllegalArgumentException("negative memory reservation");
        if (bytes > Long.MAX_VALUE - used || limit != 0 && bytes > limit - used) { rejected.run(); return false; }
        used += bytes;
        return true;
    }
    void reserve(long bytes) {
        if (!tryReserve(bytes)) throw exceeded();
    }
    synchronized void release(long bytes) {
        if (bytes < 0 || bytes > used) throw new IllegalStateException("unbalanced query-memory release");
        used -= bytes;
    }
    static long payload(SqlValue value) {
        if (value.isNull()) return 0;
        return switch (value.type().kind()) {
            case BOOLEAN, I16, I32, I64, F32, F64, UUID, DATE, TIMESTAMP, TIMESTAMPTZ, INTERVAL -> 0;
            case JSONPATH -> SqlText.encodedLength(value.asJsonPath().toString());
            case JSON -> SqlText.encodedLength(value.asJson());
            case JSONB -> JsonCodec.memory(value.asJsonb()) - MemorySchedule.VALUE;
            case RANGE -> { SqlRange range = value.asRange(); yield (range.lower() == null ? 0 : value(range.lower())) + (range.upper() == null ? 0 : value(range.upper())); }
            case COMPOSITE, RECORD -> { long bytes = 0; for (SqlValue field : value.asComposite()) bytes = add(bytes, value(field)); yield bytes; }
            case ARRAY -> { long bytes = MemorySchedule.ARRAY_DIM * value.asArray().dimensions(); for (SqlValue item : value.asArray().elements()) bytes = add(bytes, value(item)); yield bytes; }
            case TEXT -> SqlText.encode(value.asText()).length;
            case BYTEA -> value.asBytes().length;
            case DECIMAL -> value.asDecimal().isZero() ? 0 : MemorySchedule.DECIMAL_GROUP * ((value.asDecimal().precision() + 3L) / 4);
            default -> throw new SqlException("0A000", "logical memory payload unavailable for " + value.type());
        };
    }
    static long value(SqlValue value) { return add(MemorySchedule.VALUE, payload(value)); }
    static long row(List<SqlValue> values) {
        long bytes = MemorySchedule.ROW;
        for (SqlValue value : values) bytes = add(bytes, value(value));
        return bytes;
    }
    static long add(long a, long b) {
        if (a < 0 || b < 0 || b > Long.MAX_VALUE - a) throw exceeded();
        return a + b;
    }
    static SqlException exceeded() { return new SqlException("54P05", "query memory limit exceeded"); }
}
