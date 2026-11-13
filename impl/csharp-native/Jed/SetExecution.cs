using System.Runtime.CompilerServices;

namespace Jed;

internal static class SetExecution
{
    internal static async IAsyncEnumerable<SqlValue[]> Run(IAsyncEnumerable<SqlValue[]> left, IAsyncEnumerable<SqlValue[]> right,
        SqlType[] types, string operation, bool all, Meter meter, [EnumeratorCancellation] CancellationToken cancellationToken,
        bool leftMemory = false, bool rightMemory = false)
    {
        using var inputMemory = new QueryInputMemory(meter);
        using var stateMemory = new QueryInputMemory(meter);
        using var outputMemory = new QueryInputMemory(meter);
        async ValueTask<List<SqlValue[]>> Read(IAsyncEnumerable<SqlValue[]> source, bool transferred)
        {
            var rows = new List<SqlValue[]>();
            await foreach (var row in source.WithCancellation(cancellationToken).ConfigureAwait(false))
            {
                var converted = row.Select((value, index) => Resolve.Cast(value, types[index])).ToArray();
                var before = transferred && meter.Memory.Enabled ? MemorySize.RowBytes(row) : 0;
                rows.Add(inputMemory.Take(converted, transferred, previousBytes: before));
            }
            return rows;
        }
        var a = await Read(left, leftMemory).ConfigureAwait(false); var b = await Read(right, rightMemory).ConfigureAwait(false);
        var kept = new List<SqlValue[]>();
        if (operation == "union" && all) kept.AddRange(a.Concat(b));
        else
        {
            var aKeys = new string[a.Count]; var bKeys = new string[b.Count];
            for (var i = 0; i < a.Count; i++) { foreach (var value in a[i]) GroupExecution.ChargeKey(value, meter); aKeys[i] = GroupExecution.Identity(a[i]); }
            for (var i = 0; i < b.Count; i++) { foreach (var value in b[i]) GroupExecution.ChargeKey(value, meter); bKeys[i] = GroupExecution.Identity(b[i]); }
            var emitted = new HashSet<string>(StringComparer.Ordinal);
            void Keep(SqlValue[] row, string key)
            {
                if (!all && !emitted.Add(key)) return;
                if (!all && meter.Memory.Enabled) stateMemory.ReserveBytes(MemorySize.EntryBytes(row));
                kept.Add(row);
            }
            if (operation == "union")
            {
                for (var i = 0; i < a.Count; i++) Keep(a[i], aKeys[i]);
                for (var i = 0; i < b.Count; i++) Keep(b[i], bKeys[i]);
            }
            else
            {
                var counts = new Dictionary<string, int>(StringComparer.Ordinal);
                for (var i = 0; i < b.Count; i++)
                {
                    if (!counts.TryGetValue(bKeys[i], out var count) && meter.Memory.Enabled) stateMemory.ReserveBytes(MemorySize.EntryBytes(b[i]));
                    counts[bKeys[i]] = count + 1;
                }
                for (var i = 0; i < a.Count; i++)
                {
                    meter.Checkpoint(cancellationToken); var key = aKeys[i]; var count = counts.GetValueOrDefault(key);
                    if (all && count > 0) counts[key] = count - 1;
                    if (operation == "intersect" ? count > 0 : count == 0) Keep(a[i], key);
                }
            }
        }
        foreach (var row in kept) { inputMemory.Transfer(row); outputMemory.Take(row, true); }
        inputMemory.Dispose(); stateMemory.Dispose();
        foreach (var row in kept) { meter.Checkpoint(cancellationToken); outputMemory.Transfer(row); yield return row; }
    }
}
