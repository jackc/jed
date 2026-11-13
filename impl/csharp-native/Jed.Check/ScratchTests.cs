using Jed;
using System.Security.Cryptography;

internal static class ScratchTests
{
    internal static async Task Run()
    {
        await RoundTrip();
        foreach (var attack in new[] { "bit", "replay", "ordinal", "file", "truncate" }) await Reject(attack);
        var directory = Path.Combine(Path.GetTempPath(), "jed-scratch-tests-" + Guid.NewGuid().ToString("N")); Directory.CreateDirectory(directory);
        try
        {
            foreach (var encrypted in new[] { false, true })
            {
                await using var host = new ScratchHost(encrypted, RandomNumberGenerator.GetBytes, directory);
                var bytes = Enumerable.Range(0, 131072).Select(i => unchecked((byte)(i * 31))).ToArray();
                await using (var store = host.CreateSpillable(65536))
                {
                    await store.WriteAsync(0, bytes.AsMemory(0, 65536)); Corpus.Equal(false, store.Spilled, "scratch stays within resident allowance");
                    await store.WriteAsync(65536, bytes.AsMemory(65536)); Corpus.Equal(true, store.Spilled, "scratch crosses into native file");
                    var result = new byte[bytes.Length]; await store.ReadAsync(0, result);
                    Corpus.Equal(true, bytes.AsSpan().SequenceEqual(result), "migration preserves every logical byte");
                    for (var i = 0; i < 24; i++) await store.WriteAsync(bytes.Length + (long)i * bytes.Length, bytes);
                    Corpus.Equal(true, store.Spilled, "larger scratch retains only bounded I/O buffers");
                    foreach (var path in Directory.GetFiles(directory))
                        if (!OperatingSystem.IsWindows()) Corpus.Equal(UnixFileMode.UserRead | UnixFileMode.UserWrite, File.GetUnixFileMode(path), "scratch permissions private at creation");
                    await store.SetLengthAsync(9); await store.SetLengthAsync(100);
                    var zero = new byte[91]; await store.ReadAsync(9, zero); Corpus.Equal(true, zero.All(value => value == 0), "spilled truncate/regrow zeros tail");
                }
                Corpus.Equal(0, Directory.GetFiles(directory).Length, "operator disposal removes scratch file");
                var leaked = await host.CreateAsync(); await leaked.WriteAsync(0, bytes);
                await host.DisposeAsync();
                Corpus.Equal(0, Directory.GetFiles(directory).Length, "handle disposal removes outstanding scratch files");
                await leaked.DisposeAsync();
            }
            await using var failed = new ScratchHost(true, RandomNumberGenerator.GetBytes, Path.Combine(directory, "missing"));
            await using var resident = failed.CreateSpillable(4); await resident.WriteAsync(0, new byte[] { 1, 2, 3, 4 });
            await HostTests.Error(SqlStates.IoError, async () => await resident.WriteAsync(4, new byte[] { 5 }));
            Corpus.Equal(false, resident.Spilled, "failed migration does not adopt partial file");
            Corpus.Equal(4L, await resident.GetLengthAsync(), "failed spill cannot retain overflowing write in memory");
            using var cancelled = new CancellationTokenSource(); cancelled.Cancel();
            try { await failed.CreateAsync(cancelled.Token); throw new InvalidDataException("cancelled scratch create succeeded"); }
            catch (OperationCanceledException) { }
        }
        finally { Directory.Delete(directory, true); }
        Console.WriteLine("PASS scratch host: bounded migration, private ephemeral files, handle cleanup, encrypted partial IO/holes/truncation, counter/file/ordinal replay rejection and fail-closed spill");
    }
    private static async Task RoundTrip()
    {
        await using var raw = new MemoryBlockStore();
        await using var store = new EncryptedScratchStore(raw, new byte[32], new byte[16], RandomNumberGenerator.GetBytes);
        await store.SetLengthAsync(16000);
        var original = Enumerable.Range(0, 10000).Select(i => unchecked((byte)(i * 17))).ToArray();
        await store.WriteAsync(2000, original); var result = new byte[16000]; await store.ReadAsync(0, result);
        Corpus.Equal(true, result.AsSpan(2000, original.Length).SequenceEqual(original), "encrypted unaligned multiple blocks roundtrip");
        Corpus.Equal(true, result.AsSpan(0, 2000).IndexOfAnyExcept((byte)0) < 0 && result.AsSpan(12000).IndexOfAnyExcept((byte)0) < 0, "unwritten blocks and gaps zero");
        await store.SetLengthAsync(5100); await store.SetLengthAsync(16000);
        result.AsSpan().Fill(99); await store.ReadAsync(0, result);
        Corpus.Equal(true, result.AsSpan(5100).IndexOfAnyExcept((byte)0) < 0, "truncation zeros kept tail and removes later blocks");
    }
    private static async Task Reject(string attack)
    {
        await using var raw = new MemoryBlockStore();
        await using var store = new EncryptedScratchStore(raw, new byte[32], new byte[16], RandomNumberGenerator.GetBytes);
        var block = Enumerable.Repeat((byte)42, 4096).ToArray(); await store.WriteAsync(0, block);
        var saved = new byte[4136]; await raw.ReadAsync(0, saved);
        long target = 0;
        if (attack == "bit") saved[34] ^= 1;
        if (attack is "replay" or "truncate")
        {
            if (attack == "truncate") { await store.SetLengthAsync(0); await store.SetLengthAsync(4096); }
            block[0] = 77; await store.WriteAsync(0, block);
        }
        if (attack == "ordinal") { await store.WriteAsync(4096, block); target = 4136; }
        if (attack == "file")
        {
            await using var otherRaw = new MemoryBlockStore();
            await using var other = new EncryptedScratchStore(otherRaw, new byte[32], Enumerable.Repeat((byte)1, 16).ToArray(), RandomNumberGenerator.GetBytes);
            await other.WriteAsync(0, block); await otherRaw.ReadAsync(0, saved);
        }
        await raw.WriteAsync(target, saved);
        await HostTests.Error(SqlStates.IoError, async () => await store.ReadAsync(target == 0 ? 0 : 4096, new byte[4096]));
    }
}
