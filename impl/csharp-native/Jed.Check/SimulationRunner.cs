using System.Globalization;
using System.Security.Cryptography;
using Jed;

internal sealed class SimulationRunner
{
    private sealed record Handle(Session Session, bool Write, ulong Version);
    private readonly SimulationTrace trace;
    private readonly SimulationFileSystem fs;
    private readonly Dictionary<string, Handle> sessions = new(StringComparer.Ordinal);
    private readonly Dictionary<ulong, HashSet<string>> history = [];
    private readonly List<string> transcript = [];
    private readonly SimulationTranscript json = new();
    private Database database = null!;
    private long clock;
    private ulong version, floor, attempted;
    private SimulationFileSystem.Fault? armed;
    private string? pendingDigest;
    private long growthBound;
    private bool previousReclaimed;
    private ulong openings;
    private int key;
    private sealed record KeyAttempt(int Old, int Next, bool EntryPending, bool Acknowledged = false);
    private KeyAttempt? keyAttempt;
    private readonly DatabaseCredential[] keys = Enumerable.Range(0, 4).Select(index => DatabaseCredential.FromKey(Enumerable.Repeat((byte)(0x4a + index), 32).ToArray())).ToArray();
    private SimulationRunner(SimulationTrace trace)
    { this.trace = trace; fs = new(trace.Seed, trace.PageSize, trace.Sector); clock = trace.Clock; key = trace.Encrypted ? 0 : -1; }

