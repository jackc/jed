package org.jed;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;

/** Host-loaded dependency pins, missing-data degradation and transactional rebuild identity. */
public final class IndexDependenciesTest {
    public static void main(String[] args) throws Exception {
        Environment environment = Environment.builder().loadTimeZoneData(UnicodeDataTest.resource("/tzdata.jtz")).build();
        Path directory = Files.createTempDirectory("jed-java-zone-index-"); Path path = directory.resolve("zones.jed");
        try {
            try (Database database = Database.createExclusive(path, 1024, 2048, environment); Session session = database.session()) {
                execute(session, "CREATE TABLE events (id i32 PRIMARY KEY, stamp timestamptz, zone text)");
                execute(session, "INSERT INTO events VALUES (1, '2024-01-01 00:00:00+00', 'America/New_York')");
                execute(session, "CREATE INDEX fixed_zone ON events ((stamp AT TIME ZONE 'America/New_York'))");
                execute(session, "CREATE INDEX dynamic_zone ON events ((stamp AT TIME ZONE zone))");
                var indexes = database.snapshot().tables().get("events").indexes();
                var dynamic = indexes.stream().filter(index -> index.name().equals("dynamic_zone")).findFirst().orElseThrow().timezoneDependencies();
                var fixed = indexes.stream().filter(index -> index.name().equals("fixed_zone")).findFirst().orElseThrow().timezoneDependencies();
                check(dynamic.dynamic() && dynamic.pins().equals(environment.zonePins()), "dynamic named set not complete");
                check(!fixed.dynamic() && fixed.pins().equals(List.of(environment.zonePin("America/New_York"))), "static zone identity");
                var extended = new ArrayList<>(dynamic.pins()); extended.add(new IndexDependencies.ZonePin("unloaded", "2026a", 0));
                check(!IndexDependencies.matches(new IndexDependencies.TimeZones(true, extended), environment), "dynamic set skew accepted");
            }
            try (FileDatabase file = FileDatabase.openExclusive(path, 2048)) {
                Engine.Snapshot snapshot = file.snapshot(); Engine.Table table = snapshot.tables().get("events"); List<Engine.Index> indexes = new ArrayList<>();
                for (Engine.Index index : table.indexes()) {
                    List<IndexDependencies.ZonePin> pins = index.timezoneDependencies().pins().stream().map(pin -> new IndexDependencies.ZonePin(pin.name(), "obsolete", pin.crc())).toList();
                    indexes.add(new Engine.Index(index.name(), index.keys(), index.unique(), index.constraint(), index.predicate(), index.kind(), index.entries(), new IndexDependencies.TimeZones(index.timezoneDependencies().dynamic(), pins), index.gist()));
                }
                file.commit(snapshot.put(table.withIndexes(indexes)));
            }
            try (Database database = Database.openExclusive(path, 2048); Session session = database.session()) {
                check(database.snapshot().tables().get("events").rows().size() == 1, "missing timezone data blocked stored rows");
                expect("XX002", () -> execute(session, "INSERT INTO events VALUES (2, NULL, NULL)"));
                expect("22023", () -> execute(session, "REINDEX TABLE events"));
                check(stale(database), "failed rebuild advanced pins");
            }
            try (Database database = Database.openExclusive(path, 2048, environment); Session session = database.session()) {
                check(stale(database), "open silently refreshed pins");
                execute(session, "BEGIN"); execute(session, "REINDEX TABLE events"); check(stale(database), "uncommitted rebuild leaked pins"); execute(session, "ROLLBACK");
                check(stale(database), "rollback lost stale pins"); execute(session, "REINDEX TABLE events");
                check(!stale(database), "successful rebuild did not refresh pins");
                for (Engine.Index index : database.snapshot().tables().get("events").indexes()) check(IndexDependencies.matches(index.timezoneDependencies(), environment), "refreshed dependency mismatch");
            }
            try (Database database = Database.openExclusive(path, 2048, environment)) { check(!stale(database), "reopened dependency pin changed"); }
        } finally { try (var paths = Files.walk(directory)) { for (Path item : paths.sorted(java.util.Comparator.reverseOrder()).toList()) Files.deleteIfExists(item); } }
        System.out.println("IndexDependenciesTest: static/dynamic timezone pins, missing data and transactional REINDEX passed");
    }
    private static boolean stale(Database database) { return database.snapshot().tables().get("events").indexes().stream().anyMatch(index -> index.timezoneDependencies().pins().stream().anyMatch(pin -> pin.version().equals("obsolete"))); }
    private static void execute(Session session, String sql) { try (Rows rows = session.query(sql)) { while (rows.next() != null) { } } }
    private static void check(boolean condition, String message) { if (!condition) throw new AssertionError(message); }
    private static void expect(String state, Runnable operation) { try { operation.run(); throw new AssertionError("expected " + state); } catch (SqlException expected) { if (!expected.sqlState().equals(state)) throw expected; } }
}
