using Jed;

// Canonical simulation-testing.md §6. This is a host, not a recovery model:
// the production database performs all writes, publication and recovery.
internal sealed class SimulationFileSystem(ulong seed, int pageSize, int sectorSize = 64) : IFileHost, IScratchDeviceHost
{
    internal sealed record Fault(string Kind, int Probability = 0, int Tear = 0, int Nth = 1, string Operation = "");
    internal sealed class CrashException : Exception { }
    private sealed record Pending(byte[] Bytes, bool Issued);
    private sealed class Inode
    {
        internal byte[] Durable = [];
        internal byte[] Cache = [];
        internal readonly SortedDictionary<long, List<Pending>> Pending = [];
        internal readonly SortedDictionary<uint, List<byte[]>> Archive = [];
    }
    private sealed record DirectoryChange(string Kind, string Path, string? Destination, Inode? File);
    private Dictionary<string, Inode> directory = new(StringComparer.Ordinal);
    private Dictionary<string, Inode> durableDirectory = new(StringComparer.Ordinal);
    private readonly List<DirectoryChange> directoryChanges = [];
    private readonly HashSet<Inode> inodes = [];
    private Fault? fault;
    private int barriers;
    private int syncs;
    private long generation;
    private bool dead;
    private int PageSize => pageSize;
    private ulong scratchOrdinal;
    private readonly HashSet<ScratchDevice> scratch = [];
    internal ulong Step { get; private set; }
    internal string Fired { get; private set; } = "none";
    internal bool OracleReads { get; set; }
    internal List<string> Operations { get; } = [];
    private readonly Dictionary<string, HashSet<uint>> writtenPages = new(StringComparer.Ordinal);
    internal IReadOnlySet<uint> WrittenPages(string path) => writtenPages.GetValueOrDefault(Name(path)) ?? [];

    internal void BeginStep(ulong step, Fault? armed = null)
    { Check(); Step = step; fault = armed; barriers = syncs = 0; scratchOrdinal = 0; Fired = "none"; writtenPages.Clear(); }

    internal void Restart()
    { dead = false; fault = null; barriers = syncs = 0; Fired = "none"; }

    internal void EndFaultedStep()
    {
        if (dead || fault is null) return;
        Fired = Fired == "none" ? "end" : Fired;
        if (fault.Kind == "power_loss") PowerLoss(fault.Probability, fault.Tear);
        else ProcessCrash();
    }

    internal void Crash(bool power, int survive = 0, int tear = 0)
    { Check(); if (power) PowerLoss(survive, tear); else ProcessCrash(); }

    internal byte[] Image(string path) => Find(path).Cache.ToArray();

    public ValueTask<IBlockStore> CreateAsync(string identity, string? directory, CancellationToken token)
    {
        Check(token); ScratchOperation("create");
        var device = new ScratchDevice(this, generation); scratch.Add(device);
        return ValueTask.FromResult<IBlockStore>(device);
    }
    private void ScratchOperation(string operation)
    {
        var ordinal = scratchOrdinal++; Operations.Add($"scratch {operation} {ordinal}");
        if (fault is { Kind: "spill_error" } active && Decide(seed, 11, Step, ordinal, 0) % 1000 < (uint)active.Probability)
        { Fired = "spill"; throw new SqlException(SqlStates.IoError, "simulated scratch failure"); }
    }

    internal static ulong Decide(ulong seed, ulong lane, ulong step, ulong offset, ulong version)
    {
        static ulong Mix(ulong value)
        { unchecked { value = (value ^ (value >> 30)) * 0xbf58476d1ce4e5b9; value = (value ^ (value >> 27)) * 0x94d049bb133111eb; return value ^ (value >> 31); } }
        unchecked
        {
            foreach (var word in new[] { lane, step, offset, version }) seed = Mix((seed ^ word) + 0x9e3779b97f4a7c15);
            return seed;
        }
    }

    public ValueTask<bool> ExistsAsync(string path, CancellationToken token = default)
    { Check(token); return ValueTask.FromResult(directory.ContainsKey(Name(path))); }

    public ValueTask<IBlockStore> OpenAsync(string path, FileMode mode, bool readOnly = false, CancellationToken token = default)
    {
        Check(token); path = Name(path);
        if (mode == FileMode.CreateNew)
        {
            if (directory.ContainsKey(path)) throw new IOException("simulated file already exists");
            var inode = new Inode(); inodes.Add(inode); directory.Add(path, inode);
            directoryChanges.Add(new("create", path, null, inode)); Operations.Add($"create {path}");
        }
        else if (mode != FileMode.Open) throw new InvalidOperationException("unsupported simulation open mode");
        return ValueTask.FromResult<IBlockStore>(new Device(this, Find(path), path, generation, readOnly));
    }

