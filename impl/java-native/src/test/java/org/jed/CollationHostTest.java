package org.jed;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.TreeMap;

/** Host-only pin introspection, transactional default changes and atomic migration publication. */
public final class CollationHostTest {
    public static void main(String[] args) throws Exception {
        Environment environment = Environment.builder().loadUnicodeData(UnicodeDataTest.resource("/unicode.jucd")).build();
        try (Database database = Database.memory(environment); Session session = database.session()) {
            check(database.defaultCollation().equals("C") && database.collations().isEmpty(), "fresh default");
            execute(session, "BEGIN"); session.setDefaultCollation("unicode");
            check(session.defaultCollation().equals("unicode") && database.defaultCollation().equals("C"), "default leaked before commit");
            execute(session, "CREATE TABLE inherited (s text)");
            execute(session, "ROLLBACK"); check(database.snapshot().tables().isEmpty() && database.collations().isEmpty(), "default rollback");
            session.setDefaultCollation("unicode"); long version = database.snapshot().version();
            session.setDefaultCollation("unicode"); check(database.snapshot().version() == version, "identical default advanced generation");
            execute(session, "CREATE TABLE inherited (s text)");
            check(database.snapshot().tables().get("inherited").columns().getFirst().collation().equals("unicode"), "default not frozen on column");
            session.setDefaultCollation("C"); check(database.snapshot().tables().get("inherited").columns().getFirst().collation().equals("unicode"), "default changed existing column");
            execute(session, "BEGIN READ ONLY"); expect("25006", () -> session.setDefaultCollation("unicode")); expect("25P02", session::upgradeCollations); execute(session, "ROLLBACK");
        }
        Path directory = Files.createTempDirectory("jed-java-collation-"); Path path = directory.resolve("pins.jed");
        try {
            try (Database database = Database.createExclusive(path, 1024, 2048, environment); Session session = database.session()) {
                execute(session, "CREATE TABLE keyed (s text COLLATE \"unicode\" PRIMARY KEY, n i32)");
                execute(session, "CREATE INDEX keyed_n ON keyed(n)");
                execute(session, "INSERT INTO keyed VALUES ('Z', 2), ('a', 1)");
            }
            try (FileDatabase file = FileDatabase.openExclusive(path, 2048)) {
                Engine.Snapshot snapshot = file.snapshot(); var references = new TreeMap<>(snapshot.collations());
                Engine.CollationReference old = references.get("unicode"); references.put("unicode", new Engine.CollationReference(old.name(), "9999.0.0", old.cldrVersion(), "host-test", false));
                file.commit(new Engine.Snapshot(snapshot.tables(), snapshot.types(), snapshot.views(), snapshot.sequences(), snapshot.version(), references));
            }
            expect("XX002", () -> { try (Database ignored = Database.openExclusive(path, 2048)) { throw new AssertionError("missing dependency accepted"); } });
            try (Database database = Database.openExclusive(path, 2048, environment); Session session = database.session()) {
                check(database.collations().getFirst().skewed(), "version mismatch not exposed");
                long version = database.snapshot().version(); execute(session, "BEGIN"); check(session.upgradeCollations() == 1, "upgrade count");
                check(database.collations().getFirst().skewed(), "migration leaked before commit"); execute(session, "ROLLBACK");
                check(database.snapshot().version() == version && database.collations().getFirst().skewed(), "migration rollback changed committed pins");
                check(database.upgradeCollations() == 1, "upgrade pin count"); check(!database.collations().getFirst().skewed(), "migration did not repin");
                version = database.snapshot().version(); check(database.upgradeCollations() == 0 && database.snapshot().version() == version, "idempotent upgrade published");
                check(database.collations().getFirst().description().equals("host-test"), "migration changed provenance");
            }
            try (Database database = Database.openExclusive(path, 2048, environment)) { check(!database.collations().getFirst().skewed(), "reopened pin is skewed"); }
        } finally { try (var paths = Files.walk(directory)) { for (Path item : paths.sorted(java.util.Comparator.reverseOrder()).toList()) Files.deleteIfExists(item); } }
        System.out.println("CollationHostTest: transactional defaults, missing/skewed pins and atomic/idempotent migration passed");
    }
    private static void execute(Session session, String sql) { try (Rows rows = session.query(sql)) { while (rows.next() != null) { } } }
    private static void check(boolean condition, String message) { if (!condition) throw new AssertionError(message); }
    private static void expect(String state, Runnable operation) { try { operation.run(); throw new AssertionError("expected " + state); } catch (SqlException expected) { if (!expected.sqlState().equals(state)) throw expected; } }
}
