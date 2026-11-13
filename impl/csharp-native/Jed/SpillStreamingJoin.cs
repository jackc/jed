using System.Runtime.CompilerServices;

namespace Jed;

// Materialize each base input on the ordinary schedule, then stop only the
// join probe work once the requested ordered prefix is complete.
internal static class SpillStreamingJoin
{
    internal static async IAsyncEnumerable<SqlValue[]> Run(PreparedRelation left, PreparedRelation right, Resolved? on,
        Resolved? filter, string kind, IndexNestedLoop? index, long count, SpillPolicy policy, Meter meter,
        [EnumeratorCancellation] CancellationToken token)
    {
        await using var outer = new SpillRowSpool(policy.Host, policy.WorkMem, meter);
        await using var inner = new SpillRowSpool(policy.Host, policy.WorkMem, meter);
        async ValueTask Fill(PreparedRelation input, SpillRowSpool spool)
        {
            await foreach (var row in input.Rows.WithCancellation(token).ConfigureAwait(false))
            {
                var owned = new SqlValue[row.Length];
                for (var i = 0; i < owned.Length; i++) owned[i] = await row[i].ResolveAsync(token).ConfigureAwait(false);
                if (input.MemoryOutput) meter.Memory.Release(MemorySize.RowBytes(row));
                await spool.Append(owned, token).ConfigureAwait(false);
            }
        }
        await Fill(left, outer).ConfigureAwait(false);
        if (index is null) await Fill(right, inner).ConfigureAwait(false);
        if (count == 0) yield break;
        long produced = 0;
        await foreach (var a in outer.Read(token).ConfigureAwait(false))
        {
            await using var candidates = new SpillRowSpool(policy.Host, policy.WorkMem, meter);
            var selected = inner.Read(token);
            if (index is not null)
            {
                await foreach (var row in index.Rows(a, meter, token).ConfigureAwait(false)) await candidates.Append(row, token).ConfigureAwait(false);
                selected = candidates.Read(token);
            }
            var matched = false;
            await foreach (var b in selected.ConfigureAwait(false))
            {
                var row = a.Concat(b).ToArray();
                if (!await QueryExecution.KeepsAsync(on, row, meter).ConfigureAwait(false)) continue;
                matched = true;
                if (!await QueryExecution.KeepsAsync(filter, row, meter).ConfigureAwait(false)) continue;
                produced++; yield return row;
                if (produced >= count) yield break;
            }
            if (!matched && kind == "left")
            {
                var row = a.Concat(right.Columns.Select(column => SqlValue.Null(column.Type))).ToArray();
                if (!await QueryExecution.KeepsAsync(filter, row, meter).ConfigureAwait(false)) continue;
                produced++; yield return row;
                if (produced >= count) yield break;
            }
        }
    }
}
