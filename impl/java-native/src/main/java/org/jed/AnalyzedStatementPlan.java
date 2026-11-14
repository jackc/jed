package org.jed;

import java.util.List;
import java.util.function.Supplier;

/** A writable WITH executes once through the ordinary atomic statement boundary. */
final class AnalyzedStatementPlan implements QueryPlan {
    private final QueryPlan metadata;
    private final Supplier<QueryPlan> execute;
    private final CostMeter meter;
    private QueryPlan result;
    AnalyzedStatementPlan(QueryPlan metadata, Supplier<QueryPlan> execute, CostMeter meter) {
        this.metadata = metadata; this.execute = execute; this.meter = meter;
    }
    public List<String> names() { return metadata.names(); }
    public List<SqlType> types() { return metadata.types(); }
    public void requireAnalyze() { metadata.requireAnalyze(); }
    public PlanNode explain(boolean verbose) { return metadata.explain(verbose); }
    public long affectedRows() { return result == null ? 0 : result.affectedRows(); }
    public Row next(CostMeter ignored) {
        if (result == null) { meter.writeMetrics = new PlanMetrics(); result = execute.get(); }
        return result.next(meter);
    }
    public PlanNode actualPlan(boolean verbose) { return result.actualPlan(verbose); }
    public void close() { if (result != null) result.close(); metadata.close(); }
}
