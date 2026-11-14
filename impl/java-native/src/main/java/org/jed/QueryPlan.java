package org.jed;

import java.util.List;

/** A resolved, single-execution stream with stable metadata. */
interface QueryPlan extends AutoCloseable {
    List<String> names();
    List<SqlType> types();
    default List<CollationDerivation> collations() { return java.util.Collections.nCopies(types().size(), CollationDerivation.NONE); }
    Row next(CostMeter meter);
    default void collect(RowSpool target, CostMeter meter) { Row row; while ((row = next(meter)) != null) target.append(row.values()); }
    default void requireAnalyze() { throw new SqlException("0A000", "EXPLAIN ANALYZE checkpoints are not connected for " + getClass().getSimpleName()); }
    default PlanNode actualPlan(boolean verbose) { throw new SqlException("0A000", "EXPLAIN ANALYZE checkpoints are not connected for " + getClass().getSimpleName()); }
    default long affectedRows() { return -1; }
    static QueryPlan counted(QueryPlan source, long count) {
        return new QueryPlan() {
            public List<String> names() { return source.names(); }
            public List<SqlType> types() { return source.types(); }
            public List<CollationDerivation> collations() { return source.collations(); }
            public long affectedRows() { return count; }
            public Row next(CostMeter meter) { return source.next(meter); }
            public PlanNode explain(boolean verbose) { return source.explain(verbose); }
            public void requireAnalyze() { source.requireAnalyze(); }
            public PlanNode actualPlan(boolean verbose) { return source.actualPlan(verbose); }
            public void close() { source.close(); }
        };
    }
    default PlanNode explain(boolean verbose) { throw new SqlException("0A000", "EXPLAIN metadata is not connected for " + getClass().getSimpleName()); }
    @Override default void close() {}
}
