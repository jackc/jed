package org.jed;

import java.util.Arrays;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;
import java.util.zip.CRC32;
import org.jed.storage.Lz4;

/** Immutable JUCD loader and pinned unconditional Unicode casing. Never consults host locale tables. */
public final class UnicodeData {
    private final byte[] source;
    private final String unicodeVersion, cldrVersion;
    private final Map<String, Collation> collations;
    private final Map<Integer, int[][]> casing;
    private final boolean properties;
    private UnicodeData(byte[] source, String unicode, String cldr, Map<String, Collation> collations, Map<Integer, int[][]> casing, boolean properties) {
        this.source = source; unicodeVersion = unicode; cldrVersion = cldr; this.collations = Map.copyOf(collations); this.casing = Map.copyOf(casing); this.properties = properties;
    }
    public String unicodeVersion() { return unicodeVersion; }
    public String cldrVersion() { return cldrVersion; }
    public byte[] save() { return source.clone(); }
    public List<String> names() { return collations.keySet().stream().sorted(SqlText::compare).toList(); }
    public Collation collation(String name) { return collations.get(name); }
    boolean hasProperties() { return properties; }
    public static UnicodeData load(byte[] source) {
        byte[] bytes = source.clone(); Reader in = new Reader(bytes);
        if (bytes.length < 12 || crc(Arrays.copyOf(bytes, bytes.length - 4)) != java.nio.ByteBuffer.wrap(bytes, bytes.length - 4, 4).getInt()) throw invalid();
        if (!Arrays.equals(in.bytes(6), new byte[]{'J','U','C','D',0,0}) || in.u16() != 1) throw invalid();
        String unicode = in.string(), cldr = in.string(); in.string(); int count = in.u16();
        record Section(int kind, String name, int hash, int raw, int length, int offset) {}
        java.util.ArrayList<Section> sections = new java.util.ArrayList<>(); java.util.HashSet<String> names = new java.util.HashSet<>(); boolean property = false, rootSeen = false;
        for (int i = 0; i < count; i++) {
            int kind = in.u8(); String name = in.string();
            if (kind > 2 || kind == 0 && (property || !name.isEmpty()) || kind == 1 && rootSeen || kind != 0 && (name.isEmpty() || name.equals("C") || !names.add(name))) throw invalid();
            property |= kind == 0; rootSeen |= kind == 1;
            sections.add(new Section(kind, name, in.i32(), in.count(), in.count(), in.count()));
        }
        TreeMap<String, Collation> collations = new TreeMap<>(SqlText::compare); TreeMap<Integer, int[][]> casing = new TreeMap<>(); Collation root = null;
        TreeMap<String, byte[]> tailoring = new TreeMap<>(SqlText::compare);
        for (Section section : sections) {
            if (section.offset != in.at || section.length > in.remaining() - 4) throw invalid(); byte[] raw = unpack(in.bytes(section.length), section.raw, section.hash);
            if (section.kind == 0) readCasing(raw, casing);
            else if (section.kind == 1) { root = Collation.table(section.name, unicode, cldr, raw, null); collations.put(section.name, root); }
            else tailoring.put(section.name, raw);
        }
        in.i32(); in.end();
        if (!tailoring.isEmpty() && root == null) throw invalid();
        for (Map.Entry<String, byte[]> entry : tailoring.entrySet()) collations.put(entry.getKey(), root.tailor(entry.getKey(), entry.getValue()));
        return new UnicodeData(bytes, unicode, cldr, collations, casing, property);
    }
    private static void readCasing(byte[] bytes, Map<Integer, int[][]> result) {
        Reader in = new Reader(bytes); if (in.u8() != 1) throw invalid(); int count = in.count(), previous = -1;
        if (count > in.remaining() / 16) throw invalid();
        for (int i = 0; i < count; i++) { int point = in.point(); if (point <= previous) throw invalid(); previous = point; result.put(point, new int[][]{{in.point()}, {in.point()}, {in.point()}}); }
        count = in.count(); previous = -1; if (count > in.remaining() / 7) throw invalid();
        for (int i = 0; i < count; i++) { int point = in.point(); if (point <= previous) throw invalid(); previous = point; int[][] mapping = new int[3][];
            for (int j = 0; j < 3; j++) { int length = in.u8(); if (length > in.remaining() / 4) throw invalid(); mapping[j] = new int[length]; for (int k = 0; k < length; k++) mapping[j][k] = in.point(); }
            result.put(point, mapping);
        }
        in.end();
    }
    public String upper(String value) { return mapCase(value, true); }
    public String lower(String value) { return mapCase(value, false); }
    private String mapCase(String value, boolean upper) {
        StringBuilder result = new StringBuilder(value.length());
        value.codePoints().forEach(point -> { int[][] mapping = casing.get(point); if (mapping == null) result.appendCodePoint(point); else for (int out : mapping[upper ? 0 : 1]) result.appendCodePoint(out); });
        return result.toString();
    }
    static String asciiCase(String value, boolean upper) {
        StringBuilder out = new StringBuilder(value.length());
        for (int i = 0; i < value.length(); i++) { char c = value.charAt(i); out.append((char)(upper && c >= 'a' && c <= 'z' ? c - 32 : !upper && c >= 'A' && c <= 'Z' ? c + 32 : c)); }
        return out.toString();
    }
    static byte[] unpack(byte[] compressed, int rawLength, int hash) {
        if (rawLength > 64 * 1024 * 1024) throw invalid();
        try { byte[] result = Lz4.decompress(compressed, rawLength, 64 * 1024 * 1024); if (crc(result) != hash) throw invalid(); return result; }
        catch (java.io.IOException failure) { throw invalid(); }
    }
    static int crc(byte[] bytes) { CRC32 crc = new CRC32(); crc.update(bytes); return (int)crc.getValue(); }
    static SqlException invalid() { return new SqlException("XX001", "invalid Unicode collation data"); }
    static final class Reader {
        final byte[] bytes; int at;
        Reader(byte[] bytes) { this.bytes = bytes; }
        int remaining() { return bytes.length - at; }
        int u8() { if (at == bytes.length) throw invalid(); return bytes[at++] & 255; }
        int u16() { return u8() << 8 | u8(); }
        int i32() { return u16() << 16 | u16(); }
        int count() { int count = i32(); if (count < 0) throw invalid(); return count; }
        int point() { int point = count(); if (point > 0x10ffff || point >= 0xd800 && point <= 0xdfff) throw invalid(); return point; }
        byte[] bytes(int count) { if (count < 0 || count > remaining()) throw invalid(); byte[] result = Arrays.copyOfRange(bytes, at, at + count); at += count; return result; }
        String string() { try { return SqlText.decode(bytes(u16())); } catch (SqlException failure) { throw invalid(); } }
        void end() { if (remaining() != 0) throw invalid(); }
    }
}
