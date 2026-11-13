using System.Buffers.Binary;

namespace Jed;

internal static class StorageLargeValues
{
    internal static long Slabs(long bytes, int capacity) => (bytes + capacity - 1) / capacity;
    internal static bool External(byte[]? cell) => cell is { Length: > 0 } && cell[0] is 2 or 4;
    internal static bool Compressed(byte[]? cell) => cell is { Length: > 0 } && cell[0] is 3 or 4;

    internal static StorageRecord Prepare(StorageRecord record, StorageLeafSchema schema, int capacity, bool encrypted, Meter? meter = null)
    {
        if (record.Prepared) return record;
        if (record.Cells.Length != schema.Columns.Length) throw new ArgumentException("row width differs from schema");
        var cells = record.Cells.Select(cell => cell?.ToArray()).ToArray();
        long size = record.Key.Length;
        for (var i = 0; i < cells.Length; i++)
        {
            var column = schema.Columns[i];
            if (column.KeyOffset >= 0) continue;
            size += column.Width > 0 ? column.Width : cells[i]?.Length ?? 0;
            if (column.Width == 0 && cells[i] is { Length: > 0 } cell && cell[0] is not (0 or 5))
                throw new ArgumentException("new record must contain materialized inline values");
        }
        var target = Math.Max(0L, (capacity - 12L - 16L * cells.Length) / 2);
        var candidates = Enumerable.Range(0, cells.Length).Where(i => schema.Columns[i].Width == 0 && cells[i] is { Length: >= 33 })
            .OrderByDescending(i => cells[i]!.Length).ThenBy(i => i).ToArray();
        foreach (var i in candidates)
        {
            if (size <= target) break;
            var plain = cells[i]!; var raw = plain.Length - 1;
            meter?.Charge(Slabs(raw, capacity) * Costs.ValueCompress); meter?.Checkpoint();
            var compressed = StorageLz4.Compress(plain.AsSpan(1));
            if (7L + compressed.Length >= plain.Length) continue;
            var encoded = new byte[7 + compressed.Length]; encoded[0] = 3;
            BinaryPrimitives.WriteUInt32BigEndian(encoded.AsSpan(1), (uint)raw);
            // Oversize blocks are necessarily externalized by pass two before
            // any leaf encoding. Their temporary u16 is never persisted.
            BinaryPrimitives.WriteUInt16BigEndian(encoded.AsSpan(5), (ushort)Math.Min(compressed.Length, ushort.MaxValue));
            compressed.CopyTo(encoded, 7); size += encoded.Length - plain.Length; cells[i] = encoded;
        }
        byte[]?[]? fresh = null;
        candidates = Enumerable.Range(0, cells.Length).Where(i => schema.Columns[i].Width == 0 &&
                cells[i] is { } cell && cell.Length > PointerSize(cell[0] == 3, encrypted))
            .OrderByDescending(i => cells[i]!.Length).ThenBy(i => i).ToArray();
        foreach (var i in candidates)
        {
            if (size <= target) break;
            var cell = cells[i]!; var compressed = cell[0] == 3;
            var payload = cell[(compressed ? 7 : 1)..];
            var pointer = new byte[PointerSize(compressed, encrypted)]; pointer[0] = compressed ? (byte)4 : (byte)2;
            BinaryPrimitives.WriteUInt32BigEndian(pointer.AsSpan(5), (uint)payload.Length);
            if (compressed) cell.AsSpan(1, 4).CopyTo(pointer.AsSpan(9));
            (fresh ??= new byte[]?[cells.Length])[i] = payload;
            size += pointer.Length - cell.Length; cells[i] = pointer;
        }
        return new(record.Key.ToArray(), cells) { FreshChains = fresh, Prepared = true };
    }
    private static int PointerSize(bool compressed, bool encrypted) => (compressed ? 13 : 9) + (encrypted ? 16 : 0);

