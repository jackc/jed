package org.jed;

import java.io.IOException;
import java.util.ArrayList;
import java.util.List;

/** Metadata for the resolved single-table pull pipeline. */
final class TableExplanation {
    private TableExplanation() {}
    static PlanNode describe(SqlParser.Select query, Engine.Table table, List<String> names, String alias,
                             List<SqlParser.Expr> projections, List<ScalarPlan.Binding> bindings,
                             ScalarPlan.Resolver resolver, CostMeter meter, boolean sort, boolean distinct, boolean reverse, boolean verbose) {
        return describe(query, table, names, alias, projections, bindings, resolver, meter, sort, distinct, reverse, verbose, true);
    }
    static PlanNode describe(SqlParser.Select query, Engine.Table table, List<String> names, String alias, List<SqlParser.Expr> projections, List<ScalarPlan.Binding> bindings, ScalarPlan.Resolver resolver, CostMeter meter, boolean sort, boolean distinct, boolean reverse, boolean verbose, boolean emit) {
        return describe(query, table, names, alias, projections, bindings, resolver, meter, sort, distinct, reverse, verbose, emit, true);
    }
    static PlanNode describe(SqlParser.Select query, Engine.Table table, List<String> names, String alias, List<SqlParser.Expr> projections, List<ScalarPlan.Binding> bindings, ScalarPlan.Resolver resolver, CostMeter meter, boolean sort, boolean distinct, boolean reverse, boolean verbose, boolean emit, boolean allowPartialFull) {
        return describe(query, table, names, alias, projections, bindings, resolver, meter, sort, distinct, reverse, verbose, emit, allowPartialFull, null);
    }
    static PlanNode describe(SqlParser.Select query, Engine.Table table, List<String> names, String alias, List<SqlParser.Expr> projections, List<ScalarPlan.Binding> bindings, ScalarPlan.Resolver resolver, CostMeter meter, boolean sort, boolean distinct, boolean reverse, boolean verbose, boolean emit, boolean allowPartialFull, DistinctOn distinctOn) {
        return describe(query, table, names, alias, projections, bindings, resolver, meter, sort, distinct, reverse, verbose, emit, allowPartialFull, distinctOn, null);
    }
    static PlanNode describe(SqlParser.Select query, Engine.Table table, List<String> names, String alias, List<SqlParser.Expr> projections, List<ScalarPlan.Binding> bindings, ScalarPlan.Resolver resolver, CostMeter meter, boolean sort, boolean distinct, boolean reverse, boolean verbose, boolean emit, boolean allowPartialFull, DistinctOn distinctOn, Engine.Index orderedIndex) {
        return describe(query, table, names, alias, projections, bindings, resolver, meter, sort, distinct, reverse, verbose, emit, allowPartialFull, distinctOn, orderedIndex, false);
    }
    static PlanNode describe(SqlParser.Select query, Engine.Table table, List<String> names, String alias, List<SqlParser.Expr> projections, List<ScalarPlan.Binding> bindings, ScalarPlan.Resolver resolver, CostMeter meter, boolean sort, boolean distinct, boolean reverse, boolean verbose, boolean emit, boolean allowPartialFull, DistinctOn distinctOn, Engine.Index orderedIndex, boolean forceFull) {
        try {
            ExplainExpression printer = new ExplainExpression(bindings, resolver, meter);
            StatisticsEstimate statistics = new StatisticsEstimate(table, names, alias, resolver, meter);
            long n = table.rows().size(); var facts = table.rows().structuralFacts();
            List<String> unique = table.primaryKey().length == 1 ? List.of(names.get(table.primaryKey()[0])) : List.of();
            TableScan.PrimarySelection primary = forceFull ? null : TableScan.primary(table, names, alias, query.where(), resolver, meter);
            IndexAccess.Selection index = forceFull ? null : IndexAccess.select(table, names, alias, query.where(), meter, null, resolver, allowPartialFull);
            String detail = "Full scan"; long scanRows = n, pages = facts.nodeCount(), extra = 0;
            long incumbent = PlanEstimate.add(pages, PlanEstimate.mul(n, 1 + PlanEstimate.operations(query.where(), resolver, meter)));
            boolean bounded = false; int rank = 6;
            if (primary != null && primary.cost() <= incumbent) {
                List<String> parts = new ArrayList<>();
                for (SqlParser.Expr term : FilterRules.terms(primary.predicate())) {
                    SqlParser.Binary binary = (SqlParser.Binary)term;
                    String key = ((SqlParser.Name)binary.left()).parts().getLast();
                    List<SqlParser.Binary> authored = new ArrayList<>();
                    if (binary.op().equals("=")) for (SqlParser.Expr original : FilterRules.terms(query.where())) {
                        if (original instanceof SqlParser.Binary equal && equal.op().equals("=") && equal.left() instanceof SqlParser.Name name && name.parts().getLast().equals(key)) authored.add(equal);
                    }
                    if (authored.isEmpty()) authored.add(binary);
                    for (SqlParser.Binary bound : authored) parts.add(key + " " + bound.op() + " " + (outer(bound.right(), names, alias) ? "outer" : bound.right() instanceof SqlParser.Literal literal && literal.text().equals("null") ? "<value>" : printer.print(bound.right())));
                }
                detail = "PK bound: " + String.join(" and ", parts); scanRows = primary.rows(); pages = primary.pages(); incumbent = primary.cost(); bounded = true; rank = 0;
            }
            if (index != null && (index.cost() < incumbent || index.cost() == incumbent && !bounded)) {
                detail = indexDetail(index, names);
                scanRows = index.rows(); pages = index.pages(); incumbent = index.cost(); bounded = true; rank = IndexAccess.interval(index.predicate()) ? 5 : 1;
            } else index = null;
            GistEstimate.Selection gist = forceFull ? null : GistEstimate.select(table, names, alias, query.where(), resolver, meter, incumbent, rank > 2);
            if (gist != null) { detail = "GiST bound: using " + gist.bound().index().name(); scanRows = gist.rows(); pages = gist.pages(); extra = gist.entries(); incumbent = gist.cost(); bounded = true; rank = 2; }
            GinEstimate.Selection gin = forceFull ? null : GinEstimate.select(table, names, alias, query.where(), resolver, meter, incumbent, false, rank > 3);
            if (gin != null) { detail = "GIN bound: using " + gin.bound().index().name(); scanRows = gin.rows(); pages = gin.pages(); extra = gin.entries(); incumbent = gin.cost(); bounded = true; rank = 3; }
            KeySetEstimate.Selection keySet = forceFull ? null : KeySetEstimate.primary(table, names, alias, query.where(), resolver, meter);
            if (keySet != null && (keySet.cost() < incumbent || keySet.cost() == incumbent && rank > 4)) {
                detail = "PK interval set: " + String.join(", ", keySet.columns()) + "; intervals=" + keySet.intervals() + (keySet.arrays() == 0 ? "" : "; arrays=" + keySet.arrays());
                scanRows = keySet.rows(); pages = keySet.pages(); extra = 0; incumbent = keySet.cost(); bounded = true; rank = 4;
            }
            if (orderedIndex != null) {
                IndexAccess.Selection orderedBound = IndexAccess.select(table, names, alias, query.where(), meter, orderedIndex, resolver, allowPartialFull);
                detail = orderedBound == null ? "Full scan" : indexDetail(orderedBound, names);
                long wanted = PlanEstimate.add(boundEstimate(query.limit(), Long.MAX_VALUE, resolver, meter), boundEstimate(query.offset(), 0, resolver, meter));
                long consumed = n;
                if (statistics.rows(query.where(), n, unique) >= wanted) {
                    long low = 0, high = n;
                    while (low < high) { long middle = low + (high - low) / 2; if (statistics.rows(query.where(), middle, unique) >= wanted) high = middle; else low = middle + 1; }
                    consumed = low;
                }
                scanRows = orderedBound == null ? consumed : Math.min(consumed, orderedBound.rows());
                long indexPages = orderedBound == null ? orderedIndex.entries().structuralFacts().nodeCount() : Math.max(0, orderedBound.pages() - PlanEstimate.mul(orderedBound.rows(), facts.height()));
                pages = PlanEstimate.add(indexPages, PlanEstimate.mul(scanRows, facts.height())); extra = 0;
            }
            int touched = 0; boolean[] columns = resolver.touched(); for (int c = 0; c < Math.min(columns.length, table.columns().size()); c++) if (columns[c]) touched++;
            if (touched != 0) detail += "; touched=" + touched;
            if (!query.order().isEmpty() && !sort) detail += orderedIndex == null ? "; ordered: pk ordered" + (reverse ? " (reverse)" : "") : "; ordered: index order: " + orderedIndex.name();
            boolean contradiction = resolver.provenContradiction(query.where());
            long filtered = Math.min(scanRows, statistics.rows(query.where(), n, unique));
            if (contradiction) { scanRows = 0; filtered = 0; pages = 0; extra = 0; }
            long offset = boundEstimate(query.offset(), 0, resolver, meter), limit = boundEstimate(query.limit(), Long.MAX_VALUE, resolver, meter);
            if (!sort && !distinct && query.where() == null && limit != Long.MAX_VALUE) {
                scanRows = Math.min(scanRows, PlanEstimate.add(offset, limit));
                if (orderedIndex == null) pages = Math.min(pages, Math.max(facts.height(), scanRows));
            }
            if (scanRows == 0) pages = 0;
            PlanNode node = new PlanNode("Scan " + table.name(), detail, scanRows, PlanEstimate.add(extra, PlanEstimate.add(pages, PlanEstimate.mul(scanRows, Costs.STORAGE_ROW_READ))), List.of(), "streaming");
            if (query.where() != null) node = node.wrap("Filter", (verbose ? "filter=" + printer.print(orderedPredicate(query.where(), resolver.queryEnvironment())) : "conjuncts=" + PlanEstimate.conjuncts(query.where())) + (contradiction ? "; contradiction" : ""), Math.min(filtered, scanRows), PlanEstimate.add(PlanEstimate.mul(scanRows, PlanEstimate.operations(query.where(), resolver, meter)), resolver.queryEnvironment() == null ? 0 : resolver.queryEnvironment().hiddenEstimate(query.where(), scanRows)));
            if (distinct) node = node.wrap("Distinct", "", statistics.groups(projections, node.rows()), 0);
            List<SqlParser.Order> orderTerms = distinctOn == null ? query.order() : distinctOn.order;
            if (sort && !orderTerms.isEmpty()) {
                long operations = 0; for (SqlParser.Order order : orderTerms) operations = PlanEstimate.add(operations, PlanEstimate.operations(order.expression()));
                String sortDetail = "keys=" + orderTerms.size();
                if (!distinct && distinctOn == null && limit != Long.MAX_VALUE && (limit == 0 || offset <= Long.MAX_VALUE - limit)) sortDetail += ", top-k=" + (limit == 0 ? 0 : PlanEstimate.add(offset, limit));
                node = node.wrap("Sort", sortDetail, node.rows(), PlanEstimate.mul(node.rows(), operations));
            }
            if (distinctOn != null) node = node.wrap("Distinct", "on=" + distinctOn.keys.size(), statistics.groups(distinctOn.keys, node.rows()), 0);
            if (query.limit() != null || query.offset() != null) {
                String limits = (limit == Long.MAX_VALUE ? "" : "limit=" + limit) + (query.offset() == null ? "" : (limit == Long.MAX_VALUE ? "" : " ") + "offset=" + offset);
                if (!limits.isEmpty()) node = node.wrap("Limit", limits, Math.min(limit, Math.max(0, node.rows() - offset)), 0);
            }
            long output = 0; for (SqlParser.Expr expression : projections) output = PlanEstimate.add(output, PlanEstimate.operations(expression));
            long hidden = 0; if (resolver.queryEnvironment() != null) for (SqlParser.Expr expression : projections) hidden = PlanEstimate.add(hidden, resolver.queryEnvironment().hiddenEstimate(expression, node.rows()));
            String rootDetail = node.detail(); if (verbose && emit) rootDetail = append(rootDetail, "output=[" + printer.list(projections) + "]");
            return node.finish(rootDetail, PlanEstimate.add(hidden, PlanEstimate.mul(node.rows(), PlanEstimate.add(emit ? Costs.ROW_PRODUCED : 0, output))), sort || distinct || projections.stream().anyMatch(TableExplanation::mutates) ? "buffered" : "streaming");
        } catch (IOException failure) { throw new SqlException("58030", "cannot inspect storage skeleton: " + failure.getMessage()); }
    }
    private record SetFacts(long intervals, long arrays) {}
    private static SetFacts setFacts(SqlParser.Expr syntax) {
        long intervals = 0, arrays = 0;
        for (SqlParser.Expr term : FilterRules.terms(syntax)) {
            if (term instanceof SqlParser.In in && !in.negated()) { intervals += in.query() == null ? in.values().size() : 1; if (in.query() != null) arrays++; }
            else if (term instanceof SqlParser.Quantified q && q.operator().equals("=") && !q.quantifier().equals("all")) { intervals++; arrays++; }
            else if (term instanceof SqlParser.Binary b && b.op().equals("or")) intervals += disjuncts(term);
        }
        return new SetFacts(intervals, arrays);
    }
    private static long disjuncts(SqlParser.Expr expression) { return expression instanceof SqlParser.Binary b && b.op().equals("or") ? PlanEstimate.add(disjuncts(b.left()), disjuncts(b.right())) : expression instanceof SqlParser.In in && in.query() == null ? in.values().size() : 1; }
    static String indexDetail(IndexAccess.Selection index, List<String> names) {
        if (index.predicate() == null) return "Index scan: using " + index.index().name();
        SetFacts sets = setFacts(index.predicate());
        if (sets.intervals() == 0) return "Index bound: using " + index.index().name();
        List<String> used = new ArrayList<>();
        for (Engine.IndexKey key : index.index().keys()) {
            if (key.column() < 0) break;
            String name = names.get(key.column());
            if (mentions(index.predicate(), name)) used.add(name); else break;
        }
        return "Index interval set: using " + index.index().name() + (used.size() > 1 ? " (" + String.join(", ", used) + ")" : "") + "; intervals=" + sets.intervals() + (sets.arrays() == 0 ? "" : "; arrays=" + sets.arrays());
    }
    private static boolean mentions(SqlParser.Expr expression, String name) {
        if (expression instanceof SqlParser.Name ref && ref.parts().getLast().equals(name)) return true;
        for (SqlParser.Expr child : DefaultExpression.children(expression)) if (child != null && mentions(child, name)) return true;
        return false;
    }
    private static boolean outer(SqlParser.Expr expression, List<String> names, String alias) {
        return expression instanceof SqlParser.Name name && (name.parts().size() > 1 ? !name.parts().getFirst().equals(alias) : !names.contains(name.parts().getFirst()));
    }
    private static boolean mutates(SqlParser.Expr expression) {
        if (expression instanceof SqlParser.Call call && (call.name().equals("nextval") || call.name().equals("setval"))) return true;
        for (SqlParser.Expr child : DefaultExpression.children(expression)) if (child != null && mutates(child)) return true;
        return false;
    }
    static String append(String detail, String addition) { return detail.isEmpty() ? addition : detail + "; " + addition; }
    /** Mirror the executor's stable conjunct order without changing the authored bound predicates. */
    static SqlParser.Expr orderedPredicate(SqlParser.Expr syntax, QueryEnvironment environment) {
        List<SqlParser.Expr> terms = FilterRules.terms(syntax);
        List<SqlParser.Expr> ordered = new ArrayList<>(terms);
        ordered.sort(java.util.Comparator.<SqlParser.Expr>comparingInt(term -> FilterRules.correlated(term, environment) ? 1 : 0)
            .thenComparingLong(term -> FilterRules.nodes(term, environment)));
        if (ordered.equals(terms)) return syntax;
        SqlParser.Expr result = null;
        for (SqlParser.Expr term : ordered) result = FilterRules.and(result, term);
        return result;
    }
    static long boundEstimate(SqlParser.Expr syntax, long fallback, ScalarPlan.Resolver resolver, CostMeter meter) {
        if (syntax == null || !StatisticsEstimate.literalSyntax(syntax)) return fallback;
        try {
            SqlValue value = meter.unmetered(() -> resolver.resolve(syntax, SqlType.I64).run(meter));
            return value.isNull() ? fallback : Math.max(0, value.asInteger());
        } catch (SqlException unknown) {
            if (unknown.sqlState().equals("42P02")) return fallback;
            throw unknown;
        }
    }
    static long literalBound(SqlParser.Expr syntax, long fallback) {
        if (syntax == null) return fallback;
        if (syntax instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.INTEGER) return Long.parseLong(literal.text());
        if (syntax instanceof SqlParser.Literal literal && literal.text().equals("null")) return fallback;
        throw new SqlException("0A000", "EXPLAIN dynamic row bounds are not connected");
    }
}
