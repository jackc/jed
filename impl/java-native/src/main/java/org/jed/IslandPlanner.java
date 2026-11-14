package org.jed;

import java.nio.file.Path;
import java.util.ArrayList;
import java.util.BitSet;
import java.util.List;
import java.util.function.Function;

/** Bounded left-deep search over an all-base INNER/CROSS island. No candidate executes SQL. */
final class IslandPlanner {
    private record Clause(int owner, BitSet dependencies, SqlParser.Expr expression) {}
    private record Step(int relation, SqlParser.Join join, TableRelation.Probe probe, AccessCandidate access, int algorithm) {}
    private record State(BitSet mask, List<Integer> order, List<AccessCandidate> access, List<Step> steps, List<ScalarPlan.Binding> bindings,
                         long cost, long rows, long logicalRows, boolean ordered) {}
    private final List<TableRelation> tables;
    private final List<SqlParser.From> sources;
    private final RelationPlan fixed;
    private final JoinedStatistics statistics;
    private final List<Clause> clauses;
    private final java.util.IdentityHashMap<SqlParser.Expr, BitSet> dependencyCache = new java.util.IdentityHashMap<>();
    private final List<ScalarPlan.Binding> logical, raw;
    private final List<Integer> origins;
    private final List<SqlParser.Expr> pushed;
    private final SqlParser.Expr residual, access;
    private final SqlValue[] args;
    private final QueryEnvironment environment;
    private final Path scratch;
    private final long workMem;
    private final SqlParser.Select query;
    private final boolean contradiction;
    private List<SqlParser.Order> order = List.of();
    private long take = Long.MAX_VALUE, skip;

