package org.jed;

import java.util.ArrayList;
import java.util.IdentityHashMap;
import java.util.List;

/** Statement snapshot, lexical outer scopes, and once-only uncorrelated subquery folds. */
final class QueryEnvironment implements AutoCloseable {
    Engine.Snapshot readRoot;
    ScalarPlan.Expression mergeAction;
    interface Planner { QueryPlan resolve(SqlParser.Statement statement, QueryEnvironment environment); }
    private static final class Init {
        QueryPlan plan;
        final boolean exists;
        final SqlType type;
        final boolean membership;
        final CollationDerivation collation;
        SqlValue value;
        RowSpool rows;
        Init(QueryPlan plan, boolean exists, SqlType type) { this(plan, exists, type, false); }
        Init(QueryPlan plan, boolean exists, SqlType type, boolean membership) { this.plan = plan; this.exists = exists; this.type = type; this.membership = membership; this.collation = exists ? CollationDerivation.NONE : plan.collations().getFirst().asColumn(); }
        void evaluate(CostMeter meter) {
            if (value != null || rows != null) return;
            try { rows = collect(plan, meter); if (!membership) value = consume(rows, exists, type); }
            finally { plan.close(); plan = null; }
        }
    }
    private static final class Shared {
        final List<CtePlan> ctes = new ArrayList<>();
        final IdentityHashMap<SqlParser.Star, List<List<String>>> recursiveStars = new IdentityHashMap<>();
        CostMeter meter;
        Sequences sequences;
        java.util.Map<String, SqlType> namedTypes = java.util.Map.of();
        final IdentityHashMap<SqlParser.Expr, Init> bySyntax = new IdentityHashMap<>();
        final List<Init> ordered = new ArrayList<>();
        boolean explainProbe;
        boolean estimateCapture;
        final IdentityHashMap<SqlParser.Expr, SqlParser.Expr> estimateOrigins = new IdentityHashMap<>();
        final IdentityHashMap<SqlParser.Expr, Long> dynamicEstimates = new IdentityHashMap<>();
        final IdentityHashMap<SqlParser.Expr, Boolean> dynamicOnce = new IdentityHashMap<>();
    }
    private static final class Initializers {
        final List<Init> ordered = new ArrayList<>();
        int initialized;
        boolean initializing;
    }
    private Initializers initializers = new Initializers();
    // RETURNING resolves with the statement, but its folds execute after mutation inputs.
    QueryEnvironment deferred() { QueryEnvironment child = fork(); child.initializers = new Initializers(); return child; }
    private void register(SqlParser.Expr syntax, Init init) { shared.bySyntax.put(syntax, init); shared.ordered.add(init); initializers.ordered.add(init); }
    private java.util.Map<String, Engine.ViewDependency> viewDependencies;
    private IdentityHashMap<SqlParser.Expr, String> viewStars;
    private java.util.Map<String, java.util.Set<Integer>> viewColumnReferences;
    private List<String> viewStack = List.of();
    private boolean runtimeBody;
    private boolean estimateOnly;
    QueryEnvironment estimateOnly() { QueryEnvironment child = fork(); child.estimateOnly = true; return child; }
    void estimateOrigin(SqlParser.Expr original, SqlParser.Expr resolved) { if (estimateOnly && original != resolved) shared.estimateOrigins.put(original, resolved); }
    QueryEnvironment runtimeBody() { QueryEnvironment child = fork(); child.runtimeBody = true; return child; }
    private boolean shapeOnly;
    boolean shapeOnly() { return shapeOnly; }
    QueryEnvironment shape() { QueryEnvironment child = fork(); child.shapeOnly = true; return child; }
    QueryEnvironment execution() { QueryEnvironment child = fork(); child.shapeOnly = false; return child; }
    QueryEnvironment viewScope(String name) {
        if (viewStack.contains(name)) throw new SqlException("42P17", "recursive view definition: " + name);
        if (viewStack.size() >= 256) throw new SqlException("54001", "view nesting limit reached");
        QueryEnvironment scope = new QueryEnvironment(planner, new SqlValue[0], null, shared);
        scope.initializers = initializers; scope.viewStack = new ArrayList<>(viewStack); scope.shapeOnly = shapeOnly; scope.estimateOnly = estimateOnly; scope.readRoot = readRoot;
        if (!name.isEmpty()) scope.viewStack.add(name);
        return scope;
    }
    QueryEnvironment captureView() {
        QueryEnvironment scope = viewScope(""); scope.viewDependencies = new java.util.TreeMap<>(); scope.viewStars = new IdentityHashMap<>(); scope.viewColumnReferences = new java.util.HashMap<>(); return scope;
    }
    List<Engine.ViewDependency> viewDependencies() {
        return viewDependencies == null ? List.of() : viewDependencies.values().stream().sorted((a, b) -> {
            int order = java.util.Arrays.compareUnsigned(SqlText.encode(a.name()), SqlText.encode(b.name()));
            return order == 0 ? Integer.compare(a.kind(), b.kind()) : order;
        }).toList();
    }
    java.util.Map<SqlParser.Expr, String> viewStars() { return viewStars == null ? java.util.Map.of() : viewStars; }
    boolean freezableStars() { return viewStars == null || viewStars.values().stream().noneMatch(java.util.Objects::isNull); }
    void hideRecursiveStar(SqlParser.Star star, String qualifier, List<String> columns) {
        shared.recursiveStars.put(star, columns.stream().map(name -> List.of(qualifier, name)).toList());
    }
    boolean starVisible(SqlParser.Star star, ScalarPlan.Binding binding) {
        for (List<String> hidden : shared.recursiveStars.getOrDefault(star, List.of()))
            if (binding.qualifier().equals(List.of(hidden.getFirst())) && binding.name().equals(hidden.getLast())) return false;
        return true;
    }
    void captureStars(List<SqlParser.Item> items, List<ScalarPlan.Binding> bindings) {
        if (viewStars == null) return;
        for (SqlParser.Item item : items) if (item.expression() instanceof SqlParser.Star star) {
            List<String> columns = new ArrayList<>();
            for (ScalarPlan.Binding binding : bindings) if (starVisible(star, binding) && (star.qualifier().isEmpty() ? !binding.qualifiedOnly() : star.qualifier().equals(binding.qualifier()))) {
                List<String> parts = new ArrayList<>(binding.qualifier()); parts.add(binding.name());
                columns.add(parts.stream().map(part -> "\"" + part.replace("\"", "\"\"") + "\"").collect(java.util.stream.Collectors.joining(" . ")));
            }
            viewStars.put(star, new java.util.HashSet<>(columns).size() == columns.size() ? String.join(" , ", columns) : null);
        }
    }
    void viewTable(Engine.Table table) { viewTable(table, new boolean[table.columns().size()]); }
    void viewTable(Engine.Table table, boolean[] touched) {
        if (viewDependencies == null) return;
        Engine.ViewDependency previous = viewDependencies.get("0:" + table.name());
        java.util.Set<String> columns = new java.util.TreeSet<>((a, b) -> java.util.Arrays.compareUnsigned(SqlText.encode(a), SqlText.encode(b)));
        if (previous != null) columns.addAll(previous.columns());
        for (int i = 0; i < touched.length; i++) if (touched[i]) columns.add(table.columns().get(i).name());
        viewDependencies.put("0:" + table.name(), new Engine.ViewDependency(0, table.name(), new ArrayList<>(columns)));
    }
    void viewColumns(Engine.View view, boolean[] touched) {
        if (viewColumnReferences == null) return;
        java.util.Set<Integer> columns = viewColumnReferences.computeIfAbsent(view.name(), ignored -> new java.util.HashSet<>());
        for (int i = 0; i < touched.length; i++) if (touched[i]) columns.add(i);
    }
    boolean viewColumnAtOrAbove(String name, int ordinal) { return viewColumnReferences != null && viewColumnReferences.getOrDefault(name, java.util.Set.of()).stream().anyMatch(column -> column >= ordinal); }
    void viewReference(Engine.View view) {
        if (viewDependencies != null) viewDependencies.put("1:" + view.name(), new Engine.ViewDependency(1, view.name(), List.of()));
    }
    void sequences(Sequences state) { shared.sequences = state; }
    Sequences sequences() { return shared.sequences; }
    void viewSequence(String name) {
        if (viewDependencies != null) viewDependencies.put("2:" + name, new Engine.ViewDependency(2, name, List.of()));
    }
    private final Planner planner;
    private final SqlValue[] args;
    boolean contradiction(SqlParser.Expr expression) { return Contradictions.proven(expression, args); }
    private final ScalarPlan.Resolver outer;
    private final Shared shared;
    private final java.util.Map<String, CtePlan> relations = new java.util.HashMap<>();
    boolean correlated;
    private QueryEnvironment correlationParent;
    QueryEnvironment(Planner planner, SqlValue[] args) { this(planner, args, null, new Shared()); }
    QueryEnvironment(Planner planner, SqlValue[] args, CostMeter meter) { this(planner, args); shared.meter = meter; }
    void namedTypes(java.util.Map<String, SqlType> types) { shared.namedTypes = java.util.Map.copyOf(types); }
    SqlType namedType(String name) { SqlType type = shared.namedTypes.get(name); if (type != null && viewDependencies != null) viewDependencies.put("3:" + name, new Engine.ViewDependency(3, name, List.of())); return type; }
    CostMeter meter() { return shared.meter; }
    void meter(CostMeter meter) { shared.meter = meter; }
    boolean estimateCapture() { return shared.estimateCapture; }
    void estimateCapture(boolean value) { shared.estimateCapture = value; }
    boolean explainProbe() { return shared.explainProbe; }
    void explainProbe(boolean value) { shared.explainProbe = value; }
    private QueryEnvironment(Planner planner, SqlValue[] args, ScalarPlan.Resolver outer, Shared shared) {
        this.planner = planner; this.args = args; this.outer = outer; this.shared = shared;
    }
    QueryEnvironment fork() { QueryEnvironment fork = child(outer); fork.correlationParent = this; return fork; }
    void markCorrelated() { correlated = true; if (correlationParent != null) correlationParent.markCorrelated(); }
    QueryEnvironment child(ScalarPlan.Resolver scope) {
        QueryEnvironment child = new QueryEnvironment(planner, args, scope, shared); child.initializers = initializers; child.relations.putAll(relations); child.viewDependencies = viewDependencies; child.viewStars = viewStars; child.viewColumnReferences = viewColumnReferences; child.viewStack = viewStack; child.shapeOnly = shapeOnly; child.runtimeBody = runtimeBody; child.estimateOnly = estimateOnly; child.readRoot = readRoot; return child;
    }
    private ScalarPlan.Resolver lateralPrefix, lateralBase;
    ScalarPlan.Resolver lateralPrefix() { return lateralPrefix; }
    QueryEnvironment childLateral(ScalarPlan.Resolver scope) {
        QueryEnvironment result = child(scope); result.lateralPrefix = scope;
        result.lateralBase = lateralPrefix == null ? outer : lateralBase; return result;
    }
    QueryEnvironment lateralFence() {
        if (lateralPrefix == null) return this;
        QueryEnvironment result = child(lateralBase); result.lateralPrefix = lateralPrefix; result.lateralBase = lateralBase; return result;
    }
    QueryEnvironment lexical() { return fork(); }
    QueryPlan plan(SqlParser.Statement statement) { return planner.resolve(statement, this); }
    CtePlan cte(String name) { return relations.get(name); }
    void cte(String name, CtePlan plan) { relations.put(name, plan); shared.ctes.add(plan); }
    ScalarPlan.Resolver resolver(List<ScalarPlan.Binding> bindings, ScalarPlan.Frame frame) {
        return new ScalarPlan.Resolver(args, bindings, frame, this, outer);
    }
    ScalarPlan.Resolver resolver() { return resolver(List.of(), new ScalarPlan.Frame()); }
    long hiddenEstimate(SqlParser.Expr expression) { return hiddenEstimate(expression, 1); }
    long hiddenEstimate(SqlParser.Expr expression, long invocations) {
        return hiddenEstimate(expression, invocations, new IdentityHashMap<>(), false);
    }
    long hiddenStageEstimate(SqlParser.Expr expression, long invocations) { return hiddenEstimate(expression, invocations, new IdentityHashMap<>(), true); }
    private long hiddenEstimate(SqlParser.Expr expression, long invocations, IdentityHashMap<Init, Boolean> seen, boolean outputStage) {
        if (expression == null || outputStage && (expression instanceof SqlParser.Window || expression instanceof SqlParser.Call call && AggregatePlan.aggregateName(call.name()))) return 0;
        SqlParser.Expr resolved = expression;
        while (shared.estimateOrigins.containsKey(resolved)) resolved = shared.estimateOrigins.get(resolved);
        Init init = shared.bySyntax.get(expression);
        if (init == null) init = shared.bySyntax.get(resolved);
        if (init != null) return seen.put(init, Boolean.TRUE) == null && init.plan != null ? init.plan.explain(false).cost() : 0;
        long cost = PlanEstimate.mul(shared.dynamicEstimates.getOrDefault(resolved, 0L), shared.dynamicOnce.containsKey(resolved) ? 1 : invocations);
        for (SqlParser.Expr child : DefaultExpression.children(expression)) cost = PlanEstimate.add(cost, hiddenEstimate(child, invocations, seen, outputStage));
        return cost;
    }
    ScalarPlan.Expression subquery(SqlParser.Expr syntax, SqlParser.Statement statement, boolean exists, ScalarPlan.Resolver scope) {
        return subquery(syntax, statement, exists, scope, -1);
    }
    private ScalarPlan.Expression subquery(SqlParser.Expr syntax, SqlParser.Statement statement, boolean exists, ScalarPlan.Resolver scope, int rowWidth) {
        Init cached = runtimeBody || estimateOnly ? null : shared.bySyntax.get(syntax);
        if (cached != null) return new ScalarPlan.Expression(cached.type, meter -> { initialize(meter); return cached.value; }, true, cached.collation);
        QueryEnvironment child = child(scope);
        QueryPlan plan = packed(planner.resolve(statement, child), rowWidth);
        if (!exists && plan.types().size() != 1) { plan.close(); throw new SqlException("42601", "scalar subquery must have one column"); }
        SqlType type = exists ? SqlType.BOOLEAN : plan.types().getFirst();
        CollationDerivation collation = exists ? CollationDerivation.NONE : plan.collations().getFirst().asColumn();
        if (!estimateOnly && !runtimeBody && !child.correlated) {
            Init init = new Init(plan, exists, type);
            register(syntax, init);
            return new ScalarPlan.Expression(type, meter -> { initialize(meter); return init.value; }, true, collation);
        }
        if (shared.explainProbe || shared.estimateCapture) { shared.dynamicEstimates.put(syntax, plan.explain(false).cost()); if (!child.correlated) shared.dynamicOnce.put(syntax, Boolean.TRUE); }
        plan.close();
        return new ScalarPlan.Expression(type, meter -> {
            try (QueryPlan execution = packed(planner.resolve(statement, child(scope)), rowWidth)) {
                SqlValue result = consume(execution, exists, type, meter);
                meter.charge(Costs.OPERATOR_EVAL);
                return result;
            }
        }, false, collation);
    }
    ScalarPlan.Expression rowComparison(SqlParser.Binary syntax, ScalarPlan.Resolver scope) {
        SqlParser.RowConstructor row = (SqlParser.RowConstructor)syntax.left();
        SqlParser.Subquery query = (SqlParser.Subquery)syntax.right();
        ScalarPlan.Expression right = subquery(query, query.query(), false, scope, row.values().size());
        ScalarPlan.Expression left = scope.resolve(CompositeExpressions.comparisonNulls(row, right.type()), null);
        ScalarPlan.compatible(left.type(), right.type());
        return new ScalarPlan.Expression(SqlType.BOOLEAN, meter -> {
            meter.operation(); SqlValue a = left.run(meter), b = right.run(meter);
            return a.isNull() || b.isNull() ? SqlValue.nullValue(SqlType.BOOLEAN) : CompositeValues.sqlCompare(syntax.op(), a, b, meter);
        });
    }
    private static QueryPlan packed(QueryPlan plan, int width) {
        if (width < 0) return plan;
        if (plan.types().size() != width) { plan.close(); throw new SqlException("42601", "row subquery column count mismatch"); }
        List<SqlType.Field> fields = new ArrayList<>();
        for (int i = 0; i < width; i++) fields.add(new SqlType.Field("f" + (i + 1), plan.types().get(i)));
        SqlType type = SqlType.record(fields);
        return new QueryPlan() {
            public List<String> names() { return List.of("row"); }
            public List<SqlType> types() { return List.of(type); }
            public Row next(CostMeter meter) { Row row = plan.next(meter); return row == null ? null : new Row(names(), List.of(SqlValue.ofComposite(type, row.values()))); }
            public void close() { plan.close(); }
        };
    }
    ScalarPlan.Expression membership(SqlParser.In syntax, ScalarPlan.Resolver scope) {
        Init init = runtimeBody || estimateOnly ? null : shared.bySyntax.get(syntax);
        QueryEnvironment child = child(scope);
        int rowWidth = syntax.subject() instanceof SqlParser.RowConstructor row ? row.values().size() : -1;
        QueryPlan plan = init == null ? packed(planner.resolve(syntax.query(), child), rowWidth) : null;
        if (plan != null && plan.types().size() != 1) { plan.close(); throw new SqlException("42601", "IN subquery must have one column"); }
        SqlType element = init == null ? plan.types().getFirst() : init.type;
        ScalarPlan.Expression subject = scope.resolve(rowWidth >= 0 ? CompositeExpressions.comparisonNulls(syntax.subject(), element) : syntax.subject(), rowWidth >= 0 ? null : element);
        ScalarPlan.compatible(subject.type(), element);
        CollationDerivation.comparison(subject.collation(), init == null ? plan.collations().getFirst().asColumn() : init.collation).require();
        if (init != null || !estimateOnly && !runtimeBody && !child.correlated) {
            if (init == null) { init = new Init(plan, false, element, true); register(syntax, init); }
            Init folded = init;
            return new ScalarPlan.Expression(SqlType.BOOLEAN, meter -> {
                initialize(meter);
                return member(folded.rows, subject, syntax.negated(), false, meter);
            });
        }
        if (shared.explainProbe || shared.estimateCapture) { shared.dynamicEstimates.put(syntax, plan.explain(false).cost()); if (!child.correlated) shared.dynamicOnce.put(syntax, Boolean.TRUE); }
        plan.close();
        return new ScalarPlan.Expression(SqlType.BOOLEAN, meter -> {
            meter.operation();
            try (QueryPlan execution = packed(planner.resolve(syntax.query(), child(scope)), rowWidth); RowSpool rows = collect(execution, meter)) {
                return member(rows, subject, syntax.negated(), true, meter);
            }
        });
    }
    ScalarPlan.Expression quantified(SqlParser.Quantified syntax, ScalarPlan.Resolver scope) {
        Init init = runtimeBody || estimateOnly ? null : shared.bySyntax.get(syntax);
        QueryEnvironment child = child(scope);
        int rowWidth = syntax.subject() instanceof SqlParser.RowConstructor row ? row.values().size() : -1;
        QueryPlan plan = init == null ? packed(planner.resolve(syntax.query(), child), rowWidth) : null;
        if (plan != null && plan.types().size() != 1) { plan.close(); throw new SqlException("42601", "quantified subquery must have one column"); }
        SqlType element = init == null ? plan.types().getFirst() : init.type;
        ScalarPlan.Expression subject;
        try { subject = scope.resolve(rowWidth >= 0 ? CompositeExpressions.comparisonNulls(syntax.subject(), element) : syntax.subject(), rowWidth >= 0 ? null : element); ScalarPlan.compatible(subject.type(), element); }
        catch (SqlException error) { if (plan != null) plan.close(); if (error.sqlState().equals("42804")) throw new SqlException("42883", "quantified comparison element types are incompatible"); throw error; }
        String collationName = CollationDerivation.comparison(subject.collation(), init == null ? plan.collations().getFirst().asColumn() : init.collation).require();
        Collation collation = meter().environment().collation(collationName);
        if (init != null || !estimateOnly && !runtimeBody && !child.correlated) {
            if (init == null) { init = new Init(plan, false, element, true); register(syntax, init); }
            Init folded = init;
            return new ScalarPlan.Expression(SqlType.BOOLEAN, meter -> {
                initialize(meter); meter.operation();
                return quantify(folded.rows, subject.run(meter), syntax, meter, collation);
            });
        }
        if (shared.explainProbe || shared.estimateCapture) { shared.dynamicEstimates.put(syntax, plan.explain(false).cost()); if (!child.correlated) shared.dynamicOnce.put(syntax, Boolean.TRUE); }
        plan.close();
        return new ScalarPlan.Expression(SqlType.BOOLEAN, meter -> {
            meter.operation(); SqlValue left = subject.run(meter);
            try (QueryPlan execution = packed(planner.resolve(syntax.query(), child(scope)), rowWidth); RowSpool rows = collect(execution, meter)) {
                return quantify(rows, left, syntax, meter, collation);
            }
        });
    }
    private static SqlValue quantify(RowSpool rows, SqlValue left, SqlParser.Quantified syntax, CostMeter meter, Collation collation) {
        boolean all = syntax.quantifier().equals("all"), unknown = false;
        try (RowSpool.Cursor cursor = rows.cursor()) {
            List<SqlValue> row;
            while ((row = cursor.next()) != null) {
                meter.operation(); SqlValue right = row.getFirst();
                if (left.isNull() || right.isNull()) { unknown = true; continue; }
                if (left.type().kind() == SqlType.Kind.RECORD) {
                    SqlValue result = CompositeValues.sqlCompare(syntax.operator(), left, right, meter);
                    if (result.isNull()) unknown = true;
                    else if (result.asBoolean() != all) return result;
                    continue;
                }
                int c = List.of("=", "!=", "<>").contains(syntax.operator()) ? ScalarPlan.compare(left, right, meter) : CollationDerivation.compare(left, right, meter, collation);
                boolean match = switch (syntax.operator()) { case "=" -> c == 0; case "!=", "<>" -> c != 0; case "<" -> c < 0; case "<=" -> c <= 0; case ">" -> c > 0; case ">=" -> c >= 0; default -> throw new AssertionError(syntax.operator()); };
                if (match != all) return SqlValue.ofBoolean(match);
            }
        }
        return unknown ? SqlValue.nullValue(SqlType.BOOLEAN) : SqlValue.ofBoolean(all);
    }
    private static RowSpool collect(QueryPlan plan, CostMeter meter) {
        RowSpool rows = new RowSpool(plan.types(), 0, null, meter.memory());
        try { Row row; while ((row = plan.next(meter)) != null) rows.append(row.values()); return rows; }
        catch (RuntimeException | Error failure) { rows.close(); throw failure; }
    }
    private static SqlValue member(RowSpool rows, ScalarPlan.Expression subject, boolean negated, boolean correlated, CostMeter meter) {
        if (!correlated) meter.operation();
        SqlValue left = subject.run(meter);
        boolean unknown = false, match = false;
        try (RowSpool.Cursor cursor = rows.cursor()) {
            List<SqlValue> row;
            while ((row = cursor.next()) != null) {
                meter.operation(); SqlValue right = row.getFirst();
                if (left.isNull() || right.isNull()) unknown = true;
                else if (left.type().kind() == SqlType.Kind.RECORD) {
                    SqlValue equal = CompositeValues.sqlCompare("=", left, right, meter);
                    if (equal.isNull()) unknown = true; else if (equal.asBoolean()) match = true;
                } else if (ScalarPlan.compare(left, right, meter) == 0) match = true;
            }
        }
        return match ? SqlValue.ofBoolean(!negated) : unknown ? SqlValue.nullValue(SqlType.BOOLEAN) : SqlValue.ofBoolean(negated);
    }
    boolean hasPlanInit(SqlParser.Expr syntax) { return hasInit(syntax) || shared.explainProbe && shared.dynamicOnce.containsKey(syntax); }
    boolean hasInit(SqlParser.Expr syntax) { return !runtimeBody && shared.bySyntax.containsKey(syntax); }
    RowSpool foldedMembership(SqlParser.Expr syntax) {
        Init init = runtimeBody || estimateOnly ? null : shared.bySyntax.get(syntax);
        return init != null && init.membership ? init.rows : null;
    }
    void initialize(CostMeter meter) {
        if (initializers.initializing) return;
        initializers.initializing = true;
        try {
            while (initializers.initialized < initializers.ordered.size()) initializers.ordered.get(initializers.initialized++).evaluate(meter);
        } finally { initializers.initializing = false; }
    }
    QueryPlan wrap(QueryPlan plan) {
        return new QueryPlan() {
            private long initializerCost;
            public void requireAnalyze() { plan.requireAnalyze(); }
            public PlanNode actualPlan(boolean verbose) { PlanNode node = plan.actualPlan(verbose); return node.finish(node.detail(), initializerCost, node.lane()); }
            public List<String> names() { return plan.names(); }
            public List<SqlType> types() { return plan.types(); }
            public List<CollationDerivation> collations() { return plan.collations(); }
            public PlanNode explain(boolean verbose) { return plan.explain(verbose); }
            public void collect(RowSpool target, CostMeter meter) { long before = meter.cost(); initialize(meter); initializerCost = PlanEstimate.add(initializerCost, meter.cost() - before); plan.collect(target, meter); }
            public Row next(CostMeter meter) { long before = meter.cost(); initialize(meter); initializerCost = PlanEstimate.add(initializerCost, meter.cost() - before); return plan.next(meter); }
            public void close() { plan.close(); }
        };
    }
    private static SqlValue consume(QueryPlan plan, boolean exists, SqlType type, CostMeter meter) {
        try (RowSpool rows = collect(plan, meter)) { return consume(rows, exists, type); }
    }
    private static SqlValue consume(RowSpool rows, boolean exists, SqlType type) {
        if (exists) return SqlValue.ofBoolean(rows.size() != 0);
        if (rows.size() == 0) return SqlValue.nullValue(type);
        if (rows.size() > 1) throw new SqlException("21000", "scalar subquery returned more than one row");
        try (RowSpool.Cursor cursor = rows.cursor()) { return cursor.next().getFirst(); }
    }
    @Override public void close() {
        for (Init init : shared.ordered) { if (init.plan != null) { init.plan.close(); init.plan = null; } if (init.rows != null) init.rows.close(); }
        for (CtePlan cte : shared.ctes) cte.close();
    }
}
