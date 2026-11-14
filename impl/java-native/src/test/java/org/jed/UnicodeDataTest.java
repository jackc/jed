package org.jed;

import java.nio.charset.StandardCharsets;
import java.util.Arrays;
import java.util.HexFormat;
import java.util.Map;
import java.util.TreeMap;

/** Canonical artifact bytes, UCA vectors, sparse-table merge and host-loaded casing isolation. */
public final class UnicodeDataTest {
    public static void main(String[] args) throws Exception {
        byte[] production = resource("/unicode.jucd"); UnicodeData data = UnicodeData.load(production);
        if (!Arrays.equals(production, data.save())) throw new AssertionError("JUCD bytes changed");
        Map<String, Collation> collations = new TreeMap<>(); for (String name : data.names()) collations.put(name, data.collation(name));
        int count = 0; HexFormat hex = HexFormat.of();
        for (String line : new String(resource("/collation.tsv"), StandardCharsets.UTF_8).split("\n")) {
            if (line.isEmpty() || line.startsWith("#")) continue; String[] fields = line.split("\t", -1); byte[] input = hex.parseHex(fields[2]);
            if (fields[0].equals("artifact")) {
                Collation collation = Collation.open(input); equal(input, collation.save()); equal(hex.parseHex(fields[3]), collation.tableBytes()); collations.put(fields[1], collation);
                for (int length = 0; length < input.length; length++) rejects(Arrays.copyOf(input, length), false);
            } else if (fields[0].equals("bundle")) {
                UnicodeData dev = UnicodeData.load(input); equal(input, dev.save());
                for (String name : dev.names()) { equal(collations.get(name).tableBytes(), dev.collation(name).tableBytes()); }
                for (int length = 0; length < input.length; length++) rejects(Arrays.copyOf(input, length), true);
            } else { equal(hex.parseHex(fields[3]), collations.get(fields[1]).sortKey(SqlText.decode(input))); }
            count++;
        }
        Environment environment = Environment.builder().loadUnicodeData(production).build();
        if (!environment.caseText("Straße", true).equals("STRASSE") || !Environment.empty().caseText("Straße", true).equals("STRAßE")) throw new AssertionError("host casing isolation");
        byte[] bad = production.clone(); bad[bad.length - 1] ^= 1; rejects(bad, true);
        System.out.println("UnicodeDataTest: " + count + " shared artifact/merge/sort-key vectors and malformed input checks passed");
    }
    static byte[] resource(String name) throws Exception { try (var in = UnicodeDataTest.class.getResourceAsStream(name)) { return in.readAllBytes(); } }
    static void equal(byte[] expected, byte[] actual) { if (!Arrays.equals(expected, actual)) throw new AssertionError(HexFormat.of().formatHex(expected) + " != " + HexFormat.of().formatHex(actual)); }
    static void rejects(byte[] bytes, boolean bundle) { try { if (bundle) UnicodeData.load(bytes); else Collation.open(bytes); throw new AssertionError("truncated/corrupt data accepted"); } catch (SqlException expected) { if (!expected.sqlState().equals("XX001")) throw expected; } }
}
