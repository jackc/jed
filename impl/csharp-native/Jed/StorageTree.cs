using System.Buffers.Binary;
using System.Runtime.CompilerServices;

namespace Jed;

internal sealed record StorageInterior(uint[] Children, byte[][] Separators)
{
    internal byte[][]? Tags { get; init; }
    internal static StorageInterior Parse(ReadOnlySpan<byte> page, uint pageCount = uint.MaxValue, bool encrypted = false)
    {
        var header = StorageFormat.ParsePage(page, verifyChecksum: !encrypted);
        var pointerSize = encrypted ? 20 : 4; var capacity = page.Length - 16 - (encrypted ? StoragePageCodec.Reserve : 0);
        if (header.Type != 3 || header.Count > (capacity - pointerSize) / (pointerSize + 4)) throw StorageFormat.Corrupt("invalid interior header");
        var count = (int)header.Count; var payload = page.Slice(16, capacity);
        var children = new uint[count + 1]; var keys = new byte[count][];
        var tags = encrypted ? new byte[count + 1][] : null;
        for (var i = 0; i < children.Length; i++)
        {
            children[i] = BinaryPrimitives.ReadUInt32BigEndian(payload[(i * pointerSize)..]);
            if (children[i] < (encrypted ? 4 : 2) || children[i] >= pageCount) throw StorageFormat.Corrupt("interior child out of bounds");
            if (tags is not null) tags[i] = payload.Slice(i * pointerSize + 4, 16).ToArray();
        }
        var directory = (count + 1) * pointerSize; var blob = directory + count * 4; uint previous = 0;
        for (var i = 0; i < count; i++)
        {
            var end = BinaryPrimitives.ReadUInt32BigEndian(payload[(directory + i * 4)..]);
            if (end <= previous || end > payload.Length - blob) throw StorageFormat.Corrupt("invalid separator directory");
            keys[i] = payload.Slice(blob + (int)previous, (int)(end - previous)).ToArray();
            if (i != 0 && keys[i - 1].AsSpan().SequenceCompareTo(keys[i]) >= 0) throw StorageFormat.Corrupt("unordered separators");
            previous = end;
        }
        if (!StorageFormat.IsZero(payload[(blob + (int)previous)..])) throw StorageFormat.Corrupt("interior nonzero padding");
        return new(children, keys) { Tags = tags };
    }

    internal byte[] Encode(int pageSize, bool encrypted = false)
    {
        if (Children.Length != Separators.Length + 1 || Children.Any(id => id < 2)) throw new ArgumentException("invalid interior children");
        var pointerSize = encrypted ? 20 : 4;
        if (encrypted && (Tags is null || Tags.Length != Children.Length || Tags.Any(tag => tag.Length != 16) || Children.Any(id => id < 4))) throw new ArgumentException("encrypted interior children require bound tags");
        var size = pointerSize + (pointerSize + 4L) * Separators.Length + Separators.Sum(key => (long)key.Length);
        if (size > pageSize - 16 - (encrypted ? StoragePageCodec.Reserve : 0)) throw Errors.Unsupported("oversize interior page");
        var payload = new byte[(int)size];
        for (var i = 0; i < Children.Length; i++)
        {
            BinaryPrimitives.WriteUInt32BigEndian(payload.AsSpan(i * pointerSize), Children[i]);
            if (encrypted) Tags![i].CopyTo(payload, i * pointerSize + 4);
        }
        var directory = Children.Length * pointerSize; var blob = directory + Separators.Length * 4; var end = 0;
        for (var i = 0; i < Separators.Length; i++)
        {
            var key = Separators[i];
            if (key.Length == 0 || (i != 0 && Separators[i - 1].AsSpan().SequenceCompareTo(key) >= 0)) throw new ArgumentException("unordered separators");
            key.CopyTo(payload, blob + end); end += key.Length;
            BinaryPrimitives.WriteUInt32BigEndian(payload.AsSpan(directory + i * 4), (uint)end);
        }
        return StorageFormat.EncodePage(pageSize, new(3, 0, (uint)Separators.Length, 0), payload);
    }
}

