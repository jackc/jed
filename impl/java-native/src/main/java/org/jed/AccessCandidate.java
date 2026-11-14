package org.jed;

import java.io.IOException;
import java.util.ArrayList;
import java.util.List;

/** Policy-free resident-fact access inventory. The identity also fixes executor visitation order. */
record AccessCandidate(int kind, Engine.Index index, SqlParser.Expr predicate, long rows, long pages, long extra) {
    static final int PK = 0, BTREE = 1, GIST = 2, GIN = 3, PK_SET = 4, INDEX_SET = 5, FULL = 6;
    String name() { return index == null ? "" : index.name(); }
    boolean storageOrder() { return kind != BTREE && kind != INDEX_SET; }
    long scanCost() { return PlanEstimate.add(extra, PlanEstimate.add(pages, rows)); }
    static int compare(AccessCandidate a, AccessCandidate b) {
        int kind = Integer.compare(a.kind(), b.kind()); return kind != 0 ? kind : SqlText.compare(a.name(), b.name());
    }
    static List<AccessCandidate> inventory(Engine.Table table, List<String> names, String alias, SqlParser.Expr predicate,
                                         ScalarPlan.Resolver constants, CostMeter meter, boolean fullOnly) {
        try {
            List<AccessCandidate> result = new ArrayList<>();
            if (!fullOnly && predicate != null) {
                TableScan.PrimarySelection pk = TableScan.primary(table, names, alias, predicate, constants, meter);
                if (pk != null) result.add(new AccessCandidate(PK, null, pk.predicate(), pk.rows(), pk.pages(), 0));
                for (IndexAccess.Selection index : IndexAccess.inventory(table, names, alias, predicate, meter, constants, false))
                    result.add(new AccessCandidate(IndexAccess.interval(index.predicate()) ? INDEX_SET : BTREE, index.index(), index.predicate(), index.rows(), index.pages(), 0));
                for (Engine.Index index : table.indexes()) {
                    Engine.Table single = table.withIndexes(List.of(index));
                    if (index.kind() == 2) {
                        GistEstimate.Selection gist = GistEstimate.select(single, names, alias, predicate, constants, meter, Long.MAX_VALUE, true);
                        if (gist != null) result.add(new AccessCandidate(GIST, index, predicate, gist.rows(), gist.pages(), gist.entries()));
                    } else if (index.kind() == 1) {
                        GinEstimate.Selection gin = GinEstimate.select(single, names, alias, predicate, constants, meter, Long.MAX_VALUE, false, true);
                        if (gin != null) result.add(new AccessCandidate(GIN, index, gin.bound().predicate(), gin.rows(), gin.pages(), gin.entries()));
                    }
                }
                KeySetEstimate.Selection set = KeySetEstimate.primary(table, names, alias, predicate, constants, meter);
                if (set != null) result.add(new AccessCandidate(PK_SET, null, predicate, set.rows(), set.pages(), 0));
            }
            result.add(new AccessCandidate(FULL, null, null, table.rows().size(), table.rows().structuralFacts().nodeCount(), 0));
            result.sort(AccessCandidate::compare); return List.copyOf(result);
        } catch (IOException failure) { throw Engine.storageError(failure); }
    }
}