    static RelationPlan plan(SqlParser.From syntax, SqlParser.Expr filter, SqlParser.Select query, Function<SqlParser.From, RelationPlan> resolve,
                             SqlValue[] args, QueryEnvironment environment, Path scratch, long workMem) {
        List<SqlParser.Table> ordinary = new ArrayList<>();
        if (flatten(syntax, ordinary) && ordinary.size() >= 2) {
            List<TableRelation> tables = new ArrayList<>();
            try {
                boolean base = true;
                for (SqlParser.Table source : ordinary) {
                    RelationPlan relation = resolve.apply(source);
                    if (!(relation instanceof TableRelation table)) { relation.close(); base = false; break; }
                    tables.add(table);
                }
                if (base) return create(syntax, tables, new ArrayList<>(ordinary), null, filter, query, args, environment, scratch, workMem);
            } catch (RuntimeException | Error failure) { for (var table : tables) table.close(); throw failure; }
            for (var table : tables) table.close();
        }
        // Walk backward only over movable base inputs. The prefix, including the last fence,
        // is one fixed driver and can never trade places with a suffix relation.
        List<TableRelation> suffix = new ArrayList<>(); List<SqlParser.From> sources = new ArrayList<>();
        SqlParser.From prefix = syntax;
        try {
            while (prefix instanceof SqlParser.Join join && List.of("inner", "cross").contains(join.kind()) && join.right() instanceof SqlParser.Table) {
                RelationPlan relation = resolve.apply(join.right());
                if (!(relation instanceof TableRelation table)) { relation.close(); break; }
                suffix.addFirst(table); sources.addFirst(join.right()); prefix = join.left();
            }
            if (suffix.size() < 2) {
                for (var table : suffix) table.close(); return null;
            }
            RelationPlan fixed = resolve.apply(prefix);
            suffix.addFirst(null); sources.addFirst(prefix);
            try { return create(syntax, suffix, sources, fixed, filter, query, args, environment, scratch, workMem); }
            catch (RuntimeException | Error failure) { fixed.close(); throw failure; }
        } catch (RuntimeException | Error failure) { for (var table : suffix) if (table != null) table.close(); throw failure; }
    }
    private static RelationPlan create(SqlParser.From syntax, List<TableRelation> tables, List<SqlParser.From> sources, RelationPlan fixed,
                                       SqlParser.Expr filter, SqlParser.Select query, SqlValue[] args, QueryEnvironment environment, Path scratch, long workMem) {
        if (query != null) {
            SqlParser.From authored = query.from().getFirst();
            for (int i = 1; i < query.from().size(); i++) authored = new SqlParser.Join(authored, query.from().get(i), "cross", null, List.of(), false);
            if (!authored.equals(syntax)) { query = null; filter = null; }
        }
        boolean prior = environment.estimateCapture(); environment.estimateCapture(true);
        try { return new IslandPlanner(syntax, tables, sources, fixed, filter, query, args, environment, scratch, workMem).choose(); }
        finally { environment.estimateCapture(prior); }
    }
    private static boolean flatten(SqlParser.From from, List<SqlParser.Table> sources) {
        if (from instanceof SqlParser.Table table) { sources.add(table); return true; }
        if (!(from instanceof SqlParser.Join join) || !List.of("inner", "cross").contains(join.kind())) return false;
        if (!flatten(join.left(), sources)) return false;
        if (join.right() instanceof SqlParser.Table right) { sources.add(right); return true; }
        return join.kind().equals("cross") && join.on() == null && !join.natural() && join.using().isEmpty() && flatten(join.right(), sources);
    }
    private static BitSet with(BitSet before, int relation) { BitSet result = (BitSet)before.clone(); result.set(relation); return result; }
    private static boolean contains(BitSet available, BitSet required) { BitSet missing = (BitSet)required.clone(); missing.andNot(available); return missing.isEmpty(); }
    private IslandPlanner(SqlParser.From syntax, List<TableRelation> tables, List<SqlParser.From> sources, RelationPlan fixed,
                          SqlParser.Expr filter, SqlParser.Select query, SqlValue[] args, QueryEnvironment environment, Path scratch, long workMem) {
        contradiction = query != null && environment.contradiction(query.where());
        this.query = query; this.fixed = fixed; this.tables = tables; this.statistics = new JoinedStatistics(tables); this.sources = sources; this.args = args; this.environment = environment; this.scratch = scratch; this.workMem = workMem;
        raw = new ArrayList<>();
        java.util.Set<List<String>> qualifiers = new java.util.HashSet<>();
        for (int i = 0; i < tables.size(); i++) {
            var bindings = input(i).bindings(); var local = new java.util.HashSet<List<String>>();
            for (var binding : bindings) if (!binding.qualifier().isEmpty()) local.add(binding.qualifier());
            for (var qualifier : local) if (!qualifiers.add(qualifier)) throw new SqlException("42712", "duplicate relation name");
            raw.addAll(bindings);
        }
        clauses = new ArrayList<>();
        Namespace namespace = namespace(syntax, new int[]{0, 0});
        logical = List.copyOf(namespace.bindings()); origins = List.copyOf(namespace.origins()); pushed = new ArrayList<>();
        if (filter != null) environment.resolver(logical, new ScalarPlan.Frame()).predicate(filter);
        SqlParser.Expr rest = null, bounds = null;
        for (SqlParser.Expr authored : FilterRules.terms(filter)) {
            SqlParser.Expr term = qualify(authored, logical, origins, raw); boolean moved = false;
            for (int i = 0; i < tables.size(); i++) if (!hasSlot(term) && FilterRules.belongs(term, input(i).bindings())) {
                if (input(i).pushInJoin(term)) moved = true;
                else if (tables.get(i) != null && FilterRules.accessHint(term)) tables.get(i).accessWhere(term);
                break;
            }
            if (moved) pushed.add(authored); else rest = FilterRules.and(rest, term);
            if (FilterRules.accessHint(term)) bounds = FilterRules.and(bounds, term);
        }
        residual = rest; access = bounds;
        if (query != null && AggregatePlan.needed(query)) {
            List<RelationPlan.FunctionalKey> dependencies = new ArrayList<>(); int base = 0;
            for (int i = 0; i < tables.size(); i++) {
                final int offset = base;
                for (RelationPlan.FunctionalKey key : input(i).functionalKeys()) dependencies.add(new RelationPlan.FunctionalKey(
                    key.determinant().stream().map(slot -> origins.lastIndexOf(offset + slot)).toList(),
                    key.dependent().stream().map(slot -> origins.lastIndexOf(offset + slot)).toList()));
                base += input(i).bindings().size();
            }
            AggregatePlan.captureEstimates(query, logical, dependencies, args, environment);
        }
        if (query != null && !AggregatePlan.needed(query)) {
            ScalarPlan.Resolver scope = environment.resolver(logical, new ScalarPlan.Frame());
            for (SqlParser.Item item : query.items()) captureHidden(item.expression(), scope);
            for (SqlParser.Order key : query.order()) captureHidden(key.expression(), scope);
        }
        if (fixed == null && query != null && query.limit() != null && !query.distinct() && !AggregatePlan.needed(query) && !WindowSyntax.needed(query)) {
            take = TableExplanation.boundEstimate(query.limit(), Long.MAX_VALUE, environment.resolver(), environment.meter());
            skip = TableExplanation.boundEstimate(query.offset(), 0, environment.resolver(), environment.meter());
            List<SqlParser.Order> terms = new ArrayList<>();
            for (SqlParser.Order term : query.order()) {
                SqlParser.Expr expression = term.expression();
                if (expression instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.INTEGER) {
                    int position = ScalarPlan.ordinal(literal.text());
                    if (position >= 1 && position <= query.items().size()) expression = query.items().get(position - 1).expression();
                } else if (expression instanceof SqlParser.Name name && name.parts().size() == 1) {
                    for (SqlParser.Item item : query.items()) if (name.parts().getFirst().equals(item.alias())) { expression = item.expression(); break; }
                }
                terms.add(new SqlParser.Order(qualify(expression, logical, origins, raw), term.descending(), term.nullsFirst()));
            }
            order = List.copyOf(terms);
        }
    }
    private static void captureHidden(SqlParser.Expr expression, ScalarPlan.Resolver scope) {
        if (expression == null) return;
        if (expression instanceof SqlParser.Subquery || expression instanceof SqlParser.Exists || expression instanceof SqlParser.In in && in.query() != null || expression instanceof SqlParser.Quantified q && q.query() != null) {
            scope.resolve(expression, null); return;
        }
        for (SqlParser.Expr child : DefaultExpression.children(expression)) captureHidden(child, scope);
    }
    private record Namespace(List<ScalarPlan.Binding> bindings, List<Integer> origins, int first, int last) {}
    /** Resolve the authored namespace before flattening physical order, including comma-item scopes. */
    private Namespace namespace(SqlParser.From source, int[] position) {
        if (source == sources.getFirst() && fixed != null || source instanceof SqlParser.Table) {
            int ordinal = position[0]++; var bindings = input(ordinal).bindings(); List<Integer> slots = new ArrayList<>();
            for (int c = 0; c < bindings.size(); c++) slots.add(position[1]++);
            return new Namespace(bindings, slots, ordinal, ordinal);
        }
        SqlParser.Join join = (SqlParser.Join)source;
        Namespace left = namespace(join.left(), position), right = namespace(join.right(), position);
        List<ScalarPlan.Binding> scope = new ArrayList<>(left.bindings()); scope.addAll(right.bindings());
        List<Integer> slots = new ArrayList<>(left.origins()); slots.addAll(right.origins());
        SqlParser.Expr on = join.on();
        if (on != null) environment.resolver(scope, new ScalarPlan.Frame()).resolve(on, SqlType.BOOLEAN);
        on = qualify(on, scope, slots, raw);
        List<String> using = join.natural() ? left.bindings().stream().filter(binding -> !binding.qualifiedOnly() && right.bindings().stream().anyMatch(other -> !other.qualifiedOnly() && binding.name().equals(other.name()))).map(ScalarPlan.Binding::name).distinct().toList() : join.using();
        for (String name : using) {
            SqlParser.Expr a = qualify(new SqlParser.ColumnSlot(named(left.bindings(), name)), scope, slots, raw);
            SqlParser.Expr b = qualify(new SqlParser.ColumnSlot(left.bindings().size() + named(right.bindings(), name)), scope, slots, raw);
            on = FilterRules.and(on, new SqlParser.Binary("=", a, b));
        }
        SqlParser.Expr remaining = null;
        for (SqlParser.Expr term : FilterRules.terms(on)) {
            boolean moved = false;
            for (int i = 0; i < tables.size(); i++) if (!hasSlot(term) && FilterRules.belongs(term, input(i).bindings()) && input(i).pushInJoin(term)) { moved = true; break; }
            if (!moved) remaining = FilterRules.and(remaining, term);
        }
        if (remaining != null || join.kind().equals("inner")) clauses.add(new Clause(right.last(), with(dependencies(remaining), right.last()), remaining));
        List<ScalarPlan.Binding> output = JoinPlan.outputBindings(join, left.bindings(), right.bindings());
        int merged = output.size() - scope.size(); List<Integer> outputSlots = new ArrayList<>();
        for (int c = 0; c < merged; c++) outputSlots.add(left.origins().get(named(left.bindings(), output.get(c).name())));
        outputSlots.addAll(slots);
        return new Namespace(output, outputSlots, left.first(), right.last());
    }
    private BitSet dependencies(SqlParser.Expr expression) {
        if (expression == null) return new BitSet();
        BitSet cached = dependencyCache.get(expression); if (cached != null) return (BitSet)cached.clone();
        ScalarPlan.Resolver resolver = environment.resolver(raw, new ScalarPlan.Frame());
        resolver.resolve(expression, SqlType.BOOLEAN);
        boolean[] touched = resolver.touched(); BitSet result = new BitSet(); int offset = 0;
        for (int i = 0; i < tables.size(); i++) for (int c = 0; c < input(i).bindings().size(); c++) if (touched[offset++]) result.set(i);
        dependencyCache.put(expression, (BitSet)result.clone()); return result;
    }
    private static boolean hasSlot(SqlParser.Expr expression) {
        if (expression == null) return false;
        if (expression instanceof SqlParser.ColumnSlot) return true;
        for (SqlParser.Expr child : DefaultExpression.children(expression)) if (hasSlot(child)) return true;
        return false;
    }
    private static int slot(List<ScalarPlan.Binding> bindings, SqlParser.Name name) {
        int found = -1;
        for (int i = 0; i < bindings.size(); i++) {
            var binding = bindings.get(i);
            if (binding.name().equals(name.parts().getLast()) && (name.parts().size() == 1 ? !binding.qualifiedOnly() : binding.qualifier().equals(name.parts().subList(0, name.parts().size() - 1)))) {
                if (found >= 0) throw new SqlException("42702", "ambiguous column: " + name.parts().getLast());
                found = i;
            }
        }
        return found;
    }
    private static int named(List<ScalarPlan.Binding> bindings, String name) { int index = slot(bindings, new SqlParser.Name(List.of(name))); if (index < 0) throw new SqlException("42703", "USING column does not exist: " + name); return index; }
    private static SqlParser.Expr qualify(SqlParser.Expr expression, List<ScalarPlan.Binding> scope, List<Integer> origins, List<ScalarPlan.Binding> raw) {
        if (expression == null) return null;
        return WindowSyntax.map(expression, node -> {
            int index = node instanceof SqlParser.Name name ? slot(scope, name) : node instanceof SqlParser.ColumnSlot column ? column.index() : -1;
            if (index < 0) return node;
            var binding = raw.get(origins.get(index));
            if (binding.qualifier().isEmpty()) return new SqlParser.ColumnSlot(origins.get(index));
            List<String> parts = new ArrayList<>(binding.qualifier()); parts.add(binding.name());
            return new SqlParser.Name(parts);
        });
    }
    private RelationPlan choose() {
        State winner;
        if (tables.size() - (fixed == null ? 0 : 1) <= 8) {
            int count = 1 << tables.size(); List<List<State>> frontier = new ArrayList<>(count);
            for (int i = 0; i < count; i++) frontier.add(new ArrayList<>());
            for (int i = 0; i < (fixed == null ? tables.size() : 1); i++) for (AccessCandidate access : options(i)) retain(frontier.get(1 << i), driver(i, access));
            for (int size = 1; size < tables.size(); size++) for (int mask = 1; mask < count; mask++) if (Integer.bitCount(mask) == size) {
                frontier.get(mask).sort(IslandPlanner::structural);
                for (State state : frontier.get(mask)) for (int next = 0; next < tables.size(); next++) if ((mask & 1 << next) == 0)
                    for (State extension : extend(state, next)) {
                        List<State> bucket = frontier.get(mask | 1 << next);
                        // The final step's LIMIT discount is not monotone in its undiscounted cost/rows.
                        if ((mask | 1 << next) == count - 1) bucket.add(extension); else retain(bucket, extension);
                    }
            }
            winner = frontier.getLast().stream().min(this::completeCompare).orElseThrow();
        } else {
            winner = null;
            for (int i = 0; i < (fixed == null ? tables.size() : 1); i++) for (AccessCandidate access : options(i)) { State next = driver(i, access); if (winner == null || compare(next, winner) < 0) winner = next; }
            while (winner.order().size() < tables.size()) {
                State nextWinner = null;
                for (int i = 0; i < tables.size(); i++) if (!winner.mask().get(i)) for (State next : extend(winner, i)) if (nextWinner == null || compare(next, nextWinner) < 0) nextWinner = next;
                winner = nextWinner;
            }
        }
        if (fixed == null) tables.get(winner.order().getFirst()).selectAccess(winner.access().getFirst());
        RelationPlan result = input(winner.order().getFirst());
        BitSet installed = with(new BitSet(), winner.order().getFirst());
        for (Step step : winner.steps()) {
            TableRelation table = tables.get(step.relation());
            installed.set(step.relation());
            if (step.probe() != null) {
                SqlParser.Expr bounds = FilterRules.and(step.join().on(), availableAccess(installed));
                result = new LateralJoinPlan(result, scope -> table.probe(scope, bounds, step.probe()), step.join(), args, environment, scratch, workMem);
            } else { table.selectAccess(step.access()); result = new JoinPlan(result, result.bindings(), table, table.bindings(), step.join(), args, environment, scratch, workMem, step.algorithm() == 1).statistics(statistics); }
        }
        int[] physical = new int[logical.size()]; int[] rawToPhysical = new int[raw.size()]; int offset = 0;
        for (int relation : winner.order()) {
            int start = 0; for (int i = 0; i < relation; i++) start += input(i).bindings().size();
            for (int c = 0; c < input(relation).bindings().size(); c++) rawToPhysical[start + c] = offset++;
        }
        for (int i = 0; i < physical.length; i++) physical[i] = rawToPhysical[origins.get(i)];
        ReorderedRelation remapped = new ReorderedRelation(result, logical, physical, pushed);
        State selected = winner;
        return remapped.orderedEstimate(selected.ordered(), node -> prefixPlan(selected, node));
    }
    private RelationPlan input(int index) { return index == 0 && fixed != null ? fixed : tables.get(index); }
    private List<AccessCandidate> options(int index) { return index == 0 && fixed != null ? List.of(new AccessCandidate(AccessCandidate.FULL, null, null, 0, 0, 0)) : tables.get(index).accessOptions(); }
    private State driver(int relation, AccessCandidate access) {
        var table = tables.get(relation); var node = table == null ? fixed.explain(false) : table.accessEstimate(access, false);
        return new State(with(new BitSet(), relation), List.of(relation), List.of(access), List.of(), input(relation).bindings(), contradiction ? 0 : node.cost(), contradiction ? 0 : node.rows(), contradiction ? 0 : table == null ? node.rows() : table.logicalRows(), table != null && !order.isEmpty() && table.orderedBy(order, false, access));
    }
    private List<State> extend(State state, int relation) {
        BitSet mask = with(state.mask(), relation); SqlParser.Expr on = null; boolean inner = false;
        for (Clause clause : clauses) if (contains(mask, clause.dependencies()) && (state.steps().isEmpty() || !contains(state.mask(), clause.dependencies()))) { on = FilterRules.and(on, clause.expression()); inner = true; }
        var join = new SqlParser.Join(sources.get(state.order().getFirst()), sources.get(relation), inner ? "inner" : "cross", on, List.of(), false);
        TableRelation table = tables.get(relation); var right = table.bindings();
        long operations = PlanEstimate.operations(on); List<State> result = new ArrayList<>();
        long logicalRows = statistics.rows(on, PlanEstimate.mul(state.logicalRows(), table.logicalRows()));
        for (AccessCandidate access : table.accessOptions()) {
            var full = table.accessEstimate(access, false); long pairs = PlanEstimate.mul(state.rows(), full.rows());
            long rows = statistics.rows(on, pairs), cost = PlanEstimate.add(state.cost(), full.cost());
            result.add(extension(state, relation, join, null, access, 2, PlanEstimate.add(cost, onWork(on, pairs)), rows, logicalRows));
            if (JoinPlan.hashEligible(join, state.bindings(), right)) result.add(extension(state, relation, join, null, access, 1, PlanEstimate.add(cost, JoinPlan.hashWork(join, state.bindings(), right, state.rows(), full.rows(), statistics)), rows, logicalRows));
        }
        SqlParser.Expr bounds = FilterRules.and(on, availableAccess(mask));
        for (TableRelation.Probe probe : table.probeOptions(bounds, state.bindings())) {
            var estimate = table.probeEstimate(probe); long pairs = PlanEstimate.mul(state.rows(), estimate.rows());
            int kind = switch (probe.kind()) { case "Index" -> IndexAccess.interval(probe.access()) ? AccessCandidate.INDEX_SET : AccessCandidate.BTREE; case "GiST" -> AccessCandidate.GIST; case "GIN" -> AccessCandidate.GIN; case "PK interval" -> AccessCandidate.PK_SET; default -> AccessCandidate.PK; };
            AccessCandidate access = new AccessCandidate(kind, probe.index(), probe.access(), estimate.rows(), 0, 0);
            result.add(extension(state, relation, join, probe, access, 0, PlanEstimate.add(state.cost(), PlanEstimate.add(PlanEstimate.mul(state.rows(), estimate.cost()), onWork(on, pairs))), pairs, pairs));
        }
        return result;
    }
    private long onWork(SqlParser.Expr expression, long rows) {
        return PlanEstimate.add(PlanEstimate.mul(rows, PlanEstimate.operations(expression)), environment.hiddenEstimate(expression, rows));
    }
    private SqlParser.Expr availableAccess(BitSet mask) {
        SqlParser.Expr result = null;
        for (SqlParser.Expr term : FilterRules.terms(access)) if (contains(mask, dependencies(term))) result = FilterRules.and(result, term);
        return result;
    }
    private State extension(State state, int relation, SqlParser.Join join, TableRelation.Probe probe, AccessCandidate access, int algorithm, long cost, long rows, long logicalRows) {
        if (contradiction) { cost = 0; rows = 0; logicalRows = 0; }
        List<Integer> order = new ArrayList<>(state.order()); order.add(relation);
        List<Step> steps = new ArrayList<>(state.steps()); steps.add(new Step(relation, join, probe, access, algorithm));
        List<AccessCandidate> accesses = new ArrayList<>(state.access()); accesses.add(access);
        List<ScalarPlan.Binding> bindings = new ArrayList<>(state.bindings()); bindings.addAll(tables.get(relation).bindings());
        return new State(with(state.mask(), relation), List.copyOf(order), List.copyOf(accesses), List.copyOf(steps), List.copyOf(bindings), cost, rows, logicalRows, state.ordered());
    }
    private int completeCompare(State a, State b) {
        long ca = finish(a), cb = finish(b); return ca == cb ? structural(a, b) : Long.compare(ca, cb);
    }
    private long finish(State state) {
        long cost = state.cost(), rows = state.rows();
        if (state.ordered()) { long[] prefix = prefix(state); cost = prefix[0]; rows = prefix[1]; }
        if (query != null) {
            SqlParser.Select pipeline = new SqlParser.Select(query.items(), WindowSyntax.map(residual, node -> node instanceof SqlParser.ColumnSlot column ? new SqlParser.ColumnSlot(origins.indexOf(column.index())) : node), query.order(), query.limit(), query.offset(), query.distinct(), query.from(), query.group(), query.having(), query.distinctOn(), query.windows());
            return SelectPipelineEstimate.cost(pipeline, logical, environment, cost, rows, state.logicalRows(), state.ordered(), this::distinctBuckets, (expression, count) -> statistics.rows(qualify(expression, logical, origins, raw), count));
        }
        long filtered = Math.min(rows, statistics.rows(residual, state.logicalRows()));
        return PlanEstimate.add(cost, PlanEstimate.add(PlanEstimate.mul(rows, PlanEstimate.operations(residual)), Math.min(take, Math.max(0, filtered - skip))));
    }
    private Long distinctBuckets(SqlParser.Expr key) {
        SqlParser.Expr qualified = qualify(key, logical, origins, raw);
        return statistics.buckets(qualified);
    }
    private long visits(State state) {
        Step step = state.steps().getLast(); State before = prefixState(state);
        long rows = statistics.rows(residual, state.rows()), wanted = PlanEstimate.add(take, skip);
        if (take == 0) return 0;
        if (rows == 0 || rows <= wanted) return before.rows();
        return Math.min(before.rows(), StatisticsEstimate.scale(wanted, before.rows(), rows));
    }
    private State prefixState(State state) {
        State prefix = driver(state.order().getFirst(), state.access().getFirst());
        for (int i = 0; i < state.steps().size() - 1; i++) {
            Step step = state.steps().get(i);
            for (State candidate : extend(prefix, step.relation())) if (candidate.steps().getLast().algorithm() == step.algorithm() && AccessCandidate.compare(candidate.steps().getLast().access(), step.access()) == 0) { prefix = candidate; break; }
        }
        return prefix;
    }
    private long[] prefix(State state) {
        if (!state.ordered() || state.steps().isEmpty()) return new long[]{state.cost(), state.rows()};
        State before = prefixState(state); Step step = state.steps().getLast(); TableRelation table = tables.get(step.relation());
        PlanNode inner = step.probe() == null ? table.accessEstimate(step.access(), false) : table.probeEstimate(step.probe());
        long count = visits(state), pairs = PlanEstimate.mul(count, inner.rows()), work;
        long rows = step.probe() == null ? statistics.rows(step.join().on(), pairs) : pairs;
        if (step.algorithm() == 0) work = PlanEstimate.add(PlanEstimate.mul(count, inner.cost()), onWork(step.join().on(), pairs));
        else if (step.algorithm() == 1) work = PlanEstimate.add(inner.cost(), JoinPlan.hashWork(step.join(), before.bindings(), table.bindings(), count, inner.rows(), statistics));
        else work = PlanEstimate.add(inner.cost(), onWork(step.join().on(), pairs));
        return new long[]{PlanEstimate.add(before.cost(), work), rows};
    }
    private PlanNode prefixPlan(State state, PlanNode node) {
        if (!state.ordered()) return node;
        long[] prefix = prefix(state); List<PlanNode> children = node.children();
        if (state.steps().getLast().algorithm() == 0) {
            TableRelation table = tables.get(state.steps().getLast().relation());
            PlanNode single = table.probeEstimate(state.steps().getLast().probe()), existing = children.getLast(); long count = visits(state);
            children = List.of(children.getFirst(), new PlanNode(existing.label(), existing.detail(), PlanEstimate.mul(count, single.rows()), PlanEstimate.mul(count, single.cost()), existing.children(), existing.lane()));
        }
        return new PlanNode(node.label(), node.detail(), prefix[1], prefix[0], children, node.lane());
    }
    private static int compare(State a, State b) { return a.cost() == b.cost() ? structural(a, b) : Long.compare(a.cost(), b.cost()); }
    private static int structural(State a, State b) {
        for (int i = 0; i < a.order().size(); i++) { int order = Integer.compare(a.order().get(i), b.order().get(i)); if (order != 0) return order; }
        for (int i = 0; i < a.access().size(); i++) { int order = AccessCandidate.compare(a.access().get(i), b.access().get(i)); if (order != 0) return order; }
        for (int i = 0; i < a.steps().size(); i++) { int order = Integer.compare(a.steps().get(i).algorithm(), b.steps().get(i).algorithm()); if (order != 0) return order; }
        return 0;
    }
    private static void retain(List<State> states, State incoming) {
        for (State prior : states) if (dominates(prior, incoming)) return;
        states.removeIf(prior -> dominates(incoming, prior)); states.add(incoming);
    }
    private static boolean dominates(State a, State b) {
        if (a.ordered() != b.ordered()) return false;
        if (a.cost() > b.cost() || a.rows() > b.rows() || a.logicalRows() > b.logicalRows()) return false;
        return a.cost() < b.cost() || a.rows() < b.rows() || a.logicalRows() < b.logicalRows() || structural(a, b) <= 0;
    }
}
