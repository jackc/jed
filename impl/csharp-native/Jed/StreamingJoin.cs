using System.Runtime.CompilerServices;

namespace Jed;

internal static class StreamingJoin
{
    internal static async IAsyncEnumerable<SqlValue[]> Run(PreparedRelation left, PreparedRelation right, Resolved? on,
        Resolved? filter, string kind, IndexNestedLoop? index, long count, Meter meter,
        [EnumeratorCancellation] CancellationToken token)
    {
        using var leftMemory = new QueryInputMemory(meter);
        var outer = new List<SqlValue[]>();
        await foreach (var row in left.Rows.WithCancellation(token).ConfigureAwait(false))
            outer.Add(await leftMemory.TakeAsync(row, left.MemoryOutput).ConfigureAwait(false));
        using var rightMemory = new QueryInputMemory(meter);
        using var outputMemory = new QueryInputMemory(meter);
        var materialized = new List<SqlValue[]>();
        if (index is null)
            await foreach (var row in right.Rows.WithCancellation(token).ConfigureAwait(false))
                materialized.Add(await rightMemory.TakeAsync(row, right.MemoryOutput).ConfigureAwait(false));
        long produced = 0;
        if (count == 0) yield break;
        foreach (var a in outer)
        {
            var matched = false;
            using var boundMemory = new QueryInputMemory(meter);
            var inner = materialized;
            if (index is not null)
            {
                inner = [];
                await foreach (var row in index.Rows(a, meter, token).ConfigureAwait(false))
                    inner.Add(await boundMemory.TakeAsync(row, false).ConfigureAwait(false));
            }
            foreach (var b in inner)
            {
                var combined = a.Concat(b).ToArray();
                if (!await QueryExecution.KeepsAsync(on, combined, meter).ConfigureAwait(false)) continue;
                matched = true;
                if (!await QueryExecution.KeepsAsync(filter, combined, meter).ConfigureAwait(false)) continue;
                outputMemory.Reserve(combined); outputMemory.Transfer(combined);
                produced++; yield return combined;
                if (produced >= count) yield break;
            }
            if (!matched && kind == "left")
            {
                var combined = a.Concat(right.Columns.Select(column => SqlValue.Null(column.Type))).ToArray();
                if (!await QueryExecution.KeepsAsync(filter, combined, meter).ConfigureAwait(false)) continue;
                outputMemory.Reserve(combined); outputMemory.Transfer(combined);
                produced++; yield return combined;
                if (produced >= count) yield break;
            }
        }
    }
}
