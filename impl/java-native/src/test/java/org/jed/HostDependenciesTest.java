package org.jed;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;

/** Host registrations and reopen identity are outside the SQL corpus surface. */
public final class HostDependenciesTest {
    static HostFunction function(String name, long version, long delta) {
        return HostFunction.scalar(name, List.of(SqlType.I64), SqlType.I64, args -> SqlValue.ofInteger(args.getFirst().asInteger() + delta))
            .volatility(HostFunction.Volatility.IMMUTABLE).component("test/" + name, version);
    }
    public static void main(String[] args) throws Exception {
        alterRepins();
        Path directory = Files.createTempDirectory("jed-host-dependencies-"); Path path = directory.resolve("data.jed");
        Environment original = Environment.builder().registerFunction(function("host_key", 1, 0)).registerFunction(function("host_check", 1, 0))
            .registerFunction(function("host_default", 1, 0)).registerFunction(HostFunction.scalar("unversioned", List.of(SqlType.I64), SqlType.I64, values -> values.getFirst())).build();
        try {
            try (Database database = Database.createExclusive(path, 1024, 2048, original); Session session = database.session()) {
                execute(session, "CREATE TABLE indexed (id i64 PRIMARY KEY, v i64)");
                execute(session, "INSERT INTO indexed VALUES (1, 10), (2, 20)");
                execute(session, "CREATE INDEX host_idx ON indexed ((host_key(v)))");
                execute(session, "CREATE TABLE checked (id i64 PRIMARY KEY, v i64 DEFAULT host_default(5), CONSTRAINT host_guard CHECK (host_check(v) < 100))");
                execute(session, "INSERT INTO checked VALUES (1, 10)");
                expect("42P17", () -> execute(session, "CREATE INDEX bad_host ON indexed ((unversioned(v)))"));
                expect("42P17", () -> execute(session, "CREATE TABLE bad_check (v i64 CHECK (unversioned(v) > 0))"));
                check(database.snapshot().tables().get("indexed").indexes().getFirst().hostDependencies().size() == 1, "index dependency not recorded");
                check(database.snapshot().tables().get("checked").checks().getFirst().hostDependencies().size() == 1, "check dependency not recorded");
            }
            try (Database database = Database.openExclusive(path, 2048); Session session = database.session()) {
                execute(session, "SELECT * FROM indexed"); execute(session, "SELECT * FROM checked"); execute(session, "DELETE FROM checked WHERE id = 99");
                expect("42883", () -> execute(session, "INSERT INTO indexed VALUES (3,30)"));
                expect("42883", () -> execute(session, "INSERT INTO checked VALUES (3,30)"));
            }
            Environment changed = Environment.builder().registerFunction(function("host_key", 2, 1)).registerFunction(function("host_check", 2, 1)).build();
            try (Database database = Database.openExclusive(path, 2048, changed); Session session = database.session()) {
                try (Rows rows = session.query("SELECT id FROM indexed WHERE host_key(v) = 11")) { check(rows.next().values().getFirst().asInteger() == 1, "stale index was read"); }
                expect("XX002", () -> execute(session, "INSERT INTO indexed VALUES (3,30)"));
                expect("XX002", () -> execute(session, "INSERT INTO checked (id) VALUES (3)"));
                execute(session, "REINDEX INDEX host_idx"); execute(session, "INSERT INTO indexed VALUES (3,30)");
                execute(session, "ALTER TABLE checked DROP CONSTRAINT host_guard");
                execute(session, "INSERT INTO checked VALUES (2,20)");
                expect("42883", () -> execute(session, "INSERT INTO checked (id) VALUES (3)"));
                execute(session, "ALTER TABLE checked ADD CONSTRAINT host_guard CHECK (host_check(v) < 100)");
                execute(session, "INSERT INTO checked VALUES (3,30)");
            }
            try (Database database = Database.openExclusive(path, 2048, changed); Session session = database.session()) { execute(session, "INSERT INTO indexed VALUES (4,40)"); execute(session, "INSERT INTO checked VALUES (4,40)"); }
        } finally { try (var paths = Files.walk(directory)) { for (Path item : paths.sorted(java.util.Comparator.reverseOrder()).toList()) Files.deleteIfExists(item); } }
        System.out.println("HostDependenciesTest: persisted pins, missing/changed registry, fallback, REINDEX, checks and lazy defaults passed");
    }
    private static void alterRepins() {
        Environment environment = Environment.builder().registerFunction(function("typed_guard", 1, 0))
            .registerFunction(HostFunction.scalar("typed_guard", List.of(SqlType.I32), SqlType.I32, values -> values.getFirst())
                .volatility(HostFunction.Volatility.IMMUTABLE).component("test/typed_guard/i32", 3)).build();
        try (Database database = Database.memory(environment); Session session = database.session()) {
            execute(session, "CREATE TABLE typed (id i64 PRIMARY KEY, v i64 CHECK (typed_guard(v) > 0))");
            execute(session, "INSERT INTO typed VALUES (1, 10)");
            check(database.snapshot().tables().get("typed").checks().getFirst().hostDependencies().getFirst().arguments().equals(List.of(SqlType.I64)), "initial CHECK overload pin");
            execute(session, "ALTER TABLE typed ALTER COLUMN v TYPE i32");
            HostDependencies.Dependency dependency = database.snapshot().tables().get("typed").checks().getFirst().hostDependencies().getFirst();
            check(dependency.arguments().equals(List.of(SqlType.I32)) && dependency.semanticVersion() == 3, "ALTER TYPE did not repin CHECK overload");
            execute(session, "INSERT INTO typed VALUES (2, 20)");
            expect("23514", () -> execute(session, "INSERT INTO typed VALUES (3, -1)"));
            execute(session, "ALTER TABLE typed ADD CONSTRAINT host_exclude EXCLUDE USING btree (v WITH =) WHERE (typed_guard(v) > 0)");
            check(database.snapshot().tables().get("typed").indexes().getFirst().hostDependencies().size() == 1, "exclusion predicate dependency not pinned");
        }
    }
    private static void execute(Session session, String sql) { SessionVariablesTest.execute(session, sql); }
    private static void expect(String state, Runnable action) { SessionVariablesTest.expect(state, action); }
    private static void check(boolean value, String message) { SessionVariablesTest.check(value, message); }
}
