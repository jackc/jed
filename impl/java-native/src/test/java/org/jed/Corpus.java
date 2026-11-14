package org.jed;

import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.regex.Pattern;

/** Complete-file shared corpus consumer. Unsupported harness actions fail closed. */
final class Corpus {
    private Corpus() {}
    private static final Pattern DIRECTIVE = Pattern.compile("^#\\s*([a-z_-]+):\\s*(.*)$");
    private static final Set<String> KNOWN = Set.of(
        "requires", "cost", "types", "names", "max_cost", "lifetime_max_cost", "params", "fixture", "skip", "format",
        "load-collation", "load-timezone", "upgrade-collations", "timezone", "attach", "max_scalar_bytes",
        "max_query_memory_bytes", "max_sql_length", "max_storage_bytes", "max_temp_storage_bytes",
        "default_privileges", "grant", "revoke", "allow_ddl", "allow_temp_ddl", "work_mem", "set", "seed", "clock", "clock_advance");
    private static String resultType(SqlType type) {
        return switch (type.kind()) {
            case DECIMAL -> "decimal"; case TEXT -> "text"; case TIMESTAMP -> "timestamp"; case TIMESTAMPTZ -> "timestamptz"; case INTERVAL -> "interval";
            case ARRAY -> resultType(type.element()) + "[]";
            default -> type.toString();
        };
    }
    private static final Set<String> SUPPORTED = Set.of("max_sql_length", "set", "default_privileges", "grant", "revoke", "allow_ddl", "allow_temp_ddl", "cost", "types", "names", "params", "max_cost", "lifetime_max_cost", "max_scalar_bytes", "max_query_memory_bytes", "work_mem", "timezone", "clock", "clock_advance", "seed", "upgrade-collations", "max_storage_bytes", "max_temp_storage_bytes");
    static final class Inputs {
        EncryptedCorpusOracle oracle;
        final Environment defaults = Environment.empty();
        final Map<String, Long> storageLimits = new LinkedHashMap<>();
        java.util.function.LongSupplier clock = defaults::clock;
        java.util.function.Consumer<byte[]> random = defaults::random;
        void set(Record record) {
            var directives = record.directives(); clock = defaults::clock; random = defaults::random;
            if (directives.containsKey("max_storage_bytes")) for (String limit : directives.get("max_storage_bytes").split("\n")) {
                String[] fields = limit.trim().split("\\s+");
                if (fields.length < 1 || fields.length > 2) fail("invalid storage limit directive");
                storageLimits.put(fields.length == 2 ? fields[1] : "main", Long.parseLong(fields[0]));
            }
            if (directives.containsKey("clock") && directives.containsKey("clock_advance")) fail("conflicting clock directives");
            if (directives.containsKey("clock")) { long micros = Long.parseLong(directives.get("clock")); clock = () -> micros; }
            if (directives.containsKey("clock_advance")) {
                String[] fields = directives.get("clock_advance").split(","); if (fields.length != 2) fail("invalid advancing clock");
                clock = new AdvancingClock(Long.parseLong(fields[0].trim()), Long.parseLong(fields[1].trim()));
            }
            if (directives.containsKey("seed")) random = new SeededRandom(Long.parseUnsignedLong(directives.get("seed")));
        }
    }
    record Record(int line, String header, String sql, List<String> expected, Map<String, String> directives) {}

