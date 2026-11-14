package org.jed;

import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashSet;
import java.util.List;
import java.util.Set;
import java.util.function.Function;

/** Earlier-sibling correlation with once-only independent sources and ordered per-row dependent sources. */
final class LateralJoinPlan implements RelationPlan {
    private final RelationPlan left;
    private final Function<QueryEnvironment, RelationPlan> factory;
    private final SqlParser.Join syntax;
    private final SqlValue[] args;
    private final QueryEnvironment environment;
    private final ScalarPlan.Frame frame = new ScalarPlan.Frame();
    private final ScalarPlan.Resolver scope;
    private final Path scratch;
    private final long workMem;
    private final boolean[] dependencies;
    private final JoinPlan independent, shape;
    private final Shape leftShape;
    private Shape rightShapeMetadata;
    private final List<ScalarPlan.Binding> bindings;
    private RelationPlan explainRight;
    private final List<SqlParser.Expr> rightFilters = new ArrayList<>();
    private boolean[] touched;
    private RowSpool rows;
    private RowSpool.Cursor cursor;
    private JoinPlan current;
    private RelationPlan currentRight;
    private final PlanMetrics metrics = new PlanMetrics();
    private long completedRightCost;
    private PlanNode lastRightActual;
    private boolean started, closed, measured;

