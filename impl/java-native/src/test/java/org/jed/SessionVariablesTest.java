package org.jed;

/** Host-only settings isolation, lifecycle and transaction boundaries. */
public final class SessionVariablesTest {
    public static void main(String[] args) {
        try (Database database = Database.memory(); Session first = database.session(); Session second = database.session()) {
            first.setVar("App.Tenant", "A"); check("A".equals(first.var("APP.TENANT")), "ASCII name folding");
            check(second.var("app.tenant") == null && first.var("unset") == null, "session isolation");
            expect("42704", () -> first.setVar("search_path", "x")); expect("42704", () -> first.resetVar("timezone"));
            execute(first, "BEGIN"); first.setVar("app.tenant", "B"); execute(first, "ROLLBACK"); check("B".equals(first.var("app.tenant")), "variables rolled back");
            try (Rows rows = first.query("SELECT current_setting('APP.TENANT'), current_setting(NULL::text), current_setting('unset', true), current_setting('app.tenant', NULL::boolean)")) {
                Row row = rows.next(); check(row.values().getFirst().asText().equals("B"), "SQL setting value");
                for (int i = 1; i < 4; i++) check(row.values().get(i).isNull(), "strict setting NULL");
                expect("55000", () -> first.setVar("app.tenant", "C"));
            }
            first.resetVar("APP.TENANT"); check(first.var("app.tenant") == null, "reset did not remove");
            execute(first, "BEGIN"); expect("42704", () -> execute(first, "SELECT current_setting('unset')")); expect("25P02", () -> execute(first, "SELECT 1")); execute(first, "ROLLBACK");
        }
        System.out.println("SessionVariablesTest: independent settings, non-rollback, frozen cursor and failures passed");
    }
    static void execute(Session session, String sql) { try (Rows rows = session.query(sql)) { while (rows.next() != null) {} } }
    static void expect(String state, Runnable operation) { try { operation.run(); throw new AssertionError("expected " + state); } catch (SqlException failure) { if (!failure.sqlState().equals(state)) throw failure; } }
    static void check(boolean value, String message) { if (!value) throw new AssertionError(message); }
}
