package org.jed;

import java.util.List;

/** A computed relation's scan filter, after its own evaluation and before its parent join. */
final class FilteredRelation implements RelationPlan {
    private final RelationPlan source;
    private final QueryEnvironment environment;
    private final ScalarPlan.Frame frame = new ScalarPlan.Frame();
    private final ScalarPlan.Resolver resolver;
    private final PlanMetrics metrics = new PlanMetrics();
    private SqlParser.Expr syntax;
    private ScalarPlan.Expression predicate;
    private boolean started;
    FilteredRelation(RelationPlan source, QueryEnvironment environment) {
        this.source = source; this.environment = environment; resolver = environment.resolver(source.bindings(), frame);
    }
    public List<ScalarPlan.Binding> bindings() { return source.bindings(); }
    public List<String> names() { return source.names(); }
    public List<SqlType> types() { return source.types(); }
    public boolean projectedResult() { return source.projectedResult(); }
    public boolean orderedBy(List<SqlParser.Order> order) { return source.orderedBy(order); }
    public List<FunctionalKey> functionalKeys() { return source.functionalKeys(); }
    public void touch(boolean[] mask) {
        boolean[] touched = mask.clone(), filtered = resolver.touched();
        for (int i = 0; i < touched.length; i++) touched[i] |= filtered[i]; source.touch(touched);
    }
    public boolean push(SqlParser.Expr term) {
        if (started || !FilterRules.belongs(term, bindings()) || !FilterRules.safe(term, resolver)) return false;
        syntax = FilterRules.and(syntax, term); predicate = resolver.predicate(syntax); return true;
    }
    public Row next(CostMeter meter) {
        started = true; long before = meter.cost();
        try {
            Row row;
            while ((row = source.next(meter)) != null) {
                frame.values = row.values();
                if (predicate != null) { SqlValue value = predicate.run(meter); if (value.isNull() || !value.asBoolean()) continue; }
                return row;
            }
            return null;
        } finally { metrics.total(before, meter); }
    }
    public PlanNode explain(boolean verbose) { return describe(source.explain(verbose), verbose, false); }
    public void requireAnalyze() { source.requireAnalyze(); }
    public PlanNode actualPlan(boolean verbose) { return describe(source.actualPlan(verbose), verbose, true); }
    private PlanNode describe(PlanNode node, boolean verbose, boolean actual) {
        if (syntax == null) return node;
        String detail = verbose ? "filter=" + new ExplainExpression(bindings(), resolver, environment.meter()).print(TableExplanation.orderedPredicate(syntax, environment)) : "filter:conjuncts=" + FilterRules.terms(syntax).size();
        long rows = PlanEstimate.rows(syntax, node.rows(), List.of());
        return new PlanNode(node.label(), TableExplanation.append(node.detail(), detail), rows, actual ? metrics.total() : PlanEstimate.add(node.cost(), PlanEstimate.mul(node.rows(), PlanEstimate.operations(syntax))), node.children(), node.lane());
    }
    public void close() { source.close(); }
}
