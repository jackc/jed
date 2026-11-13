using System.Buffers.Binary;

namespace Jed;

// The database uses the same publication gate for its committed catalog and
// this ledger. Local and remote pins must both enter here before a reuse grant
// is computed; merely pinning in the transport would miss local readers.
internal sealed class PageOwnerLedger
{
    private sealed record Commit(ulong TransactionId, uint[] Written, ulong Digest);
    private readonly object gate;
    private readonly byte[] incarnation;
    private readonly Queue<Commit> commits = new();
    private readonly SortedDictionary<ulong, int> pins = [];
    private byte[] meta;
    private ulong transactionId;
    private ulong invalidationFloor;
    private uint pageCount;
    private TaskCompletionSource? publication;
    internal object PublicationGate => gate;
    internal ulong CurrentTransactionId { get { lock (gate) return transactionId; } }
    internal int PinCount { get { lock (gate) return pins.Values.Sum(); } }
    internal ulong? OldestPin { get { lock (gate) return pins.Count == 0 ? null : pins.First().Key; } }
    internal int PageSize => meta.Length;
    internal byte[] Incarnation => incarnation.ToArray();

    internal PageOwnerLedger(object publicationGate, byte[] incarnation, byte[] meta, byte[]? previousMeta = null)
    {
        if (incarnation.Length != 16) throw new ArgumentException("invalid incarnation");
        gate = publicationGate; this.incarnation = incarnation.ToArray(); this.meta = meta.ToArray();
        (transactionId, pageCount) = Header(meta); invalidationFloor = transactionId;
        if (previousMeta is not null)
        {
            var previous = Header(previousMeta);
            if (previous.TransactionId != ulong.MaxValue && previous.TransactionId + 1 == transactionId)
                commits.Enqueue(new(previous.TransactionId, [], ResolutionDigest(previousMeta)));
        }
        commits.Enqueue(new(transactionId, [], ResolutionDigest(meta)));
    }
    internal PageOwnerPin Pin(byte[] cachedIncarnation, ulong lastTransactionId)
    {
        lock (gate)
        {
            var all = !incarnation.AsSpan().SequenceEqual(cachedIncarnation) || lastTransactionId == 0 || lastTransactionId > transactionId ||
                lastTransactionId < invalidationFloor;
            var indices = new SortedSet<uint>();
            if (!all)
            {
                foreach (var commit in commits)
                    if (commit.TransactionId > lastTransactionId)
                    {
                        foreach (var index in commit.Written) indices.Add(index);
                        if (indices.Count > 65536) { all = true; break; }
                    }
            }
            pins.TryGetValue(transactionId, out var count); pins[transactionId] = checked(count + 1);
            return new(this, new(incarnation.ToArray(), transactionId, all, all ? [] : indices.ToArray(), meta.ToArray()), pageCount);
        }
    }
    private void Unpin(ulong version)
    {
        lock (gate)
        {
            if (!pins.TryGetValue(version, out var count)) throw new InvalidOperationException("unknown page-service pin");
            if (count == 1) pins.Remove(version); else pins[version] = count - 1;
        }
    }
    // Call exactly at commit Check C / the local commit's point of no return.
    // Status requests await publication, even if their connection differed.
    internal void BeginPublication()
    {
        lock (gate)
        {
            if (publication is not null) throw new InvalidOperationException("overlapping publication");
            publication = new(TaskCreationOptions.RunContinuationsAsynchronously);
        }
    }
    internal void Publish(byte[] nextMeta, IEnumerable<uint> written)
    {
        var next = Header(nextMeta); var ordered = written.Distinct().Order().ToArray();
        lock (gate)
        {
            if (transactionId == ulong.MaxValue || next.TransactionId != transactionId + 1) throw new InvalidOperationException("nonsequential page-service publication");
            meta = nextMeta.ToArray(); transactionId = next.TransactionId; pageCount = next.PageCount;
            commits.Enqueue(new(transactionId, ordered, ResolutionDigest(meta)));
            while (commits.Count > 256) invalidationFloor = Math.Max(invalidationFloor, commits.Dequeue().TransactionId);
            var pending = publication; publication = null; pending?.TrySetResult();
        }
    }
    internal void FailPublication(Exception error)
    {
        lock (gate) { var pending = publication; publication = null; pending?.TrySetException(error); }
    }
    internal async ValueTask<byte> CommitStatusAsync(ulong version, ulong digest, CancellationToken cancellationToken = default)
    {
        while (true)
        {
            Task? wait;
            lock (gate)
            {
                wait = publication?.Task;
                if (wait is null)
                {
                    foreach (var commit in commits) if (commit.TransactionId == version) return commit.Digest == digest ? (byte)0 : (byte)1;
                    return version > transactionId ? (byte)1 : (byte)2;
                }
            }
            await wait.WaitAsync(cancellationToken).ConfigureAwait(false);
        }
    }
    internal bool CanReuse(ulong freeListGeneration)
    { lock (gate) return pins.Count == 0 || pins.First().Key >= freeListGeneration; }
    internal static ulong ResolutionDigest(ReadOnlySpan<byte> bytes)
    {
        if (bytes.Length > 7 && bytes[6] != 0)
        {
            var canonical = bytes.ToArray(); canonical.AsSpan(32, 4).Clear(); canonical.AsSpan(canonical.Length - 40).Clear();
            return StorageFormat.Crc64(canonical);
        }
        return StorageFormat.Crc64(bytes);
    }
    private static (ulong TransactionId, uint PageCount) Header(ReadOnlySpan<byte> bytes)
    {
        if (!StorageFormat.ValidPageSize(bytes.Length) || !bytes[..4].SequenceEqual("JEDB"u8) ||
            BinaryPrimitives.ReadUInt16BigEndian(bytes[4..]) != StorageFormat.Version ||
            BinaryPrimitives.ReadUInt32BigEndian(bytes[8..]) != bytes.Length) throw new ArgumentException("invalid published meta");
        return (BinaryPrimitives.ReadUInt64BigEndian(bytes[12..]), BinaryPrimitives.ReadUInt32BigEndian(bytes[24..]));
    }
    internal sealed class PageOwnerPin(PageOwnerLedger owner, PageReadSnapshot snapshot, uint pageCount) : IDisposable
    {
        private int disposed;
        internal PageReadSnapshot Snapshot { get; } = snapshot;
        internal uint PageCount { get; } = pageCount;
        public void Dispose() { if (Interlocked.Exchange(ref disposed, 1) == 0) owner.Unpin(Snapshot.TransactionId); }
    }
}
