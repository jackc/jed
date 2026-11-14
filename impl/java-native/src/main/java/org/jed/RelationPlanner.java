package org.jed;

import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.function.BiFunction;
import java.util.function.Function;

/** FROM namespace construction and authored-order relation planning. */
final class RelationPlanner {
    private final SqlValue[] args;
    private final Function<SqlParser.Name, Engine.Table> tableLookup;
    private final Function<SqlParser.Name, Engine.View> viewLookup;
    private final BiFunction<SqlParser.Statement, QueryEnvironment, QueryPlan> reader;
    private final BiFunction<SqlParser.Values, QueryEnvironment, QueryPlan> valuesReader;
    private final Path scratch;
    private final long workMem;
    private SqlParser.Select select;
    private int groupedDepth;
    RelationPlanner select(SqlParser.Select query) { select = query; return this; }
    RelationPlanner(SqlValue[] args, Function<SqlParser.Name, Engine.Table> tableLookup, Function<SqlParser.Name, Engine.View> viewLookup,
                    BiFunction<SqlParser.Statement, QueryEnvironment, QueryPlan> reader,
                    BiFunction<SqlParser.Values, QueryEnvironment, QueryPlan> valuesReader, Path scratch, long workMem) {
        this.args = args; this.tableLookup = tableLookup; this.viewLookup = viewLookup; this.reader = reader; this.valuesReader = valuesReader;
        this.scratch = scratch; this.workMem = workMem;
    }
    private static int inputCount(SqlParser.From source) {
        if (source instanceof SqlParser.Join join) return inputCount(join.left()) + (join.right() instanceof SqlParser.Grouped ? 1 : inputCount(join.right()));
        if (source instanceof SqlParser.Grouped group && group.alias() == null) return inputCount(group.source());
        return 1;
    }
    RelationPlan relation(SqlParser.From syntax, QueryEnvironment environment) { return relation(syntax, environment, null); }
    RelationPlan relation(SqlParser.From syntax, QueryEnvironment environment, SqlParser.Expr filter) {
        if (syntax instanceof SqlParser.Grouped group) {
            RelationPlan inner; groupedDepth++;
            try { inner = relation(group.source(), environment.lateralFence()); } finally { groupedDepth--; }
            if (group.alias() == null) return metadata(inner, inner.bindings().stream().map(binding -> binding.extendNulls(group.source() instanceof SqlParser.Join)).toList());
            return produced(aliasGroup(inner, group));
        }
        if (syntax instanceof SqlParser.JsonTable table) return new JsonTablePlan(table, environment.lateralPrefix() == null ? environment : environment.child(environment.lateralPrefix()));
        if (syntax instanceof SqlParser.TableFunction function) {
            QueryEnvironment scope = environment.lateralPrefix() == null ? environment : environment.child(environment.lateralPrefix());
            RelationPlan source = JsonRecordPlan.recognized(function.call().name()) ? new JsonRecordPlan(function, scope) : new TableFunctionPlan(function, scope);
            return new FilteredRelation(source, scope);
        }
        if (syntax instanceof SqlParser.Join join) {
            QueryEnvironment islandEnvironment = environment;
            RelationPlan island = groupedDepth != 0 ? null : IslandPlanner.plan(join, filter, select, source -> relation(source, islandEnvironment), args, environment, scratch, workMem);
            if (island != null) return island;
            RelationPlan left = relation(join.left(), environment), right = null;
            try {
                if (lateralEligible(join.right())) {
                    SqlParser.From rightSyntax = join.right();
                    return new LateralJoinPlan(left, scope -> rightInput(rightSyntax, scope), join, args, environment, scratch, workMem);
                }
                right = rightInput(join.right(), environment);
                java.util.Set<List<String>> qualifiers = new java.util.HashSet<>();
                for (ScalarPlan.Binding binding : left.bindings()) if (!binding.qualifier().isEmpty()) qualifiers.add(binding.qualifier());
                for (ScalarPlan.Binding binding : right.bindings()) if (!binding.qualifier().isEmpty() && qualifiers.contains(binding.qualifier())) throw new SqlException("42712", "duplicate relation name");
                List<SqlParser.Expr> pushedWhere = new ArrayList<>();
                if (filter != null) {
                    List<ScalarPlan.Binding> output = JoinPlan.outputBindings(join, left.bindings(), right.bindings());
                    ScalarPlan.Resolver filterResolver = environment.resolver(output, new ScalarPlan.Frame());
                    filterResolver.predicate(filter);
                    for (SqlParser.Expr term : FilterRules.terms(filter)) {
                        boolean safe = FilterRules.safe(term, filterResolver);
                        if (FilterRules.accessHint(term) && (!(join.kind().equals("right") || join.kind().equals("full")) || FilterRules.nullRejecting(term)) && (!safe || join.kind().equals("right") || join.kind().equals("full")) && left instanceof TableRelation table && FilterRules.belongs(term, left.bindings())) table.accessWhere(term);
                        if (FilterRules.accessHint(term) && (!(join.kind().equals("left") || join.kind().equals("full")) || FilterRules.nullRejecting(term)) && (!safe || join.kind().equals("left") || join.kind().equals("full")) && right instanceof TableRelation table && FilterRules.belongs(term, right.bindings())) table.accessWhere(term);
                        if (!FilterRules.safe(term, filterResolver)) continue;
                        if ((join.kind().equals("inner") || join.kind().equals("cross") || join.kind().equals("left")) && FilterRules.belongs(term, left.bindings()) && left.pushInJoin(term)
                            || (join.kind().equals("inner") || join.kind().equals("cross") || join.kind().equals("right")) && FilterRules.belongs(term, right.bindings()) && right.pushInJoin(term)) pushedWhere.add(term);
                    }
                }
                boolean hashGate = (!join.kind().equals("left") || select == null || select.from().stream().mapToInt(RelationPlanner::inputCount).sum() == 2) && JoinPlan.hashEligible(join, left.bindings(), right.bindings());
                if (join.on() != null) {
                    List<ScalarPlan.Binding> all = new ArrayList<>(left.bindings()); all.addAll(right.bindings());
                    ScalarPlan.Resolver onResolver = environment.resolver(all, new ScalarPlan.Frame());
                    ScalarPlan.Expression condition = onResolver.resolve(join.on(), SqlType.BOOLEAN);
                    if (condition.type() != SqlType.BOOLEAN) throw SqlException.typeMismatch("JOIN condition requires boolean");
                    SqlParser.Expr residual = null;
                    for (SqlParser.Expr term : FilterRules.terms(join.on())) {
                        boolean pushed = FilterRules.safe(term, onResolver) && ((join.kind().equals("inner") || join.kind().equals("cross") || join.kind().equals("right")) && FilterRules.belongs(term, left.bindings()) && left.pushInJoin(term)
                            || (join.kind().equals("inner") || join.kind().equals("cross") || join.kind().equals("left")) && FilterRules.belongs(term, right.bindings()) && right.pushInJoin(term));
                        if (!pushed) residual = FilterRules.and(residual, term);
                    }
                    join = new SqlParser.Join(join.left(), join.right(), join.kind(), residual, join.using(), join.natural());
                }
                SqlParser.Expr residualWhere = null;
                for (SqlParser.Expr term : FilterRules.terms(filter)) if (!pushedWhere.contains(term)) residualWhere = FilterRules.and(residualWhere, term);
                if ((join.kind().equals("inner") || join.kind().equals("cross")) && join.using().isEmpty() && !join.natural() && left instanceof TableRelation first && right instanceof TableRelation second) {
                    SqlParser.Expr condition = JoinPlan.condition(join, left.bindings(), right.bindings());
                    SqlParser.Expr bounds = FilterRules.and(condition, accessTerms(filter));
                    PlanNode a = first.explain(false), b = second.explain(false);
                    long operations = PlanEstimate.operations(condition);
                    long work = PlanEstimate.mul(PlanEstimate.mul(a.rows(), b.rows()), operations);
                    if (hashGate && JoinPlan.hashEligible(join, left.bindings(), right.bindings())) work = Math.min(work, JoinPlan.hashWork(join, left.bindings(), right.bindings(), a.rows(), b.rows()));
                    long incumbent = finishJoinEstimate(PlanEstimate.add(PlanEstimate.add(a.cost(), b.cost()), work), PlanEstimate.rows(condition, PlanEstimate.mul(a.rows(), b.rows()), List.of()), residualWhere);
                    TableRelation.Probe forward = second.probeCandidate(bounds, left.bindings());
                    if (forward != null) {
                        PlanNode probe = second.probeEstimate(forward);
                        incumbent = Math.min(incumbent, finishJoinEstimate(PlanEstimate.add(a.cost(), PlanEstimate.mul(a.rows(), PlanEstimate.add(probe.cost(), PlanEstimate.mul(probe.rows(), operations)))), PlanEstimate.mul(a.rows(), probe.rows()), residualWhere));
                    }
                    TableRelation.Probe backward = first.probeCandidate(bounds, right.bindings());
                    if (backward != null) {
                        PlanNode probe = first.probeEstimate(backward);
                        long cost = finishJoinEstimate(PlanEstimate.add(b.cost(), PlanEstimate.mul(b.rows(), PlanEstimate.add(probe.cost(), PlanEstimate.mul(probe.rows(), operations)))), PlanEstimate.mul(b.rows(), probe.rows()), residualWhere);
                        if (cost < incumbent) {
                            SqlParser.Join reversed = new SqlParser.Join(join.right(), join.left(), join.kind(), join.on(), List.of(), false);
                            List<ScalarPlan.Binding> logical = JoinPlan.outputBindings(join, left.bindings(), right.bindings());
                            first.close();
                            RelationPlan physical = new LateralJoinPlan(right, scope -> first.probe(scope, bounds, backward), reversed, args, environment, scratch, workMem).prePushed(pushedWhere);
                            return new ReorderedRelation(physical, logical, left.bindings().size(), right.bindings().size());
                        }
                    }
                }
                if ((join.kind().equals("inner") || join.kind().equals("cross") || join.kind().equals("left")) && right instanceof TableRelation table) {
                    SqlParser.Expr accessCondition = FilterRules.and(JoinPlan.condition(join, left.bindings(), right.bindings()), join.kind().equals("left") ? nullRejectingAccessTerms(filter) : accessTerms(filter));
                    TableRelation.Probe candidate = table.probeCandidate(accessCondition, left.bindings());
                    if (candidate != null) {
                        PlanNode outer = null, full = null;
                        try { outer = left.explain(false); full = table.explain(false); }
                        catch (SqlException unsupported) { if (!unsupported.sqlState().equals("0A000")) throw unsupported; }
                        if (outer != null) {
                            PlanNode probe = table.probeEstimate(candidate); long operation = PlanEstimate.operations(JoinPlan.condition(join, left.bindings(), right.bindings()));
                            long repeated = PlanEstimate.mul(outer.rows(), PlanEstimate.add(probe.cost(), PlanEstimate.mul(probe.rows(), operation)));
                            long materialized = PlanEstimate.add(full.cost(), PlanEstimate.mul(PlanEstimate.mul(outer.rows(), full.rows()), operation));
                            if (hashGate && JoinPlan.hashEligible(join, left.bindings(), right.bindings())) materialized = Math.min(materialized, PlanEstimate.add(full.cost(), JoinPlan.hashWork(join, left.bindings(), right.bindings(), outer.rows(), full.rows())));
                            if (left.baseIsland() && table.baseIsland()) {
                                repeated = finishJoinEstimate(repeated, PlanEstimate.mul(outer.rows(), probe.rows()), residualWhere);
                                materialized = finishJoinEstimate(materialized, PlanEstimate.rows(JoinPlan.condition(join, left.bindings(), right.bindings()), PlanEstimate.mul(outer.rows(), full.rows()), List.of()), residualWhere);
                            }
                            if (join.kind().equals("left") || repeated <= materialized) {
                                SqlParser.Join selected = join; table.close();
                                return new LateralJoinPlan(left, scope -> table.probe(scope, accessCondition, candidate), selected, args, environment, scratch, workMem).prePushed(pushedWhere);
                            }
                        }
                    }
                }
                boolean hash = hashGate && JoinPlan.hashEligible(join, left.bindings(), right.bindings());
                if (hash && join.kind().equals("inner") && left.baseIsland() && right.baseIsland() && !(select != null && Contradictions.proven(select.where(), args))) try {
                    PlanNode outer = left.explain(false), inner = right.explain(false);
                    long joinOperations = PlanEstimate.operations(join.on());
                    if (!join.using().isEmpty()) joinOperations = PlanEstimate.add(joinOperations, join.using().size());
                    else if (join.natural()) { var rightColumns = right.bindings(); joinOperations = PlanEstimate.add(joinOperations, left.bindings().stream().filter(binding -> !binding.qualifiedOnly() && rightColumns.stream().anyMatch(other -> !other.qualifiedOnly() && other.name().equals(binding.name()))).map(ScalarPlan.Binding::name).distinct().count()); }
                    long nestedWork = PlanEstimate.mul(PlanEstimate.mul(outer.rows(), inner.rows()), joinOperations);
                    hash = JoinPlan.hashWork(join, left.bindings(), right.bindings(), outer.rows(), inner.rows()) <= nestedWork;
                } catch (SqlException unavailable) {
                    if (!unavailable.sqlState().equals("0A000")) throw unavailable;
                }
                return new JoinPlan(left, left.bindings(), right, right.bindings(), join, args, environment, scratch, workMem, hash).prePushed(pushedWhere);
            } catch (RuntimeException | Error failure) { left.close(); if (right != null) right.close(); throw failure; }
        }
        QueryPlan input; List<String> aliases; List<String> qualifier;
        SqlParser.Statement inlineBody = null;
        if (syntax instanceof SqlParser.Table table) {
            CtePlan cte = table.name().parts().size() == 1 ? environment.cte(table.name().parts().getFirst()) : null;
            if (cte == null) {
                if (CatalogRelation.recognized(table.name())) return new FilteredRelation(new CatalogRelation(table, environment), environment);
                Engine.View view = viewLookup.apply(table.name());
                if (view != null) {
                    environment.viewReference(view);
                    if (table.columns().size() > view.columns().size()) throw new SqlException("42P10", "too many view column aliases");
                    List<String> names = new ArrayList<>();
                    for (int i = 0; i < view.columns().size(); i++) names.add(i < table.columns().size() ? table.columns().get(i) : view.columns().get(i).name());
                    RelationPlan source = relation(new SqlParser.Derived(view.query(), table.alias() == null ? view.name() : table.alias(), names, false), environment.viewScope(view.name()));
                    var bindings = new ArrayList<ScalarPlan.Binding>();
                    for (int i = 0; i < source.bindings().size(); i++) bindings.add(source.bindings().get(i).catalog(view.columns().get(i).name(), false));
                    QueryEnvironment trace = environment;
                    return new RelationPlan() {
                        public List<ScalarPlan.Binding> bindings() { return bindings; }
                        public List<String> names() { return source.names(); }
                        public List<SqlType> types() { return source.types(); }
                        public Row next(CostMeter meter) { return source.next(meter); }
                        public PlanNode explain(boolean verbose) { return source.explain(verbose); }
                        public void requireAnalyze() { source.requireAnalyze(); }
                        public PlanNode actualPlan(boolean verbose) { return source.actualPlan(verbose); }
                        public boolean push(SqlParser.Expr predicate) { boolean pushed = source.push(predicate); if (pushed) { ScalarPlan.Resolver resolver = trace.resolver(bindings, new ScalarPlan.Frame()); resolver.resolve(predicate, SqlType.BOOLEAN); trace.viewColumns(view, resolver.touched()); } return pushed; }
                        public boolean orderedBy(List<SqlParser.Order> order) { return source.orderedBy(order); }
                        public List<FunctionalKey> functionalKeys() { return source.functionalKeys(); }
                        public void touch(boolean[] mask) { trace.viewColumns(view, mask); source.touch(mask); }
                        public void close() { source.close(); }
                    };
                }
                Engine.Table base = tableLookup.apply(table.name()); environment.viewTable(base);
                return new TableRelation(base, table, null, args, environment);
            }
            if (cte.correlated()) environment.markCorrelated();
            input = cte.reference(); inlineBody = cte.inlineBody(); if (inlineBody != null) environment = cte.inlineEnvironment(); aliases = table.columns(); qualifier = List.of(table.alias() == null ? table.name().parts().getFirst() : table.alias());
        } else if (syntax instanceof SqlParser.Derived derived) {
            environment = (derived.lateral() && environment.lateralPrefix() != null ? environment.child(environment.lateralPrefix()) : environment).lexical().runtimeBody();
            input = derived.query() instanceof SqlParser.Values v && v.order().isEmpty() && v.limit() == null && v.offset() == null ? valuesReader.apply(v, environment) : reader.apply(derived.query(), environment.shape());
            aliases = derived.columns(); qualifier = derived.alias() == null ? List.of() : List.of(derived.alias());
        } else throw new SqlException("0A000", "relation source is not implemented yet");
        QueryEnvironment bodyEnvironment = environment;
        SqlParser.Statement relationBody = syntax instanceof SqlParser.Derived derived ? derived.query() : inlineBody;
        try {
            if (aliases.size() > input.types().size()) throw new SqlException("42P10", "too many relation column aliases");
            List<ScalarPlan.Binding> bindings = new ArrayList<>();
            for (int c = 0; c < input.types().size(); c++) bindings.add(new ScalarPlan.Binding(qualifier, c < aliases.size() ? aliases.get(c) : input.names().get(c), input.types().get(c), false, false, input.collations().get(c).asColumn()));
            return new RelationPlan() {
                private QueryPlan current = input;
                private SqlParser.Statement body = relationBody;
                private boolean started;
                private boolean planned = relationBody == null || syntax instanceof SqlParser.Derived derived && derived.query() instanceof SqlParser.Values;
                private final PlanMetrics metrics = new PlanMetrics();
                public void requireAnalyze() { explain(false); current.requireAnalyze(); }
                public PlanNode actualPlan(boolean verbose) {
                    PlanNode node = explain(verbose);
                    return new PlanNode(node.label(), node.detail(), node.rows(), metrics.total(), syntax instanceof SqlParser.Table && relationBody == null ? List.of() : List.of(current.actualPlan(verbose)), node.lane());
                }
                private SqlParser.Expr referenceFilter;
                private final ScalarPlan.Frame referenceFrame = new ScalarPlan.Frame();
                private ScalarPlan.Expression referencePredicate;
                public boolean push(SqlParser.Expr predicate) {
                    if (started || bodyEnvironment.shapeOnly() || !FilterRules.belongs(predicate, bindings) || !FilterRules.safe(predicate, bodyEnvironment.resolver(bindings, new ScalarPlan.Frame()))) return false;
                    if (body == null) return false;
                    SqlParser.Statement replacement = pushStatement(body, predicate, bindings, bodyEnvironment);
                    if (replacement == null) return false;
                    body = replacement; planned = false; return true;
                }
                public boolean pushInJoin(SqlParser.Expr predicate) { return push(predicate) || pushScan(predicate); }
                public boolean pushScan(SqlParser.Expr predicate) {
                    if (started || bodyEnvironment.shapeOnly() || !FilterRules.belongs(predicate, bindings) || !FilterRules.safe(predicate, bodyEnvironment.resolver(bindings, referenceFrame))) return false;
                    referenceFilter = FilterRules.and(referenceFilter, predicate);
                    referencePredicate = bodyEnvironment.resolver(bindings, referenceFrame).predicate(referenceFilter);
                    return true;
                }
                public List<ScalarPlan.Binding> bindings() { return List.copyOf(bindings); }
                public List<String> names() { return bindings.stream().map(ScalarPlan.Binding::name).toList(); }
                public List<SqlType> types() { return input.types(); }
                public void touch(boolean[] mask) { }
                public boolean projectedResult() { return relationBody != null; }
                public void collect(RowSpool target, CostMeter meter) {
                    if (referencePredicate != null) { RelationPlan.super.collect(target, meter); return; }
                    started = true; if (!planned) { QueryPlan next = reader.apply(body, bodyEnvironment.execution()); current.close(); current = next; planned = true; }
                    current.collect(target, meter);
                }
                public PlanNode explain(boolean verbose) {
                    if (!planned) { QueryPlan replacement = reader.apply(body, bodyEnvironment.execution()); current.close(); current = replacement; planned = true; }
                    PlanNode child = current.explain(verbose); String label = syntax instanceof SqlParser.Table table ? "CTE Scan " + table.name().parts().getLast() : "Subquery" + (qualifier.isEmpty() ? "" : " " + qualifier.getFirst());
                    long count = referenceFilter == null ? child.rows() : PlanEstimate.rows(referenceFilter, child.rows(), List.of());
                    long work = referenceFilter == null ? 0 : PlanEstimate.mul(child.rows(), PlanEstimate.operations(referenceFilter));
                    String detail = referenceFilter == null ? "" : verbose ? "filter=" + new ExplainExpression(bindings, bodyEnvironment.resolver(bindings, referenceFrame), bodyEnvironment.meter()).print(TableExplanation.orderedPredicate(referenceFilter, bodyEnvironment)) : "filter:conjuncts=" + FilterRules.terms(referenceFilter).size();
                    return new PlanNode(label, detail, count, PlanEstimate.add(child.cost(), work), syntax instanceof SqlParser.Table && relationBody == null ? List.of() : List.of(child), "buffered");
                }
                public Row next(CostMeter meter) { long before = meter.cost(); try { return nextMeasured(meter); } finally { metrics.total(before, meter); } }
                private Row nextMeasured(CostMeter meter) { started = true; if (!planned) { QueryPlan next = reader.apply(body, bodyEnvironment.execution()); current.close(); current = next; planned = true; } Row row;
                    while ((row = current.next(meter)) != null) { referenceFrame.values = row.values(); if (referencePredicate != null) { SqlValue allowed = referencePredicate.run(meter); if (allowed.isNull() || !allowed.asBoolean()) continue; } return new Row(names(), row.values()); }
                    return null; }
                public void close() { current.close(); }
            };
        } catch (RuntimeException | Error failure) { input.close(); throw failure; }
    }

