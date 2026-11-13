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
        internal readonly Dictionary<ulong, HashSet<string>> History = [];
        internal void Remember(ulong version, string digest)
        {
            if (!History.TryGetValue(version, out var values)) History.Add(version, values = []);
            values.Add(digest); Attempted = Math.Max(Attempted, version);
        }
    }
    private readonly SortedDictionary<string, AttachmentState> attached = new(Utf8NameComparer.Instance);
    private DatabaseFileOptions processOptions = null!;

    private async Task<string> AttachmentDigest(AttachmentState state)
    {
        await using var session = database.CreateSession(SessionOptions(fs.Step) with { MaxQueryMemoryBytes = 0 });
        return await SimulationRows.Digest(session, fs, state.Name);
    }
    private async Task AttachDomain(string name)
    {
        if (sessions.Values.Any(handle => handle.Write)) throw new InvalidDataException("trace attaches with an open writer");
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
                Options() with { Credential = state.Key < 0 ? null : keys[state.Key] }, trace.PageSize)) { }
            state.Floor = 1;
        }
        if (!database.AttachedDatabases.Contains(name)) await OpenAttachment(state);
        state.Version = state.Database.Version;
        var digest = await AttachmentDigest(state);
        VerifyAttachment(state, digest);
        json.Text("db", name).Number("version", state.Version);
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
            await OpenAttachment(state);
        }
    }
    private void VerifyAttachment(AttachmentState state, string digest)
    {
        SimulationStorageChecks.Require(state.Version >= state.Floor && state.Version <= state.Attempted,
            $"attachment {state.Name} recovered outside its durability window");
        SimulationStorageChecks.Require(state.History.TryGetValue(state.Version, out var values) && values.Contains(digest),
            $"attachment {state.Name} recovered unrecorded contents");
    }
    private Dictionary<string, ulong> AttachmentVersions() => attached.ToDictionary(pair => pair.Key, pair => pair.Value.Database.Version, StringComparer.Ordinal);
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
    private async Task FinishAttachedCommit(Dictionary<string, StorageRecovered> before, bool quiet)
    {
        foreach (var state in attached.Values)
        {
            var old = state.Version; state.Version = state.Database.Version;
            var digest = await AttachmentDigest(state);
            SimulationStorageChecks.Require(state.Pending is null || state.Pending == digest, "attachment commit differs from pending session");
            SimulationStorageChecks.Require(old != state.Version || state.History[old].Contains(digest), "unchanged attachment commit changed contents");
            state.Remember(state.Version, digest);
            if (state.Version != old && fs.EntryDurable(state.File))
                state.Floor = Math.Max(state.Floor, state.Database.CommittedStorageForTesting().Batch?.FloorTransactionId ?? state.Version);
            await Oracle(() => SimulationStorageChecks.Live(state.Database));
            if (state.Version != old) CheckGrowth(state.Database, state.File, before[state.Name], quiet, ref state.Growth, ref state.Reclaimed);
        }
        WriteAttachmentVersions(AttachmentVersions());
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
        if (attached.Count == 0) return;
        json.Object("dbs");
        foreach (var state in attached.Values)
        {
            state.Version = state.Database.Version;
            var digest = await AttachmentDigest(state); VerifyAttachment(state, digest);
            json.Object(state.Name).Number(recovered ? "recovered" : "version", state.Version).Text("digest", digest);
            if (!summary)
            {
                await Oracle(() => SimulationStorageChecks.Live(state.Database));
                var bytes = images is null ? fs.Image(state.File) : images[state.Name];
                var checkedImage = await SimulationStorageChecks.Image(bytes, state.Version, state.Key < 0 ? null : keys[state.Key].Value);
                json.Number("live", checkedImage.Live).Number("pages", checkedImage.Pages).Number("free", checkedImage.Free).Text("image", checkedImage.Image);
                if (Environment.GetEnvironmentVariable("JED_SIM_OUT") is { Length: > 0 } output)
                    File.WriteAllBytes(Path.Combine(output, $"{Path.GetFileNameWithoutExtension(trace.Name)}.csharp-native.step{fs.Step}.{state.Name}.img"), bytes);
            }
            json.EndObject();
        }
        json.EndObject();
    }
}
