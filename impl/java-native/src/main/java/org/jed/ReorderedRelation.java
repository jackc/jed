package org.jed;

import java.util.ArrayList;
import java.util.List;

/** Keep SQL column ordinals stable when a physical join visits the two inputs in reverse order. */
final class ReorderedRelation implements RelationPlan {
    private final RelationPlan source;
    private final List<ScalarPlan.Binding> bindings;
    private final int[] physical;
    private List<SqlParser.Expr> pushed;
    private Boolean ordered;
    private java.util.function.UnaryOperator<PlanNode> estimate = node -> node;
    ReorderedRelation orderedEstimate(boolean ordered, java.util.function.UnaryOperator<PlanNode> estimate) { this.ordered = ordered; this.estimate = estimate; return this; }
    ReorderedRelation(RelationPlan source, List<ScalarPlan.Binding> logical, int leftWidth, int rightWidth) {
        this.source = source; bindings = List.copyOf(logical); physical = new int[logical.size()];
        for (int i = 0; i < physical.length; i++) physical[i] = i < leftWidth ? rightWidth + i : i - leftWidth;
    }
    ReorderedRelation(RelationPlan source, List<ScalarPlan.Binding> logical, int[] physical, List<SqlParser.Expr> pushed) {
        this.source = source; this.bindings = List.copyOf(logical); this.physical = physical.clone(); this.pushed = List.copyOf(pushed);
    }
    public boolean baseIsland() { return source.baseIsland(); }
    public List<ScalarPlan.Binding> bindings() { return bindings; }
    public List<String> names() { return bindings.stream().map(ScalarPlan.Binding::name).toList(); }
    public List<SqlType> types() { return bindings.stream().map(ScalarPlan.Binding::type).toList(); }
    public Row next(CostMeter meter) {
        Row row = source.next(meter); if (row == null) return null;
        List<SqlValue> values = new ArrayList<>(physical.length); for (int index : physical) values.add(row.get(index));
        return new Row(names(), values);
    }
    public void touch(boolean[] mask) { boolean[] translated = new boolean[source.bindings().size()]; for (int i = 0; i < mask.length; i++) translated[physical[i]] |= mask[i]; source.touch(translated); }
    public void prepare(CostMeter meter) { source.prepare(meter); }
    public boolean push(SqlParser.Expr predicate) { return pushed == null ? source.push(predicate) : pushed.contains(predicate); }
    public boolean orderedBy(List<SqlParser.Order> order) { return ordered == null ? source.orderedBy(order) : ordered; }
    public List<FunctionalKey> functionalKeys() {
        int[] logical = new int[physical.length]; for (int i = 0; i < physical.length; i++) logical[physical[i]] = i;
        return source.functionalKeys().stream().map(key -> new FunctionalKey(key.determinant().stream().map(index -> logical[index]).toList(), key.dependent().stream().map(index -> logical[index]).toList())).toList();
    }
    private int[] logicalSlots() {
        int[] slots = new int[source.bindings().size()];
        for (int i = physical.length - 1; i >= 0; i--) slots[physical[i]] = i;
        return slots;
    }
    public PlanNode explain(boolean verbose) { PlanNode node = estimate.apply(source.explain(verbose)); return verbose ? PlanSlots.remap(node, logicalSlots()) : node; }
    public void requireAnalyze() { source.requireAnalyze(); }
    public PlanNode actualPlan(boolean verbose) { PlanNode node = source.actualPlan(verbose); return verbose ? PlanSlots.remap(node, logicalSlots()) : node; }
    public void close() { source.close(); }
}
