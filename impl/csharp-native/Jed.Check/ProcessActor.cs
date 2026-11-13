using System.Globalization;
using Jed;

// Canonical process corpus line protocol. All SQL runs through native sessions;
// maintenance commands invoke the same host seams as the library API.
internal static class ProcessActor
{
    internal static async Task<int> Run(string[] arguments)
    {
        Database? database = null; Session? reader = null; Session? writer = null;
        try
        {
            if (arguments.Length < 2) throw new SqlException("22023", "actor requires action and path");
            var action = arguments[0]; var path = arguments[1];
            if (action == "pages")
            {
                await using var stream = new ProcessDuplexStream(Console.OpenStandardInput(), Console.OpenStandardOutput());
                await Database.ServePageStreamAsync(path, stream).ConfigureAwait(false); return 0;
            }
            var settings = arguments.Skip(2).Where(value => value.Contains('=')).Select(value => value.Split('=', 2)).ToDictionary(pair => pair[0], pair => pair[1], StringComparer.Ordinal);
            var positional = arguments.Skip(2).Where(value => !value.Contains('=')).ToArray();
            long Number(string name, long fallback) => settings.TryGetValue(name, out var value) ? long.Parse(value, CultureInfo.InvariantCulture) : fallback;
            using var credential = settings.TryGetValue("key", out var key) ? DatabaseCredential.FromKey(Convert.FromHexString(key)) : null;
            var options = new DatabaseFileOptions
            {
                Synchronous = settings.GetValueOrDefault("synchronous", "on") switch
                { "on" => Synchronous.On, "off" => Synchronous.Off, _ => throw new SqlException("22023", "invalid synchronous mode") },
                FlushIntervalMilliseconds = Number("flush_interval_ms", 1000),
                Credential = credential, ReadOnly = action is "open-read-only" or "open-remote-read-only",
                Locking = action == "open-exclusive" ? FileLocking.Exclusive : FileLocking.Auto,
                Ownership = settings.GetValueOrDefault("ownership", "prefer") switch { "prefer" => FileOwnership.Prefer, "yield" => FileOwnership.Yield, "never" => FileOwnership.Never, _ => throw new SqlException("22023", "invalid ownership") },
                FileLockTimeoutMilliseconds = positional.Length > 0 ? long.Parse(positional[0], CultureInfo.InvariantCulture) : 5000,
                IdleTransactionMilliseconds = positional.Length > 1 ? long.Parse(positional[1], CultureInfo.InvariantCulture) : 60000,
                MaximumConnections = checked((int)Number("max_connections", 0)), IdleSessionMilliseconds = Number("idle_session", 0),
                YieldGraceMilliseconds = Number("yield_grace", 5000), ShutdownGraceMilliseconds = Number("shutdown_grace", 30000)
            };
            database = action switch
            {
                "open-remote" or "open-remote-read-only" => await Database.OpenRemoteAsync(token => ProcessDuplexStream.Connect(path, token), options).ConfigureAwait(false),
                "create" => await Database.CreateFileAsync(path, options).ConfigureAwait(false),
                "open" or "open-read-only" or "open-exclusive" => await Database.OpenFileAsync(path, options).ConfigureAwait(false),
                _ => throw new SqlException("0A000", "actor transport is not implemented: " + action)
            };
            var activeDatabase = database;
            Console.WriteLine("READY"); await Console.Out.FlushAsync().ConfigureAwait(false);
            while (await Console.In.ReadLineAsync().ConfigureAwait(false) is { } line)
            {
                var fields = line.Split('\t', 2); var command = fields[0]; var argument = fields.Length == 2 ? fields[1] : "";
                try
                {
                    string value = "";
                    switch (command)
                    {
                        case "EXEC":
                            await using (var session = activeDatabase.CreateSession()) await Execute(session, Sql(argument)).ConfigureAwait(false); break;
                        case "QUERY_I64":
                            await using (var session = activeDatabase.CreateSession()) value = await Query(session, Sql(argument)).ConfigureAwait(false); break;
                        case "READ_OPEN":
                            if (reader is not null) await reader.DisposeAsync().ConfigureAwait(false);
                            reader = await activeDatabase.OpenReadSessionAsync().ConfigureAwait(false); break;
                        case "READ_QUERY_I64": value = await Query(reader ?? throw new SqlException("25000", "no actor reader"), Sql(argument)).ConfigureAwait(false); break;
                        case "READ_CLOSE": if (reader is not null) await reader.DisposeAsync().ConfigureAwait(false); reader = null; break;
                        case "WRITE_OPEN":
                            if (writer is not null) await writer.DisposeAsync().ConfigureAwait(false);
                            writer = null;
                            var timeout = argument.Length == 0 ? 0 : long.Parse(argument, CultureInfo.InvariantCulture);
                            writer = timeout == 0 ? await activeDatabase.OpenWriteSessionAsync().ConfigureAwait(false) :
                                await activeDatabase.OpenWriteSessionAsync(TimeSpan.FromMilliseconds(timeout)).ConfigureAwait(false); break;
                        case "WRITE_EXEC": await Execute(writer ?? throw new SqlException("25000", "no actor writer"), Sql(argument)).ConfigureAwait(false); break;
                        case "WRITE_COMMIT":
                            if (writer is null) throw new SqlException("25000", "no actor writer");
                            var committing = writer; writer = null; await committing.CommitAsync().ConfigureAwait(false); break;
                        case "WRITE_ROLLBACK":
                            if (writer is null) break;
                            var rollingBack = writer; writer = null; await rollingBack.RollbackAsync().ConfigureAwait(false); break;
                        case "ATTACH":
                            var attached = argument.Split('\t', 3);
                            if (attached.Length != 3 || attached[1] is not ("0" or "1")) throw new SqlException("22023", "invalid attach arguments");
                            await activeDatabase.AttachFileAsync(attached[0], attached[2], new() { ReadOnly = attached[1] == "1", FileLockTimeoutMilliseconds = options.FileLockTimeoutMilliseconds }).ConfigureAwait(false); break;
                        case "DETACH": await activeDatabase.DetachAsync(argument).ConfigureAwait(false); break;
                        case "COMPACT": await activeDatabase.CompactAsync(argument.Length == 0 ? "main" : argument).ConfigureAwait(false); break;
                        case "BACKUP":
                            var backup = argument.Split('\t', 2);
                            if (backup.Length != 2 || backup[0] is not ("page_identical" or "compacted")) throw new SqlException("22023", "invalid backup arguments");
                            await activeDatabase.BackupForTestingAsync(backup[1], backup[0] == "compacted").ConfigureAwait(false); break;
                        case "CHANGE_KEY":
                            using (var replacement = DatabaseCredential.FromKey(Convert.FromHexString(argument))) await activeDatabase.ChangeCredentialAsync(replacement).ConfigureAwait(false); break;
                        case "REKEY":
                            using (var replacement = argument.Length == 0 ? null : DatabaseCredential.FromKey(Convert.FromHexString(argument))) await activeDatabase.RekeyAsync(replacement).ConfigureAwait(false); break;
                        case "MAX_STORAGE_BYTES":
                            var limit = long.Parse(argument, CultureInfo.InvariantCulture);
                            if (limit < 0) throw new SqlException("22023", "invalid storage limit"); activeDatabase.MaxStorageBytes = limit; break;
                        case "STORAGE_BYTES": case "PAGE_COUNT": case "TXID":
                            await using (var pin = await activeDatabase.PinAsync().ConfigureAwait(false))
                            {
                                var meta = pin.Snapshot.Meta;
                                value = command switch { "STORAGE_BYTES" => ((ulong)meta.PageSize * meta.LivePages).ToString(CultureInfo.InvariantCulture),
                                    "PAGE_COUNT" => meta.PageCount.ToString(CultureInfo.InvariantCulture), _ => meta.TransactionId.ToString(CultureInfo.InvariantCulture) };
                            }
                            break;
                        case "ROLE":
                            if (argument.Length == 0) value = await activeDatabase.GetFileRoleAsync().ConfigureAwait(false);
                            else
                            {
                                var registry = activeDatabase.AcquireAttachmentRegistry();
                                try { value = await (registry.FirstOrDefault(item => item.Name == argument)?.Database ?? throw new SqlException("42704", "database is not attached")).GetFileRoleAsync().ConfigureAwait(false); }
                                finally { activeDatabase.ReleaseAttachmentRegistry(); }
                            }
                            break;
                        case "HANGUP_NEXT_ACK": activeDatabase.BeforeNextAcknowledgementForTesting(() => throw new IOException("injected lost acknowledgement")); break;
                        case "CRASH_NEXT_ACK": activeDatabase.BeforeNextAcknowledgementForTesting(() => Environment.Exit(86)); break;
                        case "CLOSE":
                            if (reader is not null) await reader.DisposeAsync().ConfigureAwait(false); reader = null;
                            if (writer is not null) await writer.DisposeAsync().ConfigureAwait(false); writer = null;
                            Reply(); await activeDatabase.DisposeAsync().ConfigureAwait(false); database = null; return 0;
                        default: throw new SqlException("22023", "unknown actor command: " + command);
                    }
                    Reply(value);
                }
                catch (Exception error) { Reply(error); }
            }
            return 0;
        }
        catch (Exception error) { Reply(error); return 1; }
        finally
        {
            if (reader is not null) await reader.DisposeAsync().ConfigureAwait(false);
            if (writer is not null) await writer.DisposeAsync().ConfigureAwait(false);
            if (database is not null) await database.DisposeAsync().ConfigureAwait(false);
        }
    }
    private static void Reply(string value = "") { Console.WriteLine("OK\t" + value); Console.Out.Flush(); }
    private static void Reply(Exception error)
    {
        Console.WriteLine("ERR\t" + (error is ISqlError sql ? sql.SqlState : "XXXXX") + "\t" + Convert.ToHexStringLower(System.Text.Encoding.UTF8.GetBytes(error.Message))); Console.Out.Flush();
        if (Environment.GetEnvironmentVariable("JED_PROCESS_TRACE") == "1") Console.Error.WriteLine(error);
    }
    private static string Sql(string hex) => new System.Text.UTF8Encoding(false, true).GetString(Convert.FromHexString(hex));
    private static async Task Execute(Session session, string sql)
    {
        await using var rows = await session.QueryAsync(sql).ConfigureAwait(false);
        while (await rows.ReadAsync().ConfigureAwait(false) is not null) { }
    }
    private static async Task<string> Query(Session session, string sql)
    {
        await using var rows = await session.QueryAsync(sql).ConfigureAwait(false); var output = new List<string>();
        while (await rows.ReadAsync().ConfigureAwait(false) is { } row)
            output.Add(string.Join(':', Enumerable.Range(0, row.Count).Select(index => row[index].AsInteger().ToString(CultureInfo.InvariantCulture))));
        return string.Join(',', output);
    }
}
