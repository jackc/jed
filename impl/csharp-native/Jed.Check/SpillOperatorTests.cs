using System.Security.Cryptography;
using Jed;

internal static class SpillOperatorTests
{
    internal static async Task Run()
    {
        var directory = Path.Combine(Path.GetTempPath(), "jed-spill-operators-" + Guid.NewGuid().ToString("N")); Directory.CreateDirectory(directory);
        try
        {
            foreach (var encrypted in new[] { false, true })
            {
                await using var host = new ScratchHost(encrypted, RandomNumberGenerator.GetBytes, directory);
                await using var session = new Session();
                var meter = new Meter(session, 0, default, new QueryOptions { MaxQueryMemoryBytes = 4096 });
                var expected = Enumerable.Range(0, 300).OrderBy(i => i % 7).ToArray();
                await using (var sorter = new ExternalSorter(host, 256, meter, (a, b) => a[0].AsInteger().CompareTo(b[0].AsInteger())))
                {
                    for (var i = 0; i < 300; i++) await sorter.Add([SqlValue.Integer(i % 7), SqlValue.Integer(i), SqlValue.Text("value-" + i)], default);
                    var observed = new List<int>();
                    await foreach (var row in sorter.Read()) observed.Add((int)row[1].AsInteger());
                    Corpus.Equal(true, expected.SequenceEqual(observed), "external merge stability across compacted runs");
                    Corpus.Equal(true, sorter.Spilled, "actual sorter spill");
                    Corpus.Equal(true, sorter.PeakResidentBytes <= 256 + 137, "bounded sorter residency including current row");
                }
                Corpus.Equal(0L, meter.Memory.Used, "sorter releases all reservations");
                Corpus.Equal(0, meter.Memory.ReleaseUnderflows, "sorter reservation ownership");
                Corpus.Equal(0, Directory.GetFiles(directory).Length, "sorter removes all runs");
                var tight = new Meter(session, 0, default, new QueryOptions { MaxQueryMemoryBytes = 100 });
                await using (var spool = new SpillRowSpool(host, 4096, tight))
                {
                    for (var i = 0; i < 5; i++) await spool.Append([SqlValue.Integer(i), SqlValue.Text("x")], default);
                    Corpus.Equal(true, spool.Spilled, "query account spills before work_mem");
                    Corpus.Equal(0L, spool.ResidentBytes, "spilled spool releases resident prefix");
                    for (var pass = 0; pass < 2; pass++)
                    {
                        var n = 0; await foreach (var row in spool.Read()) Corpus.Equal((long)n++, row[0].AsInteger(), "repeatable spool order");
                        Corpus.Equal(5, n, "repeatable spool count");
                    }
                }
                Corpus.Equal(0L, tight.Memory.Used, "spool releases all reservations");
                Corpus.Equal(0, tight.Memory.ReleaseUnderflows, "spool reservation ownership");
                await using (var partial = new ExternalSorter(host, 1, meter, (a, b) => a[0].AsInteger().CompareTo(b[0].AsInteger())))
                {
                    for (var i = 0; i < 12; i++) await partial.Add([SqlValue.Integer(i)], default);
                    await foreach (var unused in partial.Read()) break;
                }
                Corpus.Equal(0, Directory.GetFiles(directory).Length, "early output disposal removes runs");
                Corpus.Equal(0L, meter.Accrued, "scratch replay adds no SQL cost");
            }
        }
        finally { Directory.Delete(directory, true); }
        Console.WriteLine("PASS spill operators: bounded stable run compaction, repeatable row spool, account-driven spill, plaintext/encrypted scratch and early cleanup");
    }
}
