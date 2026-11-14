package org.jed;

import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/** Public handle/cursor ownership and cancellation contracts outside SQL corpus reach. */
public final class HostLifecycleTest {
    private HostLifecycleTest() {}
    public static void main(String[] args) throws Exception {
        affectedRows();
        try (Database database = Database.memory(); Session session = database.session()) {
            Rows active = session.query("SELECT 42 AS answer");
            state("55000", () -> session.query("SELECT 1"));
            Row retained = active.next();
            check(retained != null && retained.get("answer").asInteger() == 42, "owned row");
            active.close();
            active.close();
            check(retained.get(0).asInteger() == 42, "row survives cursor close");
            try (Rows next = session.query("SELECT 1")) {
                active.close();
                state("55000", () -> session.query("SELECT 2"));
                check(next.next() != null && next.next() == null, "drain releases operation");
            }
            Cancellation before = new Cancellation();
            before.cancel();
            state("57014", () -> session.query("SELECT 1", before));
            Cancellation after = new Cancellation();
            try (Rows cancelled = session.query("SELECT 1", after)) {
                after.cancel();
                state("57014", cancelled::next);
                check(cancelled.next() == null, "failed cursor closed");
            }
            try (Rows failure = session.query("SELECT 1 / 0")) {
                state("22012", failure::next);
            }
            try (Rows recovered = session.query("SELECT 1")) {
                check(recovered.next() != null, "session recovers from cursor error");
            }
            try (var executor = Executors.newVirtualThreadPerTaskExecutor()) {
                List<Future<?>> tasks = new ArrayList<>();
                for (int i = 0; i < 4; i++) tasks.add(executor.submit(() -> {
                    try (Session separate = database.session()) {
                        for (int iteration = 0; iteration < 50; iteration++) {
                            try (Rows rows = separate.query("SELECT 1")) {
                                check(rows.next() != null && rows.next() == null, "independent virtual-thread sessions");
                            }
                        }
                    }
                }));
                for (Future<?> task : tasks) task.get();
            }
        }
        Database closed = Database.memory();
        Session child = closed.session();
        Rows cursor = child.query("SELECT 1");
        closed.close();
        closed.close();
        state("55000", closed::session);
        state("55000", () -> child.query("SELECT 1"));
        check(cursor.next() == null, "database close owns cursor cleanup");
        concurrentClose();
        writerClose();
        setupClose();
        System.out.println("HostLifecycleTest passed");
    }
    private static void affectedRows() {
        try (Database database = Database.memory(); Session session = database.session()) {
            check(session.execute("CREATE TABLE affected (id i64 PRIMARY KEY)") == -1, "DDL has no affected count");
            check(session.execute("INSERT INTO affected VALUES (1), (2)") == 2, "insert affected count");
            check(session.execute("UPDATE affected SET id = id WHERE id = 9") == 0, "zero DML has a present zero count");
            check(session.execute("DELETE FROM affected WHERE id = 1 RETURNING id") == 1, "returning affected count");
            check(session.execute("SELECT * FROM affected") == -1, "SELECT has no affected count");
            check(session.execute("ANALYZE affected") == 0, "ANALYZE reports zero");
            check(session.execute("CREATE TABLE filled AS SELECT id FROM affected") == 1, "CTAS affected count");
            check(session.execute("CREATE TABLE IF NOT EXISTS filled AS SELECT id FROM affected") == -1, "skipped CTAS has no count");
            check(session.execute("CREATE TABLE empty AS SELECT id FROM affected WITH NO DATA") == -1, "no-data CTAS has no count");
            check(session.execute("BEGIN") == -1 && session.execute("ROLLBACK") == -1, "transaction controls have no count");
        }
    }
    private static void concurrentClose() throws Exception {
        try (Database database = Database.memory(); Session session = database.session();
             var executor = Executors.newVirtualThreadPerTaskExecutor()) {
            CountDownLatch entered = new CountDownLatch(1);
            CountDownLatch resume = new CountDownLatch(1);
            QueryPlan plan = pausedPlan(entered, resume);
            session.enter();
            Rows active = new Rows(session, plan, new CostMeter(session, null));
            session.publish(active);
            Future<Row> advancing = executor.submit(active::next);
            try {
                check(entered.await(10, TimeUnit.SECONDS), "kernel entry timeout");
                state("55000", active::next);
                active.close();
                active.close();
                state("55000", () -> session.query("SELECT 2"));
            } finally { resume.countDown(); }
            check(advancing.get() == null, "closed cursor cannot publish an in-flight row");
            try (Rows replacement = session.query("SELECT 2")) {
                active.close();
                session.release(active);
                state("55000", () -> session.query("SELECT 3"));
                check(replacement.next() != null, "replacement remains active after old cleanup");
            }
        }
    }
    private static QueryPlan pausedPlan(CountDownLatch entered, CountDownLatch resume) {
        return new QueryPlan() {
            public List<String> names() { return List.of("value"); }
            public List<SqlType> types() { return List.of(SqlType.I64); }
            public Row next(CostMeter meter) {
                entered.countDown();
                try { if (!resume.await(10, TimeUnit.SECONDS)) throw new AssertionError("kernel resume timeout"); }
                catch (InterruptedException error) { throw new AssertionError(error); }
                return new Row(names(), List.of(SqlValue.ofInteger(1)));
            }
        };
    }
    private static void writerClose() throws Exception {
        try (Database database = Database.memory(); Session owner = database.session();
             Session successor = database.session(); var executor = Executors.newVirtualThreadPerTaskExecutor()) {
            try (Rows begin = owner.query("BEGIN")) { check(begin.next() == null, "begin writer"); }
            CountDownLatch entered = new CountDownLatch(1), resume = new CountDownLatch(1);
            owner.enter();
            Rows active = new Rows(owner, pausedPlan(entered, resume), new CostMeter(owner, null)); owner.publish(active);
            Future<Row> advancing = executor.submit(active::next);
            try {
                check(entered.await(10, TimeUnit.SECONDS), "writer kernel entry");
                owner.close();
                var writer = Database.class.getDeclaredField("writer"); writer.setAccessible(true);
                synchronized (database) { check(writer.get(database) == owner, "writer remains owned until in-flight cursor finishes"); }
                state("55000", () -> owner.query("SELECT 1"));
            } finally { resume.countDown(); }
            check(advancing.get(10, TimeUnit.SECONDS) == null, "closed writer cursor cannot publish row");
            try (Rows begin = successor.query("BEGIN")) { check(begin.next() == null, "successor obtains released writer"); }
            active.close();
            var writer = Database.class.getDeclaredField("writer"); writer.setAccessible(true);
            synchronized (database) { check(writer.get(database) == successor, "old cursor cannot release successor writer"); }
        }
    }
    private static void setupClose() throws Exception {
        try (Database database = Database.memory(); Session closing = database.session()) {
            var result = new java.util.concurrent.CompletableFuture<String>();
            Thread thread;
            synchronized (database) {
                // The host database monitor pauses setup after Session.enter and before writer acquisition.
                thread = Thread.ofPlatform().start(() -> {
                    try (Rows ignored = closing.query("BEGIN")) { result.complete("published"); }
                    catch (SqlException failure) { result.complete(failure.sqlState()); }
                    catch (Throwable failure) { result.completeExceptionally(failure); }
                });
                long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(10);
                while (thread.getState() != Thread.State.BLOCKED && !result.isDone() && System.nanoTime() < deadline) Thread.yield();
                check(thread.getState() == Thread.State.BLOCKED, "query setup reached database gate");
                closing.close();
            }
            check(result.get(10, TimeUnit.SECONDS).equals("55000"), "closing during setup refuses cursor publication");
            thread.join();
            var sessions = Database.class.getDeclaredField("sessions"); sessions.setAccessible(true);
            synchronized (database) { check(((java.util.Set<?>) sessions.get(database)).isEmpty(), "closed setup releases database ownership"); }
            try (Session replacement = database.session(); Rows begin = replacement.query("BEGIN")) {
                check(begin.next() == null, "failed setup leaves no writer lease");
            }
        }
    }
    private static void state(String expected, Runnable action) {
        try { action.run(); throw new AssertionError("expected SQLSTATE " + expected); }
        catch (SqlException error) { check(expected.equals(error.sqlState()), "SQLSTATE " + error.sqlState()); }
    }
    private static void check(boolean condition, String message) {
        if (!condition) throw new AssertionError(message);
    }
}
