using System.Runtime.CompilerServices;

namespace Jed;

internal sealed record IndexSetAccess(CatalogIndex Index, StorageTree Tree, TableScan[] Ranges,
    long EstimatedRows, long EstimatedPages, long EstimatedCost, Resolved Predicate)
{
    private const int MaximumProbes = 65536;
    internal static IEnumerable<IndexSetAccess> Candidates(CatalogTable table, StorageTree primary, Resolved? filter, QueryStorage storage)
    {
        if (filter is null) yield break;
        foreach (var index in (table.Indexes ?? []).OrderBy(item => item.Name, Utf8NameComparer.Instance))
        {
            if (index.Kind != 0 || index.Keys.Length == 0 || index.Keys.Any(key => key.CollationSkew || key.Expression is not null) ||
                !StatementExecution.IndexDependenciesUsable(index, storage.Catalog)) continue;
            if (index.Predicate is not null && !SecondaryAccess.Implies(filter, StatementExecution.ResolveIndexPredicate(table, index, storage.Catalog)!)) continue;
            var keys = StatementExecution.ResolveIndexKeys(table, index, storage.Catalog);
            var terms = PredicatePlan.Conjuncts(filter).ToArray();
            var variants = new List<Resolved[]> { terms }; var replaced = new HashSet<int>(); var consumed = false;
            foreach (var key in keys)
            {
                for (var at = 0; at < terms.Length; at++)
                {
                    if (replaced.Contains(at) || Expand(terms[at], key) is not { } alternatives) continue;
                    if (alternatives.Length != 0 && variants.Count > MaximumProbes / alternatives.Length) break;
                    var next = new List<Resolved[]>();
                    foreach (var variant in variants) foreach (var alternative in alternatives)
                    { var copy = variant.ToArray(); copy[at] = alternative; next.Add(copy); }
                    variants = next; replaced.Add(at); consumed = true;
                }
            }
            if (!consumed) continue;
            var ranges = new List<TableScan>(); long count = 0, pages = 0;
            var tree = storage.Database.IndexTree(index, storage.Pending, storage.Snapshot);
            var legal = true; var usedSet = variants.Count == 0;
            foreach (var variant in variants)
            {
                var bound = SecondaryAccess.Bounds(PredicatePlan.Combine(variant)!, keys, index.Keys);
                if (bound is null) { legal = false; break; }
                var used = bound.Value.Predicate is null ? [] : PredicatePlan.Conjuncts(bound.Value.Predicate).ToArray();
                usedSet |= replaced.Any(at => used.Any(term => ReferenceEquals(term, variant[at]) || PredicatePlan.Conjuncts(variant[at]).Any(part => ReferenceEquals(term, part))));
                ranges.Add(bound.Value.Scan);
                var rows = bound.Value.Scan.Empty ? 0 : Estimate.Rows(table, storage.Catalog, bound.Value.Predicate);
                if (index.Unique && bound.Value.Equalities == keys.Length) rows = Math.Min(rows, 1);
                count = Math.Min(table.RowCount, Estimate.Add(count, rows));
                pages = Estimate.Add(pages, rows == 0 ? 0 : Math.Clamp(Estimate.Rows(table, storage.Catalog, bound.Value.Predicate, tree.NodeCount), tree.Height, tree.NodeCount));
            }
            if (!legal || !usedSet) continue;
            var canonical = KeyIntervals.Canonical(ranges, false);
            // Duplicate probes are one physical interval and one estimate.
            if (canonical.Length < ranges.Count && ranges.Count != 0)
            { count = Estimate.Scale(count, canonical.Length, ranges.Count); pages = Estimate.Scale(pages, canonical.Length, ranges.Count); }
            count = KeySetEstimate.Rows(table, storage.Catalog, filter, keys, index.Unique, table.RowCount);
            var partial = StatementExecution.ResolveIndexPredicate(table, index, storage.Catalog);
            if (partial is not null)
            {
                var constrained = keys.Where(key => terms.Any(term => PredicatePlan.Slots(term).SetEquals([key.Slot]))).Select(key => key.Slot).ToHashSet();
                foreach (var term in PredicatePlan.Conjuncts(partial))
                    if (!PredicatePlan.Slots(term).Overlaps(constrained)) count = Estimate.Rows(count, term);
            }
            pages = count == 0 ? 0 : Math.Clamp(KeySetEstimate.Rows(table, storage.Catalog, filter, keys, index.Unique, tree.NodeCount), tree.Height, tree.NodeCount);
            var cost = Estimate.Add(pages, Estimate.Multiply(count, Estimate.Add(primary.Height + 1L, PredicatePlan.Nodes(filter))));
            yield return new(index, tree, canonical, count, pages, cost, filter);
        }
    }
    internal static Resolved[]? Expand(Resolved term, Resolved key)
    {
        if (term.Op == "or" && PredicatePlan.Slots(term).SetEquals([key.Slot]))
        {
            Resolved[]? Arm(Resolved value)
            {
                if (value.Op == "or")
                {
                    var a = Arm(value.Args[0]); var b = Arm(value.Args[1]);
                    return a is null || b is null || a.Length > MaximumProbes - b.Length ? null : a.Concat(b).ToArray();
                }
                if (value.Op == "and")
                {
                    var a = Arm(value.Args[0]); var b = Arm(value.Args[1]);
                    if (a is null || b is null || b.Length != 0 && a.Length > MaximumProbes / b.Length) return null;
                    return a.SelectMany(left => b.Select(right => PredicatePlan.Combine([left, right])!)).ToArray();
                }
                if (Expand(value, key) is { } expanded) return expanded;
                if (value.Op is "=" or "<" or "<=" or ">" or ">=" && value.Args.Length == 2 &&
                    (Resolve.Equivalent(value.Args[0], key) && value.Args[1].Op == "constant" || Resolve.Equivalent(value.Args[1], key) && value.Args[0].Op == "constant")) return [value];
                return null;
            }
            return Arm(term);
        }
        if (term.Args.Length == 0 || !Resolve.Equivalent(term.Args[0], key)) return null;
        if (term.Op == "quantified" && term.Args[1] is { Op: "array" } literalArray && literalArray.Type.Element != key.Type && !(key.Type.IsInteger() && literalArray.Args.All(AccessConstant.IntegerLiteralOrNull))) return null;
        IEnumerable<Resolved>? values = null;
        if (term.Op == "in_list" && term.Text != "not" && term.Args.Skip(1).All(value => value.Op == "constant")) values = term.Args.Skip(1);
        else if (term.Op == "quantified" && term.Text == "= any")
        {
            if (term.Args[1] is { Op: "array", Text: not "nested" } array && array.Args.All(value => AccessConstant.Try(value, out _)))
                values = array.Args.Select(value => { AccessConstant.Try(value, out var literal); return new Resolved("constant", literal.Type, literal, []); });
            else if (term.Args[1] is { Op: "constant", ParameterUnbound: false } constant)
                values = constant.Constant.IsNull ? [] : constant.Constant.AsArray().Values.Select(value => new Resolved("constant", value.Type, value, []));
        }
        else if (term.Op is "in_init" or "quantified_init" && term.Subquery is { Initialized: true } cell && (term.Op == "in_init" && term.Text != "not" || term.Text == "= any"))
            values = cell.Values.Select(value => new Resolved("constant", value.Type, value, []));
        if (values is null) return null;
        var result = new List<Resolved>(); var seen = new HashSet<string>(StringComparer.Ordinal);
        foreach (var source in values)
        {
            var value = source;
            if (value.ParameterUnbound) return null;
            if (value.Constant.IsNull) continue;
            if (value.Type != key.Type)
            {
                if (!AccessConstant.Point(value.Constant, key.Type, out var point)) return null;
                if (point is null) continue;
                value = new("constant", key.Type, point.Value, []);
            }
            if (!seen.Add(GroupExecution.Identity([value.Constant]))) continue;
            result.Add(new("=", SqlType.Boolean, default, [term.Args[0], value], Collation: term.Collation));
            if (result.Count > MaximumProbes) return null;
        }
        return result.ToArray();
    }
    internal async IAsyncEnumerable<StorageRecord> Records(CatalogTable table, StorageTree primary, Catalog catalog,
        Meter meter, IReadOnlySet<int>? touched, bool streaming, [EnumeratorCancellation] CancellationToken token)
    {
        var buffered = streaming ? null : new List<StorageRecord>();
        foreach (var range in Ranges)
        {
            meter.Charge(Tree.CountRangeNodes(range.Lower, range.LowerInclusive, range.Upper, range.UpperInclusive)); meter.Checkpoint(token);
            await foreach (var entry in Tree.ScanRangeAsync(range.Lower, range.LowerInclusive, range.Upper, range.UpperInclusive, cancellationToken: token).ConfigureAwait(false))
            {
                var key = StatementExecution.IndexStorageKey(table, Index, entry.Key, catalog);
                meter.Charge(primary.Height); meter.Checkpoint(token);
                var record = await primary.FindAsync(key, token).ConfigureAwait(false) ?? throw StorageFormat.Corrupt("index interval names a missing row");
                meter.Charge(primary.ValueReadUnits(record, touched));
                if (streaming) { meter.Charge(Costs.StorageRowRead); meter.Checkpoint(token); yield return record; }
                else buffered!.Add(record);
            }
        }
        if (buffered is not null) foreach (var record in buffered)
        { meter.Charge(Costs.StorageRowRead); meter.Checkpoint(token); yield return record; }
    }
}
