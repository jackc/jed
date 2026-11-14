package org.jed;

import java.util.List;
import java.util.concurrent.CompletableFuture;

/** Prepared handles outlive their preparing session and always use the executing context. */
public final class PreparedStatementTest {
    public static void main(String[] args) {
        PreparedStatement select;
        try (Database first = Database.memory(); Session session = first.session()) { select = session.prepare("SELECT $1 + 1, current_setting('app.value', true)"); }
        try (Database database = Database.memory(); Session session = database.session()) {
            session.setVar("app.value", "current");
            try (Rows rows = session.queryPrepared(select, SqlValue.ofInteger(4))) {
                Row row = rows.next(); check(row.values().get(0).asInteger() == 5 && row.values().get(1).asText().equals("current"), "prepared statement retained its preparing context");
            }
            session.setMaxSqlLength(7); expect("54000", () -> session.prepare("SELECT 1"));
            try (Rows rows = session.queryPrepared(select, SqlValue.ofInteger(5))) { check(rows.next().values().getFirst().asInteger() == 6, "prepared execution reparsed under input limit"); }
            session.setMaxSqlLength(10); expect("54000", () -> session.prepare("SELECT 'é'"));
            session.setMaxSqlLength(Session.DEFAULT_MAX_SQL_LENGTH);
            expect("42601", () -> session.queryPrepared(select));
            expect("42601", () -> session.queryPrepared(select, SqlValue.ofInteger(1), SqlValue.ofInteger(2)));
            check(database.execute("CREATE TABLE t (id i64 PRIMARY KEY, v i64)") == -1, "DDL has no affected row count");
            PreparedStatement insert = database.prepare("INSERT INTO t VALUES ($1,$2)");
            check(session.executePrepared(insert, SqlValue.ofInteger(1), SqlValue.ofInteger(10)) == 1, "insert count");
            check(session.execute("INSERT INTO t VALUES (2,20),(3,30)") == 2, "multiinsert count");
            check(session.execute("UPDATE t SET v=v") == 3, "no-op update count");
            check(session.execute("INSERT INTO t VALUES (1,99),(4,40) ON CONFLICT DO NOTHING") == 1, "DO NOTHING count");
            check(session.execute("INSERT INTO t VALUES (1,99) ON CONFLICT (id) DO UPDATE SET v=excluded.v") == 1, "upsert count");
            PreparedStatement lookup = database.prepare("SELECT v FROM t WHERE id=$1");
            try (Rows rows = database.queryPrepared(lookup, SqlValue.ofInteger(1))) { check(rows.next().values().getFirst().asInteger() == 99, "database prepared read"); }
            session.revoke("t", Privilege.SELECT);
            expect("42501", () -> session.queryPrepared(lookup, SqlValue.ofInteger(1)));
            session.clearPrivilegeOverrides();
            check(session.execute("DELETE FROM t WHERE id=4 RETURNING id") == 1, "delete RETURNING count");
            check(session.execute("DELETE FROM t WHERE id=99") == 0, "empty delete count");
            CompletableFuture<?>[] readers = new CompletableFuture<?>[4];
            for (int i = 0; i < readers.length; i++) readers[i] = CompletableFuture.runAsync(() -> {
                try (Session reader = database.session()) { for (int n = 0; n < 20; n++) try (Rows rows = reader.queryPrepared(lookup, SqlValue.ofInteger(2))) { check(rows.next().values().getFirst().asInteger() == 20, "concurrent prepared result"); } }
            });
            CompletableFuture.allOf(readers).join();
        }
        System.out.println("PreparedStatementTest: detached/shared parse, current session context, bind counts, privilege checks and mutation tags passed");
    }
    private static void expect(String state, Runnable action) { SessionVariablesTest.expect(state, action); }
    private static void check(boolean value, String message) { SessionVariablesTest.check(value, message); }
}