    static List<Record> parse(Path file) throws IOException {
        List<String> lines = Files.readAllLines(file, StandardCharsets.UTF_8);
        List<Record> records = new ArrayList<>();
        Map<String, String> pending = new LinkedHashMap<>();
        int i = 0;
        while (i < lines.size()) {
            String line = lines.get(i).trim();
            if (line.isEmpty()) { i++; continue; }
            if (line.startsWith("#")) {
                var match = DIRECTIVE.matcher(line);
                if (match.matches()) {
                    String key = match.group(1), value = match.group(2);
                    if (key.equals("load-timezone") || key.equals("load-collation") || key.equals("fixture") || key.equals("attach")) { i++; continue; }
                    if (key.equals("skip")) {
                        if (!value.matches("(?:memory|disk|encrypted)(?:\\s.*)?")) fail("unsupported skip mode " + value);
                        if (!records.isEmpty() || !pending.isEmpty()) fail("skip must be file-level before records");
                        i++; continue;
                    }
                    if (KNOWN.contains(key) && !key.equals("requires")) {
                        if (!SUPPORTED.contains(key)) fail(file + ":" + (i + 1) + ": unsupported harness directive " + key);
                        if (key.equals("grant") || key.equals("revoke") || key.equals("set") || key.equals("max_storage_bytes")) pending.merge(key, value, (left, right) -> left + "\n" + right);
                        else if (pending.putIfAbsent(key, value) != null) fail("duplicate directive " + key);
                    }
                }
                i++;
                continue;
            }
            int number = i + 1;
            String header = line;
            if (!header.startsWith("query ") && !header.startsWith("statement ")) fail("unsupported record " + header);
            i++;
            List<String> sql = new ArrayList<>();
            while (i < lines.size() && !lines.get(i).isEmpty() && !lines.get(i).equals("----")) sql.add(lines.get(i++));
            List<String> expected = new ArrayList<>();
            if (header.startsWith("query ")) {
                if (i >= lines.size() || !lines.get(i).equals("----")) fail("query missing ----");
                i++;
                while (i < lines.size() && !lines.get(i).isEmpty()) expected.add(lines.get(i++));
            }
            if (sql.isEmpty()) fail("empty SQL record");
            records.add(new Record(number, header, String.join("\n", sql), List.copyOf(expected), Map.copyOf(pending)));
            pending.clear();
        }
        if (!pending.isEmpty()) fail("dangling harness directives");
        if (records.isEmpty() && fixture(file) == null) fail("empty corpus file");
        return records;
    }