// An immutable COW tree. Clean children remain page identities, so a mutation
// retains only the paths it rewrites and scans keep only their traversal stack.
internal sealed partial class StorageTree
{
    private sealed record Ref(uint Page = 0, Node? Dirty = null, ReadOnlyMemory<byte> Tag = default, StorageTreeShape? Shape = null, Node? Resident = null);
    private sealed record Node(StorageRecord[]? Rows = null, StorageLeaf? Packed = null,
        Ref[]? Children = null, byte[][]? Keys = null)
    {
        internal bool IsLeaf => Children is null;
        internal int Count => IsLeaf ? (Rows?.Length ?? Packed!.Count) : Keys!.Length;
        internal StorageRecord[] Records() => Rows ?? Enumerable.Range(0, Packed!.Count).Select(Packed.Record).ToArray();
        internal StorageRecord Record(int i) => Rows is not null ? Rows[i] : Packed!.Record(i);
        internal byte[] Key(int i) => IsLeaf ? (Rows is not null ? Rows[i].Key : Packed!.Key(i)) : Keys![i];
    }
    private sealed record Split(Ref[] Nodes, byte[][] Keys);
    private readonly StoragePager pager;
    private readonly StorageLeafSchema schema;
    private readonly Ref? root;
    private readonly uint pageCount;
    private readonly HashSet<uint> retired;

    internal StorageTree(StoragePager pager, StorageLeafSchema schema, uint rootPage = 0, uint pageCount = uint.MaxValue, ReadOnlyMemory<byte> rootTag = default, StorageTreeShape? shape = null, StorageRecord[]? residentRoot = null)
        : this(pager, schema, rootPage == 0 ? null : new Ref(rootPage, Tag: rootTag, Shape: shape, Resident: residentRoot is null ? null : new(Rows: residentRoot)), pageCount, []) { }

    private StorageTree(StoragePager pager, StorageLeafSchema schema, Ref? root, uint pageCount, HashSet<uint> retired)
    {
        this.pager = pager; this.schema = schema; this.root = root; this.pageCount = pageCount; this.retired = retired;
    }

    internal StorageTreeShape? SerializedShape { get; private set; }
    internal StorageRecord[]? SerializedRootRecords { get; private set; }
    internal bool IsEmpty => root is null;
    internal int PageDelta => DirtyCount(root) - retired.Count;
    internal IReadOnlySet<uint> RetiredPages => retired;

    internal async ValueTask<StorageTree> ClearAsync(CancellationToken cancellationToken = default)
    {
        if (root is null) return new(pager, schema, null, pageCount, retired);
        var dead = new HashSet<uint>(retired); var seen = new HashSet<uint>();
        async ValueTask Walk(Ref reference, int depth)
        {
            CheckPath(reference, seen, depth);
            var node = await Load(reference, cancellationToken).ConfigureAwait(false);
            if (reference.Page != 0) dead.Add(reference.Page);
            if (node.IsLeaf)
                foreach (var row in node.Records()) await RetireChains(row, dead, cancellationToken).ConfigureAwait(false);
            else
                foreach (var child in node.Children!) await Walk(child, depth + 1).ConfigureAwait(false);
        }
        await Walk(root, 0).ConfigureAwait(false);
        return new(pager, schema, null, pageCount, dead);
    }

    internal async ValueTask<long> CountPagesAsync(CancellationToken cancellationToken = default)
    {
        var path = new HashSet<uint>();
        async ValueTask<long> Count(Ref reference, int depth)
        {
            CheckPath(reference, path, depth);
            try
            {
                var node = await Load(reference, cancellationToken).ConfigureAwait(false);
                long count = 1;
                if (node.IsLeaf)
                {
                    foreach (var row in node.Records())
                        for (var i = 0; i < row.Cells.Length; i++)
                            if (schema.Columns[i].Width == 0 && StorageLargeValues.External(row.Cells[i]))
                                count += StorageLargeValues.Slabs(StorageLargeValues.Inspect(row.Cells[i]!, pager.Encrypted).Stored,
                                    pager.PayloadCapacity - (pager.Encrypted ? 16 : 0));
                }
                else
                    foreach (var child in node.Children!) count = checked(count + await Count(child, depth + 1).ConfigureAwait(false));
                return count;
            }
            finally { if (reference.Page != 0) path.Remove(reference.Page); }
        }
        return root is null ? 0 : await Count(root, 0).ConfigureAwait(false);
    }

