package org.jed;

import java.util.ArrayList;
import java.util.List;

/** Render the selected plan without running its expressions or opening its row stream. */
final class ExplainPlan implements QueryPlan {
    private final QueryPlan inner;
    private final List<String> names = new ArrayList<>(List.of("depth", "node", "detail"));
    private final List<SqlType> types = new ArrayList<>(List.of(SqlType.I32, SqlType.TEXT, SqlType.TEXT));
    private final List<List<SqlValue>> rows = new ArrayList<>();
    private int next;
    private final MemoryAccount memory;
    private final CostMeter outcome;
    private long reserved;
    ExplainPlan(SqlParser.Explain syntax, QueryPlan inner, CostMeter meter) { this(syntax, inner, meter, null); }
    ExplainPlan(SqlParser.Explain syntax, QueryPlan inner, CostMeter meter, CostMeter execution) {
        this(syntax, inner, meter, execution, false);
    }
    ExplainPlan(SqlParser.Explain syntax, QueryPlan inner, CostMeter meter, CostMeter execution, boolean skipped) {
        this.inner = inner; this.memory = meter.memory(); this.outcome = meter;
        if (syntax.costs()) { names.add("est_rows"); names.add("est_cost"); types.add(SqlType.I64); types.add(SqlType.I64); }
        if (syntax.analyze()) { names.add("actual_cost"); types.add(SqlType.I64); }
        if (syntax.lane()) { names.add("lane"); types.add(SqlType.TEXT); }
        if (skipped) return;
        try {
            PlanNode root = inner.explain(syntax.verbose());
            if (syntax.analyze()) {
                inner.requireAnalyze();
                if (execution == null) throw new IllegalStateException("analyzed plan requires separate execution meter");
                long count = 0;
                while (inner.next(execution) != null) count++;
                if (inner.types().isEmpty()) count = Math.max(0, inner.affectedRows());
                PlanNode actual = inner.actualPlan(syntax.verbose());
                PlanNode wrapper = new PlanNode("Analyze", "cost=" + execution.cost() + " rows=" + count, root.rows(), root.cost(), List.of(root), root.lane());
                PlanNode measured = new PlanNode("Analyze", wrapper.detail(), count, execution.cost(), List.of(actual), root.lane());
                emit(wrapper, measured, 0, root.lane(), syntax);
            } else emit(root, null, 0, root.lane(), syntax);
        } catch (RuntimeException | Error failure) { memory.release(reserved); reserved = 0; throw failure; }
    }
    private void emit(PlanNode node, PlanNode actual, int depth, String lane, SqlParser.Explain syntax) {
        List<SqlValue> row = new ArrayList<>(List.of(SqlValue.ofInteger(SqlType.I32, depth), SqlValue.ofText(node.label()), SqlValue.ofText(node.detail().isEmpty() ? "-" : node.detail())));
        if (syntax.costs()) { row.add(SqlValue.ofInteger(SqlType.I64, node.rows())); row.add(SqlValue.ofInteger(SqlType.I64, node.cost())); }
        if (syntax.analyze()) row.add(SqlValue.ofInteger(SqlType.I64, actual.cost()));
        if (syntax.lane()) row.add(SqlValue.ofText(lane)); long bytes = MemoryAccount.row(row); outcome.charge(Costs.ROW_PRODUCED); memory.reserve(bytes); reserved += bytes; rows.add(List.copyOf(row));
        for (int i = 0; i < node.children().size(); i++) emit(node.children().get(i), actual == null ? null : actual.children().get(i), depth + 1, lane, syntax);
    }
    public List<String> names() { return List.copyOf(names); }
    public List<SqlType> types() { return List.copyOf(types); }
    public Row next(CostMeter meter) {
        if (next >= rows.size()) return null;
        return new Row(names, rows.get(next++));
    }
    public void close() { inner.close(); memory.release(reserved); reserved = 0; rows.clear(); }
}
