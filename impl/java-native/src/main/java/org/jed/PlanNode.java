package org.jed;

import java.util.List;

/** Immutable physical-plan metadata. Estimates never evaluate SQL or visit stored rows. */
record PlanNode(String label, String detail, long rows, long cost, List<PlanNode> children, String lane) {
    PlanNode { children = List.copyOf(children); if (rows < 0 || cost < 0) throw new IllegalArgumentException("negative estimate"); }
    PlanNode wrap(String label, String detail, long rows, long addedCost) {
        return new PlanNode(label, detail, rows, PlanEstimate.add(cost, addedCost), List.of(this), lane);
    }
    PlanNode finish(String detail, long extra, String lane) {
        return new PlanNode(label, detail, rows, PlanEstimate.add(cost, extra), children, lane);
    }
}
