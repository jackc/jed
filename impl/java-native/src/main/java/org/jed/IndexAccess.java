package org.jed;

import java.io.IOException;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

/** Equality-tuple index candidates; the original predicate remains a residual filter. */
final class IndexAccess {
    record Range(byte[] lower, byte[] upper) {}
    record Bound(Engine.Index index, List<Range> ranges, long pages, long rows) {}
    private IndexAccess() {}
    record Selection(Engine.Index index, SqlParser.Expr predicate, long rows, long pages, long cost) {}
    static Selection select(Engine.Table table, List<String> names, String alias, SqlParser.Expr predicate, CostMeter meter) throws IOException {
        return select(table, names, alias, predicate, meter, null);
    }
    static Selection select(Engine.Table table, List<String> names, String alias, SqlParser.Expr predicate, CostMeter meter, Engine.Index preferred) throws IOException {
        return select(table, names, alias, predicate, meter, preferred, new ScalarPlan.Resolver(new SqlValue[0]));
    }
    static Selection select(Engine.Table table, List<String> names, String alias, SqlParser.Expr predicate, CostMeter meter, Engine.Index preferred, ScalarPlan.Resolver constants) throws IOException {
        return select(table, names, alias, predicate, meter, preferred, constants, true);
    }
    static Selection select(Engine.Table table, List<String> names, String alias, SqlParser.Expr predicate, CostMeter meter, Engine.Index preferred, ScalarPlan.Resolver constants, boolean allowPartialFull) throws IOException {
        return inventory(table, names, alias, predicate, meter, preferred, constants, allowPartialFull, null);
    }
    static List<Selection> inventory(Engine.Table table, List<String> names, String alias, SqlParser.Expr predicate, CostMeter meter, ScalarPlan.Resolver constants, boolean allowPartialFull) throws IOException {
        List<Selection> candidates = new ArrayList<>();
        inventory(table, names, alias, predicate, meter, null, constants, allowPartialFull, candidates);
        candidates.sort(java.util.Comparator.<Selection>comparingInt(value -> interval(value.predicate()) ? 5 : 1).thenComparing(value -> value.index().name(), SqlText::compare));
        return List.copyOf(candidates);
    }
    private static Selection inventory(Engine.Table table, List<String> names, String alias, SqlParser.Expr predicate, CostMeter meter, Engine.Index preferred, ScalarPlan.Resolver constants, boolean allowPartialFull, List<Selection> candidates) throws IOException {
        StatisticsEstimate statistics = new StatisticsEstimate(table, names, alias, constants, meter);
        List<SqlParser.Expr> terms = new ArrayList<>(); conjuncts(predicate, terms);
        var tableFacts = table.rows().structuralFacts(); long n = table.rows().size();
        long best = PlanEstimate.add(tableFacts.nodeCount(), PlanEstimate.mul(n, 1 + PlanEstimate.operations(predicate, constants, meter))); Selection selected = null;
        for (Engine.Index index : preferred == null ? table.indexes() : List.of(preferred)) {
            if (!StoredDefinitions.parsed(index) || index.kind() != 0 || !CollationCompatibility.index(index, meter) || (!IndexDependencies.matches(index.timezoneDependencies(), meter.environment()) || !HostDependencies.matches(index.hostDependencies(), meter.environment()))) continue;
            if (index.predicate() != null) {
                if (!PredicateImplication.normalized(normalize(predicate, names, alias), normalize(index.predicate(), table.columns().stream().map(Engine.Column::name).toList(), table.name()), constants, meter)) continue;
            }
            for (boolean sets : new boolean[]{false, true}) {
            SqlParser.Expr access = null; boolean usable = true;
            for (Engine.IndexKey key : index.keys()) {
                SqlParser.Expr expected = key.column() >= 0 ? new SqlParser.ColumnSlot(key.column()) : normalize(key.expression(), table.columns().stream().map(Engine.Column::name).toList(), table.name());
                SqlParser.Expr matched = null;
                for (SqlParser.Expr term : terms) {
                    if (term instanceof SqlParser.Binary binary && binary.op().equals("=")) {
                        SqlParser.Expr other = normalize(binary.left(), names, alias).equals(expected) ? binary.right() : normalize(binary.right(), names, alias).equals(expected) ? binary.left() : null;
                        if (other != null && !(normalize(other, names, alias) instanceof SqlParser.ColumnSlot) && constant(other, key.type(), constants) != null) { matched = term; break; }
                    }
                    if (sets && term instanceof SqlParser.Quantified quantified && (quantified.query() == null || constants.queryEnvironment() != null && constants.queryEnvironment().hasPlanInit(term)) && quantified.operator().equals("=") && !quantified.quantifier().equals("all") && normalize(quantified.subject(), names, alias).equals(expected)) matched = term;
                    if (sets && term instanceof SqlParser.In in && (in.query() == null || constants.queryEnvironment() != null && constants.queryEnvironment().hasPlanInit(term)) && !in.negated() && normalize(in.subject(), names, alias).equals(expected)) matched = term;
                    if (sets && term instanceof SqlParser.Binary union && union.op().equals("or") && intervalSyntax(term, expected, names, alias)) matched = term;
                }
                if (matched == null) {
                    for (SqlParser.Expr term : terms) {
                        Comparison comparison = comparison(term, expected, names, alias);
                        if (comparison == null || comparison.operator().equals("=")) continue;
                        ScalarPlan.Expression resolved = constant(comparison.value(), key.type(), constants);
                        if (resolved != null && TableScan.keyCompatible(resolved.type(), key.type()) && CollationDerivation.merge(CollationDerivation.implicit(key.collation()), resolved.collation()).require().equals(key.collation())) access = FilterRules.and(access, term);
                    }
                    usable = access != null; break;
                }
                access = FilterRules.and(access, matched);
                if (sets && interval(matched)) for (SqlParser.Expr term : terms) {
                    Comparison clip = comparison(term, expected, names, alias);
                    if (clip == null) continue;
                    ScalarPlan.Expression resolved = constant(clip.value(), key.type(), constants);
                    if (resolved != null && TableScan.keyCompatible(resolved.type(), key.type())) access = FilterRules.and(access, term);
                }
            }
            boolean ordinary = FilterRules.terms(access).stream().anyMatch(term -> term instanceof SqlParser.Binary binary && List.of("=", "<", "<=", ">", ">=").contains(binary.op()));
            if (!sets && allowPartialFull && index.predicate() != null && !ordinary) {
                SqlParser.Expr partial = WindowSyntax.map(normalize(index.predicate(), table.columns().stream().map(Engine.Column::name).toList(), table.name()), node -> node instanceof SqlParser.ColumnSlot slot ? new SqlParser.Name(List.of(alias, names.get(slot.index()))) : node);
                long rows = PlanEstimate.rows(partial, n, List.of());
                long pages = PlanEstimate.add(index.entries().structuralFacts().nodeCount(), PlanEstimate.mul(rows, tableFacts.height()));
                long cost = PlanEstimate.add(pages, PlanEstimate.mul(rows, 1 + PlanEstimate.operations(predicate, constants, meter)));
                if (candidates != null) candidates.add(new Selection(index, null, rows, pages, cost));
                if (preferred != null && selected == null || cost < best || cost == best && earlier(index, null, selected)) {
                    selected = new Selection(index, null, rows, pages, cost); best = cost;
                }
            }
            if (!usable && index.predicate() == null) continue;
            List<String> unique = index.unique() && index.keys().size() == 1 && index.keys().getFirst().column() >= 0 ? List.of(names.get(index.keys().getFirst().column())) : List.of();
            if (access == null || sets && !interval(access)) continue;
            long rows = partialRows(index, access, names, alias, table, statistics.rows(access, n, unique)); var facts = index.entries().structuralFacts();
            long pages = rows == 0 ? 0 : Math.min(facts.nodeCount(), Math.max(facts.height(), partialRows(index, access, names, alias, table, statistics.rows(access, facts.nodeCount(), unique))));
            pages = PlanEstimate.add(pages, PlanEstimate.mul(rows, tableFacts.height()));
            long cost = PlanEstimate.add(pages, PlanEstimate.mul(rows, 1 + PlanEstimate.operations(predicate, constants, meter)));
            if (candidates != null) candidates.add(new Selection(index, access, rows, pages, cost));
            if (preferred != null && selected == null || cost < best || cost == best && earlier(index, access, selected)) { selected = new Selection(index, access, rows, pages, cost); best = cost; }
        }
        }
        return selected;
    }
    private static boolean earlier(Engine.Index index, SqlParser.Expr access, Selection previous) {
        if (previous == null) return true;
        int rank = interval(access) ? 1 : 0, prior = interval(previous.predicate()) ? 1 : 0;
        return rank < prior || rank == prior && SqlText.compare(index.name(), previous.index().name()) < 0;
    }
    static boolean interval(SqlParser.Expr access) {
        return FilterRules.terms(access).stream().anyMatch(term -> term instanceof SqlParser.In || term instanceof SqlParser.Quantified || term instanceof SqlParser.Binary binary && binary.op().equals("or"));
    }
    private static long partialRows(Engine.Index index, SqlParser.Expr access, List<String> names, String alias, Engine.Table table, long rows) {
        if (index.predicate() == null) return rows;
        java.util.Set<Integer> constrained = new java.util.HashSet<>();
        WindowSyntax.map(normalize(access, names, alias), node -> { if (node instanceof SqlParser.ColumnSlot slot) constrained.add(slot.index()); return node; });
        for (SqlParser.Expr term : FilterRules.terms(normalize(index.predicate(), table.columns().stream().map(Engine.Column::name).toList(), table.name()))) {
            boolean[] overlaps = {false};
            WindowSyntax.map(term, node -> { if (node instanceof SqlParser.ColumnSlot slot && constrained.contains(slot.index())) overlaps[0] = true; return node; });
            if (!overlaps[0]) rows = PlanEstimate.rows(term, rows, List.of());
        }
        return rows;
    }
    static Bound find(Engine.Table table, List<String> names, String alias, SqlParser.Expr predicate, ScalarPlan.Resolver constants, CostMeter meter) throws IOException {
        return find(table, names, alias, predicate, constants, meter, (Engine.Index)null);
    }
    static Bound find(Engine.Table table, List<String> names, String alias, SqlParser.Expr predicate, ScalarPlan.Resolver constants, CostMeter meter, Engine.Index preferred) throws IOException {
        return find(table, names, alias, predicate, constants, meter, preferred, true);
    }
    static Bound find(Engine.Table table, List<String> names, String alias, SqlParser.Expr predicate, ScalarPlan.Resolver constants, CostMeter meter, Engine.Index preferred, boolean allowPartialFull) throws IOException {
        Selection selection = select(table, names, alias, predicate, meter, preferred, constants, allowPartialFull);
        return find(table, names, alias, predicate, constants, meter, selection);
    }
    static Bound find(Engine.Table table, List<String> names, String alias, SqlParser.Expr predicate, ScalarPlan.Resolver constants, CostMeter meter, Selection selection) throws IOException {
        if (selection == null) return null;
        if (selection.predicate() == null) return new Bound(selection.index(), List.of(new Range(new byte[0], null)), selection.pages(), selection.rows());
        List<SqlParser.Expr> terms = new ArrayList<>(); conjuncts(selection.predicate(), terms);
        Bound chosen = null;
        for (Engine.Index index : List.of(selection.index())) {
            if (!StoredDefinitions.parsed(index) || index.kind() != 0 || !CollationCompatibility.index(index, meter) || (!IndexDependencies.matches(index.timezoneDependencies(), meter.environment()) || !HostDependencies.matches(index.hostDependencies(), meter.environment()))) continue;
            if (index.predicate() != null) {
                if (!PredicateImplication.normalized(normalize(predicate, names, alias), normalize(index.predicate(), table.columns().stream().map(Engine.Column::name).toList(), table.name()), constants, meter)) continue;
            }
            List<byte[]> prefixes = new ArrayList<>(); prefixes.add(new byte[0]);
            List<Range> ranges = null; boolean consumed = false;
            for (Engine.IndexKey key : index.keys()) {
                SqlParser.Expr expected = key.column() >= 0 ? new SqlParser.ColumnSlot(key.column()) : normalize(key.expression(), table.columns().stream().map(Engine.Column::name).toList(), table.name());
                ScalarPlan.Expression value = null;
                List<Comparison> comparisons = new ArrayList<>();
                for (SqlParser.Expr term : terms) {
                    Comparison comparison = comparison(term, expected, names, alias);
                    if (comparison == null) continue;
                    ScalarPlan.Expression resolved = constant(comparison.value(), key.type(), constants);
                    if (resolved == null || !comparison.operator().equals("=") && !TableScan.keyCompatible(resolved.type(), key.type())) continue;
                    comparisons.add(comparison);
                    if (comparison.operator().equals("=")) { value = resolved; break; }
                }
                List<byte[]> slots = new ArrayList<>();
                if (value != null) {
                    ScalarPlan.Expression resolved = value; SqlValue constant = meter.unmetered(() -> resolved.run(meter));
                    if (constant.isNull() || constant.type().isInteger() && key.type().isInteger() && !key.type().contains(constant.asInteger())) { prefixes = List.of(); consumed = true; break; }
                    if (!constant.type().equals(key.type()) && constant.type().isInteger() && key.type().isInteger()) constant = SqlValue.ofInteger(key.type(), constant.asInteger());
                    constant = TableScan.keyValue(constant, key.type());
                    if (constant == null) { prefixes = List.of(); consumed = true; break; }
                    slots.add(Indexes.slot(constant, key, meter.environment()));
                } else {
                    List<KeyIntervals.Interval> points = key.column() < 0 ? null : KeyIntervals.points(selection.predicate(), names.get(key.column()), alias, key.type(), constants, meter, key.collation());
                    if (points != null && points.stream().allMatch(point -> point.point() != null)) {
                        for (KeyIntervals.Interval point : points) {
                            byte[] slot = new byte[point.point().length + 1]; System.arraycopy(point.point(), 0, slot, 1, point.point().length);
                            if (key.descending()) for (int i = 0; i < slot.length; i++) slot[i] = (byte)~slot[i];
                            if (key.nullsFirst() != key.descending()) slot[0] ^= 1;
                            slots.add(slot);
                        }
                    } else if (points != null) {
                        ranges = new ArrayList<>();
                        for (byte[] prefix : prefixes) for (KeyIntervals.Interval interval : points) {
                            byte marker = (byte)(key.descending() ? 255 : 0); if (key.nullsFirst() != key.descending()) marker ^= 1;
                            byte[] present = concat(prefix, new byte[]{marker});
                            byte[] lower = intervalBoundary(present, key.descending() ? interval.upper() : interval.lower(), key.descending(), false);
                            byte[] upper = intervalBoundary(present, key.descending() ? interval.lower() : interval.upper(), key.descending(), true);
                            if (lower != null && (upper == null || Arrays.compareUnsigned(lower, upper) < 0)) ranges.add(new Range(lower, upper));
                        }
                        consumed = true; break;
                    } else if (!comparisons.isEmpty()) {
                        ranges = new ArrayList<>();
                        for (byte[] prefix : prefixes) {
                            byte marker = (byte)(key.descending() ? 255 : 0); if (key.nullsFirst() != key.descending()) marker ^= 1;
                            byte[] nonnull = concat(prefix, new byte[]{marker}), lower = nonnull, upper = Indexes.after(nonnull);
                            for (Comparison comparison : comparisons) {
                                ScalarPlan.Expression resolved = constant(comparison.value(), key.type(), constants);
                                if (!CollationDerivation.merge(CollationDerivation.implicit(key.collation()), resolved.collation()).require().equals(key.collation())) continue;
                                SqlValue constant = meter.unmetered(() -> resolved.run(meter));
                                if (constant.isNull()) { lower = upper; break; }
                                constant = TableScan.keyValue(constant, key.type());
                                if (constant == null) continue;
                                byte[] bound = concat(prefix, Indexes.slot(constant, key, meter.environment()));
                                String operator = key.descending() ? reverse(comparison.operator()) : comparison.operator();
                                if (operator.equals(">") || operator.equals(">=")) {
                                    byte[] candidate = operator.equals(">") ? Indexes.after(bound) : bound;
                                    if (candidate == null) { lower = upper; break; }
                                    if (Arrays.compareUnsigned(candidate, lower) > 0) lower = candidate;
                                } else {
                                    byte[] candidate = operator.equals("<=") ? Indexes.after(bound) : bound;
                                    if (candidate != null && (upper == null || Arrays.compareUnsigned(candidate, upper) < 0)) upper = candidate;
                                }
                            }
                            if (lower != null && (upper == null || Arrays.compareUnsigned(lower, upper) < 0)) ranges.add(new Range(lower, upper));
                        }
                        consumed = true; break;
                    } else break;
                }
                if ((long)prefixes.size() * slots.size() > 65536) break;
                List<byte[]> product = new ArrayList<>();
                for (byte[] prefix : prefixes) for (byte[] slot : slots) product.add(concat(prefix, slot));
                prefixes = product; consumed = true;
            }
            if (!consumed && index.predicate() == null) continue;
            if (ranges == null) { ranges = new ArrayList<>(); for (byte[] prefix : prefixes) ranges.add(new Range(prefix, Indexes.after(prefix))); }
            ranges.sort((a,b) -> Arrays.compareUnsigned(a.lower(), b.lower()));
            List<Range> merged = new ArrayList<>();
            for (Range range : ranges) {
                if (merged.isEmpty()) { merged.add(range); continue; }
                Range previous = merged.getLast();
                if (previous.upper() == null || Arrays.compareUnsigned(previous.upper(), range.lower()) >= 0) {
                    byte[] upper = previous.upper() == null || range.upper() == null ? null : Arrays.compareUnsigned(previous.upper(), range.upper()) >= 0 ? previous.upper() : range.upper();
                    merged.set(merged.size() - 1, new Range(previous.lower(), upper));
                } else merged.add(range);
            }
            chosen = new Bound(index, List.copyOf(merged), selection.pages(), selection.rows());
        }
        return chosen;
    }
    private static byte[] intervalBoundary(byte[] prefix, byte[] raw, boolean descending, boolean upper) {
        if (raw == null) return upper ? Indexes.after(prefix) : prefix;
        byte[] encoded = raw.clone();
        if (descending) for (int i = 0; i < encoded.length; i++) encoded[i] = (byte)~encoded[i];
        byte[] boundary = concat(prefix, encoded);
        return descending ? Indexes.after(boundary) : boundary;
    }
    private static boolean intervalSyntax(SqlParser.Expr syntax, SqlParser.Expr key, List<String> names, String alias) {
        if (syntax instanceof SqlParser.Binary binary && (binary.op().equals("and") || binary.op().equals("or"))) return intervalSyntax(binary.left(), key, names, alias) && intervalSyntax(binary.right(), key, names, alias);
        if (syntax instanceof SqlParser.Between b && !b.negated()) return normalize(b.subject(), names, alias).equals(key);
        if (syntax instanceof SqlParser.In in && in.query() == null && !in.negated()) return normalize(in.subject(), names, alias).equals(key);
        if (syntax instanceof SqlParser.Quantified q && q.query() == null && q.operator().equals("=") && !q.quantifier().equals("all")) return normalize(q.subject(), names, alias).equals(key);
        return comparison(syntax, key, names, alias) != null;
    }
    private record Comparison(String operator, SqlParser.Expr value) {}
    private static Comparison comparison(SqlParser.Expr expression, SqlParser.Expr key, List<String> names, String alias) {
        if (!(expression instanceof SqlParser.Binary binary) || !List.of("=", "<", "<=", ">", ">=").contains(binary.op())) return null;
        if (normalize(binary.left(), names, alias).equals(key) && !(normalize(binary.right(), names, alias) instanceof SqlParser.ColumnSlot)) return new Comparison(binary.op(), binary.right());
        if (normalize(binary.right(), names, alias).equals(key) && !(normalize(binary.left(), names, alias) instanceof SqlParser.ColumnSlot)) return new Comparison(reverse(binary.op()), binary.left());
        return null;
    }
    private static String reverse(String operator) { return switch (operator) { case "<" -> ">"; case "<=" -> ">="; case ">" -> "<"; case ">=" -> "<="; default -> operator; }; }
    private static ScalarPlan.Expression constant(SqlParser.Expr syntax, SqlType type, ScalarPlan.Resolver constants) {
        SqlParser.Literal literal = DefaultExpression.literal(syntax);
        if (literal != null) {
            if (literal.kind() == Lexer.Kind.INTEGER && !type.isInteger() && !type.isFloat() || literal.kind() == Lexer.Kind.DECIMAL && type.kind() != SqlType.Kind.DECIMAL && !type.isFloat()) return null;
            syntax = literal;
        }
        if (!(syntax instanceof SqlParser.Literal || syntax instanceof SqlParser.Parameter || syntax instanceof SqlParser.Name || syntax instanceof SqlParser.StoredValue)) return null;
        try { ScalarPlan.Expression value = constants.resolve(syntax, syntax instanceof SqlParser.Parameter ? null : type); if (syntax instanceof SqlParser.Parameter && value.type() == SqlType.UNKNOWN) value = constants.resolve(syntax, type); return (syntax instanceof SqlParser.Name || value.constant()) && (TableScan.keyCompatible(value.type(), type) || value.type() == SqlType.UNKNOWN) ? value : null; }
        catch (SqlException failure) { if (List.of("42703", "42P01").contains(failure.sqlState())) return null; throw failure; }
    }
    private static byte[] concat(byte[] a, byte[] b) { byte[] result = Arrays.copyOf(a, a.length + b.length); System.arraycopy(b, 0, result, a.length, b.length); return result; }
    private static int operations(SqlParser.Expr expression) {
        if (expression == null) return 0; expression = DefaultExpression.unwrap(expression);
        int count = expression instanceof SqlParser.Name || expression instanceof SqlParser.Literal ? 0 : 1;
        if (expression instanceof SqlParser.Quantified quantified && quantified.array() instanceof SqlParser.ArrayConstructor array) count += array.values().size();
        for (SqlParser.Expr child : DefaultExpression.children(expression)) count += operations(child);
        return count;
    }
    private static SqlParser.Expr normalize(SqlParser.Expr expression, List<String> names, String alias) {
        expression = DefaultExpression.unwrap(expression);
        return WindowSyntax.map(expression, node -> {
            if (node instanceof SqlParser.Name name && (name.parts().size() == 1 || name.parts().size() == 2 && name.parts().getFirst().equals(alias))) {
                int index = names.indexOf(name.parts().getLast()); if (index >= 0) return new SqlParser.ColumnSlot(index);
            }
            return node;
        });
    }
    private static void conjuncts(SqlParser.Expr expression, List<SqlParser.Expr> result) {
        expression = DefaultExpression.unwrap(expression);
        if (expression instanceof SqlParser.Binary binary && binary.op().equals("and")) { conjuncts(binary.left(), result); conjuncts(binary.right(), result); }
        else if (expression instanceof SqlParser.Between between && !between.negated()) {
            result.add(new SqlParser.Binary(">=", between.subject(), between.lower())); result.add(new SqlParser.Binary("<=", between.subject(), between.upper()));
        } else if (expression != null) result.add(expression);
    }
}
