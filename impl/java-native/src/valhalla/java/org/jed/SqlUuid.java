package org.jed;

/** An immutable UUID bit pattern; input follows the PostgreSQL hex-pair grammar. */
public value record SqlUuid(long high, long low) implements Comparable<SqlUuid> {
    public static SqlUuid parse(String text) {
        int start = 0, end = text.length();
        if (end > 0 && text.charAt(0) == '{') {
            if (end < 2 || text.charAt(end - 1) != '}') throw invalid();
            start++; end--;
        }
        byte[] bytes = new byte[16];
        for (int i = 0; i < bytes.length; i++) {
            if (start + 2 > end) throw invalid();
            int a = hex(text.charAt(start++)), b = hex(text.charAt(start++));
            if (a < 0 || b < 0) throw invalid();
            bytes[i] = (byte) ((a << 4) | b);
            if ((i & 1) == 1 && i < 15 && start < end && text.charAt(start) == '-') start++;
        }
        if (start != end) throw invalid();
        return fromBytes(bytes);
    }
    public static SqlUuid fromBytes(byte[] bytes) {
        if (bytes.length != 16) throw invalid();
        long high = 0, low = 0;
        for (int i = 0; i < 8; i++) high = high << 8 | bytes[i] & 255L;
        for (int i = 8; i < 16; i++) low = low << 8 | bytes[i] & 255L;
        return new SqlUuid(high, low);
    }
    public byte[] toBytes() {
        byte[] result = new byte[16];
        for (int i = 0; i < 8; i++) {
            result[i] = (byte) (high >>> (56 - 8 * i));
            result[i + 8] = (byte) (low >>> (56 - 8 * i));
        }
        return result;
    }
    public boolean rfcVariant() { return (low >>> 62) == 2; }
    public int version() { return (int) (high >>> 12 & 15); }
    public Long timestampMicros() {
        if (!rfcVariant()) return null;
        if (version() == 7) return (high >>> 16) * 1000;
        if (version() != 1) return null;
        long ticks = (high & 0xfffL) << 48 | (high >>> 16 & 0xffffL) << 32 | high >>> 32;
        return (ticks - 122192928000000000L) / 10;
    }
    @Override public int compareTo(SqlUuid other) {
        int order = Long.compareUnsigned(high, other.high);
        return order != 0 ? order : Long.compareUnsigned(low, other.low);
    }
    @Override public String toString() {
        String hex = java.util.HexFormat.of().formatHex(toBytes());
        return hex.substring(0, 8) + "-" + hex.substring(8, 12) + "-" + hex.substring(12, 16) + "-" + hex.substring(16, 20) + "-" + hex.substring(20);
    }
    private static int hex(char c) {
        if (c >= '0' && c <= '9') return c - '0';
        if (c >= 'a' && c <= 'f') return c - 'a' + 10;
        if (c >= 'A' && c <= 'F') return c - 'A' + 10;
        return -1;
    }
    private static SqlException invalid() { return new SqlException("22P02", "invalid input syntax for type uuid"); }
}
