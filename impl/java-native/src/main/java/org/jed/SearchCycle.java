package org.jed;

import java.util.ArrayList;
import java.util.HashSet;
import java.util.List;

/** SEARCH/CYCLE's ordinary resolved expressions, evaluated over coerced output and parent fields. */
final class SearchCycle {
    record Marks(SqlValue cycle, SqlValue ordinary) {}
    private final int width;
    private final List<String> names;
    private final List<SqlType> types;
    private final ScalarPlan.Frame frame = new ScalarPlan.Frame();
    private final List<ScalarPlan.Expression> anchor = new ArrayList<>(), recursive = new ArrayList<>();
    private final ScalarPlan.Expression keep;

    static boolean present(SqlParser.Cte definition) { return definition.search() != null || definition.cycle() != null; }
    static void requireRecursive(SqlParser.Cte definition, boolean recursive) {
        if (present(definition) && (!recursive || CteReferences.count(definition.body(), definition.name()) == 0))
            throw new SqlException("42601", "WITH query is not recursive");
    }
    static Marks marks(SqlParser.Cte definition, QueryEnvironment scope) {
        if (definition.cycle() == null) return null;
        SqlParser.Expr yes = definition.cycle().value(), no = definition.cycle().otherwise();
        ScalarPlan.Resolver resolver = scope.resolver();
        SqlType a = resolver.resolve(yes, null).type(), b = resolver.resolve(no, null).type();
        if (unknown(yes) && !unknown(no)) a = b;
        if (unknown(no) && !unknown(yes)) b = a;
        SqlType type = ScalarPlan.common(a, b); if (type == SqlType.UNKNOWN) type = SqlType.TEXT;
        ScalarPlan.requireComparable(type);
        SqlType selected = type;
        return scope.meter().unmetered(() -> new Marks(
            SqlCasts.cast(resolver.resolve(yes, selected).run(scope.meter()), selected, scope.meter()),
            SqlCasts.cast(resolver.resolve(no, selected).run(scope.meter()), selected, scope.meter())));
    }
    private static boolean unknown(SqlParser.Expr value) {
        return value instanceof SqlParser.Literal literal && (literal.kind() == Lexer.Kind.STRING || literal.kind() == Lexer.Kind.WORD && literal.text().equals("null"));
    }
    SearchCycle(SqlParser.Cte definition, List<String> baseNames, List<SqlType> baseTypes, QueryEnvironment scope, Marks marks) {
        width = baseTypes.size(); var outputNames = new ArrayList<>(baseNames); var outputTypes = new ArrayList<>(baseTypes);
        SqlParser.Search search = definition.search(); SqlParser.Cycle cycle = definition.cycle();
        List<Integer> searchKeys = search == null ? List.of() : keys(search.columns(), baseNames);
        List<Integer> cycleKeys = cycle == null ? List.of() : keys(cycle.columns(), baseNames);
        int searchSlot = -1, markSlot = -1, pathSlot = -1;
        if (search != null) {
            addName(outputNames, search.output()); searchSlot = outputTypes.size();
            SqlType record = record(searchKeys, baseTypes, search.breadth());
            outputTypes.add(search.breadth() ? record : SqlType.array(record));
        }
        if (cycle != null) {
            addName(outputNames, cycle.mark()); addName(outputNames, cycle.path());
            markSlot = outputTypes.size(); outputTypes.add(marks.cycle().type());
            pathSlot = outputTypes.size(); outputTypes.add(SqlType.array(record(cycleKeys, baseTypes, false)));
        }
        names = List.copyOf(outputNames); types = List.copyOf(outputTypes);
        var bindings = new ArrayList<ScalarPlan.Binding>();
        for (int i = 0; i < types.size(); i++) bindings.add(new ScalarPlan.Binding(List.of(), "column" + i, types.get(i), false));
        ScalarPlan.Resolver resolver = scope.resolver(bindings, frame);
        if (search != null) {
            SqlParser.Expr key = row(searchKeys), seed, step;
            if (search.breadth()) {
                var seedFields = new ArrayList<SqlParser.Expr>(); seedFields.add(integer(0)); seedFields.addAll(columns(searchKeys));
                var stepFields = new ArrayList<SqlParser.Expr>();
                stepFields.add(new SqlParser.Binary("+", new SqlParser.Field(new SqlParser.ColumnSlot(searchSlot), "f1"), integer(1))); stepFields.addAll(columns(searchKeys));
                seed = new SqlParser.RowConstructor(seedFields); step = new SqlParser.RowConstructor(stepFields);
            } else {
                seed = new SqlParser.ArrayConstructor(List.of(key), null);
                step = new SqlParser.Binary("||", new SqlParser.ColumnSlot(searchSlot), seed);
            }
            anchor.add(resolver.resolve(seed, types.get(searchSlot))); recursive.add(resolver.resolve(step, types.get(searchSlot)));
        }
        if (cycle != null) {
            SqlParser.Expr yes = new SqlParser.StoredValue(marks.cycle()), no = new SqlParser.StoredValue(marks.ordinary());
            SqlParser.Expr key = row(cycleKeys), seed = new SqlParser.ArrayConstructor(List.of(key), null);
            SqlParser.Expr found = new SqlParser.Quantified("=", key, "any", new SqlParser.ColumnSlot(pathSlot), null);
            SqlParser.Expr mark = new SqlParser.Conditional(null, List.of(found), List.of(yes), no);
            anchor.add(resolver.resolve(no, types.get(markSlot))); recursive.add(resolver.resolve(mark, types.get(markSlot)));
            anchor.add(resolver.resolve(seed, types.get(pathSlot)));
            recursive.add(resolver.resolve(new SqlParser.Binary("||", new SqlParser.ColumnSlot(pathSlot), seed), types.get(pathSlot)));
            keep = resolver.resolve(new SqlParser.Binary("<>", new SqlParser.ColumnSlot(markSlot), yes), SqlType.BOOLEAN);
        } else keep = null;
    }
    List<String> names() { return names; }
    List<SqlType> types() { return types; }
    private static SqlParser.Expr integer(long value) { return new SqlParser.StoredValue(SqlValue.ofInteger(SqlType.I64, value)); }
    private static List<SqlParser.Expr> columns(List<Integer> keys) { return keys.stream().map(index -> (SqlParser.Expr)new SqlParser.ColumnSlot(index)).toList(); }
    private static SqlParser.Expr row(List<Integer> keys) { return new SqlParser.RowConstructor(columns(keys)); }
    private static SqlType record(List<Integer> keys, List<SqlType> base, boolean depth) {
        var fields = new ArrayList<SqlType.Field>(); if (depth) fields.add(new SqlType.Field("f1", SqlType.I64));
        for (int key : keys) fields.add(new SqlType.Field("f" + (fields.size() + 1), base.get(key)));
        return SqlType.record(fields);
    }
    private static List<Integer> keys(List<String> columns, List<String> names) {
        var seen = new HashSet<String>(); var result = new ArrayList<Integer>();
        for (String name : columns) {
            if (!seen.add(name)) throw new SqlException("42701", "column appears more than once in SEARCH/CYCLE");
            int index = names.indexOf(name);
            if (index < 0) throw new SqlException("42601", "SEARCH/CYCLE column does not exist: " + name);
            result.add(index);
        }
        return result;
    }
    private static void addName(List<String> names, String name) {
        if (names.contains(name)) throw new SqlException("42601", "SEARCH/CYCLE column name is already used: " + name);
        names.add(name);
    }
    SqlParser.Select recursive(SqlParser.Select select, String cte, QueryEnvironment scope) {
        SelfReference self = null;
        for (SqlParser.From from : select.from()) { self = self(from, cte); if (self != null) break; }
        if (self == null) throw new SqlException("0A000", "SEARCH/CYCLE self reference must be a direct relation");
        String qualifier = self.qualifier();
        var inherited = new ArrayList<String>();
        for (int i = width; i < names.size(); i++) inherited.add(i < self.columns().size() ? self.columns().get(i) : names.get(i));
        for (SqlParser.Item item : select.items()) if (item.expression() instanceof SqlParser.Star star) scope.hideRecursiveStar(star, qualifier, inherited);
        var items = new ArrayList<>(select.items());
        for (String name : inherited) items.add(new SqlParser.Item(new SqlParser.Name(List.of(qualifier, name)), null));
        return new SqlParser.Select(items, select.where(), select.order(), select.limit(), select.offset(), select.distinct(), select.from(), select.group(), select.having(), select.distinctOn(), select.windows());
    }
    private record SelfReference(String qualifier, List<String> columns) {}
    private static SelfReference self(SqlParser.From from, String name) {
        if (from instanceof SqlParser.Table table) return table.name().parts().equals(List.of(name))
            ? new SelfReference(table.alias() == null ? name : table.alias(), table.columns()) : null;
        if (from instanceof SqlParser.Grouped group) {
            SelfReference reference = self(group.source(), name);
            if (reference == null || group.alias() == null) return reference;
            List<String> aliases = reference.columns();
            if (!group.columns().isEmpty()) {
                var renamed = new ArrayList<>(aliases);
                for (int i = 0; i < group.columns().size(); i++) { if (i < renamed.size()) renamed.set(i, group.columns().get(i)); else renamed.add(group.columns().get(i)); }
                aliases = List.copyOf(renamed);
            }
            return new SelfReference(group.alias(), aliases);
        }
        if (from instanceof SqlParser.Join join) { SelfReference left = self(join.left(), name); return left == null ? self(join.right(), name) : left; }
        return null;
    }
    List<SqlValue> apply(List<SqlValue> values, boolean seed, CostMeter meter) {
        frame.values = values;
        try {
            if (!seed && keep != null) { SqlValue accepted = keep.run(meter); if (accepted.isNull() || !accepted.asBoolean()) return null; }
            var result = new ArrayList<>(values.subList(0, width));
            for (ScalarPlan.Expression expression : seed ? anchor : recursive) result.add(expression.run(meter));
            return List.copyOf(result);
        } finally { frame.values = List.of(); }
    }
}
