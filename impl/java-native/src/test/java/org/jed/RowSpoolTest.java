package org.jed;

import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.attribute.PosixFilePermissions;
import java.util.ArrayList;
import java.util.Comparator;
import java.util.List;
import java.util.Random;

/** Spill boundaries, private scratch lifecycle, account ownership, and stable external merging. */
public final class RowSpoolTest {
    private RowSpoolTest() {}
    private static boolean encrypted;
    private static java.util.function.Consumer<byte[]> entropy;
    private static MemoryAccount account(long limit) {
        MemoryAccount account = new MemoryAccount(limit);
        account.encrypted(encrypted);
        if (encrypted) account.scratchEntropy(entropy);
        return account;
    }
    public static void main(String[] args) throws Exception {
        Path scratch = Files.createTempDirectory("jed-spool-test-");
        try {
            for (boolean variant : new boolean[] {false, true}) {
                encrypted = variant; entropy = new SeededRandom(93847);
                spool(scratch);
                arraySpool(scratch);
                descriptorSpool(scratch);
                sort(scratch);
                failures(scratch);
                keyedState(scratch);
                releasedState(scratch);
                transferredRows();
                residentSort();
                hashRows(scratch);
            }
            check(MemoryAccount.payload(SqlValue.ofDecimal(Decimal.parse("0.000"))) == 0, "zero decimal has no groups");
            check(MemoryAccount.payload(SqlValue.ofDecimal(Decimal.parse("12345.000"))) == 8, "decimal coefficient groups");
            empty(scratch);
        } finally {
            try (var paths = Files.list(scratch)) { for (Path path : paths.toList()) Files.delete(path); }
            Files.delete(scratch);
        }
        System.out.println("RowSpoolTest passed (plaintext/encrypted repeatable spill, stable binary merges, keyed state, hash buckets, cleanup)");
    }
    private static void descriptorSpool(Path scratch) throws Exception {
        List<SqlValue> expected = List.of(
            SqlValue.ofText("length is not constrained here").relabel(SqlType.varchar(1)),
            SqlValue.ofDecimal(Decimal.parse("123.4567")).relabel(SqlType.decimal(4, 1)),
            SqlValue.ofTimestamp(SqlType.TIMESTAMP, 1234567).relabel(SqlType.timestamp(false, 0)),
            SqlValue.ofInterval(new SqlInterval(2, 3, 456789)).relabel(SqlType.interval(0, 0)));
        MemoryAccount account = account(1);
        try (RowSpool rows = new RowSpool(expected.stream().map(SqlValue::type).toList(), 1, scratch, account)) {
            rows.append(expected);
            try (var cursor = rows.cursor()) {
                List<SqlValue> actual = cursor.next();
                check(actual.equals(expected), "scratch preserves binary-coercible descriptors without reapplying modifiers");
                check(actual.get(1).asDecimal().toString().equals("123.4567") && actual.get(2).asTimestamp() == 1234567, "raw precision remains intact");
            }
        }
        check(account.used() == 0, "descriptor spool releases memory"); empty(scratch);
    }
    private static void arraySpool(Path scratch) throws Exception {
        SqlType type = SqlType.array(SqlType.TEXT);
        SqlValue value = SqlValue.ofArray(type, new SqlArray(new int[]{2, 2}, new int[]{-3, 7}, List.of(
            SqlValue.ofText("λ".repeat(70_000)), SqlValue.nullValue(SqlType.TEXT), SqlValue.ofText(""), SqlValue.ofText("last"))));
        MemoryAccount account = account(1);
        try (RowSpool rows = new RowSpool(List.of(type, SqlType.UNKNOWN), 1, scratch, account)) {
            List<SqlValue> expected = List.of(value, SqlValue.nullValue(SqlType.UNKNOWN)); rows.append(expected);
            check(rows.spilled(), "array spills under bounded account");
            try (var cursor = rows.cursor()) { check(cursor.next().equals(expected), "array shape, large element and unknown NULL scratch roundtrip"); }
        }
        check(account.used() == 0, "array spill releases account"); empty(scratch);
    }
    private static void hashRows(Path scratch) throws Exception {
        for (long budget : new long[] {1, 300, 1_000_000}) {
            MemoryAccount account = account(budget == 1 ? 1 : 0);
            try (HashRows rows = new HashRows(scratch, budget, account, 2)) {
                for (int i = 0; i < 1200; i++) rows.append(new byte[] {(byte) (i % 17)}, List.of(SqlValue.ofInteger(i), SqlValue.ofText("entry" + i)));
                rows.freeze();
                check(rows.spilled() == (budget != 1_000_000), "hash rows spill at logical work budget");
                if (rows.spilled()) check(rows.residentBytes() == 0 && account.used() == 0, "hash spill returns resident charge");
                try (var paths = Files.list(scratch)) { check(paths.count() <= 32, "hash partition descriptor bound"); }
                for (int key = 0; key < 17; key++) try (var cursor = rows.probe(new byte[] {(byte) key})) {
                    for (int expected = key; expected < 1200; expected += 17) {
                        HashRows.Entry entry = cursor.next(); check(entry != null && entry.row().getFirst().asInteger() == expected, "hash duplicates retain build order");
                    }
                    check(cursor.next() == null, "hash candidate end");
                }
                try (var cursor = rows.probe(new byte[] {99})) { check(cursor.next() == null, "absent hash bucket"); }
                try (var cursor = rows.probe(new byte[] {0})) { check(cursor.next() != null, "early hash probe close"); }
            }
            check(account.used() == 0, "hash close returns all charges"); empty(scratch);
        }
        MemoryAccount account = account(1);
        try (HashRows rows = new HashRows(scratch.resolve("absent"), 1, account, 1)) {
            try { rows.append(new byte[] {1}, List.of(SqlValue.ofInteger(1))); throw new AssertionError("missing hash scratch accepted"); }
            catch (SqlException error) { check(error.sqlState().equals("58030"), "hash scratch failure is an I/O error"); }
        }
        check(account.used() == 0, "failed hash spill releases account"); empty(scratch);
    }
    private static void spool(Path scratch) throws Exception {
        List<SqlType> types = List.of(SqlType.I64, SqlType.TEXT, SqlType.BYTEA, SqlType.F64, SqlType.decimal(9, 3), SqlType.varchar(5));
        List<List<SqlValue>> rows = new ArrayList<>();
        for (int i = 0; i < 31; i++) rows.add(List.of(SqlValue.ofInteger(i), SqlValue.ofText(i == 17 ? "λ".repeat(70_000) : "row" + i),
            SqlValue.ofBytes(new byte[] {0, (byte) i, -1}), SqlValue.ofFloat(SqlType.F64, i == 0 ? -0.0 : i + 0.5),
            SqlValue.ofDecimal(SqlType.decimal(9, 3), Decimal.parse("-123.450")), SqlValue.nullValue(SqlType.varchar(5))));
        MemoryAccount account = account(700);
        try (RowSpool spool = new RowSpool(types, 1000, scratch, account)) {
            for (List<SqlValue> row : rows) spool.append(row);
            check(spool.spilled() && spool.size() == rows.size(), "account rejection triggers spill before work_mem");
            check(spool.residentBytes() == 0 && account.used() == 0, "spilled rows release account");
            try (var paths = Files.list(scratch)) {
                var files = paths.toList(); check(files.size() == 1, "one repeatable spool file");
                check(Files.getPosixFilePermissions(files.getFirst()).equals(PosixFilePermissions.fromString("rw-------")), "private scratch permissions");
            }
            try (var first = spool.cursor(); var second = spool.cursor()) {
                for (List<SqlValue> expected : rows) {
                    List<SqlValue> actual = first.next(); check(actual.equals(expected), "scratch value/type roundtrip");
                    check(Double.doubleToRawLongBits(actual.get(3).asFloat()) == Double.doubleToRawLongBits(expected.get(3).asFloat()), "scratch raw negative zero");
                    check(second.next().equals(expected), "independent repeatable cursor");
                }
                check(first.next() == null && second.next() == null, "repeatable end");
                if (encrypted) try (var files = Files.list(scratch)) {
                    byte[] stored = Files.readAllBytes(files.findFirst().orElseThrow());
                    check(stored.length % ScratchDevice.STORED_BYTES == 0, "encrypted scratch framing");
                    byte[] marker = SqlText.encode("λ".repeat(16));
                    check(java.util.Collections.indexOfSubList(box(stored), box(marker)) < 0, "no plaintext value in encrypted scratch");
                }
            }
        }
        empty(scratch);
        try (RowSpool memory = new RowSpool(List.of(SqlType.I64), 1, null, account)) {
            var mutable = new ArrayList<>(List.of(SqlValue.ofInteger(1)));
            memory.append(mutable); mutable.set(0, SqlValue.ofInteger(2));
            check(!memory.spilled() && account.used() == 64, "memory database ignores work_mem");
            try (var cursor = memory.cursor()) { check(cursor.next().getFirst().asInteger() == 1, "spool owns row container"); }
        }
        check(account.used() == 0, "memory spool releases charge");
    }
    private static void sort(Path scratch) throws Exception {
        var comparator = Comparator.<List<SqlValue>>comparingLong(row -> row.getFirst().asInteger());
        List<List<SqlValue>> rows = new ArrayList<>();
        Random random = new Random(719);
        for (int i = 0; i < 513; i++) rows.add(List.of(SqlValue.ofInteger(random.nextInt(17)), SqlValue.ofInteger(i)));
        List<List<SqlValue>> expected = new ArrayList<>(rows); expected.sort(comparator);
        for (long budget : new long[] {1, 96, 700, 1_000_000}) {
            MemoryAccount account = account(budget == 1 ? 1 : 0);
            try (ExternalSorter sort = new ExternalSorter(List.of(SqlType.I64, SqlType.I64), budget, scratch, account, comparator)) {
                for (List<SqlValue> row : rows) {
                    sort.append(row);
                    check(sort.runCount() <= 10, "binary compaction bounds run descriptors");
                }
                check(sort.size() == rows.size(), "sort row count");
                try (var cursor = sort.finish()) {
                    for (List<SqlValue> row : expected) check(row.equals(cursor.next()), "stable external merge equals in-memory sort at budget " + budget);
                    check(cursor.next() == null, "sorted end");
                }
            }
            check(account.used() == 0, "sort releases all buffer/head charges"); empty(scratch);
        }
        MemoryAccount account = account(0);
        try (ExternalSorter sort = new ExternalSorter(List.of(SqlType.I64, SqlType.I64), 200, scratch, account, comparator)) {
            for (List<SqlValue> row : rows) sort.append(row);
            try (var cursor = sort.finish()) { check(cursor.next() != null, "partially consumed sort"); }
        }
        check(account.used() == 0, "early close releases merge heads"); empty(scratch);
    }
    private static void failures(Path scratch) throws Exception {
        MemoryAccount account = account(0);
        try (RowSpool spool = new RowSpool(List.of(SqlType.I64), 1, scratch.resolve("missing"), account)) {
            expect("58030", () -> spool.append(List.of(SqlValue.ofInteger(1))));
        }
        check(account.used() == 0, "failed spill releases prior reservation");
        MemoryAccount tiny = account(1);
        try (RowSpool memory = new RowSpool(List.of(SqlType.I64), 1, null, tiny)) {
            expect("54P05", () -> memory.append(List.of(SqlValue.ofInteger(1))));
        }
        try (RowSpool unlimited = new RowSpool(List.of(SqlType.I64), 0, scratch, tiny)) {
            expect("54P05", () -> unlimited.append(List.of(SqlValue.ofInteger(1))));
        }
        check(tiny.used() == 0, "rejected memory admission stays uncharged");
        try (ExternalSorter sorter = new ExternalSorter(List.of(SqlType.I64), 1, scratch, account, (a, b) -> { throw new SqlException("22023", "test comparator failure"); })) {
            sorter.append(List.of(SqlValue.ofInteger(1)));
            expect("22023", () -> sorter.append(List.of(SqlValue.ofInteger(2))));
        }
        check(account.used() == 0, "failed binary merge releases head charges"); empty(scratch);
    }
    private static void transferredRows() {
        MemoryAccount account = account(128);
        try (RowSpool source = new RowSpool(List.of(SqlType.I64), 0, null, account);
             RowSpool target = new RowSpool(List.of(SqlType.I64), 0, null, account)) {
            source.append(List.of(SqlValue.ofInteger(1))); source.append(List.of(SqlValue.ofInteger(2)));
            try (RowSpool.Cursor cursor = source.cursor()) {
                List<SqlValue> row; while ((row = cursor.next()) != null) source.transferTo(row, target);
            }
            check(account.used() == 128, "transfer at a full account adds no duplicate row charge");
            source.close(); check(account.used() == 128, "source close retains transferred ownership");
            try (RowSpool.Cursor cursor = target.cursor()) {
                List<SqlValue> row; while ((row = cursor.next()) != null) target.releaseRow(row);
            }
            check(account.used() == 0, "yielded rows release destination ownership");
        }
        check(account.used() == 0, "transfer cleanup is balanced");
    }
    private static void residentSort() {
        MemoryAccount account = account(192);
        try (ResidentSort sort = new ResidentSort(account, 2,
                (a, b) -> Long.compare(a.getFirst().asInteger(), b.getFirst().asInteger()), MemoryAccount::row)) {
            sort.append(List.of(SqlValue.ofInteger(3))); sort.append(List.of(SqlValue.ofInteger(2))); sort.append(List.of(SqlValue.ofInteger(1)));
            check(account.used() == 128, "top-k replacement releases the evicted row");
            try (RowSpool.Cursor cursor = sort.finish()) {
                check(cursor.next().getFirst().asInteger() == 1 && cursor.next().getFirst().asInteger() == 2, "resident top-k sorts retained rows");
            }
        }
        check(account.used() == 0, "early resident sort close releases retained state");
    }
    private static void releasedState(Path scratch) throws Exception {
        MemoryAccount account = account(0);
        try (SpillKeyState state = new SpillKeyState(scratch, 640, account)) {
            for (int i = 0; i < 10; i++) state.put(new byte[]{(byte)i}, new byte[]{(byte)i}, 64);
            for (int i = 0; i < 5; i++) state.discardResident(new byte[]{(byte)i});
            check(account.used() == 320, "finished collection rows release their charge");
            for (int i = 10; i < 20; i++) state.put(new byte[]{(byte)i}, new byte[]{(byte)i}, 64);
            check(state.spilled(), "later rows can spill after earlier collection rows were released");
            for (int i = 5; i < 20; i++) check(state.get(new byte[]{(byte)i}).ordinal() == i, "live ordinal survives discarded prefix and spill");
            try (SpillKeyState.Cursor cursor = state.cursor()) {
                int count = 0; SpillKeyState.Entry entry;
                while ((entry = cursor.next()) != null) { check(entry.value().ordinal() == count + 5, "disk replay skips discarded ordinal holes"); count++; }
                check(count == 15, "only surviving records replay");
            }
        }
        check(account.used() == 0, "released keyed state closes account"); empty(scratch);
    }
    private static void keyedState(Path scratch) throws Exception {
        MemoryAccount account = account(100);
        try (SpillKeyState state = new SpillKeyState(scratch, 500, account)) {
            for (int i = 0; i < 1200; i++) {
                byte[] key = java.nio.ByteBuffer.allocate(8).putLong(i).array();
                check(state.put(key, key, 64) == i, "state first ordinal");
            }
            check(state.spilled() && account.used() == 0 && state.buckets() > 64, "linear-hash directory grows behind bounded metadata");
            for (int i = 1199; i >= 0; i--) {
                byte[] key = java.nio.ByteBuffer.allocate(8).putLong(i).array();
                byte[] value = java.nio.ByteBuffer.allocate(8).putLong(i * 3L).array();
                check(state.put(key, value, 64) == i, "state update preserves ordinal");
                check(java.util.Arrays.equals(state.get(key).bytes(), value), "state updated lookup");
            }
            try (var cursor = state.cursor()) {
                for (int i = 0; i < 1200; i++) {
                    var item = cursor.next();
                    check(item != null && item.value().ordinal() == i, "state input-order replay");
                    check(java.nio.ByteBuffer.wrap(item.key()).getLong() == i && java.nio.ByteBuffer.wrap(item.value().bytes()).getLong() == i * 3L, "state replay latest version");
                }
                check(cursor.next() == null, "state replay end");
            }
            check(state.get(new byte[0]) == null && !state.contains(new byte[0]), "missing exact key");
        }
        check(account.used() == 0, "state charges released"); empty(scratch);
        try (DistinctState distinct = new DistinctState(scratch, 1, account)) {
            check(distinct.add(List.of(SqlValue.ofDecimal(Decimal.parse("1.0")))), "first numeric key");
            check(!distinct.add(List.of(SqlValue.ofDecimal(Decimal.parse("1.00")))), "numeric scale canonical identity");
            check(distinct.add(List.of(SqlValue.ofFloat(SqlType.F64, -0.0))), "first zero key");
            check(!distinct.add(List.of(SqlValue.ofFloat(SqlType.F64, 0.0))), "signed zero canonical identity");
            check(distinct.add(List.of(SqlValue.ofFloat(SqlType.F64, Double.NaN))), "first NaN key");
            check(!distinct.add(List.of(SqlValue.ofFloat(SqlType.F64, Double.longBitsToDouble(0x7ff8000000000002L)))), "NaN canonical identity");
            check(distinct.add(List.of(SqlValue.nullValue(SqlType.TEXT))) && !distinct.add(List.of(SqlValue.nullValue(SqlType.TEXT))), "NULL canonical identity");
            check(distinct.spilled() && distinct.size() == 4, "distinct really spills");
        }
        empty(scratch);
        try (SpillKeyState memory = new SpillKeyState(null, 1, account)) {
            memory.put(new byte[] {1}, new byte[] {2}, 64);
            check(!memory.spilled() && account.used() == 64, "memory keyed state ignores work_mem");
            memory.put(new byte[] {1}, new byte[] {3}, 20);
            check(account.used() == 20, "shrinking state releases delta");
            expect("54P05", () -> memory.put(new byte[] {2}, new byte[0], 90));
        }
        check(account.used() == 0, "rejected state cleans charges");
    }
    private static List<Byte> box(byte[] bytes) { List<Byte> result = new ArrayList<>(bytes.length); for (byte value : bytes) result.add(value); return result; }
    private static void expect(String state, Runnable action) {
        try { action.run(); throw new AssertionError("expected " + state); }
        catch (SqlException failure) { check(failure.sqlState().equals(state), "expected " + state + ", got " + failure.sqlState()); }
    }
    private static void empty(Path scratch) throws Exception { try (var paths = Files.list(scratch)) { check(paths.findAny().isEmpty(), "scratch files removed"); } }
    private static void check(boolean value, String message) { if (!value) throw new AssertionError(message); }
}
