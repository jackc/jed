using Jed;

internal sealed partial class SimulationRunner
{
    private sealed record ProcessState(Database Database, DatabaseFileOptions Options);
    private readonly SimulationNetwork network = new();
    private SimulationRoleRegistry roles = null!;
    private readonly SortedDictionary<int, ProcessState> processes = [];
    private int selectedProcess;
    private bool ownerLost;
    private static int ProcessNumber(string text)
    {
        if (!text.StartsWith('p') || !int.TryParse(text.AsSpan(1), out var value) || value < 0)
            throw new InvalidDataException("invalid simulation process");
        return value;
    }
    private bool SelectProcess(SimulationStep step)
    {
        var pid = step.Operation == "open" ? step.Arguments.Length > 2 ? ProcessNumber(step.Arguments[2]) : 0 :
            step.Operation == "backup" && step.Arguments.Length > 1 ? ProcessNumber(step.Arguments[1]) :
            (step.Operation is "on" or "commit" or "rollback" or "close") && sessions.TryGetValue(step.Arguments[0], out var handle) ? handle.Process :
            processes.Count == 0 ? 0 : processes.First().Key;
        return SelectServiceProcess(pid);
    }
    private bool SelectServiceProcess(int pid)
    {
        if (!processes.TryGetValue(pid, out var process)) return false;
        selectedProcess = pid; database = process.Database; processOptions = process.Options;
        BindCurrentAttachments();
        return true;
    }
    private async Task SpawnProcess(int pid)
    {
        json.Number("pid", pid);
        if (processes.ContainsKey(pid)) { json.Flag("skip", true); return; }
        selectedProcess = pid;
        var options = Options();
        var opened = await Database.OpenFileAsync("sim.jed", options);
        processes.Add(pid, new(opened, options)); database = opened; processOptions = options; Configure();
        await ReattachDomains();
        var digest = await Digest(); var observed = database.Version;
        SimulationStorageChecks.Require(observed >= floor && observed <= attempted && history.TryGetValue(observed, out var values) && values.Contains(digest),
            "spawned process observed unrecorded contents or generation");
        json.Number("version", observed); WriteAttachmentVersions(AttachmentVersions());
    }
    private async Task DisconnectProcess(int pid)
    {
        json.Number("pid", pid);
        if (!processes.TryGetValue(pid, out var process) || !process.Database.HasPageClient) json.Flag("skip", true);
        else _ = network.Disconnect(pid, Path.GetFullPath("sim.jed"));
        await network.SettleAsync();
    }
    private async Task EndProcess(int pid, bool kill)
    {
        json.Number("pid", pid);
        if (!processes.Remove(pid, out var process)) { json.Flag("skip", true); return; }
        await FinishProcess(pid, process, kill);
    }
    private async Task FinishProcess(int pid, ProcessState process, bool kill)
    {
        foreach (var (name, handle) in sessions.Where(pair => pair.Value.Process == pid).OrderBy(pair => pair.Key, StringComparer.Ordinal).ToArray())
        {
            sessions.Remove(name);
            if (!kill) await handle.Session.DisposeAsync();
        }
        if (kill)
        {
            if (roles.OwnerOf("sim.jed") == pid) ownerLost = true;
            await roles.KillAsync(pid);
            if (process.Database.FileRole == "client") await process.Database.AbortInjectedHostAsync();
        }
        else
        {
            await process.Database.DisposeAsync();
            network.Kill(pid);
        }
        await network.SettleAsync();
    }
    private async Task CloseService()
    {
        foreach (var pid in processes.Keys.ToArray())
        {
            var process = processes[pid]; processes.Remove(pid);
            await FinishProcess(pid, process, false);
        }
        await network.SettleAsync();
        selectedProcess = 0;
    }
    private async Task CrashService()
    {
        foreach (var pid in processes.Keys.ToArray()) await roles.KillAsync(pid);
        foreach (var process in processes.Values)
            if (process.Database.FileRole == "client") await process.Database.AbortInjectedHostAsync();
        processes.Clear(); sessions.Clear(); await network.SettleAsync();
        selectedProcess = 0; roles = new(network, () => clock, increment => clock = checked(clock + increment));
    }
    private async Task<bool> LoseOwners(int step, string fired)
    {
        var owners = new[] { roles.OwnerOf("sim.jed") }.Concat(attached.Values.Select(state => roles.OwnerOf(state.File)))
            .Where(pid => pid is not null).Select(pid => pid!.Value).Distinct().Order().ToArray();
        if (owners.Length == 0) owners = [selectedProcess];
        if (owners.Length >= processes.Count) return false;
        fs.Restart();
        foreach (var pid in owners)
        {
            await roles.KillAsync(pid); processes.Remove(pid);
            foreach (var name in sessions.Where(pair => pair.Value.Process == pid).Select(pair => pair.Key).ToArray()) sessions.Remove(name);
            json.Start(step, "killed").Number("pid", pid).Text("fired", fired); Emit();
        }
        ownerLost = true;
        if (keyAttempt is not null)
        {
            var image = fs.Image("sim.jed");
            var candidate = new[] { key, -1, 0, 1, 2, 3 }.Distinct().FirstOrDefault(candidate => Unlocks(image, candidate), -2);
            SimulationStorageChecks.Require(candidate != -2, "dead owner's image cannot be unlocked by any simulation key");
            VerifyKeyAttempt(image);
            key = candidate; json.Start(step, "unlocked").Number("key", key); Emit();
        }
        foreach (var attachment in attached.Values)
        {
            if (attachment.KeyAttempt is not { } change || !await fs.ExistsAsync(attachment.File)) continue;
            var image = fs.Image(attachment.File);
            var candidate = new[] { attachment.Key, -1, 0, 1, 2, 3 }.Distinct()
                .FirstOrDefault(candidate => Unlocks(image, candidate), -2);
            SimulationStorageChecks.Require(candidate != -2, "dead owner's attachment cannot be unlocked by any simulation key");
            VerifyKeyAttempt(image, change); attachment.Key = candidate;
            json.Start(step, "unlocked").Text("db", attachment.Name).Number("key", candidate); Emit();
        }
        return true;
    }
    private void ObserveService(string digest)
    {
        if (!trace.Service) return;
        var observed = database.Version;
        SimulationStorageChecks.Require((observed >= version || ownerLost && observed >= floor) && observed <= attempted &&
            history.TryGetValue(observed, out var values) && values.Contains(digest), "service observed a generation outside its recorded history");
        version = observed; committedDigest = digest; ownerLost = false;
    }
    private async Task ObserveMaintenance(int step, string digest, ulong previousVersion, Dictionary<string, ulong> previousAttachments)
    {
        if (fs.IsDead)
        {
            // A client ACK may precede the owner's failed maintenance commit.
            // Admit only that next generation with unchanged contents; recovery
            // must still validate the complete image and its recorded digest.
            if (version > previousVersion) AddHistory(version + 1, digest);
            foreach (var attachment in attached.Values)
                if (attachment.Database.FileRole == "client" && previousAttachments.TryGetValue(attachment.Name, out var previous) && attachment.Version > previous)
                    attachment.Remember(attachment.Version + 1, attachment.CommittedDigest);
            return;
        }
        if (roles.DatabaseOf("sim.jed") is { } owner)
        {
            var state = owner.CommittedStorageForTesting();
            var next = state.Meta.TransactionId;
            if (next > version)
            {
                await using var reader = owner.CreateSession(SessionOptions(fs.Step) with { MaxQueryMemoryBytes = 0 });
                SimulationStorageChecks.Require(await SimulationRows.Digest(reader, fs) == digest, "owner maintenance changed committed contents");
                for (var generation = version + 1; generation <= next; generation++) AddHistory(generation, digest);
                version = next;
                if (fs.EntryDurable("sim.jed")) floor = Math.Max(floor, state.Batch?.FloorTransactionId ?? next);
                json.Start(step, "maintenance").Number("version", next); Emit();
            }
        }
        foreach (var attachment in attached.Values)
        {
            if (roles.DatabaseOf(attachment.File) is not { } attachedOwner) continue;
            var state = attachedOwner.CommittedStorageForTesting(); var next = state.Meta.TransactionId;
            if (next <= attachment.Version) continue;
            var contents = await AttachmentDigest(attachment);
            SimulationStorageChecks.Require(attachment.History[attachment.Version].Contains(contents), "attachment maintenance changed committed contents");
            for (var generation = attachment.Version + 1; generation <= next; generation++) attachment.Remember(generation, contents);
            attachment.Version = next;
            if (fs.EntryDurable(attachment.File)) attachment.Floor = Math.Max(attachment.Floor, state.Batch?.FloorTransactionId ?? next);
            json.Start(step, "maintenance").Text("db", attachment.Name).Number("version", next); Emit();
        }
    }
}
