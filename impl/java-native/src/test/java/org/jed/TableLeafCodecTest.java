package org.jed;

import java.io.IOException;
import java.nio.ByteBuffer;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HexFormat;
import java.util.List;
import org.jed.storage.CorruptStorageException;
import org.jed.storage.PageFormat;
import org.jed.storage.Lz4;
import org.jed.storage.PersistentTree;

/** PAX regions against shared format-v53 images, plus bounded malformed inputs. */
public final class TableLeafCodecTest {
    private TableLeafCodecTest() {}
    record Fixture(String name, List<SqlType> types, int[] primaryKey, int rows) {}
    public static void main(String[] args) throws Exception {
        Path directory = Path.of(args.length == 0 ? "spec/fileformat/fixtures" : args[0]);
        List<Fixture> fixtures = List.of(
            new Fixture("pk_table", List.of(SqlType.I32, SqlType.I16), new int[] {0}, 20),
            new Fixture("bool_table", List.of(SqlType.I32, SqlType.BOOLEAN), new int[] {0}, 3),
            new Fixture("bool_pk_table", List.of(SqlType.BOOLEAN, SqlType.BOOLEAN), new int[] {0}, 2),
            new Fixture("text_table", List.of(SqlType.I32, SqlType.TEXT), new int[] {0}, 6),
            new Fixture("bytea_table", List.of(SqlType.I32, SqlType.BYTEA), new int[] {0}, 6),
            new Fixture("array_table", List.of(SqlType.I32, SqlType.array(SqlType.I32), SqlType.array(SqlType.TEXT)), new int[] {0}, 4),
            new Fixture("array_pk_table", List.of(SqlType.array(SqlType.I32), SqlType.I32), new int[] {0}, 5),
            new Fixture("decimal_table", List.of(SqlType.I32, SqlType.DECIMAL, SqlType.decimal(10, 2)), new int[] {0}, 5),
            new Fixture("decimal_fixed_table", List.of(SqlType.I32, SqlType.decimal(4, 2), SqlType.decimal(9, 0), SqlType.decimal(18, 6), SqlType.decimal(19, 2)), new int[] {0}, 18),
            new Fixture("text_pk_table", List.of(SqlType.TEXT, SqlType.I32), new int[] {0}, 5),
            new Fixture("bytea_pk_table", List.of(SqlType.BYTEA, SqlType.I32), new int[] {0}, 6),
            new Fixture("decimal_pk_table", List.of(SqlType.DECIMAL, SqlType.I32), new int[] {0}, 7),
            new Fixture("float64_table", List.of(SqlType.I32, SqlType.F64), new int[] {0}, 9),
            new Fixture("float32_table", List.of(SqlType.I32, SqlType.F32), new int[] {0}, 9),
            new Fixture("float64_pk_table", List.of(SqlType.F64, SqlType.I32), new int[] {0}, 6),
            new Fixture("float32_pk_table", List.of(SqlType.F32, SqlType.I32), new int[] {0}, 6),
            new Fixture("composite_pk_table", List.of(SqlType.I32, SqlType.I16, SqlType.I16), new int[] {0, 1}, 8));
        int pages = 0;
        for (Fixture fixture : fixtures) pages += golden(directory, fixture);
        malformed();
        packedArrays();
        snapshots();
        splitChoices();
        sizingParity();
        System.out.println("TableLeafCodecTest passed (" + fixtures.size() + " shared images, " + pages + " exact leaf payloads)");
    }

