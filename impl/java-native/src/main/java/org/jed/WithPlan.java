package org.jed;

import java.util.ArrayList;
import java.util.List;

/** WITH defers execution, then owns the body result until rows transfer to its caller. */
final class WithPlan implements QueryPlan {
    private final SqlParser.With syntax;
    private final QueryEnvironment environment;
    private final QueryPlan body;
    WithPlan(SqlParser.With syntax, QueryEnvironment environment, QueryPlan body) {
        this.syntax = syntax; this.environment = environment; this.body = body;
    }
    public List<String> names() { return body.names(); }
    public List<SqlType> types() { return body.types(); }
    public List<CollationDerivation> collations() { return body.collations(); }
    private final PlanMetrics metrics = new PlanMetrics();
    public void requireAnalyze() { body.requireAnalyze(); for (var cte : syntax.ctes()) environment.cte(cte.name()).requireAnalyze(); }
    public PlanNode actualPlan(boolean verbose) {
        PlanNode node = explain(verbose); List<PlanNode> children = new ArrayList<>();
        for (var cte : syntax.ctes()) children.add(environment.cte(cte.name()).actualDefinition(cte.name(), verbose));
        children.add(body.actualPlan(verbose));
        return new PlanNode(node.label(), node.detail(), node.rows(), metrics.total(), children, node.lane());
    }
    private RowSpool output;
    private RowSpool.Cursor cursor;
    public Row next(CostMeter meter) {
        long before = meter.cost(); try { return nextMeasured(meter); } finally { metrics.total(before, meter); }
    }
    private Row nextMeasured(CostMeter meter) {
        if (output == null) {
            output = new RowSpool(types(), 0, null, meter.memory());
            Row row; while ((row = body.next(meter)) != null) output.append(row.values());
            body.close();
            for (SqlParser.Cte cte : syntax.ctes()) environment.cte(cte.name()).close();
            cursor = output.cursor();
        }
        List<SqlValue> values = cursor.next();
        if (values == null) return null;
        output.releaseRow(values);
        return new Row(names(), values);
    }
    public void close() { body.close(); if (cursor != null) cursor.close(); if (output != null) output.close(); for (SqlParser.Cte cte : syntax.ctes()) environment.cte(cte.name()).close(); }
    public PlanNode explain(boolean verbose) {
        PlanNode main = body.explain(verbose); List<PlanNode> children = new ArrayList<>(); long cost = main.cost();
        for (SqlParser.Cte cte : syntax.ctes()) {
            PlanNode definition = environment.cte(cte.name()).definition(cte.name(), verbose);
            children.add(definition); cost = PlanEstimate.add(cost, definition.cost());
        }
        children.add(main);
        return new PlanNode("WITH", "ctes=" + syntax.ctes().size(), main.rows(), cost, children, syntax.ctes().stream().anyMatch(cte -> mutation(cte.body())) ? "buffered" : "deferred");
    }
    private static boolean mutation(SqlParser.Statement statement) {
        return statement instanceof SqlParser.Insert || statement instanceof SqlParser.Update || statement instanceof SqlParser.Delete || statement instanceof SqlParser.Merge || statement instanceof SqlParser.With with && (mutation(with.body()) || with.ctes().stream().anyMatch(cte -> mutation(cte.body())));
    }
}
