using System.Buffers.Binary;
using Jed;

internal sealed partial class SimulationFileSystem
{
    internal sealed record DamagedPage(uint Page, byte[] Original);
    private bool PendingIn(Inode inode, long start, long end) => inode.Pending.Any(pair => pair.Key < end && pair.Value.Any(version => pair.Key + version.Bytes.Length > start));
    internal DamagedPage[] Corrupt(string path, int count, uint pageCount, bool reseal)
    {
        var inode = Find(path); var touched = new Dictionary<uint, DamagedPage>();
        if (pageCount <= 2) return [];
        for (var ordinal = 0; ordinal < count; ordinal++)
        {
            var page = 2 + (uint)(Decide(seed, 12, Step, (ulong)ordinal, 0) % (pageCount - 2));
            var bit = (int)(Decide(seed, 13, Step, (ulong)ordinal, 0) % (ulong)(pageSize * 8));
            var start = checked((int)page * pageSize);
            if (start + (long)pageSize > inode.Durable.Length || start + (long)pageSize > inode.Cache.Length || PendingIn(inode, start, start + pageSize)) continue;
            if (!touched.ContainsKey(page)) touched.Add(page, new(page, inode.Durable.AsSpan(start, pageSize).ToArray()));
            inode.Durable[start + bit / 8] ^= (byte)(1 << (bit % 8));
        }
        foreach (var item in touched.Values)
        {
            var bytes = inode.Durable.AsSpan(checked((int)item.Page * pageSize), pageSize);
            if (reseal) BinaryPrimitives.WriteUInt32BigEndian(bytes[12..], StorageFormat.PageChecksum(bytes));
            bytes.CopyTo(inode.Cache.AsSpan(checked((int)item.Page * pageSize)));
        }
        return touched.Values.ToArray();
    }
    internal void Repair(string path, IEnumerable<DamagedPage> pages)
    {
        var inode = Find(path);
        foreach (var page in pages)
        {
            var offset = checked((int)page.Page * pageSize);
            page.Original.CopyTo(inode.Durable, offset); page.Original.CopyTo(inode.Cache, offset);
        }
    }
    internal byte[][] ReplayVersions(string path, uint page)
    {
        var inode = Find(path); var offset = checked((int)page * pageSize);
        if (offset + (long)pageSize > inode.Durable.Length || offset + (long)pageSize > inode.Cache.Length || PendingIn(inode, offset, offset + pageSize)) return [];
        var versions = inode.Archive.GetValueOrDefault(page, []).Where(bytes => !bytes.AsSpan().SequenceEqual(inode.Cache.AsSpan(offset, pageSize))).Select(bytes => bytes.ToArray()).ToArray();
        Array.Sort(versions, (a, b) => a.AsSpan().SequenceCompareTo(b)); return versions;
    }
    internal DamagedPage ReplacePage(string path, uint page, byte[] bytes)
    {
        var inode = Find(path); var offset = checked((int)page * pageSize);
        if (bytes.Length != pageSize || offset + (long)pageSize > inode.Durable.Length || offset + (long)pageSize > inode.Cache.Length) throw new InvalidDataException("invalid replay page");
        var undo = new DamagedPage(page, inode.Durable.AsSpan(offset, pageSize).ToArray());
        bytes.CopyTo(inode.Durable, offset); bytes.CopyTo(inode.Cache, offset); return undo;
    }
    internal void Restore(string source, string destination)
    {
        source = Name(source); destination = Name(destination);
        durableDirectory = new(directory, StringComparer.Ordinal); directoryChanges.Clear();
        var from = Find(source); var replacement = new Inode(destination) { Cache = from.Cache.ToArray(), Durable = from.Cache.ToArray() };
        if (directory.TryGetValue(destination, out var old)) foreach (var (page, versions) in old.Archive) foreach (var bytes in versions) Archive(replacement, page, bytes);
        foreach (var (page, versions) in from.Archive) foreach (var bytes in versions) Archive(replacement, page, bytes);
        inodes.Add(replacement); directory[destination] = replacement; durableDirectory[destination] = replacement;
        Operations.Add("restore " + source + " " + destination);
    }
}
