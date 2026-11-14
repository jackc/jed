package org.jed;

import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;

/** Projection over a materialized query relation, with independent filtering and ordering. */
final class ProjectionPlan implements QueryPlan {
    private final QueryPlan source;
    private final SqlParser.Select querySyntax;
    private final ScalarPlan.Resolver resolverMetadata;
    private final List<ScalarPlan.Binding> bindingMetadata;
    private final QueryEnvironment environmentMetadata;
    private List<SqlParser.Expr> outputMetadata;
    private final ScalarPlan.Frame frame = new ScalarPlan.Frame();
    private final List<String> names = new ArrayList<>();
    private final List<ScalarPlan.Expression> projections = new ArrayList<>(), orderKeys = new ArrayList<>();
    private final List<SqlParser.Order> order;
    private final List<Integer> orderProjections = new ArrayList<>();
    private final ScalarPlan.Expression where, limit, offset;
    private final Path scratch;
    private final long workMem;
    private final boolean distinct;
    private final DistinctOn distinctOn;
    private final boolean produce;
    private final boolean orderElided, streaming;
    private RowSpool rows;
    private RowSpool.Cursor cursor;
    private ExternalSorter sorter;
    private DistinctState seen;
    private HostBatch.Window hostWindow;
    private boolean started, closed;
    private long remaining, skip;
    ProjectionPlan(SqlParser.Select query, QueryPlan source, List<ScalarPlan.Binding> bindings, QueryEnvironment environment, Path scratch, long workMem) {
        this(query, source, bindings, environment, scratch, workMem, true);
    }
    ProjectionPlan(SqlParser.Select query, QueryPlan source, List<ScalarPlan.Binding> bindings, QueryEnvironment environment, Path scratch, long workMem, boolean produce) {
        this.produce = produce; querySyntax = query; bindingMetadata = List.copyOf(bindings); environmentMetadata = environment;
        environment.captureStars(query.items(), bindings);
        this.source = source; this.scratch = scratch; this.workMem = workMem; distinct = query.distinct() && query.distinctOn().isEmpty();
        ScalarPlan.Resolver resolver = resolverMetadata = environment.resolver(bindings, frame);
        List<SqlParser.Expr> syntax = new ArrayList<>();
        List<SqlParser.Order> resolvedOrder = new ArrayList<>();
        for (SqlParser.Item item : ProjectionSyntax.expand(query.items(), resolver)) {
            if (item.expression() instanceof SqlParser.Star star) {
                int matched = 0;
                for (int c = 0; c < bindings.size(); c++) {
                    ScalarPlan.Binding binding = bindings.get(c);
                    if (environment.starVisible(star, binding) && (star.qualifier().isEmpty() && !binding.qualifiedOnly() || !star.qualifier().isEmpty() && star.qualifier().equals(binding.qualifier()))) {
                        names.add(binding.name()); syntax.add(new SqlParser.ColumnSlot(c)); matched++;
                    }
                }
                if (matched == 0 && !star.qualifier().isEmpty()) throw new SqlException("42P01", "unknown star qualifier");
            } else { names.add(item.alias() == null ? ScalarPlan.outputName(item.expression()) : item.alias()); syntax.add(item.expression()); }
        }
        outputMetadata = List.copyOf(syntax);
        for (SqlParser.Expr expression : syntax) projections.add(resolver.resolve(expression, null));
        if (query.distinct() && query.distinctOn().isEmpty()) for (int i = 0; i < projections.size(); i++) { ScalarPlan.requireComparable(projections.get(i).type()); CollationDerivation.requireSort(projections.get(i), syntax.get(i)); }
        if (AggregatePlan.contains(query.where())) throw new SqlException("42803", "aggregate in WHERE");
        where = query.where() == null ? null : resolver.predicate(query.where());
        if (where != null && where.type() != SqlType.BOOLEAN) throw SqlException.typeMismatch("WHERE requires boolean");
        distinctOn = query.distinctOn().isEmpty() ? null : new DistinctOn(query.distinctOn(), query.order(), syntax, names, bindings, resolver);
        order = distinctOn == null ? query.order() : distinctOn.order;
        for (SqlParser.Order item : order) {
            SqlParser.Expr expression = item.expression();
            if (expression instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.INTEGER) {
                int position;
                try { position = ScalarPlan.ordinal(literal.text()); } catch (NumberFormatException error) { throw new SqlException("42P10", "ORDER BY position out of range"); }
                if (position < 1 || position > syntax.size()) throw new SqlException("42P10", "ORDER BY position out of range");
                expression = syntax.get(position - 1);
            } else if (expression instanceof SqlParser.Name name && name.parts().size() == 1 && names.contains(name.parts().getFirst())) {
                int first = names.indexOf(name.parts().getFirst());
                for (int i = first + 1; i < names.size(); i++) if (names.get(i).equals(name.parts().getFirst()) && !syntax.get(first).equals(syntax.get(i))) throw new SqlException("42702", "ambiguous ORDER BY label");
                expression = syntax.get(first);
            }
            if (distinct && !syntax.contains(expression)) throw new SqlException("42P10", "DISTINCT ORDER BY must appear in select list");
            resolvedOrder.add(new SqlParser.Order(expression, item.descending(), item.nullsFirst()));
            orderKeys.add(resolver.resolve(expression, null)); ScalarPlan.requireComparable(orderKeys.getLast().type()); CollationDerivation.requireSort(orderKeys.getLast(), expression); orderProjections.add(syntax.indexOf(expression));
        }
        orderElided = !order.isEmpty() && !distinct && distinctOn == null && source instanceof RelationPlan relation && relation.orderedBy(resolvedOrder)
            && (!(source instanceof JoinPlan || source instanceof LateralJoinPlan || source instanceof ReorderedRelation) || query.limit() != null);
        streaming = orderElided && (source instanceof JoinPlan || source instanceof LateralJoinPlan || source instanceof ReorderedRelation);
        ScalarPlan.Resolver constants = environment.resolver();
        limit = query.limit() == null ? null : constants.resolve(query.limit(), SqlType.I64);
        offset = query.offset() == null ? null : constants.resolve(query.offset(), SqlType.I64);
        if (limit != null && !limit.type().isInteger() || offset != null && !offset.type().isInteger()) throw SqlException.typeMismatch("row bound requires integer");
        if (source instanceof RelationPlan relation) relation.touch(resolver.touched());
    }
    private final PlanMetrics metrics = new PlanMetrics();
    public void requireAnalyze() { source.requireAnalyze(); }
    public PlanNode actualPlan(boolean verbose) { return describe(verbose, true); }
    @Override public PlanNode explain(boolean verbose) { return describe(verbose, false); }
    private PlanNode describe(boolean verbose, boolean actual) {
        PlanNode node = actual ? source.actualPlan(verbose) : source.explain(verbose);
        if (orderElided && !(source instanceof WindowPlan)) node = node.finish(TableExplanation.append(node.detail(), "ordered: join pk ordered"), 0, node.lane());
        ExplainExpression printer = new ExplainExpression(bindingMetadata, resolverMetadata, environmentMetadata.meter());
        if (querySyntax.where() != null) node = node.wrap("Filter", (verbose ? "filter=" + printer.print(TableExplanation.orderedPredicate(querySyntax.where(), environmentMetadata)) : "conjuncts=" + FilterRules.terms(querySyntax.where()).size()) + (environmentMetadata.contradiction(querySyntax.where()) ? "; contradiction" : ""), PlanEstimate.rows(TableExplanation.orderedPredicate(querySyntax.where(), environmentMetadata), node.rows(), List.of()), actual ? metrics.phase("Filter") : PlanEstimate.mul(node.rows(), PlanEstimate.operations(querySyntax.where())));
        if (distinct) node = node.wrap("Distinct", "", PlanEstimate.groups(node.rows(), projections.size()), actual ? metrics.phase("Distinct") : 0);
        long take = TableExplanation.boundEstimate(querySyntax.limit(), Long.MAX_VALUE, resolverMetadata, environmentMetadata.meter()), skip = TableExplanation.boundEstimate(querySyntax.offset(), 0, resolverMetadata, environmentMetadata.meter());
        if (!order.isEmpty() && !orderElided) {
            long work = 0; for (SqlParser.Order item : order) work = PlanEstimate.add(work, PlanEstimate.operations(item.expression()));
            node = node.wrap("Sort", "keys=" + order.size() + (take == Long.MAX_VALUE || !produce || distinct || distinctOn != null ? "" : ", top-k=" + PlanEstimate.add(take, skip)), node.rows(), actual ? metrics.phase("Sort") : PlanEstimate.mul(node.rows(), work));
        }
        if (distinctOn != null) node = node.wrap("Distinct", "on=" + distinctOn.keys.size(), PlanEstimate.groups(node.rows(), distinctOn.keys.size()), actual ? metrics.phase("Distinct") : 0);
        if (querySyntax.limit() != null || querySyntax.offset() != null) node = node.wrap("Limit", (take == Long.MAX_VALUE ? "" : "limit=" + take) + (querySyntax.offset() == null ? "" : (take == Long.MAX_VALUE ? "" : " ") + "offset=" + skip), Math.min(take, Math.max(0, node.rows() - skip)), 0);
        long work = produce ? 1 : 0; for (SqlParser.Expr output : outputMetadata) work = PlanEstimate.add(work, PlanEstimate.operations(output));
        String detail = verbose ? TableExplanation.append(node.detail(), "output=[" + printer.list(outputMetadata) + "]") : node.detail();
        return actual ? new PlanNode(node.label(), detail, node.rows(), metrics.total(), node.children(), "buffered") : node.finish(detail, PlanEstimate.mul(node.rows(), work), "buffered");
    }
    @Override public List<String> names() { return List.copyOf(names); }
    @Override public List<CollationDerivation> collations() { return projections.stream().map(ScalarPlan.Expression::collation).toList(); }
    @Override public List<SqlType> types() { return projections.stream().map(ScalarPlan.Expression::type).toList(); }
    void prepareInput(CostMeter meter) { if (!started) { long before = meter.cost(); try { initialize(meter); } finally { metrics.total(before, meter); } } }
    private void initialize(CostMeter meter) {
        started = true; remaining = bound(limit, meter, Long.MAX_VALUE, "2201W"); skip = bound(offset, meter, 0, "2201X");
        if (streaming) { ((RelationPlan)source).prepare(meter); return; }
        rows = new RowSpool(source.types(), 0, null, meter.memory());
        Row row;
        while ((row = source.next(meter)) != null) rows.append(row.values());
        source.close();
        cursor = rows.cursor();
        if (distinct) seen = new DistinctState(scratch, workMem, meter.memory());
        if (!order.isEmpty() && !orderElided || distinct) {
            List<SqlType> storedTypes = new ArrayList<>(distinct ? types() : source.types());
            for (ScalarPlan.Expression key : orderKeys) storedTypes.add(CollationDerivation.sortType(key));
            int keyOffset = storedTypes.size() - orderKeys.size();
            sorter = new ExternalSorter(storedTypes, workMem, scratch, meter.memory(), (a, b) -> {
                for (int i = 0; i < order.size(); i++) {
                    SqlValue x = a.get(keyOffset + i), y = b.get(keyOffset + i); SqlParser.Order item = order.get(i);
                    boolean nullFirst = item.nullsFirst() == null ? item.descending() : item.nullsFirst();
                    int comparison = x.isNull() || y.isNull() ? x.isNull() == y.isNull() ? 0 : x.isNull() == nullFirst ? -1 : 1 : ScalarPlan.compare(x, y, null) * (item.descending() ? -1 : 1);
                    if (comparison != 0) return comparison;
                }
                return 0;
            });
            List<SqlValue> input;
            while ((input = cursor.next()) != null) {
                frame.values = input; if (!matches(meter)) continue;
                List<SqlValue> stored = new ArrayList<>();
                if (distinct) {
                    long beforeDistinct = meter.cost(); stored.addAll(project(meter, false)); metrics.add("Distinct", beforeDistinct, meter);
                } else stored.addAll(input);
                long beforeSort = meter.cost();
                for (int i = 0; i < orderKeys.size(); i++) { SqlValue value = orderKeys.get(i).run(meter); if (orderKeys.get(i).collation().require().equals("C") || !CollationDerivation.collatable(value.type())) { meter.charge(MemoryAccount.payload(value) * Costs.SORT_KEY); meter.guard(); } stored.add(CollationDerivation.sortValue(orderKeys.get(i), value, meter)); }
                metrics.add("Sort", beforeSort, meter);
                sorter.append(stored);
            }
            cursor.close(); rows.close(); cursor = sorter.finish();
            if (distinct) {
                rows = new RowSpool(types(), 0, null, meter.memory());
                List<SqlValue> sorted;
                while ((sorted = cursor.next()) != null) {
                    List<SqlValue> projected = sorted.subList(0, projections.size());
                    long beforeDistinct = meter.cost();
                    for (SqlValue value : projected) meter.charge(MemoryAccount.payload(value) * Costs.HASH_KEY);
                    meter.guard();
                    if (seen.add(projected)) rows.append(projected);
                    metrics.add("Distinct", beforeDistinct, meter);
                }
                cursor.close(); sorter.close(); sorter = null; cursor = rows.cursor();
            }
        }
    }
    @Override public Row next(CostMeter meter) {
        long before = meter.cost();
        try { return nextMeasured(meter); } finally { metrics.total(before, meter); }
    }
    private Row nextMeasured(CostMeter meter) {
        if (!started) initialize(meter);
        List<SqlValue> input;
        while (remaining > 0 && (input = nextInput(meter)) != null) {
            frame.values = input;
            if ((order.isEmpty() || orderElided) && !distinct && !matches(meter)) continue;
            if (distinctOn != null && !distinctOn.accept(input, source.types().size())) continue;
            if (skip > 0) { skip--; continue; }
            remaining--;
            if (distinct) { meter.charge(Costs.ROW_PRODUCED); meter.guard(); return new Row(names, input.subList(0, projections.size())); }
            return new Row(names, project(meter, true));
        }
        return null;
    }
    private List<SqlValue> nextInput(CostMeter meter) {
        if (!streaming) {
            if (distinct || resolverMetadata.hostCalls.isEmpty()) return cursor.next();
            if (hostWindow == null) {
                var roots = new ArrayList<>(outputMetadata);
                if ((order.isEmpty() || orderElided) && querySyntax.where() != null) roots.add(querySyntax.where());
                hostWindow = new HostBatch.Window(roots, resolverMetadata, frame, () -> cursor.next());
            }
            return hostWindow.next(meter, CostMeter.saturatingAdd(remaining, skip), produce ? Costs.ROW_PRODUCED : 0);
        }
        Row row = source.next(meter); return row == null ? null : row.values();
    }
    private List<SqlValue> project(CostMeter meter, boolean emit) {
        if (emit && produce) meter.charge(Costs.ROW_PRODUCED); meter.guard();
        List<SqlValue> result = new ArrayList<>(); for (ScalarPlan.Expression expression : projections) result.add(expression.run(meter)); return result;
    }
    private boolean matches(CostMeter meter) { long before = meter.cost(); try { if (where == null) return true; SqlValue value = where.run(meter); return !value.isNull() && value.asBoolean(); } finally { metrics.add("Filter", before, meter); } }
    private static long bound(ScalarPlan.Expression expression, CostMeter meter, long fallback, String state) {
        if (expression == null) return fallback; SqlValue value = expression.run(meter); if (value.isNull()) return fallback;
        if (value.asInteger() < 0) throw new SqlException(state, "negative row bound"); return value.asInteger();
    }
    @Override public void close() { if (closed) return; closed = true; if (hostWindow != null) hostWindow.close(); source.close(); if (cursor != null) cursor.close(); if (rows != null) rows.close(); if (sorter != null) sorter.close(); if (seen != null) seen.close(); }
}
