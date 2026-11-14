package org.jed;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.AbstractList;
import java.util.Arrays;
import java.util.List;
import org.jed.storage.*;

/** Storage dispositions, lazy chain access, preserved pointers and byte-exact shared compressed leaves. */
public final class StoredRowTest {
    public static void main(String[] args) throws Exception {
        Path fixtures = Path.of(args.length == 0 ? "spec/fileformat/fixtures" : args[0]);
        for (String name : List.of("overflow_table", "compressed_table", "interval_table", "interval_pk_table", "array_table", "array_pk_table", "json_table", "jsonb_table", "range_table", "range_pk_table", "composite_type_table", "composite_key_table", "array_composite_table", "composite_array_field_table", "nested_composite_table", "composite_range_field_table")) {
            try (var store = FileBlockStore.open(fixtures.resolve(name + ".jed"), false)) {
                var selected = PlaintextRecovery.recover(store);
                try (var pager = new Pager(store, selected.meta().pageSize(), 512, false)) {
                    var snapshot = CatalogCodec.read(pager, selected.meta());
                    check(snapshot.tables().size() == 1, "fixture table inventory");
                    var table = snapshot.tables().values().iterator().next();
                    TableLeafCodec codec = new TableLeafCodec(table.types(), table.primaryKey(), pager, selected.meta().pageCount());
                    for (long page = 2; page < selected.meta().pageCount(); page++) {
                        var physical = pager.readBody(page);
                        if (physical.header().type() != 2) continue;
                        var entries = codec.decode(physical.buffer().slice(16, pager.pageSize() - 16), (int) physical.header().count());
                        byte[] expected = new byte[pager.pageSize() - 16]; physical.buffer().position(16).get(expected);
                        check(Arrays.equals(Arrays.copyOf(codec.encode(entries), expected.length), expected), name + " stored leaf differs");
                        for (var entry : entries) {
                            var values = Encoding.row(entry, table.types());
                            for (int c = 0; c < values.size(); c++) values.get(c);
                            StoredRow original = (StoredRow) entry.record();
                            StoredRow fresh = (StoredRow) Encoding.entry(entry.key(), values, null, null, pager.pageSize(), null, table.primaryKey()).record();
                            for (int c = 0; c < values.size(); c++) {
                                var a = original.cells.get(c); var b = fresh.cells.get(c);
                                check(a.tag == b.tag && a.storedLength == b.storedLength && a.rawLength == b.rawLength,
                                    name + " fresh disposition differs at column " + c);
                                if (!a.external()) check(Arrays.equals(a.stored, b.stored), name + " inline bytes differ at column " + c);
                            }
                        }
                    }
                }
            }
        }
        corruptUntouchedValue();
        replanKeptValue();
        Path directory = Files.createTempDirectory("jed-lazy-row-"); Path file = directory.resolve("large.jed");
        byte[] content = new byte[100_000]; new java.util.Random(56).nextBytes(content);
        try {
            try (var db = FileDatabase.createExclusive(file, 256, 512)) {
                var types = List.of(SqlType.I64, SqlType.BYTEA, SqlType.TEXT);
                var columns = List.of(new Engine.Column("id", types.get(0), true, null), new Engine.Column("blob", types.get(1), false, null), new Engine.Column("label", types.get(2), false, null));
                byte[] key = Encoding.key(SqlValue.ofInteger(1));
                var entry = Encoding.entry(key, List.of(SqlValue.ofInteger(1), SqlValue.ofBytes(content), SqlValue.ofText("x".repeat(20_000))), null, null, 256, null);
                StoredRow planned = (StoredRow) entry.record();
                check(planned.cells.get(1).external() && planned.cells.get(2).compressed(), "large dispositions");
                var tree = PersistentTree.empty(256, new TableLeafCodec(types, new int[]{0})).put(entry);
                var state = db.commit(db.snapshot().put(new Engine.Table("large", columns, new int[]{0}, List.of(), tree, 0)));
                var table = state.tables().get("large"); var old = table.rows().getEntry(key);
                StoredRow before = (StoredRow) old.record();
                var cost = Encoding.readCost(old, types, new boolean[]{true, false, false}, 256);
                check(cost.pages() == 0 && cost.decompressions() == 0, "untouched cost");
                check(Arrays.equals(Encoding.row(old, types).get(1).asBytes(), content), "external round trip");
                db.pin(state);
                List<SqlValue> changed = new AbstractList<>() {
                    public int size() { return 3; }
                    public SqlValue get(int column) { if (column != 2) throw new AssertionError("untouched field materialized"); return SqlValue.ofText("changed"); }
                };
                var update = Encoding.entry(key, changed, old, new boolean[]{false, false, true}, 256, null);
                check(((StoredRow) update.record()).cells.get(1) == before.cells.get(1), "unchanged chain was replaced");
                var next = db.commit(state.put(table.changed(table.rows().put(update), 0)));
                check(Arrays.equals(Encoding.row(old, types).get(1).asBytes(), content), "old snapshot chain lost");
                db.unpin(state);
                var updated = next.tables().get("large").rows().getEntry(key);
                check(((StoredRow) updated.record()).cells.get(1).firstPage == before.cells.get(1).firstPage, "chain pointer changed after commit");
                check(Encoding.row(updated, types).get(2).asText().equals("changed"), "changed field not published");
            }
            try (var db = FileDatabase.openExclusive(file, 256)) {
                var table = db.snapshot().tables().get("large");
                var row = table.rows().getEntry(Encoding.key(SqlValue.ofInteger(1)));
                check(Arrays.equals(Encoding.row(row, table.types()).get(1).asBytes(), content), "large value reopen");
                db.commit(db.snapshot().drop("large"));
            }
        } finally { try (var paths = Files.walk(directory)) { for (Path path : paths.sorted(java.util.Comparator.reverseOrder()).toList()) Files.deleteIfExists(path); } }
        System.out.println("StoredRowTest: passed 16 shared images and fresh dispositions, lazy corruption, 100KB value, kept-chain rewrite and reopen");
    }
    private static void replanKeptValue() throws Exception {
        var types = List.of(SqlType.I64, SqlType.TEXT, SqlType.TEXT);
        int[] primaryKey = {0}; byte[] key = Encoding.key(SqlValue.ofInteger(1));
        String prefix = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ123";
        String value = prefix + "x".repeat(200);
        var initial = Encoding.entry(key, List.of(SqlValue.ofInteger(1), SqlValue.ofText(value), SqlValue.ofText("")), null, null, 256, null, primaryKey);
        var codec = new TableLeafCodec(types, primaryKey);
        var prior = codec.decode(java.nio.ByteBuffer.wrap(codec.encode(List.of(initial))), 1).getFirst();
        check(((StoredRow) prior.record()).cells.get(1).tag == 3, "test requires inline compression before update");
        String other = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwx";
        List<SqlValue> freshValues = List.of(SqlValue.ofInteger(1), SqlValue.ofText(value), SqlValue.ofText(other));
        List<SqlValue> changedValues = new AbstractList<>() {
            public int size() { return 3; }
            public SqlValue get(int c) { if (c != 2) throw new AssertionError("kept cell unnecessarily requested from evaluator"); return freshValues.get(c); }
        };
        try (var db = Database.memory(); var session = db.session()) {
            var freshMeter = new CostMeter(session, null); var keptMeter = new CostMeter(session, null);
            var fresh = Encoding.entry(key, freshValues, null, null, 256, freshMeter, primaryKey);
            var kept = Encoding.entry(key, changedValues, prior, new boolean[]{false, false, true}, 256, keptMeter, primaryKey);
            check(freshMeter.cost() == keptMeter.cost() && freshMeter.cost() > 0, "kept compression attempts cost differs");
            check(((StoredRow) kept.record()).cells.get(1).tag == 4, "growth did not move kept compressed value external");
            check(Arrays.equals(codec.encode(List.of(fresh)), codec.encode(List.of(kept))), "kept and fresh logical plans differ");
        }
        // The derivable primary-key slot is elided, and text's v52 inline header is one byte.
        char[] thresholdBytes = new char[89]; for (int i = 0; i < thresholdBytes.length; i++) thresholdBytes[i] = (char) (33 + i);
        String threshold = new String(thresholdBytes);
        var edge = Encoding.entry(key, List.of(SqlValue.ofInteger(1), SqlValue.ofText(threshold)), null, null, 256, null, primaryKey);
        check(((StoredRow) edge.record()).cells.get(1).tag == 0, "elided key or obsolete text length incorrectly forced compression");
    }
    private static void corruptUntouchedValue() throws Exception {
        Path directory = Files.createTempDirectory("jed-corrupt-lazy-"); Path file = directory.resolve("corrupt.jed");
        long chain;
        var types = List.of(SqlType.I64, SqlType.TEXT, SqlType.I64);
        byte[] key = Encoding.key(SqlValue.ofInteger(1));
        char[] characters = new char[100_000]; var random = new java.util.Random(57);
        for (int i = 0; i < characters.length; i++) characters[i] = (char) (33 + random.nextInt(90));
        try {
            try (var db = FileDatabase.createExclusive(file, 256, 512)) {
                var columns = List.of(new Engine.Column("id", types.get(0), true, null), new Engine.Column("content", types.get(1), false, null), new Engine.Column("revision", types.get(2), false, null));
                var entry = Encoding.entry(key, List.of(SqlValue.ofInteger(1), SqlValue.ofText(new String(characters)), SqlValue.ofInteger(0)), null, null, 256, null);
                var tree = PersistentTree.empty(256, new TableLeafCodec(types, new int[]{0})).put(entry);
                db.commit(db.snapshot().put(new Engine.Table("items", columns, new int[]{0}, List.of(), tree, 0)));
            }
            // Root-leaf values remain resident after publication. Reopen to exercise a
            // deferred, untouched chain rather than a freshly encoded resident value.
            try (var db = FileDatabase.openExclusive(file, 512)) {
                var state = db.snapshot();
                var table = state.tables().get("items"); var old = table.rows().getEntry(key);
                var cell = ((StoredRow) old.record()).cells.get(1);
                check(cell.tag == 2, "test requires an uncompressed external text value"); chain = cell.firstPage;
                // Retain the chain through another commit so the newest narrow manifest does not cover its payload.
                var changed = Encoding.entry(key, List.of(SqlValue.ofInteger(1), SqlValue.nullValue(SqlType.TEXT), SqlValue.ofInteger(1)), old, new boolean[]{false, false, true}, 256, null);
                db.commit(state.put(table.changed(table.rows().put(changed), 0)));
            }
            try (var device = FileBlockStore.open(file, true)) {
                byte[] page = device.readAt(chain * 256, 256); page[16] = (byte) 0xff;
                PageFormat.put32(page, 12, PageFormat.bodyCrc(page)); device.writeAt(chain * 256, page); device.sync();
            }
            try (var db = FileDatabase.openExclusive(file, 256)) {
                var table = db.snapshot().tables().get("items"); var entry = table.rows().getEntry(key);
                var values = Encoding.row(entry, types);
                check(values.get(0).asInteger() == 1 && values.get(2).asInteger() == 1, "untouched corrupt value was materialized");
                var changed = Encoding.entry(key, List.of(SqlValue.ofInteger(1), SqlValue.nullValue(SqlType.TEXT), SqlValue.ofInteger(2)), entry, new boolean[]{false, false, true}, 256, null);
                var committed = db.commit(db.snapshot().put(table.changed(table.rows().put(changed), 0)));
                var current = committed.tables().get("items").rows().getEntry(key);
                check(((StoredRow) current.record()).cells.get(1).firstPage == chain, "corrupt untouched chain was replaced");
                try { Encoding.row(current, types).get(1); throw new AssertionError("malformed UTF-8 was accepted"); }
                catch (SqlException failure) { check(failure.sqlState().equals("XX001"), "wrong stored UTF-8 state: " + failure.sqlState()); }
            }
        } finally { try (var paths = Files.walk(directory)) { for (Path path : paths.sorted(java.util.Comparator.reverseOrder()).toList()) Files.deleteIfExists(path); } }
    }
    private static void check(boolean condition, String message) { if (!condition) throw new AssertionError(message); }
}
