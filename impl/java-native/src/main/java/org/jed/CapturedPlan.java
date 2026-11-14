package org.jed;

import java.util.List;

/** Retained result whose execution already completed inside an atomic write statement. */
final class CapturedPlan implements QueryPlan {
    private final QueryPlan result;
    private final PlanNode estimate, actual;
    CapturedPlan(QueryPlan result, PlanNode estimate, PlanNode actual) {
        this.result = result; this.estimate = estimate; this.actual = actual;
    }
    public List<String> names() { return result.names(); }
    public List<SqlType> types() { return result.types(); }
    public List<CollationDerivation> collations() { return result.collations(); }
    public long affectedRows() { return result.affectedRows(); }
    public void requireAnalyze() {}
    public PlanNode explain(boolean verbose) { return estimate; }
    public PlanNode actualPlan(boolean verbose) { return actual; }
    public Row next(CostMeter meter) { return result.next(meter); }
    public void close() { result.close(); }
}
