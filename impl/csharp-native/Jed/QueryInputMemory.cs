using System.Runtime.CompilerServices;

namespace Jed;

// Owns incoming reservations independently of the iterator that transferred
// them. Early cursor disposal and failures release every outstanding row.
internal sealed class QueryInputMemory(Meter meter) : IDisposable
{
    private readonly Dictionary<SqlValue[], long> rows = new(ReferenceEqualityComparer.Instance);
    private long held;
    internal async IAsyncEnumerable<SqlValue[]> Adopt(IAsyncEnumerable<SqlValue[]> source,
        [EnumeratorCancellation] CancellationToken token)
    {
        await foreach (var row in source.WithCancellation(token).ConfigureAwait(false))
        {
            if (meter.Memory.Enabled)
            {
                var bytes = MemorySize.RowBytes(row);
                rows[row] = Meter.Add(rows.GetValueOrDefault(row), bytes);
                held = Meter.Add(held, bytes);
            }
            yield return row;
        }
    }
    internal async ValueTask<SqlValue[]> TakeAsync(SqlValue[] row, bool transferred, IReadOnlySet<int>? touched = null)
    {
        if (!meter.Memory.Enabled) return row;
        var resolved = new SqlValue[row.Length];
        for (var i = 0; i < row.Length; i++) resolved[i] = touched is not null && !touched.Contains(i) ? SqlValue.Null(row[i].Type) :
            await row[i].ResolveAsync(meter.Cancellation).ConfigureAwait(false);
        return Take(resolved, transferred, previousBytes: transferred ? MemorySize.RowBytes(row) : null);
    }
    internal SqlValue[] Take(SqlValue[] row, bool transferred, IReadOnlySet<int>? touched = null, long? previousBytes = null)
    {
        if (!meter.Memory.Enabled) return row;
        var before = transferred ? previousBytes ?? MemorySize.RowBytes(row) : 0;
        if (touched is not null) row = row.Select((value, slot) => touched.Contains(slot) ? value : SqlValue.Null(value.Type)).ToArray();
        var bytes = MemorySize.RowBytes(row);
        if (bytes > before) meter.Memory.Reserve(bytes - before); else meter.Memory.Release(before - bytes);
        rows[row] = Meter.Add(rows.GetValueOrDefault(row), bytes); held = Meter.Add(held, bytes);
        return row;
    }
    internal void Transfer(SqlValue[] row)
    {
        if (!rows.TryGetValue(row, out var total)) return;
        var bytes = MemorySize.RowBytes(row);
        if (total == bytes) rows.Remove(row); else rows[row] = total - bytes;
        held -= bytes;
    }
    internal void Reserve(SqlValue[] row)
    {
        if (!meter.Memory.Enabled) return;
        var bytes = meter.Memory.ReserveRow(row);
        rows[row] = Meter.Add(rows.GetValueOrDefault(row), bytes);
        held = Meter.Add(held, bytes);
    }
    internal void AdoptBytes(long bytes) { if (meter.Memory.Enabled) held = Meter.Add(held, bytes); }
    internal void TransferBytes(long bytes) { if (meter.Memory.Enabled) held -= bytes; }
    internal void ReserveBytes(long bytes) { meter.Memory.Reserve(bytes); held = Meter.Add(held, bytes); }
    internal void ReserveValue(SqlValue value)
    {
        if (meter.Memory.Enabled) held = Meter.Add(held, meter.Memory.ReserveValue(value));
    }
    internal void Release(SqlValue[] row)
    {
        if (!rows.TryGetValue(row, out var total)) return;
        var bytes = MemorySize.RowBytes(row);
        if (total == bytes) rows.Remove(row); else rows[row] = total - bytes;
        meter.Memory.Release(bytes); held -= bytes;
    }
    public void Dispose() { meter.Memory.Release(held); held = 0; rows.Clear(); }
}
