using System.Security.Cryptography;

namespace Jed;

internal readonly record struct StoragePageReference(uint Page, ReadOnlyMemory<byte> Tag);

// Immutable page buffers permit eviction while a reader holds a buffer. A reused
// id must be invalidated before its write; reader watermarks are the caller's
// stronger guarantee that a live snapshot never observes that reuse.
internal sealed class StoragePager(IBlockStore store, int pageSize, long cacheBytes = 256L * 1024 * 1024, StoragePageCodec? codec = null)
{
    private sealed class Entry(uint id, byte[] bytes, byte[] tag)
    {
        internal readonly uint Id = id;
        internal readonly byte[] Bytes = bytes;
        internal readonly byte[] Tag = tag;
        internal bool Referenced = true;
    }

    private readonly object gate = new();
    private readonly Dictionary<uint, Entry> resident = [];
    private readonly Dictionary<uint, TaskCompletionSource<Entry>> loading = [];
    private readonly LinkedList<Entry> clock = [];
    private LinkedListNode<Entry>? hand;
    private int revoked;
    private readonly long capacity = StorageFormat.ValidPageSize(pageSize)
        ? Math.Max(1, cacheBytes / pageSize) : throw new ArgumentOutOfRangeException(nameof(pageSize));

    internal IBlockStore Store => store;
    internal int PageSize => pageSize;
    internal bool Encrypted => codec is not null;
    internal StoragePageCodec? Codec => codec;
    internal int PayloadCapacity => pageSize - 16 - (Encrypted ? StoragePageCodec.Reserve : 0);
    internal int PointerSize => Encrypted ? 20 : 4;
    internal int ResidentPages { get { lock (gate) return resident.Count; } }
    internal void Revoke() => Interlocked.Exchange(ref revoked, 1);
    private void CheckLive()
    {
        if (Volatile.Read(ref revoked) != 0) throw new SqlException(SqlStates.IoError, "storage was revoked after a failed durable barrier");
    }

    internal ValueTask<ReadOnlyMemory<byte>> ReadPageAsync(uint id, CancellationToken cancellationToken = default) => ReadPageAsync(new StoragePageReference(id, default), cancellationToken);
    internal async ValueTask<ReadOnlyMemory<byte>> ReadPageAsync(StoragePageReference reference, CancellationToken cancellationToken = default)
    {
        cancellationToken.ThrowIfCancellationRequested();
        CheckLive();
        var id = reference.Page;
        if (id < (Encrypted ? 4 : 2)) throw StorageFormat.Corrupt("body page pointer names reserved slot");
        if (Encrypted && reference.Tag.Length != 16) throw StorageFormat.Corrupt("encrypted page pointer has no authentication tag");
        TaskCompletionSource<Entry> pending;
        var ownsLoad = false;
        lock (gate)
        {
            if (resident.TryGetValue(id, out var entry)) { CheckTag(entry); entry.Referenced = true; return entry.Bytes; }
            if (!loading.TryGetValue(id, out pending!))
            {
                pending = new(TaskCreationOptions.RunContinuationsAsynchronously);
                loading.Add(id, pending); ownsLoad = true;
            }
        }
        if (!ownsLoad)
        {
            var loaded = await pending.Task.WaitAsync(cancellationToken).ConfigureAwait(false); CheckTag(loaded); return loaded.Bytes;
        }
        try
        {
            var bytes = new byte[pageSize];
            await store.ReadAsync((long)id * pageSize, bytes, cancellationToken).ConfigureAwait(false);
            CheckLive();
            var tag = Encrypted ? bytes.AsSpan(bytes.Length - 16).ToArray() : [];
            if (codec is not null) bytes = codec.OpenBody(id, bytes, reference.Tag.Span);
            _ = StorageFormat.ParsePage(bytes, verifyChecksum: !Encrypted);
            var loaded = new Entry(id, bytes, tag);
            lock (gate)
            {
                // An invalidated in-flight result still serves its original
                // waiters, but cannot replace a new generation's cache entry.
                if (loading.TryGetValue(id, out var current) && ReferenceEquals(current, pending))
                {
                    loading.Remove(id); Evict();
                    resident.Add(id, loaded); clock.AddLast(loaded);
                    hand ??= clock.First;
                }
            }
            pending.SetResult(loaded);
            return loaded.Bytes;
        }
        catch (Exception error)
        {
            lock (gate)
                if (loading.TryGetValue(id, out var current) && ReferenceEquals(current, pending)) loading.Remove(id);
            if (error is OperationCanceledException canceled) pending.TrySetCanceled(canceled.CancellationToken);
            else pending.TrySetException(error);
            return (await pending.Task.ConfigureAwait(false)).Bytes;
        }
        void CheckTag(Entry entry)
        {
            CheckLive();
            if (Encrypted && !CryptographicOperations.FixedTimeEquals(entry.Tag, reference.Tag.Span)) throw StorageFormat.Corrupt("cached encrypted page pointer tag mismatch");
        }
    }

    internal void Invalidate(uint id)
    {
        lock (gate)
        {
            loading.Remove(id);
            if (!resident.Remove(id, out var entry)) return;
            var node = clock.Find(entry)!;
            if (ReferenceEquals(hand, node)) hand = node.Next ?? clock.First;
            clock.Remove(node);
            if (clock.Count == 0) hand = null;
        }
    }

    private void Evict()
    {
        while (resident.Count >= capacity)
        {
            hand ??= clock.First;
            var candidate = hand!;
            hand = candidate.Next ?? clock.First;
            if (candidate.Value.Referenced) { candidate.Value.Referenced = false; continue; }
            resident.Remove(candidate.Value.Id); clock.Remove(candidate);
            if (clock.Count == 0) hand = null;
        }
    }
}