    LateralJoinPlan(RelationPlan left, Function<QueryEnvironment, RelationPlan> factory, SqlParser.Join syntax,
                    SqlValue[] args, QueryEnvironment environment, Path scratch, long workMem) {
        this.left = left; this.factory = factory; this.syntax = syntax; this.args = args;
        this.environment = environment; this.scratch = scratch; this.workMem = workMem;
        ScalarPlan.Resolver localScope = environment.resolver(left.bindings(), frame);
        if (syntax.kind().equals("right") || syntax.kind().equals("full")) localScope.forbidLocal();
        scope = localScope.mergeEarlier(environment.lateralPrefix());
        RelationPlan right = factory.apply(environment.childLateral(scope));
        try {
            Set<List<String>> qualifiers = new HashSet<>();
            for (var binding : left.bindings()) if (!binding.qualifier().isEmpty()) qualifiers.add(binding.qualifier());
            for (var binding : right.bindings()) if (!binding.qualifier().isEmpty() && qualifiers.contains(binding.qualifier())) throw new SqlException("42712", "duplicate relation name");
            dependencies = localScope.touched();
            boolean dependent = false; for (boolean dependency : dependencies) dependent |= dependency;
            if (!syntax.kind().equals("right") && !syntax.kind().equals("full")) for (boolean dependency : scope.touched()) dependent |= dependency;
            if (dependent && (syntax.kind().equals("right") || syntax.kind().equals("full"))) throw new SqlException("42P10", "LATERAL reference requires INNER or LEFT JOIN");
            if (!dependent) {
                independent = new JoinPlan(left, left.bindings(), right, right.bindings(), syntax, args, environment, scratch, workMem,
                    syntax.kind().equals("left") && JoinPlan.hashEligible(syntax, left.bindings(), right.bindings()));
                shape = null; leftShape = null; bindings = independent.bindings();
            } else {
                independent = null;
                leftShape = new Shape(left.bindings(), null, left.functionalKeys());
                Shape rightShape = new Shape(right.bindings(), null, right.functionalKeys()); rightShapeMetadata = rightShape;
                shape = new JoinPlan(leftShape, leftShape.bindings(), rightShape, rightShape.bindings(), syntax, args, environment, scratch, workMem, false);
                bindings = shape.bindings(); explainRight = right; right.close();
            }
        } catch (RuntimeException | Error failure) { right.close(); throw failure; }
        touched = new boolean[bindings.size()]; Arrays.fill(touched, true);
    }
    public boolean baseIsland() { return left.baseIsland() && explainRight instanceof TableRelation && (syntax.kind().equals("inner") || syntax.kind().equals("cross")); }
    @Override public List<ScalarPlan.Binding> bindings() { return bindings; }
    @Override public List<FunctionalKey> functionalKeys() { return independent != null ? independent.functionalKeys() : shape.functionalKeys(); }
    @Override public boolean orderedBy(List<SqlParser.Order> order) {
        if (!List.of("inner", "left", "cross").contains(syntax.kind())) return false;
        if (left.orderedBy(order)) return true;
        if (!(left instanceof TableRelation table) || !(explainRight instanceof TableRelation right)) return false;
        for (int split = 1; split < order.size(); split++) if (table.orderedBy(order.subList(0, split), true) && right.orderedBy(order.subList(split, order.size()))) return true;
        return false;
    }
    @Override public PlanNode explain(boolean verbose) {
        if (independent != null) return independent.explain(verbose);
        PlanNode a = left.explain(verbose), b = explainRight.explain(verbose); long pairs = PlanEstimate.mul(a.rows(), b.rows());
        long rows = explainRight instanceof TableRelation ? pairs : PlanEstimate.rows(syntax.on(), pairs, List.of()); if (syntax.kind().equals("left")) rows = Math.max(rows, a.rows());
        List<ScalarPlan.Binding> raw = new ArrayList<>(left.bindings()); raw.addAll(explainRight.bindings());
        SqlParser.Expr condition = JoinPlan.condition(syntax, left.bindings(), explainRight.bindings());
        String detail = syntax.kind() + (condition == null ? "" : verbose ? "; on=" + new ExplainExpression(raw, environment.resolver(raw, new ScalarPlan.Frame()), environment.meter()).print(condition) : "; on:conjuncts=" + FilterRules.terms(condition).size());
        long rightCost = PlanEstimate.mul(a.rows(), b.cost());
        PlanNode repeated = repeated(b, a.rows());
        return new PlanNode("Nested Loop", detail, rows, PlanEstimate.add(a.cost(), PlanEstimate.add(rightCost, PlanEstimate.add(PlanEstimate.mul(pairs, PlanEstimate.operations(syntax.on())), environment.hiddenEstimate(syntax.on(), pairs)))), verbose ? List.of(a, PlanSlots.shift(repeated, left.bindings().size())) : List.of(a, repeated), "buffered");
    }
    private static PlanNode repeated(PlanNode node, long times) {
        return new PlanNode(node.label(), node.detail(), PlanEstimate.mul(node.rows(), times), PlanEstimate.mul(node.cost(), times), node.children().stream().map(child -> repeated(child, times)).toList(), node.lane());
    }
    private static List<PlanNode> actualChildren(PlanNode estimate, PlanNode actual) {
        List<PlanNode> children = new ArrayList<>();
        for (int i = 0; i < estimate.children().size(); i++) {
            PlanNode child = estimate.children().get(i), measured = actual == null || i >= actual.children().size() ? null : actual.children().get(i);
            children.add(new PlanNode(child.label(), child.detail(), child.rows(), measured == null ? 0 : measured.cost(), actualChildren(child, measured), child.lane()));
        }
        return children;
    }
    @Override public void requireAnalyze() {
        measured = true;
        if (independent != null) { independent.requireAnalyze(); return; }
        left.requireAnalyze();
        explainRight.requireAnalyze();
    }
    @Override public PlanNode actualPlan(boolean verbose) {
        if (independent != null) return independent.actualPlan(verbose);
        PlanNode estimate = explain(verbose), right = estimate.children().getLast();
        long rightCost = completedRightCost;
        if (currentRight != null) rightCost = PlanEstimate.add(rightCost, currentRight.actualPlan(verbose).cost());
        PlanNode last = currentRight == null ? lastRightActual : currentRight.actualPlan(verbose);
        PlanNode actualRight = new PlanNode(right.label(), right.detail(), right.rows(), rightCost, actualChildren(right, last), right.lane());
        return new PlanNode(estimate.label(), estimate.detail(), estimate.rows(), metrics.total(), List.of(left.actualPlan(verbose), actualRight), estimate.lane());
    }
    @Override public List<String> names() { return bindings.stream().map(ScalarPlan.Binding::name).toList(); }
    @Override public List<SqlType> types() { return bindings.stream().map(ScalarPlan.Binding::type).toList(); }
    @Override public void touch(boolean[] columns) {
        touched = columns.clone();
        if (independent != null) { independent.touch(columns); return; }
        shape.touch(columns);
        if (explainRight != null) explainRight.touch(rightShapeMetadata.touched);
        boolean[] required = leftShape.touched.clone();
        for (int i = 0; i < dependencies.length; i++) required[i] |= dependencies[i];
        left.touch(required);
    }
    private List<SqlParser.Expr> prePushed = List.of();
    LateralJoinPlan prePushed(List<SqlParser.Expr> terms) { prePushed = List.copyOf(terms); return this; }
    @Override public boolean push(SqlParser.Expr predicate) {
        if (prePushed.contains(predicate)) return true;
        if (independent != null) return independent.push(predicate);
        if (!FilterRules.safe(predicate, environment.resolver(bindings(), new ScalarPlan.Frame()))) return false;
        if (FilterRules.belongs(predicate, left.bindings()) && left.push(predicate)) return true;
        if ((syntax.kind().equals("inner") || syntax.kind().equals("cross")) && explainRight != null && FilterRules.belongs(predicate, explainRight.bindings()) && explainRight.pushScan(predicate)) {
            rightFilters.add(predicate); return true;
        }
        return false;
    }
    public void prepare(CostMeter meter) {
        long before = meter.cost();
        try {
            if (independent != null) { independent.prepare(meter); return; }
            if (!started) {
                started = true; rows = new RowSpool(left.types(), workMem, scratch, meter.memory());
                Row row; while ((row = left.next(meter)) != null) rows.append(row.values());
                left.close(); cursor = rows.cursor();
            }
        } finally { metrics.total(before, meter); }
    }
    @Override public Row next(CostMeter meter) {
        long before = meter.cost();
        try { return nextMeasured(meter); } finally { metrics.total(before, meter); }
    }
    private Row nextMeasured(CostMeter meter) {
        if (closed) return null;
        if (independent != null) return independent.next(meter);
        if (!started) {
            started = true; rows = new RowSpool(left.types(), workMem, scratch, meter.memory());
            Row row; while ((row = left.next(meter)) != null) rows.append(row.values());
            left.close(); cursor = rows.cursor();
        }
        while (true) {
            if (current != null) {
                Row row = current.next(meter); if (row != null) return row;
                if (measured) { lastRightActual = currentRight.actualPlan(false); completedRightCost = PlanEstimate.add(completedRightCost, lastRightActual.cost()); }
                current.close(); current = null; currentRight = null;
            }
            List<SqlValue> values = cursor.next(); if (values == null) { close(); return null; }
            frame.values = values;
            boolean reusableProbe = explainRight instanceof TableRelation table && table.reusableProbe();
            RelationPlan right = reusableProbe ? ((TableRelation)explainRight).probeExecution() : factory.apply(environment.childLateral(scope));
            try {
                for (SqlParser.Expr filter : reusableProbe ? List.<SqlParser.Expr>of() : rightFilters) if (!right.pushScan(filter)) throw new IllegalStateException("accepted lateral filter was rejected by execution plan");
                if (measured) right.requireAnalyze();
                currentRight = right;
                Shape one = new Shape(left.bindings(), values);
                current = (reusableProbe ? shape.probeExecution(one, right)
                    : new JoinPlan(one, one.bindings(), right, right.bindings(), syntax, args, environment, scratch, workMem, false)).borrowLeft();
                current.touch(touched);
            } catch (RuntimeException | Error failure) { right.close(); throw failure; }
        }
    }
    @Override public void close() {
        if (closed) return; closed = true;
        RuntimeException failure = null;
        List<AutoCloseable> resources = new ArrayList<>();
        if (independent != null) resources.add(independent);
        else { if (current != null) resources.add(current); if (cursor != null) resources.add(cursor); if (rows != null) resources.add(rows); resources.add(left); resources.add(shape); }
        for (AutoCloseable resource : resources) try { resource.close(); }
        catch (Exception error) { RuntimeException next = error instanceof RuntimeException runtime ? runtime : new RuntimeException(error); if (failure == null) failure = next; else failure.addSuppressed(next); }
        frame.values = List.of();
        if (failure != null) throw failure;
    }
    private static final class Shape implements RelationPlan {
        private final List<ScalarPlan.Binding> bindings;
        private List<SqlValue> row;
        private boolean[] touched;
        private final List<FunctionalKey> keys;
        Shape(List<ScalarPlan.Binding> bindings, List<SqlValue> row) { this(bindings, row, List.of()); }
        Shape(List<ScalarPlan.Binding> bindings, List<SqlValue> row, List<FunctionalKey> keys) { this.bindings = bindings; this.row = row; this.keys = keys; }
        public List<FunctionalKey> functionalKeys() { return keys; }
        public List<ScalarPlan.Binding> bindings() { return bindings; }
        public List<String> names() { return bindings.stream().map(ScalarPlan.Binding::name).toList(); }
        public List<SqlType> types() { return bindings.stream().map(ScalarPlan.Binding::type).toList(); }
        public void touch(boolean[] mask) { touched = mask.clone(); }
        public Row next(CostMeter meter) { if (row == null) return null; List<SqlValue> value = row; row = null; return new Row(names(), value); }
    }
}
