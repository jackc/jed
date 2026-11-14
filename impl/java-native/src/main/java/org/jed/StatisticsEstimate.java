package org.jed;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

/** Metadata-only statistics selectivity; never reads storage leaves or executes user functions. */
final class StatisticsEstimate {
    private final Engine.Table table;
    private final List<String> names;
    private final String alias;
    private final ScalarPlan.Resolver constants;
    private final CostMeter meter;
    private final long population;
    StatisticsEstimate(Engine.Table table, List<String> names, String alias, ScalarPlan.Resolver constants, CostMeter meter) {
        this.table = table; this.names = names; this.alias = alias; this.constants = constants; this.meter = meter; population = table.rows().size();
    }
    static long scale(long n, long p, long q) {
        if (n == 0 || p == 0) return 0;
        if (q <= 0) throw new IllegalArgumentException("statistics denominator");
        long low = n * p, high = Math.multiplyHigh(n, p), remainder = 0, quotient = 0;
        for (int bit = 127; bit >= 0; bit--) {
            boolean carry = remainder < 0; remainder = (remainder << 1) | ((bit >= 64 ? high >>> (bit - 64) : low >>> bit) & 1);
            if (carry || Long.compareUnsigned(remainder, q) >= 0) { remainder -= q; if (bit >= 63) return Long.MAX_VALUE; quotient |= 1L << bit; }
        }
        return remainder == 0 ? quotient : PlanEstimate.add(quotient, 1);
    }
    long population() { return population; }
    long unknownEquality(SqlParser.Expr key, long rows) { Fact fact = fact(key); return fact != null && fact.statistics.distinctCount() != null ? scale(rows, average(fact), population) : PlanEstimate.scale(rows, EstimatorFacts.EQUALITY); }
    record Distribution(long nonnull, long distinct) {}
    Distribution distribution(SqlParser.Expr key) { Fact fact = fact(key); return fact == null || fact.statistics.distinctCount() == null ? null : new Distribution(fact.nonnull(), fact.ndv()); }
    long groups(List<SqlParser.Expr> keys, long inputRows) {
        long groups = 1;
        for (SqlParser.Expr key : keys) { Fact fact = fact(key); groups = PlanEstimate.mul(groups, fact == null || fact.statistics.distinctCount() == null ? EstimatorFacts.DEFAULT_DISTINCT_VALUES : PlanEstimate.add(fact.ndv(), fact.nulls() > 0 ? 1 : 0)); }
        return Math.min(inputRows, groups);
    }
    long rows(SqlParser.Expr expression, long n, List<String> unique) {
        if (expression == null || n == 0) return n;
        return estimate(DefaultExpression.unwrap(expression), n, unique);
    }
    private long estimate(SqlParser.Expr expression, long n, List<String> unique) {
        if (expression instanceof SqlParser.Binary binary && binary.op().equals("and")) {
            var terms = FilterRules.terms(expression); var consumed = new ArrayList<SqlParser.Expr>(); long rows = n;
            for (SqlParser.Expr term : terms) {
                if (consumed.contains(term)) continue; Long pair = null; boolean paired = false;
                if (term instanceof SqlParser.Binary bound && List.of("<", "<=", ">", ">=").contains(bound.op())) {
                    for (SqlParser.Expr candidate : terms) if (candidate != term && !consumed.contains(candidate) && candidate instanceof SqlParser.Binary other && other.left().equals(bound.left()) && List.of("<", "<=", ">", ">=").contains(other.op()) && bound.op().startsWith("<") != other.op().startsWith("<")) {
                        Fact fact = fact(bound.left());
                        pair = fact == null ? null : paired(fact, bound.op().startsWith(">") ? bound : other, bound.op().startsWith("<") ? bound : other);
                        paired = true; consumed.add(candidate); break;
                    }
                }
                rows = pair != null ? scale(rows, pair, population) : paired ? PlanEstimate.scale(rows, EstimatorFacts.PAIRED_RANGE) : estimate(term, rows, unique);
            }
            return rows;
        }
        if (expression instanceof SqlParser.Binary binary && binary.op().equals("or")) return Math.min(n, PlanEstimate.add(estimate(binary.left(), n, unique), estimate(binary.right(), n, unique)));
        if (expression instanceof SqlParser.Unary unary && unary.op().equals("not")) {
            Fact fact = subjectFact(unary.value());
            return Math.max(0, (fact == null ? n : scale(n, fact.nonnull(), population)) - estimate(unary.value(), n, unique));
        }
        if (expression instanceof SqlParser.Unary unary && List.of("is null", "is not null").contains(unary.op())) { Fact fact = fact(unary.value()); if (fact != null) return scale(n, unary.op().equals("is null") ? fact.nulls() : fact.nonnull(), population); }
        if (expression instanceof SqlParser.Binary binary && List.of("=", "!=", "<>", "<", "<=", ">", ">=").contains(binary.op())) {
            Fact fact = fact(binary.left()); SqlParser.Expr value = binary.right(); String op = binary.op();
            if (fact == null) { fact = fact(binary.right()); value = binary.left(); op = reverse(op); }
            if (fact != null && fact.statistics.distinctCount() != null) { Long result = compare(fact, op, value); if (result != null) return scale(n, result, population); }
        }
        if (expression instanceof SqlParser.In in && in.query() == null) {
            Fact fact = fact(in.subject()); if (fact != null && fact.statistics.distinctCount() != null) { Long result = in(fact, in.values(), in.negated(), n); if (result != null) return result; }
        }
        if (expression instanceof SqlParser.Quantified quantified && quantified.query() == null && PlanEstimate.arrayOperand(quantified.array()) && (quantified.operator().equals("=") && !quantified.quantifier().equals("all") || quantified.operator().equals("<>") && quantified.quantifier().equals("all"))) {
            Fact fact = fact(quantified.subject());
            if (fact != null && fact.statistics.distinctCount() != null) {
                List<SqlParser.Expr> items = quantified.array() instanceof SqlParser.ArrayConstructor array ? array.values() : null;
                if (items == null) { SqlValue value = known(quantified.array(), null); if (value != null) { if (value.isNull()) return 0; if (value.type().kind() == SqlType.Kind.ARRAY) items = value.asArray().elements().stream().map(v -> (SqlParser.Expr)new SqlParser.StoredValue(v)).toList(); } }
                if (items != null) { Long result = in(fact, items, quantified.quantifier().equals("all"), n); if (result != null) return result; }
            }
        }
        if (expression instanceof SqlParser.Between between) {
            Fact fact = fact(between.subject());
            if (fact != null && fact.statistics.distinctCount() != null) {
                Long lower = compare(fact, "<", between.lower()), upper = compare(fact, "<=", between.upper());
                Long inside = interval(fact, between.lower(), false, between.upper(), true);
                if (inside != null) return scale(n, between.negated() ? fact.nonnull() - inside : inside, population);
                if (lower != null && upper != null) return scale(n, between.negated() ? Math.min(fact.nonnull(), PlanEstimate.add(lower, fact.nonnull() - upper)) : Math.max(0, upper - lower), population);
            }
        }
        if (expression instanceof SqlParser.Quantified q && q.query() == null && PlanEstimate.arrayOperand(q.array())
            && (q.operator().equals("=") && !q.quantifier().equals("all") || q.operator().equals("<>") && q.quantifier().equals("all"))) {
            List<SqlParser.Expr> items = q.array() instanceof SqlParser.ArrayConstructor array ? array.values() : null;
            if (items == null) {
                SqlValue array = known(q.array(), null);
                if (array != null) {
                    if (array.isNull()) return 0;
                    if (array.type().kind() == SqlType.Kind.ARRAY) items = array.asArray().elements().stream().map(value -> (SqlParser.Expr)new SqlParser.StoredValue(value)).toList();
                }
            }
            if (items != null) {
                long result = 0;
                for (SqlParser.Expr item : items) {
                    SqlValue value = known(item, null);
                    if (value != null && value.isNull()) { if (q.quantifier().equals("all")) return 0; continue; }
                    result = Math.min(n, PlanEstimate.add(result, estimate(new SqlParser.Binary("=", q.subject(), item), n, unique)));
                }
                return q.quantifier().equals("all") ? n - result : result;
            }
        }
        if (constants.queryEnvironment() != null && constants.queryEnvironment().hasPlanInit(expression)) {
            SqlParser.Expr subject = expression instanceof SqlParser.In in && !in.negated() ? in.subject()
                : expression instanceof SqlParser.Quantified q && q.operator().equals("=") && !q.quantifier().equals("all") ? q.subject() : null;
            if (subject != null) {
                Fact fact = fact(subject);
                boolean uniqueKey = subject instanceof SqlParser.Name name && unique.contains(name.parts().getLast());
                long equality = uniqueKey ? Math.min(n, 1) : fact != null && fact.statistics.distinctCount() != null ? scale(n, average(fact), population) : PlanEstimate.scale(n, EstimatorFacts.EQUALITY);
                return Math.min(n, PlanEstimate.mul(equality, EstimatorFacts.DEFAULT_ARRAY_ELEMENTS));
            }
        }
        Fact booleanFact = fact(expression);
        if (booleanFact != null && booleanFact.definition.type().equals(SqlType.BOOLEAN)) return scale(n, equality(booleanFact, SqlValue.ofBoolean(true)), population);
        Long gin = GinEstimate.logical(table, names, alias, expression, constants, meter);
        return gin == null ? PlanEstimate.rows(expression, n, unique) : scale(n, gin, population);
    }
    private Long paired(Fact fact, SqlParser.Binary lower, SqlParser.Binary upper) {
        if (fact.statistics.distinctCount() == null) return null;
        Long inside = interval(fact, lower.right(), lower.op().equals(">"), upper.right(), upper.op().equals("<="));
        if (inside != null) return inside;
        Long below = compare(fact, lower.op().equals(">") ? "<=" : "<", lower.right()), above = compare(fact, upper.op(), upper.right());
        return below == null || above == null ? null : Math.max(0, above - below);
    }
    private Long in(Fact fact, List<SqlParser.Expr> items, boolean negated, long n) {
        long rows = 0; List<byte[]> seen = new ArrayList<>();
        for (SqlParser.Expr item : items) {
            SqlValue value = known(item, fact.definition.type());
            if (value == null) { if (item instanceof SqlParser.Parameter) { rows = Math.min(n, PlanEstimate.add(rows, scale(n, average(fact), population))); continue; } return null; }
            if (value.isNull()) { if (negated) return 0L; continue; }
            SqlValue converted = exact(value, fact.definition.type()); if (converted == null) continue;
            byte[] key = ColumnStatistics.key(converted, fact.definition, meter); if (seen.stream().anyMatch(previous -> Arrays.equals(previous, key))) continue; seen.add(key);
            rows = Math.min(n, PlanEstimate.add(rows, scale(n, equality(fact, converted), population)));
        }
        return negated ? Math.max(0, scale(n, fact.nonnull(), population) - rows) : rows;
    }
    private Long compare(Fact fact, String op, SqlParser.Expr syntax) {
        SqlValue value = known(syntax, fact.definition.type());
        if (value == null) return (syntax instanceof SqlParser.Parameter || syntax instanceof SqlParser.Name && fact(syntax) == null) && op.equals("=") ? average(fact) : null;
        if (value.isNull()) return 0L;
        SqlValue converted = exact(value, fact.definition.type());
        if (converted == null) return op.equals("=") ? 0L : op.equals("<>") || op.equals("!=") ? fact.nonnull() : null;
        if (op.equals("=")) return equality(fact, converted);
        if (op.equals("<>") || op.equals("!=")) return fact.nonnull() - equality(fact, converted);
        boolean inclusive = op.equals("<=") || op.equals(">"); long less = cdf(fact, converted, inclusive);
        return op.startsWith(">") ? fact.nonnull() - less : less;
    }
    private long average(Fact fact) { return scale(fact.nonnull(), 1, Math.max(1, fact.ndv())); }
    private long equality(Fact fact, SqlValue value) {
        byte[] key = ColumnStatistics.key(value, fact.definition, meter); long used = 0;
        for (ColumnStatistics.Mcv item : fact.statistics.mcv()) {
            long count = Math.min(fact.nonnull() - used, scale(population, item.frequency(), fact.statistics.sampleRows())); used += count;
            if (Arrays.equals(key, ColumnStatistics.key(item.value(), fact.definition, meter))) return count;
        }
        long residual = fact.nonnull() - used;
        if (!fact.statistics.stale() && fact.statistics.sampleRows() == fact.statistics.analyzedRows() && fact.statistics.mcv().stream().mapToLong(ColumnStatistics.Mcv::frequency).sum() == fact.statistics.sampleNonnullRows()) return 0;
        return Math.max(fact.nonnull() == 0 ? 0 : 1, scale(residual, 1, Math.max(1, fact.ndv() - fact.statistics.mcv().size())));
    }
    private Long interval(Fact fact, SqlParser.Expr lower, boolean lowerInclusive, SqlParser.Expr upper, boolean upperInclusive) {
        SqlValue low = known(lower, fact.definition.type()), high = known(upper, fact.definition.type());
        if (low == null || high == null || low.isNull() || high.isNull()) return null;
        low = exact(low, fact.definition.type()); high = exact(high, fact.definition.type()); if (low == null || high == null) return null;
        Cdf a = steps(fact, low, lowerInclusive), b = steps(fact, high, upperInclusive);
        return Math.max(0, b.mcv() - a.mcv()) + Math.min(a.residual(), scale(a.residual(), Math.max(0, b.ordinal() - a.ordinal()), Math.max(1, a.denominator())));
    }
    private record Cdf(long mcv, long residual, long ordinal, long denominator) {}
    private long cdf(Fact fact, SqlValue value, boolean inclusive) {
        Cdf cdf = steps(fact, value, inclusive);
        return Math.min(fact.nonnull(), cdf.mcv() + (cdf.denominator() > 0 ? Math.min(cdf.residual(), scale(cdf.residual(), cdf.ordinal(), cdf.denominator())) : PlanEstimate.scale(cdf.residual(), EstimatorFacts.INEQUALITY)));
    }
    private Cdf steps(Fact fact, SqlValue value, boolean inclusive) {
        byte[] key = ColumnStatistics.key(value, fact.definition, meter); long matched = 0, used = 0;
        for (ColumnStatistics.Mcv item : fact.statistics.mcv()) {
            long count = Math.min(fact.nonnull() - used, scale(population, item.frequency(), fact.statistics.sampleRows())); used += count;
            int order = Arrays.compareUnsigned(ColumnStatistics.key(item.value(), fact.definition, meter), key); if (order < 0 || inclusive && order == 0) matched += count;
        }
        var bounds = fact.statistics.histogram(); long residual = fact.nonnull() - used;
        if (bounds.size() >= 2) {
            int ordinal = 0; for (SqlValue bound : bounds) { int order = Arrays.compareUnsigned(ColumnStatistics.key(bound, fact.definition, meter), key); if (order < 0 || inclusive && order == 0) ordinal++; else break; }
            return new Cdf(matched, residual, ordinal, bounds.size() - 1);
        }
        return new Cdf(matched, residual, 0, 0);
    }
    private SqlValue known(SqlParser.Expr expression, SqlType type) {
        if (!literalSyntax(expression)) return null;
        try { ScalarPlan.Expression resolved = constants.resolve(expression, type); return resolved.constant() ? meter.unmetered(() -> resolved.run(meter)) : null; }
        catch (SqlException unavailable) { if (List.of("42P02", "42703", "42P01", "22003").contains(unavailable.sqlState())) return null; throw unavailable; }
    }
    static boolean literalSyntax(SqlParser.Expr expression) {
        if (expression instanceof SqlParser.Literal || expression instanceof SqlParser.StoredValue || expression instanceof SqlParser.Parameter) return true;
        if (expression instanceof SqlParser.Cast cast) return literalSyntax(cast.value());
        if (expression instanceof SqlParser.Unary unary && List.of("+", "-").contains(unary.op())) return literalSyntax(unary.value());
        if (expression instanceof SqlParser.ArrayConstructor array) return array.values().stream().allMatch(StatisticsEstimate::literalSyntax);
        return false;
    }
    private SqlValue exact(SqlValue value, SqlType type) {
        if (value.type().equals(type)) return value;
        try { SqlValue converted = meter.unmetered(() -> SqlCasts.cast(value, type)); return value.type().isNumeric() && type.isNumeric() && meter.unmetered(() -> ScalarPlan.compare(value, converted, meter)) != 0 ? null : converted; }
        catch (SqlException invalid) { if (List.of("22003", "42846").contains(invalid.sqlState())) return null; throw invalid; }
    }
    private Fact subjectFact(SqlParser.Expr expression) {
        if (expression instanceof SqlParser.Binary binary) { Fact fact = fact(binary.left()); return fact == null ? fact(binary.right()) : fact; }
        if (expression instanceof SqlParser.Between between) return fact(between.subject());
        if (expression instanceof SqlParser.In in) return fact(in.subject());
        return fact(expression);
    }
    private Fact fact(SqlParser.Expr expression) {
        int column = expression instanceof SqlParser.ColumnSlot slot ? slot.index() : expression instanceof SqlParser.Name name && (name.parts().size() == 1 || name.parts().size() == 2 && name.parts().getFirst().equals(alias)) ? names.indexOf(name.parts().getLast()) : -1;
        if (column < 0 || column >= table.columns().size()) return null;
        ColumnStatistics statistics = table.statistics().get(column); Engine.Column definition = table.columns().get(column);
        return statistics == null || statistics.analyzedRows() == 0 || !CollationCompatibility.matches(definition.collation(), meter.collations(), meter.environment()) ? null : new Fact(definition, statistics);
    }
    private final class Fact {
        final Engine.Column definition; final ColumnStatistics statistics;
        Fact(Engine.Column definition, ColumnStatistics statistics) { this.definition = definition; this.statistics = statistics; }
        long nulls() { return Math.min(population, scale(population, statistics.nullCount(), statistics.analyzedRows())); }
        long nonnull() { return population - nulls(); }
        long ndv() { long count = statistics.distinctCount(); long oldNonnull = statistics.analyzedRows() - statistics.nullCount(); return Math.min(nonnull(), PlanEstimate.mul(count, 10) > oldNonnull && oldNonnull > 0 ? scale(nonnull(), count, oldNonnull) : count); }
    }
    private static String reverse(String op) { return switch(op) { case "<" -> ">"; case "<=" -> ">="; case ">" -> "<"; case ">=" -> "<="; default -> op; }; }
}