    internal async ValueTask<StorageRecord?> FindAsync(ReadOnlyMemory<byte> key, CancellationToken cancellationToken = default, Action? pageRead = null)
    {
        var current = root; var path = new HashSet<uint>(); var depth = 0;
        while (current is not null)
        {
            CheckPath(current, path, depth++);
            pageRead?.Invoke();
            var node = await Load(current, cancellationToken).ConfigureAwait(false);
            if (!node.IsLeaf) { current = node.Children![Route(node, key.Span)]; continue; }
            var at = LowerBound(node, key.Span);
            return at < node.Count && node.Key(at).AsSpan().SequenceEqual(key.Span) ? Clone(node.Record(at)) : null;
        }
        return null;
    }

    internal async IAsyncEnumerable<StorageRecord> ScanAsync([EnumeratorCancellation] CancellationToken cancellationToken = default, Action? pageRead = null)
    {
        if (root is null) yield break;
        await foreach (var row in Walk(root, new HashSet<uint>(), 0, cancellationToken, pageRead).ConfigureAwait(false)) yield return row;
    }

    private async IAsyncEnumerable<StorageRecord> Walk(Ref reference, HashSet<uint> path, int depth,
        [EnumeratorCancellation] CancellationToken cancellationToken, Action? pageRead)
    {
        CheckPath(reference, path, depth);
        try
        {
            pageRead?.Invoke();
            var node = await Load(reference, cancellationToken).ConfigureAwait(false);
            if (node.IsLeaf)
            {
                for (var i = 0; i < node.Count; i++) { cancellationToken.ThrowIfCancellationRequested(); yield return Clone(node.Record(i)); }
            }
            else
                foreach (var child in node.Children!)
                    await foreach (var row in Walk(child, path, depth + 1, cancellationToken, pageRead).ConfigureAwait(false)) yield return row;
        }
        finally { if (reference.Page != 0) path.Remove(reference.Page); }
    }

    internal async ValueTask<StorageTree> UpsertAsync(StorageRecord record, CancellationToken cancellationToken = default)
    {
        record = PrepareRecord(Clone(record));
        // Validate one record's admissibility before changing a pending path.
        _ = StorageLeaf.Encode(pager.PageSize, schema, [record], pager.Encrypted);
        var dead = new HashSet<uint>(retired);
        await RetainMovedChains(record, dead, cancellationToken).ConfigureAwait(false);
        var split = root is null ? new Split([new(Dirty: new(Rows: [record]))], []) :
            await Insert(root, record, dead, new HashSet<uint>(), 0, cancellationToken).ConfigureAwait(false);
        var height = 0;
        while (split.Nodes.Length > 1)
        {
            if (height++ > 64) throw Errors.Unsupported("tree height");
            split = Divide(new(Children: split.Nodes, Keys: split.Keys), -1, multiway: true)!;
        }
        return Written(new(pager, schema, split.Nodes[0], pageCount, dead), record);
    }

    private async ValueTask<Split> Insert(Ref reference, StorageRecord record, HashSet<uint> dead,
        HashSet<uint> path, int depth, CancellationToken token)
    {
        CheckPath(reference, path, depth);
        try
        {
            var node = await Load(reference, token).ConfigureAwait(false);
            Node changed; int edited;
            if (node.IsLeaf)
            {
                await RetireResidentChains(reference, dead, token).ConfigureAwait(false);
                var rows = node.Records().ToList(); edited = LowerBound(node, record.Key);
                if (edited < rows.Count && rows[edited].Key.AsSpan().SequenceEqual(record.Key))
                {
                    await RetireChains(rows[edited], dead, token, record).ConfigureAwait(false);
                    rows[edited] = record;
                }
                else rows.Insert(edited, record);
                changed = new(Rows: rows.ToArray());
            }
            else
            {
                var at = Route(node, record.Key);
                var child = await Insert(node.Children![at], record, dead, path, depth + 1, token).ConfigureAwait(false);
                var children = node.Children.ToList(); children.RemoveAt(at); children.InsertRange(at, child.Nodes);
                var keys = node.Keys!.ToList(); keys.InsertRange(at, child.Keys);
                edited = child.Keys.Length == 0 ? -1 : at + child.Keys.Length - 1;
                changed = new(Children: children.ToArray(), Keys: keys.ToArray());
            }
            if (reference.Page != 0) dead.Add(reference.Page);
            return Divide(changed, edited, multiway: true)!;
        }
        finally { if (reference.Page != 0) path.Remove(reference.Page); }
    }