    public ValueTask DeleteAsync(string path, CancellationToken token = default)
    {
        Check(token); path = Name(path);
        if (directory.Remove(path)) { directoryChanges.Add(new("delete", path, null, null)); Operations.Add($"delete {path}"); }
        return ValueTask.CompletedTask;
    }

    public ValueTask MoveAsync(string source, string destination, bool overwrite, CancellationToken token = default)
    {
        Check(token); source = Name(source); destination = Name(destination);
        var inode = Find(source);
        if (directory.TryGetValue(destination, out var previous))
        {
            if (!overwrite) throw new IOException("simulated destination already exists");
            var own = inode.Archive.ToArray(); inode.Archive.Clear();
            foreach (var (page, versions) in previous.Archive)
                foreach (var version in versions) Archive(inode, page, version);
            foreach (var (page, versions) in own)
                foreach (var version in versions) Archive(inode, page, version);
        }
        directory.Remove(source); directory[destination] = inode;
        directoryChanges.Add(new("move", source, destination, inode)); Operations.Add($"move {source} {destination}");
        return ValueTask.CompletedTask;
    }

    public ValueTask<int?> GetPermissionsAsync(string path, CancellationToken token = default)
    { Check(token); _ = Find(path); return ValueTask.FromResult<int?>(384); }

    public ValueTask SetPermissionsAsync(string path, int? permissions, CancellationToken token = default)
    { Check(token); _ = Find(path); return ValueTask.CompletedTask; }

    public ValueTask SyncDirectoryAsync(string path, CancellationToken token = default)
    {
        Check(token);
        if (directoryChanges.Count == 0) return ValueTask.CompletedTask;
        Operations.Add("sync directory"); Barrier();
        durableDirectory = new(directory, StringComparer.Ordinal); directoryChanges.Clear();
        return ValueTask.CompletedTask;
    }

    private void Barrier()
    {
        barriers++;
        if (fault is not { } active || Fired != "none" || barriers != active.Nth) return;
        if (active.Kind == "power_loss")
        { Fired = "barrier"; PowerLoss(active.Probability, active.Tear); throw new CrashException(); }
        if (active.Kind == "process_crash")
        { Fired = "barrier"; ProcessCrash(); throw new CrashException(); }
    }

    private void SyncData(Inode inode, string path)
    {
        syncs++; Barrier();
        if (fault is { Kind: "io_error", Operation: "sync" } active && Fired == "none" && syncs == active.Nth)
        {
            Fired = "sync";
            foreach (var (offset, versions) in inode.Pending)
            {
                var newest = versions[^1].Bytes;
                if (Decision(path, 7, offset, versions.Count - 1) % 2 == 0) Land(inode, offset, newest);
                else
                {
                    for (var i = 0; i < newest.Length; i++) inode.Cache[checked((int)offset) + i] = offset + i < inode.Durable.Length ? inode.Durable[offset + i] : (byte)0;
                }
            }
            inode.Pending.Clear();
            throw new IOException("simulated sync failure");
        }
        CommitData(inode);
    }

    private void PowerLoss(int survive, int tear)
    {
        foreach (var inode in inodes)
        {
            var name = PathOf(inode);
            foreach (var (offset, versions) in inode.Pending)
            {
                if (Decision(name, 1, offset, 0) % 1000 >= (uint)survive) continue;
                var selected = (int)(Decision(name, 2, offset, 0) % (uint)versions.Count);
                var bytes = versions[selected].Bytes.ToArray();
                if (Decision(name, 3, offset, selected) % 1000 < (uint)tear)
                    for (var local = 0; local < bytes.Length; local += sectorSize)
                        if ((Decision(name, 4, offset + local, selected) & 1) == 0)
                        {
                            var count = Math.Min(sectorSize, bytes.Length - local);
                            for (var i = 0; i < count; i++) bytes[local + i] = offset + local + i < inode.Durable.Length ? inode.Durable[offset + local + i] : (byte)0;
                        }
                Land(inode, offset, bytes);
            }
            inode.Pending.Clear(); inode.Cache = inode.Durable.ToArray();
        }
        var keep = (int)(Decide(seed, 10, Step, 0, 0) % (uint)(directoryChanges.Count + 1));
        var recovered = new Dictionary<string, Inode>(durableDirectory, StringComparer.Ordinal);
        foreach (var change in directoryChanges.Take(keep)) Apply(recovered, change);
        directory = recovered; durableDirectory = new(recovered, StringComparer.Ordinal); directoryChanges.Clear();
        Die();
    }

