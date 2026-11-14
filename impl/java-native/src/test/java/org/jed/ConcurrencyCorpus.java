package org.jed;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/** Frozen Layer1/2 schedules. The blocked-open annotation uses its specified serial equivalent. */
final class ConcurrencyCorpus {
    private ConcurrencyCorpus() { }
    private static boolean command(String line) { return line.matches("(?:open|close|commit|rollback|expect|on)\\s.*"); }
    private static String plain(String line) { int comment = line.indexOf('#'); return (comment < 0 ? line : line.substring(0, comment)).trim(); }
    static int run(Path file, boolean disk, boolean encrypted, Environment environment, List<String> attachments, Corpus.Inputs inputs) throws Exception {
        int count = runMode(file, disk, encrypted, environment, attachments, inputs, false);
        int threaded = runMode(file, disk, encrypted, environment, attachments, inputs, true);
        if (count != threaded) throw new AssertionError("threaded schedule changed record count");
        return count;
    }
    private static int runMode(Path file, boolean disk, boolean encrypted, Environment environment, List<String> attachments, Corpus.Inputs inputs, boolean threaded) throws Exception {
        List<String> lines = Files.readAllLines(file);
        Path directory = disk ? Files.createTempDirectory("jed-concurrency-") : null;
        Database opened = disk ? Database.createExclusive(directory.resolve("database.jed"), 8192, 262144, encrypted ? Credential.hostKey(new byte[32]) : null, environment) : Database.memory(environment);
        Map<String, Worker> sessions = new LinkedHashMap<>(); String writer = null, blocked = null; int records = 0;
        try (Database database = opened) {
            long initialVersion = database.snapshot().version();
            for (String name : attachments) database.attach(name, AttachSource.memory(), false);
            for (String line : lines) if (line.trim().startsWith("# max_storage_bytes:")) database.setMaxStorageBytes(Long.parseLong(line.substring(line.indexOf(':') + 1).trim()));
            for (int i = 0; i < lines.size();) {
                String line = plain(lines.get(i++)); if (line.isEmpty()) continue;
                int number = i; String[] words = line.split("\\s+", 3);
                switch (words[0]) {
                    case "open" -> {
                        if (sessions.containsKey(words[1]) || words[1].equals(blocked)) throw new AssertionError("duplicate scheduled session");
                        if (words[2].equals("write blocks")) {
                            if (writer == null || blocked != null) throw new AssertionError("blocks requires one held writer"); blocked = words[1]; if (threaded) { Worker queued = new Worker(database, false, true); sessions.put(blocked, queued); if (queued.open.isDone()) throw new AssertionError("writer did not block"); }
                        } else if (words[2].equals("read")) { Worker reader = new Worker(database, true, threaded); reader.await(); sessions.put(words[1], reader); }
                        else if (words[2].equals("write")) {
                            if (writer != null) throw new AssertionError("unannotated blocked writer");
                            Worker session = new Worker(database, false, threaded); session.await(); sessions.put(words[1], session); writer = words[1];
                        } else throw new AssertionError("unknown session kind");
                    }
                    case "commit", "rollback", "close" -> {
                        Worker session = sessions.remove(words[1]); if (session == null) throw new AssertionError("unknown scheduled session");
                        try { if (!words[0].equals("close")) session.run(value -> execute(value, words[0].toUpperCase(java.util.Locale.ROOT))); } finally { session.close(); }
                        if (words[1].equals(writer)) {
                            writer = null;
                            if (blocked != null) { writer = blocked; blocked = null; Worker next = threaded ? sessions.get(writer) : new Worker(database, false, false); next.await(); sessions.put(writer, next); }
                        }
                    }
                    case "expect" -> {
                        long actual = switch (words[1]) { case "version" -> database.snapshot().version(); case "oldest_live" -> database.oldestLive(); default -> throw new AssertionError("unknown concurrency assertion"); };
                        long expected = Long.parseLong(words[2]) + initialVersion;
                        if (actual != expected) throw new AssertionError(file + ":" + number + ": " + words[1] + " expected " + expected + " got " + actual);
                    }
                    case "on" -> {
                        Worker session = sessions.get(words[1]); if (session == null) throw new AssertionError("unknown scheduled session");
                        String header = words[2]; List<String> sql = new ArrayList<>(), expected = new ArrayList<>();
                        while (i < lines.size()) { String next = lines.get(i); if (next.trim().isEmpty() || next.equals("----") || command(plain(next)) || next.trim().startsWith("#")) break; sql.add(next); i++; }
                        if (header.startsWith("query ")) {
                            if (i >= lines.size() || !lines.get(i++).equals("----")) throw new AssertionError("query missing separator");
                            while (i < lines.size()) { String next = lines.get(i); if (next.trim().isEmpty() || command(plain(next)) || next.trim().startsWith("#")) break; expected.add(next); i++; }
                        }
                        session.run(value -> Corpus.checkedRecord(file, database, value, new Corpus.Record(number, header, String.join("\n", sql), List.copyOf(expected), Map.of()), inputs)); records++;
                    }
                    default -> throw new AssertionError(file + ":" + number + ": unknown concurrency step " + line);
                }
            }
            if (blocked != null) throw new AssertionError("blocked writer was never released");
        } finally {
            for (Worker session : sessions.values()) session.close();
            if (directory != null) try (var paths = Files.walk(directory)) { for (Path path : paths.sorted(java.util.Comparator.reverseOrder()).toList()) Files.deleteIfExists(path); }
        }
        System.out.println("PASS " + file + " (concurrency " + (threaded ? "threaded " : "sequential ") + (encrypted ? "encrypted" : disk ? "disk" : "memory") + "): " + records + " records"); return records;
    }
    private static final class Worker implements AutoCloseable {
        private final java.util.concurrent.ExecutorService executor;
        private Session session;
        final java.util.concurrent.CompletableFuture<Void> open = new java.util.concurrent.CompletableFuture<>();
        Worker(Database database, boolean read, boolean threaded) {
            executor = threaded ? java.util.concurrent.Executors.newSingleThreadExecutor(Thread.ofPlatform().daemon().name("jed-scheduled-session").factory()) : null;
            Runnable create = () -> { try { session = read ? database.read() : database.session(); if (!read) execute(session, "BEGIN"); open.complete(null); } catch (Throwable failure) { open.completeExceptionally(failure); } };
            if (executor == null) create.run(); else executor.execute(create);
        }
        void await() throws Exception { result(open); }
        void run(java.util.function.Consumer<Session> action) throws Exception {
            await();
            if (executor == null) action.accept(session);
            else result(executor.submit(() -> action.accept(session)));
        }
        public void close() throws Exception {
            try { if (session != null) run(Session::close); } finally { if (executor != null) executor.shutdownNow(); }
        }
        private static void result(java.util.concurrent.Future<?> task) throws Exception {
            try { task.get(30, java.util.concurrent.TimeUnit.SECONDS); }
            catch (java.util.concurrent.ExecutionException failed) { if (failed.getCause() instanceof Exception error) throw error; if (failed.getCause() instanceof Error error) throw error; throw new RuntimeException(failed.getCause()); }
        }
    }
    private static void execute(Session session, String sql) { try (Rows rows = session.query(sql)) { while (rows.next() != null) { } } }
}