    internal async ValueTask<StorageTree> DeleteAsync(ReadOnlyMemory<byte> key, CancellationToken cancellationToken = default)
    {
        if (root is null) return this;
        var dead = new HashSet<uint>(retired);
        var (updated, changed) = await Remove(root, key, dead, new HashSet<uint>(), 0, cancellationToken).ConfigureAwait(false);
        if (!changed) return this;
        while (updated is not null)
        {
            var node = await Load(updated, cancellationToken).ConfigureAwait(false);
            if (node.Count != 0) break;
            if (updated.Page != 0) dead.Add(updated.Page);
            updated = node.IsLeaf ? null : node.Children![0];
        }
        return Deleted(new(pager, schema, updated, pageCount, dead), key.Span);
    }

    private async ValueTask<(Ref Reference, bool Changed)> Remove(Ref reference, ReadOnlyMemory<byte> key, HashSet<uint> dead,
        HashSet<uint> path, int depth, CancellationToken token)
    {
        CheckPath(reference, path, depth);
        try
        {
            var node = await Load(reference, token).ConfigureAwait(false);
            if (node.IsLeaf)
            {
                var at = LowerBound(node, key.Span);
                if (at == node.Count || !node.Key(at).AsSpan().SequenceEqual(key.Span)) return (reference, false);
                await RetireResidentChains(reference, dead, token).ConfigureAwait(false);
                var rows = node.Records().ToList();
                await RetireChains(rows[at], dead, token).ConfigureAwait(false); rows.RemoveAt(at);
                if (reference.Page != 0) dead.Add(reference.Page);
                return (new(Dirty: new(Rows: rows.ToArray())), true);
            }
            var childIndex = Route(node, key.Span);
            var (childRef, changed) = await Remove(node.Children![childIndex], key, dead, path, depth + 1, token).ConfigureAwait(false);
            if (!changed) return (reference, false);
            var children = node.Children.ToList(); children[childIndex] = childRef;
            var keys = node.Keys!.ToList();
            var child = await Load(childRef, token).ConfigureAwait(false);
            if (keys.Count > 0 && !child.IsLeaf && await EmptyUnary(childRef, token).ConfigureAwait(false))
            {
                children.RemoveAt(childIndex); keys.RemoveAt(Math.Min(childIndex, keys.Count - 1));
            }
            else if (keys.Count > 0 && Payload(child) < pager.PayloadCapacity / 2)
            {
                var leftAt = childIndex + 1 < children.Count ? childIndex : childIndex - 1;
                var leftRef = children[leftAt]; var rightRef = children[leftAt + 1];
                var left = await Load(leftRef, token).ConfigureAwait(false);
                var right = await Load(rightRef, token).ConfigureAwait(false);
                if (left.IsLeaf != right.IsLeaf) throw StorageFormat.Corrupt("tree children have inconsistent heights");
                var merged = left.IsLeaf ? new Node(Rows: left.Records().Concat(right.Records()).ToArray()) :
                    new Node(Children: left.Children!.Concat(right.Children!).ToArray(),
                        Keys: left.Keys!.Concat([keys[leftAt]]).Concat(right.Keys!).ToArray());
                var replacement = Divide(merged, -1, multiway: false);
                // The merge can leave underfull nodes when no two-way split fits,
                // or its larger separator would overflow the parent on deletion.
                var parentSize = pager.PointerSize + (pager.PointerSize + 4L) * keys.Count + keys.Sum(item => (long)item.Length);
                if (replacement is not null && (replacement.Keys.Length == 0 ||
                    parentSize - keys[leftAt].Length + replacement.Keys[0].Length <= pager.PayloadCapacity))
                {
                    if (leftRef.Page != 0) dead.Add(leftRef.Page);
                    if (rightRef.Page != 0) dead.Add(rightRef.Page);
                    children.RemoveRange(leftAt, 2); children.InsertRange(leftAt, replacement.Nodes);
                    keys.RemoveAt(leftAt); keys.InsertRange(leftAt, replacement.Keys);
                }
            }
            if (reference.Page != 0) dead.Add(reference.Page);
            return (new(Dirty: new(Children: children.ToArray(), Keys: keys.ToArray())), true);
        }
        finally { if (reference.Page != 0) path.Remove(reference.Page); }
    }

