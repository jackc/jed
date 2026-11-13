namespace Jed;

// Structural interval-set facts remain available before outer/subquery values
// exist. They describe planning only; execution binds real values to real keys.
internal sealed record KeySetShape(Resolved[] Keys, long Intervals, long Arrays, bool Unknown)
{
    internal static KeySetShape? Read(Resolved? filter, Resolved[] keys)
    {
        if (filter is null) return null;
        var terms = PredicatePlan.Conjuncts(filter).ToArray();
        var consumed = new List<Resolved>(); long intervals = 0, arrays = 0, probes = 1; var unknown = false;
        foreach (var key in keys)
        {
            var sets = terms.Select(term => (Term: term, Set: KeySetEstimate.Expand(term, key))).Where(item => item.Set is not null).ToArray();
            var clips = terms.Where(term => term.Op is "=" or "<" or "<=" or ">" or ">=" && term.Args.Length == 2 &&
                (Resolve.Equivalent(term.Args[0], key) && term.Args[1].Op == "constant" || Resolve.Equivalent(term.Args[1], key) && term.Args[0].Op == "constant")).ToArray();
            if (sets.Length == 0 && clips.Length == 0) break;
            var count = sets.Length == 0 ? 1 : sets[0].Set!.Length;
            if (count > 1 && probes > 65536 / count) break;
            probes = Estimate.Multiply(probes, count); consumed.Add(key);
            unknown |= clips.Any(term => term.Args.Any(arg => arg.ParameterUnbound));
            foreach (var set in sets)
            {
                var shape = Count(set.Term); intervals += shape.Intervals; arrays += shape.Arrays;
                unknown |= set.Set!.Any(term => term.Args.Any(arg => arg.ParameterUnbound));
            }
            var points = sets.Length == 0 ? clips.Any(term => term.Op == "=") : sets[0].Set!.All(term => PredicatePlan.Conjuncts(term).All(part => part.Op == "="));
            if (!points) break;
        }
        return intervals == 0 ? null : new(consumed.ToArray(), intervals, arrays, unknown);
    }
    private static (long Intervals, long Arrays) Count(Resolved term)
    {
        if (term.Op == "or") { var a = Count(term.Args[0]); var b = Count(term.Args[1]); return (a.Intervals + b.Intervals, a.Arrays + b.Arrays); }
        if (term.Op == "and") return (1, PredicatePlan.Conjuncts(term).Count(part => part.Op is "quantified" or "quantified_init"));
        if (term.Op == "in_list") return (term.Args.Length - 1, 0);
        return (1, term.Op is "quantified" or "quantified_init" or "in_init" ? 1 : 0);
    }
    internal static IEnumerable<AccessPlan> UnknownCandidates(CatalogTable table, StorageTree tree, Resolved? filter, QueryStorage storage)
    {
        if (filter is null) yield break;
        var keys = table.PrimaryKey.Select(slot => new Resolved("column", table.Columns[slot].Type.Type, default, [], Slot: slot)).ToArray();
        if (Read(filter, keys) is { Unknown: true }) yield return Candidate(keys, true, tree, null);
        foreach (var index in (table.Indexes ?? []).OrderBy(index => index.Name, Utf8NameComparer.Instance))
        {
            if (index.Kind != 0 || index.Keys.Any(key => key.Expression is not null || key.CollationSkew) || !StatementExecution.IndexDependenciesUsable(index, storage.Catalog)) continue;
            if (index.Predicate is not null && !SecondaryAccess.Implies(filter, StatementExecution.ResolveIndexPredicate(table, index, storage.Catalog)!)) continue;
            var indexKeys = StatementExecution.ResolveIndexKeys(table, index, storage.Catalog);
            if (Read(filter, indexKeys) is { Unknown: true }) yield return Candidate(indexKeys, index.Unique, storage.Database.IndexTree(index, storage.Pending, storage.Snapshot), index);
        }
        AccessPlan Candidate(Resolved[] columns, bool unique, StorageTree accessTree, CatalogIndex? index)
        {
            var rows = KeySetEstimate.Rows(table, storage.Catalog, filter, columns, unique, table.RowCount);
            var pages = rows == 0 ? 0 : Math.Clamp(KeySetEstimate.Rows(table, storage.Catalog, filter, columns, unique, accessTree.NodeCount), accessTree.Height, accessTree.NodeCount);
            var cost = Estimate.Add(pages, Estimate.Multiply(rows, (index is null ? 1L : tree.Height + 1L) + PredicatePlan.Nodes(filter)));
            return new(index is null ? AccessKind.PrimaryIntervals : AccessKind.IndexIntervals, rows, pages, 0, cost, filter, index, Ranges: []);
        }
    }
}
