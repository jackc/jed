package org.jed;

import java.util.ArrayList;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Map;

/** Metadata-only cost of the stages above a candidate FROM tree. No row stream is opened. */
final class SelectPipelineEstimate {
    private SelectPipelineEstimate() {}
    static long cost(SqlParser.Select query, List<ScalarPlan.Binding> logical, QueryEnvironment environment,
                     long inputCost, long physicalRows, long logicalRows, boolean ordered) {
        return cost(query, logical, environment, inputCost, physicalRows, logicalRows, ordered, key -> null, (expression, rows) -> PlanEstimate.rows(expression, rows, List.of()));
    }
    static long cost(SqlParser.Select query, List<ScalarPlan.Binding> logical, QueryEnvironment environment,
                     long inputCost, long physicalRows, long logicalRows, boolean ordered,
                     java.util.function.Function<SqlParser.Expr, Long> distinctBuckets, java.util.function.BiFunction<SqlParser.Expr, Long, Long> selectivity) {
        ScalarPlan.Resolver resolver = environment.resolver(logical, new ScalarPlan.Frame());
        SqlParser.Expr predicate = TableExplanation.orderedPredicate(query.where(), environment);
        long rows = Math.min(physicalRows, selectivity.apply(predicate, logicalRows));
        long cost = add(inputCost, work(predicate, physicalRows, resolver, environment));
        List<SqlParser.Expr> output = new ArrayList<>(); List<String> labels = new ArrayList<>();
        for (SqlParser.Item item : query.items()) {
            if (item.expression() instanceof SqlParser.Star star) {
                for (int i = 0; i < logical.size(); i++) { ScalarPlan.Binding binding = logical.get(i);
                    if (star.qualifier().isEmpty() ? !binding.qualifiedOnly() : star.qualifier().equals(binding.qualifier())) { output.add(new SqlParser.ColumnSlot(i)); labels.add(binding.name()); }
                }
            } else { output.add(item.expression()); labels.add(item.alias() == null ? ScalarPlan.outputName(item.expression()) : item.alias()); }
        }
        List<SqlParser.Expr> order = query.order().stream().map(key -> alias(key.expression(), output, labels, resolver, false)).toList();
        List<SqlParser.Expr> distinctOn = query.distinctOn().stream().map(key -> alias(key, output, labels, resolver, false)).toList();
        LinkedHashSet<SqlParser.Call> aggregates = new LinkedHashSet<>();
        LinkedHashSet<SqlParser.Window> windows = new LinkedHashSet<>();
        for (SqlParser.Expr expression : output) collect(expression, aggregates, windows);
        for (SqlParser.Expr expression : order) collect(expression, aggregates, windows);
        for (SqlParser.Expr expression : distinctOn) collect(expression, aggregates, windows);
        collect(query.having(), aggregates, windows);
        Map<String, SqlParser.WindowSpec> definitions = WindowSyntax.definitions(query);
        LinkedHashSet<SqlParser.WindowSpec> windowGroups = new LinkedHashSet<>();
        for (SqlParser.Window window : windows) {
            SqlParser.WindowSpec spec = window.reference() == null ? WindowSyntax.merge(window.spec(), definitions) : definitions.get(window.reference());
            if (spec == null) throw new SqlException("42704", "unknown window: " + window.reference());
            windowGroups.add(spec);
            for (SqlParser.Expr key : spec.partition()) collect(key, aggregates, new LinkedHashSet<>());
            for (SqlParser.Order key : spec.order()) collect(key.expression(), aggregates, new LinkedHashSet<>());
        }
        boolean grouped = !query.group().isEmpty() || !aggregates.isEmpty() || query.having() != null;
        if (grouped) {
            List<List<SqlParser.Expr>> sets = List.of(List.of());
            for (SqlParser.Expr term : query.group()) sets = AggregatePlan.cross(sets, AggregatePlan.expand(term));
            LinkedHashSet<SqlParser.Expr> keys = new LinkedHashSet<>();
            for (List<SqlParser.Expr> set : sets) for (SqlParser.Expr key : set) keys.add(alias(key, output, labels, resolver, true));
            long groups = keys.isEmpty() ? 1 : groups(new ArrayList<>(keys), rows, distinctBuckets);
            if (sets.size() > 1) groups = PlanEstimate.mul(groups, sets.size());
            cost = add(cost, PlanEstimate.mul(rows, aggregates.size() * Costs.AGGREGATE_ACCUMULATE));
            for (SqlParser.Expr key : keys) cost = add(cost, work(key, rows, resolver, environment));
            for (SqlParser.Call call : aggregates) {
                cost = add(cost, work(call.filter(), rows, resolver, environment));
                for (SqlParser.Expr argument : call.args()) cost = add(cost, work(argument, call.withinGroup() && call.name().startsWith("percentile_") ? groups : rows, resolver, environment));
                for (SqlParser.Order key : call.order()) cost = add(cost, work(key.expression(), rows, resolver, environment));
            }
            cost = add(cost, stageWork(query.having(), groups, resolver, environment));
            rows = PlanEstimate.rows(query.having(), groups, List.of());
        }
        if (!windows.isEmpty()) {
            cost = add(cost, PlanEstimate.mul(rows, windows.size() * Costs.WINDOW_RESULT));
            for (SqlParser.Window window : windows) {
                cost = add(cost, stageWork(window.function().filter(), rows, resolver, environment));
                for (SqlParser.Expr argument : window.function().args()) cost = add(cost, stageWork(argument, rows, resolver, environment));
            }
            for (SqlParser.WindowSpec spec : windowGroups) {
                for (SqlParser.Expr key : spec.partition()) cost = add(cost, stageWork(key, rows, resolver, environment));
                for (SqlParser.Order key : spec.order()) cost = add(cost, stageWork(key.expression(), rows, resolver, environment));
            }
        }
        boolean distinct = query.distinct() && query.distinctOn().isEmpty();
        if (distinct) {
            for (SqlParser.Expr expression : output) cost = add(cost, stageWork(expression, rows, resolver, environment));
            rows = groups(output, rows, grouped ? key -> null : distinctBuckets);
        }
        if (!ordered) for (SqlParser.Expr key : order) cost = add(cost, stageWork(key, rows, resolver, environment));
        // DISTINCT ON decorates its explicit keys even when no ORDER BY was written.
        if (!distinctOn.isEmpty()) {
            for (SqlParser.Expr key : distinctOn) if (!order.contains(key)) cost = add(cost, stageWork(key, rows, resolver, environment));
            rows = groups(distinctOn, rows, grouped ? key -> null : distinctBuckets);
        }
        long take = TableExplanation.boundEstimate(query.limit(), Long.MAX_VALUE, resolver, environment.meter());
        long skip = TableExplanation.boundEstimate(query.offset(), 0, resolver, environment.meter());
        rows = Math.min(take, Math.max(0, rows - skip));
        if (!distinct) for (SqlParser.Expr expression : output) cost = add(cost, stageWork(expression, rows, resolver, environment));
        return add(cost, PlanEstimate.mul(rows, Costs.ROW_PRODUCED));
    }
    private static long groups(List<SqlParser.Expr> keys, long rows, java.util.function.Function<SqlParser.Expr, Long> facts) {
        long result = 1;
        for (SqlParser.Expr key : keys) {
            if (!(key instanceof SqlParser.Name || key instanceof SqlParser.ColumnSlot)) return PlanEstimate.groups(rows, keys.size());
            Long count = facts.apply(key);
            if (count == null) return PlanEstimate.groups(rows, keys.size());
            result = PlanEstimate.mul(result, count);
        }
        return Math.min(rows, result);
    }
    private static SqlParser.Expr alias(SqlParser.Expr key, List<SqlParser.Expr> output, List<String> labels, ScalarPlan.Resolver resolver, boolean group) {
        if (key instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.INTEGER) {
            int n = ScalarPlan.ordinal(literal.text()); if (n > 0 && n <= output.size()) return output.get(n - 1);
        }
        if (key instanceof SqlParser.Name name && name.parts().size() == 1 && (!group || !resolver.hasColumn(name))) {
            int at = labels.indexOf(name.parts().getFirst()); if (at >= 0) return output.get(at);
        }
        return key;
    }
    private static SqlParser.Expr staged(SqlParser.Expr expression) {
        return WindowSyntax.map(expression, node -> node instanceof SqlParser.Window || node instanceof SqlParser.Call call && AggregatePlan.aggregateName(call.name()) ? new SqlParser.ColumnSlot(0) : node);
    }
    private static void collect(SqlParser.Expr expression, LinkedHashSet<SqlParser.Call> aggregates, LinkedHashSet<SqlParser.Window> windows) {
        if (expression == null) return;
        if (expression instanceof SqlParser.Window window) { windows.add(window); collect(window.function().filter(), aggregates, windows); for (SqlParser.Expr argument : window.function().args()) collect(argument, aggregates, windows); return; }
        if (expression instanceof SqlParser.Call call && AggregatePlan.aggregateName(call.name())) { aggregates.add(call); return; }
        for (SqlParser.Expr child : DefaultExpression.children(expression)) collect(child, aggregates, windows);
    }
    private static long stageWork(SqlParser.Expr expression, long rows, ScalarPlan.Resolver resolver, QueryEnvironment environment) {
        return add(PlanEstimate.mul(rows, PlanEstimate.operations(staged(expression), resolver, environment.meter())), environment.hiddenStageEstimate(expression, rows));
    }
    private static long work(SqlParser.Expr expression, long rows, ScalarPlan.Resolver resolver, QueryEnvironment environment) {
        return add(PlanEstimate.mul(rows, PlanEstimate.operations(expression, resolver, environment.meter())), environment.hiddenEstimate(expression, rows));
    }
    private static long add(long a, long b) { return PlanEstimate.add(a, b); }
}
