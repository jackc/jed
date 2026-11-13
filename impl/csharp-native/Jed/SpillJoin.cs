using System.Runtime.CompilerServices;

namespace Jed;

internal static class SpillJoin
{
    internal static async IAsyncEnumerable<SqlValue[]> Run(PreparedRelation left, PreparedRelation right,
        Resolved? predicate, string kind, (int Left, int Right, SqlType Type)[] usingSlots,
        SpillPolicy policy, HashJoinPlan? hash, IndexNestedLoop? inl, Meter meter,
        IReadOnlySet<int> leftTouched, IReadOnlySet<int> rightTouched, IReadOnlySet<int> outputTouched, OuterFrame? lateral,
        [EnumeratorCancellation] CancellationToken token)
    {
        await using var outer = new SpillRowSpool(policy.Host, policy.WorkMem, meter);
        await using var inner = new SpillRowSpool(policy.Host, policy.WorkMem, meter);
        await using var output = new SpillRowSpool(policy.Host, policy.WorkMem, meter);
        await using var entries = new SpillHashRows(policy.Host, policy.WorkMem, meter);
        await using var matchedInner = new SpillKeyState(policy.Host, policy.WorkMem, meter);
        await Materialize(left, outer, leftTouched, meter, token).ConfigureAwait(false);
        if (inl is null && lateral is null) await Materialize(right, inner, rightTouched, meter, token).ConfigureAwait(false);
        if (hash is not null)
        {
            var type = SqlType.Record(right.Columns.Select((column, i) => new SqlField("c" + i, column.Type)));
            await foreach (var row in inner.Read(token).ConfigureAwait(false))
            {
                var key = await HashJoinTable.Key(row, hash.Right, Costs.HashBuild, meter).ConfigureAwait(false);
                if (key is not null)
                    await entries.Add(HashJoinTable.Hash(key), [SqlValue.Bytes(key), SqlValue.Composite(new(type, row, true))], token).ConfigureAwait(false);
            }
        }
        SqlValue[] Joined(SqlValue[] a, SqlValue[] b)
        {
            var result = new SqlValue[usingSlots.Length + a.Length + b.Length];
            for (var i = 0; i < usingSlots.Length; i++)
            {
                var pair = usingSlots[i]; result[i] = Resolve.Cast(a[pair.Left].IsNull ? b[pair.Right] : a[pair.Left], pair.Type);
            }
            a.CopyTo(result, usingSlots.Length); b.CopyTo(result, usingSlots.Length + a.Length);
            for (var i = 0; i < result.Length; i++) if (!outputTouched.Contains(i)) result[i] = SqlValue.Null(result[i].Type);
            return result;
        }
        await foreach (var a in outer.Read(token).ConfigureAwait(false))
        {
            await using var candidates = new SpillRowSpool(policy.Host, policy.WorkMem, meter);
            IAsyncEnumerable<SqlValue[]> selected = inner.Read(token);
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
            else if (lateral is not null)
            {
                lateral.Assign(a);
                if (right.Initialize is { } initialize) await initialize().ConfigureAwait(false);
                await Materialize(right, candidates, rightTouched, meter, token).ConfigureAwait(false);
                selected = candidates.Read(token);
            }
            else if (inl is not null)
            {
                await foreach (var row in inl.Rows(a, meter, token).ConfigureAwait(false))
                    await candidates.Append(await Owned(row, rightTouched, token).ConfigureAwait(false), token).ConfigureAwait(false);
                selected = candidates.Read(token);
            }
            var matched = false; long ordinal = -1;
            await foreach (var b in selected.ConfigureAwait(false))
            {
                ordinal++;
                meter.Checkpoint(token);
                if (!await QueryExecution.KeepsAsync(predicate, a.Concat(b).ToArray(), meter).ConfigureAwait(false)) continue;
                if (kind is "right" or "full")
                    await matchedInner.Put(KeyEncoding.Encode(SqlValue.Integer(ordinal)), [], MemorySize.EntryBytes([SqlValue.Integer(ordinal)]), token).ConfigureAwait(false);
                matched = true; await output.Append(Joined(a, b), token).ConfigureAwait(false);
            }
            if (!matched && kind is "left" or "full")
                await output.Append(Joined(a, right.Columns.Select(column => SqlValue.Null(column.Type)).ToArray()), token).ConfigureAwait(false);
        }
        if (kind is "right" or "full")
        {
            long ordinal = 0;
            var empty = left.Columns.Select(column => SqlValue.Null(column.Type)).ToArray();
            await foreach (var row in inner.Read(token).ConfigureAwait(false))
                if (await matchedInner.Get(KeyEncoding.Encode(SqlValue.Integer(ordinal++)), token).ConfigureAwait(false) is null)
                    await output.Append(Joined(empty, row), token).ConfigureAwait(false);
        }
        await outer.DisposeAsync().ConfigureAwait(false); await inner.DisposeAsync().ConfigureAwait(false); await entries.DisposeAsync().ConfigureAwait(false);
        await matchedInner.DisposeAsync().ConfigureAwait(false);
        await foreach (var row in output.Read(token).ConfigureAwait(false)) yield return row;
    }
    private static async ValueTask<SqlValue[]> Owned(SqlValue[] row, IReadOnlySet<int> touched, CancellationToken token)
    {
        var owned = new SqlValue[row.Length];
        for (var i = 0; i < row.Length; i++) owned[i] = touched.Contains(i) ? await row[i].ResolveAsync(token).ConfigureAwait(false) : SqlValue.Null(row[i].Type);
        return owned;
    }
    private static async ValueTask Materialize(PreparedRelation input, SpillRowSpool spool, IReadOnlySet<int> touched, Meter meter, CancellationToken token)
    {
        await foreach (var row in input.Rows.WithCancellation(token).ConfigureAwait(false))
        {
            var owned = await Owned(row, touched, token).ConfigureAwait(false);
            if (input.MemoryOutput) meter.Memory.Release(MemorySize.RowBytes(row));
            await spool.Append(owned, token).ConfigureAwait(false);
        }
    }
}
