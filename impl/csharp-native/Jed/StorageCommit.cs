using System.Buffers.Binary;

namespace Jed;

internal sealed record StorageWrite(uint Page, byte[] Bytes);
internal sealed record StorageCommitPlan(StorageRecovered Before, StorageMeta Meta,
    StorageWrite[] Body, StorageWrite[] Manifest, byte[] MetaBytes,
    SortedSet<uint> FreePages, HashSet<uint> FreeListPages, HashSet<uint> ProtectedPages)
{
    internal StorageEncryptedMeta? Encrypted { get; init; }
}

// The relational storage serializer owns reachability and passes exact live-page
// deltas/reclaimed identities. This allocator knows only pages already safe for
// reuse; newly orphaned pages are persisted for the NEXT transaction, never reused
// by the transaction that first orphans them.
internal sealed class StorageAllocation
{
    private readonly StorageRecovered before;
    private readonly SortedSet<uint> safe;
    private readonly SortedSet<uint> remaining;
    private readonly SortedDictionary<uint, byte[]> body = [];
    private uint high;
    private readonly StoragePageCodec? codec;

    internal StorageAllocation(StorageRecovered before, IReadOnlySet<uint>? reuseGrant = null, StoragePageCodec? codec = null)
    {
        this.before = before; this.codec = codec;
        if ((before.Encrypted is not null) != (codec is not null)) throw new ArgumentException("storage codec differs from recovered variant");
        remaining = new(before.FreePages);
        safe = new(before.FreePages.Where(id => reuseGrant is null || reuseGrant.Contains(id)));
        // Housekeeping chains become unreachable when the alternate meta is
        // published. Persist them as retired, but never allocate from them in
        // this commit: the current recovery candidate still depends on them.
        remaining.UnionWith(before.FreeListPages);
        remaining.UnionWith(before.ManifestPages);
        high = before.Meta.PageCount;
    }

    internal uint AddPage(ReadOnlySpan<byte> page)
    {
        if (page.Length != before.Meta.PageSize) throw new ArgumentException("wrong encoded page size");
        var header = StorageFormat.ParsePage(page);
        if (header.Type is 7 or 8) throw new ArgumentException("allocator owns free-list and manifest pages");
        var id = ReservePage();
        body[id] = codec is null ? page.ToArray() : codec.SealBody(id, page);
        return id;
    }

    internal uint ReservePage()
    {
        var id = Draw(safe, remaining, ref high); body.Add(id, []); return id;
    }

    internal void SetPage(uint id, ReadOnlySpan<byte> page)
    {
        if (!body.ContainsKey(id) || page.Length != before.Meta.PageSize) throw new ArgumentException("page was not reserved or has wrong size");
        var header = StorageFormat.ParsePage(page);
        if (header.Type is 7 or 8) throw new ArgumentException("allocator owns free-list and manifest pages");
        body[id] = codec is null ? page.ToArray() : codec.SealBody(id, page);
    }

    internal int PageSize => before.Meta.PageSize;
    internal bool Encrypted => codec is not null;
    internal int PayloadCapacity => PageSize - 16 - (Encrypted ? StoragePageCodec.Reserve : 0);
    internal ReadOnlyMemory<byte> Tag(uint page) => !Encrypted ? default : page == 0 ? new byte[16] : body[page].AsMemory(PageSize - 16, 16);

    internal void Reclaim(uint id)
    {
        if (id < (Encrypted ? 4 : 2) || id >= before.Meta.PageCount || body.ContainsKey(id))
            throw new ArgumentException("invalid reclaimed page");
        remaining.Add(id);
    }