    internal static (int Stored, int Raw, uint First, ReadOnlyMemory<byte> Tag) Inspect(byte[] cell, bool encrypted)
    {
        if (cell.Length == 0) throw StorageFormat.Corrupt("empty large-value cell");
        if (cell[0] == 3)
        {
            if (cell.Length < 7 || BinaryPrimitives.ReadUInt16BigEndian(cell.AsSpan(5)) != cell.Length - 7)
                throw StorageFormat.Corrupt("invalid inline compressed length");
            return (cell.Length - 7, Length(cell, 1), 0, default);
        }
        if (cell[0] is not (2 or 4) || cell.Length != PointerSize(cell[0] == 4, encrypted))
            throw StorageFormat.Corrupt("invalid external-value pointer");
        var stored = Length(cell, 5); var raw = cell[0] == 4 ? Length(cell, 9) : stored;
        if (stored == 0) throw StorageFormat.Corrupt("empty overflow chain");
        return (stored, raw, BinaryPrimitives.ReadUInt32BigEndian(cell.AsSpan(1)), encrypted ? cell.AsMemory(cell.Length - 16) : default);
    }
    private static int Length(byte[] bytes, int offset)
    {
        var value = BinaryPrimitives.ReadUInt32BigEndian(bytes.AsSpan(offset));
        if (value > int.MaxValue) throw StorageFormat.Corrupt("large-value length exceeds host bounds");
        return (int)value;
    }

    internal static long ReadUnits(StorageRecord record, StorageLeafSchema schema, int capacity, bool encrypted, IReadOnlySet<int>? touched = null)
    {
        long units = 0; var chainCapacity = capacity - (encrypted ? 16 : 0);
        for (var i = 0; i < record.Cells.Length; i++)
        {
            if (schema.Columns[i].Width != 0 || touched is not null && !touched.Contains(i) || record.Cells[i] is not { } cell) continue;
            if (!External(cell) && !Compressed(cell)) continue;
            var info = Inspect(cell, encrypted);
            if (External(cell)) units += Slabs(info.Stored, chainCapacity) * Costs.PageRead;
            if (Compressed(cell)) units += Slabs(info.Raw, capacity) * Costs.ValueDecompress;
        }
        return units;
    }

    internal static async ValueTask<byte[]?> MaterializeCellAsync(StorageRecord record, int column, StorageLeafSchema schema,
        StoragePager pager, uint pageCount, CancellationToken token)
    {
        var cell = record.Cells[column];
        if (schema.Columns[column].Width != 0 || cell is null || !External(cell) && !Compressed(cell)) return cell;
        var info = Inspect(cell, pager.Encrypted); byte[] body;
        if (cell[0] == 3) body = cell[7..];
        else if (record.FreshChains?[column] is { } fresh) body = fresh;
        else
        {
            // Bound declared length by the snapshot's addressable bytes before
            // allocating. The chain traversal additionally checks every slab.
            if ((long)info.Stored > (long)pageCount * pager.PayloadCapacity) throw StorageFormat.Corrupt("overflow length exceeds snapshot");
            var slabs = new List<ReadOnlyMemory<byte>>();
            await WalkChainAsync(cell, pager, pageCount, (_, slab) => slabs.Add(slab), token).ConfigureAwait(false);
            body = new byte[info.Stored]; var at = 0;
            foreach (var slab in slabs) { slab.Span.CopyTo(body.AsSpan(at)); at += slab.Length; }
        }
        if (Compressed(cell)) body = StorageLz4.Decompress(body, info.Raw);
        var result = new byte[checked(body.Length + 1)]; result[0] = schema.Columns[column].Dictionary ? (byte)5 : (byte)0;
        body.CopyTo(result, 1); return result;
    }

    internal static ValueTask WalkChainAsync(byte[] cell, StoragePager pager, uint pageCount,
        Action<uint, ReadOnlyMemory<byte>> visit, CancellationToken token)
        => WalkBoundChainAsync(cell, pager, pageCount, (reference, slab) => visit(reference.Page, slab), token);

