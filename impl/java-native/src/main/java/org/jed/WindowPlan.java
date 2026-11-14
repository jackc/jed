package org.jed;

import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/** Blocking window stage over charged rows. Partitions preserve first occurrence and stable peers. */
final class WindowPlan implements RelationPlan {
    private record Keys(List<SqlParser.Expr> partition, List<SqlParser.Order> order) {}
    private record Group(Keys syntax, List<ScalarPlan.Expression> partition, List<ScalarPlan.Expression> order) {}
    private record Spec(SqlParser.Window syntax, Group group, SqlType type, List<ScalarPlan.Expression> args,
                        AggregatePlan.Fold fold, ScalarPlan.Expression lower, ScalarPlan.Expression upper) {}
    private final QueryPlan source;
    private final PlanMetrics metrics = new PlanMetrics();
    public void requireAnalyze() { source.requireAnalyze(); }
    public PlanNode actualPlan(boolean verbose) {
        PlanNode estimate = explain(verbose); PlanNode input = source.actualPlan(false);
        if (whereSyntax != null) { PlanNode filter = estimate.children().getFirst(); input = new PlanNode(filter.label(), filter.detail(), filter.rows(), PlanEstimate.add(input.cost(), metrics.phase("Filter")), List.of(input), filter.lane()); }
        return new PlanNode(estimate.label(), estimate.detail(), estimate.rows(), metrics.total(), List.of(input), estimate.lane());
    }
    private final SqlParser.Expr whereSyntax;
    private final List<ScalarPlan.Binding> bindings;
    private final ScalarPlan.Frame frame = new ScalarPlan.Frame();
    private final ScalarPlan.Resolver resolver;
    private final QueryEnvironment environment;
    private final List<Spec> specs = new ArrayList<>();
    private final LinkedHashMap<Keys, Group> groups = new LinkedHashMap<>();
    private final Map<String, SqlParser.WindowSpec> definitions;
    private final List<List<SqlValue>> rows = new ArrayList<>();
    private final ScalarPlan.Expression where;
    private final SqlParser.Select rewritten;
    private final ScalarPlan.Expression prefixLimit, prefixOffset;
    private final SqlParser.Select querySyntax;
    private MemoryAccount memory;
    private long reserved;
    private int next;
    private boolean started, closed;
    WindowPlan(SqlParser.Select query, QueryPlan source, List<ScalarPlan.Binding> input, QueryEnvironment environment) {
        this.source = source; this.environment = environment; querySyntax = query; whereSyntax = query.where(); bindings = new ArrayList<>(input);
        resolver = environment.resolver(input, frame); definitions = WindowSyntax.definitions(query);
        if (WindowSyntax.contains(query.where()) || WindowSyntax.contains(query.having()) || query.group().stream().anyMatch(WindowSyntax::contains)) throw new SqlException("42P20", "window function is not allowed in this clause");
        where = query.where() == null ? null : query.where() instanceof SqlParser.Binary binary && binary.op().equals("and") ? resolver.binary(binary, true) : resolver.resolve(query.where(), SqlType.BOOLEAN);
        if (where != null && where.type() != SqlType.BOOLEAN) throw SqlException.typeMismatch("WHERE requires boolean");
        List<SqlParser.Item> items = new ArrayList<>();
        for (SqlParser.Item item : ProjectionSyntax.expand(query.items(), resolver)) {
            if (item.expression() instanceof SqlParser.Star star) {
                for (int i = 0; i < input.size(); i++) if (environment.starVisible(star, input.get(i)) && (star.qualifier().isEmpty() ? !input.get(i).qualifiedOnly() : star.qualifier().equals(input.get(i).qualifier()))) items.add(new SqlParser.Item(new SqlParser.ColumnSlot(i), input.get(i).name()));
            } else items.add(new SqlParser.Item(rewrite(item.expression()), item.alias() == null ? ScalarPlan.outputName(item.expression()) : item.alias()));
        }
        List<SqlParser.Order> order = query.order().stream().map(o -> new SqlParser.Order(rewrite(o.expression()), o.descending(), o.nullsFirst())).toList();
        rewritten = new SqlParser.Select(items, null, order, query.limit(), query.offset(), query.distinct(), List.of(), List.of(), null, query.distinctOn().stream().map(this::rewrite).toList());
        for (SqlParser.WindowSpec definition : definitions.values()) validate(definition);
        boolean prefix = source instanceof TableRelation table && !query.distinct() && query.limit() != null && table.orderedBy(query.order(), false) && specs.stream().allMatch(spec -> prefixSafe(spec, table));
        prefixLimit = prefix ? environment.resolver().resolve(query.limit(), SqlType.I64) : null;
        prefixOffset = prefix && query.offset() != null ? environment.resolver().resolve(query.offset(), SqlType.I64) : null;
        if (source instanceof RelationPlan relation) {
            boolean[] touched = resolver.touched();
            ScalarPlan.Resolver outputs = environment.resolver(bindings, frame);
            for (SqlParser.Item item : items) outputs.resolve(item.expression(), null);
            for (int i = 0; i < touched.length; i++) touched[i] |= outputs.touched()[i];
            // Presentation ORDER BY may use a source column not present in the output list.
            for (SqlParser.Order item : order) if (!(item.expression() instanceof SqlParser.Name name && name.parts().size() == 1 && items.stream().anyMatch(v -> v.alias().equals(name.parts().getFirst())))) {
                if (!(item.expression() instanceof SqlParser.Literal)) outputs.resolve(item.expression(), null);
            }
            for (int i = 0; i < touched.length; i++) touched[i] |= outputs.touched()[i];
            relation.touch(touched);
        }
    }
    @Override public PlanNode explain(boolean verbose) {
        PlanNode node = prefixLimit != null && whereSyntax == null && source instanceof TableRelation table
            ? table.explainPrefix(verbose, querySyntax.order(), querySyntax.limit(), querySyntax.offset()) : source.explain(verbose);
        if (prefixLimit != null && !node.detail().contains("ordered: pk ordered")) node = node.finish(TableExplanation.append(node.detail(), "ordered: pk ordered"), 0, node.lane());
        if (whereSyntax != null) node = node.wrap("Filter", verbose ? "filter=" + new ExplainExpression(bindings, resolver, environment.meter()).print(TableExplanation.orderedPredicate(whereSyntax, environment)) : "conjuncts=" + FilterRules.terms(whereSyntax).size(), PlanEstimate.rows(whereSyntax, node.rows(), List.of()), PlanEstimate.mul(node.rows(), PlanEstimate.operations(whereSyntax)));
        long work = specs.size() * Costs.WINDOW_RESULT;
        for (Spec spec : specs) {
            work = PlanEstimate.add(work, PlanEstimate.operations(spec.syntax.function().filter()));
            for (SqlParser.Expr argument : spec.syntax.function().args()) work = PlanEstimate.add(work, PlanEstimate.operations(argument));
        }
        for (Group group : groups.values()) {
            for (SqlParser.Expr key : group.syntax.partition()) work = PlanEstimate.add(work, PlanEstimate.operations(key));
            for (SqlParser.Order key : group.syntax.order()) work = PlanEstimate.add(work, PlanEstimate.operations(key.expression()));
        }
        return node.wrap("Window", "funcs=" + specs.size(), node.rows(), PlanEstimate.mul(node.rows(), work));
    }
    private static boolean prefixSafe(Spec spec, TableRelation table) {
        SqlParser.WindowSpec definition = spec.syntax.spec();
        if (!definition.partition().isEmpty() || !table.orderedBy(definition.order(), false)) return false;
        String function = spec.syntax.function().name();
        if (List.of("row_number", "rank", "dense_rank", "lag").contains(function)) return true;
        if (List.of("lead", "percent_rank", "cume_dist", "ntile").contains(function)) return false;
        SqlParser.WindowFrame frame = definition.frame();
        if (frame == null) return table.orderedBy(definition.order(), true);
        if (frame.end().kind().equals("following") || frame.end().kind().equals("unbounded_following")) return false;
        return !frame.end().kind().equals("current") || frame.mode().equals("rows") || table.orderedBy(definition.order(), true);
    }
    SqlParser.Select rewritten() { return rewritten; }
    private SqlParser.Expr rewrite(SqlParser.Expr expression) {
        return WindowSyntax.map(expression, value -> {
            if (!(value instanceof SqlParser.Window window)) return value;
            SqlParser.WindowSpec definition = window.reference() == null ? WindowSyntax.merge(window.spec(), definitions) : definitions.get(window.reference());
            if (definition == null) throw new SqlException("42704", "window does not exist");
            SqlParser.Window normalized = new SqlParser.Window(window.function(), definition, null);
            for (int i = 0; i < specs.size(); i++) if (specs.get(i).syntax.equals(normalized)) return new SqlParser.ColumnSlot(source.types().size() + i);
            Spec spec = resolve(normalized); int position = bindings.size(); specs.add(spec);
            CollationDerivation collation = spec.fold != null ? AggregatePlan.foldCollation(spec.fold) : CollationDerivation.NONE;
            if (CollationDerivation.collatable(spec.type)) for (ScalarPlan.Expression argument : spec.args) collation = CollationDerivation.merge(collation, argument.collation());
            bindings.add(new ScalarPlan.Binding(List.of(), "$window" + specs.size(), spec.type, false, false, collation));
            return new SqlParser.ColumnSlot(position);
        });
    }
    private Spec resolve(SqlParser.Window syntax) {
        SqlParser.Call call = syntax.function(); SqlParser.WindowSpec definition = syntax.spec();
        if (call.args().stream().anyMatch(WindowSyntax::contains) || WindowSyntax.contains(call.filter())) throw new SqlException("42P20", "window calls cannot be nested");
        if (call.withinGroup()) throw new SqlException("0A000", "ordered-set window aggregate is not supported");
        if (call.distinct()) throw new SqlException("0A000", "DISTINCT window aggregate is not supported");
        if (!call.order().isEmpty()) throw new SqlException("0A000", "aggregate ORDER BY in a window is not supported");
        Group group = validate(definition);
        var arguments = new ArrayList<ScalarPlan.Expression>(); AggregatePlan.Fold fold = null; SqlType type;
        if (AggregatePlan.aggregateName(call.name()) && !List.of("rank", "dense_rank", "percent_rank", "cume_dist").contains(call.name())) { fold = AggregatePlan.resolve(call, resolver); type = fold.type(); }
        else {
            WindowCatalog.Fact fact = WindowCatalog.FACTS.stream().filter(f -> f.name().equals(call.name())).findFirst().orElseThrow(() -> new SqlException("42809", "OVER requires a window or aggregate function"));
            int count = call.args().size();
            boolean valid = switch (fact.arguments()) { case "none" -> count == 0; case "one" -> count == 1; case "value_n" -> count == 2; default -> count >= 1 && count <= 3; };
            if (!valid || call.star()) throw new SqlException("42883", "window function argument count does not match");
            if (call.filter() != null) throw new SqlException("0A000", "FILTER requires an aggregate function");
            for (int i = 0; i < count; i++) {
                SqlType context = call.name().equals("ntile") || i == 1 ? SqlType.I64 : i == 2 ? arguments.getFirst().type() : null;
                ScalarPlan.Expression arg = resolver.resolve(call.args().get(i), context);
                if (context == SqlType.I64 && !arg.type().isInteger()) throw SqlException.typeMismatch("window offset requires integer");
                if (i == 2 && !arg.type().equals(context)) throw SqlException.typeMismatch("window default type does not match value");
                arguments.add(arg);
            }
            type = fact.result().equals("same_as_input") ? arguments.getFirst().type() : SqlType.named(fact.result());
        }
        return new Spec(syntax, group, type, arguments, fold, bound(definition, true), bound(definition, false));
    }
    private Group validate(SqlParser.WindowSpec spec) {
        if (spec.partition().stream().anyMatch(WindowSyntax::contains) || spec.order().stream().anyMatch(o -> WindowSyntax.contains(o.expression()))) throw new SqlException("42P20", "window function in window key");
        Keys key = new Keys(spec.partition(), spec.order()); Group found = groups.get(key);
        if (found == null) {
            found = new Group(key, spec.partition().stream().map(this::key).toList(), spec.order().stream().map(o -> key(o.expression())).toList());
            groups.put(key, found);
        }
        SqlParser.WindowFrame frame = spec.frame();
        if (frame != null) {
            String start = frame.start().kind(), end = frame.end().kind();
            if (start.equals("unbounded_following") || end.equals("unbounded_preceding") || rank(start) > rank(end)) throw new SqlException("42P20", "invalid window frame bounds");
            boolean offsets = frame.start().offset() != null || frame.end().offset() != null;
            if (frame.mode().equals("groups") && spec.order().isEmpty()) throw new SqlException("42P20", "GROUPS requires ORDER BY");
            if (frame.mode().equals("range") && offsets) {
                if (spec.order().size() != 1) throw new SqlException("42P20", "RANGE offset requires one ordering key");
                if (!found.order.getFirst().type().isNumeric()) throw new SqlException("0A000", "RANGE offset key type is not supported");
            }
            bound(spec, true); bound(spec, false);
        }
        return found;
    }
    private ScalarPlan.Expression key(SqlParser.Expr expression) {
        ScalarPlan.Resolver keyResolver = environment.resolver(source instanceof RelationPlan relation ? relation.bindings() : bindings.subList(0, source.types().size()), frame);
        ScalarPlan.Expression result = keyResolver.resolve(expression, null);
        ScalarPlan.requireComparable(result.type()); CollationDerivation.requireSort(result, expression);
        if (keyResolver.usedOuterReference()) throw new SqlException("0A000", "correlated window keys are not supported");
        return resolver.resolve(expression, null);
    }
    private static int rank(String bound) { return switch (bound) { case "unbounded_preceding" -> 0; case "preceding" -> 1; case "current" -> 2; case "following" -> 3; default -> 4; }; }
    private ScalarPlan.Expression bound(SqlParser.WindowSpec spec, boolean start) {
        if (spec.frame() == null) return null;
        SqlParser.Expr expression = (start ? spec.frame().start() : spec.frame().end()).offset();
        if (expression == null) return null;
        SqlParser.Expr literal = expression instanceof SqlParser.Unary unary && (unary.op().equals("-") || unary.op().equals("+")) ? unary.value() : expression;
        if (!(literal instanceof SqlParser.Literal)) throw new SqlException("0A000", "frame offset requires a numeric literal");
        if (WindowSyntax.contains(expression) || AggregatePlan.contains(expression)) throw new SqlException("42P20", "frame offset cannot contain aggregate/window calls");
        ScalarPlan.Expression result = environment.resolver().resolve(expression, null);
        if (spec.frame().mode().equals("range") ? !result.type().isNumeric() : !result.type().isInteger()) throw new SqlException("0A000", "frame offset has invalid type");
        if (spec.frame().mode().equals("range") && !spec.order().isEmpty() && resolver.resolve(spec.order().getFirst().expression(), null).type().isInteger() && !result.type().isInteger()) throw new SqlException("0A000", "integer RANGE requires an integer offset");
        return result;
    }
    @Override public List<ScalarPlan.Binding> bindings() { return List.copyOf(bindings); }
    @Override public List<String> names() { return bindings.stream().map(ScalarPlan.Binding::name).toList(); }
    @Override public List<SqlType> types() { return bindings.stream().map(ScalarPlan.Binding::type).toList(); }
    @Override public void touch(boolean[] touched) { }
    private void reserve(long bytes) { memory.reserve(bytes); reserved = MemoryAccount.add(reserved, bytes); }
    private void release(long bytes) { memory.release(bytes); reserved -= bytes; }
    private void initialize(CostMeter meter) {
        started = true; memory = meter.memory(); Row row;
        long cap = Long.MAX_VALUE;
        if (prefixLimit != null) {
            SqlValue limit = prefixLimit.run(meter), offset = prefixOffset == null ? SqlValue.ofInteger(0) : prefixOffset.run(meter);
            if (!limit.isNull() && limit.asInteger() < 0) throw new SqlException("2201W", "negative LIMIT");
            if (!offset.isNull() && offset.asInteger() < 0) throw new SqlException("2201X", "negative OFFSET");
            if (!limit.isNull()) { try { cap = Math.addExact(limit.asInteger(), offset.isNull() ? 0 : offset.asInteger()); } catch (ArithmeticException overflow) { cap = Long.MAX_VALUE; } }
        }
        while (rows.size() < cap && (row = source.next(meter)) != null) {
            frame.values = row.values();
            if (where != null) { long before = meter.cost(); SqlValue test; try { test = where.run(meter); } finally { metrics.add("Filter", before, meter); } if (test.isNull() || !test.asBoolean()) continue; }
            reserve(MemoryAccount.row(row.values())); rows.add(new ArrayList<>(row.values()));
        }
        source.close();
        List<Group> active = specs.stream().map(Spec::group).distinct().toList();
        for (Group group : active) {
            var partitions = new LinkedHashMap<List<SqlValue>, List<Integer>>();
            var ordering = new ArrayList<List<SqlValue>>(); long partitionBytes = 0, keyBytes = 0;
            for (int i = 0; i < rows.size(); i++) {
                frame.values = rows.get(i); List<SqlValue> keys = new ArrayList<>();
                for (int k = 0; k < group.partition.size(); k++) { ScalarPlan.Expression key = group.partition.get(k); SqlValue value = key.run(meter);
                    if (!(group.syntax.partition.get(k) instanceof SqlParser.Name || group.syntax.partition.get(k) instanceof SqlParser.ColumnSlot)) for (int repeat = 1; repeat < specs.stream().filter(s -> s.group == group).count(); repeat++) key.run(meter); meter.charge(MemoryAccount.payload(value) * Costs.HASH_KEY); meter.guard(); keys.add(value); }
                List<Integer> partition = partitions.get(keys);
                if (partition == null) { long bytes = MemorySchedule.ENTRY; for (SqlValue key : keys) bytes += MemoryAccount.value(key); reserve(bytes); partitionBytes += bytes; partition = new ArrayList<>(); partitions.put(List.copyOf(keys), partition); }
                partition.add(i); List<SqlValue> order = new ArrayList<>();
                for (int k = 0; k < group.order.size(); k++) { ScalarPlan.Expression key = group.order.get(k); SqlValue value = key.run(meter);
                    if (!(group.syntax.order.get(k).expression() instanceof SqlParser.Name || group.syntax.order.get(k).expression() instanceof SqlParser.ColumnSlot)) for (int repeat = 1; repeat < specs.stream().filter(s -> s.group == group).count(); repeat++) key.run(meter); order.add(AggregatePlan.sortKey(key, value, meter)); }
                // Materialized compound keys are Q1 synthetic columns; bare column references share their value.
                for (int k = 0; k < order.size(); k++) {
                    ScalarPlan.Expression key = group.order.get(k);
                    boolean collated = !key.collation().require().equals("C") && CollationDerivation.collatable(key.type());
                    boolean expression = !(group.syntax.order.get(k).expression() instanceof SqlParser.Name || group.syntax.order.get(k).expression() instanceof SqlParser.ColumnSlot);
                    if (collated ? !order.get(k).isNull() : expression) { long bytes = MemoryAccount.value(order.get(k)); reserve(bytes); keyBytes += bytes; }
                }
                ordering.add(order);
            }
            for (List<Integer> partition : partitions.values()) partition.sort((a,b) -> compare(ordering.get(a), ordering.get(b), group.syntax.order));
            for (Spec spec : specs) if (spec.group == group) for (List<Integer> partition : partitions.values()) evaluate(spec, partition, ordering, meter);
            release(partitionBytes + keyBytes);
        }
    }
    private static int compare(List<SqlValue> a, List<SqlValue> b, List<SqlParser.Order> order) {
        for (int i = 0; i < order.size(); i++) {
            SqlValue x = a.get(i), y = b.get(i); SqlParser.Order key = order.get(i); boolean first = key.nullsFirst() == null ? key.descending() : key.nullsFirst();
            int c = x.isNull() || y.isNull() ? x.isNull() == y.isNull() ? 0 : x.isNull() == first ? -1 : 1 : ScalarPlan.compare(x, y, null) * (key.descending() ? -1 : 1);
            if (c != 0) return c;
        }
        return 0;
    }
    private void evaluate(Spec spec, List<Integer> partition, List<List<SqlValue>> order, CostMeter meter) {
        int n = partition.size(); int[] loPeer = new int[n], hiPeer = new int[n], groupId = new int[n]; List<Integer> starts = new ArrayList<>();
        for (int i = 0; i < n;) {
            int end = i + 1; while (end < n && compare(order.get(partition.get(i)), order.get(partition.get(end)), spec.group.syntax.order) == 0) end++;
            starts.add(i); for (int j = i; j < end; j++) { loPeer[j] = i; hiPeer[j] = end; groupId[j] = starts.size() - 1; } i = end;
        }
        SqlValue lower = offset(spec.lower, meter), upper = offset(spec.upper, meter);
        List<SqlValue> cache = new ArrayList<>(); long cacheBytes = 0;
        String name = spec.syntax.function().name(); boolean cached = spec.fold == null && !spec.args.isEmpty() && !name.equals("ntile");
        if (cached) for (int index : partition) { frame.values = rows.get(index); SqlValue value = spec.args.getFirst().run(meter); long bytes = MemoryAccount.value(value); reserve(bytes); cacheBytes += bytes; cache.add(value); }
        SqlValue buckets = null; if (name.equals("ntile") && n > 0) { frame.values = rows.get(partition.getFirst()); buckets = spec.args.getFirst().run(meter); if (!buckets.isNull() && buckets.asInteger() < 1) throw new SqlException("22014", "ntile argument must be positive"); }
        AggregatePlan.State state = null; int previousLo = -1, previousHi = 0; long stateBytes = 0;
        boolean cacheOperands = spec.syntax.spec().frame() != null;
        SqlValue[] operandCache = spec.fold == null ? null : new SqlValue[n]; boolean[] visited = spec.fold == null ? null : new boolean[n]; boolean[] accepted = spec.fold == null ? null : new boolean[n];
        int output = source.types().size() + specs.indexOf(spec);
        List<SqlValue> computed = new ArrayList<>();
        for (int i = 0; i < n; i++) {
            frame.values = rows.get(partition.get(i)); SqlValue result;
            int lo = frameBound(spec, true, i, n, loPeer, hiPeer, groupId, starts, order, partition, lower);
            int hi = frameBound(spec, false, i, n, loPeer, hiPeer, groupId, starts, order, partition, upper);
            if (hi < lo) hi = lo;
            switch (name) {
                case "row_number" -> result = SqlValue.ofInteger(i + 1);
                case "rank" -> result = SqlValue.ofInteger(loPeer[i] + 1);
                case "dense_rank" -> result = SqlValue.ofInteger(groupId[i] + 1);
                case "percent_rank" -> result = SqlValue.ofFloat(SqlType.F64, n == 1 ? 0 : (double)loPeer[i] / (n - 1));
                case "cume_dist" -> result = SqlValue.ofFloat(SqlType.F64, (double)hiPeer[i] / n);
                case "ntile" -> {
                    if (buckets.isNull()) result = SqlValue.nullValue(SqlType.I64);
                    else { long count = buckets.asInteger(), small = n / count, large = n % count, boundary = (small + 1) * large; long bucket = i < boundary ? i / (small + 1) : small == 0 ? i : large + (i - boundary) / small; result = SqlValue.ofInteger(bucket + 1); }
                }
                case "lag", "lead" -> {
                    SqlValue offset = spec.args.size() > 1 ? spec.args.get(1).run(meter) : SqlValue.ofInteger(1);
                    if (offset.isNull()) result = SqlValue.nullValue(spec.type);
                    else { long distance = offset.asInteger(), target; try { target = name.equals("lag") ? Math.subtractExact(i, distance) : Math.addExact(i, distance); } catch (ArithmeticException error) { target = -1; }
                        result = target >= 0 && target < n ? cache.get((int)target) : spec.args.size() > 2 ? spec.args.get(2).run(meter) : SqlValue.nullValue(spec.type); }
                }
                case "first_value", "last_value", "nth_value" -> {
                    long nth = 1; boolean nullNth = false;
                    if (name.equals("nth_value")) { SqlValue value = spec.args.get(1).run(meter); nullNth = value.isNull(); if (!nullNth) { nth = value.asInteger(); if (nth < 1) throw new SqlException("22016", "nth_value argument must be positive"); } }
                    result = SqlValue.nullValue(spec.type);
                    if (!nullNth) for (int j = name.equals("last_value") ? hi - 1 : lo; j >= lo && j < hi; j += name.equals("last_value") ? -1 : 1) if (!excluded(spec, i, j, loPeer, hiPeer) && --nth == 0) { result = cache.get(j); break; }
                }
                default -> {
                    boolean sliding = spec.fold.filter() == null && (spec.syntax.spec().frame() == null || spec.syntax.spec().frame().exclude().equals("none"));
                    boolean retain = sliding && state != null && hi >= previousHi && (lo == previousLo || name.equals("count") && lo >= previousLo);
                    if (!retain) { if (state != null) release(stateBytes); state = new AggregatePlan.State(spec.fold); stateBytes = state.retainedBytes(); reserve(stateBytes); previousHi = lo; }
                    else if (lo > previousLo) for (int j = previousLo; j < Math.min(lo, previousHi); j++) { meter.charge(Costs.WINDOW_FRAME_STEP); meter.guard(); if (accepted[j]) state.count--; }
                    int begin = retain ? Math.max(lo, previousHi) : lo;
                    for (int j = begin; j < hi; j++) {
                        if (excluded(spec, i, j, loPeer, hiPeer)) continue;
                        meter.charge(Costs.WINDOW_FRAME_STEP); meter.guard();
                        if (!visited[j] || spec.fold.filter() != null) {
                            if (visited[j] && operandCache[j] != null) { long bytes = cacheOperands ? MemoryAccount.value(operandCache[j]) : 0; release(bytes); cacheBytes -= bytes; operandCache[j] = null; }
                            frame.values = rows.get(partition.get(j));
                            SqlValue filter = spec.fold.filter() == null ? SqlValue.ofBoolean(true) : spec.fold.filter().run(meter);
                            accepted[j] = !filter.isNull() && filter.asBoolean();
                            if (accepted[j]) { operandCache[j] = spec.fold.input() == null ? SqlValue.ofInteger(1) : spec.fold.input().run(meter); accepted[j] = !operandCache[j].isNull() || AggregatePlan.collectionFold(spec.fold.name()); long bytes = cacheOperands ? MemoryAccount.value(operandCache[j]) : 0; reserve(bytes); cacheBytes += bytes; }
                            visited[j] = true;
                        }
                        if (accepted[j]) { state.add(operandCache[j], meter); long bytes = state.retainedBytes(); if (bytes > stateBytes) reserve(bytes - stateBytes); else release(stateBytes - bytes); stateBytes = bytes; }
                    }
                    previousLo = lo; previousHi = hi; result = state.finish();
                }
            }
            meter.charge(Costs.WINDOW_RESULT); meter.guard(); computed.add(result);
        }
        if (state != null) release(stateBytes); release(cacheBytes);
        for (int i = 0; i < computed.size(); i++) {
            SqlValue result = computed.get(i); reserve(MemoryAccount.value(result));
            List<SqlValue> row = rows.get(partition.get(i)); while (row.size() <= output) row.add(null); row.set(output, result);
        }
    }
    private static boolean excluded(Spec spec, int current, int other, int[] lo, int[] hi) {
        if (spec.syntax.spec().frame() == null) return false;
        return switch (spec.syntax.spec().frame().exclude()) { case "current" -> current == other; case "group" -> other >= lo[current] && other < hi[current]; case "ties" -> other != current && other >= lo[current] && other < hi[current]; default -> false; };
    }
    private static SqlValue offset(ScalarPlan.Expression expression, CostMeter meter) {
        if (expression == null) return null; SqlValue value = expression.run(meter);
        if (value.isNull()) throw new SqlException("22004", "frame offset must not be null");
        if (value.type().isInteger() ? value.asInteger() < 0 : value.type().isFloat() ? value.asFloat() < 0 : value.asDecimal().compareTo(Decimal.zero(0)) < 0) throw new SqlException("22013", "frame offset must not be negative");
        return value;
    }
    private int frameBound(Spec spec, boolean start, int current, int n, int[] lo, int[] hi, int[] groups, List<Integer> starts, List<List<SqlValue>> ordering, List<Integer> partition, SqlValue offset) {
        SqlParser.WindowFrame frame = spec.syntax.spec().frame();
        if (frame == null) return start ? 0 : hi[current];
        String kind = (start ? frame.start() : frame.end()).kind();
        if (kind.equals("unbounded_preceding")) return 0; if (kind.equals("unbounded_following")) return n;
        if (kind.equals("current")) return frame.mode().equals("rows") ? current + (start ? 0 : 1) : start ? lo[current] : hi[current];
        if (frame.mode().equals("range")) return rangeBound(spec, start, kind, current, n, ordering, partition, offset, lo, hi);
        long distance = offset.asInteger(), base = frame.mode().equals("groups") ? groups[current] : current, target;
        try { target = kind.equals("preceding") ? Math.subtractExact(base, distance) : Math.addExact(base, distance); } catch (ArithmeticException error) { target = kind.equals("preceding") ? Long.MIN_VALUE : Long.MAX_VALUE; }
        if (frame.mode().equals("groups")) { if (target < 0) return 0; if (target >= starts.size()) return n; return start ? starts.get((int)target) : target + 1 == starts.size() ? n : starts.get((int)target + 1); }
        if (!start && target != Long.MAX_VALUE) target++; return (int)Math.max(0, Math.min(n, target));
    }
    private int rangeBound(Spec spec, boolean start, String kind, int current, int n, List<List<SqlValue>> ordering, List<Integer> partition, SqlValue offset, int[] lo, int[] hi) {
        SqlValue key = ordering.get(partition.get(current)).getFirst(); if (key.isNull()) return start ? lo[current] : hi[current];
        boolean descending = spec.group.syntax.order.getFirst().descending(); boolean subtract = kind.equals("preceding") != descending;
        SqlValue boundary;
        if (key.type().isFloat()) { double amount = offset.type().isFloat() ? offset.asFloat() : offset.type().isInteger() ? offset.asInteger() : Double.parseDouble(offset.asDecimal().toString()); boundary = SqlValue.ofFloat(SqlType.F64, key.asFloat() + (subtract ? -amount : amount)); }
        else { Decimal number = key.type().isInteger() ? Decimal.of(key.asInteger()) : key.asDecimal(), amount = offset.type().isInteger() ? Decimal.of(offset.asInteger()) : offset.asDecimal(); boundary = SqlValue.ofDecimal(subtract ? number.subtract(amount) : number.add(amount)); }
        for (int i = 0; i < n; i++) {
            SqlValue value = ordering.get(partition.get(i)).getFirst(); if (value.isNull()) { boolean first = spec.group.syntax.order.getFirst().nullsFirst() == null ? descending : spec.group.syntax.order.getFirst().nullsFirst(); if (!first) return i; continue; }
            int comparison = ScalarPlan.compare(value, boundary, null) * (descending ? -1 : 1); if (start ? comparison >= 0 : comparison > 0) return i;
        }
        return n;
    }
    @Override public boolean orderedBy(List<SqlParser.Order> order) {
        return source instanceof RelationPlan relation && relation.orderedBy(order);
    }
    @Override public Row next(CostMeter meter) {
        long before = meter.cost(); try { return nextMeasured(meter); } finally { metrics.total(before, meter); }
    }
    private Row nextMeasured(CostMeter meter) {
        if (closed) return null;
        if (!started) initialize(meter);
        if (next == rows.size()) return null;
        List<SqlValue> values = rows.get(next++);
        release(MemoryAccount.row(values));
        return new Row(names(), values);
    }
    @Override public void close() { if (closed) return; closed = true; source.close(); rows.clear(); if (memory != null) { memory.release(reserved); reserved = 0; } }
}
