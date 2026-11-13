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
        await CommitReadback();
        await TemporaryReplacement();
        await RelaxedImageMaintenance();
        await ScratchFaults();
        await InodeFaultIdentity();
        await RelaxedClock();
        await PageService();
        await FailedPromotionReleasesOwnership();
        await CloseOwnerWithRemoteWriter();
        await RetainedWriterSnapshots();
        await FailedWriterProtection();
        for (ulong seed = 1; seed <= 12; seed++) await Recovery(seed);
        Console.WriteLine("PASS simulation host: write-order independence, delayed unissued writes, directory durability and real-engine crash/recovery across content-keyed torn images");
    }

    private static async Task CommitReadback()
    {
        var fs = new SimulationFileSystem(19, 256);
        var calls = 0;
        Task<string> DisconnectOnce() => ++calls == 1
            ? Task.FromException<string>(new SqlException(SqlStates.ConnectionFailure, "read-back disconnected"))
            : Task.FromResult("committed");
        Corpus.Equal("committed", await SimulationRunner.ReadCommitDigest(fs, false, true, "pending", DisconnectOnce), "read-back retries a live service disconnect");
        Corpus.Equal(2, calls, "read-back retries once");
        foreach (var state in new[] { SqlStates.ConnectionFailure, SqlStates.IoError, SqlStates.DataCorrupted })
        {
            calls = 0;
            var failure = new SqlException(state, "read-back failure");
            Task<string> Fail() { calls++; return Task.FromException<string>(failure); }
            try
            {
                await SimulationRunner.ReadCommitDigest(fs, false, true, "pending", Fail);
                throw new InvalidDataException("unexpected read-back failure was hidden");
            }
            catch (SqlException error) when (ReferenceEquals(error, failure)) { }
            Corpus.Equal(state == SqlStates.ConnectionFailure ? 2 : 1, calls, "only live connection failures retry");
        }
        fs.Crash(power: false);
        Corpus.Equal(true, fs.IsDead, "read-back host records owner death");
        foreach (var state in new[] { SqlStates.IoError, SqlStates.ConnectionFailure })
            Corpus.Equal("pending", await SimulationRunner.ReadCommitDigest(fs, false, true, "pending",
                () => Task.FromException<string>(new SqlException(state, "owner died after ACK"))), "dead service defers read-back to recovery");
        foreach (var state in new[] { SqlStates.DataCorrupted, SqlStates.DivisionByZero })
        {
            var failure = new SqlException(state, "not an IO fault");
            try
            {
                await SimulationRunner.ReadCommitDigest(fs, true, true, "pending", () => Task.FromException<string>(failure));
                throw new InvalidDataException("dead-owner fallback hid an unexpected error");
            }
            catch (SqlException error) when (ReferenceEquals(error, failure)) { }
        }
        fs.Restart();
        await using var store = await fs.OpenAsync("sim.jed", FileMode.CreateNew);
        fs.BeginStep(1, new("io_error", Operation: "sync"));
        await store.WriteAsync(0, new byte[256]);
        try { await store.SyncAsync(); throw new InvalidDataException("cap-flush fault did not fire"); }
        catch (IOException) { }
        Corpus.Equal("sync", fs.Fired, "relaxed read-back fallback requires a fired fault");
        Corpus.Equal("pending", await SimulationRunner.ReadCommitDigest(fs, true, false, "pending",
            () => Task.FromException<string>(new SqlException(SqlStates.IoError, "cap flush failed"))), "relaxed cap flush defers read-back to recovery");
    }

    private static async Task TemporaryReplacement()
    {
        var fs = new SimulationFileSystem(2, 256);
        foreach (var (path, value) in new[] { ("sim.jed", (byte)1), ("sim.jed.jedtmp", (byte)2) })
        {
            await using var store = await fs.OpenAsync(path, FileMode.CreateNew);
            await store.WriteAsync(0, new[] { value }); await store.SyncAsync();
        }
        await fs.SyncDirectoryAsync("sim.jed");
        fs.BeginStep(194);
        await using (var replacement = await fs.CreateTemporaryAsync("sim.jed.jedtmp"))
        {
            await replacement.WriteAsync(0, new byte[] { 3 }); await replacement.SyncAsync();
        }
        await fs.MoveAsync("sim.jed.jedtmp", "sim.jed", overwrite: true);
        fs.Crash(power: false); fs.Restart();
        fs.BeginStep(200); fs.Crash(power: true, survive: 1000); fs.Restart();
        // Lane 10 retains both canonical operations (replace, rename). An
        // extra unlink would change the prefix selection and lose the rename.
        Corpus.Equal((byte)3, fs.Image("sim.jed")[0], "stale staging replacement preserves the canonical directory prefix");
        Corpus.Equal(false, await fs.ExistsAsync("sim.jed.jedtmp"), "surviving rename consumes staging name");
        var rejected = false;
        try { await using var unexpected = await fs.OpenAsync("sim.jed", FileMode.CreateNew); }
        catch (IOException) { rejected = true; }
        Corpus.Equal(true, rejected, "exclusive creation refuses an existing file");
    }

    private static async Task RelaxedImageMaintenance()
    {
        foreach (var fail in new[] { false, true })
        {
            var fs = new SimulationFileSystem(89, 512);
            var options = new DatabaseFileOptions { FileHost = fs, Locking = FileLocking.None,
                Synchronous = Synchronous.Off, EnableIdleFlush = false, Clock = () => 0 };
            var database = await Database.CreateFileAsync("maintenance.jed", options, 512);
            await using (var session = database.CreateSession()) await Execute(session, "CREATE TABLE t(id i32 PRIMARY KEY)");
            var batch = database.CommittedStorageForTesting().Batch ?? throw new InvalidDataException("relaxed mutation did not start a batch");
            var target = database.Version + 1;
            fs.Operations.Clear(); fs.BeginStep(1, fail ? new("power_loss", Probability: 0) : null);
            try { await database.CompactAsync(); }
            catch (SqlException error) when (fail && error.SqlState == SqlStates.IoError && fs.Fired == "barrier") { }
            Corpus.Equal(false, fs.Operations.Contains("sync maintenance.jed"), "image maintenance does not preflush the old relaxed file");
            Corpus.Equal(true, fs.Operations.Contains("sync maintenance.jed.jedtmp"), "replacement image receives the first data barrier");
            if (fail)
            {
                Corpus.Equal("barrier", fs.Fired, "replacement failure reached its barrier"); fs.Restart();
            }
            else
            {
                Corpus.Equal<StorageBatch?>(null, database.CommittedStorageForTesting().Batch, "successful replacement ends the relaxed batch");
                await database.DisposeAsync(); fs.Crash(power: true); fs.Restart();
            }
            await using var reopened = await Database.OpenFileAsync("maintenance.jed", options);
            Corpus.Equal(fail ? batch.FloorTransactionId : target, reopened.Version, "replacement failure preserves old floor while success publishes new durable image");
        }
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
        var abandoned = await client.OpenWriteSessionAsync(cancellationToken: timeout.Token);
        await Execute(abandoned, "INSERT INTO t VALUES (6)");
        network.Disconnect(1, Path.GetFullPath("service.jed")); await network.SettleAsync(timeout.Token);
        for (var attempt = 0; attempt < 2; attempt++)
        {
            try { await SimulationRows.Query(abandoned, "SELECT id FROM t"); throw new InvalidDataException("lost writer role admitted a query"); }
            catch (SqlException error) when (error.SqlState == SqlStates.ConnectionFailure) { }
        }
        try { await abandoned.CommitAsync(timeout.Token); throw new InvalidDataException("lost writer role acknowledged a commit"); }
        catch (SqlException error) when (error.SqlState == SqlStates.ConnectionFailure) { }
        await roles.KillAsync(0);
        await using (var promoted = client.CreateSession()) await Execute(promoted, "INSERT INTO t VALUES (5)");
        await network.SettleAsync(timeout.Token);
        Corpus.Equal("owner", client.FileRole, "simulated client promotes through ordinary recovery");
        Corpus.Equal(1, roles.OwnerOf("service.jed")!.Value, "exclusive simulated owner follows promotion");
        await client.DisposeAsync(); await network.SettleAsync(timeout.Token);
        Corpus.Equal<int?>(null, roles.OwnerOf("service.jed"), "clean close releases injected ownership");
    }

    private static async Task RetainedWriterSnapshots()
    {
        using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(20));
        var fs = new SimulationFileSystem(49, 512);
        var network = new SimulationNetwork(); var roles = new SimulationRoleRegistry(network);
        DatabaseFileOptions Options(int process) => new()
        {
            FileHost = fs, ScratchDevices = fs, RoleHost = roles.Host(process), CacheBytes = 1,
            EnableIdleFlush = false, IdleTransactionMilliseconds = 0, ShutdownGraceMilliseconds = 1,
            Entropy = count => new byte[count]
        };
        await using var owner = await Database.CreateFileAsync("retained.jed", Options(0), 512, timeout.Token);
        await using var local = owner.CreateSession();
        await Execute(local, "CREATE TABLE t(id i64 PRIMARY KEY)");
        await using var client = await Database.OpenFileAsync("retained.jed", Options(1), timeout.Token);
        foreach (var commit in new[] { false, true })
        {
            await Execute(local, "INSERT INTO t SELECT g FROM generate_series(1,100) g");
            var lease = await DomainLease.Acquire(client, null, true, false, timeout.Token);
            var retained = lease.Retain();
            try
            {
                if (commit)
                {
                    var domain = lease.Scope.Get("main"); var pending = domain.Pending!;
                    var table = pending.Catalog.Tables.Single();
                    pending.Trees[table.Name] = await client.Tree(table, pending, pending.Before)
                        .UpsertAsync(table.EncodeRow([SqlValue.Integer(101)]));
                    pending.Catalog = pending.Catalog with { Tables = [table with { RowCount = 101 }] };
                    pending.Changed = true;
                    await lease.PrepareWriterPinsForCommitAsync();
                    await Database.CommitDomainsAsync(lease.Scope, timeout.Token);
                }
                await lease.ReleaseWriters(); await lease.DisposeAsync();
                await Execute(local, "DELETE FROM t");
                retained.CheckRoles();
                var snapshot = retained.Scope.Get("main").Snapshot;
                var tree = client.Tree(snapshot.Catalog.Tables.Single(), null, snapshot);
                var count = 0;
                await foreach (var record in tree.ScanAsync(cancellationToken: timeout.Token)) count++;
                Corpus.Equal(100, count, "retained remote writer snapshot survives " + (commit ? "commit" : "rollback") + " and eviction");
                if (commit)
                {
                    await using var cursorSession = client.CreateSession();
                    await using var cursor = new Rows(cursorSession, [new Column("n", SqlType.I64)],
                        QueryExecution.Materialized([new Row([SqlValue.Integer(7)])]), new Meter(cursorSession, 0, default), retained.Retain());
                    network.Disconnect(1, Path.GetFullPath("retained.jed")); await network.SettleAsync(timeout.Token);
                    await HostTests.Error(SqlStates.ConnectionFailure, async () => { _ = await cursor.ReadAsync(timeout.Token); });
                }
            }
            finally { await lease.DisposeAsync(); await retained.DisposeAsync(); }
        }
    }

    private static async Task FailedWriterProtection()
    {
        foreach (var disposeSession in new[] { false, true })
        {
            using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(20));
            var fs = new SimulationFileSystem(50, 512);
            var network = new SimulationNetwork(); var roles = new SimulationRoleRegistry(network);
            DatabaseFileOptions Options(int process) => new()
            {
                FileHost = fs, ScratchDevices = fs, RoleHost = roles.Host(process), CacheBytes = 1,
                EnableIdleFlush = false, IdleTransactionMilliseconds = 0, ShutdownGraceMilliseconds = 1,
                Entropy = count => new byte[count]
            };
            // Main sorts before the remote attachment: its reader pin succeeds
            // before the disconnected attachment's protection fails.
            var main = await Database.CreateFileAsync("a-protection.jed", Options(0), 512, timeout.Token);
            await using var owner = await Database.CreateFileAsync("z-protection.jed", Options(1), 512, timeout.Token);
            await main.AttachFileAsync("a1", "z-protection.jed", Options(0), timeout.Token);
            var session = main.CreateSession();
            await Execute(session, "BEGIN");
            var cursor = await session.QueryAsync("SELECT 7", cancellationToken: timeout.Token);
            network.Disconnect(0, Path.GetFullPath("z-protection.jed")); await network.SettleAsync(timeout.Token);
            await HostTests.Error(SqlStates.ConnectionFailure, async () =>
            {
                if (disposeSession) await session.DisposeAsync();
                else await Execute(session, "ROLLBACK");
            });
            await HostTests.Error(SqlStates.ConnectionFailure, async () => { _ = await cursor.ReadAsync(timeout.Token); });
            await cursor.DisposeAsync();
            await session.DisposeAsync();
            // Both the successful partial pin and the root registry reference
            // must be gone, otherwise close reports an active local operation.
            await main.DisposeAsync();
        }
    }

    private static async Task CloseOwnerWithRemoteWriter()
    {
        using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(20));
        var fs = new SimulationFileSystem(48, 512);
        var network = new SimulationNetwork(); var roles = new SimulationRoleRegistry(network);
        DatabaseFileOptions Options(int process) => new()
        {
            FileHost = fs, ScratchDevices = fs, RoleHost = roles.Host(process),
            EnableIdleFlush = false, IdleTransactionMilliseconds = 0, ShutdownGraceMilliseconds = 1,
            Entropy = count => new byte[count]
        };
        var owner = await Database.CreateFileAsync("closing.jed", Options(0), 512, timeout.Token);
        await owner.AttachInMemoryAsync("a1", cancellationToken: timeout.Token);
        await using (var local = await owner.OpenWriteSessionAsync(cancellationToken: timeout.Token))
        {
            await HostTests.Error(SqlStates.ObjectInUse, async () => await owner.DisposeAsync());
            await local.RollbackAsync(timeout.Token);
        }
        var client = await Database.OpenFileAsync("closing.jed", Options(1), timeout.Token);
        await using var remote = await client.OpenWriteSessionAsync(cancellationToken: timeout.Token);
        await owner.DisposeAsync();
        await network.SettleAsync(timeout.Token);
        Corpus.Equal<int?>(null, roles.OwnerOf("closing.jed"), "owner shutdown drains a remote writer before closing attachments");
        await HostTests.Error(SqlStates.ConnectionFailure, async () => await remote.CommitAsync(timeout.Token));
        await remote.DisposeAsync();
        await client.DisposeAsync();
    }

    private static async Task FailedPromotionReleasesOwnership()
    {
        using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(20));
        var fs = new SimulationFileSystem(47, 1024);
        var network = new SimulationNetwork(); var roles = new SimulationRoleRegistry(network);
        using var firstKey = DatabaseCredential.FromKey(new byte[32]);
        using var nextKey = DatabaseCredential.FromKey(Enumerable.Repeat((byte)1, 32).ToArray());
        DatabaseFileOptions Options(int process, DatabaseCredential credential, byte entropy) => new()
        {
            FileHost = fs, ScratchDevices = fs, RoleHost = roles.Host(process), Credential = credential,
            EnableIdleFlush = false, IdleTransactionMilliseconds = 0, ShutdownGraceMilliseconds = 1,
            Entropy = count => new byte[count], KeyEntropy = count => Enumerable.Repeat(entropy, count).ToArray()
        };
        _ = await Database.CreateFileAsync("promotion.jed", Options(0, firstKey, 3), 1024, timeout.Token);
        var client = await Database.OpenFileAsync("promotion.jed", Options(1, firstKey, 3), timeout.Token);
        await roles.KillAsync(0);
        // Model replacement after the old owner died: the surviving client still
        // holds its original data key, while the file requires a different key.
        await using (var replacement = await Database.CreateFileAsync("replacement.jed",
            Options(2, nextKey, 7) with { RoleHost = null, Locking = FileLocking.None }, 1024, timeout.Token)) { }
        await fs.MoveAsync("replacement.jed", "promotion.jed", true, timeout.Token);
        await fs.SyncDirectoryAsync("promotion.jed", timeout.Token);
        for (var attempt = 0; attempt < 2; attempt++)
        {
            try
            {
                await using var pin = await client.PinAsync(timeout.Token);
                throw new InvalidDataException("stale held key promoted into a replacement file");
            }
            catch (SqlException error) when (error.SqlState == SqlStates.StaleKey) { }
            Corpus.Equal<int?>(null, roles.OwnerOf("promotion.jed"), "failed promotion releases exclusive ownership");
        }
        await using (var correct = await Database.OpenFileAsync("promotion.jed", Options(2, nextKey, 7), timeout.Token))
            Corpus.Equal("owner", correct.FileRole, "fresh credential can acquire ownership after failed promotion");
        await client.DisposeAsync();
        await network.SettleAsync(timeout.Token);
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
        catch (SqlException error) when (error.SqlState == SqlStates.IoError && fs.Fired == "barrier") { }
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
