using System.Text.Json;
using Jed;

internal static class MemoryTests
{
    internal static async Task Run(string root)
    {
        using var data = JsonDocument.Parse(File.ReadAllBytes(Path.Combine(root, "impl/csharp-native/.build/memory-vectors.json")));
        var count = 0;
        foreach (var vector in data.RootElement.GetProperty("data").GetProperty("vector").EnumerateArray())
        {
            await using var session = new Session();
            if (vector.TryGetProperty("setup", out var setup)) foreach (var command in setup.EnumerateArray())
            { await using var result = await session.QueryAsync(command.GetString()!); while (await result.ReadAsync() is not null) { } }
            await using var rows = await session.QueryAsync(vector.GetProperty("sql").GetString()!);
            var row = await rows.ReadAsync() ?? throw new InvalidDataException("memory vector has no row");
            Corpus.Equal<Row?>(null, await rows.ReadAsync(), "one row per memory vector");
            var actual = vector.GetProperty("measure").GetString() == "value" ? MemorySize.ValueBytes(row[0]) :
                MemorySize.RowBytes(Enumerable.Range(0, row.Count).Select(index => row[index]).ToArray());
            Corpus.Equal(vector.GetProperty("bytes").GetInt64(), actual, vector.GetProperty("sql").GetString()!); count++;
        }
        await using var owner = new Session();
        var meter = new Meter(owner, 0, CancellationToken.None, new() { MaxQueryMemoryBytes = 64 });
        meter.Memory.Reserve(64); Corpus.Equal(false, meter.Memory.TryReserve(1), "rejected reservation changes no account");
        Corpus.Equal(64L, meter.Memory.Used, "account equality admission"); meter.Memory.Release(32); meter.Memory.HoldPending(16);
        meter.Memory.Release(32); Corpus.Equal(16L, meter.Memory.Used, "pending memory survives row release");
        Corpus.Equal(0, meter.Memory.ReleaseUnderflows, "balanced account releases");
        Console.WriteLine($"PASS query memory sizes: {count} complete shared vectors; account equality, release, pending floor");
    }
}
