using Jed;

// The registry models only exclusive OS ownership and stream endpoints. Every
// snapshot, page request and commit still uses the production protocol handlers.
internal sealed class SimulationRoleRegistry(SimulationNetwork network, Func<long>? now = null, Action<long>? advance = null)
{
    private readonly SimulationNetwork network = network;
    private long localClock;
    private sealed record Owner(int Process, Database? Database = null, PageOwnerServer? Server = null);
    private readonly Dictionary<string, Owner> owners = new(StringComparer.Ordinal);
    internal IFileRoleHost Host(int process) => new HostAdapter(this, process);
    internal int? OwnerOf(string path) => owners.GetValueOrDefault(Path.GetFullPath(path))?.Process;
    internal Database? DatabaseOf(string path) => owners.GetValueOrDefault(Path.GetFullPath(path))?.Database;
    internal async Task KillAsync(int process)
    {
        network.Kill(process);
        foreach (var (path, owner) in owners.Where(pair => pair.Value.Process == process).ToArray())
        {
            owners.Remove(path);
            if (owner.Database is not null) await owner.Database.AbortInjectedHostAsync();
        }
        await network.SettleAsync();
    }
    private sealed class HostAdapter(SimulationRoleRegistry registry, int process) : IFileRoleHost
    {
        public long NowMicroseconds => registry.Now;
        public async ValueTask PollAsync(CancellationToken token)
        {
            token.ThrowIfCancellationRequested(); registry.Advance();
            await Task.Yield();
        }
        public async ValueTask<PageClientConnection?> ElectAsync(string path, bool readOnly, CancellationToken token)
        {
            if (!registry.owners.TryGetValue(path, out var owner))
            { registry.owners.Add(path, new(process)); return null; }
            if (owner.Process == process) throw new InvalidOperationException("process opened the same owned file twice");
            if (owner.Server is null) throw new InvalidOperationException("owner has not published its page service");
            var (client, server) = registry.network.Connect(process, owner.Process, path);
            registry.network.Serve(server, owner.Server.ServeStreamAsync(server));
            return await PageClientConnection.ConnectStreamAsync(client, readOnly, new(0, []), token);
        }
        public void Publish(string path, Database database, PageOwnerServer server)
        {
            if (!registry.owners.TryGetValue(path, out var owner) || owner.Process != process)
                throw new InvalidOperationException("page service published without exclusive ownership");
            registry.owners[path] = new(process, database, server);
        }
        public void Release(string path)
        { if (registry.owners.TryGetValue(path, out var owner) && owner.Process == process) registry.owners.Remove(path); }
    }
    private long Now => now?.Invoke() ?? localClock;
    private void Advance()
    {
        if (advance is not null) advance(2000);
        else localClock = checked(localClock + 2000);
    }
}
