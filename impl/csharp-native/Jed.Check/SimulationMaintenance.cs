using System.Globalization;
using Jed;

internal sealed partial class SimulationRunner
{
    private async Task AttachmentMaintenance(SimulationStep step, string name)
    {
        if (!attached.TryGetValue(name, out var state) || !database.AttachedDatabases.Contains(name) ||
            step.Operation is "change_key" or "rekey" && !trace.Encrypted)
        { json.Text("db", name).Flag("skip", true); return; }
        json.Text("db", name);
        var oldVersion = state.Version;
        if (step.Operation == "change_key")
        {
            var next = int.Parse(step.Arguments[0], CultureInfo.InvariantCulture);
            if (next is < 1 or > 3) throw new InvalidDataException("invalid credential target");
            state.KeyAttempt = new(state.Key, next, !fs.EntryDurable(state.File));
            await database.ChangeCredentialAsync(keys[next], name);
            state.KeyAttempt = state.KeyAttempt with { Acknowledged = true }; state.Key = next;
            SimulationStorageChecks.Require(state.Database.Version == oldVersion, "credential change advanced attachment version");
            json.Number("key", next); return;
        }
        var digest = await AttachmentDigest(state);
        state.Pending = digest; state.Remember(oldVersion + 1, digest);
        if (step.Operation == "rekey")
        {
            var next = step.Arguments[0] == "plain" ? -1 : int.Parse(step.Arguments[0], CultureInfo.InvariantCulture);
            if (next is < -1 or > 3 or 0) throw new InvalidDataException("invalid rekey target");
            state.KeyAttempt = new(state.Key, next, !fs.EntryDurable(state.File));
            await database.RekeyAsync(next < 0 ? null : keys[next], name);
            state.KeyAttempt = state.KeyAttempt with { Acknowledged = true }; state.Key = next;
        }
        else await database.CompactAsync(name);
        state.Version = state.Database.Version;
        SimulationStorageChecks.Require(state.Version == oldVersion + 1 && digest == await AttachmentDigest(state), "attachment maintenance changed contents or generation incorrectly");
        state.Floor = state.Version;
        var storage = state.Database.CommittedStorageForTesting();
        state.Growth = storage.Meta.PageCount - storage.FreePages.Count; state.Reclaimed = false;
        json.Number("version", state.Version);
    }
}
