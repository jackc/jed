using System.Runtime.CompilerServices;

namespace Jed;

// Stable bounded merge sort. Binary run compaction bounds descriptors and
// metadata independently of the input row count. Scratch work accrues no cost.
internal sealed class ExternalSorter(ScratchHost host, long workMem, Meter meter, Comparison<SqlValue[]> compare) : IAsyncDisposable
{
    private sealed record Item(SqlValue[] Row, long Ordinal);
    private readonly List<Item> buffer = [];
    private readonly SpillRowSpool?[] levels = new SpillRowSpool[64];
    private long held;
    private long ordinal;
    private bool finished;
    private bool disposed;
    internal bool Spilled { get; private set; }
    internal long PeakResidentBytes { get; private set; }
    internal async ValueTask Add(SqlValue[] values, CancellationToken token)
    {
        token.ThrowIfCancellationRequested();
        ObjectDisposedException.ThrowIf(disposed, this);
        if (finished) throw new InvalidOperationException("sort already finished");
        var row = new SqlValue[values.Length];
        for (var i = 0; i < row.Length; i++) row[i] = await values[i].ResolveAsync(token).ConfigureAwait(false);
        var bytes = MemorySize.RowBytes(row); var admitted = meter.Memory.TryReserve(bytes);
        if (admitted) { held = Meter.Add(held, bytes); PeakResidentBytes = Math.Max(PeakResidentBytes, held); }
        buffer.Add(new(row, ordinal++));
        if (!admitted || workMem > 0 && held > workMem) await Flush(token).ConfigureAwait(false);
    }
    private void Sort() => buffer.Sort((a, b) => { var order = compare(a.Row, b.Row); return order == 0 ? a.Ordinal.CompareTo(b.Ordinal) : order; });
    private async ValueTask Flush(CancellationToken token)
    {
        if (buffer.Count == 0) return;
        Sort();
        SpillRowSpool? run = new(host, workMem, meter);
        try
        {
            await run.ForceDisk(token).ConfigureAwait(false);
            foreach (var row in buffer) await run.Append(row.Row, token).ConfigureAwait(false);
            buffer.Clear(); meter.Memory.Release(held); held = 0; Spilled = true;
            for (var level = 0; ; level++)
            {
                if (level == levels.Length) throw new SqlException(SqlStates.ProgramLimitExceeded, "too many sort rows");
                if (levels[level] is null) { levels[level] = run; run = null; return; }
                var earlier = levels[level]!; levels[level] = null;
                try
                {
                    var merged = await Merge(earlier, run, token).ConfigureAwait(false);
                    await run.DisposeAsync().ConfigureAwait(false); run = merged;
                }
                finally { await earlier.DisposeAsync().ConfigureAwait(false); }
            }
        }
        finally { if (run is not null) await run.DisposeAsync().ConfigureAwait(false); }
    }
    private async ValueTask<SpillRowSpool> Merge(SpillRowSpool left, SpillRowSpool right, CancellationToken token)
    {
        var result = new SpillRowSpool(host, workMem, meter);
        using var heads = new QueryInputMemory(meter);
        await using var a = left.Read(token).GetAsyncEnumerator(token);
        await using var b = right.Read(token).GetAsyncEnumerator(token);
        try
        {
            await result.ForceDisk(token).ConfigureAwait(false);
            var hasA = await a.MoveNextAsync().ConfigureAwait(false); var hasB = await b.MoveNextAsync().ConfigureAwait(false);
            if (hasA) heads.TryReserve(a.Current); if (hasB) heads.TryReserve(b.Current);
            while (hasA || hasB)
            {
                if (hasA && (!hasB || compare(a.Current, b.Current) <= 0))
                {
                    await result.Append(a.Current, token).ConfigureAwait(false); var previous = a.Current;
                    hasA = await a.MoveNextAsync().ConfigureAwait(false); if (hasA) heads.TryReserve(a.Current);
                    heads.Release(previous);
                }
                else
                {
                    await result.Append(b.Current, token).ConfigureAwait(false); var previous = b.Current;
                    hasB = await b.MoveNextAsync().ConfigureAwait(false); if (hasB) heads.TryReserve(b.Current);
                    heads.Release(previous);
                }
            }
            return result;
        }
        catch { await result.DisposeAsync().ConfigureAwait(false); throw; }
    }
    internal async IAsyncEnumerable<SqlValue[]> Read([EnumeratorCancellation] CancellationToken token = default)
    {
        ObjectDisposedException.ThrowIf(disposed, this);
        if (finished) throw new InvalidOperationException("sort output already consumed");
        finished = true;
        if (!Spilled)
        {
            Sort(); foreach (var item in buffer) { token.ThrowIfCancellationRequested(); yield return item.Row; }
            yield break;
        }
        await Flush(token).ConfigureAwait(false);
        var runs = new List<SpillRowSpool>();
        var readers = new List<IAsyncEnumerator<SqlValue[]>>();
        var heap = new List<int>();
        using var heads = new QueryInputMemory(meter);
        int CompareRuns(int left, int right)
        {
            var order = compare(readers[left].Current, readers[right].Current);
            return order == 0 ? left.CompareTo(right) : order;
        }
        try
        {
            // Higher levels precede lower levels in original input order.
            // The final merge retains one charged-or-rejected head per run.
            for (var level = levels.Length - 1; level >= 0; level--)
            {
                if (levels[level] is not { } run) continue;
                levels[level] = null; runs.Add(run);
                var reader = run.Read(token).GetAsyncEnumerator(token); readers.Add(reader);
                if (await reader.MoveNextAsync().ConfigureAwait(false))
                { heads.TryReserve(reader.Current); heap.Add(readers.Count - 1); }
            }
            while (heap.Count != 0)
            {
                heap.Sort(CompareRuns);
                var slot = heap[0]; var reader = readers[slot]; var row = reader.Current;
                if (await reader.MoveNextAsync().ConfigureAwait(false)) heads.TryReserve(reader.Current);
                else heap.RemoveAt(0);
                heads.Release(row);
                yield return row;
            }
        }
        finally
        {
            foreach (var reader in readers) await reader.DisposeAsync().ConfigureAwait(false);
            foreach (var run in runs) await run.DisposeAsync().ConfigureAwait(false);
        }
    }
    public async ValueTask DisposeAsync()
    {
        if (disposed) return; disposed = true;
        buffer.Clear(); meter.Memory.Release(held); held = 0;
        foreach (var run in levels) if (run is not null) await run.DisposeAsync().ConfigureAwait(false);
        Array.Clear(levels);
    }
}
