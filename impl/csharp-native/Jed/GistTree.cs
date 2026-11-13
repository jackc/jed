using System.Buffers.Binary;
using System.Runtime.CompilerServices;

namespace Jed;

internal sealed record GistEntry(GistBounds Bound, byte[] Key, GistNode? Child = null);
internal sealed record GistNode(GistEntry[] Entries)
{
    internal bool Leaf => Entries[0].Child is null;
    internal GistBounds Bound => Entries.Skip(1).Aggregate(Entries[0].Bound, (bound, entry) => bound.Union(entry.Bound));
    internal int Count => 1 + Entries.Sum(entry => entry.Child?.Count ?? 0);
}
internal sealed record GistTree(StorageTree Leaves, GistNode? Root, uint[] Pages)
{
    private static readonly ConditionalWeakTable<StorageTree, GistTree> Cache = new();
    internal static byte[] LeafKey(GistBounds bound, byte[] key)
    {
        var bytes = bound.Encode(); var writer = new WireWriter(); writer.U16(checked((ushort)bytes.Length)); writer.Bytes(bytes); writer.Bytes(key); return writer.ToArray();
    }
    internal static GistEntry LeafEntry(byte[] bytes, SqlType[] types)
    {
        var reader = new WireReader(bytes); var bound = GistBounds.Decode(reader.Bytes(reader.U16()), types); var key = reader.Bytes(reader.Remaining).ToArray();
        if (key.Length == 0) throw StorageFormat.Corrupt("empty GiST row key");
        return new(bound, key);
    }
    internal static async ValueTask<GistTree> BuildAsync(StorageTree leaves, SqlType[] types, CancellationToken token)
    {
        if (Cache.TryGetValue(leaves, out var cached)) return cached;
        var entries = new List<GistEntry>();
        await foreach (var row in leaves.ScanAsync(token).ConfigureAwait(false)) entries.Add(LeafEntry(row.Key, types));
        entries.Sort(CompareEntries); GistNode? root = null;
        foreach (var entry in entries)
        {
            token.ThrowIfCancellationRequested();
            var split = root is null ? new[] { new GistNode([entry]) } : Insert(root, entry);
            root = split.Length == 1 ? split[0] : new(split.Select(child => new GistEntry(child.Bound, [], child)).ToArray());
        }
        var result = new GistTree(leaves, root, []); return Cache.GetValue(leaves, _ => result);
    }
    private static GistNode[] Insert(GistNode node, GistEntry entry)
    {
        var entries = node.Entries.ToList();
        if (node.Leaf) entries.Add(entry);
        else
        {
            var selected = 0; var score = entries[0].Bound.Union(entry.Bound).Encode();
            for (var i = 1; i < entries.Count; i++)
            {
                var candidate = entries[i].Bound.Union(entry.Bound).Encode();
                if (candidate.AsSpan().SequenceCompareTo(score) < 0) { selected = i; score = candidate; }
            }
            var children = Insert(entries[selected].Child!, entry);
            entries.RemoveAt(selected);
            entries.InsertRange(selected, children.Select(child => new GistEntry(child.Bound, [], child)));
        }
        // Stable ties preserve child order before deterministic page allocation.
        var sorted = entries.Select((item, slot) => (item, slot)).OrderBy(pair => pair.item, Comparer<GistEntry>.Create(CompareEntries)).ThenBy(pair => pair.slot).Select(pair => pair.item).ToArray();
        return sorted.Length <= 4 ? [new(sorted)] : [new(sorted[..3]), new(sorted[3..])];
    }
    private static int CompareEntries(GistEntry a, GistEntry b)
    {
        var order = a.Bound.CompareTo(b.Bound); return order != 0 ? order : a.Key.AsSpan().SequenceCompareTo(b.Key);
    }
    internal byte[][] Probe(SqlValue[] queries, byte[] strategies, Meter? meter = null)
    {
        if (Root is null || queries.Any(query => query.IsNull)) return [];
        var keys = new List<byte[]>();
        void Visit(GistNode node)
        {
            meter?.Charge(Costs.PageRead + (node.Leaf ? 0 : Costs.GistDescent)); meter?.Checkpoint();
            foreach (var entry in node.Entries)
                if (entry.Bound.Matches(queries, strategies, node.Leaf))
                {
                    if (entry.Child is { } child) Visit(child); else keys.Add(entry.Key);
                }
        }
        Visit(Root); keys.Sort((a, b) => a.AsSpan().SequenceCompareTo(b)); return keys.ToArray();
    }
    internal async ValueTask<StoragePageReference> SerializeImageAsync(StorageImageWriter output, CancellationToken token)
    {
        async ValueTask<StoragePageReference> Write(GistNode node)
        {
            var writer = new WireWriter();
            foreach (var entry in node.Entries)
            {
                var bound = entry.Bound.Encode(); writer.U16(checked((ushort)bound.Length)); writer.Bytes(bound);
                if (entry.Child is { } child)
                {
                    var reference = await Write(child).ConfigureAwait(false);
                    writer.U32(reference.Page); if (output.Encrypted) writer.Bytes(reference.Tag.Span);
                }
                else { writer.U16(checked((ushort)entry.Key.Length)); writer.Bytes(entry.Key); }
            }
            if (writer.Length > output.PayloadCapacity) throw Errors.Unsupported("GiST node exceeds target page payload");
            return await output.WriteAsync(output.Reserve(), StorageFormat.EncodePage(output.PageSize,
                new(node.Leaf ? (byte)5 : (byte)6, 0, (uint)node.Entries.Length, 0), writer.ToArray()), token).ConfigureAwait(false);
        }
        return Root is null ? new(0, output.Encrypted ? new byte[16] : default) : await Write(Root).ConfigureAwait(false);
    }

