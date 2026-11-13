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
                var largeText = new string('x', 70_000);
                var largeArray = SqlValue.Array(new(SqlType.Text, [SqlValue.Text(largeText), SqlValue.Null(SqlType.Text)]));
                await using (var large = new SpillRowSpool(host, 1, tight))
                {
                    await large.Append([SqlValue.Text(largeText), SqlValue.Bytes(new byte[70_000]), largeArray], default);
                    await foreach (var row in large.Read())
                    {
                        Corpus.Equal(largeText, row[0].AsText(), "scratch text exceeds inline cell framing");
                        Corpus.Equal(70_000, row[1].AsBytes().Length, "scratch binary exceeds inline cell framing");
                        Corpus.Equal(largeText, row[2].AsArray().Values[0].AsText(), "scratch nested large scalar");
                        Corpus.Equal(true, row[2].AsArray().Values[1].IsNull, "scratch nested NULL survives");
                    }
                }
                foreach (var allowance in new[] { 1L, 4096L })
                {
                    var stateMeter = new Meter(session, 0, default, new QueryOptions { MaxQueryMemoryBytes = allowance });
                    await using (var state = new SpillKeyState(host, 256, stateMeter))
                    {
                        for (var i = 0; i < 100; i++)
                            Corpus.Equal((long)i, await state.Put(BitConverter.GetBytes(i), BitConverter.GetBytes(i * 2), 64, default), "state insertion ordinal");
                        Corpus.Equal(true, state.Spilled, "state spills under both limits");
                        for (var i = 99; i >= 0; i--)
                            Corpus.Equal((long)i, await state.Put(BitConverter.GetBytes(i), BitConverter.GetBytes(i * 3), 80, default), "updates preserve first ordinal");
                        Corpus.Equal(true, await state.Get(BitConverter.GetBytes(101), default) is null, "absent state key");
                        var ordinal = 0;
                        await foreach (var entry in state.Read(default))
                        {
                            Corpus.Equal(ordinal, BitConverter.ToInt32(entry.Key), "state first-occurrence replay");
                            Corpus.Equal(ordinal * 3, BitConverter.ToInt32(entry.Value.Bytes), "latest state payload");
                            var found = await state.Get(entry.Key, default);
                            Corpus.Equal((long)ordinal++, found!.Ordinal, "state lookup ordinal");
                        }
                        Corpus.Equal(100, ordinal, "state entry count");
                    }
                    Corpus.Equal(0L, stateMeter.Memory.Used, "state releases reservations");
                    Corpus.Equal(0, stateMeter.Memory.ReleaseUnderflows, "state reservation ownership");
                    Corpus.Equal(0, Directory.GetFiles(directory).Length, "state removes scratch files");
                }
                // A cancelled operation has the same boundary before and after
                // spill, and must not append a row or change keyed state.
                using var canceled = new CancellationTokenSource(); canceled.Cancel();
                async ValueTask Reject(Func<ValueTask> operation)
                {
                    try { await operation(); }
                    catch (OperationCanceledException error) when (error.CancellationToken == canceled.Token) { return; }
                    throw new InvalidDataException("scratch operation ignored cancellation");
                }
                foreach (var budget in new[] { 4096L, 1L })
                {
                    await using var state = new SpillKeyState(host, budget, meter);
                    await using var spool = new SpillRowSpool(host, budget, meter);
                    await using var hashRows = new SpillHashRows(host, budget, meter);
                    await using var sort = new ExternalSorter(host, budget, meter, (a, b) => a[0].AsInteger().CompareTo(b[0].AsInteger()));
                    await state.Put([1], [2], 64, default);
                    await spool.Append([SqlValue.Integer(1)], default);
                    await hashRows.Add(1, [SqlValue.Integer(1)], default);
                    await sort.Add([SqlValue.Integer(1)], default);
                    var held = meter.Memory.Used;
                    await Reject(async () => { _ = await state.Get([1], canceled.Token); });
                    await Reject(async () => { _ = await state.Put([1], [3], 64, canceled.Token); });
                    await Reject(() => spool.Append([SqlValue.Integer(2)], canceled.Token));
                    await Reject(() => hashRows.Add(1, [SqlValue.Integer(2)], canceled.Token));
                    await Reject(() => sort.Add([SqlValue.Integer(2)], canceled.Token));
                    Corpus.Equal(held, meter.Memory.Used, "cancelled scratch operations preserve reservations");
                    Corpus.Equal((byte)2, (await state.Get([1], default))!.Bytes[0], "cancelled update preserves payload");
                    var count = 0; await foreach (var unused in spool.Read()) count++;
                    Corpus.Equal(1, count, "cancelled append preserves spool");
                    count = 0; await foreach (var unused in hashRows.Read(1, default)) count++;
                    Corpus.Equal(1, count, "cancelled append preserves hash bucket");
                    count = 0; await foreach (var unused in sort.Read()) count++;
                    Corpus.Equal(1, count, "cancelled append preserves sorter");
                }
                Corpus.Equal(0L, meter.Memory.Used, "cancelled scratch owners release reservations");
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
