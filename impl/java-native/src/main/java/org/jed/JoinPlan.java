package org.jed;

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashSet;
import java.util.List;
import java.util.Set;

/** Authored-order relation join with bounded inputs, candidate replay, and outer extensions. */
final class JoinPlan implements RelationPlan {
    record Pair(int left, int right) {}
    private final QueryPlan left, right;
    private final PlanMetrics metrics = new PlanMetrics();
    private JoinedStatistics statistics;
    JoinPlan statistics(JoinedStatistics facts) { statistics = facts; return this; }
    public void requireAnalyze() { left.requireAnalyze(); right.requireAnalyze(); }
    public PlanNode actualPlan(boolean verbose) {
        PlanNode node = explain(verbose);
        return new PlanNode(node.label(), node.detail(), node.rows(), metrics.total(), List.of(left.actualPlan(verbose), right.actualPlan(verbose)), node.lane());
    }
    private final List<ScalarPlan.Binding> leftBindings, rightBindings, bindings;
    private final List<SqlType> leftTypes, rightTypes;
    private final List<String> labels;
    private final List<Pair> merged = new ArrayList<>(), hashKeys = new ArrayList<>();
    private final ScalarPlan.Frame frame;
    private final ScalarPlan.Expression on;
    private final SqlParser.Expr onMetadata;
    private final ScalarPlan.Resolver resolverMetadata;
    private final QueryEnvironment environmentMetadata;
    private final String kind;
    private final Path scratch;
    private final long workMem;
    private final boolean hash;
    private final boolean[] predicateTouched;
    private boolean[] touched;
    private RowSpool leftRows, rightRows;
    private RowSpool.Cursor leftCursor, rightCursor;
    private HashRows hashRows;
    private HashRows.Cursor hashCursor;
    private DistinctState matchedRight;
    private List<SqlValue> currentLeft;
    private byte[] currentKey;
    private long rightOrdinal;
    private boolean started, closed, matchedLeft, trailingRight, borrowedLeft;
    private boolean retainedByParent, exhausted;
    private int residentWidth;
    private long intermediatePadding;
    private MemoryAccount memory;
    private void retainForParent(int width) {
        retainedByParent = true; residentWidth = width;
        if (left instanceof JoinPlan child) child.retainForParent(width);
    }
    private Row exhausted() {
        exhausted = true;
        if (!retainedByParent) close();
        return null;
    }
    JoinPlan borrowLeft() { if (started) throw new IllegalStateException("join already started"); borrowedLeft = true; return this; }