    private record Fixture(Path path, String expectedError) {}
    private static Fixture fixture(Path file) throws IOException {
        Fixture result = null;
        for (String line : Files.readAllLines(file, StandardCharsets.UTF_8)) {
            var match = DIRECTIVE.matcher(line.trim()); if (!match.matches() || !match.group(1).equals("fixture")) continue;
            if (result != null) fail("duplicate fixture directive");
            String[] fields = match.group(2).trim().split("\\s+");
            if (fields.length != 1 && (fields.length != 3 || !fields[1].equals("error") || !fields[2].matches("[0-9A-Z]{5}"))) fail("invalid fixture directive");
            Path root = file.toAbsolutePath().getParent(); while (root != null && !root.getFileName().toString().equals("spec")) root = root.getParent();
            if (root == null) throw new IllegalStateException("fixture corpus has no spec root");
            Path path = root.resolve(fields[0]).normalize(); if (!path.startsWith(root)) fail("fixture path escapes spec root");
            result = new Fixture(path, fields.length == 3 ? fields[2] : null);
        }
        return result;
    }
    static int run(Path file) throws Exception { return run(file, false); }
    static int run(Path file, boolean disk) throws Exception { return run(file, disk, false); }
    static int run(Path file, boolean disk, boolean encrypted) throws Exception {
        if (encrypted && !disk) throw new IllegalArgumentException("encrypted corpus requires durable storage");
        String mode = disk ? "disk" : "memory";
        for (String line : Files.readAllLines(file, StandardCharsets.UTF_8)) {
            var directive = DIRECTIVE.matcher(line.trim());
            if (directive.matches() && directive.group(1).equals("skip") && (directive.group(2).matches(mode + "(?:\\s.*)?") || encrypted && directive.group(2).matches("encrypted(?:\\s.*)?"))) {
                System.out.println("INAPPLICABLE " + file + " in " + mode + ": " + directive.group(2));
                return -1;
            }
            if (line.startsWith("statement ") || line.startsWith("query ")) break;
        }
        boolean concurrency = Files.readAllLines(file, StandardCharsets.UTF_8).stream().anyMatch(line -> line.trim().matches("#\\s*format:\\s*concurrency"));
        if (concurrency && disk) {
            System.out.println("INAPPLICABLE " + file + " in " + (encrypted ? "encrypted" : "disk") + ": canonical concurrency schedules use the memory host");
            return -1;
        }
        List<Record> records = concurrency ? List.of() : parse(file);
        List<String> zones = new ArrayList<>(), collations = new ArrayList<>(), attachments = new ArrayList<>();
        for (String line : Files.readAllLines(file, StandardCharsets.UTF_8)) {
            var directive = DIRECTIVE.matcher(line.trim());
            if (directive.matches() && directive.group(1).equals("attach")) attachments.add(directive.group(2).trim());
            if (directive.matches() && directive.group(1).equals("load-timezone")) zones.add(directive.group(2));
            if (directive.matches() && directive.group(1).equals("load-collation")) collations.add(directive.group(2).split("=", 2)[0].trim());
        }
        Environment environment = Environment.empty();
        if (!zones.isEmpty() || !collations.isEmpty()) {
            Path root = file.toAbsolutePath().getParent();
            while (root != null && !root.getFileName().toString().equals("spec")) root = root.getParent();
            if (root == null) throw new IllegalStateException("timezone corpus has no spec root");
            Environment.Builder builder = Environment.builder();
            if (!zones.isEmpty()) builder.loadTimeZoneData(Files.readAllBytes(root.resolve("tz/fixtures/tzdata.jtz")));
            if (!collations.isEmpty()) builder.loadUnicodeData(Files.readAllBytes(root.resolve("collation/fixtures/unicode.jucd")));
            environment = builder.build();
            for (String collation : collations) environment.collation(collation);
            for (String zone : zones) environment.zone(zone);
        }
        Inputs inputs = new Inputs();
        if (encrypted) inputs.oracle = EncryptedCorpusOracle.load(file);
        environment = environment.toBuilder().clockSource(() -> inputs.clock.getAsLong()).randomSource(bytes -> inputs.random.accept(bytes)).build();
        // Layer1/2 schedules describe the shared in-memory host, including its physical-storage limit.
        // Durable corpus modes exclude this format, as the frozen peer runners do.
        if (concurrency) return ConcurrencyCorpus.run(file, false, false, environment, attachments, inputs);
        Fixture fixture = fixture(file);
        if (fixture != null) {
            Path directory = Files.createTempDirectory("jed-java-fixture-"); Path databasePath = directory.resolve("database.jed");
            try {
                Files.copy(fixture.path(), databasePath);
                Database opened;
                try { opened = Database.openExclusive(databasePath, 262144, null, environment); }
                catch (SqlException error) {
                    if (fixture.expectedError() == null) throw error;
                    equal(fixture.expectedError(), error.sqlState(), "fixture open SQLSTATE");
                    if (!records.isEmpty()) fail("fixture open-error case contains SQL records");
                    if (inputs.oracle != null) inputs.oracle.finish();
                    System.out.println("PASS " + file + ": 1 fixture open-error record"); return 1;
                }
                try (Database database = opened; Session session = database.session()) {
                    if (fixture.expectedError() != null) fail("fixture open unexpectedly succeeded");
                    attach(database, attachments);
                    for (Record record : records) checkedRecord(file, database, session, record, inputs);
                }
            } finally { try (var paths = Files.walk(directory)) { for (Path path : paths.sorted(java.util.Comparator.reverseOrder()).toList()) Files.deleteIfExists(path); } }
            if (inputs.oracle != null) inputs.oracle.finish();
            System.out.println("PASS " + file + " (retained canonical fixture): " + records.size() + " records"); return records.size();
        }
        if (disk) {
            Path directory = Files.createTempDirectory("jed-java-corpus-"); Path databasePath = directory.resolve("database.jed");
            byte[] key = new byte[32]; Arrays.fill(key, (byte)0x4a);
            Credential credential = encrypted ? Credential.hostKey(key) : null;
            try {
                try (Database created = Database.createExclusive(databasePath, 8192, 262144, credential, environment)) { }
                for (Record record : records) {
                    // A cold handle per record exercises actual catalog and leaf decoding.
                    try (Database database = Database.openExclusive(databasePath, 262144, credential, environment); Session session = database.session()) { checkedRecord(file, database, session, record, inputs); }
                }
            } finally {
                try (var paths = Files.walk(directory)) { for (Path path : paths.sorted(java.util.Comparator.reverseOrder()).toList()) Files.deleteIfExists(path); }
            }
        } else try (Database database = Database.memory(environment); Session session = database.session()) {
            attach(database, attachments);
            for (Record record : records) checkedRecord(file, database, session, record, inputs);
        }
        if (inputs.oracle != null) inputs.oracle.finish();
        System.out.println("PASS " + file + " (" + (encrypted ? "encrypted, independent cost/EXPLAIN checked" : mode) + "): " + records.size() + " records");
        return records.size();
    }
    private static void attach(Database database, List<String> names) {
        for (String name : names) database.attach(name, AttachSource.memory(), false);
    }
    static void checkedRecord(Path file, Database database, Session session, Record record, Inputs inputs) {
        try { inputs.set(record); inputs.storageLimits.forEach(database::setMaxStorageBytes); runRecord(session, record, inputs.oracle); }
        catch (Exception error) { throw new IllegalStateException(file + ":" + record.line() + ": " + record.sql() + "\n" + error.getMessage(), error); }
        finally { session.clearVars(); }
    }

