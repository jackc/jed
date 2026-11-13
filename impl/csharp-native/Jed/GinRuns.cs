using System.Buffers.Binary;

namespace Jed;

// v47 empty-payload keys; the footer frames bounded front-coded postings.
internal static class GinRuns
{
    internal const int MaximumBytes = 64;
    internal static byte[] Encode(ReadOnlySpan<byte> term, IReadOnlyList<byte[]> keys)
    {
        if (keys.Count == 0 || keys[^1].Length == 0) throw new ArgumentException("empty GIN run");
        var body = new WireWriter(); var prior = keys[^1];
        for (var i = 0; i + 1 < keys.Count; i++)
        {
            var key = keys[i];
            if (key.Length == 0 || key.AsSpan().SequenceCompareTo(keys[i + 1]) >= 0) throw new ArgumentException("unordered GIN run");
            var shared = 0; while (shared < key.Length && shared < prior.Length && key[shared] == prior[shared]) shared++;
            Varint(body, shared); Varint(body, key.Length - shared); body.Bytes(key.AsSpan(shared)); prior = key;
        }
        if (body.Length > ushort.MaxValue) throw new ArgumentException("oversize GIN run body");
        var writer = new WireWriter(); writer.Bytes(term); writer.Bytes(keys[^1]); writer.Bytes(body.ToArray()); writer.U16((ushort)body.Length); return writer.ToArray();
    }
    internal static byte[][] Decode(ReadOnlySpan<byte> record, int termLength)
    {
        if (termLength < 0 || record.Length < termLength + 3) throw StorageFormat.Corrupt("truncated GIN run");
        var bodySize = BinaryPrimitives.ReadUInt16BigEndian(record[^2..]); var lastEnd = record.Length - 2 - bodySize;
        if (lastEnd <= termLength) throw StorageFormat.Corrupt("invalid GIN run body length");
        var last = record[termLength..lastEnd].ToArray(); var prior = last;
        var body = record.Slice(lastEnd, bodySize); var keys = new List<byte[]>();
        while (!body.IsEmpty)
        {
            var shared = ReadVarint(ref body); var suffix = ReadVarint(ref body);
            if (shared > prior.Length || suffix > body.Length || (long)shared + suffix > int.MaxValue || shared + suffix == 0)
                throw StorageFormat.Corrupt("invalid GIN run prefix");
            var key = new byte[shared + suffix]; prior.AsSpan(0, shared).CopyTo(key); body[..suffix].CopyTo(key.AsSpan(shared)); body = body[suffix..];
            if (key.AsSpan().SequenceCompareTo(last) >= 0 || keys.Count != 0 && keys[^1].AsSpan().SequenceCompareTo(key) >= 0)
                throw StorageFormat.Corrupt("unordered GIN postings");
            keys.Add(key); prior = key;
        }
        keys.Add(last); return keys.ToArray();
    }
    internal static IEnumerable<byte[]> Pack(byte[] term, IEnumerable<byte[]> keys)
    {
        var run = new List<byte[]>();
        foreach (var key in keys)
        {
            run.Add(key);
            if (run.Count > 1 && Encode(term, run).Length > MaximumBytes)
            { run.RemoveAt(run.Count - 1); yield return Encode(term, run); run.Clear(); run.Add(key); }
        }
        if (run.Count != 0) yield return Encode(term, run);
    }
    private static IEnumerable<byte[]> Split(byte[] term, byte[][] keys)
    {
        if (keys.Length == 0) yield break;
        var encoded = Encode(term, keys);
        if (keys.Length == 1 || encoded.Length <= MaximumBytes) { yield return encoded; yield break; }
        var middle = (keys.Length + 1) / 2;
        foreach (var item in Split(term, keys[..middle])) yield return item;
        foreach (var item in Split(term, keys[middle..])) yield return item;
    }
    internal static async ValueTask<StorageTree> Edit(StorageTree tree, byte[] term, byte[] key, bool insert, CancellationToken token)
    {
        var probe = term.Concat(key).ToArray(); var end = SecondaryAccess.Successor(term);
        StorageRecord? existing = null; var append = false;
        await foreach (var record in tree.ScanRangeAsync(probe, true, end, false, cancellationToken: token).ConfigureAwait(false))
        { existing = record; break; }
        if (existing is null)
        {
            if (!insert) return tree;
            append = true;
            await foreach (var record in tree.ScanRangeAsync(term, true, end, false, reverse: true, cancellationToken: token).ConfigureAwait(false))
            { existing = record; break; }
        }
        var keys = existing is null ? new List<byte[]>() : Decode(existing.Key, term.Length).ToList();
        var index = keys.BinarySearch(key, StatisticsBytes.Instance);
        if (insert && index >= 0 || !insert && index < 0) return tree;
        if (insert) keys.Insert(~index, key); else keys.RemoveAt(index);
        if (append && existing is not null && Encode(term, keys).Length > MaximumBytes)
            return await tree.UpsertAsync(new(Encode(term, [key]), []), token).ConfigureAwait(false);
        if (existing is not null) tree = await tree.DeleteAsync(existing.Key, token).ConfigureAwait(false);
        foreach (var run in Split(term, keys.ToArray())) tree = await tree.UpsertAsync(new(run, []), token).ConfigureAwait(false);
        return tree;
    }
    private static void Varint(WireWriter writer, int value)
    { do { var next = (byte)(value & 127); value >>= 7; writer.Byte(value == 0 ? next : (byte)(next | 128)); } while (value != 0); }
    private static int ReadVarint(ref ReadOnlySpan<byte> bytes)
    {
        uint value = 0;
        for (var shift = 0; shift < 35; shift += 7)
        {
            if (bytes.IsEmpty) throw StorageFormat.Corrupt("truncated GIN run varint");
            var next = bytes[0]; bytes = bytes[1..];
            if (shift == 28 && next > 7) throw StorageFormat.Corrupt("GIN run varint overflow");
            value |= (uint)(next & 127) << shift;
            if ((next & 128) == 0)
            { if (shift != 0 && next == 0) throw StorageFormat.Corrupt("noncanonical GIN run varint"); return (int)value; }
        }
        throw StorageFormat.Corrupt("invalid GIN run varint");
    }
}