    internal (StoragePageReference Root, uint[] Pages) Serialize(StorageAllocation allocation)
    {
        var pages = new List<uint>();
        StoragePageReference Write(GistNode node)
        {
            var writer = new WireWriter();
            foreach (var entry in node.Entries)
            {
                var bound = entry.Bound.Encode(); writer.U16(checked((ushort)bound.Length)); writer.Bytes(bound);
                if (entry.Child is { } child)
                {
                    var reference = Write(child); writer.U32(reference.Page); if (allocation.Encrypted) writer.Bytes(reference.Tag.Span);
                }
                else { writer.U16(checked((ushort)entry.Key.Length)); writer.Bytes(entry.Key); }
            }
            if (writer.Length > allocation.PayloadCapacity) throw StorageFormat.Corrupt("GiST node exceeds page payload");
            var id = allocation.ReservePage(); pages.Add(id);
            allocation.SetPage(id, StorageFormat.EncodePage(allocation.PageSize, new(node.Leaf ? (byte)5 : (byte)6, 0, (uint)node.Entries.Length, 0), writer.ToArray()));
            return new(id, allocation.Tag(id));
        }
        var root = Root is null ? new StoragePageReference(0, allocation.Encrypted ? new byte[16] : default) : Write(Root);
        return (root, pages.ToArray());
    }
    internal static async ValueTask<GistTree> LoadAsync(StoragePager pager, CatalogTable table, CatalogIndex index, StorageMeta meta, CancellationToken token)
    {
        var types = index.Keys.Select(key => table.Columns[key.Column].Type.Type).ToArray();
        var seen = new HashSet<uint>(); var leaves = new StorageTree(pager, new([]));
        async ValueTask<GistNode> Read(StoragePageReference reference, int depth)
        {
            if (depth > 128 || !seen.Add(reference.Page)) throw StorageFormat.Corrupt("cyclic GiST tree");
            var bytes = await pager.ReadPageAsync(reference, token).ConfigureAwait(false);
            var parsed = StorageGistPage.Parse(bytes.Span, meta.PageCount, pager.Encrypted);
            var entries = DecodePage(bytes.Span, parsed, types, pager.Encrypted);
            if (parsed.Header.Type == 5)
            {
                foreach (var entry in entries) leaves = await leaves.UpsertAsync(new(LeafKey(entry.Bound, entry.Key), []), token).ConfigureAwait(false);
            }
            else
            {
                for (var i = 0; i < entries.Length; i++)
                {
                    var child = await Read(parsed.Children[i], depth + 1).ConfigureAwait(false);
                    if (entries[i].Bound.CompareTo(child.Bound) != 0) throw StorageFormat.Corrupt("GiST bound differs from child union");
                    entries[i] = entries[i] with { Child = child };
                }
            }
            return new(entries);
        }
        var root = index.Root == 0 ? null : await Read(new(index.Root, index.RootTag), 0).ConfigureAwait(false);
        leaves = leaves.CommittedResidentCopy();
        var result = new GistTree(leaves, root, seen.Order().ToArray()); Cache.Add(leaves, result); return result;
    }
    private static GistEntry[] DecodePage(ReadOnlySpan<byte> bytes, StorageGistPage page, SqlType[] types, bool encrypted)
    {
        var reader = new WireReader(bytes.Slice(16, page.Used)); var entries = new GistEntry[page.Header.Count];
        for (var i = 0; i < entries.Length; i++)
        {
            var bound = GistBounds.Decode(reader.Bytes(reader.U16()), types);
            entries[i] = new(bound, page.Header.Type == 5 ? reader.Bytes(reader.U16()).ToArray() : []);
            if (page.Header.Type == 6) reader.Bytes(encrypted ? 20 : 4);
            if (i > 0 && CompareEntries(entries[i - 1], entries[i]) > 0) throw StorageFormat.Corrupt("unordered GiST entries");
        }
        reader.Finish(); return entries;
    }
}
