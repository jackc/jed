using System.Runtime.CompilerServices;

namespace Jed;

// An unparenthesized comma chain materializes its relation inputs before any
// combination. Parenthesized joins remain separate eager pipeline boundaries.
internal sealed record CrossJoinChain(PreparedRelation[] Inputs)
{
    internal async IAsyncEnumerable<SqlValue[]> Rows(Meter meter, HashSet<int>? touched, [EnumeratorCancellation] CancellationToken token)
    {
        var inputOwners = Inputs.Select(_ => new QueryInputMemory(meter)).ToArray();
        var rowsets = new List<SqlValue[]>[Inputs.Length];
        QueryInputMemory? previousOwner = null;
        try
        {
            var offset = 0;
            for (var i = 0; i < Inputs.Length; i++)
            {
                var input = Inputs[i];
                var mask = Enumerable.Range(0, input.Columns.Length).Where(slot => touched is null || touched.Contains(offset + slot)).ToHashSet();
                if (input.BaseAccess?.Filter is { } filter) mask.UnionWith(PredicatePlan.Slots(filter));
                if (input.WithTouched is { } withTouched) input = withTouched(mask);
                rowsets[i] = [];
                await foreach (var row in input.Rows.WithCancellation(token).ConfigureAwait(false))
                    rowsets[i].Add(await inputOwners[i].TakeAsync(row, input.MemoryOutput, mask).ConfigureAwait(false));
                offset += input.Columns.Length;
            }
            var types = Inputs.SelectMany(input => input.Columns.Select(column => column.Type)).ToArray();
            var current = rowsets[0]; var rightOffset = Inputs[0].Columns.Length;
            for (var i = 1; i < rowsets.Length; i++)
            {
                var nextOwner = new QueryInputMemory(meter); var next = new List<SqlValue[]>();
                try
                {
                    foreach (var left in current)
                        foreach (var right in rowsets[i])
                        {
                            meter.Checkpoint(token);
                            var combined = types.Select(SqlValue.Null).ToArray(); left.CopyTo(combined, 0); right.CopyTo(combined, rightOffset);
                            nextOwner.Reserve(combined); next.Add(combined);
                        }
                }
                catch { nextOwner.Dispose(); throw; }
                previousOwner?.Dispose(); previousOwner = nextOwner; current = next; rightOffset += Inputs[i].Columns.Length;
            }
            foreach (var owner in inputOwners) owner.Dispose();
            foreach (var row in current) { previousOwner!.Transfer(row); yield return row; }
        }
        finally
        {
            foreach (var owner in inputOwners) owner.Dispose();
            previousOwner?.Dispose();
        }
    }
}