    /** Size-only layout must match serialization across encoding transitions and retained chains. */
    private static void sizingParity() {
        List<SqlType> types = List.of(SqlType.I32, SqlType.I64, SqlType.TEXT, SqlType.BYTEA, SqlType.array(SqlType.I64));
        TableLeafCodec codec = new TableLeafCodec(types, new int[]{0});
        long[] spans = {0, 255, 256, 65535, 65536, 0xffffffffL, 0x100000000L, Long.MAX_VALUE, Long.MIN_VALUE};
        for (long span : spans) for (int count : new int[]{1, 2, 3, 8, 17, 129, 257, 513}) {
            List<PersistentTree.Entry> rows = new ArrayList<>();
            for (int i = 0; i < count; i++) {
                SqlValue id = SqlValue.ofInteger(SqlType.I32, i);
                SqlValue number = i % 7 == 0 ? SqlValue.nullValue(SqlType.I64) : SqlValue.ofInteger(i % 2 == 0 ? 0 : span);
                SqlValue text = i % 11 == 0 ? SqlValue.nullValue(SqlType.TEXT) : SqlValue.ofText(i % 3 == 0 ? "repeat" : "unique-" + i);
                StoredRow.Cell bytes = StoredRow.Cell.plain(SqlValue.ofBytes(new byte[]{1, 2, (byte)i}));
                if (i % 5 == 0) { int tag = i % 10 == 0 ? 0 : 16; bytes = bytes.externalized(tag).allocated(7, new byte[tag]); }
                if (i % 5 == 1) {
                    byte[] content = new byte[128]; Arrays.fill(content, (byte)42);
                    bytes = StoredRow.Cell.plain(SqlValue.ofBytes(content)).compressed(Lz4.compress(content));
                }
                SqlValue array = i % 13 == 0 ? SqlValue.nullValue(types.get(4)) : array(SqlType.I64, 0, span);
                StoredRow row = new StoredRow(List.of(StoredRow.Cell.plain(id), StoredRow.Cell.plain(number), StoredRow.Cell.plain(text), bytes, StoredRow.Cell.plain(array)));
                rows.add(new PersistentTree.Entry(Encoding.key(id), row));
            }
            check(codec.payloadSize(rows) == codec.encode(rows).length, "counted layout differs at span " + span + " rows " + count);
        }
        var large = new TableLeafCodec(List.of(SqlType.I32, SqlType.TEXT), new int[]{0});
        SqlValue id = SqlValue.ofInteger(SqlType.I32, 1);
        var oversized = List.of(new PersistentTree.Entry(Encoding.key(id), new StoredRow(List.of(
            StoredRow.Cell.plain(id), StoredRow.Cell.plain(SqlValue.ofText("x".repeat(70000)))))));
        check(large.payloadSize(oversized) > 65535, "sizing must accept an overfull candidate");
        try { large.encode(oversized); throw new AssertionError("serialized oversized leaf"); }
        catch (SqlException expected) { check(expected.sqlState().equals("0A000"), "oversized serialization state"); }
    }

    /** Compare optimized split search against the exhaustive canonical chooser after each mutation. */
    private static void splitChoices() throws Exception {
        var types = List.of(SqlType.I32, SqlType.I64, SqlType.TEXT, SqlType.array(SqlType.I32));
        TableLeafCodec actual = new TableLeafCodec(types, new int[] {0});
        org.jed.storage.LeafCodec exhaustive = new org.jed.storage.LeafCodec() {
            public long payloadSize(List<PersistentTree.Entry> entries) { return actual.payloadSize(entries); }
            public byte[] encode(List<PersistentTree.Entry> entries) { return actual.encode(entries); }
            public List<PersistentTree.Entry> decode(ByteBuffer bytes, int count) throws IOException { return actual.decode(bytes, count); }
        };
        PersistentTree fast = PersistentTree.empty(256, actual), slow = PersistentTree.empty(256, exhaustive);
        var random = new java.util.Random(483726);
        for (int step = 0; step < 700; step++) {
            int id = step < 200 ? step : random.nextInt(220);
            byte[] key = Encoding.key(SqlValue.ofInteger(SqlType.I32, id));
            if (step > 200 && random.nextInt(5) == 0) { fast = fast.remove(key); slow = slow.remove(key); }
            else {
                var elements = new ArrayList<SqlValue>();
                for (int j = 0, n = random.nextInt(5); j < n; j++) elements.add(SqlValue.ofInteger(SqlType.I32, random.nextInt(300)));
                var array = elements.isEmpty() ? new SqlArray(new int[0], new int[0], elements) : new SqlArray(new int[] {elements.size()}, new int[] {1}, elements);
                var values = List.of(SqlValue.ofInteger(SqlType.I32, id), SqlValue.ofInteger(SqlType.I64, random.nextInt(4) == 0 ? random.nextLong() : 17),
                    random.nextInt(5) == 0 ? SqlValue.nullValue(SqlType.TEXT) : SqlValue.ofText("word".repeat(random.nextInt(8))), SqlValue.ofArray(types.get(3), array));
                var entry = new PersistentTree.Entry(key, Encoding.row(values)); fast = fast.put(entry); slow = slow.put(entry);
            }
            var left = fast.prepare(new org.jed.storage.PageAllocator(2, new long[0]), 1);
            var right = slow.prepare(new org.jed.storage.PageAllocator(2, new long[0]), 1);
            check(left.root() == right.root() && left.writes().size() == right.writes().size(), "optimized split changes node shape at " + step);
            for (int i = 0; i < left.writes().size(); i++) check(Arrays.equals(left.writes().get(i).bytes(), right.writes().get(i).bytes()), "optimized split changes exact bytes at " + step);
        }
    }

