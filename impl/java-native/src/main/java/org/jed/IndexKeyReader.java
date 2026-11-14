package org.jed;

import java.util.Arrays;
import java.util.List;

/** Bounded structural key walk; never decodes values or allocates element collections. */
final class IndexKeyReader {
    private IndexKeyReader() {}
    static byte[] primaryKey(byte[] entry, List<Engine.IndexKey> keys) {
        Reader in = new Reader(entry);
        for (Engine.IndexKey key : keys) {
            in.mask = key.descending() ? 255 : 0;
            int tag = in.byteValue(); if (key.nullsFirst() != key.descending()) tag ^= 1;
            if (tag != 0 && tag != 1) throw invalid();
            if (tag == 0) in.value(key.type(), !key.collation().equals("C"), 0);
        }
        if (in.at == entry.length) throw invalid();
        return Arrays.copyOfRange(entry, in.at, entry.length);
    }
    private static final class Reader {
        final byte[] bytes; int at, mask;
        Reader(byte[] bytes) { this.bytes = bytes; }
        int byteValue() { if (at == bytes.length) throw invalid(); return (bytes[at++] & 255) ^ mask; }
        void skip(int count) { if (count < 0 || count > bytes.length - at) throw invalid(); at += count; }
        long unsigned4() { long value = 0; for (int i = 0; i < 4; i++) value = value << 8 | byteValue(); return value; }
        void terminated() { while (true) if (byteValue() == 0) { int escape = byteValue(); if (escape == 1) return; if (escape != 255) throw invalid(); } }
        void decimal() {
            int kind = byteValue(); if (kind == 4) return; if (kind != 3 && kind != 5) throw invalid();
            skip(4); int terminator = kind == 3 ? 255 : 0;
            while (true) { int digit = byteValue(); if (digit == terminator) return; if (kind == 3) digit ^= 255; if (digit < 1 || digit > 100) throw invalid(); }
        }
        void value(SqlType type, boolean collated, int depth) {
            if (depth > 256) throw invalid();
            switch (type.kind()) {
                case BOOLEAN -> { if (byteValue() > 1) throw invalid(); }
                case I16 -> skip(2);
                case I32, F32, DATE -> skip(4);
                case I64, F64, TIMESTAMP, TIMESTAMPTZ -> skip(8);
                case UUID, INTERVAL -> skip(16);
                case DECIMAL -> decimal();
                case BYTEA -> terminated();
                case TEXT -> {
                    if (collated) for (int level = 0; level < 3; level++) while ((byteValue() << 8 | byteValue()) != 0) { }
                    terminated();
                }
                case ARRAY -> {
                    while (true) { int tag = byteValue(); if (tag == 0) break; if (tag == 1) value(type.element(), collated, depth + 1); else if (tag != 2) throw invalid(); }
                    int dimensions = byteValue(); skip(dimensions * 8);
                }
                case COMPOSITE, RECORD -> {
                    for (SqlType.Field field : type.fields()) { int tag = byteValue(); if (tag == 0) value(field.type(), false, depth + 1); else if (tag != 1) throw invalid(); }
                }
                case RANGE -> {
                    int present = byteValue(); if (present == 0) return; if (present != 1) throw invalid();
                    int lower = byteValue(); if (lower == 1) { value(type.element(), false, depth + 1); if (byteValue() > 1) throw invalid(); } else if (lower != 0) throw invalid();
                    int upper = byteValue(); if (upper == 1) { value(type.element(), false, depth + 1); if (byteValue() > 1) throw invalid(); } else if (upper != 2) throw invalid();
                }
                case JSONB -> json(depth, true);
                default -> throw invalid();
            }
        }
        void json(int depth, boolean root) {
            if (depth > 256) throw invalid();
            int kind = byteValue();
            switch (kind) {
                case 0 -> { if (!root) throw invalid(); }
                case 1, 4, 5 -> { }
                case 2 -> terminated();
                case 3 -> decimal();
                case 6, 7 -> {
                    long count = unsigned4(); if (count > bytes.length - at || kind == 7 && count > (bytes.length - at) / 3) throw invalid();
                    for (long i = 0; i < count; i++) { if (kind == 7) terminated(); json(depth + 1, false); }
                }
                default -> throw invalid();
            }
        }
    }
    private static SqlException invalid() { return new SqlException("XX001", "invalid secondary index key framing"); }
}