    internal static async ValueTask WalkBoundChainAsync(byte[] cell, StoragePager pager, uint pageCount,
        Action<StoragePageReference, ReadOnlyMemory<byte>> visit, CancellationToken token)
    {
        var info = Inspect(cell, pager.Encrypted); var remaining = info.Stored;
        var current = new StoragePageReference(info.First, info.Tag); var seen = new HashSet<uint>();
        var capacity = pager.PayloadCapacity - (pager.Encrypted ? 16 : 0);
        while (remaining != 0)
        {
            if (current.Page < (pager.Encrypted ? 4 : 2) || current.Page >= pageCount || !seen.Add(current.Page)) throw StorageFormat.Corrupt("invalid overflow chain pointer");
            var bytes = await pager.ReadPageAsync(current, token).ConfigureAwait(false);
            var header = StorageFormat.ParsePage(bytes.Span, verifyChecksum: !pager.Encrypted);
            var length = Math.Min(remaining, capacity); var start = pager.Encrypted ? 32 : 16;
            if (header.Type != 4 || header.Count != length || (header.Next == 0) != (remaining == length))
                throw StorageFormat.Corrupt("invalid overflow chain slab");
            if (!StorageFormat.IsZero(bytes.Span.Slice(start + length, pager.PayloadCapacity - (start - 16) - length)))
                throw StorageFormat.Corrupt("nonzero overflow padding");
            var nextTag = pager.Encrypted ? bytes.Slice(16, 16) : default;
            if (pager.Encrypted && header.Next == 0 && !StorageFormat.IsZero(nextTag.Span)) throw StorageFormat.Corrupt("nonzero overflow tail tag");
            visit(current, bytes.Slice(start, length)); remaining -= length; current = new(header.Next, nextTag);
        }
    }

    internal static StorageRecord WriteFresh(StorageRecord record, StorageAllocation allocation)
    {
        if (record.FreshChains is null) return record;
        var cells = record.Cells.ToArray(); var capacity = allocation.PayloadCapacity - (allocation.Encrypted ? 16 : 0);
        for (var column = 0; column < cells.Length; column++)
        {
            if (record.FreshChains[column] is not { } payload) continue;
            var count = checked((int)Slabs(payload.Length, capacity)); var ids = new uint[count];
            for (var i = 0; i < ids.Length; i++) ids[i] = allocation.ReservePage();
            for (var i = ids.Length - 1; i >= 0; i--)
            {
                var length = Math.Min(capacity, payload.Length - i * capacity); var next = i + 1 < ids.Length ? ids[i + 1] : 0;
                var body = new byte[length + (allocation.Encrypted ? 16 : 0)];
                if (allocation.Encrypted) allocation.Tag(next).Span.CopyTo(body);
                payload.AsSpan(i * capacity, length).CopyTo(body.AsSpan(allocation.Encrypted ? 16 : 0));
                allocation.SetPage(ids[i], StorageFormat.EncodePage(allocation.PageSize, new(4, 0, (uint)length, next), body));
            }
            var pointer = cells[column]!.ToArray(); BinaryPrimitives.WriteUInt32BigEndian(pointer.AsSpan(1), ids[0]);
            if (allocation.Encrypted) allocation.Tag(ids[0]).Span.CopyTo(pointer.AsSpan(pointer.Length - 16));
            cells[column] = pointer;
        }
        return new(record.Key, cells) { Prepared = true };
    }
}

