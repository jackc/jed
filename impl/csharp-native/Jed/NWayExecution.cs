using System.Runtime.CompilerServices;

namespace Jed;

internal sealed record NWayStep(PreparedRelation Input, Resolved? Predicate, HashJoinPlan? Hash, IndexNestedLoop? Index, ExplainNode? Observation);

// An eager island retains its base relations for the whole join phase. Driver
// and intermediate rows share the final physical layout, including NULL slots
// for sources that have not joined yet.
internal static class NWayExecution
{
    internal static async IAsyncEnumerable<SqlValue[]> Run(NWayStep[] steps, HashSet<int> touched, Meter meter,
        [EnumeratorCancellation] CancellationToken token, Resolved? finalFilter = null, long? count = null)
    {
        var inputs = steps.Select(_ => new QueryInputMemory(meter)).ToArray();
        var rows = steps.Select(_ => new List<SqlValue[]>()).ToArray();
        var types = steps.SelectMany(step => step.Input.Columns.Select(column => column.Type)).ToArray();
        var offsets = new int[steps.Length + 1];
        var masks = new HashSet<int>[steps.Length];
        QueryInputMemory? currentOwner = null;
        try
        {
            for (var i = 0; i < steps.Length; i++)
            {
                var input = steps[i].Input;
                offsets[i + 1] = offsets[i] + input.Columns.Length;
                masks[i] = Enumerable.Range(0, input.Columns.Length).Where(slot => touched.Contains(offsets[i] + slot)).ToHashSet();
                if (input.BaseAccess?.Filter is { } filter) masks[i].UnionWith(PredicatePlan.Slots(filter));
                if (steps[i].Index is not null) continue;
                await foreach (var row in input.Rows.WithCancellation(token).ConfigureAwait(false))
                    rows[i].Add(await inputs[i].TakeAsync(row, input.MemoryOutput, masks[i]).ConfigureAwait(false));
            }
            currentOwner = new(meter);
            var current = rows[0];
            if (steps.Length > 2)
            {
                current = [];
                foreach (var row in rows[0])
                {
                    var combined = types.Select(SqlValue.Null).ToArray(); row.CopyTo(combined, 0);
                    currentOwner.Reserve(combined); current.Add(combined);
                }
            }
            for (var i = 1; i < steps.Length; i++)
            {
                var step = steps[i]; var nextOwner = new QueryInputMemory(meter); var next = new List<SqlValue[]>();
                var before = meter.Accrued;
                var indexBefore = step.Index?.Observation?.ActualCost ?? 0;
                try
                {
                    using var hash = step.Hash is null ? null : await HashJoinTable.Build(step.Hash, rows[i], meter).ConfigureAwait(false);
                    var final = i == steps.Length - 1;
                    foreach (var left in current)
                    {
                        if (final && count is { } maximum && next.Count >= maximum) break;
                        if (step.Index is { } index)
                        {
                            inputs[i].Dispose(); rows[i].Clear();
                            await foreach (var row in index.Rows(left, meter, token).ConfigureAwait(false))
                                rows[i].Add(await inputs[i].TakeAsync(row, false, masks[i]).ConfigureAwait(false));
                        }
                        IEnumerable<int> candidates = hash is null ? Enumerable.Range(0, rows[i].Count) : await hash.Probe(step.Hash!, left, meter).ConfigureAwait(false);
                        foreach (var candidate in candidates)
                        {
                            meter.Checkpoint(token);
                            var combined = types.Select(SqlValue.Null).ToArray(); left.CopyTo(combined, 0); rows[i][candidate].CopyTo(combined, offsets[i]);
                            if (!await QueryExecution.KeepsAsync(step.Predicate, combined, meter).ConfigureAwait(false)) continue;
                            if (final && !await QueryExecution.KeepsAsync(finalFilter, combined, meter).ConfigureAwait(false)) continue;
                            nextOwner.Reserve(combined); next.Add(combined);
                            if (final && count is { } target && next.Count >= target) break;
                        }
                        if (step.Index is not null) { inputs[i].Dispose(); rows[i].Clear(); }
                    }
                }
                catch { nextOwner.Dispose(); throw; }
                finally
                {
                    var indexed = (step.Index?.Observation?.ActualCost ?? 0) - indexBefore;
                    step.Observation?.AttributeLocal(Math.Max(0, meter.Accrued - before - indexed));
                }
                currentOwner.Dispose(); currentOwner = nextOwner; current = next;
            }
            foreach (var owner in inputs) owner.Dispose();
            foreach (var row in current) { currentOwner.Transfer(row); yield return row; }
        }
        finally
        {
            foreach (var owner in inputs) owner.Dispose();
            currentOwner?.Dispose();
        }
    }
}
