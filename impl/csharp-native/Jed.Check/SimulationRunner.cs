using System.Globalization;
using System.Security.Cryptography;
using Jed;

internal sealed partial class SimulationRunner
{
    private sealed record Handle(Session Session, bool Write, ulong Version, Dictionary<string, ulong> Attachments, int Process);
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
    private string committedDigest = "";
    private long growthBound;
    private bool previousReclaimed;
    private ulong openings;
    private int key;
    private sealed record KeyAttempt(int Old, int Next, bool EntryPending, bool Acknowledged = false);
    private KeyAttempt? keyAttempt;
    private readonly DatabaseCredential[] keys = Enumerable.Range(0, 4).Select(index => DatabaseCredential.FromKey(Enumerable.Repeat((byte)(0x4a + index), 32).ToArray())).ToArray();
    private SimulationRunner(SimulationTrace trace)
    { this.trace = trace; fs = new(trace.Seed, trace.PageSize, trace.Sector); clock = trace.Clock; key = trace.Encrypted ? 0 : -1; roles = new(network, () => clock, increment => clock = checked(clock + increment)); }

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
        var environment = DatabaseEnvironment.Builder().WithInsecureKeySeed(SimulationFileSystem.Decide(trace.Seed, 14, openings++, (ulong)selectedProcess, 0)).Build();
        return new() { FileHost = fs, ScratchDevices = fs, Locking = trace.Service ? FileLocking.Auto : FileLocking.None,
            RoleHost = trace.Service ? roles.Host(selectedProcess) : null,
            IdleTransactionMilliseconds = 0, IdleSessionMilliseconds = 0, ShutdownGraceMilliseconds = 1000,
            Synchronous = trace.Synchronous == "off" ? Synchronous.Off : Synchronous.On,
            FlushIntervalMilliseconds = trace.FlushIntervalMilliseconds, Clock = () => clock, EnableIdleFlush = false,
            CacheBytes = trace.CacheBytes == 0 ? 256L * 1024 * 1024 : trace.CacheBytes,
            Environment = environment, Entropy = count => new byte[count], ScratchEntropy = new SeededRandomSource(trace.Seed).Fill,
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
        foreach (var step in trace.Steps)
            if (step.Operation is not ("open" or "on" or "commit" or "rollback" or "close" or "fault" or "crash" or "reopen" or "compact" or "check" or "clock" or "change_key" or "rekey" or "attach" or "spawn" or "disconnect" or "kill" or "exit" or "backup" or "corrupt" or "replay" or "restore"))
                throw new NotSupportedException("simulation operation " + step.Operation);
        database = await Database.CreateFileAsync("sim.jed", Options() with { RoleHost = null, Locking = FileLocking.None }, trace.PageSize);
        await database.DisposeAsync();
        database = await Database.OpenFileAsync("sim.jed", processOptions = Options());
        if (trace.Service) processes.Add(0, new(database, processOptions));
        Configure(); version = floor = attempted = database.Version;
        AddHistory(version, await Digest());
        foreach (var step in trace.Steps)
        {
            if (System.Environment.GetEnvironmentVariable("JED_SIM_PROGRESS") is { Length: > 0 } progress)
                File.AppendAllText(progress, $"{trace.Name}:{step.Index} {step.Operation}\n");
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
    private void Emit()
    {
        var line = json.End(); transcript.Add(line);
        if (System.Environment.GetEnvironmentVariable("JED_SIM_DIAGNOSTICS") == "1")
        {
            var state = (trace.Service ? roles.DatabaseOf("sim.jed") ?? database : database).PublishedStateForTesting().Storage;
            Console.Error.WriteLine($"model step={fs.Step} floor={floor} version={version} ownerVersion={state.Meta.TransactionId} batchFloor={state.Batch?.FloorTransactionId} durable={fs.ContentDurable("sim.jed")} event={line}");
        }
    }
    private void AddHistory(ulong generation, string digest)
    { if (!history.TryGetValue(generation, out var values)) history.Add(generation, values = []); values.Add(digest); attempted = Math.Max(attempted, generation); }
    private async Task<string> Digest()
    { await using var session = database.CreateSession(SessionOptions(fs.Step) with { MaxQueryMemoryBytes = 0 }); return committedDigest = await SimulationRows.Digest(session, fs); }
    private async Task CloseClean()
    {
        var closingVersion = version;
        if (trace.Service) await CloseService();
        else
        {
            foreach (var handle in sessions.Values) await handle.Session.DisposeAsync();
            sessions.Clear(); closingVersion = database.Version; await database.DisposeAsync();
        }
        if (fs.EntryDurable("sim.jed") && fs.ContentDurable("sim.jed")) floor = Math.Max(floor, closingVersion);
        foreach (var state in attached.Values)
            if (fs.EntryDurable(state.File) && fs.ContentDurable(state.File)) state.Floor = Math.Max(state.Floor, state.Version);
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
            if (kind is not ("power_loss" or "process_crash" or "io_error" or "read_error" or "enospc" or "spill_error" or "disconnect")) throw new NotSupportedException("simulation fault " + kind);
            armed = new(kind, probability, Parameter(args, "tear", 0), Parameter(args, "nth", 1), args.FirstOrDefault(value => value.StartsWith("op=", StringComparison.Ordinal))?[3..] ?? "");
            json.Start(step.Index, "fault").Text("kind", kind); Emit(); return;
        }
        // The canonical host restarts p0 when a real operation follows the
        // last clean process exit. Clock/control and image-probe steps do not.
        if (trace.Service && processes.Count == 0 && step.Operation is not
            ("spawn" or "clock" or "tick" or "disconnect" or "exit" or "kill" or "crash" or "corrupt" or "replay" or "restore"))
        {
            fs.BeginStep((ulong)step.Index, null);
            json.Start(step.Index, "spawn"); await SpawnProcess(0); Emit();
        }
        var active = armed; armed = null; fs.BeginStep((ulong)step.Index, active?.Kind == "disconnect" ? null : active); network.BeginStep(active?.Kind == "disconnect" ? active.Nth : 0); pendingDigest = null; keyAttempt = null;
        foreach (var state in attached.Values) { state.Pending = null; state.KeyAttempt = null; }
        var attachmentStarts = attached.ToDictionary(pair => pair.Key, pair => pair.Value.Version, StringComparer.Ordinal);
        bool BatchOpen(Database selected, string path)
        {
            var owner = trace.Service ? roles.DatabaseOf(path) ?? selected : selected;
            var batch = owner.PublishedStateForTesting().Storage.Batch;
            return trace.Synchronous == "off" && batch is not null &&
                (batch.DeadlineMicroseconds is null || clock < batch.DeadlineMicroseconds);
        }
        var mainBatchOpen = BatchOpen(database, "sim.jed");
        var attachmentBatches = attached.ToDictionary(pair => pair.Key, pair => BatchOpen(pair.Value.Database, pair.Value.File), StringComparer.Ordinal);
        var start = version; var acknowledged = false;
        if (step.Operation is "corrupt" or "replay" or "restore")
        {
            if (step.Operation == "corrupt") await CorruptImage(step);
            else if (step.Operation == "replay") await ReplayPage(step);
            else await RestoreBackup(step);
            return;
        }
        if (step.Operation == "crash")
        {
            fs.Crash(args[0] == "power", Parameter(args, "survive", 0), Parameter(args, "tear", 0));
            await Recover(step.Index, "none"); return;
        }
        if (trace.Service && !SelectProcess(step) && step.Operation is "open" or "backup")
        {
            json.Start(step.Index, step.Operation);
            if (step.Operation == "open") json.Text("sid", args[0]);
            else json.Number("pid", args.Length > 1 ? ProcessNumber(args[1]) : 0);
            json.Flag("skip", true); Emit(); return;
        }
        json.Start(step.Index, step.Operation);
        if (step.Operation is "open" or "on" or "commit" or "rollback" or "close") json.Text("sid", args[0]);
        try
        {
            switch (step.Operation)
            {
                case "spawn": await SpawnProcess(ProcessNumber(args[0])); break;
                case "disconnect": await DisconnectProcess(ProcessNumber(args[0])); break;
                case "kill":
                    await EndProcess(ProcessNumber(args[0]), true);
                    if (processes.Count == 0) { _ = json.End(); await Recover(step.Index, "none"); return; }
                    break;
                case "exit": await EndProcess(ProcessNumber(args[0]), false); break;
                case "attach": json.Text("db", args[0]); await AttachDomain(args[0]); break;
                case "backup": await Backup(args[0]); break;
                case "open":
                {
                    if (args.Length > 2 && !trace.Service) throw new InvalidDataException("process handle outside service trace");
                    if (sessions.ContainsKey(args[0]) || (args[1] == "write" && sessions.Values.Any(handle => handle.Write))) throw new InvalidDataException("trace opens a duplicate session or second writer");
                    var write = args[1] == "write";
                    if (!write && args[1] != "read") throw new InvalidDataException("invalid session mode");
                    var session = write ? await database.OpenWriteSessionAsync(SessionOptions((ulong)step.Index)) : await database.OpenReadSessionAsync(SessionOptions((ulong)step.Index));
                    if (trace.Service) ObserveService(await SimulationRows.Digest(session, fs));
                    var versions = AttachmentVersions();
                    sessions.Add(args[0], new(session, write, database.Version, versions, selectedProcess));
                    json.Number("version", database.Version); WriteAttachmentVersions(versions); break;
                }
                case "on":
                    if (!sessions.TryGetValue(args[0], out var querying)) { json.Flag("skip", true); break; }
                    var rows = await SimulationRows.Query(querying.Session, step.Sql!);
                    json.Number("rows", rows.Rendered.Length).Number("affected", rows.Affected).Text("hash", rows.Hash).Number("cost", rows.Cost); break;
                case "commit":
                    if (!sessions.Remove(args[0], out var committing)) { json.Flag("skip", true); break; }
                    try
                    {
                        try { pendingDigest = await SimulationRows.Digest(committing.Session, fs); }
                        catch (SqlException error) when (error.SqlState != SqlStates.DataCorrupted) { }
                        try { await PrepareAttachedCommit(committing.Session); }
                        catch (SqlException error) when (error.SqlState != SqlStates.DataCorrupted) { }
                        var beforePublication = database.PublishedStateForTesting();
                        var beforeCommit = beforePublication.Storage;
                        var beforeAttachmentVersions = AttachmentVersions(published: true);
                        var attachedBefore = attached.ToDictionary(pair => pair.Key, pair => pair.Value.Database.PublishedStateForTesting().Storage, StringComparer.Ordinal);
                        try { await committing.Session.CommitAsync(); }
                        catch
                        {
                            SimulationStorageChecks.Require(database.PublishedStateForTesting().Version == beforePublication.Version,
                                "failed commit published the main generation");
                            foreach (var (name, before) in beforeAttachmentVersions)
                                SimulationStorageChecks.Require(attached[name].Database.PublishedStateForTesting().Version == before,
                                    "failed commit published attachment " + name);
                            throw;
                        }
                        acknowledged = true;
                        var publication = database.PublishedStateForTesting();
                        var acknowledgedAttachments = AttachmentVersions(published: true);
                        version = publication.Version;
                        SimulationStorageChecks.Require(version == start || start < ulong.MaxValue && version == start + 1,
                            "commit published an invalid generation transition");
                        SimulationStorageChecks.Require(version == start || pendingDigest is not null,
                            "commit published changes after its in-flight digest failed");
                        if (System.Environment.GetEnvironmentVariable("JED_SIM_OUT") is { Length: > 0 } stateDirectory)
                        {
                            var state = publication.Storage;
                            File.AppendAllText(Path.Combine(stateDirectory, "commits.jsonl"), FormattableString.Invariant($"{{\"step\":{step.Index},\"version\":{version},\"ReclamationLive\":{state.ReclamationLive},\"FreeGeneration\":{state.FreeGeneration},\"free\":{state.FreePages.Count},\"PageCount\":{state.Meta.PageCount},\"oldest\":{publication.Oldest}}}\n"));
                        }
                        var digest = version == start ? committedDigest :
                            await ReadCommitDigest(fs, trace.Synchronous == "off", trace.Service, pendingDigest!, Digest);
                        SimulationStorageChecks.Require(pendingDigest is null || pendingDigest == digest, "acknowledged commit differs from pending session");
                        SimulationStorageChecks.Require(version != start || history[start].Contains(digest), "unchanged commit changed contents");
                        committedDigest = digest;
                        AddHistory(version, digest);
                        if (version != start && fs.EntryDurable("sim.jed"))
                        {
                            var committed = (trace.Service ? roles.DatabaseOf("sim.jed") ?? database : database).PublishedStateForTesting().Storage;
                            floor = Math.Max(floor, committed.Batch?.FloorTransactionId ?? version);
                        }
                        if (!(trace.Synchronous == "off" && fs.Fired != "none") && (!trace.Service || database.FileRole == "owner"))
                            await Oracle(() => SimulationStorageChecks.Live(database));
                        if (version != start && !trace.Service) CheckGrowth(beforeCommit, active is null && sessions.Count == 0);
                        json.Number("version", version);
                        await FinishAttachedCommit(attachedBefore, acknowledgedAttachments, active is null && sessions.Count == 0);
                        if (trace.Service) json.Text("role", database.FileRole); break;
                    }
                    finally { await committing.Session.DisposeAsync(); }
                case "rollback":
                case "close":
                    if (!sessions.Remove(args[0], out var closing)) { json.Flag("skip", true); break; }
                    if (!closing.Write && step.Operation == "close")
                    {
                        try
                        {
                            var pinned = await SimulationRows.Digest(closing.Session, fs);
                            SimulationStorageChecks.Require(history[closing.Version].Contains(pinned), "reader snapshot changed while pinned");
                            await CheckAttachedReader(closing);
                        }
                        catch (SqlException error) when (trace.Service && error.SqlState == SqlStates.ConnectionFailure)
                        { json.Flag("lost", true); }
                    }
                    try { if (step.Operation == "rollback") await closing.Session.RollbackAsync(); }
                    finally { await closing.Session.DisposeAsync(); }
                    break;
                case "clock": clock = checked(clock + long.Parse(args[0], CultureInfo.InvariantCulture)); break;
                case "reopen":
                    await CloseClean(); database = await Database.OpenFileAsync("sim.jed", processOptions = Options()); Configure(); await ReattachDomains();
                    if (trace.Service) processes.Add(0, new(database, processOptions));
                    version = database.Version; var reopened = await Digest();
                    SimulationStorageChecks.Require(history[version].Contains(reopened), "clean reopen changed contents");
                    json.Number("version", version).Text("digest", reopened); await AttachedFields(false, summary: true); break;
                case "change_key":
                    if (args.Length > 1 && args[1] != "main") { await AttachmentMaintenance(step, args[1]); break; }
                    if (!trace.Encrypted) { json.Flag("skip", true); break; }
                    var replacementKey = int.Parse(args[0], CultureInfo.InvariantCulture);
                    if (replacementKey is < 1 or > 3 || args.Length > 1 && args[1] != "main") throw new InvalidDataException("invalid credential target");
                    keyAttempt = new(key, replacementKey, !fs.EntryDurable("sim.jed"));
                    await database.ChangeCredentialAsync(keys[replacementKey]); keyAttempt = keyAttempt with { Acknowledged = true }; key = replacementKey; json.Number("key", key); break;
                case "rekey":
                case "compact":
                    var targetName = step.Operation == "compact" ? args.FirstOrDefault() ?? "main" : args.Length > 1 ? args[1] : "main";
                    if (targetName != "main") { await AttachmentMaintenance(step, targetName); break; }
                    if (step.Operation == "rekey" && !trace.Encrypted) { json.Flag("skip", true); break; }
                    pendingDigest = committedDigest; AddHistory(start + 1, pendingDigest);
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
                    if (!trace.Service || database.FileRole == "owner") await Oracle(() => SimulationStorageChecks.Live(database));
                    await Fields(false); await AttachedFields(false); break;
                default: throw new NotSupportedException("simulation operation " + step.Operation);
            }
        }
        catch (SimulationFileSystem.CrashException)
        {
            if (acknowledged) throw new InvalidDataException("acknowledged commit failed a post-publication oracle");
            json.Text("error", SqlStates.IoError); RecordAttempt(start);
        }
        catch (SqlException error)
        {
            if (acknowledged) throw new InvalidDataException("acknowledged commit failed a post-publication oracle", error);
            if (System.Environment.GetEnvironmentVariable("JED_SIM_DIAGNOSTICS") == "1") Console.Error.WriteLine($"simulation step {step.Index}: {error}");
            if (error.SqlState == SqlStates.DataCorrupted) throw new InvalidDataException("unexpected corruption outside corruption mode", error);
            json.Text("error", error.SqlState); RecordAttempt(start);
        }
        Emit();
        if (trace.Service)
        {
            await network.SettleAsync();
            if (acknowledged && database.FileRole == "client" && pendingDigest is not null)
                await ObserveMaintenance(step.Index, pendingDigest, start, attachmentStarts);
        }
        if (active?.Kind == "disconnect")
        { json.Start(step.Index, "disconnected").Text("fired", network.Disconnected ? "frame" : "end"); Emit(); return; }
        // Growth can accidentally fsync a batch's pending slot; the next batch
        // commit may overwrite it. Only a step that began outside that batch
        // proves its starting generation durable (canonical step oracle §8.2).
        if (fs.DurableBarrier("sim.jed") && !mainBatchOpen) floor = Math.Max(floor, start);
        foreach (var state in attached.Values)
            if (fs.DurableBarrier(state.File) && !attachmentBatches.GetValueOrDefault(state.Name) && attachmentStarts.TryGetValue(state.Name, out var old))
                state.Floor = Math.Max(state.Floor, old);
        if (active is not null)
        {
            fs.EndFaultedStep(); var fired = fs.Fired;
            if (trace.Service && active.Kind != "power_loss" && await LoseOwners(step.Index, fired)) return;
            await Recover(step.Index, fired);
        }
    }
    private void CheckGrowth(StorageRecovered before, bool quiet)
        => CheckGrowth(database, "sim.jed", before, quiet, ref growthBound, ref previousReclaimed);
    private void CheckGrowth(Database selected, string path, StorageRecovered before, bool quiet, ref long bound, ref bool previouslyReclaimed)
    {
        var after = selected.PublishedStateForTesting().Storage;
        var firstBody = after.Encrypted is null ? 2u : 4u;
        var written = fs.WrittenPages(path).Where(page => page >= firstBody).ToArray();
        var threshold = Math.Max(16, 2 * bound);
        var reclaim = quiet && before.Meta.PageCount - before.FreePages.Count > threshold;
        if (quiet)
        {
            if (reclaim) SimulationStorageChecks.Require((long)after.Meta.PageCount - firstBody - after.Meta.LivePages - after.FreePages.Count <= written.Length,
                "quiet commit exceeded reclamation threshold without reclaiming unreachable pages");
            var reusable = before.Batch is null ? before.FreePages : new SortedSet<uint>(before.FreePages
                .Intersect(before.Batch.FloorFree).Except(before.Batch.Pending.Select(entry => entry.Page)));
            if (previouslyReclaimed) SimulationStorageChecks.Require((long)after.Meta.PageCount - before.Meta.PageCount <= Math.Max(0, written.Length - reusable.Count),
                "commit grew without first reusing the reclaimed free list");
            var tail = before.Meta.PageCount;
            while (tail > firstBody && reusable.Contains(tail - 1)) tail--;
            var high = written.Length == 0 ? 0 : written.Max() + 1;
            SimulationStorageChecks.Require(after.Meta.PageCount == Math.Max(tail, high), "quiet commit did not use canonical trailing truncation/high-water");
            if (after.Batch is null && after.Meta.PageCount < before.Meta.PageCount)
            {
                var allowance = Math.Clamp((long)after.Meta.PageCount, Math.Max(1, 16 * 1024 / trace.PageSize), Math.Max(1, 1024 * 1024 / trace.PageSize));
                SimulationStorageChecks.Require(fs.Image(path).LongLength <= ((long)after.Meta.PageCount + allowance) * trace.PageSize,
                    "trailing truncation retained more than one growth step of slack");
            }
        }
        bound = Math.Max(bound, (long)after.Meta.LivePages + written.Length); previouslyReclaimed = reclaim;
    }
    private void RecordAttempt(ulong start)
    { if (pendingDigest is not null && fs.WrittenPages("sim.jed").Count > 0) AddHistory(start + 1, pendingDigest); RecordAttachedAttempts(); }
    private async Task Oracle(Func<Task> action)
    { var before = fs.OracleReads; fs.OracleReads = true; try { await action(); } finally { fs.OracleReads = before; } }
    private async Task Fields(bool recovered, byte[]? recoveredImage = null)
    {
        var digest = await Digest(); var bytes = recoveredImage ?? fs.Image("sim.jed");
        if (System.Environment.GetEnvironmentVariable("JED_SIM_OUT") is { Length: > 0 } output)
        {
            Directory.CreateDirectory(output);
            File.WriteAllBytes(Path.Combine(output, $"{Path.GetFileNameWithoutExtension(trace.Name)}.csharp-native.step{fs.Step}.img"), bytes);
        }
        var image = await SimulationStorageChecks.Image(bytes, version, key < 0 ? null : keys[key].Value);
        json.Number(recovered ? "recovered" : "version", version).Text("digest", digest).Number("live", image.Live).Number("pages", image.Pages).Number("free", image.Free).Text("image", image.Image);
    }
    private async Task Recover(int step, string fired)
    {
        if (trace.Service) await CrashService();
        sessions.Clear(); fs.Restart();
        var image = fs.Image("sim.jed");
        var attachmentImages = new Dictionary<string, byte[]>(StringComparer.Ordinal);
        foreach (var state in attached.Values)
            if (await fs.ExistsAsync(state.File)) attachmentImages.Add(state.Name, fs.Image(state.File));
        VerifyKeyAttempt(image);
        var options = processOptions = Options(); var oldKey = key; var opened = false;
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
        if (trace.Service) processes.Add(0, new(database, processOptions));
        if (key != oldKey || keyAttempt is not null) { json.Start(step, "unlocked").Number("key", key); Emit(); }
        Configure(); version = database.Version;
        await ReattachDomains();
        SimulationStorageChecks.Require(version >= floor && version <= attempted, $"recovered generation {version} outside durability window {floor}..{attempted}");
        var digest = await Digest();
        SimulationStorageChecks.Require(history.TryGetValue(version, out var values) && values.Contains(digest), "recovered unrecorded contents");
        json.Start(step, "crash").Text("fired", fired); await Fields(true, image); await AttachedFields(true, attachmentImages); Emit();
    }
    private bool Unlocks(byte[] image, int candidate)
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
    private void VerifyKeyAttempt(byte[] image)
    {
        if (keyAttempt is not { } change) return;
        VerifyKeyAttempt(image, change);
    }
    private void VerifyKeyAttempt(byte[] image, KeyAttempt change)
    {
        var oldWorks = Unlocks(image, change.Old); var newWorks = Unlocks(image, change.Next);
        SimulationStorageChecks.Require(change.Old == change.Next ? oldWorks :
            oldWorks && !newWorks && !change.Acknowledged || !oldWorks && newWorks || change.EntryPending && !oldWorks && !newWorks,
            "credential recovery violates atomic replacement or acknowledged durability");
    }
}
