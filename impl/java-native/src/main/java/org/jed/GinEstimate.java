package org.jed;

import java.io.IOException;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

/** GIN candidate prices from resident structural facts and bounded term statistics. */
final class GinEstimate {
    record Terms(long rows, long entries, long totalEntries, boolean statistics) {}
    record Selection(GinAccess.Bound bound, long rows, long pages, long entries, long scanCost, long cost) {}
    private GinEstimate() {}
    static Selection select(Engine.Table table, List<String> names, String alias, SqlParser.Expr predicate, ScalarPlan.Resolver constants, CostMeter meter, long incumbent) throws IOException {
        return select(table, names, alias, predicate, constants, meter, incumbent, false);
    }
    static Selection select(Engine.Table table, List<String> names, String alias, SqlParser.Expr predicate, ScalarPlan.Resolver constants, CostMeter meter, long incumbent, boolean dynamic) throws IOException {
        return select(table, names, alias, predicate, constants, meter, incumbent, dynamic, false);
    }
    static Selection select(Engine.Table table, List<String> names, String alias, SqlParser.Expr predicate, ScalarPlan.Resolver constants, CostMeter meter, long incumbent, boolean dynamic, boolean allowTie) throws IOException {
        Selection best = null;
        for (Engine.Index index : table.indexes().stream().filter(value -> value.kind() == 1).sorted((a,b) -> SqlText.compare(a.name(), b.name())).toList()) {
            GinAccess.Bound bound;
            try { bound = GinAccess.find(table.withIndexes(List.of(index)), names, alias, predicate, constants, meter, !dynamic); }
            catch (SqlException unknown) { if (unknown.sqlState().equals("42P02")) continue; throw unknown; }
            if (bound == null) continue;
            boolean known = knownOperand(bound.predicate(), names, alias);
            long n = table.rows().size(); long fallback = PlanEstimate.scale(n, EstimatorFacts.MATCHING); Terms terms = known ? terms(table, bound) : new Terms(fallback, fallback, 0, false); var facts = index.entries().structuralFacts();
            long pages = terms.rows() == 0 ? 0 : Math.min(facts.nodeCount(), Math.max(facts.height(), terms.statistics() && terms.totalEntries() > 0 ? StatisticsEstimate.scale(facts.nodeCount(), terms.entries(), terms.totalEntries()) : PlanEstimate.scale(facts.nodeCount(), EstimatorFacts.MATCHING)));
            pages = PlanEstimate.add(pages, PlanEstimate.mul(terms.rows(), table.rows().height()));
            long scan = PlanEstimate.add(pages, PlanEstimate.add(terms.entries(), terms.rows()));
            long cost = PlanEstimate.add(scan, PlanEstimate.mul(terms.rows(), PlanEstimate.operations(predicate, constants, meter)));
            if (cost < incumbent || best == null && allowTie && cost == incumbent) { best = new Selection(bound, terms.rows(), pages, terms.entries(), scan, cost); incumbent = cost; }
        }
        return best;
    }
    static Long logical(Engine.Table table, List<String> names, String alias, SqlParser.Expr leaf, ScalarPlan.Resolver constants, CostMeter meter) {
        if (!knownOperand(leaf, names, alias) || leaf instanceof SqlParser.In || leaf instanceof SqlParser.Binary binary && binary.op().equals("=")) return null;
        GinAccess.Bound bound;
        try { bound = GinAccess.find(table, names, alias, leaf, constants, meter); }
        catch (SqlException unknown) { if (unknown.sqlState().equals("42P02")) return null; throw unknown; }
        if (bound == null) return null; Terms terms = terms(table, bound); return terms.statistics() ? terms.rows() : null;
    }
    static boolean knownOperand(SqlParser.Expr expression, List<String> names, String alias) {
        if (expression instanceof SqlParser.Binary binary && (binary.op().equals("and") || binary.op().equals("or"))) return knownOperand(binary.left(), names, alias) && knownOperand(binary.right(), names, alias);
        if (expression instanceof SqlParser.Binary binary) return column(binary.left(), names, alias) && StatisticsEstimate.literalSyntax(binary.right()) || column(binary.right(), names, alias) && StatisticsEstimate.literalSyntax(binary.left());
        if (expression instanceof SqlParser.In in && in.query() == null && !in.negated()) return column(in.subject(), names, alias) && in.values().stream().allMatch(StatisticsEstimate::literalSyntax);
        if (expression instanceof SqlParser.Quantified q) return column(q.array(), names, alias) && StatisticsEstimate.literalSyntax(q.subject());
        return false;
    }
    private static boolean column(SqlParser.Expr expression, List<String> names, String alias) { return expression instanceof SqlParser.Name name && names.contains(name.parts().getLast()) && (name.parts().size() == 1 || name.parts().size() == 2 && name.parts().getFirst().equals(alias)); }
    private static Terms terms(Engine.Table table, GinAccess.Bound bound) {
        long n = table.rows().size();
        if (bound.fullScan()) { long rows = PlanEstimate.scale(n, EstimatorFacts.MATCHING); return new Terms(rows, rows, 0, false); }
        List<Long> counts = new ArrayList<>(); long total = 0, nonnull = n; boolean statistics = true;
        for (byte[] encoded : bound.terms()) {
            int key = bound.index().keys().size() == 1 ? 0 : encoded[0] & 255; byte[] term = key == 0 && bound.index().keys().size() == 1 ? encoded : Arrays.copyOfRange(encoded, 1, encoded.length);
            ColumnStatistics fact = table.statistics().get(bound.index().keys().get(key).column());
            if (fact == null || fact.terms() == null || fact.analyzedRows() == 0) { statistics = false; break; }
            nonnull = Math.min(nonnull, n - StatisticsEstimate.scale(n, fact.nullCount(), fact.analyzedRows()));
            total = Math.max(total, StatisticsEstimate.scale(n, fact.terms().occurrences(), fact.analyzedRows()));
            Long listed = null; for (ColumnStatistics.Term entry : fact.terms().entries()) if (Arrays.equals(term, entry.bytes())) { listed = entry.rows(); break; }
            long rows = listed != null ? Math.min(nonnull, StatisticsEstimate.scale(n, listed, fact.analyzedRows())) : !fact.stale() && fact.terms().untrackedRows() == 0 ? 0 : Math.max(1, Math.min(nonnull, StatisticsEstimate.scale(n, (fact.terms().untrackedRows() + 1) / 2, fact.analyzedRows())));
            counts.add(rows);
        }
        if (!statistics) { long rows = PlanEstimate.scale(n, EstimatorFacts.MATCHING); return new Terms(rows, rows, 0, false); }
        counts.sort(Long::compare); long rows = 0, entries = 0; boolean initialized = false;
        for (long count : counts) {
            if (!initialized || bound.any()) { rows = Math.min(nonnull, PlanEstimate.add(rows, count)); entries = PlanEstimate.add(entries, count); }
            else { entries = PlanEstimate.add(entries, rows); rows = n == 0 ? 0 : StatisticsEstimate.scale(rows, count, n); }
            initialized = true;
        }
        for (GinAccess.Bound arm : bound.arms()) {
            Terms child = terms(table, arm); statistics &= child.statistics(); total = Math.max(total, child.totalEntries()); entries = PlanEstimate.add(entries, child.entries());
            rows = !initialized || bound.any() ? Math.min(nonnull, PlanEstimate.add(rows, child.rows())) : n == 0 ? 0 : StatisticsEstimate.scale(rows, child.rows(), n); initialized = true;
        }
        return new Terms(rows, entries, total, statistics);
    }
}