    private void ProcessCrash()
    {
        foreach (var inode in inodes)
        {
            inode.Cache = inode.Durable.ToArray();
            foreach (var (offset, versions) in inode.Pending.ToArray())
            {
                versions.RemoveAll(version => !version.Issued);
                if (versions.Count == 0) inode.Pending.Remove(offset);
                else Copy(ref inode.Cache, offset, versions[^1].Bytes);
            }
        }
        Die();
    }

    private static void Apply(Dictionary<string, Inode> target, DirectoryChange change)
    {
        if (change.Kind == "create") target[change.Path] = change.File!;
        else if (change.Kind == "delete") target.Remove(change.Path);
        else { target.Remove(change.Path); target[change.Destination!] = change.File!; }
    }
    private void Die()
    {
        dead = true; generation++;
        foreach (var device in scratch.ToArray()) device.Kill();
    }
    private static string Name(string path) => Path.GetFileName(path);
    private Inode Find(string path) => directory.TryGetValue(Name(path), out var inode) ? inode : throw new FileNotFoundException("simulated file not found", path);
    private string PathOf(Inode inode) => directory.Where(pair => ReferenceEquals(pair.Value, inode)).Select(pair => pair.Key).Order(StringComparer.Ordinal).FirstOrDefault()
        ?? durableDirectory.Where(pair => ReferenceEquals(pair.Value, inode)).Select(pair => pair.Key).Order(StringComparer.Ordinal).FirstOrDefault() ?? "sim.jed";
    private ulong Decision(string name, ulong lane, long offset, int version)
    {
        unchecked
        {
            ulong key = (ulong)offset;
            if (name != "sim.jed") { uint hash = 2166136261; foreach (var value in System.Text.Encoding.UTF8.GetBytes(name)) hash = (hash ^ value) * 16777619; key += ((ulong)hash + 1) << 40; }
            return Decide(seed, lane, Step, key, (ulong)version);
        }
    }
    private void Check(CancellationToken token = default)
    { token.ThrowIfCancellationRequested(); if (dead) throw new CrashException(); }
    private static void Copy(ref byte[] destination, long offset, ReadOnlySpan<byte> source)
    {
        var end = checked((int)(offset + source.Length));
        if (destination.Length < end) Array.Resize(ref destination, end);
        source.CopyTo(destination.AsSpan(checked((int)offset)));
    }
    private static void Land(Inode inode, long offset, byte[] bytes) => Copy(ref inode.Durable, offset, bytes);
    private void CaptureArchive(Inode inode)
    {
        for (uint page = 4; (ulong)(page + 1) * (uint)pageSize <= (ulong)inode.Durable.Length; page++)
            Archive(inode, page, inode.Durable.AsSpan(checked((int)page * pageSize), pageSize).ToArray());
    }
    private static void Archive(Inode inode, uint page, byte[] bytes)
    {
        if (!inode.Archive.TryGetValue(page, out var history)) inode.Archive.Add(page, history = []);
        if (history.Any(previous => previous.AsSpan().SequenceEqual(bytes))) return;
        history.Add(bytes.ToArray()); if (history.Count > 32) history.RemoveAt(0);
    }
    private void CommitData(Inode inode)
    { inode.Durable = inode.Cache.ToArray(); inode.Pending.Clear(); CaptureArchive(inode); }

    private sealed class ScratchDevice(SimulationFileSystem owner, long generation) : IBlockStore
    {
        private readonly MemoryBlockStore memory = new();
        private bool disposed;
        private void Check(CancellationToken token)
        { owner.Check(token); ObjectDisposedException.ThrowIf(disposed, this); if (generation != owner.generation) throw new CrashException(); }
        public ValueTask ReadAsync(long offset, Memory<byte> destination, CancellationToken token = default)
        { Check(token); owner.ScratchOperation("read"); return memory.ReadAsync(offset, destination, token); }
        public ValueTask WriteAsync(long offset, ReadOnlyMemory<byte> source, CancellationToken token = default)
        { Check(token); owner.ScratchOperation("write"); return memory.WriteAsync(offset, source, token); }
        public ValueTask<long> GetLengthAsync(CancellationToken token = default)
        { Check(token); return memory.GetLengthAsync(token); }
        public ValueTask SetLengthAsync(long length, CancellationToken token = default)
        { Check(token); return memory.SetLengthAsync(length, token); }
        public ValueTask SyncAsync(CancellationToken token = default)
        { Check(token); return ValueTask.CompletedTask; }
        internal void Kill() { disposed = true; owner.scratch.Remove(this); _ = memory.DisposeAsync(); }
        public ValueTask DisposeAsync() { Kill(); return ValueTask.CompletedTask; }
    }