    private static int golden(Path directory, Fixture fixture) throws Exception {
        byte[] image = Files.readAllBytes(directory.resolve(fixture.name + ".jed"));
        TableLeafCodec codec = new TableLeafCodec(fixture.types, fixture.primaryKey);
        List<List<SqlValue>> decoded = new ArrayList<>();
        int pages = 0;
        // These selected canonical fixtures all use 256-byte pages and contain
        // one table without secondary indexes; every type-2 page is its leaf.
        for (int at = 512; at < image.length; at += 256) {
            byte[] page = Arrays.copyOfRange(image, at, at + 256);
            var header = PageFormat.parseBody(page);
            if (header.type() != 2) continue;
            List<PersistentTree.Entry> rows = codec.decode(ByteBuffer.wrap(page, 16, 240).slice(), (int) header.count());
            byte[] encoded = codec.encode(rows);
            check(encoded.length <= 240, fixture.name + " payload size");
            byte[] padded = Arrays.copyOf(encoded, 240);
            check(Arrays.equals(padded, Arrays.copyOfRange(page, 16, 256)), fixture.name + " byte-exact leaf " + at / 256);
            check(codec.payloadSize(rows) == encoded.length, "exact measured payload");
            for (var row : rows) decoded.add(Encoding.row(row.value(), fixture.types));
            pages++;
        }
        check(decoded.size() == fixture.rows, fixture.name + " complete row count");
        switch (fixture.name) {
            case "pk_table" -> {
                for (int i = 0; i < decoded.size(); i++) {
                    check(decoded.get(i).get(0).asInteger() == i + 1, "elided integer key");
                    check(i == 2 ? decoded.get(i).get(1).isNull() : decoded.get(i).get(1).asInteger() == (i + 1) * 10, "base-offset integer value");
                }
            }
            case "text_table" -> {
                String[] expected = {"alice", "", "O'Brien", "café", null, "😀"};
                for (int i = 0; i < expected.length; i++) check(expected[i] == null ? decoded.get(i).get(1).isNull() : expected[i].equals(decoded.get(i).get(1).asText()), "golden text bytes");
            }
            case "decimal_table" -> check(decoded.get(1).get(1).asDecimal().toString().equals("-12345.6789") && decoded.get(1).get(2).asDecimal().toString().equals("-12.34"), "unscaled negative decimal");
            case "decimal_fixed_table" -> {
                String[] expected = {"-99.99", "-999999999", "-999999999999.999999", "-99999999999999999.99"};
                for (int c = 0; c < expected.length; c++) check(decoded.get(1).get(c + 1).asDecimal().toString().equals(expected[c]), "fixed negative decimal slot " + c);
                check(decoded.get(5).get(2).asDecimal().toString().equals("-42"), "negative unscaled decimal integer");
            }
            case "float64_table", "float32_table" -> check(Double.doubleToRawLongBits(decoded.get(3).get(1).asFloat()) == Long.MIN_VALUE, "golden stored negative zero");
            default -> { }
        }
        return pages;
    }

    private static void malformed() throws Exception {
        TableLeafCodec onlyKey = new TableLeafCodec(List.of(SqlType.I32), new int[] {0});
        corrupt(onlyKey, "0100058000000100000c000c", 1, "schema/key-width mismatch");
        TableLeafCodec flag = new TableLeafCodec(List.of(SqlType.I32, SqlType.BOOLEAN), new int[] {0});
        List<PersistentTree.Entry> values = List.of(entry(1, SqlValue.ofBoolean(true)));
        byte[] good = flag.encode(values);
        for (int size = 0; size < good.length; size++) corrupt(flag, Arrays.copyOf(good, size), 1, "truncated leaf " + size);
        byte[] bad = good.clone(); bad[0] |= 4; corrupt(flag, bad, 1, "reserved leaf flags");
        bad = good.clone(); bad[bad.length - 1] = 2; corrupt(flag, bad, 1, "invalid boolean cell");
        bad = Arrays.copyOf(good, good.length + 1); bad[bad.length - 1] = 1; corrupt(flag, bad, 1, "nonzero padding");
        TableLeafCodec decimal = new TableLeafCodec(List.of(SqlType.I32, SqlType.decimal(4, 2)), new int[] {0});
        bad = decimal.encode(List.of(entry(1, SqlValue.ofDecimal(Decimal.parse("99.99")))));
        bad[bad.length - 2] = (byte) 0xff; bad[bad.length - 1] = (byte) 0xff;
        var raw = decimal.decode(ByteBuffer.wrap(bad), 1).getFirst();
        check(Encoding.row(raw.value(), List.of(SqlType.I32, SqlType.decimal(4, 2))).get(1).asDecimal().toString().equals("327.67"), "out-of-typmod slot preserved for integrity check");
    }

