using System.Runtime.CompilerServices;

namespace Jed;

// The vector fold retains its measured table until the end of a row, then hands
// over to the ordinary group map and creation-order spool if either limit trips.
internal sealed class SpillGroupStore(SpillPolicy policy, Meter meter, bool lane, GroupPlan plan) : IAsyncDisposable
{
    internal readonly record struct Value(SpillGroup.State State, long Ordinal);
    private sealed class Entry(byte[] key, SpillGroup.State state, SqlValue[] values, long size, long charged, long ordinal)
    {
        internal readonly byte[] Key = key;
        internal readonly long Ordinal = ordinal;
        internal SpillGroup.State State = state;
        internal SqlValue[] Values = values;
        internal long Size = size, Charged = charged;
        internal void Update(SpillGroup.State state, SqlValue[] values, long size, long charged)
        { State = state; Values = values; Size = size; Charged = charged; }
    }
    private readonly Dictionary<byte[], Entry> resident = new(ByteKeyComparer.Instance);
    private readonly SpillKeyState map = new(policy.Host, policy.WorkMem, meter);
    private readonly SpillRowSpool keys = new(policy.Host, policy.WorkMem, meter);
    private bool handed = !lane, rejected, released, ordinarySpilled;
    private long held;
    private long count;
    internal async ValueTask<Value?> Get(byte[] key, CancellationToken token)
    {
        token.ThrowIfCancellationRequested();
        if (handed && (lane || ordinarySpilled))
        {
            var stored = await map.Get(key, token).ConfigureAwait(false);
            return stored is null ? null : new(SpillGroup.Decode(stored.Bytes, plan), stored.Ordinal);
        }

        return resident.TryGetValue(key, out var item)
            ? new Value(item.State, item.Ordinal) : null;
    }
    internal async ValueTask<long> Put(byte[] key, SpillGroup.State value, SqlValue[] keyValues, long bytes, CancellationToken token)
    {
        if (!lane && !ordinarySpilled)
            return await PutOrdinary(key, value, keyValues, bytes, token).ConfigureAwait(false);
        if (handed)
        {
            var fresh = !await map.Contains(key, token).ConfigureAwait(false);
            var ordinal = await map.Put(key, SpillGroup.Encode(value), bytes, token).ConfigureAwait(false);
            if (fresh) await keys.AppendOwned([SqlValue.Bytes(key)], token, MemorySize.RowBytes(keyValues)).ConfigureAwait(false);
            return ordinal;
        }
        resident.TryGetValue(key, out var old);
        var charged = old?.Charged ?? 0; var delta = bytes - charged;
        if (delta > 0 && !meter.Memory.TryReserve(delta)) rejected = true;
        else { if (delta < 0) meter.Memory.Release(-delta); held += delta; charged = bytes; }
        var position = old?.Ordinal ?? count++;
        if (old is null) resident.Add(key, new(key, value, keyValues, bytes, charged, position));
        else old.Update(value, keyValues, bytes, charged);
        return position;
    }
    // Ordinary folds have the same immediate admission and spool schedule as
    // SpillKeyState, but keep native fold objects until actual disk spill.
    private async ValueTask<long> PutOrdinary(byte[] key, SpillGroup.State value, SqlValue[] keyValues, long bytes, CancellationToken token)
    {
        token.ThrowIfCancellationRequested();
        if (bytes < 0) throw new ArgumentOutOfRangeException(nameof(bytes));
        resident.TryGetValue(key, out var old);
        var delta = bytes - (old?.Size ?? 0);
        var admitted = delta <= 0 || meter.Memory.TryReserve(delta);
        if (admitted)
        {
            if (delta < 0) meter.Memory.Release(-delta);
            held += delta;
            if (policy.WorkMem == 0 || held <= policy.WorkMem)
            {
                var position = old?.Ordinal ?? count++;
                if (old is null) resident.Add(key, new(key, value, keyValues, bytes, bytes, position));
                else old.Update(value, keyValues, bytes, bytes);
                if (old is null) await keys.AppendOwned([SqlValue.Bytes(key)], token, MemorySize.RowBytes(keyValues)).ConfigureAwait(false);
                return position;
            }
        }
        await map.ForceDisk(token).ConfigureAwait(false);
        foreach (var item in resident.Values.OrderBy(item => item.Ordinal))
            await map.Put(item.Key, SpillGroup.Encode(item.State), item.Size, token).ConfigureAwait(false);
        resident.Clear(); meter.Memory.Release(held); held = 0; ordinarySpilled = true;
        var ordinal = await map.Put(key, SpillGroup.Encode(value), bytes, token).ConfigureAwait(false);
        if (old is null) await keys.AppendOwned([SqlValue.Bytes(key)], token, MemorySize.RowBytes(keyValues)).ConfigureAwait(false);
        return ordinal;
    }
    internal async ValueTask EndRow(CancellationToken token)
    {
        if (handed || !rejected && (policy.WorkMem == 0 || held <= policy.WorkMem)) return;
        meter.Memory.Release(held); held = 0; handed = true;
        foreach (var item in resident.Values.OrderBy(item => item.Ordinal))
        {
            await map.Put(item.Key, SpillGroup.Encode(item.State), item.Size, token).ConfigureAwait(false);
            await keys.AppendOwned([SqlValue.Bytes(item.Key)], token, MemorySize.RowBytes(item.Values)).ConfigureAwait(false);
        }
        resident.Clear();
    }
    internal async IAsyncEnumerable<(byte[] Key, Value Value)> Read([EnumeratorCancellation] CancellationToken token)
    {
        if (!handed)
        {
            if (!released) { meter.Memory.Release(held); held = 0; released = true; }
            foreach (var item in resident.Values.OrderBy(item => item.Ordinal))
            { token.ThrowIfCancellationRequested(); yield return (item.Key, new(item.State, item.Ordinal)); }
        }
        else
            await foreach (var key in keys.Read(token).ConfigureAwait(false))
            {
                var bytes = key[0].AsBytes();
                if (!lane && !ordinarySpilled)
                {
                    var item = resident[bytes];
                    yield return (bytes, new(item.State, item.Ordinal));
                }
                else
                {
                    var stored = await map.Get(bytes, token).ConfigureAwait(false) ?? throw new InvalidOperationException("missing group state");
                    yield return (bytes, new(SpillGroup.Decode(stored.Bytes, plan), stored.Ordinal));
                }

            }
    }
    public async ValueTask DisposeAsync()
    {
        meter.Memory.Release(held); held = 0; resident.Clear();
        await map.DisposeAsync().ConfigureAwait(false); await keys.DisposeAsync().ConfigureAwait(false);
    }
}
