package org.jed;

/** Sequence histories are session-local and roll back with snapshots and savepoints. */
public final class SequenceSessionTest {
    public static void main(String[] args) {
        try (Database database = Database.memory(); Session first = database.session(); Session second = database.session()) {
            run(first, "CREATE SEQUENCE s"); value(first, "SELECT nextval('s')", 1);
            state(second, "SELECT currval('s')", "55000"); value(second, "SELECT nextval('s')", 2);
            value(first, "SELECT currval('s')", 1); value(second, "SELECT currval('s')", 2);
            run(first, "BEGIN"); value(first, "SELECT nextval('s')", 3);
            run(first, "SAVEPOINT p"); value(first, "SELECT nextval('s')", 4);
            run(first, "ROLLBACK TO p"); value(first, "SELECT currval('s')", 3); value(first, "SELECT lastval()", 3);
            run(first, "ROLLBACK"); value(first, "SELECT currval('s')", 1); value(first, "SELECT lastval()", 1);
            value(second, "SELECT nextval('s')", 3);
            state(first, "SELECT nextval('s'), 1 / 0", "22012"); value(first, "SELECT currval('s')", 1);
            value(second, "SELECT nextval('s')", 4);
            run(first, "BEGIN READ ONLY"); state(first, "SELECT nextval('s')", "25006"); run(first, "ROLLBACK");
            run(first, "BEGIN"); value(first, "SELECT nextval('s')", 5); run(first, "COMMIT");
            value(first, "SELECT currval('s')", 5); value(first, "SELECT lastval()", 5);
            value(first, "SELECT setval('s', 20)", 20); value(first, "SELECT lastval()", 20);
            value(first, "SELECT setval('s', 30, false)", 30); value(first, "SELECT lastval()", 20);
        }
        System.out.println("SequenceSessionTest passed (independent histories, failed statement, rollback, savepoint and read-only)");
    }
    private static void run(Session session, String sql) { try (Rows rows = session.query(sql)) { while (rows.next() != null) { } } }
    private static void value(Session session, String sql, long expected) {
        try (Rows rows = session.query(sql)) {
            Row row = rows.next(); if (row == null || row.get(0).asInteger() != expected || rows.next() != null) throw new AssertionError(sql + " expected " + expected);
        }
    }
    private static void state(Session session, String sql, String expected) {
        try { run(session, sql); throw new AssertionError(sql + " did not fail"); }
        catch (SqlException failure) { if (!failure.sqlState().equals(expected)) throw new AssertionError(sql + " expected " + expected + " got " + failure.sqlState()); }
    }
}
