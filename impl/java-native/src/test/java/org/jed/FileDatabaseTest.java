package org.jed;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import org.jed.storage.*;

/** File host lifecycle, generation pins, exact live deltas, and durable primitive catalog round trips. */
public final class FileDatabaseTest {
    public static void main(String[] args) throws Exception {
        Path directory = Files.createTempDirectory("jed-java-domain-");
        Path path = directory.resolve("database.jed");
        try {
            try (var file = FileDatabase.createExclusive(path, 256, 512)) {
                expectState("55006", () -> FileDatabase.openExclusive(path, 512));
                Engine.Snapshot state = file.snapshot();
                var columns = List.of(new Engine.Column("id", SqlType.I64, true, null),
                    new Engine.Column("label", SqlType.TEXT, false, new SqlParser.StoredValue(SqlValue.ofText("default"))));
                for (String name : List.of("first", "second")) {
                    var tree = PersistentTree.empty(256, new TableLeafCodec(List.of(SqlType.I64, SqlType.TEXT), new int[]{0}));
                    for (int i = 0; i < 100; i++) tree = tree.put(key(i), row(i, name));
                    state = state.put(new Engine.Table(name, columns, new int[]{0}, List.of(), tree, 0));
                }
                state = file.commit(state);
                Engine.Snapshot retained = state; file.pin(retained);
                for (int round = 0; round < 40; round++) {
                    for (String name : List.of("first", "second")) {
                        var table = state.tables().get(name);
                        state = state.put(table.changed(table.rows().put(key(round), row(round, "round" + round)), 0));
                    }
                    state = file.commit(state);
                    for (String name : List.of("first", "second")) {
                        check(java.util.Arrays.equals(retained.tables().get(name).rows().get(key(round)), row(round, name)), "pinned generation was overwritten");
                    }
                }
                file.unpin(retained);
                for (int round = 40; round < 90; round++) {
                    var table = state.tables().get("first");
                    state = file.commit(state.put(table.changed(table.rows().put(key(round), row(round, "round" + round)), 0)));
                }
                check(file.commit(state) == state, "empty work publishes generation");
                checkLive(path, state);
                state = file.commit(state.drop("second"));
                checkLive(path, state);
                // Dropping/recreating the same catalog name must retire the old tree exactly once.
                var previous = state.tables().get("first");
                var replacement = new Engine.Table("first", previous.columns(), previous.primaryKey(), List.of(),
                    PersistentTree.empty(256, new TableLeafCodec(previous.types(), previous.primaryKey())), 0);
                state = file.commit(state.put(replacement));
                checkLive(path, state);
            }
            try (var reopened = FileDatabase.openExclusive(path, 512)) {
                check(reopened.snapshot().tables().size() == 1, "catalog table count");
                check(reopened.snapshot().tables().get("first").rows().size() == 0, "reopened replacement");
                check(reopened.snapshot().tables().get("first").columns().get(1).defaultValue() instanceof SqlParser.StoredValue, "stored default lost");
            }
            expectState("58P02", () -> FileDatabase.createExclusive(path, 256, 512));
            Path hard = directory.resolve("hard.jed"); Files.createLink(hard, path);
            expectState("55006", () -> FileDatabase.openExclusive(hard, 512)); Files.delete(hard);
            Path alias = directory.resolve("alias.jed"); Files.createSymbolicLink(alias, path);
            try (var first = FileDatabase.openExclusive(path, 512)) { expectState("55006", () -> FileDatabase.openExclusive(alias, 512)); }
            publicLifecycle(directory.resolve("public.jed"));
            compositeLifecycle(directory.resolve("composite.jed"), null);
            compositeLifecycle(directory.resolve("composite-encrypted.jed"), Credential.hostKey(new byte[32]));
            System.out.println("FileDatabaseTest: passed");
        } finally {
            try (var paths = Files.walk(directory)) { for (Path item : paths.sorted(java.util.Comparator.reverseOrder()).toList()) Files.deleteIfExists(item); }
        }
    }
    private static void compositeLifecycle(Path path, Credential credential) throws Exception {
        StringBuilder text = new StringBuilder(); var random = new java.util.Random(17);
        for (int i = 0; i < 20_000; i++) text.append((char) ('a' + random.nextInt(26)));
        String payload = text.toString();
        try (Database database = Database.createExclusive(path, 1024, 1024, credential); Session session = database.session()) {
            execute(session, "CREATE TYPE addr AS (street text NOT NULL, zip i32)");
            execute(session, "CREATE TYPE route AS (stops addr[], label text)");
            execute(session, "CREATE TABLE paths (id i32 PRIMARY KEY, data route)");
            execute(session, "INSERT INTO paths VALUES (1, ROW(ARRAY[ROW('Main', 7)::addr], '" + payload + "'))");
        }
        try (Database database = Database.openExclusive(path, 1024, credential); Session session = database.session()) {
            check(database.snapshot().types().size() == 2, "named types lost after reopen");
            execute(session, "INSERT INTO paths VALUES (2, ROW(ARRAY[ROW('Side', 8)::addr], 'short'))");
            try (Rows rows = session.query("SELECT data FROM paths WHERE id = 1")) {
                var value = rows.next().get(0).asComposite();
                check(value.get(1).asText().equals(payload), "nested composite overflow round trip");
                check(value.get(0).asArray().element(0).asComposite().get(0).asText().equals("Main"), "array-of-composite round trip");
            }
        }
        try (Database database = Database.openExclusive(path, 1024, credential)) {
            check(database.snapshot().types().size() == 2 && database.snapshot().tables().get("paths").rows().size() == 2, "type catalog lost in data commit");
        }
    }
    private static void publicLifecycle(Path path) throws Exception {
        try (Database database = Database.createExclusive(path, 256, 512); Session writer = database.session(); Session reader = database.session()) {
            execute(writer, "CREATE TABLE records (id i64 PRIMARY KEY, label text DEFAULT 'fresh')");
            execute(writer, "BEGIN");
            for (int i = 0; i < 50; i++) execute(writer, "INSERT INTO records (id) VALUES (" + i + ")");
            execute(writer, "COMMIT");
            execute(reader, "BEGIN READ ONLY");
            try (Rows retained = reader.query("SELECT id, label FROM records")) {
                check(retained.next() != null, "cursor opens pinned tree");
                for (int i = 0; i < 30; i++) execute(writer, "UPDATE records SET label = 'updated' WHERE id = " + i);
                Row row;
                while ((row = retained.next()) != null) check(row.get(1).asText().equals("fresh"), "cursor pin changed under writer");
            }
            try (Rows retained = reader.query("SELECT label FROM records WHERE id = 1")) {
                check(retained.next().get(0).asText().equals("fresh"), "transaction lost pin after cursor close");
            }
            execute(reader, "COMMIT");
            try (Rows current = reader.query("SELECT label FROM records WHERE id = 1")) {
                check(current.next().get(0).asText().equals("updated"), "new transaction missed publication");
            }
            // Writer close must release both its gate and transaction pin.
            execute(writer, "BEGIN"); execute(writer, "DELETE FROM records WHERE id = 1"); writer.close();
            try (Rows unchanged = reader.query("SELECT label FROM records WHERE id = 1")) {
                check(unchanged.next().get(0).asText().equals("updated"), "uncommitted delete survived writer close");
            }
        }
        try (Database database = Database.openExclusive(path, 256); Session session = database.session()) {
            execute(session, "INSERT INTO records (id) VALUES (100)");
            try (Rows row = session.query("SELECT label FROM records WHERE id = 100")) {
                check(row.next().get(0).asText().equals("fresh"), "default failed after reopen");
            }
        }
    }
    private static void execute(Session session, String sql) { try (Rows rows = session.query(sql)) { while (rows.next() != null) { } } }
    private static void checkLive(Path path, Engine.Snapshot state) throws Exception {
        try (var store = FileBlockStore.open(path, false)) {
            var selected = PlaintextRecovery.recover(store);
            try (var pager = new Pager(store, 256, 256, false); var cursor = CatalogChain.cursor(pager, selected.meta().root(), selected.meta().pageCount())) {
                while (cursor.next() != null) { }
                long count = cursor.visitedPages().size();
                for (var table : state.tables().values()) count += table.rows().scanPageCount();
                check(count == selected.meta().livePages(), "live page delta mismatch: " + count + " vs " + selected.meta().livePages());
            }
        }
    }
    private static byte[] key(long id) { return Encoding.key(SqlValue.ofInteger(id)); }
    private static byte[] row(long id, String text) { return Encoding.row(List.of(SqlValue.ofInteger(id), SqlValue.ofText(text))); }
    private static void check(boolean condition, String message) { if (!condition) throw new AssertionError(message); }
    @FunctionalInterface private interface Action { void run() throws Exception; }
    private static void expectState(String state, Action action) throws Exception {
        try { action.run(); throw new AssertionError("expected " + state); }
        catch (SqlException error) { if (!error.sqlState().equals(state)) throw error; }
    }
}
