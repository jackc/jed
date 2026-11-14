package org.jed;

import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicLong;

/** Shared unordered stress profiles: real simultaneous writers and independently pinned readers. */
final class StressRunner {
    private static int integer(JsonNode node) { return Integer.parseInt(node.number().toString()); }
    private static JsonNode number(long value) { return JsonNode.number(Decimal.parse(Long.toString(value))); }
    private static JsonNode.Member field(String key, String value) { return new JsonNode.Member(key, JsonNode.string(value)); }
    private static JsonNode.Member field(String key, long value) { return new JsonNode.Member(key, number(value)); }
    private static void execute(Session session, String sql, Cancellation cancellation) {
        try (Rows rows = session.query(sql, cancellation)) { while (rows.next() != null) { } }
    }
    static void run(String input, String storage) throws Exception {
        if (!List.of("memory", "disk", "encrypted").contains(storage)) throw new IllegalArgumentException("unknown stress storage");
        JsonNode data = JsonNode.parse(Files.readString(Path.of(input)), true);
        if (integer(data.get("schema")) != 1) throw new IllegalArgumentException("unknown stress schema");
        for (JsonNode profile : data.get("profiles").elements()) runProfile(profile, storage);
    }
    private static void runProfile(JsonNode profile, String storage) throws Exception {
        Path directory = Files.createTempDirectory("jed-java-stress-");
        Cancellation cancellation = new Cancellation();
        var executor = Executors.newCachedThreadPool(Thread.ofPlatform().daemon().name("jed-stress-", 0).factory());
        try (Database database = storage.equals("memory") ? Database.memory() : Database.create(directory.resolve("stress.jed"),
                FileOptions.builder().credential(storage.equals("encrypted") ? Credential.hostKey(new byte[32]) : null).build())) {
            try (Session setup = database.session()) {
                for (JsonNode sql : profile.get("setup").get("sql").elements()) execute(setup, sql.string(), cancellation);
            }
            List<Future<?>> jobs = new ArrayList<>(); CountDownLatch start = new CountDownLatch(1);
            AtomicLong checks = new AtomicLong(), writes = new AtomicLong(); int writers = 0;
            long started = System.nanoTime(), deadline = started + TimeUnit.SECONDS.toNanos(60);
            for (JsonNode worker : profile.get("worker").elements()) {
                String kind = worker.get("kind").string(); int count = integer(worker.get("count")), iterations = integer(worker.get("iterations"));
                if (count <= 0 || iterations <= 0 || !List.of("reader", "writer").contains(kind)) throw new IllegalArgumentException("invalid stress worker");
                if (kind.equals("writer")) writers = Math.addExact(writers, count);
                for (int index = 0; index < count; index++) jobs.add(executor.submit(() -> {
                    try {
                        start.await();
                        for (int iteration = 0; iteration < iterations; iteration++) {
                            if (kind.equals("writer")) {
                                try (Session session = database.session()) {
                                    session.setLockTimeoutMillis(60_000);
                                    for (String sql : worker.get("op").string().split(";")) if (!sql.isBlank()) execute(session, sql.trim(), cancellation);
                                    writes.incrementAndGet();
                                }
                            } else {
                                try (Session session = database.read(); Rows rows = session.query(worker.get("invariant_query").string(), cancellation)) {
                                    Row row = rows.next();
                                    if (row == null || row.values().size() != 1 || !session.render(row.get(0)).equals(worker.get("invariant_expect").string()) || rows.next() != null)
                                        throw new AssertionError("parallel reader observed a torn snapshot");
                                    checks.incrementAndGet();
                                }
                            }
                        }
                        return null;
                    } catch (Throwable failure) { cancellation.cancel(); throw failure; }
                }));
            }
            start.countDown();
            try { for (Future<?> job : jobs) job.get(Math.max(1, deadline - System.nanoTime()), TimeUnit.NANOSECONDS); }
            finally { cancellation.cancel(); executor.shutdown(); if (!executor.awaitTermination(5, TimeUnit.SECONDS)) throw new IllegalStateException("stress workers did not stop after cancellation"); }
            JsonNode finalState = profile.get("final"); List<JsonNode> expected = finalState.get("expect").elements();
            int rowIndex = 0; long hash = 0xcbf29ce484222325L;
            try (Session reader = database.read(); Rows rows = reader.query(finalState.get("query").string())) {
                Row row;
                while ((row = rows.next()) != null) {
                    if (rowIndex >= expected.size() || row.values().size() != expected.get(rowIndex).elements().size()) throw new AssertionError("stress final shape differs");
                    for (int column = 0; column < row.values().size(); column++) {
                        String value = reader.render(row.get(column));
                        if (!value.equals(expected.get(rowIndex).elements().get(column).render())) throw new AssertionError("stress final state differs");
                        for (byte b : value.getBytes(StandardCharsets.UTF_8)) hash = (hash ^ (b & 255)) * 0x100000001b3L;
                        hash = (hash ^ 0x1f) * 0x100000001b3L;
                    }
                    hash = (hash ^ 0x1e) * 0x100000001b3L; rowIndex++;
                }
            }
            if (rowIndex != expected.size()) throw new AssertionError("stress final rows missing");
            System.out.println(JsonNode.object(List.of(field("schema", 1), field("name", profile.get("meta").get("name").string()),
                field("lang", "java-native"), field("storage", storage), field("mode", "threaded"), field("status", "pass"),
                field("invariant_checks", checks.get()), field("writers", writers), field("writer_iters", writes.get()),
                new JsonNode.Member("final_ok", JsonNode.bool(true)), field("checksum", String.format("%016x", hash)),
                new JsonNode.Member("cross_core_checksum", finalState.get("cross_core_checksum")), field("duration_ms", (System.nanoTime() - started) / 1_000_000))).render());
        } finally {
            cancellation.cancel(); executor.shutdown();
            try (var paths = Files.walk(directory)) { for (Path path : paths.sorted(java.util.Comparator.reverseOrder()).toList()) Files.deleteIfExists(path); }
        }
    }
}
