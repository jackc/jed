using System.Runtime.InteropServices;
using Jed;

internal static class FileCoordinationTests
{
    internal static async Task Run()
    {
        var directory = Path.Combine(Path.GetTempPath(), "jed-csharp-lock-" + Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(directory);
        try
        {
            var path = Path.Combine(directory, "main.jed"); File.WriteAllBytes(path, []);
            var alias = Path.Combine(directory, "alias.jed"); File.CreateSymbolicLink(alias, path);
            using (var claim = FileClaim.Acquire(path, creating: false))
            {
                Reject(SqlStates.ObjectInUse, () => FileClaim.Acquire(alias, creating: false).Dispose(), "symlink normalized duplicate");
                using var bundle = await FileLockBundle.PrepareAsync(claim, 0);
                await bundle.AcquireExclusiveAsync(0);
                var incarnation = Enumerable.Range(0, 16).Select(i => (byte)i).ToArray();
                var endpoint = bundle.SelectEndpoint(incarnation);
                if (endpoint is null) throw new InvalidDataException("test host has no private endpoint directory");
                var owner = new PageOwnerRecord(true, LinuxFileHost.EffectiveUid, incarnation, (uint)Environment.ProcessId, endpoint);
                await bundle.PublishOwnerAsync(owner);
                Corpus.Equal(endpoint, bundle.ReadOwner()!.Endpoint, "atomic owner record publication");
                Corpus.Equal(true, File.Exists(Path.Combine(path + ".lock", "protocol-v2")), "persistent protocol marker");
            }
            Corpus.Equal(false, File.Exists(Path.Combine(path + ".lock", "owner")), "close removes owner hint");
            Corpus.Equal(true, Directory.Exists(path + ".lock"), "close keeps stable bundle");
            File.WriteAllBytes(Path.Combine(path + ".lock", "protocol-v3"), []);
            using (var claim = FileClaim.Acquire(path, false))
                await RejectAsync(SqlStates.FeatureNotSupported, async () => { using var bundle = await FileLockBundle.PrepareAsync(claim, 0); }, "unknown marker");
            File.Delete(Path.Combine(path + ".lock", "protocol-v3"));
            File.Delete(Path.Combine(path + ".lock", "presence"));
            File.CreateSymbolicLink(Path.Combine(path + ".lock", "presence"), path);
            using (var claim = FileClaim.Acquire(path, false))
                await RejectAsync(SqlStates.IoError, async () => { using var bundle = await FileLockBundle.PrepareAsync(claim, 0); }, "nofollow lock entry");
            var hardLink = Path.Combine(directory, "hard.jed");
            if (Link(path, hardLink) != 0) throw new IOException("test hard link failed");
            Reject(SqlStates.ObjectInUse, () => FileClaim.Acquire(hardLink, false).Dispose(), "hard links split lock domains");
            var absent = Path.Combine(directory, "create.jed");
            using var creation = FileClaim.Acquire(absent, true);
            Corpus.Equal(absent, creation.Path, "absent create normalizes real parent");
            Console.WriteLine("PASS file host invariants: normalized identity, hard-link rejection, stable protocol-v2 bundle, nofollow entries, private endpoint and owner record lifetime");
        }
        finally { Directory.Delete(directory, recursive: true); }
    }
    private static void Reject(string sqlState, Action operation, string name)
    {
        try { operation(); } catch (SqlException error) when (error.SqlState == sqlState) { return; }
        throw new InvalidDataException("accepted invalid file coordination: " + name);
    }
    private static async Task RejectAsync(string sqlState, Func<Task> operation, string name)
    {
        try { await operation(); } catch (SqlException error) when (error.SqlState == sqlState) { return; }
        throw new InvalidDataException("accepted invalid file coordination: " + name);
    }
    [DllImport("libc", EntryPoint = "link", SetLastError = true)]
    private static extern int Link([MarshalAs(UnmanagedType.LPUTF8Str)] string source, [MarshalAs(UnmanagedType.LPUTF8Str)] string target);
}