    private async ValueTask<bool> EmptyUnary(Ref reference, CancellationToken token)
    {
        var path = new HashSet<uint>(); var depth = 0;
        while (true)
        {
            CheckPath(reference, path, depth++);
            var node = await Load(reference, token).ConfigureAwait(false);
            if (node.Count != 0) return false;
            if (node.IsLeaf) return true;
            reference = node.Children![0];
        }
    }

    private Split? Divide(Node node, int edited, bool multiway)
    {
        var size = Payload(node); var cap = pager.PayloadCapacity;
        if (size <= cap) return new([new(Dirty: node)], []);
        var count = node.Count;
        if ((!node.IsLeaf && count < 2) || (node.IsLeaf && count < 2)) throw Errors.Unsupported("entry or separator exceeds page size");
        var first = 1; var last = node.IsLeaf ? count - 1 : count == 2 ? 1 : count - 2;
        var min = -1; var max = -1; var balanced = -1;
        for (var m = first; m <= last; m++)
        {
            var left = Slice(node, 0, m); var right = Slice(node, node.IsLeaf ? m : m + 1, count);
            var leftSize = Payload(left); var rightSize = Payload(right);
            if (min < 0 && rightSize <= cap) min = m;
            if (leftSize <= cap) max = m;
            if (balanced < 0 && leftSize * 2 >= size) balanced = m;
        }
        if ((max < 0 || min < 0 || min > max) && !multiway) return null;
        if (max < 0) throw Errors.Unsupported("separator exceeds page size");
        var twoWay = min >= 0 && min <= max;
        var splitAt = twoWay ? (edited == count - 1 ? max : Math.Clamp(Math.Min(balanced < 0 ? last : balanced, max), min, max)) : max;
        var leftNode = Slice(node, 0, splitAt);
        var rightFrom = node.IsLeaf ? splitAt : splitAt + 1;
        var rightNode = Slice(node, rightFrom, count);
        var separator = node.IsLeaf ? node.Key(splitAt) : node.Keys![splitAt];
        if (Payload(leftNode) > cap) throw Errors.Unsupported("split node exceeds page size");
        if (twoWay) return new([new(Dirty: leftNode), new(Dirty: rightNode)], [separator]);
        var rest = Divide(rightNode, edited < rightFrom ? -1 : edited - rightFrom, multiway: true)!;
        return new(new[] { new Ref(Dirty: leftNode) }.Concat(rest.Nodes).ToArray(), new[] { separator }.Concat(rest.Keys).ToArray());
    }

    private static Node Slice(Node node, int from, int to) => node.IsLeaf
        ? new(Rows: node.Records()[from..to])
        : new(Children: node.Children![from..(to + 1)], Keys: node.Keys![from..to]);

    private long Payload(Node node)
    {
        if (!node.IsLeaf) return pager.PointerSize + (pager.PointerSize + 4L) * node.Count + node.Keys!.Sum(key => (long)key.Length);
        if (node.Count == 0) return 0;
        return StorageLeaf.MeasurePayload(schema, node.Records());
    }

    internal async ValueTask<uint> SerializeAsync(StorageAllocation allocation, CancellationToken cancellationToken = default)
        => (await SerializeBoundAsync(allocation, cancellationToken).ConfigureAwait(false)).Page;

    internal async ValueTask<StoragePageReference> SerializeBoundAsync(StorageAllocation allocation, CancellationToken cancellationToken = default)
    {
        if (allocation.Encrypted != pager.Encrypted) throw new ArgumentException("tree and allocator variants differ");
        foreach (var id in retired) allocation.Reclaim(id);
        if (root is null) { SerializedShape = null; SerializedRootRecords = null; return new(0, allocation.Encrypted ? new byte[16] : default); }
        var written = await Write(root, allocation, cancellationToken).ConfigureAwait(false);
        SerializedShape = written.Shape;
        SerializedRootRecords = written.Resident?.Records();
        return new(written.Page, written.Tag);
    }

