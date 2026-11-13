using System.Runtime.CompilerServices;
namespace Jed;

internal sealed record MergePlanning(AccessPlan Probe, HashJoinPlan? Hash, ExplainNode? Root, ExplainNode? Loop,
    ExplainNode? ProbeNode, ExplainNode? FinalNode, ExplainNode? FinalFilter)
{
    internal static MergePlanning Create(CatalogTable table, StorageTree tree, PreparedRelation source, Resolved on,
        Resolved? viewFilter, bool finalScan, int clauses, IReadOnlySet<int> touched, QueryStorage storage, Meter meter)
    {
        var width = table.Columns.Length;
        Resolved Generic(Resolved value) => value.Op == "column" && value.Slot >= width
            ? new("constant", value.Type, SqlValue.Null(value.Type), [], ParameterUnbound: true, Collation: value.Collation)
            : value with { Args = value.Args.Select(Generic).ToArray() };
        var generic = Generic(on);
        var probe = AccessPlan.Select(table, tree, generic, storage, meter);
        var sourceRows = source.EstimatedRows ?? PlannerFacts.SrfRows;
        var matched = Estimate.Multiply(sourceRows, Estimate.Rows(table, storage.Catalog, generic, probe.EstimatedRows));
        var probeCost = Estimate.Multiply(sourceRows, probe.EstimatedCost);
        var scanCost = Math.Max(0, probe.EstimatedCost - Estimate.Multiply(probe.EstimatedRows, PredicatePlan.Nodes(on)));
        HashJoinPlan? hash = null; long hashCost = 0;
        if (probe.Kind == AccessKind.Full && HashJoinPlan.Select(on, width, "left", table.RowCount, sourceRows, on) is { } keys)
        {
            long keyWidth = 0;
            foreach (var type in keys.Types) keyWidth = Estimate.Add(keyWidth, type.Kind switch
            {
                SqlKind.Boolean => 1, SqlKind.I16 => 2, SqlKind.I32 or SqlKind.F32 or SqlKind.Date => 4,
                SqlKind.I64 or SqlKind.F64 or SqlKind.Timestamp or SqlKind.Timestamptz => 8, SqlKind.Uuid or SqlKind.Interval => 16, _ => PlannerFacts.VariableKeyBytes
            });
            hashCost = Estimate.Add(scanCost, Estimate.Add(Estimate.Multiply(Estimate.Add(table.RowCount, sourceRows), keyWidth),
                Estimate.Multiply(matched, Estimate.Add(PredicatePlan.Nodes(on), Estimate.Add(4L * keys.Types.Length, keyWidth)))));
            if (hashCost < probeCost) hash = new(keys.Right, keys.Left, keys.Types);
        }
        if (meter.Explain is null) return new(probe, hash, null, null, null, null, null);
        var originalInput = source.Plan ?? throw new InvalidOperationException("MERGE source plan missing");
        var input = originalInput;
        if (input.Label.StartsWith("Subquery", StringComparison.Ordinal) && input.Children.Count == 1 && input.Children[0] is { Label: "Subquery *VALUES*" } values)
            input = new(input.Label, meter.Explain.Verbose ? values.Detail : input.Detail, input.EstimatedRows, input.EstimatedCost, values.Children.ToArray()) { HiddenActual = () => Math.Max(0, originalInput.ActualCost - values.Children.Sum(child => child.ActualCost)) };
        input.EstimatedCost = Estimate.Add(input.EstimatedCost, sourceRows);
        if (meter.Explain.Verbose && !input.Detail.Contains("output=[", StringComparison.Ordinal))
            input.Append("output=[" + string.Join(", ", source.Columns.Select((column, slot) => (column, slot)).Where(item => !item.column.Hidden)
                .Select(item => item.column.Projection is { } projection ? ExplainExpression.Print(projection) : "#" + item.slot)) + "]");
        // Values describes its row count; the surrounding Subquery owns the
        // SELECT-star projection metadata in this DML source position.
        foreach (var child in input.Children.Where(child => child.Label == "Values"))
            child.Detail = string.Join("; ", child.Detail.Split("; ").Where(part => !part.StartsWith("output=[", StringComparison.Ordinal)));
        var scan = ExplainPlanning.Scan(table, probe, generic, touched, false, meter, storage.Catalog, tree.Height);
        if (probe.Kind == AccessKind.Primary) scan.Detail = scan.Detail.Replace(" = null", " = outer", StringComparison.Ordinal)
            .Replace(" < null", " < outer", StringComparison.Ordinal).Replace(" > null", " > outer", StringComparison.Ordinal)
            .Replace(" <= null", " <= outer", StringComparison.Ordinal).Replace(" >= null", " >= outer", StringComparison.Ordinal);
        if (hash is null)
        {
            scan.EstimatedRows = Estimate.Multiply(sourceRows, scan.EstimatedRows);
            scan.EstimatedCost = Estimate.Multiply(sourceRows, scan.EstimatedCost);
        }
        var loop = new ExplainNode(hash is null ? "Nested Loop" : "Hash Join", "left; " + (hash is null ? "" : "keys=" + hash.Types.Length + "; ") +
            ExplainPlanning.Predicate(on, meter.Explain.Verbose, "on"), matched, Estimate.Add(input.EstimatedCost, hash is null ? probeCost : hashCost), input, scan);
        ExplainNode? final = null; ExplainNode? finalFilter = null;
        if (finalScan)
        {
            var access = AccessPlan.Select(table, tree, viewFilter, storage, meter);
            final = ExplainPlanning.Scan(table, access, viewFilter, touched, false, meter, storage.Catalog, tree.Height);
            if (viewFilter is not null) finalFilter = new("Filter", ExplainPlanning.Predicate(viewFilter, meter.Explain.Verbose),
                Estimate.Rows(table, storage.Catalog, viewFilter, access.EstimatedRows), access.EstimatedCost, final);
        }
        var tail = finalFilter ?? final;
        var root = new ExplainNode("Merge " + table.Name, "when=" + clauses, Estimate.Add(sourceRows, tail?.EstimatedRows ?? 0),
            Estimate.Add(loop.EstimatedCost, tail?.EstimatedCost ?? 0), tail is null ? [loop] : [loop, tail]);
        return new(probe, hash, root, loop, scan, final, finalFilter);
    }
    internal AccessPlan ForRow(CatalogTable table, StorageTree tree, Resolved predicate, QueryStorage storage, Meter meter) =>
        AccessPlan.Inventory(table, tree, predicate, storage, meter).FirstOrDefault(candidate => candidate.Kind == Probe.Kind && candidate.Index?.Name == Probe.Index?.Name)
        ?? AccessPlan.Primary(table, tree, null);
    internal static async IAsyncEnumerable<StorageRecord> Observe(IAsyncEnumerable<StorageRecord> records, ExplainNode? node,
        Meter meter, [EnumeratorCancellation] CancellationToken token)
    {
        await using var iterator = records.GetAsyncEnumerator(token);
        while (true)
        {
            var before = meter.Accrued; bool next;
            try { next = await iterator.MoveNextAsync().ConfigureAwait(false); }
            finally { node?.Attribute(meter.Accrued - before); }
            if (!next) yield break;
            yield return iterator.Current;
        }
    }
}
