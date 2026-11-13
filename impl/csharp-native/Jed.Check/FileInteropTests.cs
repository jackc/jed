using System.Diagnostics;
using Jed;

internal static class FileInteropTests
{
    internal static async Task Run(string peerExecutable)
    {
        var directory = Path.Combine(Path.GetTempPath(), "jed-csharp-peer-" + Guid.NewGuid().ToString("N")); Directory.CreateDirectory(directory);
        try
        {
            await CSharpOwner(Path.Combine(directory, "owner.jed"), peerExecutable);
            await CSharpClient(Path.Combine(directory, "client.jed"), peerExecutable);
            await YieldOwner(Path.Combine(directory, "yield.jed"), peerExecutable);
            await YieldPeer(Path.Combine(directory, "peer-yield.jed"), peerExecutable);
            await CrashAcknowledgement(Path.Combine(directory, "crash.jed"), peerExecutable);
            await ReadOnlyOwner(Path.Combine(directory, "readonly-owner.jed"), peerExecutable);
            await ReadOnlyPeer(Path.Combine(directory, "readonly-peer.jed"), peerExecutable);
            await CSharpOwner(Path.Combine(directory, "encrypted-owner.jed"), peerExecutable, true);
            await CSharpClient(Path.Combine(directory, "encrypted-client.jed"), peerExecutable, true);
        }
        finally { Directory.Delete(directory, recursive: true); }
        Console.WriteLine("PASS cross-process file service: plaintext/encrypted C#↔Go SQL reads/writes, snapshot pins, refresh/promotion, yield/read-only handoff both directions, lost/crashed-owner acknowledgement resolution, retained DEK after credential disposal, live-client compaction refresh and both remote backup forms");
    }
    private const string TestKey = "4242424242424242424242424242424242424242424242424242424242424242";
    private static async Task CSharpOwner(string path, string executable, bool encrypted = false)
    {
        using var credential = encrypted ? DatabaseCredential.FromKey(Convert.FromHexString(TestKey)) : null;
        await using var owner = await Database.CreateFileAsync(path, new() { Credential = credential }, pageSize: encrypted ? 1024 : 256);
        credential?.Dispose();
        await using var session = owner.CreateSession();
        await DatabaseFileTests.Exec(session, "CREATE TABLE t (id i64 PRIMARY KEY, n i64)");
        await DatabaseFileTests.Exec(session, "INSERT INTO t VALUES (1,11)");
        await using var client = await Actor.Start(executable, "open", path, encrypted ? ["key=" + TestKey] : []);
        Corpus.Equal("client", await client.Command("ROLE"), "Go became page client");
        Corpus.Equal("11", await client.Sql("QUERY_I64", "SELECT n FROM t WHERE id=1"), "Go reads C# file");
        await client.Sql("EXEC", "INSERT INTO t VALUES (2,22)");
        Corpus.Equal("22", await DatabaseFileTests.Scalar(session, "SELECT n FROM t WHERE id=2"), "C# adopts Go commit");
        await client.Command("READ_OPEN");
        Corpus.Equal("11", await client.Sql("READ_QUERY_I64", "SELECT n FROM t WHERE id=1"), "Go pins C# generation");
        await DatabaseFileTests.Exec(session, "UPDATE t SET n=33 WHERE id=1");
        Corpus.Equal("11", await client.Sql("READ_QUERY_I64", "SELECT n FROM t WHERE id=1"), "remote pinned generation remains readable");
        await client.Command("READ_CLOSE");
        Corpus.Equal("33", await client.Sql("QUERY_I64", "SELECT n FROM t WHERE id=1"), "Go refreshes C# commit");
        // The pinned Go peer poisons its client writer after appending to an
        // inherited overflow root (also reproduced with a Go owner). Isolate
        // that peer writer lifetime; the retained client still verifies full
        // invalidation and subsequent writes across the replacement below.
        await using (var overflowWriter = await Actor.Start(executable, "open", path, encrypted ? ["key=" + TestKey] : []))
            await LargeValues(session, overflowWriter);
        await client.Sql("EXEC", "UPDATE t SET n=33 WHERE id=1");
        await client.Command("READ_OPEN");
        Corpus.Equal("33", await client.Sql("READ_QUERY_I64", "SELECT n FROM t WHERE id=1"), "Go pins before compaction refusal");
        await HostTests.Error(SqlStates.ObjectInUse, () => owner.CompactAsync());
        await client.Command("READ_CLOSE");
        var compactDeadline = Stopwatch.StartNew();
        while (true)
        {
            try { await owner.CompactAsync(); break; }
            catch (SqlException error) when (error.SqlState == SqlStates.ObjectInUse && compactDeadline.ElapsedMilliseconds < 5000)
            { await Task.Delay(10); }
        }
        Corpus.Equal("33", await client.Sql("QUERY_I64", "SELECT n FROM t WHERE id=1"), "Go reloads all pages after C# compaction");
        await client.Sql("EXEC", "UPDATE t SET n=34 WHERE id=1");
        Corpus.Equal("34", await DatabaseFileTests.Scalar(session, "SELECT n FROM t WHERE id=1"), "Go commits through replaced owner file");
        await client.Sql("EXEC", "UPDATE t SET n=33 WHERE id=1");
        if (encrypted)
        {
            var newKey = new string('3', 64);
            using var replacement = DatabaseCredential.FromKey(Convert.FromHexString(newKey));
            await client.Command("READ_OPEN");
            await owner.ChangeCredentialAsync(replacement);
            Corpus.Equal("33", await client.Sql("READ_QUERY_I64", "SELECT n FROM t WHERE id=1"), "Go pinned reader survives credential rewrap");
            await client.Command("READ_CLOSE");
            Corpus.Equal("33", await client.Sql("QUERY_I64", "SELECT n FROM t WHERE id=1"), "Go held data key survives owner credential change");
            await using var fresh = await Actor.Start(executable, "open", path, "key=" + newKey);
            Corpus.Equal("33", await fresh.Sql("QUERY_I64", "SELECT n FROM t WHERE id=1"), "fresh Go client unlocks new credential");
        }
    }
    private static async Task CSharpClient(string path, string executable, bool encrypted = false)
    {
        using var credential = encrypted ? DatabaseCredential.FromKey(Convert.FromHexString(TestKey)) : null;
        await using var owner = await Actor.Start(executable, "create", path, encrypted ? ["key=" + TestKey] : []);
        await owner.Sql("EXEC", "CREATE TABLE t (id i64 PRIMARY KEY, n i64)"); await owner.Sql("EXEC", "INSERT INTO t VALUES (1,11)");
        await using var database = await Database.OpenFileAsync(path, new() { Credential = credential });
        credential?.Dispose();
        await using var session = database.CreateSession();
        await HostTests.Error(SqlStates.ObjectInUse, () => database.CompactAsync());
        if (encrypted)
        {
            using var replacement = DatabaseCredential.FromKey(new byte[32]);
            await HostTests.Error(SqlStates.ObjectNotInPrerequisiteState, () => database.ChangeCredentialAsync(replacement));
        }
        Corpus.Equal("11", await DatabaseFileTests.Scalar(session, "SELECT n FROM t WHERE id=1"), "C# reads Go file");
        await DatabaseFileTests.Exec(session, "INSERT INTO t VALUES (2,22)");
        Corpus.Equal("22", await owner.Sql("QUERY_I64", "SELECT n FROM t WHERE id=2"), "Go adopts C# commit");
        await using (var frozen = await session.QueryAsync("SELECT n FROM t ORDER BY id"))
        {
            await owner.Sql("EXEC", "UPDATE t SET n=33 WHERE id=1");
            var values = new List<string>(); await foreach (var row in frozen) values.Add(row[0].ToString());
            Corpus.Equal("11,22", string.Join(',', values), "C# retains remote snapshot while owner commits");
        }
        Corpus.Equal("33", await DatabaseFileTests.Scalar(session, "SELECT n FROM t WHERE id=1"), "C# refreshes Go commit");
        await LargeValues(session, owner);
        foreach (var compacted in new[] { false, true })
        {
            var backup = path + (compacted ? ".compacted-backup" : ".page-backup");
            await database.BackupForTestingAsync(backup, compacted);
            await using var restored = await Actor.Start(executable, "open", backup, encrypted ? ["key=" + TestKey] : []);
            Corpus.Equal("33", await restored.Sql("QUERY_I64", "SELECT n FROM t WHERE id=1"), "Go opens C# page-client backup");
            Corpus.Equal("6000", await restored.Sql("QUERY_I64", "SELECT length(body) FROM large_values WHERE id=1"), "page-client backup retains overflow values");
        }
        await owner.Command("HANGUP_NEXT_ACK");
        await DatabaseFileTests.Exec(session, "INSERT INTO t VALUES (4,55)");
        Corpus.Equal("55", await owner.Sql("QUERY_I64", "SELECT n FROM t WHERE id=4"), "lost Go acknowledgement resolved without duplicate publication");
        await owner.Close();
        await DatabaseFileTests.Exec(session, "INSERT INTO t VALUES (3,44)");
        Corpus.Equal("44", await DatabaseFileTests.Scalar(session, "SELECT n FROM t WHERE id=3"), "C# promotes after owner exit");
    }
    private static async Task LargeValues(Session session, Actor peer)
    {
        const string alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        uint state = 0x4A454442; var chars = new char[6000];
        for (var i = 0; i < chars.Length; i++)
        {
            state ^= state << 13; state ^= state >> 17; state ^= state << 5; chars[i] = alphabet[(int)(state % 64)];
        }
        var text = new string(chars); var reverse = new string(chars.Reverse().ToArray());
        await DatabaseFileTests.Exec(session, "CREATE TABLE large_values (id i32 PRIMARY KEY, body text)");
        await DatabaseFileTests.Exec(session, $"INSERT INTO large_values VALUES (1,'{text}'),(2,'{new string('x', 6000)}')");
        Corpus.Equal("1", await peer.Sql("QUERY_I64", $"SELECT CASE WHEN body='{text}' THEN 1 ELSE 0 END FROM large_values WHERE id=1"), "Go reads exact C# overflow bytes");
        Corpus.Equal("6000", await peer.Sql("QUERY_I64", "SELECT length(body) FROM large_values WHERE id=2"), "Go decodes C# pinned compression");
        await peer.Sql("EXEC", $"INSERT INTO large_values VALUES (3,'{reverse}'),(4,'{new string('y', 6000)}')");
        Corpus.Equal(reverse, await DatabaseFileTests.Scalar(session, "SELECT body FROM large_values WHERE id=3"), "C# reads exact Go overflow bytes");
        Corpus.Equal(new string('y', 6000), await DatabaseFileTests.Scalar(session, "SELECT body FROM large_values WHERE id=4"), "C# decodes Go pinned compression");
    }
    private static async Task YieldOwner(string path, string executable)
    {
        await using var database = await Database.CreateFileAsync(path, new() { Ownership = FileOwnership.Yield, YieldGraceMilliseconds = 50 }, 256);
        await using var session = database.CreateSession();
        await DatabaseFileTests.Exec(session, "CREATE TABLE t (id i64 PRIMARY KEY)");
        await using var oldPin = await database.PinAsync();
        await using var retainedPin = oldPin.Retain();
        var oldWrite = await database.BeginWriteAsync(default);
        await using var peer = await Actor.Start(executable, "open", path);
        var deadline = Stopwatch.StartNew();
        while (await peer.Command("ROLE") != "owner")
        {
            if (deadline.Elapsed > TimeSpan.FromSeconds(10)) throw new TimeoutException("Go did not receive ownership");
            await peer.Sql("QUERY_I64", "SELECT 1"); await Task.Delay(5);
        }
        await peer.Sql("EXEC", "INSERT INTO t VALUES (1)");
        Corpus.Equal("owner", await peer.Command("ROLE"), "Go requested and received C# ownership");
        try { oldPin.CheckRole(); throw new InvalidDataException("old read role survived forced handoff"); }
        catch (SqlException error) when (error.SqlState == SqlStates.ConnectionFailure) { }
        try { database.CheckPendingRole(oldWrite); throw new InvalidDataException("old write role survived forced handoff"); }
        catch (SqlException error) when (error.SqlState == SqlStates.ConnectionFailure) { }
        await database.EndWriteAsync(oldWrite);
        Corpus.Equal("1", await DatabaseFileTests.Scalar(session, "SELECT id FROM t"), "yielding C# owner reconnects as client");
    }
    private static async Task YieldPeer(string path, string executable)
    {
        await using var peer = await Actor.Start(executable, "create", path, "ownership=yield", "yield_grace=50");
        await peer.Sql("EXEC", "CREATE TABLE t (id i64 PRIMARY KEY)");
        await using var database = await Database.OpenFileAsync(path);
        await using var session = database.CreateSession();
        var deadline = Stopwatch.StartNew();
        while (database.FileRole != "owner")
        {
            if (deadline.Elapsed > TimeSpan.FromSeconds(10)) throw new TimeoutException("C# did not receive ownership");
            _ = await DatabaseFileTests.Scalar(session, "SELECT 1"); await Task.Delay(5);
        }
        await DatabaseFileTests.Exec(session, "INSERT INTO t VALUES (1)");
        Corpus.Equal("1", await peer.Sql("QUERY_I64", "SELECT id FROM t"), "Go yielding owner reconnects to C#");
        Corpus.Equal("client", await peer.Command("ROLE"), "C# requested and received Go ownership");
    }
    private static async Task CrashAcknowledgement(string path, string executable)
    {
        await using var peer = await Actor.Start(executable, "create", path);
        await peer.Sql("EXEC", "CREATE TABLE t (id i64 PRIMARY KEY)");
        await using var database = await Database.OpenFileAsync(path);
        await using var session = database.CreateSession();
        await peer.Command("CRASH_NEXT_ACK");
        await DatabaseFileTests.Exec(session, "INSERT INTO t VALUES (1)");
        Corpus.Equal("owner", database.FileRole, "C# promotes to resolve crashed owner acknowledgement");
        Corpus.Equal("1", await DatabaseFileTests.Scalar(session, "SELECT id FROM t"), "crashed owner commit recovered exactly once");
    }
    private static async Task ReadOnlyOwner(string path, string executable)
    {
        await using (var initial = await Database.CreateFileAsync(path, pageSize: 256))
        { await using var session = initial.CreateSession(); await DatabaseFileTests.Exec(session, "CREATE TABLE t (id i64 PRIMARY KEY)"); }
        await using var database = await Database.OpenFileAsync(path, new() { ReadOnly = true, YieldGraceMilliseconds = 50 });
        await using var peer = await Actor.Start(executable, "open", path);
        await peer.Sql("EXEC", "INSERT INTO t VALUES (1)");
        Corpus.Equal("owner", await peer.Command("ROLE"), "Go writer replaces read-only C# owner");
        await using var read = database.CreateSession();
        Corpus.Equal("1", await DatabaseFileTests.Scalar(read, "SELECT id FROM t"), "read-only C# continues as client");
    }
    private static async Task ReadOnlyPeer(string path, string executable)
    {
        await using (var initial = await Actor.Start(executable, "create", path))
            await initial.Sql("EXEC", "CREATE TABLE t (id i64 PRIMARY KEY)");
        await using var peer = await Actor.Start(executable, "open-read-only", path, "yield_grace=50");
        await using var database = await Database.OpenFileAsync(path, new() { WriterLockTimeoutMilliseconds = 5000 });
        await using var session = database.CreateSession();
        await DatabaseFileTests.Exec(session, "INSERT INTO t VALUES (1)");
        Corpus.Equal("owner", database.FileRole, "C# writer replaces read-only Go owner");
        Corpus.Equal("1", await peer.Sql("QUERY_I64", "SELECT id FROM t"), "read-only Go continues as client");
    }
    private sealed class Actor(Process process) : IAsyncDisposable
    {
        private readonly Task<string> errors = process.StandardError.ReadToEndAsync();
        private bool closed;
        internal static async Task<Actor> Start(string executable, string action, string path, params string[] options)
        {
            var start = new ProcessStartInfo(executable) { RedirectStandardInput = true, RedirectStandardOutput = true, RedirectStandardError = true, UseShellExecute = false };
            start.ArgumentList.Add(action); start.ArgumentList.Add(path);
            foreach (var option in options) start.ArgumentList.Add(option);
            var process = Process.Start(start) ?? throw new IOException("could not start peer actor"); var actor = new Actor(process);
            var line = await actor.Line(); if (line != "READY") { await actor.DisposeAsync(); throw new InvalidDataException("peer start: " + line); }
            return actor;
        }
        private async Task<string> Line()
        {
            var line = await process.StandardOutput.ReadLineAsync().WaitAsync(TimeSpan.FromSeconds(30));
            return line ?? throw new IOException("peer exited: " + await errors);
        }
        internal Task<string> Sql(string command, string sql) => Command(command, Convert.ToHexStringLower(System.Text.Encoding.UTF8.GetBytes(sql)));
        internal async Task<string> Command(string command, string argument = "")
        {
            await process.StandardInput.WriteLineAsync(argument.Length == 0 ? command : command + "\t" + argument); await process.StandardInput.FlushAsync();
            var line = await Line(); var fields = line.Split('\t');
            if (fields[0] == "OK") return fields.Length > 1 ? fields[1] : "";
            throw new InvalidDataException("peer command " + command + ": " + line);
        }
        internal async Task Close()
        {
            if (closed) return; await Command("CLOSE"); await process.WaitForExitAsync().WaitAsync(TimeSpan.FromSeconds(30)); closed = true;
            if (process.ExitCode != 0) throw new IOException("peer close failed: " + await errors);
        }
        public async ValueTask DisposeAsync()
        {
            try { if (!closed && !process.HasExited) await Close(); }
            finally { if (!process.HasExited) { process.Kill(entireProcessTree: true); await process.WaitForExitAsync(); } process.Dispose(); }
        }
    }
}