    internal StorageCommitPlan Prepare(uint root, uint livePages)
    {
        if (body.Count == 0 || !body.ContainsKey(root)) throw new ArgumentException("changed domain must write its catalog root");
        if (body.Values.Any(page => page.Length == 0)) throw new ArgumentException("reserved page has not been encoded");
        var size = before.Meta.PageSize;
        var txid = checked(before.Meta.TransactionId + 1);
        // Monotone reservation: freeing a reserved page from the persisted list
        // may reduce the number of free-list pages, but never shrink M again.
        var reserved = 0;
        while (true)
        {
            var available = new SortedSet<uint>(safe);
            var persist = new SortedSet<uint>(remaining);
            var next = high;
            var manifests = new uint[reserved];
            for (var i = 0; i < manifests.Length; i++) manifests[i] = Draw(available, persist, ref next);
            var freeIds = new List<uint>();
            var per = (PayloadCapacity - (Encrypted ? 16 : 0)) / 4;
            // A one-page residue is deliberately retained only in memory: using
            // it to store itself cannot make useful reusable space. It is found
            // again by a later reclamation walk (the peer allocation contract).
            if (persist.Count >= 2)
                while ((persist.Count + per - 1) / per > freeIds.Count)
                    freeIds.Add(Draw(available, persist, ref next));
            var inlineCapacity = Encrypted ? StorageCryptoMeta.InlineCapacity(size) : StorageFormat.InlineCapacity(size);
            var overflowEntries = Math.Max(0, body.Count + freeIds.Count - inlineCapacity);
            var required = (overflowEntries + StorageFormat.OverflowCapacity(size) - 1) / StorageFormat.OverflowCapacity(size);
            if (required > reserved) { reserved = required; continue; }
            var pages = body.Select(pair => new StorageWrite(pair.Key, pair.Value)).ToList();
            var persisted = persist.ToArray();
            byte[] nextTag = new byte[16];
            // Allocate in chain order, seal in reverse order so each successor's
            // stored tag exists before its predecessor draws its own nonce.
            for (var i = freeIds.Count - 1; i >= 0; i--)
            {
                var from = Math.Min(i * per, persisted.Length);
                var count = Math.Min(per, persisted.Length - from);
                var contentOffset = Encrypted ? 16 : 0; var payload = new byte[contentOffset + count * 4];
                if (Encrypted) nextTag.CopyTo(payload, 0);
                for (var j = 0; j < count; j++) BinaryPrimitives.WriteUInt32BigEndian(payload.AsSpan(contentOffset + j * 4), persisted[from + j]);
                var encoded = StorageFormat.EncodePage(size, new(7, 0, (uint)count, i + 1 < freeIds.Count ? freeIds[i + 1] : 0), payload);
                if (codec is not null) { encoded = codec.SealBody(freeIds[i], encoded); nextTag = encoded.AsSpan(size - 16).ToArray(); }
                pages.Add(new(freeIds[i], encoded));
            }
            var entries = pages.OrderBy(page => page.Page).Select(page =>
                new StorageManifestEntry(page.Page, StorageFormat.Crc64(page.Bytes))).ToArray();
            var overflow = new StorageWrite[reserved];
            var consumed = Math.Min(entries.Length, inlineCapacity);
            ulong chainDigest = 0;
            var idBytes = new byte[4];
            for (var i = 0; i < reserved; i++)
            {
                var count = Math.Min(StorageFormat.OverflowCapacity(size), entries.Length - consumed);
                var payload = new byte[16 + count * 12];
                BinaryPrimitives.WriteUInt64BigEndian(payload, txid);
                BinaryPrimitives.WriteUInt32BigEndian(payload.AsSpan(8), (uint)i);
                for (var j = 0; j < count; j++) StorageFormat.PutEntry(payload.AsSpan(16 + j * 12), entries[consumed + j]);
                var bytes = StorageFormat.EncodePage(size, new(8, 0, (uint)count, i + 1 < reserved ? manifests[i + 1] : 0), payload);
                overflow[i] = new(manifests[i], bytes);
                BinaryPrimitives.WriteUInt32BigEndian(idBytes, manifests[i]);
                chainDigest = StorageFormat.Crc64(bytes, StorageFormat.Crc64(idBytes, chainDigest));
                consumed += count;
            }
            var meta = new StorageMeta(size, txid, root, next, freeIds.Count > 0 ? freeIds[0] : 0, livePages,
                reserved > 0 ? manifests[0] : 0, (uint)entries.Length, (uint)reserved, chainDigest);
            StorageEncryptedMeta? encryptedMeta = null;
            byte[] metaBytes;
            if (codec is null) metaBytes = StorageFormat.EncodeMeta(meta, entries.AsSpan(0, Math.Min(entries.Length, inlineCapacity)));
            else
            {
                encryptedMeta = new(meta, Tag(root).ToArray(), nextTag);
                var unsigned = StorageCryptoMeta.Encode(meta, encryptedMeta.RootTag, nextTag, entries.AsSpan(0, Math.Min(entries.Length, inlineCapacity)));
                metaBytes = codec.SealMeta((uint)(1 - before.FloorSlot), unsigned, overflow.Select(page => new StorageManifestPage(page.Page, page.Bytes)).ToArray());
            }
            var dependencies = new HashSet<uint>(pages.Select(page => page.Page));
            dependencies.UnionWith(manifests);
            if (persist.Overlaps(dependencies)) throw new InvalidOperationException("free list overlaps commit dependencies");
            return new(before, meta, pages.OrderBy(page => page.Page).ToArray(), overflow, metaBytes, persist, new(freeIds), dependencies) { Encrypted = encryptedMeta };
        }
    }

