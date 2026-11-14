package org.jed;

import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.Set;
import org.jed.storage.PersistentTree;

/** Input-order aggregate folds and first-occurrence groups backed by bounded keyed state. */
final class AggregatePlan implements QueryPlan {
    private static final Set<String> NAMES = Set.of("count", "sum", "avg", "min", "max", "bool_and", "bool_or", "every", "bit_and", "bit_or", "bit_xor",
        "json_object_agg", "jsonb_object_agg", "json_object_agg_strict", "jsonb_object_agg_strict", "json_object_agg_unique", "jsonb_object_agg_unique", "json_object_agg_unique_strict", "jsonb_object_agg_unique_strict",
        "array_agg", "string_agg", "json_agg", "jsonb_agg", "json_agg_strict", "jsonb_agg_strict", "stddev", "stddev_pop", "stddev_samp", "variance", "var_pop", "var_samp",
        "corr", "covar_pop", "covar_samp", "regr_count", "regr_avgx", "regr_avgy", "regr_sxx", "regr_syy", "regr_sxy", "regr_slope", "regr_intercept", "regr_r2");
    record Fold(String name, ScalarPlan.Expression input, ScalarPlan.Expression filter, SqlType type, boolean distinct,
                List<SqlParser.Order> order, List<ScalarPlan.Expression> orderKeys, OrderedSet orderedSet) {
        Fold(String name, ScalarPlan.Expression input, ScalarPlan.Expression filter, SqlType type, boolean distinct, List<SqlParser.Order> order, List<ScalarPlan.Expression> orderKeys) { this(name, input, filter, type, distinct, order, orderKeys, null); }
        Fold unordered() { return new Fold(name, input, filter, type, distinct, List.of(), List.of()); }
    }
    private record Group(List<SqlValue> keys, List<State> states, int set) implements SpillKeyState.Encodable {
        @Override public byte[] encode() { return AggregatePlan.encode(this); }
        Group copyForUpdate() {
            List<State> copies = new ArrayList<>(states.size());
            for (State state : states) copies.add(new State(state));
            return new Group(keys, copies, set);
        }
    }
    private final Engine.Table table;
    private final QueryPlan source;
    private final List<ScalarPlan.Binding> sourceBindings;
    private final String alias;
    private final SqlValue[] arguments;
    private final SqlParser.Select query;
    private final boolean distinct;
    private final DistinctOn distinctOn;
    private final List<SqlParser.Order> effectiveOrder;
    private final Path scratch;
    private final long workMem;
    private final QueryEnvironment environment;
    private final ScalarPlan.Resolver inputResolver;
    private final ScalarPlan.Resolver outputResolver;
    private final List<ScalarPlan.Binding> outputBindings;
    private final List<SqlParser.Expr> outputMetadata;
    private final SqlParser.Expr havingMetadata;
    private final ScalarPlan.Frame inputFrame = new ScalarPlan.Frame();
    private final ScalarPlan.Frame outputFrame = new ScalarPlan.Frame();
    private final List<Fold> folds = new ArrayList<>();
    private final List<SqlParser.Call> foldMetadata = new ArrayList<>();
    private final List<SqlParser.Expr> grouping = new ArrayList<>();
    private final List<ScalarPlan.Expression> keys = new ArrayList<>();
    private final List<List<Integer>> groupingSets = new ArrayList<>();
    private final List<List<Integer>> groupingResults = new ArrayList<>();
    private final List<String> labels = new ArrayList<>();
    private final List<ScalarPlan.Expression> projections = new ArrayList<>();
    private final List<ScalarPlan.Expression> orderKeys = new ArrayList<>();
    private final ScalarPlan.Expression where, having, limit, offset;
    private SpillKeyState groups;
    private AggregateCollections collections;
    private DistinctState distinctInputs, distinctOutputs;
    private final java.util.Map<Long, List<List<SqlValue>>> distinctGroupInputs = new java.util.HashMap<>();
    private static final class GroupKey {
        final byte[] bytes;
        GroupKey(byte[] bytes) { this.bytes = bytes.clone(); }
        public int hashCode() { return java.util.Arrays.hashCode(bytes); }
        public boolean equals(Object value) { return value instanceof GroupKey key && java.util.Arrays.equals(bytes, key.bytes); }
    }
    private java.util.Map<GroupKey, Long> columnarCharges;
    private long columnarHeld;
    private boolean columnarRejected;
    private MemoryAccount groupAccount;
    private RowSpool results, retainedInput, materializedSource, scannedInput, groupKeyRows;
    private boolean collectingInput;
    private ExternalSorter sorter;
    private RowSpool.Cursor output;
    private long remaining, skip;
    private boolean started, closed;
    private boolean produce = true;
    AggregatePlan withoutEmissionCharge() { produce = false; return this; }

