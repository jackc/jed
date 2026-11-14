package org.jed;

import java.util.List;
import java.util.function.Supplier;

/** CTAS executes through the ordinary write path and reports its source's measured phases. */
final class AnalyzedCreatePlan implements QueryPlan {
    private final QueryPlan source;
    private final Supplier<QueryPlan> execute;
    private final CostMeter meter;
    private final boolean withData;
    private boolean complete;
    AnalyzedCreatePlan(QueryPlan source, Supplier<QueryPlan> execute, CostMeter meter, boolean withData) {
        this.source = source; this.execute = execute; this.meter = meter; this.withData = withData;
    }
    public List<String> names() { return List.of(); }
    public List<SqlType> types() { return List.of(); }
    public long affectedRows() { return meter.writeAffected; }
    public void requireAnalyze() { if (withData) source.requireAnalyze(); }
    public PlanNode explain(boolean verbose) { return source.explain(verbose); }
    public Row next(CostMeter ignored) {
        if (complete) return null;
        meter.writeMetrics = new PlanMetrics();
        try (QueryPlan result = execute.get()) { while (result.next(meter) != null) { } }
        complete = true; return null;
    }
    public PlanNode actualPlan(boolean verbose) {
        if (!withData) return PlanMetrics.zero(source.explain(verbose));
        if (meter.writeSourceActual == null) throw new IllegalStateException("CTAS source checkpoint missing");
        return meter.writeSourceActual;
    }
    public void close() { source.close(); }
}
