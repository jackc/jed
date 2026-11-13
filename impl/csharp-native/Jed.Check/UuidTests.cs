using System.Globalization;
using System.Text.Json;
using Jed;

internal static class UuidTests
{
    internal static async Task Run(string root)
    {
        SeededRandomSource? source = null; var vectors = 0;
        foreach (var line in File.ReadLines(Path.Combine(root, "spec/encoding/prng.toml")).Select(line => line.Trim()))
        {
            if (line.StartsWith("[[")) { source = line == "[[stream]]" ? new(0) : null; continue; }
            if (source is null) continue;
            if (line.StartsWith("seed = ")) source.Reseed(ulong.Parse(line[7..], CultureInfo.InvariantCulture));
            if (!line.StartsWith("outputs = ")) continue;
            using var values = JsonDocument.Parse(line[10..]);
            foreach (var expected in values.RootElement.EnumerateArray())
            {
                Corpus.Equal(expected.GetString(), Convert.ToHexStringLower(source.Fill(8)), "complete shared splitmix stream");
                vectors++;
            }
        }
        Corpus.Equal(10, vectors, "all shared splitmix stream outputs covered");
        var partial = new SeededRandomSource(1);
        Corpus.Equal("910a", Convert.ToHexStringLower(partial.Fill(2)), "partial fill takes high bytes");
        Corpus.Equal("beeb8da1658eec67", Convert.ToHexStringLower(partial.Fill(8)), "partial fill consumes a whole draw");
        partial.Reseed(1);
        Corpus.Equal("910a2dec89025cc1", Convert.ToHexStringLower(partial.Fill(8)), "host reseed restarts stream");
        var hostBuffer = new byte[16];
        await using (var session = new Session(new SessionOptions { RandomSource = _ => hostBuffer }))
        {
            Corpus.Equal("00000000-0000-4000-8000-000000000000", await DatabaseFileTests.Scalar(session, "SELECT uuidv4()"), "host random buffer fills UUID");
            Corpus.Equal(true, hostBuffer.All(value => value == 0), "UUID owns and does not mutate host entropy buffer");
        }
        await using (var session = new Session(new SessionOptions { RandomSource = _ => [] }))
            await HostTests.Error(SqlStates.IoError, async () => { _ = await DatabaseFileTests.Scalar(session, "SELECT uuidv4()"); });
        Console.WriteLine("PASS UUID host entropy: complete shared splitmix streams, partial-fill/reseed contract, host-buffer ownership and entropy error");
    }
}