    private static SqlValue[] parameters(String text, Session source) {
        if (text == null || text.isBlank()) return new SqlValue[0];
        try (Database database = Database.memory(source.environment()); Session session = database.session()) {
            session.setTimeZone(source.timeZone());
            source.variableSnapshot().forEach(session::setVar);
            try (Rows rows = session.query("SELECT " + text)) {
            Row row = rows.next();
            if (row == null || rows.next() != null) throw new IllegalStateException("parameter expression did not produce one row");
            return row.values().toArray(SqlValue[]::new);
            }
        }
    }

    private static Set<Privilege> privileges(String value) {
        if (value.equalsIgnoreCase("NONE")) return Set.of();
        if (value.equalsIgnoreCase("ALL")) return Set.of(Privilege.SELECT, Privilege.INSERT, Privilege.UPDATE, Privilege.DELETE);
        Set<Privilege> result = java.util.EnumSet.noneOf(Privilege.class);
        for (String name : value.split("[,\\s]+")) result.add(Privilege.valueOf(name.toUpperCase(java.util.Locale.ROOT)));
        return result;
    }
    private static void runRecord(Session session, Record record, EncryptedCorpusOracle oracle) {
        session.clearVars();
        if (record.directives().containsKey("set")) for (String setting : record.directives().get("set").split("[,\n]")) {
            String[] fields = setting.split("=", 2);
            if (fields.length != 2) fail("invalid session variable directive");
            session.setVar(fields[0].trim(), fields[1].trim());
        }
        session.clearPrivilegeOverrides();
        session.setDefaultPrivileges(privileges(record.directives().getOrDefault("default_privileges", "ALL")));
        session.setAllowDdl(!record.directives().getOrDefault("allow_ddl", "on").equals("off"));
        session.setAllowTempDdl(record.directives().containsKey("allow_temp_ddl") ? !record.directives().get("allow_temp_ddl").equals("off") : null);
        for (String operation : List.of("grant", "revoke")) if (record.directives().containsKey(operation)) {
            for (String specification : record.directives().get(operation).split("\n")) {
            String[] grant = specification.split("(?i)\\s+ON\\s+", 2);
            if (grant.length != 2) fail("invalid privilege directive");
            Privilege[] selected = privileges(grant[0]).toArray(Privilege[]::new);
            if (operation.equals("grant")) session.grant(grant[1], selected); else session.revoke(grant[1], selected);
            }
        }
        session.setTimeZone(record.directives().getOrDefault("timezone", "UTC"));
        session.setMaxTempStorageBytes(Long.parseLong(record.directives().getOrDefault("max_temp_storage_bytes", "0")));
        session.setMaxSqlLength(Long.parseLong(record.directives().getOrDefault("max_sql_length", Long.toString(Session.DEFAULT_MAX_SQL_LENGTH))));
        session.setMaxScalarBytes(Long.parseLong(record.directives().getOrDefault("max_scalar_bytes", "0")));
        session.setMaxQueryMemoryBytes(Long.parseLong(record.directives().getOrDefault("max_query_memory_bytes", System.getenv().getOrDefault("JED_CONFORMANCE_QUERY_MEMORY", "0"))));
        session.setWorkMem(Long.parseLong(record.directives().getOrDefault("work_mem", System.getenv().getOrDefault("JED_CONFORMANCE_WORK_MEM", "268435456"))));
        session.setMaxCost(Long.parseLong(record.directives().getOrDefault("max_cost", "0")));
        if (record.directives().containsKey("upgrade-collations")) { if (!record.directives().get("upgrade-collations").isEmpty()) fail("upgrade-collations takes no value"); session.upgradeCollations(); }
        if (record.directives().containsKey("lifetime_max_cost")) session.setLifetimeMaxCost(Long.parseLong(record.directives().get("lifetime_max_cost")));
        String[] fields = record.header().split("\\s+");
        boolean query = fields[0].equals("query");
        String tags = query && fields.length >= 2 ? fields[1] : "";
        String sort = query && fields.length >= 3 ? fields[2] : "nosort";
        if (query && (fields.length < 2 || fields.length > 4 || !tags.matches("[IBTDR]+") ||
                !Set.of("nosort", "rowsort", "valuesort").contains(sort))) fail("unsupported query header " + record.header());
        String expectedError = null;
        if (!query) {
            if (fields.length == 2 && fields[1].equals("ok")) { /* success expected */ }
            else if (fields.length >= 3 && fields[1].equals("error") && fields[2].matches("[A-Z0-9]{5}")) expectedError = fields[2];
            else fail("unsupported statement header " + record.header());
            if (record.directives().containsKey("names") || record.directives().containsKey("types")) fail("names/types require query record");
        }
        SqlValue[] parameters = parameters(record.directives().get("params"), session);
        SqlException failure = null;
        List<String> cells = new ArrayList<>();
        long cost = 0;
        try (Rows rows = session.query(record.sql(), parameters)) {
            if (query) {
                // Render tags group flat cells; legacy homogeneous records use one tag for multiple columns.
                // Explicit types/names directives assert the full result metadata.
                if (record.directives().containsKey("names")) equal(split(record.directives().get("names")), rows.names(), "names");
                if (record.directives().containsKey("types")) equal(split(record.directives().get("types")), rows.types().stream().map(Corpus::resultType).toList(), "types");
            }
            // Deferred errors and final costs are observed only after externally draining.
            Row row;
            while ((row = rows.next()) != null) for (SqlValue value : row.values()) cells.add(Temporal.renderValue(value, session.zone()));
            cost = rows.cost();
        } catch (SqlException error) { failure = error; }
        if (expectedError != null) equal(expectedError, failure == null ? "success" : failure.sqlState(), "SQLSTATE");
        else if (failure != null) fail("unexpected SQLSTATE " + failure.sqlState() + ": " + failure.getMessage());
        if (record.directives().containsKey("cost") && (oracle == null || failure != null)) {
            if (failure != null) cost = failure.cost();
            equal(Long.parseLong(record.directives().get("cost")), cost, "cost");
        }
        if (query && failure == null) {
            List<String> actual = sorted(cells, tags.length(), sort);
            if (oracle != null && record.sql().stripLeading().regionMatches(true, 0, "EXPLAIN", 0, 7)) {
                oracle.check("rows", record.sql(), String.join(" ", actual));
            } else {
            List<String> expected = sorted(record.expected(), tags.length(), sort);
            equal(expected.size(), actual.size(), "value count");
            for (int i = 0; i < expected.size(); i++) {
                if (!expected.get(i).equals(actual.get(i)) && !(tags.charAt(i % tags.length()) == 'R' && realEqual(expected.get(i), actual.get(i)))) {
                    fail("cell " + i + ": expected " + expected.get(i) + ", got " + actual.get(i));
                }
            }
            }
        }
        if (oracle != null && failure == null && record.directives().containsKey("cost")) oracle.check("cost", record.sql(), Long.toString(cost));
    }

