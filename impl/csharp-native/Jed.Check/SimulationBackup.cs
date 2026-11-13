using Jed;

internal sealed partial class SimulationRunner
{
    private sealed record BackupState(int Number, string File, ulong Version, string Digest, int Key);
    private readonly List<BackupState> backups = [];
    private int backupOrdinal;
    private async Task Backup(string form)
    {
        if (form is not ("page_identical" or "compacted")) throw new InvalidDataException("invalid backup form");
        var destination = "sim.jed.backup" + ++backupOrdinal;
        json.Text("form", form);
        try { await database.BackupForTestingAsync(destination, form == "compacted"); }
        catch
        {
            if (await fs.ExistsAsync(destination)) throw new InvalidDataException("failed backup left a destination");
            throw;
        }
        var bytes = fs.Image(destination);
        await using var copy = key < 0 ? await Database.OpenStoreAsync(new MemoryBlockStore(bytes)) :
            await Database.OpenEncryptedStoreAsync(new MemoryBlockStore(bytes), keys[key].Value, count => new byte[count]);
        await using var session = copy.CreateSession(SessionOptions(fs.Step) with { MaxQueryMemoryBytes = 0 });
        var digest = await SimulationRows.Digest(session, fs);
        var snapshot = form == "compacted" ? copy.Version - 1 : copy.Version;
        SimulationStorageChecks.Require(snapshot >= floor && (snapshot >= version || ownerLost) && snapshot <= attempted && history.TryGetValue(snapshot, out var values) && values.Contains(digest),
            "backup contains an unrecorded snapshot or violates the durability floor");
        if (ownerLost) { version = snapshot; ownerLost = false; }
        var result = await SimulationStorageChecks.Image(bytes, copy.Version, key < 0 ? null : keys[key].Value);
        backups.Add(new(backupOrdinal, destination, copy.Version, digest, key));
        json.Number("version", copy.Version).Text("digest", digest).Number("live", result.Live).Number("pages", result.Pages)
            .Number("free", result.Free).Text("image", result.Image);
        if (System.Environment.GetEnvironmentVariable("JED_SIM_OUT") is { Length: > 0 } output)
            await File.WriteAllBytesAsync(Path.Combine(output, $"{Path.GetFileNameWithoutExtension(trace.Name)}.csharp-native.step{fs.Step}.backup.img"), bytes);
    }
}