    JoinPlan(QueryPlan left, List<ScalarPlan.Binding> leftBindings, QueryPlan right, List<ScalarPlan.Binding> rightBindings,
             SqlParser.Join syntax, SqlValue[] args, QueryEnvironment environment, Path scratch, long workMem, boolean hash) {
        this.frame = new ScalarPlan.Frame();
        this.left = left; this.right = right; this.leftBindings = List.copyOf(leftBindings); this.rightBindings = List.copyOf(rightBindings);
        this.leftTypes = left.types(); this.rightTypes = right.types(); this.kind = syntax.kind(); this.scratch = scratch; this.workMem = workMem;
        List<ScalarPlan.Binding> raw = new ArrayList<>(leftBindings); raw.addAll(rightBindings);
        List<String> using = new ArrayList<>(syntax.using());
        if (syntax.natural()) for (ScalarPlan.Binding binding : leftBindings) if (!binding.qualifiedOnly() && !using.contains(binding.name()) && rightBindings.stream().anyMatch(other -> !other.qualifiedOnly() && other.name().equals(binding.name()))) using.add(binding.name());
        if (kind.equals("full") && !using.isEmpty()) throw new SqlException("0A000", "FULL JOIN USING is not supported");
        Set<String> unique = new HashSet<>();
        List<ScalarPlan.Binding> synthetic = new ArrayList<>();
        for (int i = 0; i < raw.size(); i++) synthetic.add(new ScalarPlan.Binding(List.of(), "$join" + i, raw.get(i).type(), false, false, raw.get(i).collation()));
        SqlParser.Expr predicate = syntax.on();
        for (String name : using) {
            if (!unique.add(name)) throw new SqlException("42701", "duplicate USING column");
            int a = named(leftBindings, name), b = named(rightBindings, name); merged.add(new Pair(a, b));
            SqlParser.Expr equality = new SqlParser.Binary("=", new SqlParser.Name(List.of("$join" + a)), new SqlParser.Name(List.of("$join" + (leftBindings.size() + b))));
            predicate = predicate == null ? equality : new SqlParser.Binary("and", predicate, equality);
        }
        if (AggregatePlan.contains(predicate)) throw new SqlException("42803", "aggregate in JOIN condition");
        ScalarPlan.Resolver resolver = environment == null ? new ScalarPlan.Resolver(args, using.isEmpty() ? raw : synthetic, frame) : environment.resolver(using.isEmpty() ? raw : synthetic, frame);
        onMetadata = predicate; resolverMetadata = resolver; environmentMetadata = environment;
        on = predicate == null ? null : predicate instanceof SqlParser.Binary binary && binary.op().equals("and") ? resolver.binary(binary, true) : resolver.resolve(predicate, SqlType.BOOLEAN);
        if (on != null && on.type() != SqlType.BOOLEAN) throw SqlException.typeMismatch("JOIN condition requires boolean");
        predicateTouched = resolver.touched();
        this.hash = hash && (kind.equals("inner") || kind.equals("left")) && hashKeys(predicate, using.isEmpty() ? raw : synthetic, leftBindings.size(), hashKeys);
        bindings = outputBindings(syntax, leftBindings, rightBindings); labels = bindings.stream().map(ScalarPlan.Binding::name).toList();
        touched = new boolean[bindings.size()]; Arrays.fill(touched, true);
        residentWidth = bindings.size();
        if ((scratch == null || workMem == 0) && left instanceof JoinPlan child) child.retainForParent(residentWidth);
    }
    /** Sequential lateral probes share resolved expressions, but own every execution buffer. */
    JoinPlan probeExecution(QueryPlan probeLeft, QueryPlan probeRight) {
        if (hash || started) throw new IllegalStateException("probe template must be an unused nested-loop shape");
        return new JoinPlan(this, probeLeft, probeRight);
    }
    private JoinPlan(JoinPlan template, QueryPlan probeLeft, QueryPlan probeRight) {
        left = probeLeft; right = probeRight;
        leftBindings = template.leftBindings; rightBindings = template.rightBindings; bindings = template.bindings;
        leftTypes = template.leftTypes; rightTypes = template.rightTypes; labels = template.labels;
        merged.addAll(template.merged); hashKeys.addAll(template.hashKeys);
        frame = template.frame; on = template.on; onMetadata = template.onMetadata;
        resolverMetadata = template.resolverMetadata; environmentMetadata = template.environmentMetadata;
        kind = template.kind; scratch = template.scratch; workMem = template.workMem; hash = false;
        predicateTouched = template.predicateTouched; touched = template.touched.clone();
        residentWidth = template.residentWidth; statistics = template.statistics;
    }
    /** WHERE resolves against merged USING columns, unlike the raw ON namespace. */
    static List<ScalarPlan.Binding> outputBindings(SqlParser.Join syntax, List<ScalarPlan.Binding> leftBindings, List<ScalarPlan.Binding> rightBindings) {
        List<ScalarPlan.Binding> raw = new ArrayList<>(leftBindings); raw.addAll(rightBindings);
        List<String> using = new ArrayList<>(syntax.using());
        if (syntax.natural()) for (ScalarPlan.Binding binding : leftBindings) if (!binding.qualifiedOnly() && !using.contains(binding.name()) && rightBindings.stream().anyMatch(other -> !other.qualifiedOnly() && other.name().equals(binding.name()))) using.add(binding.name());
        List<Pair> merged = new ArrayList<>();
        for (String name : using) merged.add(new Pair(named(leftBindings, name), named(rightBindings, name)));
        String kind = syntax.kind();
        List<ScalarPlan.Binding> output = new ArrayList<>();
        for (int i = 0; i < merged.size(); i++) {
            Pair pair = merged.get(i); ScalarPlan.Binding binding = kind.equals("right") ? rightBindings.get(pair.right) : leftBindings.get(pair.left);
            output.add(new ScalarPlan.Binding(List.of(), using.get(i), binding.type(), false, false, binding.collation()));
        }
        for (int i = 0; i < raw.size(); i++) {
            ScalarPlan.Binding binding = raw.get(i); int position = i;
            boolean hidden = binding.qualifiedOnly() || merged.stream().anyMatch(pair -> position == pair.left || position == leftBindings.size() + pair.right);
            output.add(binding.hidden(hidden).extendNulls(kind.equals("full") || (position < leftBindings.size() ? kind.equals("right") : kind.equals("left"))));
        }
        return List.copyOf(output);
    }
    public List<FunctionalKey> functionalKeys() {
        List<FunctionalKey> result = new ArrayList<>();
        if (left instanceof RelationPlan relation) for (FunctionalKey key : relation.functionalKeys()) result.add(shift(key, merged.size()));
        if (right instanceof RelationPlan relation) for (FunctionalKey key : relation.functionalKeys()) result.add(shift(key, merged.size() + leftBindings.size()));
        return result;
    }
    private static FunctionalKey shift(FunctionalKey key, int offset) {
        return new FunctionalKey(key.determinant().stream().map(index -> index + offset).toList(), key.dependent().stream().map(index -> index + offset).toList());
    }
    static SqlParser.Expr condition(SqlParser.Join syntax, List<ScalarPlan.Binding> left, List<ScalarPlan.Binding> right) {
        SqlParser.Expr result = syntax.on();
        List<String> names = syntax.natural() ? left.stream().filter(binding -> !binding.qualifiedOnly() && right.stream().anyMatch(r -> !r.qualifiedOnly() && r.name().equals(binding.name()))).map(ScalarPlan.Binding::name).distinct().toList() : syntax.using();
        for (String name : names) {
            var a = left.get(named(left, name)); var b = right.get(named(right, name));
            List<String> x = new ArrayList<>(a.qualifier()), y = new ArrayList<>(b.qualifier()); x.add(a.name()); y.add(b.name());
            result = FilterRules.and(result, new SqlParser.Binary("=", new SqlParser.Name(x), new SqlParser.Name(y)));
        }
        return result;
    }
    static boolean hashEligible(SqlParser.Join syntax, List<ScalarPlan.Binding> left, List<ScalarPlan.Binding> right) {
        if (!(syntax.kind().equals("inner") || syntax.kind().equals("left"))) return false;
        if (syntax.natural() || !syntax.using().isEmpty()) {
            List<String> names = syntax.natural() ? left.stream().filter(binding -> !binding.qualifiedOnly() && right.stream().anyMatch(r -> !r.qualifiedOnly() && r.name().equals(binding.name()))).map(ScalarPlan.Binding::name).distinct().toList() : syntax.using();
            if (names.isEmpty()) return false;
            for (String name : names) { SqlType a = left.get(named(left, name)).type(), b = right.get(named(right, name)).type(); if (a.kind() != b.kind() || !keyable(a)) return false; }
            return true;
        }
        List<ScalarPlan.Binding> both = new ArrayList<>(left); both.addAll(right);
        return hashKeys(syntax.on(), both, left.size(), new ArrayList<>());
    }
    static long hashWork(SqlParser.Join syntax, List<ScalarPlan.Binding> left, List<ScalarPlan.Binding> right, long leftRows, long rightRows) {
        return hashWork(syntax, left, right, leftRows, rightRows, null);
    }
    static long hashWork(SqlParser.Join syntax, List<ScalarPlan.Binding> left, List<ScalarPlan.Binding> right, long leftRows, long rightRows, JoinedStatistics statistics) {
        List<ScalarPlan.Binding> both = new ArrayList<>(left); both.addAll(right); List<Pair> keys = new ArrayList<>();
        if (syntax.natural() || !syntax.using().isEmpty()) {
            List<String> names = syntax.natural() ? left.stream().filter(binding -> !binding.qualifiedOnly() && right.stream().anyMatch(r -> !r.qualifiedOnly() && r.name().equals(binding.name()))).map(ScalarPlan.Binding::name).distinct().toList() : syntax.using();
            for (String name : names) keys.add(new Pair(named(left, name), named(right, name)));
        } else hashKeys(syntax.on(), both, left.size(), keys);
        return hashWork(leftRows, rightRows, syntax.on(), keys, left, syntax.kind().equals("left"), statistics == null ? null : statistics.rows(hashPredicate(keys, left, right), PlanEstimate.mul(leftRows, rightRows)));
    }
    private static SqlParser.Expr hashPredicate(List<Pair> keys, List<ScalarPlan.Binding> left, List<ScalarPlan.Binding> right) {
        SqlParser.Expr result = null;
        for (Pair key : keys) result = FilterRules.and(result, new SqlParser.Binary("=", hashColumn(left.get(key.left()), key.left()), hashColumn(right.get(key.right()), left.size() + key.right())));
        return result;
    }
    private static SqlParser.Expr hashColumn(ScalarPlan.Binding binding, int index) {
        if (binding.qualifier().isEmpty()) return new SqlParser.ColumnSlot(index);
        List<String> parts = new ArrayList<>(binding.qualifier()); parts.add(binding.name()); return new SqlParser.Name(parts);
    }
    private static long hashWork(long leftRows, long rightRows, SqlParser.Expr condition, List<Pair> keys, List<ScalarPlan.Binding> left, boolean preserveLeft, Long estimatedCandidates) {
        long width = 0;
        for (Pair key : keys) width = PlanEstimate.add(width, Math.max(EstimatorFacts.DEFAULT_VARIABLE_KEY_BYTES, left.get(key.left).type().kind() == SqlType.Kind.DECIMAL ? 0 : Encoding.fixedWidth(left.get(key.left).type())));
        long candidates = PlanEstimate.mul(leftRows, rightRows);
        if (estimatedCandidates != null) candidates = estimatedCandidates;
        else for (int i = 0; i < keys.size(); i++) candidates = PlanEstimate.scale(candidates, EstimatorFacts.EQUALITY);
        if (preserveLeft) candidates = Math.max(candidates, leftRows);
        long build = PlanEstimate.mul(rightRows, PlanEstimate.mul(width, Costs.HASH_BUILD));
        long probe = PlanEstimate.add(PlanEstimate.mul(leftRows, width), PlanEstimate.mul(candidates, PlanEstimate.add(width, keys.size() * 4L)));
        return PlanEstimate.add(build, PlanEstimate.add(PlanEstimate.mul(probe, Costs.HASH_PROBE), PlanEstimate.mul(candidates, PlanEstimate.operations(condition))));
    }
    static List<Pair> matchingKeys(SqlParser.Expr condition, List<ScalarPlan.Binding> target, List<ScalarPlan.Binding> source) {
        List<ScalarPlan.Binding> bindings = new ArrayList<>(target); bindings.addAll(source); List<Pair> keys = new ArrayList<>();
        return hashKeys(condition, bindings, target.size(), keys) ? List.copyOf(keys) : List.of();
    }
    private static boolean hashKeys(SqlParser.Expr expression, List<ScalarPlan.Binding> bindings, int boundary, List<Pair> keys) {
        List<SqlParser.Expr> terms = new ArrayList<>(); terms(expression, terms);
        if (terms.isEmpty()) return false;
        for (SqlParser.Expr term : terms) {
            if (!(term instanceof SqlParser.Binary binary) || !Set.of("=", "<>", "!=").contains(binary.op()) || !hashLeaf(binary.left()) || !hashLeaf(binary.right())) return false;
            int x = hashSlot(bindings, binary.left()), y = hashSlot(bindings, binary.right());
            if (x == -2 || y == -2) continue; if (x < 0 || y < 0) return false;
            if (binary.op().equals("=") && (x < boundary) != (y < boundary)) {
                SqlType first = bindings.get(x).type(), second = bindings.get(y).type();
                if (first.kind() != second.kind() || !keyable(first)) return false;
                keys.add(x < boundary ? new Pair(x, y - boundary) : new Pair(y, x - boundary));
            }
        }
        return !keys.isEmpty();
    }
    private static int hashSlot(List<ScalarPlan.Binding> bindings, SqlParser.Expr expression) {
        return expression instanceof SqlParser.Name name ? slot(bindings, name) : expression instanceof SqlParser.ColumnSlot column ? column.index() : -2;
    }
    private static boolean hashLeaf(SqlParser.Expr expression) { return expression instanceof SqlParser.ColumnSlot || expression instanceof SqlParser.Name || expression instanceof SqlParser.Literal || expression instanceof SqlParser.StoredValue || expression instanceof SqlParser.Parameter; }
    private static boolean keyable(SqlType type) { return switch (type.kind()) { case BOOLEAN, I16, I32, I64, F32, F64, TEXT, DECIMAL, BYTEA, UUID, DATE, TIMESTAMP, TIMESTAMPTZ, INTERVAL, JSONB -> true; case ARRAY, RANGE -> keyable(type.element()); case COMPOSITE, RECORD -> type.fields().stream().allMatch(field -> keyable(field.type())); default -> false; }; }
    private static void terms(SqlParser.Expr expression, List<SqlParser.Expr> terms) { if (expression instanceof SqlParser.Binary binary && binary.op().equals("and")) { terms(binary.left(), terms); terms(binary.right(), terms); } else if (expression != null) terms.add(expression); }
    private static int named(List<ScalarPlan.Binding> bindings, String name) {
        int found = -1;
        for (int i = 0; i < bindings.size(); i++) if (!bindings.get(i).qualifiedOnly() && bindings.get(i).name().equals(name)) { if (found >= 0) throw new SqlException("42702", "ambiguous USING column"); found = i; }
        if (found < 0) throw new SqlException("42703", "USING column does not exist: " + name); return found;
    }
    private static int slot(List<ScalarPlan.Binding> bindings, SqlParser.Name name) {
        int found = -1;
        for (int i = 0; i < bindings.size(); i++) {
            ScalarPlan.Binding binding = bindings.get(i);
            if (!binding.name().equals(name.parts().getLast())) continue;
            if (name.parts().size() == 1 ? binding.qualifiedOnly() : !binding.qualifier().equals(name.parts().subList(0, name.parts().size() - 1))) continue;
            if (found >= 0) return -1; found = i;
        }
        return found;
    }
    private List<SqlParser.Expr> prePushed = List.of();
    JoinPlan prePushed(List<SqlParser.Expr> terms) { prePushed = List.copyOf(terms); return this; }
    @Override public boolean push(SqlParser.Expr predicate) {
        if (prePushed.contains(predicate)) return true;
        if (!FilterRules.safe(predicate, environmentMetadata == null ? null : environmentMetadata.resolver(bindings, new ScalarPlan.Frame()))) return false;
        if ((kind.equals("inner") || kind.equals("cross") || kind.equals("left")) && left instanceof RelationPlan relation && FilterRules.belongs(predicate, leftBindings) && relation.pushInJoin(predicate)) return true;
        return (kind.equals("inner") || kind.equals("cross") || kind.equals("right")) && right instanceof RelationPlan relation && FilterRules.belongs(predicate, rightBindings) && relation.pushInJoin(predicate);
    }
    public boolean baseIsland() { return left instanceof RelationPlan a && right instanceof RelationPlan b && a.baseIsland() && b.baseIsland() && (kind.equals("inner") || kind.equals("cross")); }
    @Override public List<ScalarPlan.Binding> bindings() { return bindings; }
    @Override public boolean orderedBy(List<SqlParser.Order> order) {
        return (kind.equals("inner") || kind.equals("left") || kind.equals("cross")) && left instanceof RelationPlan relation && relation.orderedBy(order);
    }
    @Override public PlanNode explain(boolean verbose) {
        
        PlanNode a = left.explain(verbose), b = right.explain(verbose); long candidates = PlanEstimate.mul(a.rows(), b.rows());
        long rows = PlanEstimate.rows(onMetadata, candidates, List.of());
        if (statistics != null) rows = statistics.rows(onMetadata, candidates);
        else if (left instanceof TableRelation l && right instanceof TableRelation r && onMetadata instanceof SqlParser.Binary binary && binary.op().equals("=")) {
            StatisticsEstimate.Distribution ld = l.statistics().distribution(binary.left()), rd = r.statistics().distribution(binary.right());
            if (ld == null || rd == null) { ld = l.statistics().distribution(binary.right()); rd = r.statistics().distribution(binary.left()); }
            if (ld != null && rd != null) {
                rows = StatisticsEstimate.scale(ld.nonnull(), rd.nonnull(), Math.max(1, Math.max(ld.distinct(), rd.distinct())));
                rows = StatisticsEstimate.scale(rows, a.rows(), Math.max(1, l.statistics().population()));
                rows = StatisticsEstimate.scale(rows, b.rows(), Math.max(1, r.statistics().population()));
            }
        }
        if (kind.equals("left")) rows = Math.max(rows, a.rows());
        if (kind.equals("right")) rows = Math.max(rows, b.rows());
        if (kind.equals("full")) rows = Math.max(rows, Math.max(a.rows(), b.rows()));
        String detail = kind + (hash ? "; keys=" + hashKeys.size() : "");
        if (onMetadata != null) {
            List<ScalarPlan.Binding> raw = new ArrayList<>(leftBindings); raw.addAll(rightBindings);
            detail += verbose ? "; on=" + new ExplainExpression(raw, resolverMetadata, environmentMetadata.meter()).print(onMetadata) : "; on:conjuncts=" + FilterRules.terms(onMetadata).size();
        }
        long work = hash ? hashWork(a.rows(), b.rows(), onMetadata, hashKeys, leftBindings, kind.equals("left"), statistics == null ? null : statistics.rows(hashPredicate(hashKeys, leftBindings, rightBindings), candidates)) : PlanEstimate.mul(candidates, PlanEstimate.operations(onMetadata));
        if (environmentMetadata != null) work = PlanEstimate.add(work, environmentMetadata.hiddenEstimate(onMetadata, candidates));
        return new PlanNode(hash ? "Hash Join" : "Nested Loop", detail, rows, PlanEstimate.add(PlanEstimate.add(a.cost(), b.cost()), work), verbose ? List.of(PlanSlots.shift(a, merged.size()), PlanSlots.shift(b, merged.size() + leftBindings.size())) : List.of(a, b), "buffered");
    }
    @Override public List<String> names() { return labels; }
    @Override public List<SqlType> types() { return bindings.stream().map(ScalarPlan.Binding::type).toList(); }
    @Override public void touch(boolean[] mask) {
        if (mask.length != bindings.size()) throw new IllegalArgumentException("join touch width"); touched = mask.clone();
        boolean[] a = new boolean[leftTypes.size()], b = new boolean[rightTypes.size()];
        for (int i = 0; i < predicateTouched.length; i++) if (predicateTouched[i]) { if (i < a.length) a[i] = true; else b[i - a.length] = true; }
        for (int i = 0; i < a.length; i++) a[i] |= mask[merged.size() + i];
        for (int i = 0; i < b.length; i++) b[i] |= mask[merged.size() + a.length + i];
        for (int i = 0; i < merged.size(); i++) if (mask[i]) { Pair pair = merged.get(i); if (kind.equals("right")) b[pair.right] = true; else a[pair.left] = true; }
        if (left instanceof RelationPlan relation) relation.touch(a); if (right instanceof RelationPlan relation) relation.touch(b);
    }
    public void prepare(CostMeter meter) { if (!started) { long before = meter.cost(); try { initialize(meter); } finally { metrics.total(before, meter); } } }
    private void initialize(CostMeter meter) {
        started = true; memory = meter.memory();
        leftRows = new RowSpool(leftTypes, workMem, scratch, borrowedLeft ? null : meter.memory()); rightRows = new RowSpool(rightTypes, workMem, scratch, meter.memory());
        Row row; while ((row = left.next(meter)) != null) {
            leftRows.append(row.values());
            if ((scratch == null || workMem == 0) && left instanceof JoinPlan) {
                long padding = (long) Math.max(0, residentWidth - leftTypes.size()) * MemorySchedule.VALUE;
                memory.reserve(padding); intermediatePadding = MemoryAccount.add(intermediatePadding, padding);
            }
        }
        if (!(left instanceof JoinPlan child && child.retainedByParent)) left.close();
        while ((row = right.next(meter)) != null) rightRows.append(row.values()); right.close();
        leftCursor = leftRows.cursor();
        if (hash) {
            hashRows = new HashRows(scratch, workMem, meter.memory(), rightTypes.size());
            try (RowSpool.Cursor cursor = rightRows.cursor()) {
                List<SqlValue> values; while ((values = cursor.next()) != null) { byte[] key = key(values, false, meter); if (key != null) {
                    List<SqlValue> logicalKey = new ArrayList<>(); for (Pair pair : hashKeys) logicalKey.add(values.get(pair.right));
                    hashRows.append(key, values, MemoryAccount.add(MemoryAccount.row(values), DistinctState.entryBytes(logicalKey)));
                } }
            }
            hashRows.freeze(); rightRows.close();
        }
        if (kind.equals("right") || kind.equals("full")) matchedRight = new DistinctState(scratch, workMem, meter.memory());
    }
    @Override public Row next(CostMeter meter) {
        long before = meter.cost();
        try { return nextMeasured(meter); } finally { metrics.total(before, meter); }
    }
    private Row nextMeasured(CostMeter meter) {
        if (closed || exhausted) return null;
        if (!started) initialize(meter);
        while (true) {
            if (trailingRight) {
                List<SqlValue> candidate = rightCursor.next();
                if (candidate == null) return exhausted();
                long ordinal = rightOrdinal++;
                if (matchedRight.contains(List.of(SqlValue.ofInteger(ordinal)))) continue;
                return output(nulls(leftTypes), candidate);
            }
            if (currentLeft == null) {
                currentLeft = leftCursor.next();
                if (currentLeft == null) {
                    if (matchedRight == null) return exhausted();
                    trailingRight = true; rightOrdinal = 0; rightCursor = rightRows.cursor(); continue;
                }
                matchedLeft = false; rightOrdinal = 0;
                if (hash) { currentKey = key(currentLeft, true, meter); hashCursor = currentKey == null ? null : hashRows.probe(currentKey); }
                else rightCursor = rightRows.cursor();
            }
            List<SqlValue> candidate;
            if (hash) {
                HashRows.Entry entry = hashCursor == null ? null : hashCursor.next(); candidate = entry == null ? null : entry.row();
                if (entry != null) {
                    meter.charge(Math.max(1, Math.min(currentKey.length, entry.key().length)) * Costs.HASH_PROBE); meter.guard();
                    if (!Arrays.equals(currentKey, entry.key())) continue;
                }
            } else candidate = rightCursor.next();
            if (candidate != null) {
                long ordinal = rightOrdinal++;
                List<SqlValue> joined = new ArrayList<>(currentLeft); joined.addAll(candidate); frame.values = joined;
                if (on != null) { SqlValue matches = on.run(meter); if (matches.isNull() || !matches.asBoolean()) continue; }
                matchedLeft = true; if (matchedRight != null) matchedRight.add(List.of(SqlValue.ofInteger(ordinal)));
                return output(currentLeft, candidate);
            }
            if (hashCursor != null) { hashCursor.close(); hashCursor = null; }
            if (rightCursor != null) { rightCursor.close(); rightCursor = null; }
            List<SqlValue> finished = currentLeft; currentLeft = null;
            if (!matchedLeft && (kind.equals("left") || kind.equals("full"))) return output(finished, nulls(rightTypes));
        }
    }
    private Row output(List<SqlValue> a, List<SqlValue> b) {
        List<SqlValue> row = new ArrayList<>();
        for (Pair pair : merged) row.add(kind.equals("right") ? b.get(pair.right) : a.get(pair.left));
        row.addAll(a); row.addAll(b);
        for (int i = 0; i < row.size(); i++) if (!touched[i]) row.set(i, SqlValue.nullValue(bindings.get(i).type()));
        return new Row(labels, row);
    }
    private byte[] key(List<SqlValue> values, boolean probe, CostMeter meter) {
        boolean absent = false;
        try {
            ByteArrayOutputStream bytes = new ByteArrayOutputStream(); DataOutputStream out = new DataOutputStream(bytes);
            for (Pair pair : hashKeys) {
                SqlValue value = values.get(probe ? pair.left : pair.right);
                byte[] encoded = value.isNull() ? new byte[0] : Encoding.key(value);
                meter.charge(Math.max(1, encoded.length) * (probe ? Costs.HASH_PROBE : Costs.HASH_BUILD)); meter.guard();
                absent |= value.isNull(); out.writeInt(encoded.length); out.write(encoded);
            }
            out.flush(); return absent ? null : bytes.toByteArray();
        } catch (IOException impossible) { throw new AssertionError(impossible); }
    }
    private static List<SqlValue> nulls(List<SqlType> types) { return types.stream().map(SqlValue::nullValue).toList(); }
    @Override public void close() {
        if (closed) return; closed = true; left.close(); right.close();
        if (memory != null && intermediatePadding != 0) { memory.release(intermediatePadding); intermediatePadding = 0; }
        if (leftCursor != null) leftCursor.close(); if (rightCursor != null) rightCursor.close(); if (hashCursor != null) hashCursor.close();
        if (leftRows != null) leftRows.close(); if (rightRows != null) rightRows.close(); if (hashRows != null) hashRows.close(); if (matchedRight != null) matchedRight.close();
    }
}
