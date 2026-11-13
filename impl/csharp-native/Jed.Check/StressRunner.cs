using System.Diagnostics;
using System.Text;
using System.Text.Json;
using Jed;

internal static class StressRunner
{
    internal static async Task Run(string input, string storage)
    {
        if (storage is not ("memory" or "disk" or "encrypted")) throw new InvalidDataException("unknown stress storage");
        using var data = JsonDocument.Parse(await File.ReadAllBytesAsync(input));
        if (data.RootElement.GetProperty("schema").GetInt32() != 1) throw new InvalidDataException("unknown stress schema");
        foreach (var profile in data.RootElement.GetProperty("profiles").EnumerateArray()) await Profile(profile, storage);
    }

    private static async Task Profile(JsonElement profile, string storage)
    {
        using var deadline = new CancellationTokenSource(TimeSpan.FromSeconds(60));
        var token = deadline.Token;
        var directory = Path.Combine(Path.GetTempPath(), "jed-csharp-stress-" + Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(directory);
        try
        {
            using var key = storage == "encrypted" ? DatabaseCredential.FromKey(new byte[32]) : null;
            await using var database = storage == "memory" ? await Database.CreateInMemoryAsync(cancellationToken: token) :
                await Database.CreateFileAsync(Path.Combine(directory, "stress.jed"), new() { Credential = key }, cancellationToken: token);
            await using (var setup = await database.OpenWriteSessionAsync(cancellationToken: token))
            {
                foreach (var sql in profile.GetProperty("setup").GetProperty("sql").EnumerateArray()) await Execute(setup, sql.GetString()!, token);
                await setup.CommitAsync(token);
            }
            var workers = new List<Task>();
            var start = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
            var watch = Stopwatch.StartNew();
            long checks = 0, writes = 0;
            var writers = 0;
            foreach (var worker in profile.GetProperty("worker").EnumerateArray())
            {
                var kind = worker.GetProperty("kind").GetString();
                var count = worker.GetProperty("count").GetInt32();
                var iterations = worker.GetProperty("iterations").GetInt32();
                if (count <= 0 || iterations <= 0 || kind is not ("writer" or "reader")) throw new InvalidDataException("invalid stress worker");
                if (kind == "writer") writers = checked(writers + count);
                for (var index = 0; index < count; index++)
                {
                    workers.Add(Task.Run(async () =>
                    {
                        await start.Task.WaitAsync(token);
                        try
                        {
                            for (var iteration = 0; iteration < iterations; iteration++)
                            {
                                if (kind == "writer")
                                {
                                    await using var session = await database.OpenWriteSessionAsync(cancellationToken: token);
                                    foreach (var sql in Operations(worker.GetProperty("op").GetString()!)) await Execute(session, sql, token);
                                    await session.CommitAsync(token);
                                    Interlocked.Increment(ref writes);
                                }
                                else
                                {
                                    await using var session = await database.OpenReadSessionAsync(cancellationToken: token);
                                    await using var rows = await session.QueryAsync(worker.GetProperty("invariant_query").GetString()!, cancellationToken: token);
                                    var seen = 0;
                                    await foreach (var row in rows.WithCancellation(token))
                                    {
                                        if (++seen != 1 || row.Count != 1 || row[0].ToString() != worker.GetProperty("invariant_expect").GetString())
                                            throw new InvalidDataException("parallel reader observed a torn snapshot");
                                    }
                                    if (seen != 1) throw new InvalidDataException("parallel reader observed no invariant row");
                                    Interlocked.Increment(ref checks);
                                }
                            }
                        }
                        catch { await deadline.CancelAsync(); throw; }
                    }, token));
                }
            }
            start.SetResult();
            await Task.WhenAll(workers).WaitAsync(token);
            var final = profile.GetProperty("final");
            var expected = final.GetProperty("expect").EnumerateArray().ToArray();
            var rendered = new StringBuilder(); var rowIndex = 0;
            await using (var reader = await database.OpenReadSessionAsync(cancellationToken: token))
            await using (var rows = await reader.QueryAsync(final.GetProperty("query").GetString()!, cancellationToken: token))
            {
                await foreach (var row in rows.WithCancellation(token))
                {
                    if (rowIndex >= expected.Length || row.Count != expected[rowIndex].GetArrayLength()) throw new InvalidDataException("stress final row shape differs");
                    for (var column = 0; column < row.Count; column++)
                    {
                        var value = row[column].ToString();
                        if (value != expected[rowIndex][column].GetRawText()) throw new InvalidDataException("stress final state differs");
                        rendered.Append(value).Append('\x1f');
                    }
                    rendered.Append('\x1e'); rowIndex++;
                }
            }
            if (rowIndex != expected.Length) throw new InvalidDataException("stress final rows missing");
            using var output = new MemoryStream();
            using (var json = new Utf8JsonWriter(output))
            {
                json.WriteStartObject(); json.WriteNumber("schema", 1); json.WriteString("name", profile.GetProperty("meta").GetProperty("name").GetString());
                json.WriteString("lang", "csharp-native"); json.WriteString("storage", storage); json.WriteString("mode", "threaded"); json.WriteString("status", "pass");
                json.WriteNumber("invariant_checks", checks); json.WriteNumber("writers", writers); json.WriteNumber("writer_iters", writes);
                json.WriteBoolean("final_ok", true); json.WriteString("checksum", SimulationRows.Fold(rendered.ToString()));
                json.WriteBoolean("cross_core_checksum", final.GetProperty("cross_core_checksum").GetBoolean()); json.WriteNumber("duration_ms", watch.ElapsedMilliseconds);
                json.WriteEndObject();
            }
            Console.WriteLine(Encoding.UTF8.GetString(output.ToArray()));
        }
        finally { Directory.Delete(directory, recursive: true); }
    }

    private static IEnumerable<string> Operations(string text) => text.Split(';').Select(part => part.Trim())
        .Where(sql => sql.Length > 0 && sql.ToUpperInvariant() is not ("BEGIN" or "COMMIT" or "ROLLBACK"));
    private static async Task Execute(Session session, string sql, CancellationToken token)
    {
        await using var rows = await session.QueryAsync(sql, cancellationToken: token);
        await foreach (var _ in rows.WithCancellation(token)) { }
    }
}
