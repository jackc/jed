package org.jed;

import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.nio.file.Path;

/** Exposes grouped expression slots to the following window stage without emitting a result row. */
final class GroupedWindow {
    record Prepared(SqlParser.Select query, QueryPlan source, List<ScalarPlan.Binding> bindings) {}
    private final List<SqlParser.Item> staged = new ArrayList<>();
    private final List<SqlParser.Expr> syntax = new ArrayList<>();
    private final Map<String, SqlParser.WindowSpec> definitions;
    private GroupedWindow(SqlParser.Select query) { definitions = WindowSyntax.definitions(query); }
    static Prepared prepare(SqlParser.Select query, QueryPlan source, List<ScalarPlan.Binding> bindings, SqlValue[] arguments, QueryEnvironment environment, Path scratch, long workMem) {
        GroupedWindow stage = new GroupedWindow(query);
        if (WindowSyntax.contains(query.where()) || WindowSyntax.contains(query.having()) || query.group().stream().anyMatch(WindowSyntax::contains)) throw new SqlException("42P20", "window function is not allowed here");
        List<SqlParser.Item> items = new ArrayList<>();
        for (SqlParser.Item item : query.items()) {
            if (item.expression() instanceof SqlParser.Star star) {
                for (ScalarPlan.Binding binding : bindings) if (star.qualifier().isEmpty() ? !binding.qualifiedOnly() : star.qualifier().equals(binding.qualifier())) {
                    List<String> parts = new ArrayList<>(binding.qualifier()); parts.add(binding.name());
                    items.add(new SqlParser.Item(stage.rewrite(new SqlParser.Name(parts)), binding.name()));
                }
            } else items.add(new SqlParser.Item(stage.rewrite(item.expression()), item.alias() == null ? ScalarPlan.outputName(item.expression()) : item.alias()));
        }
        List<SqlParser.Order> order = new ArrayList<>();
        for (SqlParser.Order key : query.order()) {
            SqlParser.Expr expression = key.expression();
            boolean alias = expression instanceof SqlParser.Name name && name.parts().size() == 1 && items.stream().anyMatch(item -> item.alias().equals(name.parts().getFirst()));
            boolean ordinal = expression instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.INTEGER;
            order.add(new SqlParser.Order(alias || ordinal ? expression : stage.rewrite(expression), key.descending(), key.nullsFirst()));
        }
        List<SqlParser.Expr> distinctOn = new ArrayList<>();
        for (SqlParser.Expr expression : query.distinctOn()) {
            boolean alias = expression instanceof SqlParser.Name name && name.parts().size() == 1 && items.stream().anyMatch(item -> item.alias().equals(name.parts().getFirst()));
            boolean ordinal = expression instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.INTEGER;
            distinctOn.add(alias || ordinal ? expression : stage.rewrite(expression));
        }
        // Validate all named window keys in the grouped context, even if no function references them.
        List<SqlParser.Item> validation = new ArrayList<>();
        for (SqlParser.WindowSpec named : stage.definitions.values()) {
            for (SqlParser.Expr key : named.partition()) validation.add(new SqlParser.Item(key, null));
            for (SqlParser.Order key : named.order()) validation.add(new SqlParser.Item(key.expression(), null));
        }
        if (!validation.isEmpty()) {
            RelationPlan shape = new RelationPlan() {
                public List<ScalarPlan.Binding> bindings() { return bindings; }
                public List<String> names() { return source.names(); }
                public List<SqlType> types() { return source.types(); }
                public List<FunctionalKey> functionalKeys() { return source instanceof RelationPlan relation ? relation.functionalKeys() : List.of(); }
                public void touch(boolean[] mask) { }
                public Row next(CostMeter meter) { throw new IllegalStateException("validation shape cannot execute"); }
                public void close() { }
            };
            SqlParser.Select checked = new SqlParser.Select(validation, null, List.of(), null, null, false, query.from(), query.group(), null, List.of());
            try (AggregatePlan ignored = new AggregatePlan(checked, shape, bindings, arguments, scratch, workMem, environment)) { }
        }
        SqlParser.Select grouped = new SqlParser.Select(stage.staged, query.where(), List.of(), null, null, false, query.from(), query.group(), query.having(), List.of());
        AggregatePlan plan = new AggregatePlan(grouped, source, bindings, arguments, scratch, workMem, environment).withoutEmissionCharge();
        List<ScalarPlan.Binding> output = new ArrayList<>();
        for (int i = 0; i < plan.names().size(); i++) output.add(new ScalarPlan.Binding(List.of(), plan.names().get(i), plan.types().get(i)));
        SqlParser.Select rewritten = new SqlParser.Select(items, null, order, query.limit(), query.offset(), query.distinct(), List.of(), List.of(), null, distinctOn);
        return new Prepared(rewritten, plan, output);
    }
    private SqlParser.Expr slot(SqlParser.Expr value) {
        if (WindowSyntax.contains(value)) throw new SqlException("42P20", "nested window function");
        int index = AggregatePlan.contains(value) ? -1 : syntax.indexOf(value);
        if (index < 0) { index = syntax.size(); syntax.add(value); staged.add(new SqlParser.Item(value, "$window_input" + index)); }
        return new SqlParser.ColumnSlot(index);
    }
    private SqlParser.Expr rewrite(SqlParser.Expr expression) {
        if (!WindowSyntax.contains(expression)) return slot(expression);
        return WindowSyntax.map(expression, value -> {
            if (value instanceof SqlParser.Window window) {
                SqlParser.WindowSpec spec = window.reference() == null ? WindowSyntax.merge(window.spec(), definitions) : definitions.get(window.reference());
                if (spec == null) throw new SqlException("42704", "window does not exist");
                SqlParser.Call call = window.function();
                SqlParser.Call transformed = new SqlParser.Call(call.name(), call.args().stream().map(this::slot).toList(), call.distinct(), call.star(), call.order(), call.filter() == null ? null : slot(call.filter()), call.withinGroup());
                SqlParser.WindowSpec mapped = new SqlParser.WindowSpec(null, spec.partition().stream().map(this::slot).toList(), spec.order().stream().map(o -> new SqlParser.Order(slot(o.expression()), o.descending(), o.nullsFirst())).toList(), spec.frame());
                return new SqlParser.Window(transformed, mapped, null);
            }
            if (!WindowSyntax.contains(value)) return slot(value);
            return value;
        });
    }
}