    private static SqlParser.Expr accessTerms(SqlParser.Expr filter) {
        SqlParser.Expr result = null;
        for (SqlParser.Expr term : FilterRules.terms(filter)) if (FilterRules.accessHint(term)) result = FilterRules.and(result, term);
        return result;
    }

    private SqlParser.Statement pushStatement(SqlParser.Statement query, SqlParser.Expr predicate, List<ScalarPlan.Binding> output, QueryEnvironment environment) {
        boolean matching = query instanceof SqlParser.SetOperation set && !(set.kind().equals("union") && set.all())
            || query instanceof SqlParser.Select select && (select.distinct() || AggregatePlan.needed(select) || WindowSyntax.needed(select));
        if (matching && !FilterRules.preservesEquality(predicate, environment.resolver(output, new ScalarPlan.Frame()))) return null;
        if (query instanceof SqlParser.Select select) return pushBody(select, predicate, output, environment);
        if (query instanceof SqlParser.SetOperation set) {
            SqlParser.Statement left = pushStatement(set.left(), predicate, output, environment), right = pushStatement(set.right(), predicate, output, environment);
            return left == null || right == null ? null : new SqlParser.SetOperation(left, right, set.kind(), set.all());
        }
        if (query instanceof SqlParser.QueryTail tail && tail.limit() == null && tail.offset() == null) {
            SqlParser.Statement body = pushStatement(tail.query(), predicate, output, environment);
            return body == null ? null : new SqlParser.QueryTail(body, tail.order(), null, null);
        }
        return null;
    }

