package org.jed;

import java.io.ByteArrayOutputStream;
import java.util.Arrays;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;

/** Deterministic UCA levels over verified host-loaded tables, with an identical-byte tie break. */
public final class Collation {
    public static final Collation C = new Collation("C", "", "", new TreeMap<>(), new TreeMap<>(Arrays::compare), null);
    record Element(int flags, int primary, int secondary, int tertiary) {}
    private final String name, unicodeVersion, cldrVersion;
    private final Map<Integer, Element[]> singles;
    private final TreeMap<int[], Element[]> contractions;
    private final Map<Integer, List<int[]>> starts;
    private final byte[] artifact;
    Collation(String name, String unicode, String cldr, Map<Integer, Element[]> singles, TreeMap<int[], Element[]> contractions, byte[] artifact) {
        this.name = name; unicodeVersion = unicode; cldrVersion = cldr;
        this.singles = Map.copyOf(singles); this.contractions = new TreeMap<>(Arrays::compare); this.contractions.putAll(contractions); this.artifact = artifact;
        TreeMap<Integer, List<int[]>> starts = new TreeMap<>();
        for (int[] sequence : contractions.keySet()) starts.computeIfAbsent(sequence[0], ignored -> new java.util.ArrayList<>()).add(sequence);
        for (List<int[]> sequences : starts.values()) sequences.sort((a, b) -> Integer.compare(b.length, a.length));
        this.starts = Map.copyOf(starts);
    }
    public int contentHash() { return UnicodeData.crc(tableBytes()); }
    public String name() { return name; }
    public String unicodeVersion() { return unicodeVersion; }
    public String cldrVersion() { return cldrVersion; }
    public byte[] save() { if (artifact == null) throw new SqlException("22023", "collation did not originate from an artifact"); return artifact.clone(); }
    public static Collation open(byte[] bytes) {
        UnicodeData.Reader in = new UnicodeData.Reader(bytes);
        if (!Arrays.equals(in.bytes(6), new byte[]{'J','C','O','L','L',0}) || in.u16() != 1) throw UnicodeData.invalid();
        String name = in.string(), unicode = in.string(), cldr = in.string(); in.string();
        int hash = in.i32(), raw = in.count(), length = in.count(); byte[] table = UnicodeData.unpack(in.bytes(length), raw, hash); in.end();
        return table(name, unicode, cldr, table, bytes.clone());
    }
    static Collation table(String name, String unicode, String cldr, byte[] bytes, byte[] artifact) {
        UnicodeData.Reader in = new UnicodeData.Reader(bytes); if (in.u8() != 1) throw UnicodeData.invalid();
        int singleCount = in.count(), contractionCount = in.count();
        TreeMap<Integer, Element[]> singles = readSingles(in, singleCount); TreeMap<int[], Element[]> contractions = readContractions(in, contractionCount); in.end();
        return new Collation(name, unicode, cldr, singles, contractions, artifact);
    }
    Collation tailor(String name, byte[] bytes) {
        UnicodeData.Reader in = new UnicodeData.Reader(bytes); if (in.u8() != 1) throw UnicodeData.invalid();
        int singleCount = in.count(), contractionCount = in.count();
        TreeMap<Integer, Element[]> singles = new TreeMap<>(this.singles); singles.putAll(readSingles(in, singleCount));
        TreeMap<int[], Element[]> contractions = new TreeMap<>(Arrays::compare); contractions.putAll(this.contractions); contractions.putAll(readContractions(in, contractionCount)); in.end();
        return new Collation(name, unicodeVersion, cldrVersion, singles, contractions, null);
    }
    private static TreeMap<Integer, Element[]> readSingles(UnicodeData.Reader in, int count) {
        if (count > in.remaining() / 12) throw UnicodeData.invalid();
        TreeMap<Integer, Element[]> result = new TreeMap<>(); int previous = -1;
        for (int i = 0; i < count; i++) { int point = in.point(); if (point <= previous) throw UnicodeData.invalid(); previous = point; result.put(point, elements(in)); }
        return result;
    }
    private static TreeMap<int[], Element[]> readContractions(UnicodeData.Reader in, int count) {
        if (count > in.remaining() / 17) throw UnicodeData.invalid();
        TreeMap<int[], Element[]> result = new TreeMap<>(Arrays::compare); int[] previous = null;
        for (int i = 0; i < count; i++) {
            int length = in.u8(); if (length < 2 || length > in.remaining() / 4) throw UnicodeData.invalid(); int[] points = new int[length];
            for (int j = 0; j < length; j++) points[j] = in.point();
            if (previous != null && Arrays.compare(previous, points) >= 0) throw UnicodeData.invalid(); previous = points; result.put(points, elements(in));
        }
        return result;
    }
    private static Element[] elements(UnicodeData.Reader in) {
        int count = in.u8(); if (count == 0 || count > in.remaining() / 7) throw UnicodeData.invalid(); Element[] result = new Element[count];
        for (int i = 0; i < count; i++) { int flags = in.u8(); if (flags > 1) throw UnicodeData.invalid(); result[i] = new Element(flags, in.u16(), in.u16(), in.u16()); }
        return result;
    }
    public byte[] sortKey(String text) {
        if (this == C) return Encoding.key(SqlValue.ofText(text), false, false);
        int[] points = text.codePoints().toArray(); ByteArrayOutputStream first = new ByteArrayOutputStream(), second = new ByteArrayOutputStream(), third = new ByteArrayOutputStream();
        for (int at = 0; at < points.length;) {
            Element[] elements = null; int length = 1; List<int[]> candidates = starts.get(points[at]);
            if (candidates != null) for (int[] sequence : candidates) {
                if (sequence.length > points.length - at) continue;
                int matched = 0; while (matched < sequence.length && sequence[matched] == points[at + matched]) matched++;
                if (matched == sequence.length) { elements = contractions.get(sequence); length = sequence.length; break; }
            }
            if (elements == null) elements = singles.get(points[at]);
            if (elements == null) throw new SqlException("0A000", "collation has no weight for code point " + points[at]);
            for (Element element : elements) { weight(first, element.primary); weight(second, element.secondary); weight(third, element.tertiary); }
            at += length;
        }
        first.write(0); first.write(0); first.writeBytes(second.toByteArray()); first.write(0); first.write(0); first.writeBytes(third.toByteArray()); first.write(0); first.write(0);
        first.writeBytes(Encoding.key(SqlValue.ofText(text), false, false)); return first.toByteArray();
    }
    private static void weight(ByteArrayOutputStream out, int weight) { if (weight != 0) { out.write(weight >>> 8); out.write(weight); } }
    public int compare(String left, String right) { return Arrays.compareUnsigned(sortKey(left), sortKey(right)); }
    byte[] tableBytes() {
        Encoding.Sink out = new Encoding.Sink(); out.u8(1); out.unsigned(singles.size(), 4); out.unsigned(contractions.size(), 4);
        for (Map.Entry<Integer, Element[]> entry : new TreeMap<>(singles).entrySet()) { out.unsigned(entry.getKey(), 4); writeElements(out, entry.getValue()); }
        for (Map.Entry<int[], Element[]> entry : contractions.entrySet()) { out.u8(entry.getKey().length); for (int point : entry.getKey()) out.unsigned(point, 4); writeElements(out, entry.getValue()); }
        return out.bytes();
    }
    private static void writeElements(Encoding.Sink out, Element[] elements) { out.u8(elements.length); for (Element element : elements) { out.u8(element.flags); out.u16(element.primary); out.u16(element.secondary); out.u16(element.tertiary); } }
}
