using System.Text.Json;
using Jed;

internal static class EnvironmentHostTests
{
    internal static async Task Run(string root)
    {
        var clock = new ManualClock(1_000_000);
        var random = new SeededRandomSource(7);
        var environment = DatabaseEnvironment.Builder().WithClockSource(clock).WithRandomSource(random).WithInsecureKeySeed(19).Build();
        await using var first = await Database.CreateAsync(new() { Environment = environment });
        await using var second = await Database.CreateInMemoryAsync(environment: environment);
        try { await Database.CreateAsync(new() { PageSize = 300 }); throw new InvalidDataException("invalid page size admitted"); }
        catch (SqlException error) when (error.SqlState == SqlStates.FeatureNotSupported) { }
        await using var defaultFirst = await Database.CreateInMemoryAsync();
        await using var defaultSecond = await Database.CreateInMemoryAsync();
        Corpus.Equal(false, ReferenceEquals(defaultFirst.Environment, defaultSecond.Environment), "default environments are per handle");
        await using var one = first.CreateSession(); await using var two = second.CreateSession();
        Corpus.Equal(true, ReferenceEquals(environment, one.Environment), "session retains handle environment");
        var a = (await one.QueryFirstOrDefaultAsync("SELECT uuidv4()"))![0];
        var b = (await two.QueryFirstOrDefaultAsync("SELECT uuidv4()"))![0];
        random.Reseed(7);
        Corpus.Equal(a.ToString(), (await two.QueryFirstOrDefaultAsync("SELECT uuidv4()"))![0].ToString(), "shared environment random source can be reseeded");
        Corpus.Equal(b.ToString(), (await one.QueryFirstOrDefaultAsync("SELECT uuidv4()"))![0].ToString(), "sessions share one ordered random stream");
        Corpus.Equal("1970-01-01 00:00:01+00", (await one.QueryFirstOrDefaultAsync("SELECT now()"))![0].ToString(), "environment clock reaches SQL");
        clock.Advance(2_000_000);
        Corpus.Equal("1970-01-01 00:00:03+00", (await two.QueryFirstOrDefaultAsync("SELECT now()"))![0].ToString(), "clock control remains live through frozen environment");
        await first.AttachInMemoryAsync("attached");
        var attached = first.AcquireAttachmentRegistry();
        try { Corpus.Equal(true, ReferenceEquals(environment, attached.Single().Database.Environment), "attachment inherits handle environment"); }
        finally { first.ReleaseAttachmentRegistry(); }
        await one.ExecuteAsync("CREATE TEMP TABLE t (value timestamptz DEFAULT now())");
        await one.ExecuteAsync("INSERT INTO t DEFAULT VALUES");
        Corpus.Equal("1970-01-01 00:00:03+00", (await one.QueryFirstOrDefaultAsync("SELECT value FROM temp.t"))![0].ToString(), "temporary domain inherits clock");

        var fs = new SimulationFileSystem(29, 1024);
        try
        {
            await using var invalid = await Database.CreateAsync(new() { Path = "invalid-limit.jed", PageSize = 1024,
                FileHost = fs, Locking = FileLocking.None, MaxStorageBytes = -1 });
            throw new InvalidDataException("negative storage limit admitted");
        }
        catch (ArgumentOutOfRangeException) { }
        Corpus.Equal(false, await fs.ExistsAsync("invalid-limit.jed"), "invalid storage allowance is rejected before creating a file");
        await using (var abandoned = await fs.OpenAsync("retry-create.jed.jedtmp", FileMode.CreateNew))
        { await abandoned.WriteAsync(0, new byte[] { 1, 2, 3 }); }
        await using (var retried = await Database.CreateAsync(new() { Path = "retry-create.jed", PageSize = 1024,
            FileHost = fs, Locking = FileLocking.None }))
            Corpus.Equal(1UL, retried.Version, "create replaces its reserved stale temporary image");
        using var credential = DatabaseCredential.FromKey(new byte[32]);
        var options = new DatabaseFileOptions { Environment = environment, FileHost = fs, Locking = FileLocking.None, Credential = credential };
        await using (var encrypted = await Database.CreateFileAsync("environment-one.jed", options, 1024)) { }
        _ = random.Fill(256);
        await using (var encrypted = await Database.CreateFileAsync("environment-two.jed", options, 1024)) { }
        Corpus.Equal(true, fs.Image("environment-one.jed").AsSpan().SequenceEqual(fs.Image("environment-two.jed")), "fresh key stream per handle is separate from SQL entropy");

        using var baseline = JsonDocument.Parse(File.ReadAllBytes(Path.Combine(root, "impl/csharp-native/baseline.json")));
        var spec = Path.Combine(root, "impl/csharp-native/.build/baseline-" + baseline.RootElement.GetProperty("semantic_revision").GetString(), "spec");
        var loaded = environment.ToBuilder().LoadUnicodeData(File.ReadAllBytes(Path.Combine(spec, "collation/fixtures/unicode.jucd"))).Build();
        Corpus.Equal<UnicodeData?>(null, environment.UnicodeData, "derived builder cannot mutate original environment");
        var plainOptions = options with { Credential = null, Environment = loaded };
        await using (var database = await Database.CreateFileAsync("environment-collation.jed", plainOptions, 1024))
        {
            await using var session = database.CreateSession();
            await session.ExecuteAsync("CREATE TABLE words (s text COLLATE unicode PRIMARY KEY)");
            await session.ExecuteAsync("INSERT INTO words VALUES ('z'), ('a')");
        }
        try
        {
            await using var missing = await Database.OpenFileAsync("environment-collation.jed", plainOptions with { Environment = null });
            throw new InvalidDataException("open accepted an unavailable persisted collation");
        }
        catch (SqlException error) when (error.SqlState == SqlStates.CollationVersionMismatch) { }
        await using (var reopened = await Database.OpenAsync("environment-collation.jed", plainOptions))
        {
            await using var session = reopened.CreateSession();
            Corpus.Equal("a", (await session.QueryFirstOrDefaultAsync("SELECT s FROM words ORDER BY s"))![0].ToString(), "open binds loaded collation before storage validation");
            var image = await reopened.SerializeAsync();
            await using var imported = await Database.DeserializeAsync(image, new DatabaseDeserializeOptions { Environment = loaded, MaxStorageBytes = 4096 });
            Corpus.Equal(true, ReferenceEquals(loaded, imported.Environment), "deserialize retains supplied environment");
            Corpus.Equal(4096L, imported.MaxStorageBytes, "deserialize captures storage limit");
        }
        Console.WriteLine("PASS immutable environment: isolation, shared clock/random sources, attachment/temp inheritance, fresh key streams and open-time collation binding");
    }
}
