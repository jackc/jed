package org.jed;

import java.util.List;
import java.util.function.Supplier;

/** Execute a separately resolved mutation only after its metadata is fully validated. */
final class AnalyzedWritePlan implements QueryPlan {
    private final DmlExplanation metadata;
    private final Supplier<QueryPlan> execute;
    private final CostMeter meter;
    private QueryPlan result;
    private boolean complete;
    AnalyzedWritePlan(DmlExplanation metadata, Supplier<QueryPlan> execute, CostMeter meter) {
        this.metadata = metadata; this.execute = execute; this.meter = meter;
    }
    public List<String> names() { return metadata.names(); }
    public List<SqlType> types() { return metadata.types(); }
    public void requireAnalyze() { metadata.requireAnalyzedWrite(); }
    public PlanNode explain(boolean verbose) { return metadata.explain(verbose); }
    public long affectedRows() { return result == null ? 0 : result.affectedRows(); }
    public Row next(CostMeter ignored) {
        if (complete) return null;
        if (result == null) { meter.writeMetrics = new PlanMetrics(); result = execute.get(); }
        Row row = result.next(meter);
        if (row == null) { complete = true; meter.writeMetrics.total(0, meter); }
        return row;
    }
    public PlanNode actualPlan(boolean verbose) { return metadata.actualWrite(verbose, meter.writeMetrics, meter.writeSourceActual); }
    public void close() { if (result != null) result.close(); metadata.close(); }
}