    private static uint Draw(SortedSet<uint> safe, SortedSet<uint> persisted, ref uint high)
    {
        if (safe.Count > 0)
        {
            var result = safe.Min; safe.Remove(result); persisted.Remove(result); return result;
        }
        if (high == uint.MaxValue) throw new SqlException(SqlStates.ProgramLimitExceeded, "database page limit exceeded");
        return high++;
    }
}

// Durable synchronous-mode commit executor. Callers own writer admission and
// the short publication lock; no SQL snapshot is published until this returns.
// Relaxed batches, live-reader reclamation and encrypted sealing compose above
// this layer and are not implemented by pretending a write is already durable.
internal sealed class StorageCommitter
{
    private readonly StoragePager pager;
    private bool stabilize;
    private bool directoryPending = true;
    private int committing;

    internal StorageCommitter(StoragePager pager, StorageRecovered recovered)
    {
        this.pager = pager; Current = recovered; stabilize = recovered.RequiresStabilization;
    }

    internal StorageRecovered Current { get; private set; }
    internal bool IsPoisoned { get; private set; }

    internal void Adopt(StorageRecovered recovered, bool stabilized = false)
    {
        if (IsPoisoned || Volatile.Read(ref committing) != 0) throw new InvalidOperationException("cannot adopt into active or poisoned storage");
        Current = recovered; stabilize = !stabilized && recovered.RequiresStabilization;
    }

    internal async ValueTask CommitAsync(StorageCommitPlan plan, CancellationToken cancellationToken = default)
    {
        cancellationToken.ThrowIfCancellationRequested();
        if (IsPoisoned) throw new SqlException(SqlStates.IoError, "storage handle failed; close and reopen");
        if (!ReferenceEquals(plan.Before, Current)) throw new InvalidOperationException("stale storage commit plan");
        if (Interlocked.Exchange(ref committing, 1) != 0) throw new InvalidOperationException("overlapping storage commits");
        try
        {
            if (IsPoisoned) throw new SqlException(SqlStates.IoError, "storage handle failed; close and reopen");
            if (!ReferenceEquals(plan.Before, Current)) throw new InvalidOperationException("stale storage commit plan");
            // Preparation and all encoding checks happen before the guard. From
            // stabilization onward any failure must require recovery on reopen.
            try
            {
                if (directoryPending && pager.Store is FileBlockStore file) file.SyncDirectory();
                if (stabilize) await pager.Store.SyncAsync(cancellationToken).ConfigureAwait(false);
                var length = await pager.Store.GetLengthAsync(cancellationToken).ConfigureAwait(false);
                var needed = (long)plan.Meta.PageCount * pager.PageSize;
                if (needed > length)
                {
                    const long chunk = 1024 * 1024;
                    var allocation = checked((needed + chunk - 1) / chunk * chunk);
                    await pager.Store.SetLengthAsync(allocation, cancellationToken).ConfigureAwait(false);
                }
                foreach (var write in plan.Body.Concat(plan.Manifest))
                {
                    pager.Invalidate(write.Page);
                    await pager.Store.WriteAsync((long)write.Page * pager.PageSize, write.Bytes, cancellationToken).ConfigureAwait(false);
                }
                var slot = 1 - Current.FloorSlot;
                await pager.Store.WriteAsync((long)slot * pager.PageSize, plan.MetaBytes, cancellationToken).ConfigureAwait(false);
                await pager.Store.SyncAsync(cancellationToken).ConfigureAwait(false);
                Current = new(plan.Meta, slot, slot, plan.ProtectedPages, plan.FreePages, plan.FreeListPages)
                { ManifestPages = plan.Manifest.Select(page => page.Page).ToHashSet(), Encrypted = plan.Encrypted };
                stabilize = false; directoryPending = false;
            }
            catch { IsPoisoned = true; throw; }
        }
        finally { Volatile.Write(ref committing, 0); }
    }
}
