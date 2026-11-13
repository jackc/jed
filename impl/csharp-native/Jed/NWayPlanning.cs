using System.Runtime.CompilerServices;

namespace Jed;

// Left-deep all-base islands retain physical source order separately from SQL
// slots. Every authored ON tree owns its source relation even if it references
// only earlier columns, and becomes ready exactly once.
internal static class NWayPlanning
{
    private sealed record Edge(int Owner, Resolved? Authored, Resolved? Residual, int[] Dependencies);
    private sealed record State(int[] Sequence, int[] Slots, PreparedRelation Metadata, long Cost, long LogicalRows,
        AccessPlan Access, State? Previous = null, Resolved? On = null, HashJoinPlan? Hash = null, IndexNestedLoop? Inl = null, string Kind = "inner");

    internal static PreparedRelation? Prepare(RelationSyntax syntax, Database database, DatabaseSnapshot snapshot,
        PendingDatabase? pending, Catalog catalog, Meter meter, CancellationToken token, QueryScope? scope)
    {
        var names = new List<TableRelation>(); var syntaxEdges = new List<JoinedRelation>();
        bool Flatten(RelationSyntax item)
        {
            if (item is TableRelation name) { names.Add(name); return true; }
            if (item is not JoinedRelation join || join.Kind is not ("inner" or "cross") || join.Natural || join.Using is { Length: > 0 } || join.Right is not TableRelation right) return false;
            if (!Flatten(join.Left)) return false;
            names.Add(right); syntaxEdges.Add(join); return true;
        }
        if (!Flatten(syntax) || names.Count < 3) return null;
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
        for (var i = 0; i < syntaxEdges.Count; i++)
        {
            var authored = syntaxEdges[i].On is null ? null : Resolve.Expr(Resolve.Bind(syntaxEdges[i].On!, columns.Take(offsets[i + 2]).Concat(scope?.OuterColumns ?? []).ToArray()));
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
        PreparedRelation Build(PreparedRelation[] relations, Resolved? residualWhere = null, HashSet<int>? touched = null)
        {
            var inventories = relations.Select(relation =>
            {
                var source = relation.BaseAccess!;
                return AccessPlan.Inventory(source.Table, source.Tree, source.Filter, source.Storage, meter)
                    .Select(access => access with { Ordered = access.Kind is AccessKind.Btree or AccessKind.IndexIntervals }).ToArray();
            }).ToArray();
            State Driver(int index, AccessPlan access)
            {
                var relation = relations[index]; var rows = Math.Min(relation.EstimatedRows ?? PlannerFacts.SrfRows, access.EstimatedRows);
                var slots = Enumerable.Range(offsets[index], relation.Columns.Length).ToArray();
                return new([index], slots, relation with { EstimatedRows = rows }, access.EstimatedCost, relation.EstimatedRows ?? rows, access);
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
                    yield return new(sequence, slots, metadata, Estimate.Add(previous.Cost, Estimate.Add(access.EstimatedCost, work)), rows, access, previous, on, Kind: kind);
                    var hash = HashJoinPlan.Select(authored, previous.Slots.Length, kind, previous.Metadata.EstimatedRows, inner.EstimatedRows, on);
                    if (hash is not null)
                    {
                        work = HashJoinPlan.Work(previous.Metadata.EstimatedRows ?? 0, inner.EstimatedRows ?? 0, hash.Types, on);
                        yield return new(sequence, slots, metadata, Estimate.Add(previous.Cost, Estimate.Add(access.EstimatedCost, work)), rows, access, previous, on, hash, Kind: kind);
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
                        yield return new(sequence, slots, metadata, cost, rows, access, previous, on, Inl: inl with { PinnedAccess = access }, Kind: kind);
                    }
            }
            var states = Enumerable.Range(0, relations.Length).SelectMany(index => inventories[index].Select(access => Driver(index, access))).ToList();
            if (relations.Length <= 8)
            {
                for (var size = 1; size < relations.Length; size++)
                {
                    var next = new Dictionary<int, List<State>>();
                    foreach (var previous in states.OrderBy(state => state, StructuralComparer.Instance))
                        for (var index = 0; index < relations.Length; index++)
                            if (!previous.Sequence.Contains(index))
                                foreach (var candidate in Extend(previous, index))
                                {
                                    var mask = candidate.Sequence.Aggregate(0, (set, value) => set | 1 << value);
                                    if (!next.TryGetValue(mask, out var frontier)) next[mask] = frontier = [];
                                    if (frontier.Any(state => Dominates(state, candidate))) continue;
                                    frontier.RemoveAll(state => Dominates(candidate, state)); frontier.Add(candidate);
                                }
                    states = next.OrderBy(pair => pair.Key).SelectMany(pair => pair.Value).ToList();
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
            long FinalCost(State state)
            {
                var rows = state.Metadata.EstimatedRows ?? 0;
                return Estimate.Add(state.Cost, Estimate.Add(Estimate.Multiply(rows, residualWhere is null ? 0 : PredicatePlan.Nodes(residualWhere)), Estimate.Rows(rows, residualWhere)));
            }
            var winner = states.OrderBy(FinalCost).ThenBy(state => state, StructuralComparer.Instance).First();
            var needed = touched is null ? Enumerable.Range(0, columns.Length).ToHashSet() : new HashSet<int>(touched);
            foreach (var edge in edges) if (edge.Residual is { } on) needed.UnionWith(PredicatePlan.Slots(on));
            if (residualWhere is not null) needed.UnionWith(PredicatePlan.Slots(residualWhere));
            var spill = database.WorkMem > 0 && database.ScratchHost is { } scratch ? new SpillPolicy(scratch, database.WorkMem, null) : null;
            PreparedRelation Materialize(State state)
            {
                var index = state.Sequence[^1]; var source = relations[index];
                if (state.Inl is null && source.WithAccess is { } access) source = access(state.Access);
                if (source.WithTouched is { } mask) source = mask(needed.Where(slot => slot >= offsets[index] && slot < offsets[index + 1]).Select(slot => slot - offsets[index]).ToHashSet());
                ExplainPlanning.ScanFilterSlots(source.Plan, source.BaseAccess?.Filter, offsets[index], meter);
                if (state.Previous is null) return source with { EstimatedRows = state.Metadata.EstimatedRows };
                var left = Materialize(state.Previous);
                var physicalMask = state.Slots.Select((slot, position) => (slot, position)).Where(item => needed.Contains(item.slot)).Select(item => item.position).ToHashSet();
                var rows = RelationExecution.Join(left, source, state.On, state.Kind, [], meter, token, hashPlan: state.Hash, inl: state.Inl, touched: physicalMask, spill: spill);
                var relation = new PreparedRelation(state.Metadata.Columns, rows, EstimatedRows: state.Metadata.EstimatedRows, MemoryOutput: spill is null, BoundedOutput: spill is not null);
                relation = ExplainPlanning.Join(relation, left, source, state.Kind, state.On, state.Hash, state.Inl, meter);
                if (meter.Explain?.Verbose == true && state.On is not null && relation.Plan is { } node)
                {
                    var marker = node.Detail.IndexOf("; on=", StringComparison.Ordinal);
                    if (marker >= 0) node.Detail = node.Detail[..marker] + "; on=" + ExplainExpression.Print(Global(state.On, state.Slots));
                }
                return relation;
            }
            var physical = Materialize(winner);
            return physical with
            {
                Columns = columns, Rows = Restore(physical.Rows, winner.Slots, token),
                WithTouched = mask => Build(relations, residualWhere, mask),
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
                    return (Build(next, rest, touched), rest);
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