    private SqlParser.Select pushBody(SqlParser.Select query, SqlParser.Expr predicate, List<ScalarPlan.Binding> output, QueryEnvironment environment) {
        if (query.limit() != null || query.offset() != null || !query.distinctOn().isEmpty() || query.from().isEmpty()) return null;
        if (AggregatePlan.needed(query) && (query.group().isEmpty() || query.group().stream().anyMatch(key -> !(key instanceof SqlParser.Name)))) return null;
        SqlParser.From syntax = query.from().getFirst();
        for (int i = 1; i < query.from().size(); i++) syntax = new SqlParser.Join(syntax, query.from().get(i), "cross", null, List.of(), false);
        List<SqlParser.Expr> columns = new ArrayList<>();
        List<SqlType> columnTypes = new ArrayList<>();
        List<SqlParser.Name> dependentColumns = new ArrayList<>();
        try (RelationPlan input = relation(syntax, environment.shape())) {
            ScalarPlan.Resolver resolver = environment.resolver(input.bindings(), new ScalarPlan.Frame());
            for (SqlParser.Item item : ProjectionSyntax.expand(query.items(), resolver)) {
                if (item.expression() instanceof SqlParser.Star star) {
                    for (ScalarPlan.Binding binding : input.bindings()) if (star.qualifier().isEmpty() ? !binding.qualifiedOnly() : star.qualifier().equals(binding.qualifier())) {
                        List<String> parts = new ArrayList<>(binding.qualifier()); parts.add(binding.name()); columns.add(new SqlParser.Name(parts));
                    }
                } else columns.add(item.expression());
            }
            for (SqlParser.Expr column : columns) columnTypes.add(column instanceof SqlParser.Name || column instanceof SqlParser.ColumnSlot ? resolver.resolve(column, null).type() : SqlType.UNKNOWN);
            List<ScalarPlan.Binding> inputBindings = input.bindings();
            for (RelationPlan.FunctionalKey key : input.functionalKeys()) {
                boolean grouped = key.determinant().stream().allMatch(index -> {
                    ScalarPlan.Binding binding = inputBindings.get(index);
                    List<String> parts = new ArrayList<>(binding.qualifier()); parts.add(binding.name());
                    return query.group().stream().anyMatch(term -> sameColumn(term, new SqlParser.Name(parts)));
                });
                if (grouped) for (int index : key.dependent()) {
                    ScalarPlan.Binding binding = inputBindings.get(index);
                    List<String> parts = new ArrayList<>(binding.qualifier()); parts.add(binding.name()); dependentColumns.add(new SqlParser.Name(parts));
                }
            }
        }
        if (columns.size() != output.size()) return null;
        Map<String, SqlParser.WindowSpec> definitions = WindowSyntax.definitions(query);
        List<SqlParser.Window> windows = new ArrayList<>();
        for (SqlParser.Item item : query.items()) WindowSyntax.map(item.expression(), value -> { if (value instanceof SqlParser.Window window) windows.add(window); return value; });
        for (SqlParser.Order order : query.order()) WindowSyntax.map(order.expression(), value -> { if (value instanceof SqlParser.Window window) windows.add(window); return value; });
        boolean[] admitted = {true};
        SqlParser.Expr translated = WindowSyntax.map(predicate, value -> {
            if (value instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.INTEGER) {
                try { long integer = Long.parseLong(literal.text()); if (integer < Integer.MIN_VALUE || integer > Integer.MAX_VALUE) return new SqlParser.StoredValue(SqlValue.ofInteger(integer)); }
                catch (NumberFormatException ignored) { /* Resolver retains the original numeric error. */ }
            }
            if (!(value instanceof SqlParser.Name name)) return value;
            int index = -1;
            for (int i = 0; i < output.size(); i++) if (output.get(i).name().equals(name.parts().getLast()) && (name.parts().size() == 1 || name.parts().subList(0, name.parts().size() - 1).equals(output.get(i).qualifier()))) { if (index >= 0) { admitted[0] = false; return value; } index = i; }
            if (index < 0 || output.get(index).collation().inheritedMissing() || !(columns.get(index) instanceof SqlParser.Name column)) { admitted[0] = false; return value; }
            if (AggregatePlan.needed(query) && query.group().stream().noneMatch(key -> sameColumn(key, column)) && dependentColumns.stream().noneMatch(key -> sameColumn(key, column))) { admitted[0] = false; return value; }
            for (SqlParser.Window window : windows) {
                SqlParser.WindowSpec spec = window.reference() == null ? WindowSyntax.merge(window.spec(), definitions) : definitions.get(window.reference());
                if (spec == null || spec.partition().stream().noneMatch(key -> sameColumn(key, column))) { admitted[0] = false; return value; }
            }
            SqlType source = columnTypes.get(index), target = output.get(index).type();
            if (source.equals(target) || source.isInteger() && target.isInteger()) return column;
            if (source.isInteger() && target.kind() == SqlType.Kind.DECIMAL || source == SqlType.F32 && target == SqlType.F64)
                return new SqlParser.Cast(column, new SqlParser.TypeName(target.kind().name().toLowerCase(java.util.Locale.ROOT), -1, -1, 0));
            admitted[0] = false; return value;
        });
        if (!admitted[0]) return null;
        return new SqlParser.Select(query.items(), FilterRules.and(query.where(), translated), query.order(), query.limit(), query.offset(), query.distinct(), query.from(), query.group(), query.having(), query.distinctOn(), query.windows());
    }
    private static boolean sameColumn(SqlParser.Expr left, SqlParser.Name right) {
        return left instanceof SqlParser.Name name && name.parts().getLast().equals(right.parts().getLast()) && (name.parts().size() == 1 || right.parts().size() == 1 || name.parts().equals(right.parts()));
    }

