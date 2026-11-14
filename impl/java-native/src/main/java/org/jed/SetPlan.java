package org.jed;

import java.nio.ByteBuffer;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;

/** Set combination retains operand order and adds no projection or production charge. */
final class SetPlan implements QueryPlan {
    private final QueryPlan left, right;
    private final String operation;
    private final boolean all;
    private final List<SqlType> types = new ArrayList<>();
    private final List<CollationDerivation> collations = new ArrayList<>();
    private final Path scratch;
    private final long workMem;
    private final PlanMetrics metrics = new PlanMetrics();
    public void requireAnalyze() { left.requireAnalyze(); right.requireAnalyze(); }
    public PlanNode actualPlan(boolean verbose) { PlanNode node = explain(verbose); return new PlanNode(node.label(), node.detail(), node.rows(), metrics.total(), List.of(left.actualPlan(verbose), right.actualPlan(verbose)), node.lane()); }
    private RowSpool output;
    private RowSpool.Cursor cursor;
    SetPlan(QueryPlan left, QueryPlan right, String operation, boolean all, Path scratch, long workMem) {
        this.left = left; this.right = right; this.operation = operation; this.all = all; this.scratch = scratch; this.workMem = workMem;
        if (left.types().size() != right.types().size()) throw new SqlException("42601", "set operands have different column counts");
        for (int c = 0; c < left.types().size(); c++) {
            CollationDerivation collation = CollationDerivation.merge(left.collations().get(c), right.collations().get(c));
            if ((!all || !operation.equals("union")) && collation.conflict()) throw new SqlException("42P21", "set operands have different collations");
            collations.add(collation);
            try { types.add(ScalarPlan.common(left.types().get(c), right.types().get(c))); }
            catch (SqlException error) { throw SqlException.typeMismatch("incompatible set operand types"); }
            if (!all || !operation.equals("union")) ScalarPlan.requireComparable(types.getLast());
        }
    }
    @Override public PlanNode explain(boolean verbose) {
        PlanNode a = left.explain(verbose), b = right.explain(verbose); long rows = PlanEstimate.add(a.rows(), b.rows());
        if (!all) rows = PlanEstimate.groups(rows, types.size());
        if (operation.equals("intersect")) rows = Math.min(a.rows(), b.rows());
        if (operation.equals("except")) rows = PlanEstimate.scale(a.rows(), EstimatorFacts.OPAQUE);
        return new PlanNode(Character.toUpperCase(operation.charAt(0)) + operation.substring(1), all ? "all" : "distinct", rows, PlanEstimate.add(a.cost(), b.cost()), List.of(a, b), "deferred");
    }
    @Override public List<CollationDerivation> collations() { return List.copyOf(collations); }
    @Override public List<String> names() { return left.names(); }
    @Override public List<SqlType> types() { return List.copyOf(types); }
    private List<SqlValue> convert(Row row, CostMeter meter) {
        List<SqlValue> values = new ArrayList<>();
        for (int c = 0; c < types.size(); c++) {
            SqlValue value = SqlCasts.cast(row.get(c), types.get(c)); values.add(value);
            if (!(operation.equals("union") && all)) meter.charge(MemoryAccount.payload(value) * Costs.HASH_KEY);
        }
        meter.guard(); return values;
    }
    private void initialize(CostMeter meter) {
        output = new RowSpool(types, 0, null, meter.memory());
        try (RowSpool lhs = new RowSpool(types, 0, null, meter.memory()); RowSpool rhs = new RowSpool(types, 0, null, meter.memory());
             DistinctState emitted = new DistinctState(null, 0, meter.memory());
             SpillKeyState counts = new SpillKeyState(null, 0, meter.memory())) {
            Row row;
            while ((row = left.next(meter)) != null) lhs.append(convert(row, meter));
            left.close();
            while ((row = right.next(meter)) != null) rhs.append(convert(row, meter));
            right.close();
            if (operation.equals("union")) {
                for (RowSpool operand : List.of(lhs, rhs)) try (RowSpool.Cursor rows = operand.cursor()) {
                    List<SqlValue> values;
                    while ((values = rows.next()) != null) if (all || emitted.add(values)) operand.transferTo(values, output);
                }
            } else {
                try (RowSpool.Cursor rows = rhs.cursor()) {
                    List<SqlValue> values;
                    while ((values = rows.next()) != null) {
                        byte[] key = DistinctState.key(values); SpillKeyState.Value existing = counts.get(key);
                        long count = existing == null ? 0 : ByteBuffer.wrap(existing.bytes()).getLong();
                        counts.put(key, ByteBuffer.allocate(8).putLong(count + 1).array(), DistinctState.entryBytes(values));
                    }
                }
                try (RowSpool.Cursor rows = lhs.cursor()) {
                    List<SqlValue> values;
                    while ((values = rows.next()) != null) {
                        byte[] key = DistinctState.key(values); SpillKeyState.Value existing = counts.get(key);
                        long count = existing == null ? 0 : ByteBuffer.wrap(existing.bytes()).getLong();
                        boolean keep = operation.equals("intersect") ? count > 0 : count == 0;
                        if (all && count > 0) counts.put(key, ByteBuffer.allocate(8).putLong(count - 1).array(), DistinctState.entryBytes(values));
                        if (keep && (all || emitted.add(values))) lhs.transferTo(values, output);
                    }
                }
            }
        }
        cursor = output.cursor();
    }
    @Override public Row next(CostMeter meter) { long before = meter.cost(); try { if (cursor == null) initialize(meter); List<SqlValue> row = cursor.next(); return row == null ? null : new Row(names(), row); } finally { metrics.total(before, meter); } }
    @Override public void close() { left.close(); right.close(); if (cursor != null) cursor.close(); if (output != null) output.close(); }
}
