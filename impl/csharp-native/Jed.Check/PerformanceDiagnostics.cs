using System.Diagnostics;
using System.Globalization;
using System.Runtime;
using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json;
using Jed;

// An opt-in diagnostic host, deliberately separate from the engine and normal
// CI. SQL, iteration counts and setup are shared with an independent peer.
internal static class PerformanceDiagnostics
{
    private sealed class Checksum
    {
        internal ulong Value = 14695981039346656037;
        internal void Text(string text)
        {
            foreach (var b in Encoding.UTF8.GetBytes(text)) Value = unchecked((Value ^ b) * 1099511628211);
        }
        internal async Task<long> Drain(Rows rows)
        {
            await using (rows)
            {
                long count = 0;
                await foreach (var row in (IAsyncEnumerable<Row>)rows)
                {
                    for (var i = 0; i < row.Count; i++) { Text(row[i].ToString()); Text("\x1f"); }
                    Text("\x1e"); count++;
                }
                return count;
            }
        }
    }

    internal static async Task Ready()
    {
        await using var session = new Session();
        _ = await new Checksum().Drain(await session.QueryAsync("SELECT 1"));
        Console.WriteLine("ready");
    }

    private static async Task Execute(Session session, string sql) =>
        _ = await new Checksum().Drain(await session.QueryAsync(sql));

    internal static async Task Run(string inputPath)
    {
        using var input = JsonDocument.Parse(await File.ReadAllBytesAsync(inputPath));
        var directory = Path.Combine(Path.GetTempPath(), "jed-csharp-performance-" + Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(directory);
        try
        {
            foreach (var benchmark in input.RootElement.GetProperty("cases").EnumerateArray())
            {
                var name = benchmark.GetProperty("name").GetString()!;
                var sql = benchmark.GetProperty("sql").GetString()!;
                var mode = benchmark.GetProperty("mode").GetString()!;
                var warmup = benchmark.GetProperty("warmup").GetInt32();
                var iterations = benchmark.GetProperty("iterations").GetInt32();
                if (warmup < 0 || iterations <= 0) throw new ArgumentException("invalid diagnostic iteration count");
                var path = Path.Combine(directory, name + ".jed");
                var options = new DatabaseFileOptions { Synchronous = Synchronous.On };
                await using (var setup = await Database.CreateFileAsync(path, options))
                {
                    await using var session = setup.CreateSession();
                    foreach (var statement in input.RootElement.GetProperty("setup").EnumerateArray())
                        await Execute(session, statement.GetString()!);
                }
                Database? database = mode == "cold_open" ? null : await Database.OpenFileAsync(path, options);
                Session? connection = database?.CreateSession();
                try
                {
                    var prepared = connection?.Prepare(sql);
                    var checksum = new Checksum(); long rows = 0;
                    var samples = new long[iterations];
                    long allocationStart = 0, allocationEnd = 0; var collections = new int[3];
                    for (var i = 0; i < warmup + iterations; i++)
                    {
                        if (i == warmup)
                        {
                            allocationStart = GC.GetTotalAllocatedBytes(precise: true);
                            for (var generation = 0; generation < 3; generation++) collections[generation] = GC.CollectionCount(generation);
                        }
                        var measured = i >= warmup;
                        var sink = measured ? checksum : new Checksum();
                        var start = Stopwatch.GetTimestamp(); long count;
                        if (mode == "cold_open")
                        {
                            await using var opened = await Database.OpenFileAsync(path, options);
                            await using var session = opened.CreateSession();
                            count = await sink.Drain(await session.QueryAsync(session.Prepare(sql)));
                        }
                        else if (mode == "rollback")
                        {
                            await Execute(connection!, "BEGIN");
                            count = await sink.Drain(await connection!.QueryAsync(prepared!));
                            await Execute(connection, "ROLLBACK");
                        }
                        else count = await sink.Drain(await connection!.QueryAsync(prepared!));
                        if (measured)
                        {
                            samples[i - warmup] = checked((long)(Stopwatch.GetElapsedTime(start).TotalNanoseconds));
                            rows += count;
                        }
                    }
                    allocationEnd = GC.GetTotalAllocatedBytes(precise: true);
                    for (var generation = 0; generation < 3; generation++) collections[generation] = GC.CollectionCount(generation) - collections[generation];
                    // Writes must leave the intended durable state. This query is
                    // outside timing/allocation samples but part of answer parity.
                    if (mode is "rollback" or "durable")
                        rows += await checksum.Drain(await connection!.QueryAsync("SELECT count(*), sum(v) FROM t"));
                    using var output = new MemoryStream();
                    using (var json = new Utf8JsonWriter(output))
                    {
                        json.WriteStartObject(); json.WriteString("name", name); json.WriteString("mode", mode);
                        json.WriteString("runtime", RuntimeInformation.FrameworkDescription);
                        json.WriteBoolean("server_gc", GCSettings.IsServerGC); json.WriteString("gc_latency", GCSettings.LatencyMode.ToString());
                        json.WriteNumber("warmup", warmup); json.WriteNumber("iterations", iterations);
                        json.WriteNumber("rows", rows); json.WriteString("checksum", checksum.Value.ToString("x16", CultureInfo.InvariantCulture));
                        json.WriteNumber("allocated_bytes", allocationEnd - allocationStart);
                        json.WriteStartArray("gc_collections"); foreach (var count in collections) json.WriteNumberValue(count); json.WriteEndArray();
                        json.WriteStartArray("samples_ns"); foreach (var sample in samples) json.WriteNumberValue(sample); json.WriteEndArray();
                        json.WriteEndObject();
                    }
                    Console.WriteLine(Encoding.UTF8.GetString(output.ToArray()));
                }
                finally
                {
                    if (connection is not null) await connection.DisposeAsync();
                    if (database is not null) await database.DisposeAsync();
                }
            }
        }
        finally { Directory.Delete(directory, recursive: true); }
    }
}
