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
        await InodeFaultIdentity();
        await RelaxedClock();
        await PageService();
        for (ulong seed = 1; seed <= 12; seed++) await Recovery(seed);
        Console.WriteLine("PASS simulation host: write-order independence, delayed unissued writes, directory durability and real-engine crash/recovery across content-keyed torn images");
    }

    private static async Task PageService()
    {
        using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(20));
        var fs = new SimulationFileSystem(46, 512);
        var network = new SimulationNetwork(); var roles = new SimulationRoleRegistry(network);
        DatabaseFileOptions Options(int process) => new()
        {
            FileHost = fs, ScratchDevices = fs, RoleHost = roles.Host(process), Locking = FileLocking.Auto,
            EnableIdleFlush = false, IdleTransactionMilliseconds = 0, ShutdownGraceMilliseconds = 1,
            Entropy = count => new byte[count]
        };
        var owner = await Database.CreateFileAsync("service.jed", Options(0), 512, timeout.Token);
        await using (var local = owner.CreateSession()) await Execute(local, "CREATE TABLE t(id i64 PRIMARY KEY)");
        var client = await Database.OpenFileAsync("service.jed", Options(1), timeout.Token);
        await using (var remote = client.CreateSession()) await Execute(remote, "INSERT INTO t VALUES (4)");
        await network.SettleAsync(timeout.Token);
        Corpus.Equal(owner.Version, client.Version, "simulated client commits through the live owner");
        Corpus.Equal("client", client.FileRole, "simulated role uses real client driver");
        var reader = await client.OpenReadSessionAsync(cancellationToken: timeout.Token);
        network.Disconnect(1, Path.GetFullPath("service.jed")); await network.SettleAsync(timeout.Token);
        try { await SimulationRows.Query(reader, "SELECT id FROM t"); throw new InvalidDataException("disconnected read retained a snapshot"); }
        catch (SqlException error) when (error.SqlState == SqlStates.ConnectionFailure) { }
        await reader.DisposeAsync();
        await using (var reconnected = client.CreateSession())
        {
            var rows = await SimulationRows.Query(reconnected, "SELECT id FROM t");
            Corpus.Equal(1, rows.Rendered.Length, "simulated reconnect restores service access");
        }
        await roles.KillAsync(0);
        await using (var promoted = client.CreateSession()) await Execute(promoted, "INSERT INTO t VALUES (5)");
        await network.SettleAsync(timeout.Token);
        Corpus.Equal("owner", client.FileRole, "simulated client promotes through ordinary recovery");
        Corpus.Equal(1, roles.OwnerOf("service.jed")!.Value, "exclusive simulated owner follows promotion");
        await client.DisposeAsync(); await network.SettleAsync(timeout.Token);
        Corpus.Equal<int?>(null, roles.OwnerOf("service.jed"), "clean close releases injected ownership");
    }

    private static async Task InodeFaultIdentity()
    {
        static async Task<byte[]> Crash(bool rename)
        {
            var fs = new SimulationFileSystem(39, 256);
            await using var store = await fs.OpenAsync("sim.jed.jedtmp", FileMode.CreateNew);
            await store.SetLengthAsync(4096);
            if (rename) await fs.MoveAsync("sim.jed.jedtmp", "sim.jed", false);
            await fs.SyncDirectoryAsync("sim.jed");
            fs.BeginStep(32, new("process_crash", Probability: 665));
            for (var offset = 0; offset < 4096; offset += 256)
                await store.WriteAsync(offset, Enumerable.Repeat((byte)7, 256).ToArray());
            try { await store.SyncAsync(); throw new InvalidDataException("process fault did not fire"); }
            catch (SimulationFileSystem.CrashException) { }
            fs.Restart(); return fs.Image(rename ? "sim.jed" : "sim.jed.jedtmp");
        }
        var original = await Crash(false); var renamed = await Crash(true);
        Corpus.Equal(true, original.AsSpan().SequenceEqual(renamed), "fault decisions retain inode creation identity through rename");

        var later = new SimulationFileSystem(1, 256);
        await using var file = await later.OpenAsync("sim.jed", FileMode.CreateNew);
        await file.SetLengthAsync(256); await later.SyncDirectoryAsync("sim.jed");
        later.BeginStep(1, new("process_crash", Probability: 0, Nth: 2));
        await file.WriteAsync(0, new byte[] { 9 });
        later.BeginStep(2); later.Crash(power: false); later.Restart();
        Corpus.Equal((byte)9, later.Image("sim.jed")[0], "unissued-write decisions expire at the step boundary");
    }

    private static async Task RelaxedClock()
    {
        var fs = new SimulationFileSystem(147, 512);
        long now = 999; var reads = 0;
        var options = new DatabaseFileOptions { FileHost = fs, ScratchDevices = fs, Locking = FileLocking.None,
            Synchronous = Synchronous.Off, FlushIntervalMilliseconds = 1, EnableIdleFlush = false,
            Clock = () => { reads++; return now; }, Entropy = count => new byte[count] };
        var database = await Database.CreateFileAsync("clock.jed", options, 512);
        await using (var session = database.CreateSession())
        {
            await Execute(session, "CREATE TABLE t(id i32 PRIMARY KEY)");
            Corpus.Equal(1, reads, "relaxed commit reads host clock once");
            Corpus.Equal<long?>(1999, database.CommittedStorageForTesting().Batch?.DeadlineMicroseconds, "deadline preserves fractional milliseconds");
            now = 1000;
            await Execute(session, "INSERT INTO t VALUES (1)");
            Corpus.Equal(2, reads, "next relaxed commit reads clock once");
            Corpus.Equal<long?>(1999, database.CommittedStorageForTesting().Batch?.DeadlineMicroseconds, "submillisecond boundary does not flush early");
            var previous = database.Version; now = 1999;
            await Execute(session, "INSERT INTO t VALUES (2)");
            Corpus.Equal(3, reads, "deadline rollover uses one reading");
            Corpus.Equal<ulong?>(previous, database.CommittedStorageForTesting().Batch?.FloorTransactionId, "deadline flush precedes the next commit");
            Corpus.Equal<long?>(2999, database.CommittedStorageForTesting().Batch?.DeadlineMicroseconds, "rollover dates the new batch with the same reading");
        }
        var version = database.Version;
        await database.DisposeAsync();
        fs.BeginStep(1); fs.Crash(power: true); fs.Restart();
        await using var reopened = await Database.OpenFileAsync("clock.jed", options);
        Corpus.Equal(version, reopened.Version, "clean close flushes the relaxed batch");
        Corpus.Equal(3, reads, "open and close do not read the durability clock");
        Console.WriteLine("PASS relaxed host clock: one reading per commit, fractional deadline, flush-before-plan and durable close");
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
