package org.jed;

import java.util.List;

/** Internal lexical slot identities and host-source isolation, outside the SQL corpus surface. */
public final class ExplainExpressionTest {
    private static void equal(String expected, String actual) {
        if (!expected.equals(actual)) throw new AssertionError(expected + " != " + actual);
    }
    private static SqlParser.Name name(String... parts) { return new SqlParser.Name(List.of(parts)); }
    public static void main(String[] args) {
        var ancestorBindings = List.of(new ScalarPlan.Binding(List.of("a"), "id", SqlType.I32, false), new ScalarPlan.Binding(List.of("a"), "value", SqlType.TEXT, false));
        var parent = new ScalarPlan.Resolver(new SqlValue[0], ancestorBindings, new ScalarPlan.Frame());
        var localBindings = List.of(new ScalarPlan.Binding(List.of("b"), "id", SqlType.I32, false));
        var child = new ScalarPlan.Resolver(new SqlValue[0], localBindings, new ScalarPlan.Frame(), null, parent);
        var printer = new ExplainExpression(localBindings, child, null);
        equal("#0", printer.print(name("id")));
        equal("outer(1,0)", printer.print(name("a", "id")));
        equal("outer(1,1)", printer.print(name("value")));
        equal("row(outer(1,0), outer(1,1))", printer.print(name("a")));
        var grandchild = new ScalarPlan.Resolver(new SqlValue[0], List.of(), new ScalarPlan.Frame(), null, child);
        equal("outer(2,1)", new ExplainExpression(List.of(), grandchild, null).print(name("value")));
        var sibling = new ScalarPlan.Resolver(new SqlValue[0], List.of(), new ScalarPlan.Frame());
        var merged = sibling.mergeEarlier(child);
        equal("outer(1,1)", new ExplainExpression(merged.metadataBindings(), merged, null).print(name("value")));
        equal("NULL", printer.print(new SqlParser.StoredValue(SqlValue.nullValue(SqlType.TEXT))));
        equal("'a''b\\\\c'", printer.print(new SqlParser.StoredValue(SqlValue.ofText("a'b\\c"))));
        equal("'{1,2}'", printer.print(new SqlParser.StoredValue(SqlValue.ofArray(SqlType.array(SqlType.I64), SqlArray.vector(List.of(SqlValue.ofInteger(1), SqlValue.ofInteger(2)))))));
        Environment environment = Environment.builder().clockSource(() -> { throw new AssertionError("EXPLAIN read host clock"); }).build();
        try (Database database = Database.memory(environment); Session session = database.session(); Rows rows = session.query("EXPLAIN (VERBOSE, COSTS FALSE) SELECT date 'today', date 'yesterday', date 'tomorrow', (SELECT 1/0)")) {
            equal("output=[date_clock(0), date_clock(-1), date_clock(1), scalar(<subquery>)]", rows.next().get(2).asText());
            if (rows.next() != null) throw new AssertionError("unexpected explain row");
        }
        System.out.println("ExplainExpressionTest: lexical slot identities, stored constants and inert host clock/subquery metadata passed");
    }
}
