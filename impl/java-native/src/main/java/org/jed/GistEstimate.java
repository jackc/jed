package org.jed;

import java.io.IOException;
import java.util.List;

/** GiST access candidates priced from resident tree facts. */
final class GistEstimate {
    record Selection(GistAccess.Bound bound, long rows, long pages, long entries, long cost) {}
    private GistEstimate() {}
    static Selection select(Engine.Table table, List<String> names, String alias, SqlParser.Expr predicate,
                            ScalarPlan.Resolver constants, CostMeter meter, long incumbent) throws IOException {
        return select(table, names, alias, predicate, constants, meter, incumbent, false);
    }
    static Selection select(Engine.Table table, List<String> names, String alias, SqlParser.Expr predicate,
                            ScalarPlan.Resolver constants, CostMeter meter, long incumbent, boolean allowTie) throws IOException {
        Selection selected = null;
        for (Engine.Index index : table.indexes().stream().filter(value -> value.kind() == 2).sorted((a, b) -> SqlText.compare(a.name(), b.name())).toList()) {
            GistAccess.Bound bound;
            try { bound = GistAccess.find(table.withIndexes(List.of(index)), names, alias, predicate, constants, meter); }
            catch (SqlException failure) { if (failure.sqlState().equals("42P02")) continue; throw failure; }
            if (bound == null) continue;
            // The frozen estimator admits the table skeleton for GiST descent pricing.
            var facts = table.rows().structuralFacts();
            var fraction = index.keys().getFirst().type().kind() == SqlType.Kind.RANGE ? EstimatorFacts.MATCHING : EstimatorFacts.EQUALITY;
            long rows = PlanEstimate.scale(table.rows().size(), fraction);
            long entries = rows == 0 ? 0 : Math.min(facts.nodeCount(), Math.max(facts.height(), PlanEstimate.scale(facts.nodeCount(), fraction)));
            long pages = PlanEstimate.add(entries, PlanEstimate.mul(rows, table.rows().structuralFacts().height()));
            long cost = PlanEstimate.add(entries, PlanEstimate.add(pages, PlanEstimate.mul(rows, 1 + PlanEstimate.operations(predicate, constants, meter))));
            if (cost < incumbent || selected == null && allowTie && cost == incumbent) { selected = new Selection(bound, rows, pages, entries, cost); incumbent = cost; }
        }
        return selected;
    }
}