    private async ValueTask<Ref> Write(Ref reference, StorageAllocation allocation, CancellationToken token)
    {
        token.ThrowIfCancellationRequested();
        if (reference.Dirty is not { } node) return reference;
        if (node.IsLeaf)
        {
            var id = allocation.ReservePage();
            var original = node.Records();
            var rows = original.Select(row => StorageLargeValues.WriteFresh(row, allocation)).ToArray();
            allocation.SetPage(id, StorageLeaf.Encode(pager.PageSize, schema, rows, pager.Encrypted));
            // Only the root's returned Resident payload survives publication;
            // interior serialization keeps child shapes and drops leaf rows.
            var retained = rows.Select((row, index) => row with { FreshChains = original[index].FreshChains }).ToArray();
            return new(id, Tag: allocation.Tag(id), Shape: new(id, allocation.Tag(id)), Resident: new(Rows: retained));
        }
        var children = new uint[node.Children!.Length];
        var shapes = new StorageTreeShape?[children.Length];
        var tags = pager.Encrypted ? new byte[children.Length][] : null;
        for (var i = 0; i < children.Length; i++)
        {
            var child = await Write(node.Children[i], allocation, token).ConfigureAwait(false); children[i] = child.Page; shapes[i] = child.Shape;
            if (tags is not null) tags[i] = child.Tag.ToArray();
        }
        var page = allocation.AddPage(new StorageInterior(children, node.Keys!) { Tags = tags }.Encode(pager.PageSize, pager.Encrypted));
        var shape = shapes.All(item => item is not null) ? new StorageTreeShape(page, allocation.Tag(page), node.Keys!, shapes.Select(item => item!).ToArray()) : null;
        return new(page, Tag: allocation.Tag(page), Shape: shape);
    }

    private async ValueTask<Node> Load(Ref reference, CancellationToken token)
    {
        token.ThrowIfCancellationRequested();
        if (reference.Dirty is { } node) return node;
        if (reference.Resident is { } resident) return resident;
        if (reference.Shape is { IsLeaf: false } shape) return ShapeNode(shape);
        if (!StorageFormat.BodyPointer(reference.Page, pageCount)) throw StorageFormat.Corrupt("tree pointer out of bounds");
        var page = await pager.ReadPageAsync(new StoragePageReference(reference.Page, reference.Tag), token).ConfigureAwait(false);
        return page.Span[0] switch
        {
            2 => new(Packed: StorageLeaf.Parse(page, schema, pager.Encrypted)),
            3 => InteriorNode(StorageInterior.Parse(page.Span, pageCount, pager.Encrypted)),
            _ => throw StorageFormat.Corrupt("tree pointer names non-tree page")
        };
    }

    private static Node InteriorNode(StorageInterior interior) => new(Children: interior.Children.Select((id, index) => new Ref(id, Tag: interior.Tags is null ? default : interior.Tags[index])).ToArray(), Keys: interior.Separators);
    private static int LowerBound(Node node, ReadOnlySpan<byte> key)
    {
        var low = 0; var high = node.Count;
        while (low < high) { var mid = low + (high - low) / 2; if (node.Key(mid).AsSpan().SequenceCompareTo(key) < 0) low = mid + 1; else high = mid; }
        return low;
    }
    private static int Route(Node node, ReadOnlySpan<byte> key)
    {
        var low = 0; var high = node.Count;
        while (low < high) { var mid = low + (high - low) / 2; if (node.Keys![mid].AsSpan().SequenceCompareTo(key) <= 0) low = mid + 1; else high = mid; }
        return low;
    }
    private static StorageRecord Clone(StorageRecord record) => new(record.Key.ToArray(), record.Cells.Select(cell => cell?.ToArray()).ToArray())
    { Prepared = record.Prepared, FreshChains = record.FreshChains?.Select(cell => cell?.ToArray()).ToArray() };
    private int DirtyCount(Ref? reference) => reference?.Dirty is not { } node ? 0 : checked(1 + (node.IsLeaf
        ? node.Records().Sum(FreshPageCount) : node.Children!.Sum(DirtyCount)));
    private static void CheckPath(Ref reference, HashSet<uint> path, int depth)
    {
        if (depth > 128 || reference.Page != 0 && !path.Add(reference.Page)) throw StorageFormat.Corrupt("cyclic or excessively deep tree");
    }
}