    private static SqlParser.Expr nullRejectingAccessTerms(SqlParser.Expr filter) {
        SqlParser.Expr result = null;
        for (SqlParser.Expr term : FilterRules.terms(filter)) if (FilterRules.accessHint(term) && FilterRules.nullRejecting(term)) result = FilterRules.and(result, term);
        return result;
    }
    private static long finishJoinEstimate(long cost, long rows, SqlParser.Expr filter) {
        cost = PlanEstimate.add(cost, PlanEstimate.mul(rows, PlanEstimate.operations(filter)));
        return PlanEstimate.add(cost, PlanEstimate.rows(filter, rows, List.of()));
    }
    private RelationPlan rightInput(SqlParser.From syntax, QueryEnvironment environment) {
        RelationPlan input = relation(syntax, syntax instanceof SqlParser.Join ? environment.lateralFence() : environment);
        boolean emit = syntax instanceof SqlParser.Grouped group && group.alias() == null || syntax instanceof SqlParser.Join join && hasRightFull(join);
        return emit ? produced(input) : input;
    }
    private static boolean hasRightFull(SqlParser.From syntax) {
        return syntax instanceof SqlParser.Join join && (join.kind().equals("right") || join.kind().equals("full") || hasRightFull(join.left()) || hasRightFull(join.right()));
    }
    private static RelationPlan metadata(RelationPlan input, List<ScalarPlan.Binding> bindings) {
        List<ScalarPlan.Binding> fields = List.copyOf(bindings);
        return new RelationPlan() {
            public List<ScalarPlan.Binding> bindings() { return fields; }
            public List<FunctionalKey> functionalKeys() { return input.functionalKeys(); }
            public List<String> names() { return input.names(); }
            public List<SqlType> types() { return input.types(); }
            public void touch(boolean[] columns) { input.touch(columns); }
            public boolean orderedBy(List<SqlParser.Order> order) { return input.orderedBy(order); }
            public boolean push(SqlParser.Expr predicate) { return input.push(predicate); }
            public PlanNode explain(boolean verbose) { return input.explain(verbose); }
            public void requireAnalyze() { input.requireAnalyze(); }
            public PlanNode actualPlan(boolean verbose) { return input.actualPlan(verbose); }
            public Row next(CostMeter meter) { return input.next(meter); }
            public void close() { input.close(); }
        };
    }
    private static RelationPlan produced(RelationPlan input) {
        return new RelationPlan() {
            private long emitted;
            public PlanNode explain(boolean verbose) {
                PlanNode body = input.explain(verbose); body = body.finish(body.detail(), PlanEstimate.mul(body.rows(), Costs.ROW_PRODUCED), body.lane());
                return new PlanNode("Subquery", "", body.rows(), body.cost(), List.of(body), "buffered");
            }
            public void requireAnalyze() { input.requireAnalyze(); }
            public PlanNode actualPlan(boolean verbose) {
                PlanNode body = input.actualPlan(verbose); body = body.finish(body.detail(), PlanEstimate.mul(emitted, Costs.ROW_PRODUCED), body.lane());
                return new PlanNode("Subquery", "", body.rows(), body.cost(), List.of(body), "buffered");
            }
            public List<ScalarPlan.Binding> bindings() { return input.bindings(); }
            public List<String> names() { return input.names(); }
            public List<SqlType> types() { return input.types(); }
            public void touch(boolean[] columns) { boolean[] full = new boolean[columns.length]; java.util.Arrays.fill(full, true); input.touch(full); }
            public Row next(CostMeter meter) { Row row = input.next(meter); if (row != null) { meter.charge(Costs.ROW_PRODUCED); meter.guard(); emitted++; } return row; }
            public void close() { input.close(); }
        };
    }
    private static boolean lateralEligible(SqlParser.From source) {
        if (source instanceof SqlParser.TableFunction || source instanceof SqlParser.JsonTable || source instanceof SqlParser.Derived d && d.lateral()) return true;
        if (source instanceof SqlParser.Grouped group) return lateralEligible(group.source());
        return source instanceof SqlParser.Join join && (lateralEligible(join.left()) || lateralEligible(join.right()));
    }
    private static RelationPlan aliasGroup(RelationPlan source, SqlParser.Grouped group) {
        List<Integer> visible = new ArrayList<>(); List<ScalarPlan.Binding> bindings = new ArrayList<>();
        for (int i = 0; i < source.bindings().size(); i++) if (!source.bindings().get(i).qualifiedOnly()) visible.add(i);
        if (group.columns().size() > visible.size()) { source.close(); throw new SqlException("42P10", "too many relation column aliases"); }
        for (int i = 0; i < visible.size(); i++) {
            var original = source.bindings().get(visible.get(i));
            bindings.add(new ScalarPlan.Binding(List.of(group.alias()), i < group.columns().size() ? group.columns().get(i) : original.name(), original.type(), false, false, original.collation().asColumn()));
        }
        List<ScalarPlan.Binding> result = List.copyOf(bindings);
        return new RelationPlan() {
            public List<ScalarPlan.Binding> bindings() { return result; }
            public PlanNode explain(boolean verbose) { return source.explain(verbose); }
            public void requireAnalyze() { source.requireAnalyze(); }
            public PlanNode actualPlan(boolean verbose) { return source.actualPlan(verbose); }
            public List<String> names() { return result.stream().map(ScalarPlan.Binding::name).toList(); }
            public List<SqlType> types() { return result.stream().map(ScalarPlan.Binding::type).toList(); }
            public void touch(boolean[] columns) { boolean[] mask = new boolean[source.bindings().size()]; for (int i = 0; i < columns.length; i++) mask[visible.get(i)] = columns[i]; source.touch(mask); }
            public Row next(CostMeter meter) { Row row = source.next(meter); if (row == null) return null; List<SqlValue> values = new ArrayList<>(); for (int index : visible) values.add(row.get(index)); return new Row(names(), values); }
            public void close() { source.close(); }
        };
    }

}
