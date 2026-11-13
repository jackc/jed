using System.Buffers.Binary;

namespace Jed;

// A streamed delta validator retains only bounded manifest metadata and the
// received identity set. The owner writes each page only after Check B; it never
// collects the entire dirty body in memory. Publication remains the adapter's job.
internal sealed class PageCommitValidator
{
    private readonly Dictionary<uint, ulong> listed = [];
    private readonly HashSet<uint> received = [];
    private readonly HashSet<uint> pending;
    private readonly PageWirePage[] manifest;
    private readonly byte[] metaBytes;
    private readonly bool encrypted;
    internal StorageMeta Meta { get; }
    internal StorageEncryptedMeta? EncryptedMeta => encrypted ? StorageCryptoMeta.Parse(metaBytes, long.MaxValue) : null;
    internal IReadOnlyDictionary<uint, ulong> Entries => listed;
    internal IReadOnlyList<PageWirePage> Manifest => manifest;
    internal IEnumerable<uint> WrittenIndices => listed.Keys.Concat(manifest.Select(page => page.Index));

    // Check A: before the first manifest write. before/pending/granted are fixed
    // by begin_write, never read again from changing owner state mid-delta.
    internal PageCommitValidator(StorageMeta before, bool encrypted, IReadOnlySet<uint> granted,
        IReadOnlyDictionary<uint, ulong> pending, ulong transactionId, byte[] meta, PageWirePage[] manifest,
        StoragePageCodec? codec = null)
    {
        this.encrypted = encrypted; this.pending = new(pending.Keys);
        if (before.TransactionId == ulong.MaxValue || transactionId != before.TransactionId + 1 || meta.Length != before.PageSize)
            throw PageWire.Invalid("commit transaction or page size differs");
        Meta = (encrypted ? StorageCryptoMeta.Parse(meta, long.MaxValue)?.Header : StorageFormat.ParseMeta(meta, long.MaxValue))
            ?? throw PageWire.Invalid("invalid commit meta");
        if (Meta.TransactionId != transactionId || Meta.PageCount < before.PageCount || Meta.ManifestEntries == 0 || Meta.ManifestPages != manifest.Length)
            throw PageWire.Invalid("invalid commit generation or manifest length");
        var minimum = encrypted ? 4u : 2u;
        bool Legal(uint index) => index >= minimum && index < Meta.PageCount && (index >= before.PageCount || granted.Contains(index));
        uint last = minimum - 1;
        void Entries(ReadOnlySpan<byte> bytes, uint count)
        {
            for (var i = 0; i < count; i++)
            {
                var entry = StorageFormat.ReadEntry(bytes.Slice(i * 12, 12));
                if (entry.Page <= last || entry.Page < minimum || entry.Page >= Meta.PageCount) throw PageWire.Invalid("invalid manifest entry order or bounds");
                if (pending.TryGetValue(entry.Page, out var prior))
                { if (prior != entry.Digest) throw PageWire.Invalid("pending page digest differs"); }
                else if (!Legal(entry.Page)) throw PageWire.Invalid("manifest entry not granted for writing");
                listed.Add(entry.Page, entry.Digest); last = entry.Page;
            }
        }
        var inline = Math.Min(Meta.ManifestEntries, (uint)(encrypted ? StorageCryptoMeta.InlineCapacity(meta.Length) : StorageFormat.InlineCapacity(meta.Length)));
        Entries(meta.AsSpan(encrypted ? 96 : 64), inline);
        var consumed = inline; var next = Meta.ManifestHead; ulong chainDigest = 0;
        var manifestIds = new HashSet<uint>(); var idBytes = new byte[4];
        foreach (var (page, ordinal) in manifest.Select((page, ordinal) => (page, ordinal)))
        {
            if (page.Index != next || !Legal(page.Index) || !manifestIds.Add(page.Index) || page.Bytes.Length != before.PageSize)
                throw PageWire.Invalid("invalid manifest chain identity");
            StoragePageHeader header;
            try { header = StorageFormat.ParsePage(page.Bytes); }
            catch (SqlException error) when (error.SqlState == SqlStates.DataCorrupted) { throw PageWire.Invalid("invalid manifest page"); }
            var expected = Math.Min(Meta.ManifestEntries - consumed, (uint)StorageFormat.OverflowCapacity(before.PageSize));
            if (header.Type != 8 || header.Count != expected || BinaryPrimitives.ReadUInt64BigEndian(page.Bytes.AsSpan(16)) != transactionId ||
                BinaryPrimitives.ReadUInt32BigEndian(page.Bytes.AsSpan(24)) != ordinal || !StorageFormat.IsZero(page.Bytes.AsSpan(28, 4)) ||
                !StorageFormat.IsZero(page.Bytes.AsSpan(32 + (int)expected * 12))) throw PageWire.Invalid("invalid manifest chain contents");
            Entries(page.Bytes.AsSpan(32), expected); consumed += expected; next = header.Next;
            BinaryPrimitives.WriteUInt32BigEndian(idBytes, page.Index);
            chainDigest = StorageFormat.Crc64(page.Bytes, StorageFormat.Crc64(idBytes, chainDigest));
        }
        if (next != 0 || consumed != Meta.ManifestEntries || chainDigest != Meta.ManifestDigest ||
            !listed.ContainsKey(Meta.Root) || (Meta.FreeListHead != 0 && !listed.ContainsKey(Meta.FreeListHead)) ||
            manifestIds.Overlaps(listed.Keys) || pending.Keys.Any(index => !listed.ContainsKey(index)))
            throw PageWire.Invalid("incomplete or overlapping commit manifest");
        if (encrypted)
        {
            if (codec is null) throw new ArgumentNullException(nameof(codec));
            var authenticated = manifest.Select(page => new StorageManifestPage(page.Index, page.Bytes)).ToArray();
            try { codec.AuthenticateMeta(0, meta, authenticated); }
            catch (SqlException first) when (first.SqlState == SqlStates.DataCorrupted)
            {
                try { codec.AuthenticateMeta(1, meta, authenticated); }
                catch (SqlException second) when (second.SqlState == SqlStates.DataCorrupted) { throw PageWire.Invalid("commit meta does not authenticate under either slot"); }
            }
        }
        metaBytes = meta.ToArray(); this.manifest = manifest.Select(page => new PageWirePage(page.Index, page.Bytes.ToArray())).ToArray();
    }
    // Check B is called immediately before each write, including every page in
    // a frame. A duplicate must not overwrite a previously checked page.
    internal void Receive(uint index, ReadOnlySpan<byte> page)
    {
        if (page.Length != Meta.PageSize || !listed.TryGetValue(index, out var digest) || pending.Contains(index) || received.Contains(index))
            throw PageWire.Invalid("unexpected or duplicate commit body page");
        if (StorageFormat.Crc64(page) != digest) throw PageWire.Invalid("commit page digest differs");
        if (!encrypted)
        {
            try { if (StorageFormat.ParsePage(page).Type == 8) throw PageWire.Invalid("manifest sent as body page"); }
            catch (SqlException error) when (error.SqlState == SqlStates.DataCorrupted) { throw PageWire.Invalid("invalid commit body header or checksum"); }
        }
        received.Add(index);
    }
    internal void Complete()
    {
        if (received.Count != listed.Count - pending.Count) throw PageWire.Invalid("commit body is incomplete");
    }
    internal byte[] MetaForSlot(uint slot, StoragePageCodec? codec = null)
    {
        if (slot > 1) throw new ArgumentOutOfRangeException(nameof(slot));
        if (!encrypted) return metaBytes.ToArray();
        if (codec is null) throw new ArgumentNullException(nameof(codec));
        return codec.SealMeta(slot, metaBytes, manifest.Select(page => new StorageManifestPage(page.Index, page.Bytes)).ToArray());
    }
}
