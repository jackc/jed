package org.jed;

import java.util.ArrayList;
import java.util.List;

/** Saturating integer estimator arithmetic and source-ordered predicate cardinality. */
final class PlanEstimate {
    private PlanEstimate() {}
    static long add(long a, long b) { return a > Long.MAX_VALUE - b ? Long.MAX_VALUE : a + b; }
    static long mul(long a, long b) { return b != 0 && a > Long.MAX_VALUE / b ? Long.MAX_VALUE : a * b; }
    static long scale(long n, EstimatorFacts.Fraction fraction) {
        long p = fraction.numerator(), q = fraction.denominator();
        return add(mul(n / q, p), ((n % q) * p + q - 1) / q);
    }
    static long groups(long rows, int keys) {
        long distinct = 1; for (int i = 0; i < keys; i++) distinct = mul(distinct, EstimatorFacts.DEFAULT_DISTINCT_VALUES);
        return Math.min(rows, distinct);
    }
    static int conjuncts(SqlParser.Expr syntax) {
        if (syntax == null) return 0; syntax = DefaultExpression.unwrap(syntax);
        if (syntax instanceof SqlParser.Binary b && b.op().equals("and")) return conjuncts(b.left()) + conjuncts(b.right());
        return syntax instanceof SqlParser.Between between && !between.negated() ? 2 : 1;
    }
    static long operations(SqlParser.Expr syntax) {
        if (syntax == null) return 0; syntax = DefaultExpression.unwrap(syntax);
        if (syntax instanceof SqlParser.Name || syntax instanceof SqlParser.Literal || syntax instanceof SqlParser.Parameter || syntax instanceof SqlParser.StoredValue || syntax instanceof SqlParser.ColumnSlot || syntax instanceof SqlParser.Star) return 0;
        if (syntax instanceof SqlParser.Cast cast && cast.value() instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.STRING) return 0;
        if (syntax instanceof SqlParser.Cast cast && cast.value() instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.WORD && literal.text().equals("null") && cast.target().dimensions() > 0) return 0;
        long count = syntax instanceof SqlParser.Collate ? 0 : syntax instanceof SqlParser.Between ? 3 : 1;
        if (syntax instanceof SqlParser.In in && in.query() == null) count = Math.max(1, 2L * in.values().size() - 1);
        if (syntax instanceof SqlParser.Quantified value && value.array() instanceof SqlParser.ArrayConstructor array) count = add(count, array.values().size());
        for (SqlParser.Expr child : DefaultExpression.children(syntax)) count = add(count, operations(child));
        return count;
    }
    static long operations(SqlParser.Expr syntax, ScalarPlan.Resolver constants, CostMeter meter) {
        if (syntax == null) return 0;
        syntax = DefaultExpression.unwrap(syntax);
        if (syntax instanceof SqlParser.Name || syntax instanceof SqlParser.Literal || syntax instanceof SqlParser.Parameter || syntax instanceof SqlParser.StoredValue || syntax instanceof SqlParser.ColumnSlot || syntax instanceof SqlParser.Star) return 0;
        if (syntax instanceof SqlParser.Cast cast && cast.value() instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.STRING) return 0;
        if (syntax instanceof SqlParser.Cast cast && cast.value() instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.WORD && literal.text().equals("null") && cast.target().dimensions() > 0) return 0;
        long count = syntax instanceof SqlParser.Collate ? 0 : syntax instanceof SqlParser.Between ? 3 : 1;
        if (syntax instanceof SqlParser.In in && in.query() == null) count = Math.max(1, 2L * in.values().size() - 1);
        if (syntax instanceof SqlParser.Quantified q && q.query() == null) {
            Long length = q.array() instanceof SqlParser.ArrayConstructor array ? (long)array.values().size() : null;
            if (length == null && arrayOperand(q.array()) && StatisticsEstimate.literalSyntax(q.array())) {
                try {
                    ScalarPlan.Expression resolved = constants.resolve(q.array(), null);
                    SqlValue value = resolved.constant() ? meter.unmetered(() -> resolved.run(meter)) : null;
                    if (value != null) length = value.isNull() ? 0 : value.type().kind() == SqlType.Kind.ARRAY ? (long)value.asArray().elements().size() : null;
                } catch (SqlException failure) { if (!List.of("42P02", "42703", "42P01").contains(failure.sqlState())) throw failure; }
            }
            if (length != null) count = add(count, length);
        }
        for (SqlParser.Expr child : DefaultExpression.children(syntax)) count = add(count, operations(child, constants, meter));
        return count;
    }
    static long rows(SqlParser.Expr syntax, long n, List<String> uniqueColumns) {
        if (syntax == null || n == 0) return n; syntax = DefaultExpression.unwrap(syntax);
        if (syntax instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.WORD) return literal.text().equals("true") ? n : 0;
        if (syntax instanceof SqlParser.Unary unary && (unary.op().equals("is null") || unary.op().equals("is not null"))) {
            long nulls = scale(n, EstimatorFacts.NULL_TEST); return unary.op().equals("is null") ? nulls : n - nulls;
        }
        if (syntax instanceof SqlParser.Unary unary && unary.op().equals("not")) return n - rows(unary.value(), n, uniqueColumns);
        if (syntax instanceof SqlParser.Binary binary) {
            if (binary.op().equals("or")) return Math.min(n, add(rows(binary.left(), n, uniqueColumns), rows(binary.right(), n, uniqueColumns)));
            if (binary.op().equals("and")) {
                List<SqlParser.Expr> terms = FilterRules.terms(syntax); List<SqlParser.Expr> consumed = new ArrayList<>(); long count = n;
                for (SqlParser.Expr term : terms) {
                    if (consumed.contains(term)) continue;
                    boolean paired = false;
                    if (term instanceof SqlParser.Binary lower && List.of(">", ">=").contains(lower.op())) {
                        for (SqlParser.Expr candidate : terms) if (candidate instanceof SqlParser.Binary upper && List.of("<", "<=").contains(upper.op()) && upper.left().equals(lower.left())) { consumed.add(candidate); paired = true; break; }
                    }
                    if (term instanceof SqlParser.Binary upper && List.of("<", "<=").contains(upper.op())) {
                        for (SqlParser.Expr candidate : terms) if (candidate instanceof SqlParser.Binary lower && List.of(">", ">=").contains(lower.op()) && lower.left().equals(upper.left())) { consumed.add(candidate); paired = true; break; }
                    }
                    count = paired ? scale(count, EstimatorFacts.PAIRED_RANGE) : rows(term, count, uniqueColumns);
                }
                return count;
            }
            if (List.of("like", "ilike", "~", "~*", "not like", "not ilike", "!~", "!~*").contains(binary.op())) {
                if (nullLiteral(binary.left()) || nullLiteral(binary.right())) return 0;
                long matches = scale(n, EstimatorFacts.MATCH);
                return binary.op().startsWith("not ") || binary.op().startsWith("!") ? n - matches : matches;
            }
            if (List.of("=", "<>", "!=", "<", "<=", ">", ">=").contains(binary.op())) {
                if (nullLiteral(binary.left()) || nullLiteral(binary.right())) return 0;
                boolean unique = binary.left() instanceof SqlParser.Name name && uniqueColumns.contains(name.parts().getLast()) || binary.right() instanceof SqlParser.Name rightName && uniqueColumns.contains(rightName.parts().getLast());
                long equal = unique ? Math.min(n, 1) : scale(n, EstimatorFacts.EQUALITY);
                return binary.op().equals("=") ? equal : List.of("!=", "<>").contains(binary.op()) ? n - equal : scale(n, EstimatorFacts.INEQUALITY);
            }
        }
        if (syntax instanceof SqlParser.Between between) return between.negated() ? Math.min(n, mul(scale(n, EstimatorFacts.INEQUALITY), 2)) : scale(n, EstimatorFacts.PAIRED_RANGE);
        if (syntax instanceof SqlParser.In in && in.query() == null) {
            long count = 0; List<SqlParser.Expr> seen = new ArrayList<>();
            for (SqlParser.Expr item : in.values()) { if (nullLiteral(item)) { if (in.negated()) return 0; continue; } if (seen.contains(item)) continue; seen.add(item); count = Math.min(n, add(count, rows(new SqlParser.Binary("=", in.subject(), item), n, uniqueColumns))); }
            return in.negated() ? n - count : count;
        }
        if (syntax instanceof SqlParser.Quantified quantified && quantified.operator().equals("=") && !quantified.quantifier().equals("all") && arrayOperand(quantified.array())) {
            long count = 0;
            List<SqlParser.Expr> items = quantified.array() instanceof SqlParser.ArrayConstructor array ? array.values() : java.util.Collections.nCopies((int)EstimatorFacts.DEFAULT_ARRAY_ELEMENTS, new SqlParser.Parameter(1));
            for (SqlParser.Expr item : items) count = Math.min(n, add(count, rows(new SqlParser.Binary("=", quantified.subject(), item), n, uniqueColumns)));
            return count;
        }
        return scale(n, syntax instanceof SqlParser.Name ? EstimatorFacts.BOOLEAN : EstimatorFacts.OPAQUE);
    }
    static boolean arrayOperand(SqlParser.Expr syntax) {
        if (syntax instanceof SqlParser.ArrayConstructor array) return array.query() == null && array.values().stream().allMatch(value -> value instanceof SqlParser.Literal || value instanceof SqlParser.Parameter || value instanceof SqlParser.Name);
        return syntax instanceof SqlParser.Parameter || syntax instanceof SqlParser.Name || syntax instanceof SqlParser.StoredValue || syntax instanceof SqlParser.Cast cast && cast.value() instanceof SqlParser.Literal;
    }
    private static boolean nullLiteral(SqlParser.Expr syntax) { return syntax instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.WORD && literal.text().equals("null"); }
}
