package org.jed;

import java.util.HashMap;
import java.util.List;
import java.util.Map;

/** Per-plan phase deltas. Equal labels in separate query frames never share counters. */
final class PlanMetrics {
    private final Map<String, Long> phases = new HashMap<>();
    private long total;
    void add(String phase, long before, CostMeter meter) {
        long delta = meter.cost() - before;
        phases.merge(phase, delta, PlanEstimate::add);
    }
    void total(long before, CostMeter meter) { total = PlanEstimate.add(total, meter.cost() - before); }
    void clear() { phases.clear(); total = 0; }
    long total() { return total; }
    long phase(String name) { return phases.getOrDefault(name, 0L); }
    void addUnits(String phase, long delta) { if (delta < 0) throw new IllegalArgumentException("negative phase delta"); phases.merge(phase, delta, PlanEstimate::add); }
    PlanNode render(PlanNode estimate) { return render(estimate, null); }
    PlanNode render(PlanNode estimate, PlanNode source) { return render(estimate, source, true); }
    private PlanNode render(PlanNode estimate, PlanNode source, boolean root) {
        if (estimate == source) return estimate;
        List<PlanNode> children = estimate.children().stream().map(child -> render(child, source, false)).toList();
        long cost = phases.getOrDefault(phaseName(estimate.label()), 0L);
        for (PlanNode child : children) cost = PlanEstimate.add(cost, child.cost());
        return new PlanNode(estimate.label(), estimate.detail(), estimate.rows(), root ? total : cost, children, estimate.lane());
    }
    static PlanNode zero(PlanNode node) { return new PlanNode(node.label(), node.detail(), node.rows(), 0, node.children().stream().map(PlanMetrics::zero).toList(), node.lane()); }
    private static String phaseName(String label) {
        if (label.startsWith("Scan ")) return "Scan";
        if (label.startsWith("Distinct")) return "Distinct";
        return label;
    }
}
