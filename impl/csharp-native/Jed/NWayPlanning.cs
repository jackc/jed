using System.Runtime.CompilerServices;

namespace Jed;

// Left-deep all-base islands retain physical source order separately from SQL
// slots. Every authored ON tree owns its source relation even if it references
// only earlier columns, and becomes ready exactly once.
internal static class NWayPlanning
{
    private sealed record Edge(int Owner, Resolved? Authored, Resolved? Residual, int[] Dependencies);
    private sealed record State(int[] Sequence, int[] Slots, PreparedRelation Metadata, long Cost, long LogicalRows,
        AccessPlan Access, State? Previous = null, Resolved? On = null, HashJoinPlan? Hash = null, IndexNestedLoop? Inl = null, string Kind = "inner", bool Ordered = false);

    internal static PreparedRelation? Prepare(RelationSyntax syntax, Database database, DatabaseSnapshot snapshot,
        PendingDatabase? pending, Catalog catalog, Meter meter, CancellationToken token, QueryScope? scope)
    {
        var names = new List<TableRelation>(); var syntaxEdges = new List<JoinedRelation>(); var edgeStarts = new List<int>();
        bool Flatten(RelationSyntax item, int start = 0)
        {
            if (item is TableRelation name) { names.Add(name); return true; }
            if (item is not JoinedRelation join || join.Kind is not ("inner" or "cross")) return false;
            if (!Flatten(join.Left, start)) return false;
            if (join.Right is TableRelation right)
            {
                names.Add(right); syntaxEdges.Add(join); edgeStarts.Add(start); return true;
            }
            // Comma precedence creates a right-hand join chain, but does not
            // fence its base sources from physical join ordering. Its ON names
            // remain restricted to that comma item.
            if (join.Kind != "cross" || join.On is not null || join.Natural || join.Using is { Length: > 0 } || join.Right is not JoinedRelation) return false;
            var rightStart = names.Count;
            syntaxEdges.Add(join); edgeStarts.Add(start);
            return Flatten(join.Right, rightStart);
        }
        if (!Flatten(syntax) || names.Count < 2) return null;
        static bool Nested(Expression value) => value.Query is not null || value.Args.Any(Nested);
        if (syntaxEdges.Any(edge => edge.On is { } on && Nested(on))) return null;
        // Preparing a base reference has no execution effects. A CTE, view or
        // attachment consumer that does not expose base access is a hard fence.
        var inputs = names.Select(name => RelationExecution.Prepare(name, database, snapshot, pending, catalog, meter, token, scope)).ToArray();
        if (inputs.Any(input => input.BaseAccess is null || (input.Dependencies?.Length ?? 0) != 0)) return null;
        var columns = inputs.SelectMany(input => input.Columns).ToArray();
        if (inputs.SelectMany((input, index) => input.Columns.Select(column => (column.Qualifier, index))).Where(item => item.Qualifier is not null)
            .GroupBy(item => item.Qualifier).Any(group => group.Select(item => item.index).Distinct().Count() > 1))
            throw new SqlException(SqlStates.DuplicateAlias, "relation alias appears twice");
        var offsets = new int[inputs.Length + 1];
        for (var i = 0; i < inputs.Length; i++) offsets[i + 1] = offsets[i] + inputs[i].Columns.Length;
        int Source(int slot)
        {
            for (var i = 0; i < inputs.Length; i++) if (slot < offsets[i + 1]) return i;
            throw new InvalidOperationException("join column outside island");
        }
        var edges = new List<Edge>();
        var mergedLayout = syntaxEdges.Any(edge => edge.Natural || edge.Using is { Length: > 0 });
        BoundColumn Project(BoundColumn column, int slot) => column with
        { Projection = new("column", column.Type, default, [], Slot: slot, Collation: column.Collation) };
        var outputColumns = inputs[0].Columns.Select(Project).ToArray();
        for (var i = 0; i < syntaxEdges.Count; i++)
        {
            var edgeSyntax = syntaxEdges[i];
            if (edgeSyntax.Natural && edgeSyntax.Kind == "cross") throw Errors.Syntax("NATURAL CROSS JOIN is not supported");
            var rightColumns = inputs[i + 1].Columns.Select((column, slot) => Project(column, offsets[i + 1] + slot)).ToArray();
            var startSlot = offsets[edgeStarts[i]];
            bool InItem(BoundColumn column) => PredicatePlan.Slots(column.Projection!).All(slot => slot >= startSlot);
            var precedingColumns = outputColumns.Where(column => !InItem(column)).ToArray();
            var leftColumns = outputColumns.Where(InItem).ToArray();
            var visible = leftColumns.Concat(rightColumns).ToArray();
            var onColumns = visible;
            var authored = edgeSyntax.On is null ? null : Resolve.Expr(Resolve.Bind(edgeSyntax.On, onColumns.Concat(scope?.OuterColumns ?? []).ToArray()));
            var usingNames = edgeSyntax.Natural ? leftColumns.Where(column => !column.Hidden && rightColumns.Any(right => !right.Hidden && right.Name == column.Name))
                .Select(column => column.Name).Distinct(StringComparer.Ordinal).ToArray() : edgeSyntax.Using ?? [];
            if (usingNames.Distinct(StringComparer.Ordinal).Count() != usingNames.Length) throw new SqlException(SqlStates.DuplicateColumn, "duplicate USING column");
            var merged = new List<BoundColumn>();
            foreach (var name in usingNames)
            {
                var leftFields = leftColumns.Where(column => !column.Hidden && column.Name == name).ToArray();
                var rightFields = rightColumns.Where(column => !column.Hidden && column.Name == name).ToArray();
                if (leftFields.Length == 0 || rightFields.Length == 0) throw new SqlException(SqlStates.UndefinedColumn, "USING column missing from input");
                if (leftFields.Length != 1 || rightFields.Length != 1) throw new SqlException(SqlStates.AmbiguousColumn, "ambiguous USING column");
                var leftField = leftFields[0]; var rightField = rightFields[0];
                var common = Resolve.Common(leftField.Type, rightField.Type);
                var equal = new Resolved("=", SqlType.Boolean, default, [leftField.Projection!, rightField.Projection!]);
                authored = authored is null ? equal : new("and", SqlType.Boolean, default, [authored, equal]);
                var projection = leftField.Type == common ? leftField.Projection! : new Resolved("promote", common, default, [leftField.Projection!]);
                merged.Add(new(name, common, Projection: projection));
            }
            outputColumns = precedingColumns.Concat(merged).Concat(visible.Select(column => column with { Hidden = column.Hidden || usingNames.Contains(column.Name, StringComparer.Ordinal) })).ToArray();
            if (authored is not null && authored.Type.Kind is not (SqlKind.Boolean or SqlKind.Unknown)) throw Errors.Type("JOIN ON requires boolean");
            if (authored is not null && ContainsOuter(authored)) return null;
            var dependencies = (authored is null ? Enumerable.Empty<int>() : PredicatePlan.Slots(authored).Select(Source)).Append(i + 1).Distinct().ToArray();
            var residual = new List<Resolved>();
            foreach (var term in authored is null ? [] : PredicatePlan.Conjuncts(PredicatePlan.Order(authored)!))
            {
                var sources = PredicatePlan.Slots(term).Select(Source).Distinct().ToArray();
                if (sources.Length == 1 && PredicatePlan.Safe(term))
                {
                    var source = sources[0]; inputs[source] = PredicatePlan.Filter(inputs[source], PredicatePlan.Shift(term, -offsets[source]), meter, token);
                }
                else residual.Add(term);
            }
            edges.Add(new(i + 1, authored, PredicatePlan.Combine(residual), dependencies));
        }
        PreparedRelation Build(PreparedRelation[] relations, Resolved? residualWhere = null, HashSet<int>? touched = null, SelectPlan? query = null)
        {
            var orderedQuery = query is { Empty: false, Group: null, Windows: null, Distinct: false, Limit: not null, Order.Length: > 0 };
            bool Ordered(int index, AccessPlan access)
            {
                if (!orderedQuery || access.Kind is not (AccessKind.Full or AccessKind.Primary or AccessKind.PrimaryIntervals)) return false;
                var keys = relations[index].BaseAccess!.Table.PrimaryKey;
                return query!.Order!.Length <= keys.Length && query.Order.Select((key, position) =>
                    !key.Descending && key.Expression.Op == "column" && key.Expression.Slot == offsets[index] + keys[position] && key.Expression.Collation?.Table is null).All(value => value);
            }
            var inventories = relations.Select((relation, index) =>
            {
                var source = relation.BaseAccess!;
                var extra = residualWhere is null ? [] : PredicatePlan.Conjuncts(residualWhere).Where(term =>
                {
                    var slots = PredicatePlan.Slots(term);
                    return slots.Count != 0 && slots.All(slot => Source(slot) == index);
                }).Select(term => PredicatePlan.Shift(term, -offsets[index])).ToArray();
                var accessPredicate = PredicatePlan.Combine(extra.Concat(source.Filter is null ? [] : [source.Filter]));
                var unexecuted = (accessPredicate is null ? 0 : PredicatePlan.Nodes(accessPredicate)) - (source.Filter is null ? 0 : PredicatePlan.Nodes(source.Filter));
                return AccessPlan.Inventory(source.Table, source.Tree, accessPredicate, source.Storage, meter)
                    .Select(access => access with { EstimatedCost = Math.Max(0, access.EstimatedCost - Estimate.Multiply(access.EstimatedRows, unexecuted)) })
                    .Select(access => query?.Empty == true ? access with { EstimatedRows = 0, EstimatedPages = 0, EstimatedEntries = 0, EstimatedCost = 0 }
                        : access with { Ordered = access.Kind is AccessKind.Btree or AccessKind.IndexIntervals }).ToArray();
            }).ToArray();
            State Driver(int index, AccessPlan access)
            {
                var relation = relations[index]; var rows = Math.Min(relation.EstimatedRows ?? PlannerFacts.SrfRows, access.EstimatedRows);
                var slots = Enumerable.Range(offsets[index], relation.Columns.Length).ToArray();
                return new([index], slots, relation with { EstimatedRows = rows }, access.EstimatedCost, relation.EstimatedRows ?? rows, access, Ordered: Ordered(index, access));
            }
            IEnumerable<State> Extend(State previous, int index)
            {
                var sequence = previous.Sequence.Append(index).ToArray();
                var slots = previous.Slots.Concat(Enumerable.Range(offsets[index], relations[index].Columns.Length)).ToArray();
                var ready = edges.Where(edge => edge.Dependencies.All(sequence.Contains) && !edge.Dependencies.All(previous.Sequence.Contains)).ToArray();
                var on = Remap(PredicatePlan.Combine(ready.Select(edge => edge.Residual).OfType<Resolved>()), slots);
                var authored = Remap(PredicatePlan.Combine(ready.Select(edge => edge.Authored).OfType<Resolved>()), slots);
                var kind = ready.Length == 0 || ready.All(edge => syntaxEdges[edge.Owner - 1].Kind == "cross") ? "cross" : "inner";
                var extra = residualWhere is null || !PredicatePlan.Slots(residualWhere).All(slots.Contains) ? null : Remap(residualWhere, slots);
                var source = relations[index];
                var combinedColumns = previous.Metadata.Columns.Concat(source.Columns).ToArray();
                foreach (var access in inventories[index])
                {
                    var inner = source with { EstimatedRows = Math.Min(source.EstimatedRows ?? PlannerFacts.SrfRows, access.EstimatedRows) };
                    var rows = Estimate.JoinRows(previous.Metadata, inner, on);
                    var metadata = new PreparedRelation(combinedColumns, source.Rows, EstimatedRows: rows);
                    var product = Estimate.Multiply(previous.Metadata.EstimatedRows ?? 0, inner.EstimatedRows ?? 0);
                    var work = Estimate.Multiply(product, on is null ? 0 : PredicatePlan.Nodes(on));
                    yield return new(sequence, slots, metadata, Estimate.Add(previous.Cost, Estimate.Add(access.EstimatedCost, work)), rows, access, previous, on, Kind: kind, Ordered: previous.Ordered);
                    var hash = HashJoinPlan.Select(authored, previous.Slots.Length, kind, previous.Metadata.EstimatedRows, inner.EstimatedRows, on);
                    if (hash is not null)
                    {
                        work = HashJoinPlan.Work(previous.Metadata.EstimatedRows ?? 0, inner.EstimatedRows ?? 0, hash.Types, on);
                        yield return new(sequence, slots, metadata, Estimate.Add(previous.Cost, Estimate.Add(access.EstimatedCost, work)), rows, access, previous, on, hash, Kind: kind, Ordered: previous.Ordered);
                    }
                }
                var inl = IndexNestedLoop.Select(source, previous.Slots.Length, kind, PredicatePlan.Combine(new[] { on, extra }.OfType<Resolved>()));
                if (inl is not null)
                    foreach (var access in AccessPlan.Inventory(inl.Source.Table, inl.Source.Tree, inl.GenericPredicate(), inl.Source.Storage, meter))
                    {
                        if (access.Kind == AccessKind.Full) continue;
                        var outerRows = previous.Metadata.EstimatedRows ?? 0;
                        var innerRows = Estimate.Rows(inl.Source.Table, inl.Source.Storage.Catalog, inl.Source.Filter, access.EstimatedRows);
                        var remaining = on is null ? null : PredicatePlan.Combine(PredicatePlan.Conjuncts(on).Where(term => !inl.Bounds.Any(bound => Resolve.Equivalent(bound, term))));
                        var rows = Estimate.Rows(Estimate.Multiply(outerRows, innerRows), remaining);
                        var metadata = new PreparedRelation(combinedColumns, source.Rows, EstimatedRows: rows);
                        var fetch = access.Kind is AccessKind.Btree or AccessKind.IndexIntervals or AccessKind.Gin or AccessKind.Gist ? inl.Source.Tree.Height + 1L : 1;
                        var nodes = (on is null ? 0 : PredicatePlan.Nodes(on)) + (inl.Source.Filter is null ? 0 : PredicatePlan.Nodes(inl.Source.Filter));
                        var work = Estimate.Multiply(outerRows, Estimate.Add(Estimate.Add(access.EstimatedPages, access.EstimatedEntries), Estimate.Multiply(access.EstimatedRows, fetch + nodes)));
                        var cost = Estimate.Add(previous.Cost, work);
                        yield return new(sequence, slots, metadata, cost, rows, access, previous, on, Inl: inl with { PinnedAccess = access }, Kind: kind, Ordered: previous.Ordered);
                    }
            }
            var states = Enumerable.Range(0, relations.Length).SelectMany(index => inventories[index].Select(access => Driver(index, access))).ToList();
            if (relations.Length <= 8)
            {
                for (var size = 1; size < relations.Length; size++)
                {
                    var next = new Dictionary<(int Mask, bool Ordered), List<State>>();
                    foreach (var previous in states.OrderBy(state => state, StructuralComparer.Instance))
                        for (var index = 0; index < relations.Length; index++)
                            if (!previous.Sequence.Contains(index))
                                foreach (var candidate in Extend(previous, index))
                                {
                                    var mask = candidate.Sequence.Aggregate(0, (set, value) => set | 1 << value);
                                    var bucket = (mask, candidate.Ordered);
                                    if (!next.TryGetValue(bucket, out var frontier)) next[bucket] = frontier = [];
                                    // A completed plan is priced through its LIMIT window.
                                    // Prefix discounts can favor a higher-cardinality join,
                                    // so complete candidates compare their pipeline cost.
                                    if (size == relations.Length - 1) { frontier.Add(candidate); continue; }
                                    if (frontier.Any(state => Dominates(state, candidate))) continue;
                                    frontier.RemoveAll(state => Dominates(candidate, state)); frontier.Add(candidate);
                                }
                    states = next.OrderBy(pair => pair.Key.Mask).ThenBy(pair => pair.Key.Ordered).SelectMany(pair => pair.Value).ToList();
                }
            }
            else
            {
                var state = states.OrderBy(candidate => candidate.Cost).ThenBy(candidate => candidate, StructuralComparer.Instance).First();
                while (state.Sequence.Length < relations.Length)
                    state = Enumerable.Range(0, relations.Length).Where(index => !state.Sequence.Contains(index)).SelectMany(index => Extend(state, index))
                        .OrderBy(candidate => candidate.Cost).ThenBy(candidate => candidate, StructuralComparer.Instance).First();
                states = [state];
            }
            (long Cost, long Rows, long Delivered, long Visits) Pipeline(State state)
            {
                var rows = state.Metadata.EstimatedRows ?? 0;
                var matched = Estimate.Rows(rows, residualWhere);
                var cost = state.Cost;
                var visits = state.Previous?.Metadata.EstimatedRows ?? 0;
                if (state.Ordered && state.Previous is { } previous)
                {
                    var target = query!.Limit == 0 ? 0 : Estimate.Add(query.Offset, query.Limit!.Value);
                    var leftRows = previous.Metadata.EstimatedRows ?? 0;
                    var visited = target == 0 ? 0 : matched > target ? Math.Min(leftRows, Estimate.Scale(leftRows, target, matched)) : leftRows;
                    visits = visited;
                    var inner = relations[state.Sequence[^1]];
                    var rightRows = Math.Min(inner.EstimatedRows ?? PlannerFacts.SrfRows, state.Access.EstimatedRows);
                    long work;
                    if (state.Inl is not null)
                        work = leftRows == 0 ? 0 : Estimate.Scale(state.Cost - previous.Cost, visited, leftRows);
                    else
                    {
                        work = state.Hash is not null ? HashJoinPlan.Work(visited, rightRows, state.Hash.Types, state.On) :
                            Estimate.Multiply(Estimate.Multiply(visited, rightRows), state.On is null ? 0 : PredicatePlan.Nodes(state.On));
                        work = Estimate.Add(state.Access.EstimatedCost, work);
                    }
                    cost = Estimate.Add(previous.Cost, work);
                    rows = leftRows == 0 ? 0 : Estimate.Scale(rows, visited, leftRows);
                }
                cost = Estimate.Add(cost, Estimate.Multiply(rows, residualWhere is null ? 0 : PredicatePlan.Nodes(residualWhere)));
                return (cost, matched, Estimate.Rows(rows, residualWhere), visits);
            }
            long FinalCost(State state)
            {
                var estimate = Pipeline(state); var cost = estimate.Cost; var rows = estimate.Rows;
                if (query is not { Group: null, Windows: null, Distinct: false }) return Estimate.Add(cost, rows);
                if (!state.Ordered && query.Order is { Length: > 0 } order)
                    cost = Estimate.Add(cost, Estimate.Multiply(rows, order.Aggregate(0L, (sum, key) => Estimate.Add(sum, PredicatePlan.Nodes(key.Expression)))));
                rows = Math.Max(0, rows - query.Offset);
                if (query.Limit is { } limit) rows = Math.Min(rows, limit);
                var projection = query.Projections.Aggregate(query.ChargeRows ? 1L : 0, (sum, item) => Estimate.Add(sum, PredicatePlan.Nodes(item.Expression)));
                return Estimate.Add(cost, Estimate.Multiply(rows, projection));
            }
            var winner = states.OrderBy(FinalCost).ThenBy(state => state, StructuralComparer.Instance).First();
            var needed = touched is null ? Enumerable.Range(0, columns.Length).ToHashSet() : new HashSet<int>(touched);
            foreach (var edge in edges) if (edge.Residual is { } on) needed.UnionWith(PredicatePlan.Slots(on));
            if (residualWhere is not null) needed.UnionWith(PredicatePlan.Slots(residualWhere));
            for (var index = 0; index < relations.Length; index++)
                if (relations[index].BaseAccess?.Filter is { } filter) needed.UnionWith(PredicatePlan.Slots(filter).Select(slot => slot + offsets[index]));
            var spill = (relations.Length > 2 || winner.Hash is not null || query?.Group is not null || query is { Distinct: true, DistinctOnCount: 0 }) && database.WorkMem > 0 && database.ScratchHost is { } scratch ? new SpillPolicy(scratch, database.WorkMem, null) : null;
            var steps = new List<NWayStep>();
            PreparedRelation? finalLeft = null; PreparedRelation? finalRight = null;
            PreparedRelation Materialize(State state)
            {
                var index = state.Sequence[^1]; var source = relations[index];
                if (state.Inl is null && source.WithAccess is { } access) source = access(state.Access);
                if (source.WithTouched is { } mask) source = mask(needed.Where(slot => slot >= offsets[index] && slot < offsets[index + 1]).Select(slot => slot - offsets[index]).ToHashSet());
                ExplainPlanning.ScanFilterSlots(source.Plan, source.BaseAccess?.Filter, offsets[index], meter);
                if (state.Previous is null)
                {
                    steps.Add(new(source, null, null, null, source.Plan));
                    return source with { EstimatedRows = state.Metadata.EstimatedRows };
                }
                var left = Materialize(state.Previous);
                if (ReferenceEquals(state, winner)) { finalLeft = left; finalRight = source; }
                var physicalMask = state.Slots.Select((slot, position) => (slot, position)).Where(item => needed.Contains(item.slot)).Select(item => item.position).ToHashSet();
                var rows = RelationExecution.Join(left, source, state.On, state.Kind, [], meter, token, hashPlan: state.Hash, inl: state.Inl, touched: physicalMask, spill: spill);
                var relation = new PreparedRelation(state.Metadata.Columns, rows, EstimatedRows: state.Metadata.EstimatedRows, MemoryOutput: spill is null, BoundedOutput: spill is not null);
                relation = ExplainPlanning.Join(relation, left, source, state.Kind, state.On, state.Hash, state.Inl, meter);
                steps.Add(new(source, state.On, state.Hash, state.Inl, relation.Plan));
                if (meter.Explain?.Verbose == true && state.On is not null && relation.Plan is { } node)
                {
                    var marker = node.Detail.IndexOf("; on=", StringComparison.Ordinal);
                    if (marker >= 0) node.Detail = node.Detail[..marker] + "; on=" + ExplainExpression.Print(Global(state.On, state.Slots));
                }
                return relation;
            }
            var physical = Materialize(winner);
            var physicalTouched = winner.Slots.Select((slot, position) => (slot, position))
                .Where(item => needed.Contains(item.slot)).Select(item => item.position).ToHashSet();
            if (spill is null)
                physical = physical with { Rows = NWayExecution.Run(steps.ToArray(), physicalTouched, meter, token) };
            PreparedRelation? Bounded(SortPlan[] order, long count, Resolved? filter)
            {
                if (!winner.Ordered) return null;
                if (query!.Limit == 0) count = 0;
                var bounded = physical with { Rows = spill is null
                    ? NWayExecution.Run(steps.ToArray(), physicalTouched, meter, token, Remap(filter, winner.Slots), count)
                    : SpillStreamingJoin.Run(finalLeft!, finalRight!, winner.On, Remap(filter, winner.Slots), winner.Kind, winner.Inl, count, spill, meter, token, winner.Hash),
                    MemoryOutput = spill is null };
                if (bounded.Plan is { } node)
                {
                    var estimate = Pipeline(winner); node.EstimatedCost = estimate.Cost;
                    node.EstimatedRows = estimate.Delivered;
                    if (winner.Inl is not null && winner.Previous!.Metadata.EstimatedRows is long outer && outer > 0)
                    {
                        var inner = node.Children[^1];
                        inner.EstimatedRows = Estimate.Scale(inner.EstimatedRows, estimate.Visits, outer);
                        inner.EstimatedCost = Estimate.Scale(inner.EstimatedCost, estimate.Visits, outer);
                    }
                    if (spill is not null) bounded = bounded with { Rows = ExplainObservation.Rows(bounded.Rows, node, meter, token, inclusive: true) };
                }
                return bounded with { Columns = columns, Rows = Restore(bounded.Rows, winner.Slots, token) };
            }
            return physical with
            {
                Columns = mergedLayout ? outputColumns : columns, Rows = Restore(physical.Rows, winner.Slots, token), RawLayout = mergedLayout,
                WithTouched = mask => Build(relations, residualWhere, mask, query),
                WithQuery = plan => Build(relations, residualWhere, touched, plan),
                Bounded = Bounded,
                PushFilter = filter =>
                {
                    var next = relations.ToArray(); var residual = new List<Resolved>();
                    foreach (var term in PredicatePlan.Conjuncts(filter))
                    {
                        var sources = PredicatePlan.Slots(term).Select(Source).Distinct().ToArray();
                        if (sources.Length == 1 && PredicatePlan.Safe(term))
                        { var index = sources[0]; next[index] = PredicatePlan.Filter(next[index], PredicatePlan.Shift(term, -offsets[index]), meter, token); }
                        else residual.Add(term);
                    }
                    var rest = PredicatePlan.Combine(residual);
                    return (Build(next, rest, touched, query), rest);
                }
            };
        }
        return Build(inputs);
    }
    private static Resolved Global(Resolved value, int[] slots) => value with
    { Slot = value.Op == "column" ? slots[value.Slot] : value.Slot, Args = value.Args.Select(arg => Global(arg, slots)).ToArray() };
    private static bool ContainsOuter(Resolved value) => value.Op == "outer" || value.Args.Any(ContainsOuter);
    private static Resolved? Remap(Resolved? value, int[] slots) => value is null ? null : value with
    { Slot = value.Op == "column" ? Array.IndexOf(slots, value.Slot) : value.Slot, Args = value.Args.Select(arg => Remap(arg, slots)!).ToArray() };
    private static bool Dominates(State a, State b)
    {
        var ar = a.Metadata.EstimatedRows ?? 0; var br = b.Metadata.EstimatedRows ?? 0;
        return a.Cost <= b.Cost && ar <= br && a.LogicalRows <= b.LogicalRows &&
            (a.Cost < b.Cost || ar < br || a.LogicalRows < b.LogicalRows || StructuralComparer.Instance.Compare(a, b) <= 0);
    }
    private sealed class StructuralComparer : IComparer<State>
    {
        internal static readonly StructuralComparer Instance = new();
        public int Compare(State? a, State? b)
        {
            if (a is null || b is null) return a is null ? b is null ? 0 : -1 : 1;
            for (var i = 0; i < Math.Min(a.Sequence.Length, b.Sequence.Length); i++)
                if (a.Sequence[i] != b.Sequence[i]) return a.Sequence[i].CompareTo(b.Sequence[i]);
            var first = Chain(a); var second = Chain(b);
            for (var i = 0; i < first.Length; i++)
            {
                var order = first[i].Access.Kind.CompareTo(second[i].Access.Kind); if (order != 0) return order;
                order = string.CompareOrdinal(first[i].Access.Index?.Name, second[i].Access.Index?.Name); if (order != 0) return order;
            }
            for (var i = 1; i < first.Length; i++)
            { var order = Rank(first[i]).CompareTo(Rank(second[i])); if (order != 0) return order; }
            return 0;
        }
        private static int Rank(State value) => value.Inl is not null ? 0 : value.Hash is not null ? 1 : 2;
        private static State[] Chain(State value)
        { var list = new List<State>(); for (State? node = value; node is not null; node = node.Previous) list.Add(node); list.Reverse(); return list.ToArray(); }
    }
    private static async IAsyncEnumerable<SqlValue[]> Restore(IAsyncEnumerable<SqlValue[]> source, int[] slots, [EnumeratorCancellation] CancellationToken token)
    {
        await foreach (var row in source.WithCancellation(token).ConfigureAwait(false))
        { var output = new SqlValue[slots.Length]; for (var i = 0; i < slots.Length; i++) output[slots[i]] = row[i]; yield return output; }
    }
}