    private sealed class Device(SimulationFileSystem owner, Inode inode, string initialPath, long generation, bool readOnly) : IBlockStore
    {
        private bool disposed;
        private string Path => owner.directory.Any(pair => ReferenceEquals(pair.Value, inode)) ? owner.PathOf(inode) : initialPath;
        private void Check(CancellationToken token, bool write = false)
        {
            owner.Check(token); ObjectDisposedException.ThrowIf(disposed, this);
            if (generation != owner.generation) throw new CrashException();
            if (write && readOnly) throw new UnauthorizedAccessException("read-only simulated file");
        }
        public ValueTask ReadAsync(long offset, Memory<byte> destination, CancellationToken token = default)
        {
            Check(token); MemoryBlockStore.CheckRange(offset, destination.Length);
            if (!owner.OracleReads && owner.fault is { Kind: "read_error" } active && owner.Decision(Path, 9, offset, 0) % 1000 < (uint)active.Probability)
            { owner.Fired = "read"; throw new IOException("simulated read failure"); }
            if (offset + destination.Length > inode.Cache.Length) throw new EndOfStreamException("short simulated read");
            inode.Cache.AsMemory((int)offset, destination.Length).CopyTo(destination); return ValueTask.CompletedTask;
        }
        public ValueTask WriteAsync(long offset, ReadOnlyMemory<byte> source, CancellationToken token = default)
        {
            Check(token, true); MemoryBlockStore.CheckRange(offset, source.Length);
            if (!inode.Pending.TryGetValue(offset, out var versions)) versions = [];
            if (owner.fault is { Kind: "io_error", Operation: "write" } active && owner.Fired == "none" && owner.Decision(Path, 5, offset, versions.Count) % 1000 < (uint)active.Probability)
            { owner.Fired = "write"; throw new IOException("simulated write failure"); }
            var issued = owner.fault is not { Kind: "process_crash" } crash || owner.Decision(Path, 6, offset, versions.Count) % 1000 < (uint)crash.Probability;
            var bytes = source.ToArray(); inode.Pending[offset] = versions; versions.Add(new(bytes, issued)); Copy(ref inode.Cache, offset, bytes);
            if (!owner.writtenPages.TryGetValue(Path, out var pages)) owner.writtenPages.Add(Path, pages = []);
            if (source.Length != 0)
                for (var page = (uint)(offset / owner.PageSize); page <= (uint)((offset + source.Length - 1) / owner.PageSize); page++) pages.Add(page);
            owner.Operations.Add($"write {Path} {offset} {Convert.ToHexStringLower(bytes)}"); return ValueTask.CompletedTask;
        }
        public ValueTask<long> GetLengthAsync(CancellationToken token = default)
        { Check(token); return ValueTask.FromResult((long)inode.Cache.Length); }
        public ValueTask SetLengthAsync(long length, CancellationToken token = default)
        {
            Check(token, true); ArgumentOutOfRangeException.ThrowIfNegative(length);
            var size = checked((int)length); var before = inode.Cache.Length;
            if (size > before && owner.fault is { Kind: "enospc" } && owner.Fired == "none")
            { owner.Fired = "enospc"; throw new IOException("simulated disk full"); }
            Array.Resize(ref inode.Cache, size);
            if (size > before) owner.CommitData(inode);
            else if (size < before)
            {
                Array.Resize(ref inode.Durable, Math.Min(size, inode.Durable.Length));
                foreach (var offset in inode.Pending.Keys.ToArray())
                {
                    if (offset >= size) { inode.Pending.Remove(offset); continue; }
                    var versions = inode.Pending[offset];
                    for (var i = 0; i < versions.Count; i++) if (offset + versions[i].Bytes.Length > size)
                        versions[i] = versions[i] with { Bytes = versions[i].Bytes[..checked((int)(size - offset))] };
                }
            }
            owner.Operations.Add($"size {Path} {length}"); return ValueTask.CompletedTask;
        }
        public ValueTask SyncAsync(CancellationToken token = default)
        { Check(token, true); owner.Operations.Add($"sync {Path}"); owner.SyncData(inode, Path); return ValueTask.CompletedTask; }
        public ValueTask DisposeAsync() { disposed = true; return ValueTask.CompletedTask; }
    }
}
