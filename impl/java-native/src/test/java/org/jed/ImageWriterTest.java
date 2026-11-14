package org.jed;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Arrays;
import java.util.List;
import org.jed.storage.*;

/** Whole canonical images, not merely row/cell round trips; compaction must preserve node shape. */
public final class ImageWriterTest {
    public static void main(String[] args) throws Exception {
        Path fixtures = Path.of(args.length == 0 ? "spec/fileformat/fixtures" : args[0]);
        List<String> names = List.of("pk_table", "multiway_split", "compact_leaf_table", "compressed_table", "overflow_table",
            "gin_multi_table", "gist_range_table", "gist_scalar_table", "ordered_index_table", "expr_index_table",
            "statistics_table", "statistics_terms_table", "hostfunc_index_table", "hostfunc_check_table", "view_options_table", "sequence_table");
        for (String name : names) {
            byte[] expected = Files.readAllBytes(fixtures.resolve(name + ".jed"));
            try (var original = FileBlockStore.open(fixtures.resolve(name + ".jed"), false)) {
                var selected = PlaintextRecovery.recover(original);
                try (var pager = new Pager(original, selected.meta().pageSize(), 256, false); var output = new MemoryBlockStore()) {
                    var snapshot = CatalogCodec.read(pager, selected.meta());
                    var image = ImageWriter.write(snapshot, pager.pageSize(), selected.meta().txid(), PageCodec.PLAINTEXT, null,
                        (page, bytes) -> output.writeAt(page * pager.pageSize(), bytes));
                    output.setSize(image.pageCount() * pager.pageSize());
                    byte[] actual = output.readAt(0, Math.toIntExact(output.size()));
                    if (!Arrays.equals(expected, actual)) throw new AssertionError(name + " full image mismatch at " + Arrays.mismatch(expected, actual));
                }
            }
        }
        compact(false); compact(true); memory();
        System.out.println("ImageWriterTest passed (" + names.size() + " exact shared images, plain/encrypted compaction, memory compaction)");
    }
    private static void compact(boolean encrypted) throws Exception {
        Path directory = Files.createTempDirectory("jed-compact-host-"); Path path = directory.resolve("db.jed");
        Credential credential = encrypted ? Credential.hostKey(new byte[32]) : null;
        try {
            try (Database database = Database.createExclusive(path, 1024, 1024, credential, Environment.empty()); Session session = database.session()) {
                run(session, "CREATE TABLE t(id i32 PRIMARY KEY, v text)");
                run(session, "INSERT INTO t SELECT g, repeat(md5(g::text), 60) FROM generate_series(1, 80) g");
                run(session, "DELETE FROM t WHERE id > 12");
                long previous = database.processTxid(), logicalPages = database.snapshot().tables().get("t").rows().scanPageCount();
                long cost = query(session, "SELECT count(*) FROM t", 12);
                try (Database.Pin pin = database.pinSnapshot()) { state(() -> database.compact("main"), "55006"); }
                Path identical = directory.resolve("identical.jed"), compacted = directory.resolve("compacted.jed");
                run(session, "BEGIN"); run(session, "INSERT INTO t VALUES(5000, 'pending')");
                database.backupForTesting(identical, true); database.backupForTesting(compacted, false);
                state(() -> database.backupForTesting(identical, true), "58P02"); run(session, "ROLLBACK");
                try (Database copied = Database.openExclusive(identical, 1024, credential); Session copy = copied.session()) {
                    query(copy, "SELECT count(*) FROM t", 12); check(copied.processTxid() == previous, "page-identical backup txid");
                }
                try (Database copied = Database.openExclusive(compacted, 1024, credential); Session copy = copied.session()) { query(copy, "SELECT count(*) FROM t", 12); }
                database.compact("main");
                if (!encrypted) check(Arrays.equals(Files.readAllBytes(path), Files.readAllBytes(compacted)), "compacted backup differs from compaction");
                check(database.processTxid() == previous + 1, "compaction txid");
                check(Files.size(path) == database.processPageCount() * 1024, "compaction leaves preallocation slack");
                check(database.snapshot().tables().get("t").rows().scanPageCount() == logicalPages, "compaction repacked nodes");
                check(query(session, "SELECT count(*) FROM t", 12) == cost, "compaction changed query cost");
                run(session, "INSERT INTO t VALUES(100, 'after compact')");
            }
            try (Database database = Database.openExclusive(path, 1024, credential); Session session = database.session()) { query(session, "SELECT count(*) FROM t", 13); }
        } finally { try (var paths = Files.walk(directory)) { for (Path file : paths.sorted(java.util.Comparator.reverseOrder()).toList()) Files.deleteIfExists(file); } }
    }
    private static void memory() {
        try (Database database = Database.memory(); Session session = database.session()) {
            run(session, "CREATE TABLE t(id i32 PRIMARY KEY)"); run(session, "INSERT INTO t SELECT g FROM generate_series(1, 300) g");
            run(session, "DELETE FROM t WHERE id > 20"); long before = database.storageBytes();
            database.compact("main"); check(database.storageBytes() < before, "memory compaction did not release dead pages"); query(session, "SELECT count(*) FROM t", 20);
        }
    }
    private static void run(Session session, String sql) { try (Rows rows = session.query(sql)) { while (rows.next() != null) { } } }
    private static long query(Session session, String sql, long expected) { try (Rows rows = session.query(sql)) { check(rows.next().get(0).asInteger() == expected, sql); while (rows.next() != null) { } return rows.cost(); } }
    private static void state(Runnable operation, String expected) { try { operation.run(); throw new AssertionError("expected " + expected); } catch (SqlException failure) { if (!failure.sqlState().equals(expected)) throw failure; } }
    private static void check(boolean condition, String message) { if (!condition) throw new AssertionError(message); }
}
