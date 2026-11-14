package org.jed.storage;

import java.util.zip.CRC32;

/** Format-v53 fixed-width primitives; all stored integers are big-endian. */
public final class PageFormat {
    public static final int VERSION = 53;
    public static final int HEADER_SIZE = 16;
    private PageFormat() { }

    public static boolean validPageSize(int size) {
        return size >= 256 && size <= 65536 && (size & (size - 1)) == 0;
    }

    public static int u16(byte[] bytes, int offset) {
        return (bytes[offset] & 255) << 8 | (bytes[offset + 1] & 255);
    }

    public static long u32(byte[] bytes, int offset) {
        return (long) (bytes[offset] & 255) << 24 | (long) (bytes[offset + 1] & 255) << 16
            | (long) (bytes[offset + 2] & 255) << 8 | bytes[offset + 3] & 255L;
    }

    /** The full unsigned 64-bit bit pattern; compare using Long.compareUnsigned. */
    public static long u64(byte[] bytes, int offset) {
        return u32(bytes, offset) << 32 | u32(bytes, offset + 4);
    }

    public static void put16(byte[] bytes, int offset, int value) {
        if ((value & ~0xffff) != 0) throw new IllegalArgumentException("u16 overflow");
        bytes[offset] = (byte) (value >>> 8);
        bytes[offset + 1] = (byte) value;
    }

    public static void put32(byte[] bytes, int offset, long value) {
        if (value < 0 || value > 0xffffffffL) throw new IllegalArgumentException("u32 overflow");
        for (int i = 3; i >= 0; i--) { bytes[offset + i] = (byte) value; value >>>= 8; }
    }

    public static void put64(byte[] bytes, int offset, long value) {
        for (int i = 7; i >= 0; i--) { bytes[offset + i] = (byte) value; value >>>= 8; }
    }

    public static long metaCrc(byte[] bytes) { return crcExcluding(bytes, 32); }
    public static long bodyCrc(byte[] bytes) { return crcExcluding(bytes, 12); }

    private static long crcExcluding(byte[] bytes, int offset) {
        CRC32 crc = new CRC32();
        crc.update(bytes, 0, offset);
        crc.update(bytes, offset + 4, bytes.length - offset - 4);
        return crc.getValue();
    }

    public static boolean zeros(byte[] bytes, int start, int end) {
        for (int i = start; i < end; i++) if (bytes[i] != 0) return false;
        return true;
    }

    public record BodyHeader(int type, int flags, int stamp, long count, long next) { }

    public static BodyHeader parseBody(byte[] bytes) throws CorruptStorageException {
        return parseBody(bytes, true);
    }
    static BodyHeader parseBody(byte[] bytes, boolean checksum) throws CorruptStorageException {
        if (!validPageSize(bytes.length)) throw new CorruptStorageException("invalid body page size");
        int type = bytes[0] & 255;
        int flags = bytes[1] & 255;
        if (type < 1 || type > 8 || (type == 1 ? (flags & ~1) != 0 : flags != 0)) {
            throw new CorruptStorageException("invalid body page header");
        }
        if (checksum ? u32(bytes, 12) != bodyCrc(bytes) : u32(bytes, 12) != 0) throw new CorruptStorageException("body page checksum mismatch");
        return new BodyHeader(type, flags, u16(bytes, 2), u32(bytes, 4), u32(bytes, 8));
    }

    public static int remapCapacity(int pageSize) { return ((pageSize - 64) / 3) / 10; }
    public static int remapStart(int pageSize) { return pageSize - remapCapacity(pageSize) * 10; }
    public static int inlineCapacity(int pageSize) { return (remapStart(pageSize) - 64) / 8; }
    public static int overflowCapacity(int pageSize) { return (pageSize - 32) / 8; }
}