    private static void snapshots() throws Exception {
        List<SqlType> types = List.of(SqlType.I32, SqlType.TEXT);
        TableLeafCodec codec = new TableLeafCodec(types, new int[] {0});
        PersistentTree tree = PersistentTree.empty(256, codec);
        PersistentTree before = tree;
        for (int i = 0; i < 100; i++) {
            var entry = entry(i, SqlValue.ofText("value-" + i));
            tree = tree.put(entry.key(), entry.value());
            if (i == 49) before = tree;
        }
        check(before.size() == 50 && tree.size() == 100, "tree snapshot counts");
        var replaced = entry(0, SqlValue.ofText("replacement"));
        tree = tree.put(replaced.key(), replaced.value());
        check(Encoding.row(before.get(replaced.key()), types).get(1).asText().equals("value-0"), "old snapshot unchanged after replacement");
        try (var cursor = before.cursor()) {
            for (int i = 0; i < 50; i++) {
                var row = cursor.next();
                check(row != null && Encoding.row(row.value(), types).get(0).asInteger() == i, "snapshot cursor order");
            }
            check(cursor.next() == null, "snapshot cursor end");
        }
    }
    private static void packedArrays() throws Exception {
        SqlType type = SqlType.array(SqlType.I16);
        var codec = new TableLeafCodec(List.of(SqlType.I32, type), new int[]{0});
        var records = List.of(entry(1, array(SqlType.I16, 0, 1)), entry(2, array(SqlType.I16, 2)),
            entry(3, array(SqlType.I16)), entry(4, SqlValue.nullValue(type)));
        byte[] encoded = codec.encode(records), expected = HexFormat.of().parseHex("04020100ff80000218");
        check(Arrays.equals(Arrays.copyOfRange(encoded, encoded.length - expected.length, encoded.length), expected), "packed arrays MSB-first exact region");
        check(Arrays.equals(encoded, codec.encode(codec.decode(ByteBuffer.wrap(encoded), 4))), "packed arrays null/empty round trip");
        encoded[encoded.length - 1] |= 1; corrupt(codec, encoded, 4, "nonzero packed array padding");
        type = SqlType.array(SqlType.I64); codec = new TableLeafCodec(List.of(SqlType.I32, type), new int[]{0});
        encoded = codec.encode(List.of(entry(1, array(SqlType.I64, Long.MIN_VALUE, Long.MAX_VALUE)), entry(2, array(SqlType.I64, 0))));
        var decoded = codec.decode(ByteBuffer.wrap(encoded), 2);
        var first = Encoding.row(decoded.getFirst(), List.of(SqlType.I32, type)).get(1).asArray();
        check(first.element(0).asInteger() == Long.MIN_VALUE && first.element(1).asInteger() == Long.MAX_VALUE, "packed64 unsigned delta extremes");
    }
    private static SqlValue array(SqlType element, long... values) {
        List<SqlValue> elements = new ArrayList<>(); for (long value : values) elements.add(SqlValue.ofInteger(element, value));
        return SqlValue.ofArray(SqlType.array(element), SqlArray.vector(elements));
    }
    private static PersistentTree.Entry entry(int key, SqlValue value) {
        SqlValue id = SqlValue.ofInteger(SqlType.I32, key);
        return new PersistentTree.Entry(Encoding.key(id), Encoding.row(List.of(id, value)));
    }
    private static void corrupt(TableLeafCodec codec, String hex, int count, String reason) throws Exception { corrupt(codec, HexFormat.of().parseHex(hex), count, reason); }
    private static void corrupt(TableLeafCodec codec, byte[] bytes, int count, String reason) throws Exception {
        try { codec.decode(ByteBuffer.wrap(bytes), count); throw new AssertionError("accepted " + reason); }
        catch (CorruptStorageException expected) { }
    }
    private static void check(boolean value, String message) { if (!value) throw new AssertionError(message); }
}
