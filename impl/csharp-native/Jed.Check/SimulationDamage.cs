using System.Globalization;
using Jed;

internal sealed partial class SimulationRunner
{
    private async Task StopForImageProbe()
    {
        await CloseClean();
        fs.Crash(power: false); fs.Restart();
    }
    private async Task OpenForImageProbe(bool attachments = true)
    {
        selectedProcess = 0;
        database = await Database.OpenFileAsync("sim.jed", processOptions = Options());
        if (trace.Service) processes.Add(0, new(database, processOptions));
        Configure();
        if (attachments) await ReattachDomains();
    }
    private async Task DiscardImageProbe(bool opened)
    {
        if (trace.Service) await CrashService();
        fs.Crash(power: false); fs.Restart();
        if (opened && !trace.Service)
        {
            // No writes are allowed from a process that observed damaged bytes.
            try { await database.DisposeAsync(); }
            catch (Exception error) when (error is SqlException or SimulationFileSystem.CrashException or IOException) { }
        }
    }
    private async Task VerifyRepairedImage(string digest)
    {
        await OpenForImageProbe();
        SimulationStorageChecks.Require(database.Version == version && await Digest() == digest, "repaired image differs from its original generation");
    }
    private async Task CorruptImage(SimulationStep step)
    {
        var digest = await Digest(); var count = database.CommittedStorageForTesting().Meta.PageCount;
        await StopForImageProbe();
        var reseal = Parameter(step.Arguments, "reseal", 0) == 1;
        var damage = fs.Corrupt("sim.jed", Parameter(step.Arguments, "flips", 0), count, reseal);
        var openResult = "ok"; var readResult = "-"; var opened = false;
        try
        {
            try { await OpenForImageProbe(attachments: false); opened = true; }
            catch (SqlException error) { openResult = Allowed(error); }
            if (opened)
            {
                readResult = "ok";
                try
                {
                    var observed = await Digest(); var observedVersion = database.Version;
                    if (!reseal) SimulationStorageChecks.Require(observedVersion <= version && version - observedVersion <= 1 &&
                        history.TryGetValue(observedVersion, out var values) && values.Contains(observed), "corruption returned unrecorded contents");
                }
                catch (SqlException error) { readResult = Allowed(error); }
            }
        }
        finally { await DiscardImageProbe(opened); fs.Repair("sim.jed", damage); }
        await VerifyRepairedImage(digest);
        json.Start(step.Index, "corrupt").Number("flipped", damage.Length).Text("open", openResult).Text("read", readResult); Emit();
        string Allowed(SqlException error)
        {
            SimulationStorageChecks.Require(reseal || error.SqlState == SqlStates.DataCorrupted, "CRC corruption returned an error other than XX001");
            return error.SqlState;
        }
    }
    private async Task RestoreBackup(SimulationStep step)
    {
        await StopForImageProbe();
        var ordinal = int.Parse(step.Arguments[0], CultureInfo.InvariantCulture);
        if (ordinal < 1) throw new InvalidDataException("restore ordinal must be positive");
        var selected = backups.Count == 0 ? null : backups[Math.Max(0, backups.Count - ordinal)];
        if (selected is null || !await fs.ExistsAsync(selected.File))
        {
            await OpenForImageProbe();
            json.Start(step.Index, "restore").Flag("skip", true); Emit(); return;
        }
        fs.Restore(selected.File, "sim.jed");
        foreach (var old in history.Keys.Where(value => value >= selected.Version).ToArray()) history.Remove(old);
        version = floor = attempted = selected.Version; key = selected.Key; keyAttempt = null; ownerLost = false;
        AddHistory(version, selected.Digest);
        await VerifyRepairedImage(selected.Digest);
        json.Start(step.Index, "restore").Number("backup", selected.Number).Number("version", version).Text("digest", selected.Digest); Emit();
    }
    private async Task ReplayPage(SimulationStep step)
    {
        var kind = step.Arguments[0]; var digest = await Digest();
        await StopForImageProbe();
        var image = fs.Image("sim.jed");
        var pointers = key < 0 ? null : await SimulationPointerKinds.Read(image, keys[key].Value);
        var candidates = pointers?.Kinds.GetValueOrDefault(kind, []).Select(page => (Page: page, Versions: fs.ReplayVersions("sim.jed", page)))
            .Where(item => item.Versions.Length != 0).ToArray() ?? [];
        if (candidates.Length == 0)
        {
            await VerifyRepairedImage(digest);
            json.Start(step.Index, "replay").Text("kind", kind).Flag("skip", true); Emit(); return;
        }
        var candidate = candidates[checked((int)(SimulationFileSystem.Decide(trace.Seed, 15, (ulong)step.Index, 0, 0) % (ulong)candidates.Length))];
        var older = candidate.Versions[checked((int)(SimulationFileSystem.Decide(trace.Seed, 16, (ulong)step.Index, 0, 0) % (ulong)candidate.Versions.Length))];
        ulong other = 0;
        for (var slot = 0; slot < 2; slot++)
        {
            var meta = StorageCryptoMeta.Parse(image.AsSpan(slot * trace.PageSize, trace.PageSize), image.Length);
            if (meta is not null && meta.Header.TransactionId < version) other = Math.Max(other, meta.Header.TransactionId);
        }
        var undo = fs.ReplacePage("sim.jed", candidate.Page, older);
        var outcome = ""; ulong observedVersion = 0; var opened = false;
        try
        {
            try
            {
                await OpenForImageProbe(attachments: false); opened = true; observedVersion = database.Version;
                var observed = await Digest();
                if (observedVersion < version && observedVersion == other && pointers!.Dependencies.Contains(candidate.Page) &&
                    history.TryGetValue(observedVersion, out var values) && values.Contains(observed))
                {
                    _ = await SimulationStorageChecks.Image(fs.Image("sim.jed"), observedVersion, keys[key].Value);
                    outcome = "fallback";
                }
                else if (observedVersion == version && observed == digest && !pointers!.Dependencies.Contains(candidate.Page))
                {
                    _ = await SimulationStorageChecks.Image(fs.Image("sim.jed"), observedVersion, keys[key].Value);
                    throw new InvalidDataException("authenticated page replay went undetected");
                }
                else throw new InvalidDataException("page replay recovered forbidden generation or contents");
            }
            catch (SqlException error) when (error.SqlState == SqlStates.DataCorrupted) { outcome = SqlStates.DataCorrupted; }
        }
        finally { await DiscardImageProbe(opened); fs.Repair("sim.jed", [undo]); }
        await VerifyRepairedImage(digest);
        json.Start(step.Index, "replay").Text("kind", kind).Number("page", candidate.Page).Text("outcome", outcome);
        if (outcome == "fallback") json.Number("version", observedVersion);
        Emit();
    }
}
