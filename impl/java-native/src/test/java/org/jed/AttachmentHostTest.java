package org.jed;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Arrays;

/** Host registry, independently sized file domains, atomic publication and private scratch lifecycle. */
public final class AttachmentHostTest {
    public static void main(String[] args) throws Exception {
        memory(); files(); gateIndependence(); temp(false); temp(true);
        System.out.println("AttachmentHostTest passed (registry, read-only domains, pins, atomic roots, file independence, encrypted/plain temp spill)");
    }
    private static void memory() {
        try (Database database = Database.memory(); Session writer = database.session(); Session reader = database.session()) {
            database.attach("a", AttachSource.memory(), false); database.attach("b", AttachSource.memory(), false);
            state(() -> database.attach("a", AttachSource.memory(), false), "42710");
            state(() -> database.attach("main", AttachSource.memory(), false), "42710");
            run(writer, "CREATE TABLE a.t(id i32 PRIMARY KEY)"); run(writer, "CREATE TABLE b.t(id i32 PRIMARY KEY)");
            run(writer, "INSERT INTO a.t VALUES(1)"); run(writer, "INSERT INTO b.t VALUES(2)");
            run(reader, "BEGIN READ ONLY");
            state(() -> database.detach("a"), "55006");
            run(writer, "BEGIN"); run(writer, "INSERT INTO a.t VALUES(3)"); run(writer, "INSERT INTO b.t VALUES(4)"); run(writer, "COMMIT");
            value(reader, "SELECT count(*) FROM a.t", 1); value(reader, "SELECT count(*) FROM b.t", 1);
            run(reader, "ROLLBACK"); value(reader, "SELECT count(*) FROM a.t", 2); value(reader, "SELECT count(*) FROM b.t", 2);
            database.setMaxStorageBytes("b", 1);
            run(writer, "BEGIN"); run(writer, "INSERT INTO a.t VALUES(5)"); run(writer, "INSERT INTO b.t SELECT g FROM generate_series(100, 10100) g");
            state(() -> run(writer, "COMMIT"), "54P06");
            value(reader, "SELECT count(*) FROM a.t", 2); value(reader, "SELECT count(*) FROM b.t", 2);
            database.attach("ro", AttachSource.memory(), true);
            state(() -> run(writer, "CREATE TABLE ro.t(id i32)"), "25006");
            database.detach("ro"); database.detach("a");
            state(() -> database.detach("a"), "42704");
        }
    }
    private static void files() throws Exception {
        Path directory = Files.createTempDirectory("jed-attachment-host-");
        Path main = directory.resolve("z-main.jed"), other = directory.resolve("a-other.jed");
        try {
            try (Database database = Database.createExclusive(other, 1024, 1024); Session session = database.session()) { run(session, "CREATE TABLE t(id i32 PRIMARY KEY)"); }
            try (Database database = Database.createExclusive(main, 8192, 8192); Session session = database.session()) {
                run(session, "CREATE TABLE t(id i32 PRIMARY KEY)");
                database.attach("other", AttachSource.file(other, FileOptions.builder().serve(false).build()), false);
                check(database.domainDatabase(session, "other").pageSize() == 1024, "attachment page size");
                byte[] before = Files.readAllBytes(main);
                run(session, "INSERT INTO other.t VALUES(10)");
                check(Arrays.equals(before, Files.readAllBytes(main)), "attachment-only commit rewrote main");
                run(session, "BEGIN"); run(session, "INSERT INTO main.t VALUES(20)");
                try { run(session, "INSERT INTO other.t VALUES(30)"); state(() -> run(session, "COMMIT"), "0A000"); }
                catch (SqlException failure) { check(failure.sqlState().equals("0A000"), "second durable domain rejection"); run(session, "ROLLBACK"); }
                value(session, "SELECT count(*) FROM main.t", 0); value(session, "SELECT count(*) FROM other.t", 1);
                check(Arrays.equals(before, Files.readAllBytes(main)), "rejected multi-file commit wrote main");
                database.detach("other");
                database.attach("other", AttachSource.file(other, FileOptions.builder().serve(false).build()), true);
                state(() -> run(session, "INSERT INTO other.t VALUES(99)"), "25006");
                value(session, "SELECT id FROM other.t", 10);
            }
            try (Database database = Database.openExclusive(other, 1024); Session session = database.session()) { value(session, "SELECT id FROM t", 10); }
        } finally { remove(directory); }
    }
    private static void gateIndependence() throws Exception {
        try (Database database = Database.memory(); Session held = database.session(); Session temp = database.session()) {
            run(held, "CREATE TABLE durable(id i32 PRIMARY KEY)"); run(held, "BEGIN");
            temp.setLockTimeoutMillis(150);
            var completed = java.util.concurrent.CompletableFuture.runAsync(() -> {
                run(temp, "CREATE TEMP TABLE private_rows(id i32 PRIMARY KEY)");
                run(temp, "INSERT INTO private_rows VALUES(1)");
                run(temp, "BEGIN READ ONLY"); run(temp, "INSERT INTO private_rows VALUES(2)");
                value(temp, "SELECT count(*) FROM private_rows", 2); run(temp, "ROLLBACK");
                value(temp, "SELECT count(*) FROM private_rows", 1);
                run(temp, "BEGIN READ ONLY"); run(temp, "INSERT INTO private_rows VALUES(3)"); run(temp, "COMMIT");
                value(temp, "SELECT count(*) FROM private_rows", 1);
                run(temp, "CREATE TEMP TABLE serial_rows(id serial)"); value(temp, "SELECT nextval('serial_rows_id_seq')", 1);
            });
            try { completed.get(2, java.util.concurrent.TimeUnit.SECONDS); }
            finally { run(held, "ROLLBACK"); }
        }
    }
    private static void temp(boolean encrypted) throws Exception {
        Path directory = Files.createTempDirectory("jed-temp-host-"); Path path = directory.resolve("main.jed");
        Credential credential = encrypted ? Credential.hostKey(new byte[32]) : null;
        try {
            try (Database database = Database.createExclusive(path, 1024, 1024, credential, Environment.empty()); Session session = database.session()) {
                session.setTempBuffers(1); byte[] before = Files.readAllBytes(path);
                run(session, "CREATE TEMP TABLE scratch(id i32 PRIMARY KEY, value text)");
                check(session.tempDatabase().tempSpilled(), "temp commit did not spill");
                run(session, "INSERT INTO scratch SELECT g, repeat(g::text, 200) FROM generate_series(1, 30) g");
                try (Database.Pin pin = session.tempDatabase().pinSnapshot()) {
                    run(session, "DELETE FROM scratch WHERE id < 20");
                    check(pin.snapshot().tables().get("scratch").rows().size() == 30, "spilled old snapshot changed");
                    try (var cursor = pin.snapshot().tables().get("scratch").rows().cursor()) { check(cursor.next() != null, "spilled old row missing"); }
                }
                value(session, "SELECT count(*) FROM scratch", 11);
                check(session.tempDatabase().residentTempPages() <= 1, "temp page cache exceeded budget");
                check(Arrays.equals(before, Files.readAllBytes(path)), "temp write rewrote durable main");
            }
        } finally { remove(directory); }
    }
    private static void run(Session session, String sql) { try (Rows rows = session.query(sql)) { while (rows.next() != null) { } } }
    private static void value(Session session, String sql, long expected) { try (Rows rows = session.query(sql)) { check(rows.next().get(0).asInteger() == expected, sql); } }
    private static void state(Runnable operation, String state) { try { operation.run(); throw new AssertionError("expected " + state); } catch (SqlException failure) { if (!failure.sqlState().equals(state)) throw failure; } }
    private static void check(boolean value, String message) { if (!value) throw new AssertionError(message); }
    private static void remove(Path directory) throws Exception { try (var paths = Files.walk(directory)) { for (Path path : paths.sorted(java.util.Comparator.reverseOrder()).toList()) Files.deleteIfExists(path); } }
}
