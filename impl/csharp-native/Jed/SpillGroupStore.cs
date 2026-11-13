using System.Runtime.CompilerServices;

namespace Jed;

// The vector fold retains its measured table until the end of a row, then hands
// over to the ordinary group map and creation-order spool if either limit trips.
internal sealed class SpillGroupStore(SpillPolicy policy, Meter meter, bool lane) : IAsyncDisposable
{
    private sealed record Entry(byte[] Key, byte[] Bytes, SqlValue[] Values, long Size, long Charged, long Ordinal);
    private readonly Dictionary<string, Entry> resident = new(StringComparer.Ordinal);
    private readonly SpillKeyState map = new(policy.Host, policy.WorkMem, meter);
    private readonly SpillRowSpool keys = new(policy.Host, policy.WorkMem, meter);
    private bool handed = !lane, rejected, released;
    private long held;
    private long count;
    internal ValueTask<SpillKeyState.Value?> Get(byte[] key, CancellationToken token)
    {
        if (handed) return map.Get(key, token);
        return ValueTask.FromResult(resident.TryGetValue(Convert.ToBase64String(key), out var item)
            ? new SpillKeyState.Value(item.Bytes, item.Ordinal) : null);
    }
    internal async ValueTask<long> Put(byte[] key, byte[] value, SqlValue[] keyValues, long bytes, CancellationToken token)
    {
        if (handed)
        {
            var fresh = await map.Get(key, token).ConfigureAwait(false) is null;
            var ordinal = await map.Put(key, value, bytes, token).ConfigureAwait(false);
            if (fresh) await keys.Append([SqlValue.Bytes(key)], token, MemorySize.RowBytes(keyValues)).ConfigureAwait(false);
            return ordinal;
        }
        var identity = Convert.ToBase64String(key); resident.TryGetValue(identity, out var old);
        var charged = old?.Charged ?? 0; var delta = bytes - charged;
        if (delta > 0 && !meter.Memory.TryReserve(delta)) rejected = true;
        else { if (delta < 0) meter.Memory.Release(-delta); held += delta; charged = bytes; }
        var position = old?.Ordinal ?? count++;
        resident[identity] = new(key, value, keyValues, bytes, charged, position);
        return position;
    }
    internal async ValueTask EndRow(CancellationToken token)
    {
        if (handed || !rejected && (policy.WorkMem == 0 || held <= policy.WorkMem)) return;
        meter.Memory.Release(held); held = 0; handed = true;
        foreach (var item in resident.Values.OrderBy(item => item.Ordinal))
        {
            await map.Put(item.Key, item.Bytes, item.Size, token).ConfigureAwait(false);
            await keys.Append([SqlValue.Bytes(item.Key)], token, MemorySize.RowBytes(item.Values)).ConfigureAwait(false);
        }
        resident.Clear();
    }
    internal async IAsyncEnumerable<(byte[] Key, SpillKeyState.Value Value)> Read([EnumeratorCancellation] CancellationToken token)
    {
        if (!handed)
        {
            if (!released) { meter.Memory.Release(held); held = 0; released = true; }
            foreach (var item in resident.Values.OrderBy(item => item.Ordinal))
            { token.ThrowIfCancellationRequested(); yield return (item.Key, new(item.Bytes, item.Ordinal)); }
        }
        else
            await foreach (var key in keys.Read(token).ConfigureAwait(false))
            {
                var bytes = key[0].AsBytes();
                yield return (bytes, await map.Get(bytes, token).ConfigureAwait(false) ?? throw new InvalidOperationException("missing group state"));
            }
    }
    public async ValueTask DisposeAsync()
    {
        meter.Memory.Release(held); held = 0; resident.Clear();
        await map.DisposeAsync().ConfigureAwait(false); await keys.DisposeAsync().ConfigureAwait(false);
    }
}
