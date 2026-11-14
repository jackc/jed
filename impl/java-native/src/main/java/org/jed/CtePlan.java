package org.jed;

import java.util.List;
import java.util.function.Supplier;

/** A CTE definition owns its materialization; each reference owns only a cursor. */
final class CtePlan implements AutoCloseable {
    private final Supplier<QueryPlan> factory;
    private QueryPlan prepared;
    private QueryPlan metadata;
    private boolean referenced, executed;
    private final List<String> names;
    private final List<SqlType> types;
    private final List<CollationDerivation> collations;
    private final boolean materialized;
    private RowSpool rows;
    private boolean correlated, sideEffectsOnly;
    private SqlParser.Statement inlineBody;
    private QueryEnvironment inlineEnvironment;
    CtePlan inline(SqlParser.Statement body, QueryEnvironment environment) { if (!materialized) { inlineBody = body; inlineEnvironment = environment; } return this; }
    SqlParser.Statement inlineBody() { return inlineBody; }
    QueryEnvironment inlineEnvironment() { return inlineEnvironment; }
    CtePlan sideEffectsOnly(boolean value) { sideEffectsOnly = value; return this; }
    void eager(CostMeter meter) { materialize(meter); }
    CtePlan correlated(boolean value) { correlated = value; return this; }
    boolean correlated() { return correlated; }
    CtePlan(Supplier<QueryPlan> factory, List<String> aliases, boolean materialized) {
        this.factory = factory; this.materialized = materialized; prepared = factory.get(); metadata = prepared;
        types = prepared.types(); collations = prepared.collations();
        if (aliases.size() > types.size()) { prepared.close(); throw new SqlException("42P10", "too many CTE column aliases"); }
        java.util.ArrayList<String> names = new java.util.ArrayList<>(prepared.names());
        for (int c = 0; c < aliases.size(); c++) names.set(c, aliases.get(c));
        this.names = List.copyOf(names);
    }
    CtePlan(Supplier<QueryPlan> factory, List<String> names, List<SqlType> types) { this(factory, names, types, java.util.Collections.nCopies(types.size(), CollationDerivation.NONE)); }
    CtePlan(Supplier<QueryPlan> factory, List<String> names, List<SqlType> types, List<CollationDerivation> collations) {
        this.factory = factory; this.names = List.copyOf(names); this.types = List.copyOf(types); this.collations = List.copyOf(collations); materialized = false;
    }
    PlanNode definition(String name, boolean verbose) {
        PlanNode body = metadata.explain(verbose);
        boolean inlined = !materialized;
        return new PlanNode("CTE " + name, inlined ? "inlined" : "materialized", body.rows(), inlined ? 0 : body.cost(), inlined && inlineBody != null && referenced ? List.of() : List.of(body), "deferred");
    }
    void requireAnalyze() { if (materialized && referenced) metadata.requireAnalyze(); }
    PlanNode actualDefinition(String name, boolean verbose) {
        PlanNode node = definition(name, verbose);
        if (!executed || !materialized) return PlanMetrics.zero(node);
        PlanNode child = metadata.actualPlan(verbose);
        return new PlanNode(node.label(), node.detail(), node.rows(), child.cost(), List.of(child), node.lane());
    }
    QueryPlan reference() {
        referenced = true;
        if (sideEffectsOnly) throw new SqlException("0A000", "data-modifying CTE has no RETURNING clause");
        if (!materialized) {
            QueryPlan plan = prepared == null ? factory.get() : prepared; prepared = null;
            return new QueryPlan() {
                public List<String> names() { return names; }
                public List<SqlType> types() { return types; }
                public List<CollationDerivation> collations() { return collations; }
                public PlanNode explain(boolean verbose) { return plan.explain(verbose); }
                public void requireAnalyze() { plan.requireAnalyze(); }
                public PlanNode actualPlan(boolean verbose) { return plan.actualPlan(verbose); }
                public Row next(CostMeter meter) { Row row = plan.next(meter); return row == null ? null : new Row(names, row.values()); }
                public void close() { plan.close(); }
            };
        }
        return new QueryPlan() {
            RowSpool.Cursor cursor;
            long actualCost;
            public void requireAnalyze() { metadata.requireAnalyze(); }
            public PlanNode actualPlan(boolean verbose) { PlanNode node = explain(verbose); return new PlanNode(node.label(), node.detail(), node.rows(), actualCost, List.of(), node.lane()); }
            public PlanNode explain(boolean verbose) { PlanNode body = metadata.explain(verbose); return new PlanNode("CTE materialization", "", body.rows(), PlanEstimate.mul(body.rows(), Costs.CTE_SCAN_ROW), List.of(), "deferred"); }
            public List<String> names() { return names; }
            public List<SqlType> types() { return types; }
                public List<CollationDerivation> collations() { return collations; }
            public Row next(CostMeter meter) {
                if (cursor == null) {
                    materialize(meter); cursor = rows.cursor();
                }
                List<SqlValue> values = cursor.next();
                if (values == null) return null;
                meter.charge(Costs.CTE_SCAN_ROW); actualCost = PlanEstimate.add(actualCost, Costs.CTE_SCAN_ROW); meter.guard(); return new Row(names, values);
            }
            public void close() { if (cursor != null) cursor.close(); }
        };
    }
    private void materialize(CostMeter meter) {
        if (rows != null) return;
        executed = true; rows = new RowSpool(types, 0, null, meter.memory());
        try { Row row; while ((row = prepared.next(meter)) != null) rows.append(row.values()); }
        finally { prepared.close(); prepared = null; }
    }
    public void close() { if (prepared != null) prepared.close(); if (rows != null) rows.close(); }
}