    internal static async Task Run(string path)
    {
        var trace = SimulationTrace.Read(path);
        var first = new SimulationRunner(trace); var second = new SimulationRunner(trace);
        await first.Execute(); await second.Execute();
        SimulationStorageChecks.Require(first.transcript.SequenceEqual(second.transcript), "transcript is not self-deterministic");
        SimulationStorageChecks.Require(first.fs.Operations.SequenceEqual(second.fs.Operations), "byte-device operations are not self-deterministic");
        foreach (var line in first.transcript) Console.WriteLine(line);
    }
    private DatabaseFileOptions Options()
    {
        var random = new SeededRandomSource(SimulationFileSystem.Decide(trace.Seed, 14, openings++, 0, 0));
        return new() { FileHost = fs, ScratchDevices = fs, Locking = FileLocking.None,
            CacheBytes = trace.CacheBytes == 0 ? 256L * 1024 * 1024 : trace.CacheBytes,
            Entropy = count => new byte[count], KeyEntropy = random.Fill, ScratchEntropy = new SeededRandomSource(trace.Seed).Fill,
            ScratchEncryption = trace.Encrypted ? ScratchEncryption.On : ScratchEncryption.Auto, Credential = key < 0 ? null : keys[key] };
    }
    private SessionOptions SessionOptions(ulong step)
    {
        var random = new SeededRandomSource(SimulationFileSystem.Decide(trace.Seed, 8, step, 0, 0));
        return new() { Clock = () => clock, RandomSource = random.Fill, MaxQueryMemoryBytes = trace.MaxQueryMemory,
            TempBuffers = trace.TempBuffers == 0 ? 32L * 1024 * 1024 : trace.TempBuffers };
    }
    private async Task Execute()
    {
        // Reject unsupported driver modes explicitly, before running a partial
        // history. A passing trace always executes every requested operation.
        if (trace.Synchronous != "on") throw new NotSupportedException("simulation relaxed driver mode is not implemented yet");
        foreach (var step in trace.Steps)
            if (step.Operation is not ("open" or "on" or "commit" or "rollback" or "close" or "fault" or "crash" or "reopen" or "compact" or "check" or "clock" or "change_key" or "rekey"))
                throw new NotSupportedException("simulation operation " + step.Operation);
        database = await Database.CreateFileAsync("sim.jed", Options(), trace.PageSize);
        await database.DisposeAsync();
        database = await Database.OpenFileAsync("sim.jed", Options());
        Configure(); version = floor = attempted = database.Version;
        AddHistory(version, await Digest());
        foreach (var step in trace.Steps)
        {
            try { await Step(step); }
            catch (Exception error)
            {
                var prefix = Path.Combine(Path.GetTempPath(), "jed-csharp-simulation-failure-" + trace.Name);
                File.WriteAllBytes(prefix + ".img", fs.Image("sim.jed"));
                File.WriteAllLines(prefix + ".jsonl", transcript);
                throw new InvalidDataException($"{trace.Name}:{step.Line}, step {step.Index} ({step.Operation}): {error.Message}; image {prefix}.img", error);
            }
        }
        await CloseClean(); json.Dispose(); foreach (var credential in keys) credential.Dispose();
    }
    private void Configure()
    {
        database.MaxStorageBytes = trace.MaxStorage; database.SetWorkMem(trace.WorkMemory);
        var storage = database.CommittedStorageForTesting();
        growthBound = storage.Meta.PageCount - storage.FreePages.Count; previousReclaimed = false;
    }
    private void Emit() => transcript.Add(json.End());
    private void AddHistory(ulong generation, string digest)
    { if (!history.TryGetValue(generation, out var values)) history.Add(generation, values = []); values.Add(digest); attempted = Math.Max(attempted, generation); }
    private async Task<string> Digest()
    { await using var session = database.CreateSession(SessionOptions(fs.Step) with { MaxQueryMemoryBytes = 0 }); return await SimulationRows.Digest(session, fs); }
    private async Task CloseClean()
    {
        foreach (var handle in sessions.Values) await handle.Session.DisposeAsync();
        sessions.Clear(); await database.DisposeAsync();
    }
    private static int Parameter(string[] args, string name, int fallback)
    {
        var text = args.FirstOrDefault(value => value.StartsWith(name + "=", StringComparison.Ordinal));
        return text is null ? fallback : int.Parse(text[(name.Length + 1)..], CultureInfo.InvariantCulture);
    }
    private async Task Step(SimulationStep step)
    {
        var args = step.Arguments;
        if (step.Operation == "fault")
        {
            var kind = args[0]; var probability = Parameter(args, kind == "power_loss" ? "survive" : kind == "process_crash" ? "issue" : "p", 0);
            if (kind is not ("power_loss" or "process_crash" or "io_error" or "read_error" or "enospc" or "spill_error")) throw new NotSupportedException("simulation fault " + kind);
            armed = new(kind, probability, Parameter(args, "tear", 0), Parameter(args, "nth", 1), args.FirstOrDefault(value => value.StartsWith("op=", StringComparison.Ordinal))?[3..] ?? "");
            json.Start(step.Index, "fault").Text("kind", kind); Emit(); return;
        }
        var active = armed; armed = null; fs.BeginStep((ulong)step.Index, active); pendingDigest = null; keyAttempt = null;
        var start = version;
        if (step.Operation == "crash")
        {
            fs.Crash(args[0] == "power", Parameter(args, "survive", 0), Parameter(args, "tear", 0));
            await Recover(step.Index, "none"); return;
        }
        json.Start(step.Index, step.Operation);
        if (step.Operation is "open" or "on" or "commit" or "rollback" or "close") json.Text("sid", args[0]);
        try
        {
            switch (step.Operation)
            {
                case "open":
                {
                    if (args.Length > 2) throw new NotSupportedException("simulation service process handle");
                    if (sessions.ContainsKey(args[0]) || (args[1] == "write" && sessions.Values.Any(handle => handle.Write))) throw new InvalidDataException("trace opens a duplicate session or second writer");
                    var write = args[1] == "write";
                    if (!write && args[1] != "read") throw new InvalidDataException("invalid session mode");
                    var session = write ? await database.OpenWriteSessionAsync(SessionOptions((ulong)step.Index)) : await database.OpenReadSessionAsync(SessionOptions((ulong)step.Index));
                    sessions.Add(args[0], new(session, write, database.Version)); json.Number("version", database.Version); break;
                }
                case "on":
                    if (!sessions.TryGetValue(args[0], out var querying)) { json.Flag("skip", true); break; }
                    var rows = await SimulationRows.Query(querying.Session, step.Sql!);
                    json.Number("rows", rows.Rendered.Length).Number("affected", rows.Affected).Text("hash", rows.Hash).Number("cost", rows.Cost); break;
                case "commit":
                    if (!sessions.Remove(args[0], out var committing)) { json.Flag("skip", true); break; }
                    try { pendingDigest = await SimulationRows.Digest(committing.Session, fs); }
                    catch (SqlException error) when (error.SqlState == SqlStates.InFailedSqlTransaction) { }
                    var beforeCommit = database.CommittedStorageForTesting();
                    await committing.Session.CommitAsync();
                    version = database.Version;
                    var digest = await Digest();
                    SimulationStorageChecks.Require(pendingDigest is null || pendingDigest == digest, "acknowledged commit differs from pending session");
                    SimulationStorageChecks.Require(version != start || history[start].Contains(digest), "unchanged commit changed contents");
                    AddHistory(version, digest); floor = version;
                    await Oracle(() => SimulationStorageChecks.Live(database));
                    if (version != start) CheckGrowth(beforeCommit, active is null && sessions.Count == 0);
                    json.Number("version", version); break;
                case "rollback":
                case "close":
                    if (!sessions.Remove(args[0], out var closing)) { json.Flag("skip", true); break; }
                    if (!closing.Write)
                    {
                        var pinned = await SimulationRows.Digest(closing.Session, fs);
                        SimulationStorageChecks.Require(history[closing.Version].Contains(pinned), "reader snapshot changed while pinned");
                    }
                    await closing.Session.DisposeAsync(); break;
                case "clock": clock = checked(clock + long.Parse(args[0], CultureInfo.InvariantCulture)); break;
                case "reopen":
                    await CloseClean(); database = await Database.OpenFileAsync("sim.jed", Options()); Configure();
                    version = database.Version; var reopened = await Digest();
                    SimulationStorageChecks.Require(history[version].Contains(reopened), "clean reopen changed contents");
                    json.Number("version", version).Text("digest", reopened); break;
                case "change_key":
                    if (!trace.Encrypted) { json.Flag("skip", true); break; }
                    var replacementKey = int.Parse(args[0], CultureInfo.InvariantCulture);
                    if (replacementKey is < 1 or > 3 || args.Length > 1 && args[1] != "main") throw new InvalidDataException("invalid credential target");
                    keyAttempt = new(key, replacementKey, !fs.EntryDurable("sim.jed"));
                    await database.ChangeCredentialAsync(keys[replacementKey]); keyAttempt = keyAttempt with { Acknowledged = true }; key = replacementKey; json.Number("key", key); break;
                case "rekey":
                case "compact":
                    if (step.Operation == "compact" && args.Length > 0 && args[0] != "main" || step.Operation == "rekey" && args.Length > 1 && args[1] != "main") throw new NotSupportedException("simulation attachment compaction");
                    pendingDigest = await Digest(); AddHistory(start + 1, pendingDigest);
                    if (step.Operation == "rekey")
                    {
                        var targetKey = args[0] == "plain" ? -1 : int.Parse(args[0], CultureInfo.InvariantCulture);
                        if (targetKey is < -1 or > 3 or 0) throw new InvalidDataException("invalid rekey target");
                        keyAttempt = new(key, targetKey, !fs.EntryDurable("sim.jed"));
                        await database.RekeyAsync(targetKey < 0 ? null : keys[targetKey]); keyAttempt = keyAttempt with { Acknowledged = true }; key = targetKey;
                    }
                    else await database.CompactAsync();
                    version = database.Version;
                    SimulationStorageChecks.Require(version == start + 1 && pendingDigest == await Digest(), "compaction changed contents or generation incorrectly");
                    AddHistory(version, pendingDigest); floor = version; Configure(); json.Number("version", version); break;
                case "check":
                    await Oracle(() => SimulationStorageChecks.Live(database));
                    await Fields(false); break;
                default: throw new NotSupportedException("simulation operation " + step.Operation);
            }
        }
        catch (SimulationFileSystem.CrashException)
        { json.Text("error", SqlStates.IoError); RecordAttempt(start); }
        catch (SqlException error)
        {
            if (error.SqlState == SqlStates.DataCorrupted) throw new InvalidDataException("unexpected corruption outside corruption mode", error);
            json.Text("error", error.SqlState); RecordAttempt(start);
        }
        Emit();
        if (fs.DurableBarrier("sim.jed")) floor = Math.Max(floor, start);
        if (active is not null)
        { fs.EndFaultedStep(); var fired = fs.Fired; await Recover(step.Index, fired); }
    }
    private void CheckGrowth(StorageRecovered before, bool quiet)
    {
        var after = database.CommittedStorageForTesting();
        var written = fs.WrittenPages("sim.jed").Where(page => page >= 2).ToArray();
        var threshold = Math.Max(16, 2 * growthBound);
        var reclaim = quiet && before.Meta.PageCount - before.FreePages.Count > threshold;
        if (quiet)
        {
            if (reclaim) SimulationStorageChecks.Require((long)after.Meta.PageCount - 2 - after.Meta.LivePages - after.FreePages.Count <= written.Length,
                "quiet commit exceeded reclamation threshold without reclaiming unreachable pages");
            if (previousReclaimed) SimulationStorageChecks.Require((long)after.Meta.PageCount - before.Meta.PageCount <= Math.Max(0, written.Length - before.FreePages.Count),
                "commit grew without first reusing the reclaimed free list");
            var tail = before.Meta.PageCount;
            while (tail > 2 && before.FreePages.Contains(tail - 1)) tail--;
            var high = written.Length == 0 ? 0 : written.Max() + 1;
            SimulationStorageChecks.Require(after.Meta.PageCount == Math.Max(tail, high), "quiet commit did not use canonical trailing truncation/high-water");
            if (after.Meta.PageCount < before.Meta.PageCount)
            {
                var allowance = Math.Clamp((long)after.Meta.PageCount, Math.Max(1, 16 * 1024 / trace.PageSize), Math.Max(1, 1024 * 1024 / trace.PageSize));
                SimulationStorageChecks.Require(fs.Image("sim.jed").LongLength <= ((long)after.Meta.PageCount + allowance) * trace.PageSize,
                    "trailing truncation retained more than one growth step of slack");
            }
        }
        growthBound = Math.Max(growthBound, (long)after.Meta.LivePages + written.Length); previousReclaimed = reclaim;
    }
    private void RecordAttempt(ulong start)
    { if (pendingDigest is not null && fs.WrittenPages("sim.jed").Count > 0) AddHistory(start + 1, pendingDigest); }
    private async Task Oracle(Func<Task> action)
    { var before = fs.OracleReads; fs.OracleReads = true; try { await action(); } finally { fs.OracleReads = before; } }
    private async Task Fields(bool recovered)
    {
        var digest = await Digest(); var bytes = fs.Image("sim.jed");
        if (Environment.GetEnvironmentVariable("JED_SIM_OUT") is { Length: > 0 } output)
        {
            Directory.CreateDirectory(output);
            File.WriteAllBytes(Path.Combine(output, $"{Path.GetFileNameWithoutExtension(trace.Name)}.csharp-native.step{fs.Step}.img"), bytes);
        }
        var image = await SimulationStorageChecks.Image(bytes, version, key < 0 ? null : keys[key].Value);
        json.Number(recovered ? "recovered" : "version", version).Text("digest", digest).Number("live", image.Live).Number("pages", image.Pages).Number("free", image.Free).Text("image", image.Image);
    }
    private async Task Recover(int step, string fired)
    {
        sessions.Clear(); fs.Restart();
        var image = fs.Image("sim.jed");
        bool Unlocks(int candidate)
        {
            if (image[6] == 0) return candidate < 0;
            if (candidate < 0) return false;
            try
            {
                var block = StorageKeyBlock.Select(image.AsSpan(2 * trace.PageSize, trace.PageSize), image.AsSpan(3 * trace.PageSize, trace.PageSize));
                var secret = block.Block.Unwrap(keys[candidate].Value); CryptographicOperations.ZeroMemory(secret); return true;
            }
            catch (SqlException error) when (error.SqlState == SqlStates.InvalidPassword) { return false; }
        }
        if (keyAttempt is { } change)
        {
            var oldWorks = Unlocks(change.Old); var newWorks = Unlocks(change.Next);
            SimulationStorageChecks.Require(change.Old == change.Next ? oldWorks :
                oldWorks && !newWorks && !change.Acknowledged || !oldWorks && newWorks || change.EntryPending && !oldWorks && !newWorks,
                "credential recovery violates atomic replacement or acknowledged durability");
        }
        var options = Options(); var oldKey = key; var opened = false;
        foreach (var candidate in new[] { key, -1, 0, 1, 2, 3 }.Distinct())
        {
            if (!trace.Encrypted && candidate != -1) continue;
            try
            {
                database = await Database.OpenFileAsync("sim.jed", options with { Credential = candidate < 0 ? null : keys[candidate] });
                key = candidate; opened = true; break;
            }
            catch (SqlException error) when (error.SqlState is SqlStates.InvalidPassword or SqlStates.CredentialRequired or SqlStates.CredentialNotApplicable) { }
        }
        SimulationStorageChecks.Require(opened, "recovery cannot unlock with any authorized simulation key");
        if (key != oldKey || keyAttempt is not null) { json.Start(step, "unlocked").Number("key", key); Emit(); }
        Configure(); version = database.Version;
        SimulationStorageChecks.Require(version >= floor && version <= attempted, $"recovered generation {version} outside durability window {floor}..{attempted}");
        var digest = await Digest();
        SimulationStorageChecks.Require(history.TryGetValue(version, out var values) && values.Contains(digest), "recovered unrecorded contents");
        json.Start(step, "crash").Text("fired", fired); await Fields(true); Emit();
    }
}