    private static boolean objectFold(String name) { return name.startsWith("json_object_agg") || name.startsWith("jsonb_object_agg"); }
    static boolean collectionFold(String name) { return name.equals("string_agg") || objectFold(name) || name.equals("array_agg") || name.equals("json_agg") || name.equals("jsonb_agg") || name.equals("json_agg_strict") || name.equals("jsonb_agg_strict"); }
    static boolean aggregateName(String name) { return NAMES.contains(name) || OrderedSet.NAMES.contains(name); }
    static boolean needed(SqlParser.Select query) {
        return !query.group().isEmpty() || query.items().stream().anyMatch(item -> contains(item.expression())) || query.having() != null
            || query.order().stream().anyMatch(item -> contains(item.expression())) || query.distinctOn().stream().anyMatch(AggregatePlan::contains);
    }
    static boolean contains(SqlParser.Expr expression) {
        if (expression == null) return false;
        if (expression instanceof SqlParser.Window window) return window.function().args().stream().anyMatch(AggregatePlan::contains) || contains(window.function().filter()) || window.spec() != null && (window.spec().partition().stream().anyMatch(AggregatePlan::contains) || window.spec().order().stream().anyMatch(order -> contains(order.expression())));
        if (expression instanceof SqlParser.Call call) return (aggregateName(call.name()) || call.withinGroup() || call.name().equals("grouping")) || call.args().stream().anyMatch(AggregatePlan::contains);
        if (expression instanceof SqlParser.Extrema extrema) return extrema.args().stream().anyMatch(AggregatePlan::contains);
        if (expression instanceof SqlParser.Binary binary) return contains(binary.left()) || contains(binary.right());
        if (expression instanceof SqlParser.Unary unary) return contains(unary.value());
        if (expression instanceof SqlParser.Cast cast) return contains(cast.value());
        if (expression instanceof SqlParser.In in) return contains(in.subject()) || in.values().stream().anyMatch(AggregatePlan::contains);
        if (expression instanceof SqlParser.Between between) return contains(between.subject()) || contains(between.lower()) || contains(between.upper());
        if (expression instanceof SqlParser.Conditional branch) return contains(branch.input()) || contains(branch.otherwise()) || branch.conditions().stream().anyMatch(AggregatePlan::contains) || branch.results().stream().anyMatch(AggregatePlan::contains);
        if (expression instanceof SqlParser.ArrayConstructor array) return array.values().stream().anyMatch(AggregatePlan::contains);
        if (expression instanceof SqlParser.RowConstructor row) return row.values().stream().anyMatch(AggregatePlan::contains);
        if (expression instanceof SqlParser.Access access) return contains(access.base()) || access.subscripts().stream().anyMatch(sub -> contains(sub.lower()) || contains(sub.upper()));
        if (expression instanceof SqlParser.Quantified quantified) return contains(quantified.subject()) || contains(quantified.array());
        if (expression instanceof SqlParser.NamedArgument named) return contains(named.value());
        if (expression instanceof SqlParser.Field field) return contains(field.base());
        if (expression instanceof SqlParser.Collate collate) return contains(collate.value());
        return false;
    }
    AggregatePlan(SqlParser.Select query, Engine.Table table, String alias, SqlValue[] args) { this(query, table, alias, args, null, 0, null); }
    AggregatePlan(SqlParser.Select query, Engine.Table table, String alias, SqlValue[] args, Path scratch, long workMem) { this(query, table, alias, args, scratch, workMem, null); }
    AggregatePlan(SqlParser.Select query, Engine.Table table, String alias, SqlValue[] args, Path scratch, long workMem, QueryEnvironment environment) {
        this(query, table, alias, null, table == null ? List.of() : table.bindings(alias), args, scratch, workMem, environment);
    }
    static AggregatePlan fromTable(SqlParser.Select query, Engine.Table table, SqlParser.Table source, SqlValue[] args, Path scratch, long workMem, QueryEnvironment environment) {
        return new AggregatePlan(query, table, source.alias() == null ? table.name() : source.alias(), null, aliasedBindings(table, source), args, scratch, workMem, environment);
    }
    /** Resolve the actual grouped scope without opening a source or registering executable InitPlans. */
    static void captureEstimates(SqlParser.Select query, List<ScalarPlan.Binding> bindings,
                                 List<RelationPlan.FunctionalKey> dependencies, SqlValue[] arguments, QueryEnvironment environment) {
        QueryEnvironment capture = environment.estimateOnly();
        RelationPlan shape = new RelationPlan() {
            public List<ScalarPlan.Binding> bindings() { return bindings; }
            public List<String> names() { return bindings.stream().map(ScalarPlan.Binding::name).toList(); }
            public List<SqlType> types() { return bindings.stream().map(ScalarPlan.Binding::type).toList(); }
            public List<FunctionalKey> functionalKeys() { return dependencies; }
            public void touch(boolean[] columns) { }
            public Row next(CostMeter meter) { throw new IllegalStateException("estimate shape cannot execute"); }
            public PlanNode explain(boolean verbose) { return new PlanNode("Result", "", 0, 0, List.of(), "buffered"); }
        };
        if (WindowSyntax.needed(query)) {
            GroupedWindow.Prepared grouped = GroupedWindow.prepare(query, shape, bindings, arguments, capture, null, 0);
            grouped.source().close();
        } else try (AggregatePlan ignored = new AggregatePlan(query, shape, bindings, arguments, null, 0, capture)) { }
    }
    private static List<ScalarPlan.Binding> aliasedBindings(Engine.Table table, SqlParser.Table source) {
        if (source.columns().size() > table.columns().size()) throw new SqlException("42P10", "too many table column aliases");
        List<ScalarPlan.Binding> bindings = new ArrayList<>(table.bindings(source.alias()));
        for (int i = 0; i < source.columns().size(); i++) { var binding = bindings.get(i); bindings.set(i, new ScalarPlan.Binding(binding.qualifier(), source.columns().get(i), binding.type(), binding.qualifiedOnly(), binding.scalarFunction(), binding.collation(), binding.whole())); }
        return bindings;
    }
    AggregatePlan(SqlParser.Select query, QueryPlan source, List<ScalarPlan.Binding> bindings, SqlValue[] args, Path scratch, long workMem, QueryEnvironment environment) {
        this(query, null, null, source, bindings, args, scratch, workMem, environment);
    }
    private AggregatePlan(SqlParser.Select query, Engine.Table table, String alias, QueryPlan source, List<ScalarPlan.Binding> bindings, SqlValue[] args, Path scratch, long workMem, QueryEnvironment environment) {
        this.query = query; this.distinct = query.distinct() && query.distinctOn().isEmpty(); this.table = table; this.alias = alias; this.arguments = args; this.scratch = scratch; this.workMem = workMem; this.environment = environment;
        this.source = source; sourceBindings = List.copyOf(bindings);
        List<ScalarPlan.Binding> inputBindings = sourceBindings;
        if (environment != null) environment.captureStars(query.items(), inputBindings);
        ScalarPlan.Resolver input = inputResolver = resolver(args, inputBindings, inputFrame);
        if (contains(query.where())) throw new SqlException("42803", "aggregate in WHERE");
        where = filterExpression(input, query.where());
        List<SqlParser.Expr> projected = new ArrayList<>();
        for (SqlParser.Item item : ProjectionSyntax.expand(query.items(), input)) {
            if (item.expression() instanceof SqlParser.Star star) {
                boolean found = false;
                for (ScalarPlan.Binding column : sourceBindings) if (!column.qualifiedOnly() && (star.qualifier().isEmpty() || star.qualifier().equals(column.qualifier()))) {
                    found = true; labels.add(column.name()); List<String> parts = new ArrayList<>(column.qualifier()); parts.add(column.name()); projected.add(new SqlParser.Name(parts));
                }
                if (!found) throw new SqlException("42P01", "unknown star qualifier");
            } else { labels.add(item.alias() == null ? ScalarPlan.outputName(item.expression()) : item.alias()); projected.add(item.expression()); }
        }
        List<List<SqlParser.Expr>> sets = List.of(List.of());
        for (SqlParser.Expr term : query.group()) sets = cross(sets, expand(term));
        for (List<SqlParser.Expr> set : sets) {
            List<Integer> indexes = new ArrayList<>();
            for (SqlParser.Expr raw : set) {
                SqlParser.Expr term = raw;
                if (term instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.INTEGER) term = projected.get(ordinal(literal, projected.size(), "GROUP BY"));
                else if (term instanceof SqlParser.Name name && name.parts().size() == 1 && !input.hasColumn(name) && labels.contains(name.parts().getFirst())) term = projected.get(label(name.parts().getFirst(), projected, "GROUP BY"));
                if (contains(term)) throw new SqlException("42803", "aggregate in GROUP BY");
                term = normalize(term);
                int index = grouping.indexOf(term);
                if (index < 0) { index = grouping.size(); grouping.add(term); keys.add(input.resolve(term, null)); ScalarPlan.requireComparable(keys.getLast().type()); CollationDerivation.requireSort(keys.getLast(), term); }
                if (!indexes.contains(index)) indexes.add(index);
            }
            groupingSets.add(indexes);
        }
        if (table != null && table.primaryKey().length > 0 && groupingSets.size() == 1 && query.group().stream().noneMatch(term -> term instanceof SqlParser.Call call && call.name().startsWith("$"))) {
            boolean determines = true;
            for (int column : table.primaryKey()) if (!grouping.contains(new SqlParser.Name(List.of(sourceBindings.get(column).name())))) determines = false;
            if (determines) for (ScalarPlan.Binding column : sourceBindings) {
                SqlParser.Expr name = new SqlParser.Name(List.of(column.name()));
                if (!grouping.contains(name)) { groupingSets.getFirst().add(grouping.size()); grouping.add(name); keys.add(input.resolve(name, null)); }
            }
        }
        if (source instanceof RelationPlan relation && groupingSets.size() == 1 && query.group().stream().noneMatch(term -> term instanceof SqlParser.Call call && call.name().startsWith("$"))) {
            for (RelationPlan.FunctionalKey dependency : relation.functionalKeys()) {
                if (dependency.determinant().stream().allMatch(index -> grouping.contains(bindingName(sourceBindings.get(index))))) {
                    for (int index : dependency.dependent()) {
                        SqlParser.Expr name = bindingName(sourceBindings.get(index));
                        if (!grouping.contains(name)) { groupingSets.getFirst().add(grouping.size()); grouping.add(name); keys.add(input.resolve(name, null)); }
                    }
                }
            }
        }
        List<SqlParser.Expr> rewritten = new ArrayList<>();
        for (SqlParser.Expr expression : projected) rewritten.add(rewrite(expression, input));
        SqlParser.Expr havingSyntax = query.having() == null ? null : rewrite(query.having(), input);
        List<SqlParser.Expr> ordered = new ArrayList<>();
        for (SqlParser.Order order : query.order()) {
            SqlParser.Expr expression = order.expression();
            if (expression instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.INTEGER) expression = rewrite(projected.get(ordinal(literal, projected.size(), "ORDER BY")), input);
            else if (expression instanceof SqlParser.Name name && name.parts().size() == 1 && labels.contains(name.parts().getFirst())) expression = rewrite(projected.get(label(name.parts().getFirst(), projected, "ORDER BY")), input);
            else {
                int selected = -1;
                for (int i = 0; i < projected.size(); i++) if (normalize(projected.get(i)).equals(normalize(expression))) { selected = i; break; }
                if (distinct && selected < 0) throw new SqlException("42P10", "DISTINCT ORDER BY expression must appear in select list");
                expression = rewrite(selected >= 0 ? projected.get(selected) : expression, input);
            }
            ordered.add(expression);
        }
        List<SqlParser.Expr> on = new ArrayList<>();
        for (SqlParser.Expr raw : query.distinctOn()) {
            SqlParser.Expr key = DistinctOn.resolve(raw, projected, labels); int selected = -1;
            for (int i = 0; i < projected.size(); i++) if (normalize(projected.get(i)).equals(normalize(key))) { selected = i; break; }
            int sorted = -1;
            for (int i = 0; i < query.order().size(); i++) if (normalize(DistinctOn.resolve(query.order().get(i).expression(), projected, labels)).equals(normalize(key))) { sorted = i; break; }
            // ORDER BY owns its own aggregate fold. DISTINCT ON compares that resolved
            // key, even when an equivalent SELECT expression has another fold slot.
            on.add(sorted >= 0 ? ordered.get(sorted) : selected >= 0 ? rewritten.get(selected) : rewrite(key, input));
        }
        List<ScalarPlan.Binding> outputs = new ArrayList<>();
        for (int i = 0; i < keys.size(); i++) outputs.add(groupBinding(i));
        for (int i = 0; i < folds.size(); i++) outputs.add(new ScalarPlan.Binding(List.of(), "$aggregate" + i, folds.get(i).type, false, false, foldCollation(folds.get(i))));
        for (int i = 0; i < groupingResults.size(); i++) outputs.add(new ScalarPlan.Binding(List.of(), "$grouping" + i, SqlType.I32));
        outputBindings = List.copyOf(outputs); outputMetadata = List.copyOf(rewritten); havingMetadata = havingSyntax;
        outputResolver = resolver(args, outputs, outputFrame).grouped(input);
        for (SqlParser.Expr expression : rewritten) projections.add(outputResolver.resolve(expression, null));
        having = filterExpression(outputResolver, havingSyntax);
        List<SqlParser.Order> rewrittenOrder = new ArrayList<>();
        for (int i = 0; i < ordered.size(); i++) rewrittenOrder.add(new SqlParser.Order(ordered.get(i), query.order().get(i).descending(), query.order().get(i).nullsFirst()));
        distinctOn = on.isEmpty() ? null : new DistinctOn(on, rewrittenOrder, rewritten, labels, outputs, outputResolver);
        effectiveOrder = distinctOn == null ? rewrittenOrder : distinctOn.order;
        for (SqlParser.Order order : effectiveOrder) { ScalarPlan.Expression key = outputResolver.resolve(order.expression(), null); ScalarPlan.requireComparable(key.type()); CollationDerivation.requireSort(key, order.expression()); orderKeys.add(key); }
        ScalarPlan.Resolver constants = environment == null ? new ScalarPlan.Resolver(args) : environment.resolver();
        limit = query.limit() == null ? null : constants.resolve(query.limit(), SqlType.I64);
        offset = query.offset() == null ? null : constants.resolve(query.offset(), SqlType.I64);
        if (limit != null && !limit.type().isInteger() || offset != null && !offset.type().isInteger()) throw SqlException.typeMismatch("row bound requires integer");
        if (source instanceof RelationPlan relation) relation.touch(inputResolver.touched());
        if (table != null && environment != null) environment.viewTable(table, inputResolver.touched());
    }
    private static SqlParser.Name bindingName(ScalarPlan.Binding binding) {
        List<String> parts = new ArrayList<>(binding.qualifier()); parts.add(binding.name()); return new SqlParser.Name(parts);
    }
    static List<List<SqlParser.Expr>> expand(SqlParser.Expr term) {
        if (!(term instanceof SqlParser.Call call) || !call.name().startsWith("$")) return List.of(List.of(term));
        List<List<SqlParser.Expr>> sets = new ArrayList<>();
        switch (call.name()) {
            case "$grouping_sets" -> { for (SqlParser.Expr arg : call.args()) { sets.addAll(expand(arg)); checkSets(sets.size()); } }
            case "$grouping_tuple" -> {
                sets.add(List.of()); for (SqlParser.Expr arg : call.args()) sets = cross(sets, expand(arg));
            }
            case "$rollup" -> {
                for (int size = call.args().size(); size >= 0; size--) {
                    List<List<SqlParser.Expr>> prefix = List.of(List.of());
                    for (int i = 0; i < size; i++) prefix = cross(prefix, expand(call.args().get(i)));
                    sets.addAll(prefix); checkSets(sets.size());
                }
            }
            case "$cube" -> {
                if (call.args().size() > 12) throw new SqlException("54001", "too many grouping sets");
                for (int mask = (1 << call.args().size()) - 1; mask >= 0; mask--) {
                    List<List<SqlParser.Expr>> subset = List.of(List.of());
                    for (int i = 0; i < call.args().size(); i++) if ((mask & (1 << (call.args().size() - 1 - i))) != 0) subset = cross(subset, expand(call.args().get(i)));
                    sets.addAll(subset); checkSets(sets.size());
                }
            }
            default -> throw new SqlException("42601", "invalid grouping form");
        }
        return sets;
    }
    static List<List<SqlParser.Expr>> cross(List<List<SqlParser.Expr>> left, List<List<SqlParser.Expr>> right) {
        checkSets((long) left.size() * right.size()); List<List<SqlParser.Expr>> out = new ArrayList<>();
        for (List<SqlParser.Expr> a : left) for (List<SqlParser.Expr> b : right) { List<SqlParser.Expr> values = new ArrayList<>(a); values.addAll(b); out.add(values); }
        return out;
    }
    private static void checkSets(long count) { if (count > 4096) throw new SqlException("54001", "too many grouping sets"); }
    private ScalarPlan.Resolver resolver(SqlValue[] args, List<ScalarPlan.Binding> bindings, ScalarPlan.Frame frame) {
        return environment == null ? new ScalarPlan.Resolver(args, bindings, frame) : environment.resolver(bindings, frame);
    }
    private int label(String name, List<SqlParser.Expr> projected, String clause) {
        int first = labels.indexOf(name);
        for (int i = first + 1; i < labels.size(); i++) if (labels.get(i).equals(name) && !projected.get(i).equals(projected.get(first))) throw new SqlException("42702", "ambiguous " + clause + " label");
        return first;
    }
    private static int ordinal(SqlParser.Literal literal, int width, String clause) {
        try { int value = ScalarPlan.ordinal(literal.text()); if (value > 0 && value <= width) return value - 1; }
        catch (NumberFormatException ignored) { }
        throw new SqlException("42P10", clause + " ordinal out of range");
    }
    private SqlParser.Expr normalize(SqlParser.Expr expression) {
        if (expression instanceof SqlParser.Name name && sourceBindings.stream().noneMatch(binding -> binding.name().equals(name.parts().getLast()))) return inputResolver.canonicalColumn(name);
        if (expression instanceof SqlParser.Name name && source != null) {
            List<ScalarPlan.Binding> matches = sourceBindings.stream().filter(binding -> binding.name().equals(name.parts().getLast()) && (name.parts().size() == 1 && !binding.qualifiedOnly() || name.parts().size() > 1 && binding.qualifier().equals(name.parts().subList(0, name.parts().size() - 1)))).toList();
            if (matches.size() == 1) { List<String> parts = new ArrayList<>(matches.getFirst().qualifier()); parts.add(matches.getFirst().name()); return new SqlParser.Name(parts); }
        }
        if (expression instanceof SqlParser.Name name && table != null && name.parts().size() == 2 && name.parts().getFirst().equals(alias == null ? table.name() : alias)) return new SqlParser.Name(List.of(name.parts().getLast()));
        if (expression instanceof SqlParser.JsonSql json) return json.map(this::normalize);
        if (expression instanceof SqlParser.Extrema extrema) return extrema.map(this::normalize);
        if (expression instanceof SqlParser.Binary binary) return new SqlParser.Binary(binary.op(), normalize(binary.left()), normalize(binary.right()));
        if (expression instanceof SqlParser.Unary unary) return new SqlParser.Unary(unary.op(), normalize(unary.value()));
        if (expression instanceof SqlParser.Cast cast) return new SqlParser.Cast(normalize(cast.value()), cast.target());
        if (expression instanceof SqlParser.Call call) return new SqlParser.Call(call.name(), call.args().stream().map(this::normalize).toList(), call.distinct(), call.star(), call.order(), call.filter(), call.withinGroup());
        return expression;
    }
    private ScalarPlan.Binding groupBinding(int index) {
        if (grouping.get(index) instanceof SqlParser.Name name) {
            for (ScalarPlan.Binding binding : sourceBindings) if (binding.name().equals(name.parts().getLast()) && (name.parts().size() == 1 || binding.qualifier().equals(name.parts().subList(0, name.parts().size() - 1)))) return binding;
        }
        return new ScalarPlan.Binding(List.of(), "$group" + index, keys.get(index).type(), false, false, keys.get(index).collation());
    }
    private SqlParser.Expr rewrite(SqlParser.Expr expression, ScalarPlan.Resolver input) {
        SqlParser.Expr result = rewriteBody(expression, input);
        if (environment != null) environment.estimateOrigin(expression, result);
        return result;
    }
    private SqlParser.Expr rewriteBody(SqlParser.Expr expression, ScalarPlan.Resolver input) {
        int key = grouping.indexOf(normalize(expression));
        if (key >= 0) {
            ScalarPlan.Binding binding = groupBinding(key); List<String> parts = new ArrayList<>(binding.qualifier()); parts.add(binding.name()); return new SqlParser.Name(parts);
        }
        if (expression instanceof SqlParser.Name name && name.parts().size() == 1 && sourceBindings.stream().noneMatch(binding -> binding.name().equals(name.parts().getFirst()) && !binding.qualifiedOnly())) {
            List<ScalarPlan.Binding> whole = sourceBindings.stream().filter(binding -> binding.qualifier().equals(name.parts()) && binding.whole().allowed()).toList();
            if (!whole.isEmpty() && whole.stream().allMatch(binding -> grouping.contains(normalize(bindingName(binding))))) { input.resolve(name, null); return name; }
        }
        if (expression instanceof SqlParser.Name) {
            long[] references = input.referenceUsage(); input.resolve(expression, null);
            if (input.outerOnlySince(references)) return expression;
            throw new SqlException("42803", "ungrouped column in aggregate query");
        }
        if (expression instanceof SqlParser.Star) throw new SqlException("42803", "ungrouped column in aggregate query");
        if (expression instanceof SqlParser.Call call) {
            if (call.name().equals("grouping")) {
                if (call.star() || call.args().isEmpty()) throw new SqlException("42601", "GROUPING requires columns");
                if (call.args().size() > 31) throw new SqlException("54001", "too many GROUPING arguments");
                List<Integer> positions = new ArrayList<>();
                for (SqlParser.Expr arg : call.args()) {
                    long[] references = input.referenceUsage(); input.resolve(arg, null);
                    if (input.outerOnlySince(references)) throw new SqlException("42803", "GROUPING argument belongs to an outer query");
                    int position = grouping.indexOf(normalize(arg));
                    if (!(arg instanceof SqlParser.Name) || position < 0) throw new SqlException("42803", "GROUPING argument is not a grouping column");
                    positions.add(position);
                }
                int index = groupingResults.size(); groupingResults.add(positions); return new SqlParser.Name(List.of("$grouping" + index));
            }
            if (call.withinGroup() || OrderedSet.NAMES.contains(call.name())) {
                OrderedSet.validate(call);
                if (call.args().stream().anyMatch(AggregatePlan::contains) || contains(call.filter()) || call.args().stream().anyMatch(WindowSyntax::contains)) throw new SqlException("42803", "nested ordered-set aggregate");
                List<ScalarPlan.Binding> bindings = new ArrayList<>(); for (int i = 0; i < grouping.size(); i++) bindings.add(groupBinding(i));
                List<SqlParser.Expr> direct = call.args().stream().map(arg -> rewrite(arg, input)).toList();
                Fold fold = OrderedSet.resolve(call, input, resolver(arguments, bindings, outputFrame), direct);
                int index = folds.size(); folds.add(fold); foldMetadata.add(call); return new SqlParser.Name(List.of("$aggregate" + index));
            }
            if (NAMES.contains(call.name())) {
                int index = folds.size(); folds.add(resolve(call, input)); foldMetadata.add(call);
                return new SqlParser.Name(List.of("$aggregate" + index));
            }
            return new SqlParser.Call(call.name(), call.args().stream().map(arg -> rewrite(arg, input)).toList(), call.distinct(), call.star(), call.order(), call.filter(), call.withinGroup());
        }
        if (expression instanceof SqlParser.JsonSql json) return json.map(arg -> rewrite(arg, input));
        if (expression instanceof SqlParser.Extrema extrema) return extrema.map(arg -> rewrite(arg, input));
        if (expression instanceof SqlParser.Unary unary) return new SqlParser.Unary(unary.op(), rewrite(unary.value(), input));
        if (expression instanceof SqlParser.Binary binary) return new SqlParser.Binary(binary.op(), rewrite(binary.left(), input), rewrite(binary.right(), input));
        if (expression instanceof SqlParser.Cast cast) return new SqlParser.Cast(rewrite(cast.value(), input), cast.target());
        if (expression instanceof SqlParser.In in) return new SqlParser.In(rewrite(in.subject(), input), in.values().stream().map(arg -> rewrite(arg, input)).toList(), in.query(), in.negated());
        if (expression instanceof SqlParser.Between between) return new SqlParser.Between(rewrite(between.subject(), input), rewrite(between.lower(), input), rewrite(between.upper(), input), between.negated());
        if (expression instanceof SqlParser.Conditional branch) return new SqlParser.Conditional(branch.input() == null ? null : rewrite(branch.input(), input), branch.conditions().stream().map(arg -> rewrite(arg, input)).toList(), branch.results().stream().map(arg -> rewrite(arg, input)).toList(), branch.otherwise() == null ? null : rewrite(branch.otherwise(), input));
        if (expression instanceof SqlParser.ArrayConstructor array) return new SqlParser.ArrayConstructor(array.values().stream().map(arg -> rewrite(arg, input)).toList(), array.query());
        if (expression instanceof SqlParser.RowConstructor row) return new SqlParser.RowConstructor(row.values().stream().map(arg -> rewrite(arg, input)).toList());
        if (expression instanceof SqlParser.Access access) return new SqlParser.Access(rewrite(access.base(), input), access.subscripts().stream().map(sub -> new SqlParser.Subscript(sub.lower() == null ? null : rewrite(sub.lower(), input), sub.upper() == null ? null : rewrite(sub.upper(), input), sub.slice())).toList());
        if (expression instanceof SqlParser.Quantified quantified) return new SqlParser.Quantified(quantified.operator(), rewrite(quantified.subject(), input), quantified.quantifier(), quantified.array() == null ? null : rewrite(quantified.array(), input), quantified.query());
        if (expression instanceof SqlParser.NamedArgument named) return new SqlParser.NamedArgument(named.name(), rewrite(named.value(), input));
        if (expression instanceof SqlParser.Field field) return new SqlParser.Field(rewrite(field.base(), input), field.name());
        if (expression instanceof SqlParser.Collate collate) return new SqlParser.Collate(rewrite(collate.value(), input), collate.name());
        return expression;
    }
    static Fold resolve(SqlParser.Call call, ScalarPlan.Resolver input) {
        if (call.args().stream().anyMatch(WindowSyntax::contains) || WindowSyntax.contains(call.filter())) throw new SqlException("42803", "aggregate cannot contain a window function");
        if (call.args().stream().anyMatch(AggregatePlan::contains) || contains(call.filter())) throw new SqlException("42803", "nested aggregate");
        if (call.star() && !call.name().equals("count") || !call.star() && call.args().size() != (objectFold(call.name()) || call.name().equals("string_agg") || AggregateStatistics.binary(call.name()) ? 2 : 1)) throw new SqlException("42883", "aggregate argument count");
        long[] usage = input.referenceUsage();
        ScalarPlan.Expression operand;
        if (AggregateStatistics.binary(call.name())) {
            ScalarPlan.Expression y = input.resolve(call.args().get(0), SqlType.F64), x = input.resolve(call.args().get(1), SqlType.F64);
            if (!y.type().isNumeric() || !x.type().isNumeric()) throw new SqlException("42883", "statistical aggregate requires numeric arguments");
            SqlType tuple = SqlType.record(List.of(new SqlType.Field("y", SqlType.F64), new SqlType.Field("x", SqlType.F64)));
            operand = new ScalarPlan.Expression(tuple, meter -> { meter.operation(); return SqlValue.ofComposite(tuple, List.of(SqlCasts.cast(y.run(meter), SqlType.F64), SqlCasts.cast(x.run(meter), SqlType.F64))); }, y.constant() && x.constant());
        } else if (call.name().equals("string_agg")) {
            ScalarPlan.Expression value = input.resolve(call.args().get(0), null);
            SqlType family = value.type() == SqlType.BYTEA ? SqlType.BYTEA : SqlType.TEXT;
            value = input.resolve(call.args().get(0), family); ScalarPlan.Expression delimiter = input.resolve(call.args().get(1), family);
            if (value.type() != family && value.type() != SqlType.UNKNOWN || delimiter.type() != family && delimiter.type() != SqlType.UNKNOWN) throw new SqlException("42883", "string_agg argument families do not match");
            SqlType tuple = SqlType.record(List.of(new SqlType.Field("value", family), new SqlType.Field("delimiter", family)));
            ScalarPlan.Expression first = value;
            operand = new ScalarPlan.Expression(tuple, meter -> { meter.operation(); return SqlValue.ofComposite(tuple, List.of(first.run(meter).relabel(family), delimiter.run(meter).relabel(family))); }, value.constant() && delimiter.constant());
        } else if (objectFold(call.name())) {
            ScalarPlan.Expression key = input.resolve(call.args().get(0), null), value = input.resolve(call.args().get(1), null);
            if (call.distinct() && (key.type() == SqlType.JSON || value.type() == SqlType.JSON)) throw new SqlException("42883", "json has no equality operator");
            SqlType tuple = SqlType.record(List.of(new SqlType.Field("key", key.type()), new SqlType.Field("value", value.type())));
            operand = new ScalarPlan.Expression(tuple, meter -> { meter.operation(); return SqlValue.ofComposite(tuple, List.of(key.run(meter), value.run(meter))); }, key.constant() && value.constant());
        } else operand = call.star() ? null : input.resolve(call.args().getFirst(), null);
        ScalarPlan.Expression filter = booleanExpression(input, call.filter());
        List<ScalarPlan.Expression> orderKeys = new ArrayList<>();
        for (SqlParser.Order key : call.order()) {
            if (contains(key.expression()) || WindowSyntax.contains(key.expression())) throw new SqlException("42803", "aggregate ORDER BY cannot contain an aggregate or window");
            if (call.distinct() && call.args().stream().noneMatch(arg -> arg.equals(key.expression()) || arg instanceof SqlParser.Name a && key.expression() instanceof SqlParser.Name b && a.parts().getLast().equals(b.parts().getLast()) && (a.parts().size() == 1 || b.parts().size() == 1 || a.parts().equals(b.parts())))) throw new SqlException("42P10", "DISTINCT aggregate ORDER BY must appear in argument list");
            ScalarPlan.Expression resolved = input.resolve(key.expression(), null);
            if (resolved.type() == SqlType.JSON) throw new SqlException("42883", "json has no ordering operator");
            CollationDerivation.requireSort(resolved, key.expression()); orderKeys.add(resolved);
        }
        if (input.outerOnlySince(usage)) throw new SqlException("42803", "outer-level aggregate is not supported");
        SqlType type = operand == null ? SqlType.I64 : operand.type();
        if (call.distinct() && type == SqlType.JSON) throw new SqlException("42883", "json has no equality operator");
        SqlType result = switch (call.name()) {
            case "count", "regr_count" -> SqlType.I64;
            case "corr", "covar_pop", "covar_samp", "regr_avgx", "regr_avgy", "regr_sxx", "regr_syy", "regr_sxy", "regr_slope", "regr_intercept", "regr_r2" -> SqlType.F64;
            case "stddev", "stddev_pop", "stddev_samp", "variance", "var_pop", "var_samp" -> { if (!type.isNumeric()) throw new SqlException("42883", "statistical aggregate requires numeric input"); yield type.isFloat() ? SqlType.F64 : SqlType.DECIMAL; }
            case "string_agg" -> type.fields().getFirst().type();
            case "min", "max" -> switch (type.kind()) { case TEXT -> SqlType.TEXT; case DECIMAL -> SqlType.DECIMAL; case TIMESTAMP -> SqlType.TIMESTAMP; case TIMESTAMPTZ -> SqlType.TIMESTAMPTZ; case INTERVAL -> SqlType.INTERVAL; default -> type; };
            case "json_agg", "json_agg_strict", "json_object_agg", "json_object_agg_strict", "json_object_agg_unique", "json_object_agg_unique_strict" -> SqlType.JSON;
            case "jsonb_agg", "jsonb_agg_strict", "jsonb_object_agg", "jsonb_object_agg_strict", "jsonb_object_agg_unique", "jsonb_object_agg_unique_strict" -> SqlType.JSONB;
            case "array_agg" -> { if (type == SqlType.UNKNOWN) throw new SqlException("42725", "array_agg input type is ambiguous"); yield type.kind() == SqlType.Kind.ARRAY ? type : SqlType.array(type); }
            case "sum", "avg" -> {
                if (!type.isNumeric()) throw new SqlException("42883", "numeric aggregate requires numeric input");
                if (type.isFloat()) yield type;
                yield call.name().equals("sum") && (type == SqlType.I16 || type == SqlType.I32) ? SqlType.I64 : SqlType.DECIMAL;
            }
            case "bool_and", "bool_or", "every" -> { if (type != SqlType.BOOLEAN) throw new SqlException("42883", "boolean aggregate requires boolean"); yield SqlType.BOOLEAN; }
            case "bit_and", "bit_or", "bit_xor" -> { if (!type.isInteger()) throw new SqlException("42883", "bit aggregate requires integer"); yield type; }
            default -> throw new SqlException("0A000", "aggregate is not implemented: " + call.name());
        };
        if (operand != null && (call.distinct() || call.name().equals("min") || call.name().equals("max"))) operand.collation().require();
        return new Fold(call.name(), operand, filter, result, call.distinct(), call.order(), orderKeys);
    }
    private final PlanMetrics metrics = new PlanMetrics();
    private boolean analyzed;
    public void requireAnalyze() { analyzed = true; if (source != null) source.requireAnalyze(); }
    public PlanNode actualPlan(boolean verbose) {
        PlanNode input = source == null ? null : source.actualPlan(verbose);
        return metrics.render(describe(verbose, input), input);
    }
    @Override public PlanNode explain(boolean verbose) { return describe(verbose, null); }
    private PlanNode describe(boolean verbose, PlanNode sourceActual) {
        PlanNode input;
        if (table != null) {
            SqlParser.Select scan = new SqlParser.Select(List.of(), query.where(), List.of(), null, null, false);
            input = TableExplanation.describe(scan, table, table.columns().stream().map(Engine.Column::name).toList(), alias == null ? table.name() : alias, List.of(), sourceBindings, inputResolver, environment.meter(), false, false, false, verbose, false);
        } else if (source != null) {
            input = sourceActual == null ? source.explain(verbose) : sourceActual;
            if (query.where() != null) {
                ExplainExpression printer = new ExplainExpression(sourceBindings, inputResolver, environment.meter());
                long hidden = environment.hiddenEstimate(query.where(), input.rows());
                input = input.wrap("Filter", verbose ? "filter=" + printer.print(TableExplanation.orderedPredicate(query.where(), environment)) : "conjuncts=" + FilterRules.terms(query.where()).size(),
                    PlanEstimate.rows(query.where(), input.rows(), List.of()), PlanEstimate.add(hidden, PlanEstimate.mul(input.rows(), PlanEstimate.operations(query.where()))));
            }
        } else input = new PlanNode("Result", "", 1, 0, List.of(), "buffered");
        long groups = query.group().isEmpty() ? 1 : table == null ? PlanEstimate.groups(input.rows(), query.group().size()) : new StatisticsEstimate(table, table.columns().stream().map(Engine.Column::name).toList(), alias == null ? table.name() : alias, inputResolver, environment.meter()).groups(query.group(), input.rows());
        if (groupingSets.size() > 1) groups = PlanEstimate.mul(groups, groupingSets.size());
        long work = folds.size();
        for (SqlParser.Call call : foldMetadata) work = PlanEstimate.add(work, aggregateInputWork(call));
        for (SqlParser.Expr key : grouping) work = PlanEstimate.add(work, PlanEstimate.operations(key));
        String detail = "groups=" + grouping.size() + " aggs=" + folds.size();
        if (groupingSets.size() > 1) detail += "; sets=" + groupingSets.size();
        ExplainExpression printer = new ExplainExpression(outputBindings, outputResolver, environment.meter());
        if (query.having() != null) detail += verbose ? "; having=" + printer.print(havingMetadata) : "; having:conjuncts=" + FilterRules.terms(query.having()).size();
        long aggregateWork = PlanEstimate.add(PlanEstimate.mul(input.rows(), work), PlanEstimate.mul(groups, aggregateOutputWork(query.having())));
        long hiddenInput = 0;
        for (SqlParser.Call call : foldMetadata) {
            hiddenInput = PlanEstimate.add(hiddenInput, environment.hiddenEstimate(call.filter(), input.rows()));
            for (SqlParser.Expr argument : call.args()) hiddenInput = PlanEstimate.add(hiddenInput, environment.hiddenEstimate(argument, input.rows()));
            for (SqlParser.Order order : call.order()) hiddenInput = PlanEstimate.add(hiddenInput, environment.hiddenEstimate(order.expression(), input.rows()));
        }
        for (SqlParser.Expr key : grouping) hiddenInput = PlanEstimate.add(hiddenInput, environment.hiddenEstimate(key, input.rows()));
        aggregateWork = PlanEstimate.add(aggregateWork, PlanEstimate.add(hiddenInput, environment.hiddenEstimate(havingMetadata, groups)));
        PlanNode node = input.wrap("Aggregate", detail, groups, aggregateWork);
        if (query.having() != null) node = new PlanNode(node.label(), node.detail(), PlanEstimate.rows(query.having(), groups, List.of()), node.cost(), node.children(), node.lane());
        if (distinct) node = node.wrap("Distinct", "", PlanEstimate.groups(node.rows(), projections.size()), 0);
        if (!effectiveOrder.isEmpty()) {
            long sortWork = 0; for (SqlParser.Order order : effectiveOrder) sortWork = PlanEstimate.add(sortWork, PlanEstimate.operations(order.expression()));
            long hiddenSort = 0; for (SqlParser.Order order : effectiveOrder) hiddenSort = PlanEstimate.add(hiddenSort, environment.hiddenEstimate(order.expression(), node.rows()));
            node = node.wrap("Sort", "keys=" + effectiveOrder.size(), node.rows(), PlanEstimate.add(hiddenSort, PlanEstimate.mul(node.rows(), sortWork)));
        }
        if (distinctOn != null) node = node.wrap("Distinct", "on=" + distinctOn.keys.size(), PlanEstimate.groups(node.rows(), distinctOn.keys.size()), 0);
        long take = TableExplanation.boundEstimate(query.limit(), Long.MAX_VALUE, inputResolver, environment == null ? null : environment.meter()), skip = TableExplanation.boundEstimate(query.offset(), 0, inputResolver, environment == null ? null : environment.meter());
        if (take != Long.MAX_VALUE || skip != 0) node = node.wrap("Limit", (take == Long.MAX_VALUE ? "" : "limit=" + take) + (query.offset() == null ? "" : (take == Long.MAX_VALUE ? "" : " ") + "offset=" + skip), Math.min(take, Math.max(0, node.rows() - skip)), 0);
        long output = 1; for (SqlParser.Item item : query.items()) output = PlanEstimate.add(output, aggregateOutputWork(item.expression()));
        String finalDetail = verbose ? TableExplanation.append(node.detail(), "output=[" + printer.list(outputMetadata) + "]") : node.detail();
        long hiddenOutput = 0; for (SqlParser.Expr expression : outputMetadata) hiddenOutput = PlanEstimate.add(hiddenOutput, environment.hiddenEstimate(expression, node.rows()));
        return node.finish(finalDetail, PlanEstimate.add(hiddenOutput, PlanEstimate.mul(node.rows(), output)), "buffered");
    }
    private static long aggregateInputWork(SqlParser.Expr expression) {
        if (expression == null) return 0;
        if (expression instanceof SqlParser.Call call && aggregateName(call.name())) { long work = PlanEstimate.operations(call.filter()); for (SqlParser.Expr arg : call.args()) work = PlanEstimate.add(work, PlanEstimate.operations(arg)); for (SqlParser.Order order : call.order()) work = PlanEstimate.add(work, PlanEstimate.operations(order.expression())); return work; }
        long work = 0; for (SqlParser.Expr child : DefaultExpression.children(expression)) work = PlanEstimate.add(work, aggregateInputWork(child)); return work;
    }
    private static long aggregateOutputWork(SqlParser.Expr expression) {
        if (expression == null || expression instanceof SqlParser.Call call && aggregateName(call.name())) return 0;
        long all = PlanEstimate.operations(expression), input = 0;
        for (SqlParser.Expr child : DefaultExpression.children(expression)) input = PlanEstimate.add(input, PlanEstimate.operations(child) - aggregateOutputWork(child));
        return Math.max(0, all - input);
    }
    @Override public List<String> names() { return List.copyOf(labels); }
    @Override public List<SqlType> types() { return projections.stream().map(ScalarPlan.Expression::type).toList(); }
    @Override public List<CollationDerivation> collations() { return projections.stream().map(ScalarPlan.Expression::collation).toList(); }
    static CollationDerivation foldCollation(Fold fold) { return fold.input != null && CollationDerivation.collatable(fold.type) ? fold.input.collation() : CollationDerivation.NONE; }
    @Override public Row next(CostMeter meter) {
        long before = meter.cost();
        try { return nextMeasured(meter); } finally { metrics.total(before, meter); }
    }
    private Row nextMeasured(CostMeter meter) {
        if (closed) return null;
        if (!started) {
            started = true; remaining = bound(limit, meter, Long.MAX_VALUE, "2201W"); skip = bound(offset, meter, 0, "2201X");
            build(meter);
        }
        while (remaining > 0) {
            List<SqlValue> values = output.next();
            if (values == null) { close(); return null; }
            if (distinctOn != null && !distinctOn.accept(values, keys.size() + folds.size() + groupingResults.size())) continue;
            if (skip > 0) { skip--; continue; }
            remaining--; if (produce) meter.charge(Costs.ROW_PRODUCED); meter.guard();
            if (distinct) return new Row(labels, values.subList(0, projections.size()));
            outputFrame.values = values;
            List<SqlValue> result = new ArrayList<>(); for (ScalarPlan.Expression projection : projections) result.add(projection.run(meter));
            return new Row(labels, result);
        }
        close(); return null;
    }
    private void build(CostMeter meter) {
        boolean lane = !analyzed && columnarShape() && discardSafe(query.where());
        if (lane) for (SqlParser.Item item : query.items()) lane &= discardSafe(item.expression());
        if (lane) for (SqlParser.Order order : query.order()) lane &= discardSafe(order.expression());
        if (!lane) { build(meter, false); return; }
        if (environment != null) environment.initialize(meter);
        meter.guard(); CostMeter.Checkpoint saved = meter.checkpoint();
        try { build(meter, true); meter.guard(); }
        catch (SqlException failure) {
            if (failure.sqlState().equals("57014")) throw failure;
            discardBuild(); meter.restore(saved); metrics.clear(); build(meter, false);
        }
    }
    private void discardBuild() {
        if (output != null) output.close(); if (sorter != null) sorter.close(); if (results != null) results.close();
        if (scannedInput != null) scannedInput.close(); if (groupKeyRows != null) groupKeyRows.close();
        if (columnarHeld != 0) { groupAccount.release(columnarHeld); columnarHeld = 0; } columnarCharges = null; columnarRejected = false;
        if (retainedInput != null) retainedInput.close(); if (groups != null) groups.close();
        if (collections != null) collections.close(); if (distinctInputs != null) distinctInputs.close(); if (distinctOutputs != null) distinctOutputs.close();
        output = null; sorter = null; results = null; retainedInput = null; groups = null; collections = null; distinctInputs = null; distinctOutputs = null;
        collectingInput = false; scannedInput = null; groupKeyRows = null; distinctGroupInputs.clear();
    }
    private boolean discardSafe(SqlParser.Expr syntax) {
        if (syntax == null) return true;
        syntax = DefaultExpression.unwrap(syntax);
        if (syntax instanceof SqlParser.Subquery || syntax instanceof SqlParser.Exists) return environment != null && environment.hasInit(syntax);
        if (syntax instanceof SqlParser.In in && in.query() != null) {
            if (environment == null || !environment.hasInit(syntax)) return false;
            RowSpool values = environment.foldedMembership(syntax);
            if (values == null) return false;
            SqlType subject = inputResolver.resolve(in.subject(), null).type();
            // Exact-type folded membership is also a retained point source.
            if (subject.equals(values.types().getFirst())) return false;
        }
        if (syntax instanceof SqlParser.Call call) {
            if (!aggregateName(call.name()) && !Set.of("coalesce", "nullif").contains(call.name())) {
                if (call.name().startsWith("regexp") || call.name().startsWith("json") || call.name().startsWith("array") || call.name().startsWith("range")) return false;
                boolean found = false;
                for (FunctionCatalog.Fact fact : FunctionCatalog.FACTS) if (fact.name().equals(call.name())) {
                    found = true; if (fact.volatility().equals("volatile")) return false;
                }
                if (!found) return false;
            }
        } else if (syntax instanceof SqlParser.Binary binary) {
            if (!Set.of("+", "-", "*", "/", "%", "^", "=", "<>", "!=", "<", "<=", ">", ">=", "and", "or", "like", "not like", "ilike", "not ilike", "is distinct from", "is not distinct from", "at time zone").contains(binary.op().toLowerCase(java.util.Locale.ROOT))) return false;
        } else if (!(syntax instanceof SqlParser.Literal || syntax instanceof SqlParser.Name || syntax instanceof SqlParser.ColumnSlot || syntax instanceof SqlParser.Parameter || syntax instanceof SqlParser.StoredValue || syntax instanceof SqlParser.Unary || syntax instanceof SqlParser.Cast || syntax instanceof SqlParser.Conditional || syntax instanceof SqlParser.Extrema || syntax instanceof SqlParser.In || syntax instanceof SqlParser.Between || syntax instanceof SqlParser.Collate)) return false;
        if (!(syntax instanceof SqlParser.Call call && aggregateName(call.name()))) {
            SqlType type;
            try { type = inputResolver.resolve(syntax, null).type(); }
            catch (SqlException unresolvedOutput) { return false; }
            if (type.kind() == SqlType.Kind.ARRAY || type.kind() == SqlType.Kind.RANGE || type.isComposite() || type == SqlType.JSON || type == SqlType.JSONB || type == SqlType.JSONPATH) return false;
        }
        for (SqlParser.Expr child : DefaultExpression.children(syntax)) if (!discardSafe(child)) return false;
        return true;
    }
    private void build(CostMeter meter, boolean lane) {
        groupAccount = meter.memory();
        boolean boundedLane = lane && scratch != null && workMem > 0;
        columnarCharges = boundedLane ? new java.util.LinkedHashMap<>() : null;
        groups = new SpillKeyState(boundedLane ? null : scratch, boundedLane ? 0 : workMem, boundedLane ? null : meter.memory());
        collections = new AggregateCollections(scratch, workMem, meter.memory());
        distinctInputs = new DistinctState(scratch, workMem, meter.memory());
        boolean eager = (scratch == null || workMem == 0) && !lane;
        boolean bounded = scratch != null && workMem > 0 && !boundedLane;
        boolean collectSource = !eager && source instanceof RelationPlan relation && relation.projectedResult();
        if (bounded) groupKeyRows = new RowSpool(keys.stream().map(ScalarPlan.Expression::type).toList(), workMem, scratch, meter.memory());
        if (eager || bounded) {
            List<SqlType> inputTypes = source != null ? source.types() : table == null ? List.of() : table.types();
            retainedInput = new RowSpool(inputTypes, bounded ? workMem : 0, bounded ? scratch : null, meter.memory()); collectingInput = true;
        } else if (!collectSource) initializeEmptyGroups();
        try {
            if (Contradictions.proven(query.where(), arguments)) { if (source != null) source.close(); }
            else if (source != null) {
                if (collectSource) {
                    materializedSource = new RowSpool(source.types(), 0, null, meter.memory());
                    source.collect(materializedSource, meter);
                    source.close();
                    try (RowSpool.Cursor input = materializedSource.cursor()) {
                        List<SqlValue> values; while ((values = input.next()) != null) { inputFrame.values = values; accumulate(meter); }
                    }
                } else {
                    Row row; while ((row = source.next(meter)) != null) { inputFrame.values = row.values(); accumulate(meter); }
                    source.close();
                }
            } else if (table == null) accumulate(meter);
            else {
                if (bounded) scannedInput = new RowSpool(table.types(), workMem, scratch, meter.memory());
                long beforeScan = meter.cost();
                try (TableScan scan = new TableScan(table, alias, sourceBindings.stream().map(ScalarPlan.Binding::name).toList(), query.where(), arguments, meter, inputResolver.touched(), environment)) {
                    metrics.add("Scan", beforeScan, meter);
                    while (true) {
                        long before = meter.cost(); PersistentTree.Entry entry = scan.next();
                        if (entry != null) { meter.charge(Costs.STORAGE_ROW_READ); meter.guard(); inputFrame.values = Encoding.row(entry, table.types()); }
                        metrics.add("Scan", before, meter); if (entry == null) break;
                        if (bounded) scannedInput.append(inputFrame.values); else accumulate(meter);
                    }
                }
                if (bounded) {
                    try (RowSpool.Cursor input = scannedInput.cursor()) { List<SqlValue> values; while ((values = input.next()) != null) { inputFrame.values = values; accumulate(meter); } }
                    scannedInput.close(); scannedInput = null;
                }
            }
        } catch (IOException error) { throw Engine.storageError(error); }
        if (eager || bounded) {
            collectingInput = false; initializeEmptyGroups();
            try (RowSpool.Cursor cursor = retainedInput.cursor()) {
                List<SqlValue> values;
                while ((values = cursor.next()) != null) {
                    inputFrame.values = values; long before = meter.cost();
                    try { accumulateAccepted(meter); } finally { metrics.add("Aggregate", before, meter); }
                }
            }
        }
        if (columnarCharges != null) { groupAccount.release(columnarHeld); columnarHeld = 0; columnarCharges = null; }
        if (bounded && retainedInput != null) { retainedInput.close(); retainedInput = null; }
        List<SqlType> rowTypes = new ArrayList<>();
        if (distinct) rowTypes.addAll(types());
        else { for (ScalarPlan.Expression key : keys) rowTypes.add(key.type()); for (Fold fold : folds) rowTypes.add(fold.type); for (int i = 0; i < groupingResults.size(); i++) rowTypes.add(SqlType.I32); }
        int orderedAt = rowTypes.size();
        for (ScalarPlan.Expression key : orderKeys) rowTypes.add(CollationDerivation.sortType(key));
        results = new RowSpool(rowTypes, workMem, scratch, meter.memory());
        if (!orderKeys.isEmpty()) sorter = new ExternalSorter(rowTypes, workMem, scratch, meter.memory(), (a, b) -> compare(a, b, orderedAt), row -> {
            if (scratch != null && workMem != 0) return MemoryAccount.row(row);
            long bytes = MemoryAccount.row(row.subList(0, orderedAt));
            for (int i = 0; i < orderKeys.size(); i++) {
                ScalarPlan.Expression key = orderKeys.get(i); SqlValue value = row.get(orderedAt + i);
                boolean collated = !key.collation().require().equals("C") && CollationDerivation.collatable(key.type());
                if (collated ? !value.isNull() : !(effectiveOrder.get(i).expression() instanceof SqlParser.Name || effectiveOrder.get(i).expression() instanceof SqlParser.ColumnSlot)) bytes = MemoryAccount.add(bytes, MemoryAccount.value(value));
            }
            return bytes;
        });
        if (distinct) distinctOutputs = new DistinctState(scratch, workMem, meter.memory());
        try (SpillKeyState.Cursor cursor = groups.cursor()) {
            SpillKeyState.Entry entry;
            while ((entry = cursor.next()) != null) {
                long beforeFinish = meter.cost();
                Group group = group(entry.value());
                attachCollections(group, entry.value().ordinal());
                outputFrame.values = group.keys;
                List<SqlValue> row = new ArrayList<>(group.keys);
                for (int i = 0; i < group.states.size(); i++) row.add(finish(group.states.get(i), entry.value().ordinal(), i, meter));
                for (List<Integer> positions : groupingResults) {
                    int mask = 0; for (int position : positions) mask = (mask << 1) | (groupingSets.get(group.set).contains(position) ? 0 : 1);
                    row.add(SqlValue.ofInteger(SqlType.I32, mask));
                }
                for (int i = 0; i < group.states.size(); i++) {
                    State state = group.states.get(i);
                    if (state.collection != null || state.ordered != null) collections.discard(entry.value().ordinal(), i, state.count, state.fold.order.isEmpty() && state.fold.name.contains("_unique"));
                }
                List<List<SqlValue>> inputKeys = distinctGroupInputs.remove(entry.value().ordinal());
                if (inputKeys != null) for (List<SqlValue> inputKey : inputKeys) distinctInputs.discard(inputKey);
                if (scratch == null || workMem == 0) cursor.discardCurrent();
                outputFrame.values = row;
                boolean accepted = passes(having, meter); metrics.add("Aggregate", beforeFinish, meter);
                if (!accepted) continue;
                List<SqlValue> staged = row;
                if (distinct) {
                    long beforeDistinct = meter.cost();
                    staged = new ArrayList<>(); for (ScalarPlan.Expression projection : projections) staged.add(projection.run(meter));
                    chargeKeys(staged, Costs.HASH_KEY, meter); boolean fresh = distinctOutputs.add(staged); metrics.add("Distinct", beforeDistinct, meter); if (!fresh) continue;
                }
                long beforeSort = meter.cost();
                for (ScalarPlan.Expression key : orderKeys) { SqlValue value = sortKey(key, key.run(meter), meter); staged.add(value); }
                metrics.add("Sort", beforeSort, meter);
                if (sorter == null) results.append(staged); else sorter.append(staged);
            }
        }
        groups.close(); collections.close(); distinctInputs.close(); if (distinctOutputs != null) distinctOutputs.close();
        if (groupKeyRows != null) { groupKeyRows.close(); groupKeyRows = null; }
        if (retainedInput != null) { retainedInput.close(); retainedInput = null; }
        if (materializedSource != null) { materializedSource.close(); materializedSource = null; }
        output = sorter == null ? results.cursor() : sorter.finish();
    }
    private void initializeEmptyGroups() {
        for (int set = 0; set < groupingSets.size(); set++) if (groupingSets.get(set).isEmpty()) {
            List<SqlValue> empty = keys.stream().map(key -> SqlValue.nullValue(key.type())).toList();
            if (groupKeyRows != null) groupKeyRows.append(empty);
            put(new Group(empty, fresh(), set), DistinctState.key(List.of(SqlValue.ofInteger(set))));
        }
    }
    private boolean columnarShape() {
        if (table == null || source != null || distinct || distinctOn != null || having != null || groupingSets.size() != 1 || grouping.size() > 1 || !groupingResults.isEmpty()) return false;
        if (!grouping.isEmpty() && (!(grouping.getFirst() instanceof SqlParser.Name || grouping.getFirst() instanceof SqlParser.ColumnSlot) || !keys.getFirst().type().isInteger())) return false;
        for (Fold fold : folds) if (!Set.of("count", "sum", "avg", "min", "max").contains(fold.name) || fold.distinct || fold.filter != null || !fold.order.isEmpty()) return false;
        return true;
    }
    private int compare(List<SqlValue> left, List<SqlValue> right, int start) {
        for (int i = 0; i < effectiveOrder.size(); i++) {
            SqlValue a = left.get(start + i), b = right.get(start + i); SqlParser.Order order = effectiveOrder.get(i);
            boolean nullFirst = order.nullsFirst() == null ? order.descending() : order.nullsFirst();
            int comparison;
            if (a.isNull() || b.isNull()) comparison = a.isNull() == b.isNull() ? 0 : a.isNull() == nullFirst ? -1 : 1;
            else { comparison = ScalarPlan.compare(a, b, null); if (order.descending()) comparison = -comparison; }
            if (comparison != 0) return comparison;
        }
        return 0;
    }
    private List<State> fresh() { return folds.stream().map(State::new).toList(); }
    private void accumulate(CostMeter meter) {
        long beforeFilter = meter.cost(); boolean accepted = passes(where, meter); metrics.add("Filter", beforeFilter, meter);
        if (!accepted) return;
        if (collectingInput) {
            boolean[] touched = inputResolver.touched(); List<SqlValue> row = new ArrayList<>(inputFrame.values);
            for (int i = 0; i < row.size(); i++) if (i < touched.length && !touched[i]) row.set(i, SqlValue.nullValue(row.get(i).type()));
            retainedInput.append(row); return;
        }
        long before = meter.cost();
        try { accumulateAccepted(meter); } finally { metrics.add("Aggregate", before, meter); }
    }
    private void accumulateAccepted(CostMeter meter) {
        List<SqlValue> values = new ArrayList<>(); for (ScalarPlan.Expression key : keys) values.add(key.run(meter));
        for (int set = 0; set < groupingSets.size(); set++) {
            List<SqlValue> active = new ArrayList<>(); for (int index : groupingSets.get(set)) active.add(values.get(index));
            chargeKeys(active, Costs.HASH_KEY, meter);
            List<SqlValue> encodedKey = new ArrayList<>(); encodedKey.add(SqlValue.ofInteger(set)); encodedKey.addAll(active);
            byte[] encoded = DistinctState.key(encodedKey); SpillKeyState.Value previous = groups.get(encoded);
            Group group;
            if (previous == null) {
                List<SqlValue> row = new ArrayList<>();
                for (int i = 0; i < keys.size(); i++) row.add(groupingSets.get(set).contains(i) ? values.get(i) : SqlValue.nullValue(keys.get(i).type()));
                group = new Group(row, fresh(), set);
                if (groupKeyRows != null) groupKeyRows.append(row);
            } else group = group(previous);
            long ordinal = previous == null ? put(group, encoded) : previous.ordinal();
            // Admission may spill the prior version. Keep it unchanged until replacement succeeds.
            if (!groups.spilled()) group = group.copyForUpdate();
            attachCollections(group, ordinal);
            for (int index = 0; index < folds.size(); index++) {
                State state = group.states.get(index); Fold fold = state.fold;
                if (!passes(fold.filter, meter)) continue;
                meter.charge(Costs.AGGREGATE_ACCUMULATE); meter.guard();
                SqlValue value = fold.input == null ? null : fold.input.run(meter);
                if (value != null && value.isNull() && !collectionFold(fold.name) && !(fold.orderedSet != null && fold.orderedSet.hypothetical)) continue;
                List<SqlValue> order = new ArrayList<>();
                if (fold.orderedSet != null) {
                    order.addAll(fold.orderedSet.sortedRow(value, meter));
                } else for (ScalarPlan.Expression key : fold.orderKeys) { order.add(sortKey(key, key.run(meter), meter)); }
                if (fold.distinct) {
                    if (value == null) throw new SqlException("42601", "DISTINCT star is invalid");
                    chargeKeys(List.of(value), Costs.HASH_KEY, meter);
                    List<SqlValue> distinctKey = List.of(SqlValue.ofInteger(ordinal), SqlValue.ofInteger(index), value);
                    if (!distinctInputs.add(distinctKey, DistinctState.entryBytes(scratch != null && workMem != 0 ? distinctKey : List.of(value)))) continue;
                    if (scratch == null || workMem == 0) distinctGroupInputs.computeIfAbsent(ordinal, ignored -> new ArrayList<>()).add(distinctKey);
                }
                state.add(value, order, meter);
            }
            put(group, encoded);
            if (columnarCharges != null && (columnarRejected || columnarHeld > workMem)) handOverGroups();
        }
    }
    static SqlValue sortKey(ScalarPlan.Expression expression, SqlValue value, CostMeter meter) {
        SqlValue result = CollationDerivation.sortValue(expression, value, meter);
        if (CollationDerivation.sortType(expression).equals(expression.type())) { meter.charge(MemoryAccount.payload(value) * Costs.SORT_KEY); meter.guard(); }
        return result;
    }
    private static void chargeKeys(List<SqlValue> values, long unit, CostMeter meter) { for (SqlValue value : values) meter.charge(MemoryAccount.payload(value) * unit); meter.guard(); }
    private long put(Group group, byte[] key) {
        List<SqlValue> active = new ArrayList<>(); for (int position : groupingSets.get(group.set)) active.add(group.keys.get(position));
        long bytes = DistinctState.entryBytes(active);
        if (scratch != null && workMem != 0) {
            bytes = MemoryAccount.add(bytes, MemorySchedule.VALUE);
            for (State state : group.states) bytes = MemoryAccount.add(bytes, MemoryAccount.add(MemorySchedule.VALUE, state.packedBytes()));
        }
        if (scratch == null || workMem == 0) for (State state : group.states) bytes = MemoryAccount.add(bytes, state.retainedBytes());
        if (columnarCharges != null) {
            GroupKey lookup = new GroupKey(key); long previous = columnarCharges.getOrDefault(lookup, 0L), change = bytes - previous;
            if (change < 0) { groupAccount.release(-change); columnarHeld += change; columnarCharges.put(lookup, bytes); }
            else if (change == 0 || groupAccount.tryReserve(change)) { columnarHeld += change; columnarCharges.put(lookup, bytes); }
            else columnarRejected = true;
        }
        return groups.putResident(key, group, bytes);
    }
    private void handOverGroups() {
        SpillKeyState previous = groups;
        groupAccount.release(columnarHeld); columnarHeld = 0; columnarCharges = null; columnarRejected = false;
        groups = new SpillKeyState(scratch, workMem, groupAccount);
        groupKeyRows = new RowSpool(keys.stream().map(ScalarPlan.Expression::type).toList(), workMem, scratch, groupAccount);
        try (SpillKeyState.Cursor cursor = previous.cursor()) {
            SpillKeyState.Entry entry;
            while ((entry = cursor.next()) != null) {
                Group group = group(entry.value()); put(group, entry.key()); groupKeyRows.append(group.keys);
            }
        } finally { previous.close(); }
    }
    private void attachCollections(Group group, long ordinal) {
        for (int i = 0; i < group.states.size(); i++) {
            State state = group.states.get(i); int fold = i;
            if (!state.fold.order.isEmpty()) state.ordered = new State.Ordered() {
                public void add(long index, List<SqlValue> row) { collections.put(ordinal, fold, index, row, state.fold.orderedSet == null || state.fold.orderedSet.hypothetical); }
                public List<SqlValue> get(long index) { return collections.get(ordinal, fold, index); }
            };
            else if (collectionFold(state.fold.name)) state.collection = new State.Collection() {
                public void add(long index, SqlValue value, long bytes) { collections.put(ordinal, fold, index, List.of(value), false, bytes, collections.bounded() ? state.collectionPayload(value) : 0); }
                public SqlValue get(long index) { return collections.get(ordinal, fold, index).getFirst(); }
                public boolean unique(String key) { return collections.unique(ordinal, fold, key); }
            };
        }
    }
    private SqlValue finish(State state, long ordinal, int foldIndex, CostMeter meter) {
        if (state.fold.order.isEmpty()) return state.finish();
        List<SqlType> types = new ArrayList<>(); if (state.fold.orderedSet != null) types.addAll(state.fold.orderedSet.storedTypes()); else { for (ScalarPlan.Expression key : state.fold.orderKeys) types.add(CollationDerivation.sortType(key)); types.add(state.fold.input.type()); }
        try (ExternalSorter sorter = new ExternalSorter(types, workMem, scratch, scratch == null || workMem == 0 ? null : meter.memory(), (a, b) -> {
            for (int i = 0; i < state.fold.order.size(); i++) {
                SqlValue x = a.get(i), y = b.get(i); SqlParser.Order order = state.fold.order.get(i);
                boolean first = order.nullsFirst() == null ? order.descending() : order.nullsFirst();
                int compared = x.isNull() || y.isNull() ? x.isNull() == y.isNull() ? 0 : x.isNull() == first ? -1 : 1 : ScalarPlan.compare(x, y, null) * (order.descending() ? -1 : 1);
                if (compared != 0) return compared;
            }
            return 0;
        })) {
            for (long i = 0; i < state.count; i++) sorter.append(state.ordered.get(i));
            if (state.fold.orderedSet != null) {
                try (RowSpool.Cursor cursor = sorter.finish()) { return state.fold.orderedSet.finish(cursor, state.count, meter); }
            }
            State folded = new State(state.fold.unordered());
            if (collectionFold(folded.fold.name)) folded.collection = new State.Collection() {
                public void add(long index, SqlValue value, long bytes) { collections.put(ordinal, foldIndex ^ Integer.MIN_VALUE, index, List.of(value), false, bytes, collections.bounded() ? folded.collectionPayload(value) : 0); }
                public SqlValue get(long index) { return collections.get(ordinal, foldIndex ^ Integer.MIN_VALUE, index).getFirst(); }
                public boolean unique(String key) { return collections.unique(ordinal, foldIndex ^ Integer.MIN_VALUE, key); }
            };
            try (RowSpool.Cursor cursor = sorter.finish()) { List<SqlValue> row; while ((row = cursor.next()) != null) folded.add(row.getLast(), meter); }
            SqlValue result = folded.finish();
            if (folded.collection != null) collections.discard(ordinal, foldIndex ^ Integer.MIN_VALUE, folded.count, folded.fold.name.contains("_unique"));
            return result;
        }
    }
    private static byte[] encode(Group group) {
        try {
            ByteArrayOutputStream bytes = new ByteArrayOutputStream(); DataOutputStream out = new DataOutputStream(bytes);
            out.writeInt(group.set); SpillRows.write(out, group.keys); for (State state : group.states) state.write(out); out.flush(); return bytes.toByteArray();
        } catch (IOException error) { throw SpillRows.io(error); }
    }
    private Group group(SpillKeyState.Value value) {
        return value.resident() instanceof Group group ? group : decode(value.bytes());
    }
    private Group decode(byte[] bytes) {
        try {
            DataInputStream input = new DataInputStream(new ByteArrayInputStream(bytes));
            int set = input.readInt(); List<SqlValue> values = SpillRows.read(input, keys.size()); List<State> states = fresh();
            for (State state : states) state.read(input);
            return new Group(values, states, set);
        } catch (IOException error) { throw SpillRows.io(error); }
    }
    @Override public void close() {
        if (closed) return; closed = true;
        if (source != null) source.close();
        if (columnarHeld != 0) { groupAccount.release(columnarHeld); columnarHeld = 0; }
        if (output != null) output.close(); if (sorter != null) sorter.close(); if (results != null) results.close();
        distinctGroupInputs.clear(); if (scannedInput != null) scannedInput.close(); if (groupKeyRows != null) groupKeyRows.close(); if (retainedInput != null) retainedInput.close(); if (materializedSource != null) materializedSource.close();
        if (groups != null) groups.close(); if (collections != null) collections.close(); if (distinctInputs != null) distinctInputs.close(); if (distinctOutputs != null) distinctOutputs.close();
    }
    static final class State {
        final Fold fold;
        interface Collection { void add(long index, SqlValue value, long bytes); SqlValue get(long index); boolean unique(String key); }
        interface Ordered { void add(long index, List<SqlValue> row); List<SqlValue> get(long index); }
        Ordered ordered;
        Collection collection;
        final List<SqlValue> retained = new ArrayList<>();
        final java.util.Set<String> uniqueKeys = new java.util.HashSet<>();
        private SqlValue retained(long index) { return collection == null ? retained.get(Math.toIntExact(index)) : collection.get(index); }
        long count, integer, retainedLogicalBytes;
        Decimal sum = Decimal.zero(0);
        SqlValue extremum;
        boolean truth, nan, positiveInfinity, negativeInfinity, collectionSeen;
        double floating;
        final AggregateStatistics statistics;
        State(Fold fold) { this.fold = fold; statistics = AggregateStatistics.accepts(fold.name) ? new AggregateStatistics(fold.name, fold.type) : null; }
        State(State original) {
            fold = original.fold;
            statistics = original.statistics == null ? null : new AggregateStatistics(original.statistics);
            count = original.count; integer = original.integer; retainedLogicalBytes = original.retainedLogicalBytes;
            sum = original.sum; extremum = original.extremum; truth = original.truth; floating = original.floating;
            nan = original.nan; positiveInfinity = original.positiveInfinity; negativeInfinity = original.negativeInfinity;
            collectionSeen = original.collectionSeen;
            retained.addAll(original.retained); uniqueKeys.addAll(original.uniqueKeys);
            // Collection handles are attached to this copy before folding; their storage is separately owned.
        }
        private void retain(SqlValue image, long bytes) {
            if (collection == null) { retained.add(image); retainedLogicalBytes = MemoryAccount.add(retainedLogicalBytes, bytes); }
            else collection.add(count - 1, image, bytes);
        }
        void add(SqlValue value, CostMeter meter) { add(value, List.of(), meter); }
        void add(SqlValue value, List<SqlValue> keys, CostMeter meter) {
            if (AggregateStatistics.binary(fold.name) && value.asComposite().stream().anyMatch(SqlValue::isNull)) return;
            if (fold.name.equals("string_agg") && value.asComposite().getFirst().isNull()) return;
            try { count = Math.incrementExact(count); }
            catch (ArithmeticException error) { throw SqlException.numericOverflow(); }
            if (!fold.order.isEmpty()) { List<SqlValue> row = new ArrayList<>(keys); if (fold.orderedSet == null) row.add(value); ordered.add(count - 1, row); return; }
            if (statistics != null) { statistics.add(value, count, meter); return; }
            long inputBytes = value == null ? 0 : objectFold(fold.name) || fold.name.equals("string_agg") ? MemoryAccount.payload(value) : MemoryAccount.value(value);
            if (collectionFold(fold.name)) collectionSeen = true;
            switch (fold.name) {
                case "count" -> { }
                case "string_agg" -> { retain(value, inputBytes); }
                case "array_agg" -> {
                    if (fold.input.type().kind() == SqlType.Kind.ARRAY) {
                        if (value.isNull()) throw new SqlException("22004", "cannot accumulate null arrays");
                        SqlArray array = value.asArray();
                        if (count == 1) {
                            if (array.dimensions() == 0) throw new SqlException("2202E", "cannot accumulate empty arrays");
                            if (array.dimensions() >= 6) throw new SqlException("54000", "too many array dimensions");
                        } else {
                            SqlArray first = retained(0).asArray();
                            if (!java.util.Arrays.equals(first.dims(), array.dims()) || !java.util.Arrays.equals(first.lowerBounds(), array.lowerBounds())) throw new SqlException("2202E", "cannot accumulate arrays of different dimensionality");
                        }
                    }
                    retain(value, inputBytes);
                }
                case "json_object_agg", "jsonb_object_agg", "json_object_agg_strict", "jsonb_object_agg_strict", "json_object_agg_unique", "jsonb_object_agg_unique", "json_object_agg_unique_strict", "jsonb_object_agg_unique_strict" -> {
                    String key = JsonImages.key(value.asComposite().get(0), meter, fold.type == SqlType.JSONB); SqlValue input = value.asComposite().get(1);
                    if (fold.name.contains("_unique") && !(collection == null ? uniqueKeys.add(key) : collection.unique(key))) throw new SqlException("22030", "duplicate JSON object key");
                    if (input.isNull() && fold.name.endsWith("_strict")) { count--; return; }
                    SqlType tuple = SqlType.record(List.of(new SqlType.Field("key", SqlType.TEXT), new SqlType.Field("value", fold.type)));
                    meter.charge(Costs.GENERATED_ROW); meter.guard();
                    SqlValue image = SqlValue.ofComposite(tuple, List.of(SqlValue.ofText(key), fold.type == SqlType.JSONB ? SqlValue.ofJsonb(JsonImages.node(input, meter)) : SqlValue.ofJson(JsonImages.text(input, meter, false))));
                    retain(image, inputBytes);
                }
                case "json_agg", "jsonb_agg", "json_agg_strict", "jsonb_agg_strict" -> {
                    if (value.isNull() && fold.name.endsWith("_strict")) { count--; return; }
                    meter.charge(Costs.GENERATED_ROW); meter.guard();
                    SqlValue image = fold.type == SqlType.JSONB ? SqlValue.ofJsonb(JsonImages.node(value, meter)) : SqlValue.ofJson(JsonImages.text(value, meter, false));
                    retain(image, inputBytes);
                }
                case "sum", "avg" -> {
                    if (fold.type.isFloat()) {
                        double number = value.asFloat();
                        if (Double.isNaN(number)) nan = true;
                        else if (number == Double.POSITIVE_INFINITY) positiveInfinity = true;
                        else if (number == Double.NEGATIVE_INFINITY) negativeInfinity = true;
                        else { floating += number == 0 ? 0 : number; if (fold.type == SqlType.F32) floating = (float) floating; }
                    } else if (fold.type == SqlType.I64) {
                        try { integer = Math.addExact(integer, value.asInteger()); }
                        catch (ArithmeticException error) { throw SqlException.numericOverflow(); }
                    } else {
                        Decimal number = value.type().isInteger() ? Decimal.of(value.asInteger()) : value.asDecimal();
                        int scale = Math.max(sum.scale(), number.scale());
                        long work = Math.max(sum.groupsAtScale(scale), number.groupsAtScale(scale));
                        meter.charge(Math.max(0, work - 1) * Costs.DECIMAL_WORK); meter.guard();
                        sum = sum.addUncapped(number);
                    }
                }
                case "min", "max" -> {
                    if (extremum == null || (fold.name.equals("min") ? CollationDerivation.compare(value, extremum, null, meter.environment().collation(fold.input.collation().require())) < 0 : CollationDerivation.compare(value, extremum, null, meter.environment().collation(fold.input.collation().require())) > 0)) extremum = value;
                }
                case "bool_and", "every" -> truth = count == 1 ? value.asBoolean() : truth && value.asBoolean();
                case "bool_or" -> truth |= value.asBoolean();
                case "bit_and" -> integer = count == 1 ? value.asInteger() : integer & value.asInteger();
                case "bit_or" -> integer |= value.asInteger();
                case "bit_xor" -> integer ^= value.asInteger();
                default -> throw new IllegalStateException("unresolved fold");
            }
        }
        void write(DataOutputStream out) throws IOException {
            out.writeLong(count); if (statistics != null) statistics.write(out); out.writeLong(integer); sum.writeState(out);
            SpillRows.write(out, List.of(extremum == null ? SqlValue.nullValue(fold.type) : extremum));
            out.writeBoolean(truth); out.writeDouble(floating); out.writeBoolean(nan); out.writeBoolean(positiveInfinity); out.writeBoolean(negativeInfinity); out.writeBoolean(collectionSeen);
            if (collectionFold(fold.name)) { out.writeInt(collection == null ? retained.size() : -1); if (collection == null) for (SqlValue value : retained) SpillRows.write(out, List.of(value)); }
        }
        void read(DataInputStream in) throws IOException {
            count = in.readLong(); if (statistics != null) statistics.read(in); integer = in.readLong(); sum = Decimal.readState(in);
            SqlValue value = SpillRows.read(in, 1).getFirst(); extremum = value.isNull() ? null : value;
            truth = in.readBoolean(); floating = in.readDouble(); nan = in.readBoolean(); positiveInfinity = in.readBoolean(); negativeInfinity = in.readBoolean(); collectionSeen = in.readBoolean();
            if (collectionFold(fold.name)) { int size = in.readInt(); if (size < -1) throw new IOException("invalid aggregate state"); for (int i = 0; i < size; i++) retained.add(SpillRows.read(in, 1).getFirst()); }
        }
        long retainedBytes() { if (collectionFold(fold.name)) return retainedLogicalBytes; long bytes = 0; for (SqlValue value : retained) bytes = MemoryAccount.add(bytes, MemoryAccount.value(value)); return bytes; }
        long collectionPayload(SqlValue image) {
            if (fold.name.equals("string_agg")) return MemoryAccount.payload(image);
            if (objectFold(fold.name)) return MemoryAccount.add(MemoryAccount.value(image.asComposite().getFirst()), jsonInputBytes(image.asComposite().get(1)));
            if (fold.name.startsWith("json")) return jsonInputBytes(image);
            return MemoryAccount.value(image);
        }
        private static long jsonInputBytes(SqlValue image) {
            return !image.isNull() && image.type() == SqlType.JSON ? JsonCodec.textMemory(image.asJson()) : MemoryAccount.value(image);
        }
        long packedBytes() {
            if (statistics != null) return statistics.packedBytes();
            if (!fold.order.isEmpty()) return 0;
            if (collectionFold(fold.name)) {
                long slots = 1; // seen
                if (fold.name.equals("array_agg") && fold.input.type().kind() == SqlType.Kind.ARRAY)
                    slots += 1 + (count == 0 ? 0 : 2L * retained(0).asArray().dimensions());
                return slots * MemorySchedule.VALUE;
            }
            if (fold.type.isFloat() && (fold.name.equals("sum") || fold.name.equals("avg"))) return 5 * MemorySchedule.VALUE;
            return switch (fold.name) {
                case "count" -> MemorySchedule.VALUE;
                case "sum" -> 2 * MemorySchedule.VALUE + (fold.type == SqlType.I64 ? 0 : decimalBytes(sum));
                case "avg" -> 2 * MemorySchedule.VALUE + decimalBytes(sum);
                default -> MemoryAccount.value(finish());
            };
        }
        private static long decimalBytes(Decimal value) { return value.isZero() ? 0 : ((value.precision() + 3L) / 4) * MemorySchedule.DECIMAL_GROUP; }
        SqlValue finish() {
            if (statistics != null) return statistics.finish(count);
            if (fold.name.equals("count")) return SqlValue.ofInteger(count);
            if (count == 0 && !(collectionSeen && fold.name.startsWith("json"))) return SqlValue.nullValue(fold.type);
            if (fold.type.isFloat() && (fold.name.equals("sum") || fold.name.equals("avg"))) {
                double number;
                if (nan || positiveInfinity && negativeInfinity) number = Double.NaN;
                else if (positiveInfinity) number = Double.POSITIVE_INFINITY;
                else if (negativeInfinity) number = Double.NEGATIVE_INFINITY;
                else {
                    if (Double.isInfinite(floating)) throw SqlException.numericOverflow();
                    number = fold.name.equals("avg") ? floating / count : floating;
                }
                return SqlValue.ofFloat(fold.type, number);
            }
            return switch (fold.name) {
                case "array_agg" -> {
                    if (fold.input.type().kind() != SqlType.Kind.ARRAY) {
                        List<SqlValue> items = new ArrayList<>(); for (long i = 0; i < count; i++) items.add(retained(i));
                        yield SqlValue.ofArray(fold.type, SqlArray.vector(items));
                    }
                    SqlArray first = retained(0).asArray(); int[] dims = new int[first.dimensions() + 1], lower = new int[dims.length];
                    dims[0] = Math.toIntExact(count); lower[0] = 1; System.arraycopy(first.dims(), 0, dims, 1, first.dimensions()); System.arraycopy(first.lowerBounds(), 0, lower, 1, first.dimensions());
                    List<SqlValue> elements = new ArrayList<>(); for (long i = 0; i < count; i++) elements.addAll(retained(i).asArray().elements());
                    yield SqlValue.ofArray(fold.type, new SqlArray(dims, lower, elements));
                }
                case "jsonb_object_agg", "jsonb_object_agg_strict", "jsonb_object_agg_unique", "jsonb_object_agg_unique_strict" -> {
                    List<JsonNode.Member> members = new ArrayList<>();
                    for (long i = 0; i < count; i++) { SqlValue image = retained(i); if (!image.asComposite().get(1).isNull()) members.add(new JsonNode.Member(image.asComposite().get(0).asText(), image.asComposite().get(1).asJsonb())); }
                    yield SqlValue.ofJsonb(JsonNode.object(members));
                }
                case "json_object_agg", "json_object_agg_strict", "json_object_agg_unique", "json_object_agg_unique_strict" -> {
                    StringBuilder text = new StringBuilder("{ "); boolean first = true;
                    for (long i = 0; i < count; i++) { SqlValue image = retained(i); if (image.asComposite().get(1).isNull()) continue; if (!first) text.append(", "); first = false; JsonNode.quote(text, image.asComposite().get(0).asText()); text.append(" : ").append(image.asComposite().get(1).asJson()); }
                    yield SqlValue.ofJson(text.append(" }").toString());
                }
                case "jsonb_agg", "jsonb_agg_strict" -> {
                    List<JsonNode> items = new ArrayList<>(); for (long i = 0; i < count; i++) { SqlValue image = retained(i); if (!image.isNull()) items.add(image.asJsonb()); }
                    yield SqlValue.ofJsonb(JsonNode.array(items));
                }
                case "json_agg", "json_agg_strict" -> {
                    StringBuilder text = new StringBuilder("["); boolean first = true;
                    boolean multiline = fold.input.type().kind() == SqlType.Kind.ARRAY || fold.input.type().isComposite();
                    for (long i = 0; i < count; i++) { SqlValue image = retained(i); if (image.isNull()) continue; if (!first) text.append(multiline && !image.asJson().equals("null") ? ", \n " : ", "); first = false; text.append(image.asJson()); }
                    yield SqlValue.ofJson(text.append(']').toString());
                }
                case "string_agg" -> {
                    if (fold.type == SqlType.TEXT) {
                        StringBuilder text = new StringBuilder();
                        for (long i = 0; i < count; i++) { var pair = retained(i).asComposite(); if (i != 0 && !pair.get(1).isNull()) text.append(pair.get(1).asText()); text.append(pair.get(0).asText()); }
                        yield SqlValue.ofText(text.toString());
                    }
                    ByteArrayOutputStream bytes = new ByteArrayOutputStream();
                    for (long i = 0; i < count; i++) { var pair = retained(i).asComposite(); if (i != 0 && !pair.get(1).isNull()) bytes.writeBytes(pair.get(1).asBytes()); bytes.writeBytes(pair.get(0).asBytes()); }
                    yield SqlValue.ofBytes(bytes.toByteArray());
                }
                case "sum" -> fold.type == SqlType.I64 ? SqlValue.ofInteger(integer) : SqlValue.ofDecimal(sum.checked());
                case "avg" -> SqlValue.ofDecimal(sum.divide(Decimal.of(count)));
                case "min", "max" -> extremum.relabel(fold.type);
                case "bool_and", "bool_or", "every" -> SqlValue.ofBoolean(truth);
                case "bit_and", "bit_or", "bit_xor" -> SqlValue.ofInteger(fold.type, integer);
                default -> throw new IllegalStateException("unresolved fold");
            };
        }
    }
    private static ScalarPlan.Expression filterExpression(ScalarPlan.Resolver resolver, SqlParser.Expr expression) {
        if (expression instanceof SqlParser.Binary binary && binary.op().equals("and")) return resolver.binary(binary, true);
        return booleanExpression(resolver, expression);
    }
    private static ScalarPlan.Expression booleanExpression(ScalarPlan.Resolver resolver, SqlParser.Expr expression) {
        if (expression == null) return null;
        ScalarPlan.Expression result = resolver.resolve(expression, SqlType.BOOLEAN);
        if (result.type() != SqlType.BOOLEAN) throw SqlException.typeMismatch("predicate requires boolean"); return result;
    }
    private static boolean passes(ScalarPlan.Expression expression, CostMeter meter) { if (expression == null) return true; SqlValue value = expression.run(meter); return !value.isNull() && value.asBoolean(); }
    private static long bound(ScalarPlan.Expression expression, CostMeter meter, long fallback, String state) {
        if (expression == null) return fallback; SqlValue value = expression.run(meter); if (value.isNull()) return fallback;
        long number = value.asInteger(); if (number < 0) throw new SqlException(state, "negative row bound"); return number;
    }
}
