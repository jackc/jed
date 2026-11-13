using System.Runtime.CompilerServices;

namespace Jed;

// Materialize each base input on the ordinary schedule, then stop only the
// join probe work once the requested ordered prefix is complete.
internal static class SpillStreamingJoin
{
    internal static async IAsyncEnumerable<SqlValue[]> Run(PreparedRelation left, PreparedRelation right, Resolved? on,
        Resolved? filter, string kind, IndexNestedLoop? index, long count, SpillPolicy policy, Meter meter,
        [EnumeratorCancellation] CancellationToken token, HashJoinPlan? hash = null)
    {
        await using var outer = new SpillRowSpool(policy.Host, policy.WorkMem, meter);
        await using var inner = new SpillRowSpool(policy.Host, policy.WorkMem, meter);
        await using var entries = new SpillHashRows(policy.Host, policy.WorkMem, meter);
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
        if (hash is not null)
        {
            var type = SqlType.Record(right.Columns.Select((column, i) => new SqlField("c" + i, column.Type)));
            await foreach (var row in inner.Read(token).ConfigureAwait(false))
            {
                var key = await HashJoinTable.Key(row, hash.Right, Costs.HashBuild, meter).ConfigureAwait(false);
                if (key is not null) await entries.Add(HashJoinTable.Hash(key),
                    [SqlValue.Bytes(key), SqlValue.Composite(new(type, row, true))], token).ConfigureAwait(false);
            }
        }
        if (count == 0) yield break;
        long produced = 0;
        await foreach (var a in outer.Read(token).ConfigureAwait(false))
        {
            await using var candidates = new SpillRowSpool(policy.Host, policy.WorkMem, meter);
            var selected = inner.Read(token);
            if (hash is not null)
            {
                var key = await HashJoinTable.Key(a, hash.Left, Costs.HashProbe, meter).ConfigureAwait(false);
                if (key is not null)
                    await foreach (var entry in entries.Read(HashJoinTable.Hash(key), token).ConfigureAwait(false))
                    {
                        meter.Charge(Math.Max(1, Math.Min(key.Length, entry[0].ByteSpan.Length)) * Costs.HashProbe); meter.Guard();
                        if (key.AsSpan().SequenceEqual(entry[0].ByteSpan))
                            await candidates.Append(entry[1].AsComposite().Fields.ToArray(), token).ConfigureAwait(false);
                    }
                selected = candidates.Read(token);
            }
            else if (index is not null)
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