internal sealed partial class StorageTree
{
    internal StorageRecord PrepareRecord(StorageRecord record, Meter? meter = null) => StorageLargeValues.Prepare(record, schema, pager.PayloadCapacity, pager.Encrypted, meter);
    internal ValueTask<byte[]?> MaterializeCellAsync(StorageRecord record, int column, CancellationToken token = default)
        => StorageLargeValues.MaterializeCellAsync(record, column, schema, pager, pageCount, token);
    internal async ValueTask<StorageRecord> MaterializeRecordAsync(StorageRecord record, CancellationToken token = default)
    {
        var cells = new byte[]?[record.Cells.Length];
        for (var i = 0; i < cells.Length; i++) cells[i] = await MaterializeCellAsync(record, i, token).ConfigureAwait(false);
        return new(record.Key, cells);
    }
    internal long ValueReadUnits(StorageRecord record, IReadOnlySet<int>? touched = null)
        => StorageLargeValues.ReadUnits(record, schema, pager.PayloadCapacity, pager.Encrypted, touched);
    // The caller proves these columns were not assigned and are not part of the
    // primary key. A settled plan may carry an old chain only if its form and
    // lengths still match the new record's plan after other columns changed.
    internal StorageRecord KeepUnchangedChains(StorageRecord prepared, StorageRecord previous, IReadOnlySet<int> unchanged)
    {
        if (!prepared.Prepared) throw new ArgumentException("chain preservation requires a settled record plan");
        var cells = prepared.Cells.ToArray(); var fresh = prepared.FreshChains?.ToArray();
        foreach (var i in unchanged)
        {
            if (i < 0 || i >= cells.Length || schema.Columns[i].Width != 0 || previous.FreshChains?[i] is not null) continue;
            var oldCell = previous.Cells[i]; var newCell = cells[i];
            if (!StorageLargeValues.External(oldCell) || !StorageLargeValues.External(newCell) || oldCell![0] != newCell![0]) continue;
            var oldInfo = StorageLargeValues.Inspect(oldCell, pager.Encrypted); var newInfo = StorageLargeValues.Inspect(newCell, pager.Encrypted);
            if (oldInfo.Stored != newInfo.Stored || oldInfo.Raw != newInfo.Raw) continue;
            cells[i] = oldCell.ToArray(); if (fresh is not null) fresh[i] = null;
        }
        return prepared with { Cells = cells, FreshChains = fresh };
    }
    internal SqlValue[] DecodeDeferredRow(CatalogTable table, StorageRecord record)
    {
        var values = new SqlValue[record.Cells.Length];
        for (var i = 0; i < values.Length; i++)
        {
            var column = i; var cell = record.Cells[i]; var spec = table.Columns[i].Type;
            if (schema.Columns[i].Width == 0 && cell is not null)
                values[i] = SqlValue.Deferred(spec.Type, new DeferredSqlValue(async token =>
                    StoredValues.FromLeaf(await MaterializeCellAsync(record, column, token).ConfigureAwait(false), spec)));
            else values[i] = cell is null ? SqlValue.Null(spec.Type) : StoredValues.FromLeaf(cell.AsMemory(), spec);
        }
        return values;
    }
    private int FreshPageCount(StorageRecord record) => record.FreshChains is null ? 0 : checked((int)record.FreshChains.Sum(payload =>
        payload is null ? 0L : StorageLargeValues.Slabs(payload.Length, pager.PayloadCapacity - (pager.Encrypted ? 16 : 0))));
    private async ValueTask RetireResidentChains(Ref reference, HashSet<uint> dead, CancellationToken token)
    {
        if (reference.Resident is not { IsLeaf: true } resident) return;
        foreach (var row in resident.Records())
            for (var column = 0; column < row.Cells.Length; column++)
                if (row.FreshChains?[column] is not null && StorageLargeValues.External(row.Cells[column]))
                    await StorageLargeValues.WalkChainAsync(row.Cells[column]!, pager, pageCount, (id, _) => dead.Add(id), token).ConfigureAwait(false);
    }
    private async ValueTask RetireChains(StorageRecord record, HashSet<uint> dead, CancellationToken token, StorageRecord? replacement = null)
    {
        for (var i = 0; i < record.Cells.Length; i++)
            if (schema.Columns[i].Width == 0 && StorageLargeValues.External(record.Cells[i]) &&
                StorageLargeValues.Inspect(record.Cells[i]!, pager.Encrypted).First != 0)
            {
                if (replacement?.Cells[i] is { } kept && replacement.FreshChains?[i] is null && kept.AsSpan().SequenceEqual(record.Cells[i])) continue;
                await StorageLargeValues.WalkChainAsync(record.Cells[i]!, pager, pageCount, (id, _) => dead.Add(id), token).ConfigureAwait(false);
            }
    }
    private async ValueTask RetainMovedChains(StorageRecord record, HashSet<uint> dead, CancellationToken token)
    {
        // A re-key deletes the old row before inserting its replacement. Undo
        // retirement of a carried chain without making it reusable in between.
        for (var i = 0; i < record.Cells.Length; i++)
            if (schema.Columns[i].Width == 0 && StorageLargeValues.External(record.Cells[i]) && record.FreshChains?[i] is null &&
                dead.Contains(StorageLargeValues.Inspect(record.Cells[i]!, pager.Encrypted).First))
                await StorageLargeValues.WalkChainAsync(record.Cells[i]!, pager, pageCount, (id, _) => dead.Remove(id), token).ConfigureAwait(false);
    }
}
