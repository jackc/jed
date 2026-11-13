using System.Globalization;
using Jed;

// Shared deterministic schedules use real pinned handles and the real async
// writer gate. No sleeps, version counters, or simulated query results.
internal static class ConcurrencyCorpus
{
    private sealed record Handle(Session Session, bool Write);
    internal static async Task<int> Run(string file)
    {
        var lines = File.ReadAllLines(file);
        await using var database = await Database.CreateInMemoryAsync().ConfigureAwait(false);
        foreach (var line in lines)
            if (line.Trim().StartsWith("# attach:", StringComparison.Ordinal))
                await database.AttachInMemoryAsync(line.Trim()[9..].Trim()).ConfigureAwait(false);
        foreach (var line in lines)
            if (line.Trim().StartsWith("# max_storage_bytes:", StringComparison.Ordinal))
            {
                var fields = line.Trim()["# max_storage_bytes:".Length..].Split(' ', StringSplitOptions.RemoveEmptyEntries);
                var limit = long.Parse(fields[0], CultureInfo.InvariantCulture);
                var registry = database.AcquireAttachmentRegistry();
                try
                {
                    var target = fields.Length == 1 || fields[1] == "main" ? database : registry.Single(attachment => attachment.Name == fields[1]).Database;
                    target.MaxStorageBytes = limit;
                }
                finally { database.ReleaseAttachmentRegistry(); }
            }
        var initialVersion = database.Version;
        var handles = new Dictionary<string, Handle>(StringComparer.Ordinal);
        using var cancellation = new CancellationTokenSource();
        Task<Session>? blocked = null; string? blockedName = null;
        var count = 0;
        try
        {
            for (var i = 0; i < lines.Length;)
            {
                var text = Control(lines[i]); var lineNumber = i + 1; i++;
                if (text.Length == 0) continue;
                var fields = text.Split(' ', StringSplitOptions.RemoveEmptyEntries);
                try
                {
                    switch (fields[0])
                    {
                        case "open":
                            if (fields.Length is not (3 or 4) || fields[2] is not ("read" or "write") || handles.ContainsKey(fields[1]) || blockedName == fields[1]) throw Invalid("invalid open");
                            var write = fields[2] == "write";
                            if (fields.Length == 4)
                            {
                                if (!write || fields[3] != "blocks" || blocked is not null || !handles.Values.Any(handle => handle.Write)) throw Invalid("invalid blocked writer");
                                blockedName = fields[1]; blocked = database.OpenWriteSessionAsync(cancellationToken: cancellation.Token);
                                if (blocked.IsCompleted) throw Invalid("writer gate did not block");
                            }
                            else
                            {
                                if (write && handles.Values.Any(handle => handle.Write)) throw Invalid("writer open would deadlock without blocks");
                                var session = write ? await database.OpenWriteSessionAsync(cancellationToken: cancellation.Token).ConfigureAwait(false) :
                                    await database.OpenReadSessionAsync(cancellationToken: cancellation.Token).ConfigureAwait(false);
                                handles.Add(fields[1], new(session, write));
                            }
                            break;
                        case "on":
                            if (fields.Length < 4 || !handles.TryGetValue(fields[1], out var active)) throw Invalid("unknown session");
                            var header = string.Join(' ', fields.Skip(2)); var sql = new List<string>(); var expected = new List<string>();
                            var query = fields[2] == "query";
                            while (i < lines.Length && lines[i].Trim().Length != 0 && lines[i].Trim() != "----" && !Step(lines[i])) sql.Add(lines[i++]);
                            if (query)
                            {
                                if (i >= lines.Length || lines[i].Trim() != "----") throw Invalid("missing query delimiter");
                                i++;
                                while (i < lines.Length && lines[i].Trim().Length != 0 && !Step(lines[i]) && !lines[i].TrimStart().StartsWith('#')) expected.Add(lines[i++]);
                            }
                            if (sql.Count == 0) throw Invalid("empty SQL record");
                            await Corpus.RunRecord(active.Session, new(lineNumber, header, string.Join('\n', sql), expected.ToArray(), new(StringComparer.Ordinal))).ConfigureAwait(false);
                            count++; break;
                        case "commit": case "rollback": case "close":
                            if (fields.Length != 2 || !handles.Remove(fields[1], out var ended)) throw Invalid("unknown ended session");
                            if (fields[0] != "close" && !ended.Write) throw Invalid("read handle only supports close");
                            if (fields[0] == "commit") await ended.Session.CommitAsync().ConfigureAwait(false);
                            else if (fields[0] == "rollback") await ended.Session.RollbackAsync().ConfigureAwait(false);
                            else await ended.Session.DisposeAsync().ConfigureAwait(false);
                            if (ended.Write && blocked is not null)
                            {
                                var acquired = await blocked.ConfigureAwait(false);
                                handles.Add(blockedName!, new(acquired, true)); blocked = null; blockedName = null;
                            }
                            break;
                        case "expect":
                            if (fields.Length != 3) throw Invalid("invalid expectation");
                            var actual = fields[1] switch { "version" => database.Version, "oldest_live" => database.OldestLiveVersion, _ => throw Invalid("unknown expectation") };
                            Corpus.Equal(ulong.Parse(fields[2], CultureInfo.InvariantCulture), checked(actual - initialVersion), fields[1]);
                            count++; break;
                        default: throw Invalid("unknown schedule operation: " + fields[0]);
                    }
                }
                catch (Exception error) { throw new InvalidDataException($"{file}:{lineNumber}: {text}\n{error.Message}", error); }
            }
            if (blocked is not null || handles.Count != 0) throw Invalid("schedule leaves handles open");
        }
        finally
        {
            cancellation.Cancel();
            foreach (var handle in handles.Values) await handle.Session.DisposeAsync().ConfigureAwait(false);
            if (blocked is not null)
                try { await (await blocked.ConfigureAwait(false)).DisposeAsync().ConfigureAwait(false); }
                catch (OperationCanceledException) { }
        }
        Console.WriteLine($"PASS {file}: {count} concurrency records/assertions"); return count;
    }
    private static string Control(string line) => line.Split('#', 2)[0].Trim();
    private static bool Step(string line)
    {
        var text = Control(line);
        return new[] { "open ", "on ", "commit ", "rollback ", "close ", "expect " }.Any(prefix => text.StartsWith(prefix, StringComparison.Ordinal));
    }
    private static InvalidDataException Invalid(string message) => new(message);
}