    private static List<String> sorted(List<String> values, int width, String mode) {
        if (mode.equals("nosort")) return values;
        if (mode.equals("valuesort")) return values.stream().sorted().toList();
        if (values.size() % width != 0) throw new IllegalStateException("incomplete row");
        List<List<String>> rows = new ArrayList<>();
        for (int i = 0; i < values.size(); i += width) rows.add(values.subList(i, i + width));
        rows.sort((left, right) -> {
            for (int i = 0; i < width; i++) {
                int compare = left.get(i).compareTo(right.get(i));
                if (compare != 0) return compare;
            }
            return 0;
        });
        return rows.stream().flatMap(List::stream).toList();
    }
    private static boolean realEqual(String left, String right) {
        try {
            double a = Double.parseDouble(left), b = Double.parseDouble(right);
            if (Double.isNaN(a)) return Double.isNaN(b);
            if (a == b) return true;
            if (!Double.isFinite(a) || !Double.isFinite(b)) return false;
            return Math.abs(a - b) <= 1e-9 * Math.max(1, Math.max(Math.abs(a), Math.abs(b)));
        } catch (NumberFormatException ignored) { return false; }
    }
    private static List<String> split(String text) { return Arrays.stream(text.split(",")).map(String::trim).toList(); }
    private static void equal(Object expected, Object actual, String what) {
        if (!expected.equals(actual)) fail(what + ": expected " + expected + ", got " + actual);
    }
    private static void fail(String message) { throw new IllegalStateException(message); }
}
