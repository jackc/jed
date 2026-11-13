using Jed;

internal static class SimulationHostTests
{
    internal static async Task Run()
    {
        var forward = await OrderedWrites(false); var reverse = await OrderedWrites(true);
        Corpus.Equal(true, forward.AsSpan().SequenceEqual(reverse), "content-keyed power loss is independent of distinct-offset write order");
        var fs = new SimulationFileSystem(97, 256);
        await using (var store = await fs.OpenAsync("sim.jed", FileMode.CreateNew))
        {
            await store.WriteAsync(0, new byte[256]); await store.SyncAsync(); await fs.SyncDirectoryAsync("sim.jed");
            fs.BeginStep(1, new("process_crash", Probability: 0));
            await store.WriteAsync(0, Enumerable.Repeat((byte)7, 256).ToArray());
            var bytes = new byte[256]; await store.ReadAsync(0, bytes);
            Corpus.Equal((byte)7, bytes[0], "unissued writes remain visible until process death");
            try { await store.SyncAsync(); throw new InvalidDataException("process fault did not fire"); }
            catch (SimulationFileSystem.CrashException) { }
        }
        fs.Restart();
        Corpus.Equal((byte)0, fs.Image("sim.jed")[0], "process death removes unissued writes");
        await using (var abandoned = await fs.OpenAsync("uncommitted", FileMode.CreateNew)) await abandoned.WriteAsync(0, new byte[] { 3 });
        fs.BeginStep(2); fs.Crash(power: true); fs.Restart();
        // A pending directory update may survive as an ordered prefix; whichever
        // decision occurs, it becomes the durable directory for the next crash.
        var survived = await fs.ExistsAsync("uncommitted");
        fs.BeginStep(3); fs.Crash(power: true); fs.Restart();
        Corpus.Equal(survived, await fs.ExistsAsync("uncommitted"), "recovered directory prefix stays durable");
        await ScratchFaults();
        for (ulong seed = 1; seed <= 12; seed++) await Recovery(seed);
        Console.WriteLine("PASS simulation host: write-order independence, delayed unissued writes, directory durability and real-engine crash/recovery across content-keyed torn images");
    }

    private static async Task ScratchFaults()
    {
        var fs = new SimulationFileSystem(431, 256);
        var random = new SeededRandomSource(431);
        await using var host = new ScratchHost(true, random.Fill, devices: fs);
        await using var store = await host.CreateAsync();
        await store.WriteAsync(0, new byte[] { 4, 5, 6 });
        var bytes = new byte[3]; await store.ReadAsync(0, bytes);
        Corpus.Equal("040506", Convert.ToHexString(bytes), "simulated encrypted scratch follows real codec");
        fs.BeginStep(1, new("spill_error", Probability: 1000));
        await HostTests.Error(SqlStates.IoError, () => store.ReadAsync(0, bytes).AsTask());
        fs.BeginStep(2, new("spill_error", Probability: 1000));
        await HostTests.Error(SqlStates.IoError, async () => { await using var unused = await host.CreateAsync(); });
        fs.EndFaultedStep(); fs.Restart();
        Corpus.Equal(false, await fs.ExistsAsync("sim.jed"), "scratch does not create durable database files");
    }

    private static async Task<byte[]> OrderedWrites(bool reverse)
    {
        var fs = new SimulationFileSystem(713, 256);
        await using var store = await fs.OpenAsync("sim.jed", FileMode.CreateNew);
        await store.SetLengthAsync(1024); await fs.SyncDirectoryAsync("sim.jed");
        fs.BeginStep(8, new("power_loss", Probability: 740, Tear: 700));
        foreach (var page in reverse ? new[] { 3, 2, 1, 0 } : new[] { 0, 1, 2, 3 })
            await store.WriteAsync(page * 256L, Enumerable.Repeat((byte)(page + 1), 256).ToArray());
        try { await store.SyncAsync(); throw new InvalidDataException("power fault did not fire"); }
        catch (SimulationFileSystem.CrashException) { }
        fs.Restart(); return fs.Image("sim.jed");
    }

    private static async Task Recovery(ulong seed)
    {
        var fs = new SimulationFileSystem(seed, 256);
        var options = new DatabaseFileOptions { FileHost = fs, ScratchDevices = fs, Locking = FileLocking.None, CacheBytes = 0, Entropy = count => new byte[count] };
        var database = await Database.CreateFileAsync("sim.jed", options, pageSize: 256);
        var writer = database.CreateSession();
        await Execute(writer, "CREATE TABLE t (id i32 PRIMARY KEY,v text)");
        await Execute(writer, "INSERT INTO t VALUES (1,'old')");
        await Execute(writer, "BEGIN"); await Execute(writer, "UPDATE t SET v='new' WHERE id=1");
        fs.BeginStep(5, new("power_loss", Probability: (int)(seed % 3) * 500, Tear: 450));
        try { await Execute(writer, "COMMIT"); throw new InvalidDataException("commit reached no barrier"); }
        catch (SimulationFileSystem.CrashException) { }
        // Deliberately abandon the killed engine. Calling its clean Dispose
        // would test a graceful shutdown, not a process crash.
        fs.Restart();
        await using var reopened = await Database.OpenFileAsync("sim.jed", options);
        await using var reader = reopened.CreateSession();
        await using var cursor = await reader.QueryAsync("SELECT v FROM t WHERE id=1");
        var row = await cursor.ReadAsync(); var value = row?[0].ToString();
        Corpus.Equal(true, value is "old" or "new", "validated COW recovers one complete generation after torn writes");
        await cursor.DisposeAsync(); await reader.DisposeAsync();
        var image = await reopened.SerializeAsync();
        await using var imported = await Database.DeserializeAsync(image);
        await using var check = imported.CreateSession();
        await using var result = await check.QueryAsync("SELECT v FROM t WHERE id=1");
        Corpus.Equal(value, (await result.ReadAsync())?[0].ToString(), "post-crash snapshot exports as a coherent image");
    }

    private static async Task Execute(Session session, string sql)
    { await using var cursor = await session.QueryAsync(sql); await foreach (var row in cursor) { } }
}
