using System.Security.Cryptography;
using Jed;

internal static class SpillScratchTests
{
    internal static async Task Run()
    {
        Keys();
        var directory = Path.Combine(Path.GetTempPath(), "jed-scratch-membership-" + Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(directory);
        try
        {
            foreach (var encrypted in new[] { false, true })
            {
                await using var host = new ScratchHost(encrypted, RandomNumberGenerator.GetBytes, directory);
                await using var session = new Session();
                var meter = new Meter(session, 0, default, new() { MaxQueryMemoryBytes = 4096 });
                await using (var state = new SpillKeyState(host, 128, meter))
                {
                    byte[] firstKey = [1], firstValue = [17, 18];
                    await state.Put(firstKey, firstValue, 64, default);
                    firstKey[0] = 9; firstValue[0] = 99;
                    byte[] present = [1], missing = [2];
                    Corpus.Equal(true, await state.Contains(present, default), "resident membership keeps owned input key");
                    var before = GC.GetAllocatedBytesForCurrentThread();
                    for (var i = 0; i < 1000; i++)
                    {
                        if (!state.Contains(present, default).GetAwaiter().GetResult() || state.Contains(missing, default).GetAwaiter().GetResult())
                            throw new InvalidOperationException("incorrect resident membership");
                    }
                    Corpus.Equal(0L, GC.GetAllocatedBytesForCurrentThread() - before, "resident membership allocates no owned value copies");
                    await Verify(state, meter, false);
                    await state.Put([2], [27], 64, default);
                    Corpus.Equal(false, state.Spilled, "membership preserves exact work_mem boundary");
                    Corpus.Equal(128L, meter.Memory.Used, "membership preserves resident logical charges");
                    await state.Put([3], [37], 64, default);
                    await Verify(state, meter, true);
                    Corpus.Equal(0L, meter.Memory.Used, "disk transition releases resident charges");
                    Corpus.Equal(0L, await state.Put([1], [47], 64, default), "membership does not change original update ordinal");
                    var ordinal = 0;
                    await foreach (var entry in state.Read(default))
                    {
                        Corpus.Equal((long)ordinal, entry.Value.Ordinal, "membership preserves insertion replay ordinal");
                        Corpus.Equal((byte)++ordinal, entry.Key[0], "membership preserves insertion replay key");
                        Corpus.Equal((byte)(ordinal == 1 ? 47 : ordinal * 10 + 7), entry.Value.Bytes[0], "owned values survive membership probes and updates");
                    }
                    Corpus.Equal(3, ordinal, "membership never inserts missing keys");
                }
                Corpus.Equal(0L, meter.Memory.Used, "membership state releases reservations");
                Corpus.Equal(0, meter.Memory.ReleaseUnderflows, "membership state releases exactly once");
            }
            Corpus.Equal(0, Directory.GetFiles(directory).Length, "membership state cleans up scratch files");
        }
        finally { Directory.Delete(directory, true); }
        Console.WriteLine("PASS scratch key framing, allocation and membership ownership across resident/plain/encrypted storage");
    }

    private static async Task Verify(SpillKeyState state, Meter meter, bool spilled)
    {
        Corpus.Equal(spilled, state.Spilled, "expected membership storage mode");
        var held = meter.Memory.Used; var count = state.Count;
        Corpus.Equal(true, await state.Contains([1], default), "present membership");
        Corpus.Equal(false, await state.Contains([255], default), "absent membership");
        var owned = (await state.Get([1], default))!;
        Corpus.Equal((byte)17, owned.Bytes[0], "membership does not alter resident value");
        owned.Bytes[0] = 88;
        Corpus.Equal((byte)17, (await state.Get([1], default))!.Bytes[0], "Get still returns independently owned bytes");
        using var cancellation = new CancellationTokenSource(); cancellation.Cancel();
        var cancelled = false;
        try { _ = await state.Contains([1], cancellation.Token); } catch (OperationCanceledException) { cancelled = true; }
        Corpus.Equal(true, cancelled, "membership checks cancellation in every storage mode");
        Corpus.Equal(count, state.Count, "membership leaves count unchanged");
        Corpus.Equal(held, meter.Memory.Used, "membership leaves logical reservation unchanged");
    }

    private static void Keys()
    {
        static byte[] Reference(byte kind, long group, int aggregate, int phase, long ordinal, byte[]? suffix)
        {
            var writer = new WireWriter(); writer.Byte(kind); writer.U64((ulong)group); writer.U32((uint)aggregate);
            writer.U32((uint)phase); writer.U64((ulong)ordinal); if (suffix is not null) writer.Bytes(suffix);
            return writer.ToArray();
        }
        foreach (var suffix in new byte[]?[] { null, [], [0, 255, 1], Enumerable.Range(0, 1024).Select(i => (byte)(i % 256)).ToArray() })
        foreach (var kind in new byte[] { 0, 1, 2, 3, 255 })
        {
            var actual = SpillAggregateInputs.Key(kind, long.MaxValue, int.MaxValue, 31, 9876543210, suffix);
            Corpus.Equal(Convert.ToHexString(Reference(kind, long.MaxValue, int.MaxValue, 31, 9876543210, suffix)), Convert.ToHexString(actual), "exact aggregate scratch key framing");
            if (suffix is { Length: > 0 })
            {
                var old = actual[25]; suffix[0] ^= 255;
                Corpus.Equal(old, actual[25], "scratch key owns suffix bytes"); suffix[0] ^= 255;
            }
        }
        SqlValue[][] tuples = [[], [SqlValue.Integer(-17), SqlValue.Text("a\0🙂")], [SqlValue.Null(SqlType.I64), SqlValue.Float(-0.0)]];
        foreach (var tuple in tuples)
        foreach (var set in new[] { 0, 1, int.MaxValue })
        {
            var writer = new WireWriter(); writer.U32((uint)set); writer.Bytes(GroupExecution.EncodedIdentity(tuple));
            Corpus.Equal(Convert.ToHexString(writer.ToArray()), Convert.ToHexString(SpillGroup.Key(set, tuple)), "exact group scratch key framing");
        }
        byte[] tail = [1, 2, 3];
        _ = Reference(1, 2, 3, 4, 5, tail); _ = SpillAggregateInputs.Key(1, 2, 3, 4, 5, tail);
        var before = GC.GetAllocatedBytesForCurrentThread();
        for (var i = 0; i < 1000; i++) GC.KeepAlive(Reference(1, 2, 3, 4, 5, tail));
        var referenceBytes = GC.GetAllocatedBytesForCurrentThread() - before;
        before = GC.GetAllocatedBytesForCurrentThread();
        for (var i = 0; i < 1000; i++) GC.KeepAlive(SpillAggregateInputs.Key(1, 2, 3, 4, 5, tail));
        var directBytes = GC.GetAllocatedBytesForCurrentThread() - before;
        Corpus.Equal(true, directBytes < referenceBytes, "fixed scratch key avoids writer intermediate buffers");
        Console.WriteLine($"PASS scratch key allocation probe: {referenceBytes} to {directBytes} bytes for 1000 keys");
    }
}
