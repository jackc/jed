using Jed;

internal sealed partial class SimulationRunner
{
    private sealed class AttachmentState(string name, int key)
    {
        internal readonly string Name = name;
        internal string File => Name + ".jed";
        internal int Key = key;
        internal ulong Version = 1, Floor, Attempted = 1;
        internal long Growth;
        internal bool Reclaimed;
        internal Database Database = null!;
        internal string? Pending;
        internal string CommittedDigest = "";
        internal KeyAttempt? KeyAttempt;
        internal readonly Dictionary<ulong, HashSet<string>> History = [];
        internal void Remember(ulong version, string digest)
        {
            if (!History.TryGetValue(version, out var values)) History.Add(version, values = []);
            values.Add(digest); Attempted = Math.Max(Attempted, version);
        }
    }
    private readonly SortedDictionary<string, AttachmentState> attached = new(Utf8NameComparer.Instance);
    private DatabaseFileOptions processOptions = null!;

    private void BindCurrentAttachments()
    {
        var registry = database.AcquireAttachmentRegistry();
        try
        {
            foreach (var item in registry)
                if (attached.TryGetValue(item.Name, out var state)) state.Database = item.Database;
        }
        finally { database.ReleaseAttachmentRegistry(); }
    }
    private async Task AttachEverywhere(AttachmentState state)
    {
        var owner = roles.OwnerOf("sim.jed") ?? processes.First().Key;
        _ = SelectServiceProcess(owner);
        if (roles.OwnerOf("sim.jed") is null) ObserveService(await Digest());
        foreach (var process in new[] { owner }.Concat(processes.Keys.Where(pid => pid != owner)))
        {
            _ = SelectServiceProcess(process);
            if (!database.AttachedDatabases.Contains(state.Name)) await OpenAttachment(state);
            await network.SettleAsync();
        }
        _ = SelectServiceProcess(owner);
    }

