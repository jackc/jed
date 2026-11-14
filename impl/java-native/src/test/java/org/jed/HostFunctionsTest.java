package org.jed;

import java.util.List;
import java.util.concurrent.atomic.AtomicInteger;

/** Host callbacks cannot be registered or observed through the SQL corpus. */
public final class HostFunctionsTest {
    public static void main(String[] args) {
        var calls = new AtomicInteger();
        HostFunction plus = HostFunction.scalar("host_plus", List.of(SqlType.I64, SqlType.I64), SqlType.I64, values -> { calls.incrementAndGet(); return SqlValue.ofInteger(values.get(0).asInteger() + values.get(1).asInteger()); }).cost(3);
        Environment.Builder builder = Environment.builder().registerFunction(plus);
        expect("42723", () -> builder.registerFunction(plus)); expect("22023", () -> plus.cost(-1));
        builder.registerFunction(HostFunction.scalar("host_number", List.of(SqlType.DECIMAL), SqlType.DECIMAL, values -> { check(values.getFirst().type() == SqlType.DECIMAL, "host argument was not coerced"); return values.getFirst(); }));
        builder.registerFunction(HostFunction.scalar("abs", List.of(SqlType.I64), SqlType.I64, values -> SqlValue.ofInteger(999)));
        builder.registerFunction(HostFunction.scalar("abs", List.of(SqlType.TEXT), SqlType.TEXT, values -> SqlValue.ofText("host:" + values.getFirst().asText())));
        builder.registerFunction(HostFunction.scalar("HostCase", List.of(), SqlType.I64, values -> SqlValue.ofInteger(7)));
        builder.registerFunction(HostFunction.scalar("host_bad", List.of(), SqlType.I64, values -> SqlValue.ofText("wrong")));
        builder.registerFunction(HostFunction.scalar("host_wide", List.of(), SqlType.I16, values -> SqlValue.ofInteger(32768)));
        builder.registerFunction(HostFunction.scalar("host_throw", List.of(), SqlType.I64, values -> { throw new IllegalStateException("broken"); }));
        builder.registerFunction(HostFunction.batched("host_batch", List.of(SqlType.I64), SqlType.I64, (columns, output) -> { for (SqlValue value : columns.getFirst()) output.add(value); }).volatility(HostFunction.Volatility.STABLE));
        builder.registerFunction(HostFunction.batched("host_short", List.of(SqlType.I64), SqlType.I64, (columns, output) -> {}));
        builder.registerFunction(HostFunction.batched("host_long", List.of(SqlType.I64), SqlType.I64, (columns, output) -> { output.add(SqlValue.ofInteger(1)); output.add(SqlValue.ofInteger(2)); }));
        builder.registerFunction(HostFunction.batched("host_error_after", List.of(SqlType.I64), SqlType.I64, (columns, output) -> { output.add(SqlValue.ofInteger(1)); throw new SqlException("22012", "late"); }));
        Environment environment = builder.build(); builder.registerFunction(HostFunction.scalar("later", List.of(), SqlType.I64, values -> SqlValue.ofInteger(1)));
        check(environment.functions().size() + 1 == builder.build().functions().size(), "environment registry mutated");
        try (Database database = Database.memory(environment); Session session = database.session()) {
            try (Rows rows = session.query("SELECT host_plus(2, 4), host_number(3), abs(-2), abs('x'), \"HostCase\"(), host_batch(8)")) {
                Row row = rows.next(); check(row.values().get(0).asInteger() == 6 && row.values().get(1).asDecimal().equals(Decimal.of(3)) && row.values().get(2).asInteger() == 2 && row.values().get(3).asText().equals("host:x") && row.values().get(4).asInteger() == 7 && row.values().get(5).asInteger() == 8, "host results and builtin preference");
            }
            int before = calls.get(); execute(session, "SELECT host_plus(NULL::i64, 4)"); check(calls.get() == before, "strict NULL called host");
            expect("42883", () -> execute(session, "SELECT host_plus(NULL, 4)")); expect("42883", () -> execute(session, "SELECT hostcase()")); expect("42883", () -> execute(session, "SELECT later()"));
            expect("22000", () -> execute(session, "SELECT host_bad()")); expect("22003", () -> execute(session, "SELECT host_wide()")); expect("38000", () -> execute(session, "SELECT host_throw()"));
            for (String name : List.of("host_short", "host_long", "host_error_after")) expect("22000", () -> execute(session, "SELECT " + name + "(1)"));
            session.setMaxCost(3); before = calls.get(); expect("54P01", () -> execute(session, "SELECT host_plus(2,4)")); check(calls.get() == before, "cost guard ran after host");
        }
        batching();
        System.out.println("HostFunctionsTest: immutable registry, overloads, checked results, strictness, cost and batch shape passed");
    }
    private static void batching() {
        var widths = new java.util.ArrayList<Integer>();
        HostFunction function = HostFunction.batched("batch_map", List.of(SqlType.I64), SqlType.I64, (columns, out) -> {
            widths.add(columns.getFirst().size());
            for (SqlValue value : columns.getFirst()) {
                if (value.asInteger() == 6) throw new SqlException("22012", "host row six");
                out.add(SqlValue.ofInteger(value.asInteger() * 2));
            }
        }).volatility(HostFunction.Volatility.STABLE).cost(2);
        try (Database database = Database.memory(Environment.builder().registerFunction(function).build()); Session session = database.session()) {
            execute(session, "CREATE TABLE batch_rows (id i64 PRIMARY KEY)");
            execute(session, "INSERT INTO batch_rows SELECT i FROM generate_series(1,8) AS i");
            widths.clear();
            try (Rows rows = session.query("SELECT batch_map(id) FROM batch_rows ORDER BY id")) {
                for (int i = 1; i <= 5; i++) check(rows.next().values().getFirst().asInteger() == 2 * i, "batched prefix changed");
                expect("22012", rows::next);
            }
            check(widths.getFirst() > 1, "scan host kernel did not batch");
            widths.clear(); execute(session, "SELECT batch_map(id) FROM batch_rows LIMIT 1"); check(widths.equals(List.of(1)), "LIMIT one speculated extra rows");
            widths.clear(); execute(session, "SELECT CASE WHEN id < 3 THEN batch_map(id) ELSE 0 END FROM batch_rows"); check(widths.equals(List.of(1, 1)), "conditional call was speculated");
            widths.clear();
            try (Rows rows = session.query("SELECT batch_map(x) FROM (VALUES (1::i64),(2::i64),(3::i64)) AS d(x)")) {
                for (int i = 1; i <= 3; i++) check(rows.next().values().getFirst().asInteger() == 2 * i, "buffered host result");
                check(rows.next() == null, "buffered host extra row");
            }
            check(widths.equals(List.of(3)), "buffered host kernel did not batch");
            widths.clear();
            expect("22012", () -> execute(session, "SELECT 1/(id-2), batch_map(id) FROM batch_rows"));
            check(!widths.isEmpty(), "batch replay test never reached host");
        }
    }
    private static void execute(Session session, String sql) { SessionVariablesTest.execute(session, sql); }
    private static void expect(String state, Runnable action) { SessionVariablesTest.expect(state, action); }
    private static void check(boolean value, String message) { SessionVariablesTest.check(value, message); }
}
