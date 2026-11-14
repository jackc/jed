package org.jed;

import java.io.IOException;
import java.util.ArrayList;
import java.util.List;

/** Metadata-only interval programs. Array values may be read; stored rows never are. */
final class KeySetEstimate {
    private record Term(String op, SqlParser.Expr value) {}
    private record Set(List<List<Term>> branches, long intervals, long arrays) {}
    private record Member(String name, SqlType type, List<List<Term>> branches, List<Term> clip,
                          long intervals, long arrays, boolean points) {}
    record Selection(List<String> columns, long intervals, long arrays, long rows, long pages, long cost) {}
    private final Engine.Table table;
    private final List<String> names;
    private final String alias;
    private final ScalarPlan.Resolver resolver;
    private final CostMeter meter;
    private final StatisticsEstimate statistics;
    private KeySetEstimate(Engine.Table table, List<String> names, String alias, ScalarPlan.Resolver resolver, CostMeter meter) {
        this.table = table; this.names = names; this.alias = alias; this.resolver = resolver; this.meter = meter;
        statistics = new StatisticsEstimate(table, names, alias, resolver, meter);
    }
    static Selection primary(Engine.Table table, List<String> names, String alias, SqlParser.Expr predicate,
                             ScalarPlan.Resolver resolver, CostMeter meter) throws IOException {
        if (table.primaryKey().length == 0 || !CollationCompatibility.table(table, meter)) return null;
        return new KeySetEstimate(table, names, alias, resolver, meter).select(predicate);
    }
    private Selection select(SqlParser.Expr predicate) throws IOException {
        List<Member> members = new ArrayList<>(); boolean hasSet = false, points = true; long product = 1, intervals = 0, arrays = 0;
        for (int column : table.primaryKey()) {
            String name = names.get(column); SqlType type = table.columns().get(column).type();
            Set set = null; List<Term> clip = new ArrayList<>();
            for (SqlParser.Expr term : FilterRules.terms(predicate)) {
                if (setSyntax(term)) { if (set == null) set = set(term, name, type); }
                else { List<Term> bounds = bounds(term, name, type); if (bounds != null) clip.addAll(bounds); }
            }
            if (set == null && clip.isEmpty()) break;
            boolean memberPoints = set == null ? clip.stream().anyMatch(term -> term.op().equals("="))
                : set.branches().stream().allMatch(branch -> branch.stream().anyMatch(term -> term.op().equals("=")));
            long count = set == null ? 1 : set.branches().stream().filter(branch -> !empty(branch, type)).count();
            if (count > 1 && product > 65536 / count) break;
            product = PlanEstimate.mul(product, count); points &= memberPoints;
            members.add(new Member(name, type, set == null ? null : set.branches(), clip, set == null ? 0 : set.intervals(), set == null ? 0 : set.arrays(), memberPoints));
            if (set != null) { hasSet = true; intervals += set.intervals(); arrays += set.arrays(); }
            if (!memberPoints) break;
        }
        if (!hasSet) return null;
        boolean uniqueTuple = points && members.size() == table.primaryKey().length;
        long rows = rows(members, table.rows().size(), uniqueTuple, product);
        var facts = table.rows().structuralFacts();
        long pages = rows == 0 ? 0 : Math.min(facts.nodeCount(), Math.max(facts.height(), rows(members, facts.nodeCount(), uniqueTuple, product)));
        return new Selection(members.stream().map(Member::name).toList(), intervals, arrays, rows, pages,
            PlanEstimate.add(pages, PlanEstimate.mul(rows, 1 + PlanEstimate.operations(predicate, resolver, meter))));
    }
    private long rows(List<Member> members, long n, boolean uniqueTuple, long product) {
        long rows = n;
        for (Member member : members) {
            if (member.branches() != null) {
                long sum = 0;
                for (List<Term> branch : member.branches()) sum = Math.min(rows, PlanEstimate.add(sum, range(branch, member, rows, members.size() == 1 && table.primaryKey().length == 1)));
                rows = sum;
            }
            rows = range(member.clip(), member, rows, false);
        }
        return uniqueTuple ? Math.min(rows, product) : rows;
    }
    private long range(List<Term> terms, Member member, long n, boolean unique) {
        if (terms.isEmpty() || n == 0) return n;
        if (empty(terms, member.type())) return 0;
        Term equal = terms.stream().filter(term -> term.op().equals("=")).findFirst().orElse(null);
        SqlParser.Expr key = new SqlParser.Name(List.of(alias, member.name()));
        if (equal != null) {
            if (unique && terms.stream().allMatch(term -> term.op().equals("="))) return Math.min(n, 1);
            if (equal.value() == null) return statistics.unknownEquality(key, n);
            return statistics.rows(new SqlParser.Binary("=", key, equal.value()), n, List.of());
        }
        SqlParser.Expr syntax = null;
        for (Term term : terms) syntax = FilterRules.and(syntax, new SqlParser.Binary(term.op(), key, term.value()));
        return statistics.rows(syntax, n, List.of());
    }
    private boolean empty(List<Term> terms, SqlType type) {
        for (int i = 0; i < terms.size(); i++) {
            Term term = terms.get(i); SqlValue value = known(term.value(), type);
            if (value == null) continue;
            if (value.isNull()) return true;
            if (term.op().equals("=") && type.isInteger() && value.type().isInteger() && !type.contains(value.asInteger())) return true;
            for (int j = 0; j < i; j++) {
                Term prior = terms.get(j); SqlValue previous = known(prior.value(), type);
                if (previous == null || previous.isNull()) continue;
                int order = meter.unmetered(() -> ScalarPlan.compare(previous, value, null));
                if (prior.op().equals("=") && !accept(term.op(), order) || term.op().equals("=") && !accept(prior.op(), -order)) return true;
                if (prior.op().startsWith(">") && term.op().startsWith("<") && (order > 0 || order == 0 && (!prior.op().equals(">=") || !term.op().equals("<=")))) return true;
                if (prior.op().startsWith("<") && term.op().startsWith(">") && (order < 0 || order == 0 && (!prior.op().equals("<=") || !term.op().equals(">=")))) return true;
            }
        }
        return false;
    }
    private Set set(SqlParser.Expr expression, String name, SqlType type) {
        if (expression instanceof SqlParser.Binary binary && (binary.op().equals("and") || binary.op().equals("or"))) {
            Set left = set(binary.left(), name, type), right = set(binary.right(), name, type);
            if (left == null || right == null) return null;
            List<List<Term>> branches = new ArrayList<>();
            if (binary.op().equals("or")) { branches.addAll(left.branches()); branches.addAll(right.branches()); }
            else {
                if ((long)left.branches().size() * right.branches().size() > 65536) return null;
                for (List<Term> a : left.branches()) for (List<Term> b : right.branches()) { List<Term> terms = new ArrayList<>(a); terms.addAll(b); branches.add(terms); }
            }
            return new Set(branches, binary.op().equals("or") ? left.intervals() + right.intervals() : Math.max(left.intervals(), right.intervals()), left.arrays() + right.arrays());
        }
        if (expression instanceof SqlParser.In in && !in.negated() && column(in.subject(), name)) {
            if (in.query() != null) return resolver.queryEnvironment() != null && resolver.queryEnvironment().hasPlanInit(expression) ? unknownArray() : null;
            List<List<Term>> branches = new ArrayList<>(); List<SqlParser.Expr> seen = new ArrayList<>();
            for (SqlParser.Expr value : in.values()) { if (!source(value)) return null; if (!seen.contains(value)) { seen.add(value); branches.add(List.of(new Term("=", value))); } }
            return new Set(branches, branches.size(), 0);
        }
        if (expression instanceof SqlParser.Quantified q && q.operator().equals("=") && !q.quantifier().equals("all") && column(q.subject(), name)) {
            if (q.query() != null) return resolver.queryEnvironment() != null && resolver.queryEnvironment().hasPlanInit(expression) ? unknownArray() : null;
            if (!PlanEstimate.arrayOperand(q.array())) return null;
            List<SqlParser.Expr> elements = q.array() instanceof SqlParser.ArrayConstructor array ? array.values() : null;
            if (elements == null) {
                if (!source(q.array())) return null;
                SqlValue array = known(q.array(), null);
                if (array == null) return unknownArray();
                if (array.isNull()) return new Set(List.of(), 1, 1);
                if (array.type().kind() != SqlType.Kind.ARRAY) return null;
                elements = array.asArray().elements().stream().map(value -> (SqlParser.Expr)new SqlParser.StoredValue(value)).toList();
            }
            List<List<Term>> branches = new ArrayList<>();
            for (SqlParser.Expr element : elements) {
                if (!source(element)) return null;
                SqlValue value = known(element, q.array() instanceof SqlParser.ArrayConstructor ? type : null);
                if (value != null && !value.isNull() && value.type().kind() != type.kind()
                    && !((value.type().isInteger() || value.type().kind() == SqlType.Kind.DECIMAL) && (type.isInteger() || type.kind() == SqlType.Kind.DECIMAL))) return null;
                if (value != null && !value.isNull() && value.type().isNumeric() && type.isNumeric()) {
                    try { SqlValue converted = SqlCasts.cast(value, type); element = new SqlParser.StoredValue(ScalarPlan.compare(value, converted, null) == 0 ? converted : SqlValue.nullValue(type)); }
                    catch (SqlException failure) { if (!failure.sqlState().equals("22003")) throw failure; element = new SqlParser.StoredValue(SqlValue.nullValue(type)); }
                }
                branches.add(List.of(new Term("=", element)));
            }
            return new Set(branches, 1, 1);
        }
        List<Term> terms = bounds(expression, name, type);
        return terms == null ? null : new Set(List.of(terms), 1, 0);
    }
    private Set unknownArray() { return new Set(java.util.Collections.nCopies((int)EstimatorFacts.DEFAULT_ARRAY_ELEMENTS, List.of(new Term("=", null))), 1, 1); }
    private List<Term> bounds(SqlParser.Expr expression, String name, SqlType type) {
        if (expression instanceof SqlParser.Between between && !between.negated() && column(between.subject(), name) && source(between.lower()) && source(between.upper())) return List.of(new Term(">=", between.lower()), new Term("<=", between.upper()));
        if (!(expression instanceof SqlParser.Binary b) || !List.of("=", "<", "<=", ">", ">=").contains(b.op())) return null;
        if (column(b.left(), name) && source(b.right())) return List.of(new Term(b.op(), b.right()));
        if (column(b.right(), name) && source(b.left())) return List.of(new Term(switch (b.op()) { case "<" -> ">"; case "<=" -> ">="; case ">" -> "<"; case ">=" -> "<="; default -> b.op(); }, b.left()));
        return null;
    }
    private boolean source(SqlParser.Expr expression) { return StatisticsEstimate.literalSyntax(expression) || expression instanceof SqlParser.Name name && (name.parts().size() > 1 ? !name.parts().getFirst().equals(alias) : !names.contains(name.parts().getFirst())); }
    private boolean column(SqlParser.Expr expression, String name) { return expression instanceof SqlParser.Name ref && ref.parts().getLast().equals(name) && (ref.parts().size() == 1 || ref.parts().size() == 2 && ref.parts().getFirst().equals(alias)); }
    private SqlValue known(SqlParser.Expr expression, SqlType type) {
        if (!StatisticsEstimate.literalSyntax(expression)) return null;
        try { ScalarPlan.Expression value = resolver.resolve(expression, type); return value.constant() ? meter.unmetered(() -> value.run(meter)) : null; }
        catch (SqlException failure) { if (List.of("42P02", "42703", "42P01", "22003").contains(failure.sqlState())) return null; throw failure; }
    }
    private static boolean setSyntax(SqlParser.Expr expression) { return expression instanceof SqlParser.In || expression instanceof SqlParser.Quantified || expression instanceof SqlParser.Binary b && b.op().equals("or"); }
    private static boolean accept(String op, int order) { return switch (op) { case "=" -> order == 0; case "<" -> order < 0; case "<=" -> order <= 0; case ">" -> order > 0; case ">=" -> order >= 0; default -> false; }; }
}