    private async Task<string> AttachmentDigest(AttachmentState state)
    {
        await using var session = database.CreateSession(SessionOptions(fs.Step) with { MaxQueryMemoryBytes = 0 });
        return state.CommittedDigest = await SimulationRows.Digest(session, fs, state.Name);
    }
    private async Task AttachDomain(string name)
    {
        if (sessions.Values.Any(handle => handle.Write)) throw new InvalidDataException("trace attaches with an open writer");
        if (attached.TryGetValue(name, out var existing) && database.PublishedStateForTesting().Attachments.Contains(name) &&
            (!trace.Service || processes.Values.All(process => process.Database.PublishedStateForTesting().Attachments.Contains(name))))
        {
            // An already attached domain only reports its recorded version.
            // Opening an oracle read here could elect a new owner and alter the
            // next process's role, unlike the canonical stepAttach operation.
            json.Number("version", existing.Version); return;
        }
        if (!attached.TryGetValue(name, out var state))
        {
            state = new(name, trace.Encrypted ? 0 : -1); attached.Add(name, state);
            var empty = string.Concat(new[] { "jed_tables", "jed_columns", "jed_indexes", "jed_constraints", "jed_sequences" }
                .Select(relation => "SELECT * FROM " + name + "." + relation + "\n\n\n"));
            state.Remember(1, SimulationRows.Fold(empty));
        }
        if (!await fs.ExistsAsync(state.File))
        {
            await using (var created = await Database.CreateFileAsync(state.File,
                Options() with { Credential = state.Key < 0 ? null : keys[state.Key], RoleHost = null, Locking = FileLocking.None }, trace.PageSize)) { }
            state.Floor = 1;
        }
        if (trace.Service) await AttachEverywhere(state);
        else if (!database.AttachedDatabases.Contains(name)) await OpenAttachment(state);
        state.Version = state.Database.Version;
        var digest = await AttachmentDigest(state);
        VerifyAttachment(state, digest);
        json.Number("version", state.Version);
    }
    private async Task OpenAttachment(AttachmentState state)
    {
        await database.AttachFileAsync(state.Name, state.File,
            processOptions with { Credential = state.Key < 0 ? null : keys[state.Key] });
        var registry = database.AcquireAttachmentRegistry();
        try { state.Database = registry.Single(item => item.Name == state.Name).Database; }
        finally { database.ReleaseAttachmentRegistry(); }
        state.Database.MaxStorageBytes = trace.MaxStorage;
        state.Database.SetWorkMem(trace.WorkMemory);
        var storage = state.Database.CommittedStorageForTesting();
        state.Growth = storage.Meta.PageCount - storage.FreePages.Count; state.Reclaimed = false;
    }
    private async Task ReattachDomains()
    {
        foreach (var state in attached.Values.ToArray())
        {
            if (!await fs.ExistsAsync(state.File))
            {
                SimulationStorageChecks.Require(state.Floor == 0, "durable attachment file disappeared");
                attached.Remove(state.Name); continue;
            }
            if (state.KeyAttempt is { } change) VerifyKeyAttempt(fs.Image(state.File), change);
            var oldKey = state.Key;
            var unlocked = false;
            foreach (var candidate in new[] { state.Key, -1, 0, 1, 2, 3 }.Distinct())
            {
                if (!trace.Encrypted && candidate != -1) continue;
                state.Key = candidate;
                try { await OpenAttachment(state); unlocked = true; break; }
                catch (SqlException error) when (error.SqlState is SqlStates.InvalidPassword or SqlStates.CredentialRequired or SqlStates.CredentialNotApplicable) { }
            }
            SimulationStorageChecks.Require(unlocked, "attachment recovery cannot unlock with an authorized key");
            if (oldKey != state.Key || state.KeyAttempt is not null)
            { json.Start(checked((int)fs.Step), "unlocked").Text("db", state.Name).Number("key", state.Key); Emit(); }
        }
    }
    private void VerifyAttachment(AttachmentState state, string digest)
    {
        SimulationStorageChecks.Require(state.Version >= state.Floor && state.Version <= state.Attempted,
            $"attachment {state.Name} recovered outside its durability window");
        SimulationStorageChecks.Require(state.History.TryGetValue(state.Version, out var values) && values.Contains(digest),
            $"attachment {state.Name} recovered unrecorded contents");
    }
    private Dictionary<string, ulong> AttachmentVersions(bool published = false)
    {
        var names = (published ? database.PublishedStateForTesting().Attachments : database.AttachedDatabases).ToHashSet(StringComparer.Ordinal);
        return attached.Where(pair => names.Contains(pair.Key)).ToDictionary(pair => pair.Key,
            pair => published ? pair.Value.Database.PublishedStateForTesting().Version : pair.Value.Database.Version, StringComparer.Ordinal);
    }
    private void WriteAttachmentVersions(Dictionary<string, ulong> versions)
    {
        if (attached.Count == 0) return;
        json.Object("dbs");
        foreach (var state in attached.Values)
            if (versions.TryGetValue(state.Name, out var value)) json.Number(state.Name, value);
        json.EndObject();
    }
    private async Task PrepareAttachedCommit(Session session)
    {
        foreach (var state in attached.Values) state.Pending = await SimulationRows.Digest(session, fs, state.Name);
    }
    private async Task FinishAttachedCommit(Dictionary<string, StorageRecovered> before, Dictionary<string, ulong> acknowledged, bool quiet)
    {
        foreach (var state in attached.Values)
        {
            if (!acknowledged.TryGetValue(state.Name, out var published)) continue;
            var old = state.Version; state.Version = published;
            SimulationStorageChecks.Require(state.Version == old || old < ulong.MaxValue && state.Version == old + 1,
                "attachment commit published an invalid generation transition");
            SimulationStorageChecks.Require(state.Version == old || state.Pending is not null,
                "attachment commit published changes after its in-flight digest failed");
            var digest = state.Version == old ? state.CommittedDigest :
                await ReadCommitDigest(fs, trace.Synchronous == "off", trace.Service, state.Pending!, () => AttachmentDigest(state));
            SimulationStorageChecks.Require(state.Pending is null || state.Pending == digest, "attachment commit differs from pending session");
            SimulationStorageChecks.Require(old != state.Version || state.History[old].Contains(digest), "unchanged attachment commit changed contents");
            state.CommittedDigest = digest;
            state.Remember(state.Version, digest);
            if (state.Version != old && fs.EntryDurable(state.File))
                state.Floor = Math.Max(state.Floor, (trace.Service ? roles.DatabaseOf(state.File) ?? state.Database : state.Database).PublishedStateForTesting().Storage.Batch?.FloorTransactionId ?? state.Version);
            if (!(trace.Synchronous == "off" && fs.Fired != "none") && (!trace.Service || state.Database.FileRole == "owner"))
                await Oracle(() => SimulationStorageChecks.Live(state.Database));
            if (state.Version != old && !trace.Service) CheckGrowth(state.Database, state.File, before[state.Name], quiet, ref state.Growth, ref state.Reclaimed);
        }
        WriteAttachmentVersions(attached.ToDictionary(pair => pair.Key, pair => pair.Value.Version, StringComparer.Ordinal));
    }
    private void RecordAttachedAttempts()
    {
        foreach (var state in attached.Values)
            if (state.Pending is not null && fs.WrittenPages(state.File).Count > 0) state.Remember(state.Version + 1, state.Pending);
    }
    private async Task CheckAttachedReader(Handle handle)
    {
        foreach (var (name, version) in handle.Attachments)
        {
            var digest = await SimulationRows.Digest(handle.Session, fs, name);
            SimulationStorageChecks.Require(attached[name].History[version].Contains(digest), "attachment reader snapshot changed while pinned");
        }
    }
    private async Task AttachedFields(bool recovered, Dictionary<string, byte[]>? images = null, bool summary = false)
    {
        var names = database.AttachedDatabases.ToHashSet(StringComparer.Ordinal);
        var active = attached.Values.Where(state => names.Contains(state.Name)).ToArray();
        if (active.Length == 0) return;
        json.Object("dbs");
        foreach (var state in active)
        {
            state.Version = state.Database.Version;
            var digest = await AttachmentDigest(state); VerifyAttachment(state, digest);
            json.Object(state.Name).Number(recovered ? "recovered" : "version", state.Version).Text("digest", digest);
            if (!summary)
            {
                if (!trace.Service || state.Database.FileRole == "owner") await Oracle(() => SimulationStorageChecks.Live(state.Database));
                var bytes = images is null ? fs.Image(state.File) : images[state.Name];
                var checkedImage = await SimulationStorageChecks.Image(bytes, state.Version, state.Key < 0 ? null : keys[state.Key].Value);
                json.Number("live", checkedImage.Live).Number("pages", checkedImage.Pages).Number("free", checkedImage.Free).Text("image", checkedImage.Image);
                if (System.Environment.GetEnvironmentVariable("JED_SIM_OUT") is { Length: > 0 } output)
                    File.WriteAllBytes(Path.Combine(output, $"{Path.GetFileNameWithoutExtension(trace.Name)}.csharp-native.step{fs.Step}.{state.Name}.img"), bytes);
            }
            json.EndObject();
        }
        json.EndObject();
    }
}
