using System.Globalization;
using System.Text.RegularExpressions;
using Jed;

internal sealed record CorpusRecord(int Line, string Header, string Sql, string[] Expected, Dictionary<string, string> Directives);

internal static class Corpus
{
    // Other comments are prose (e.g. "# fallback:"). Recognized but unsupported
    // stateful actions must fail, including empty-valued and hyphenated forms.
    private static readonly HashSet<string> DirectiveNames = new(StringComparer.Ordinal)
    {
        "requires", "cost", "types", "names", "max_cost", "lifetime_max_cost", "params", "fixture", "skip",
        "load-collation", "load-timezone", "upgrade-collations", "timezone", "attach",
        "max_scalar_bytes", "max_query_memory_bytes", "max_sql_length", "max_storage_bytes", "max_temp_storage_bytes",
        "default_privileges", "grant", "revoke", "allow_ddl", "allow_temp_ddl", "work_mem", "set", "seed", "clock", "clock_advance"
    };
    internal static List<CorpusRecord> Parse(string file)
    {
        var lines = File.ReadAllLines(file);
        var records = new List<CorpusRecord>();
        var pending = new Dictionary<string, string>(StringComparer.Ordinal);
        for (var i = 0; i < lines.Length;)
        {
            var line = lines[i].Trim();
            if (line.Length == 0) { i++; continue; }
            if (line.StartsWith('#'))
            {
                var match = Regex.Match(line, "^#\\s*([a-z_-]+):\\s*(.*)$", RegexOptions.CultureInvariant);
                if (match.Success)
                {
                    var key = match.Groups[1].Value;
                    if (key is "load-timezone" or "load-collation" or "fixture" or "attach") { i++; continue; }
                    if (key == "skip" && match.Groups[2].Value.Split(' ', StringSplitOptions.RemoveEmptyEntries).FirstOrDefault() is "disk" or "memory" or "encrypted")
                    { i++; continue; } // Backend declarations select the applicable host; no SQL records are omitted.
                    if (key != "requires" && DirectiveNames.Contains(key))
                    {
                        if (key is not ("cost" or "types" or "names" or "max_cost" or "lifetime_max_cost" or "clock" or "clock_advance" or "seed" or "timezone" or "max_scalar_bytes" or "max_sql_length" or "params" or "upgrade-collations" or "allow_ddl" or "allow_temp_ddl" or "max_temp_storage_bytes" or "max_query_memory_bytes" or "max_storage_bytes" or "default_privileges" or "grant" or "revoke" or "set" or "work_mem"))
                            throw new InvalidDataException($"{file}:{i + 1}: unsupported harness directive {key}");
                        if (key is "grant" or "revoke" or "max_storage_bytes" && pending.TryGetValue(key, out var previous)) pending[key] = previous + "\n" + match.Groups[2].Value;
                        else if (!pending.TryAdd(key, match.Groups[2].Value)) throw new InvalidDataException($"duplicate directive {key}");
                    }
                }
                i++; continue;
            }
            var lineNumber = i + 1;
            var header = line;
            if (!header.StartsWith("query ", StringComparison.Ordinal) && !header.StartsWith("statement ", StringComparison.Ordinal))
                throw new InvalidDataException($"{file}:{lineNumber}: unsupported record {header}");
            i++;
            var sql = new List<string>();
            while (i < lines.Length && lines[i].Length > 0 && lines[i] != "----") sql.Add(lines[i++]);
            var expected = new List<string>();
            if (header.StartsWith("query ", StringComparison.Ordinal))
            {
                if (i >= lines.Length || lines[i] != "----") throw new InvalidDataException("query is missing ----");
                i++;
                while (i < lines.Length && lines[i].Length > 0) expected.Add(lines[i++]);
            }
            if (sql.Count == 0) throw new InvalidDataException("empty SQL record");
            records.Add(new(lineNumber, header, string.Join('\n', sql), expected.ToArray(), pending));
            pending = new(StringComparer.Ordinal);
        }
        if (pending.Count > 0) throw new InvalidDataException("dangling harness directives");
        if (records.Count == 0 && Fixture(file)?.ExpectedError is null) throw new InvalidDataException("empty corpus file");
        return records;
    }

    internal static async Task<int> Run(string file, bool fileBacked = false)
    {
        if (File.ReadLines(file).Any(line => line.Trim() == "# format: concurrency")) return await ConcurrencyCorpus.Run(file).ConfigureAwait(false);
        var backendRules = File.ReadLines(file).Where(line => line.StartsWith("# skip:", StringComparison.Ordinal)).Select(line => line[7..].Trim().Split(' ')[0]).ToArray();
        if (backendRules.Contains("memory")) fileBacked = true;
        else if (backendRules.Contains("disk")) fileBacked = false;
        var records = Parse(file);
        var zones = File.ReadLines(file).Select(line => Regex.Match(line, "^#\\s*load-timezone:\\s*(.*?)\\s*$", RegexOptions.CultureInvariant)).Where(match => match.Success).Select(match => match.Groups[1].Value).ToArray();
        TimeZoneData? timeZones = null;
        if (zones.Length != 0)
        {
            var directory = new DirectoryInfo(Path.GetDirectoryName(Path.GetFullPath(file))!);
            while (directory is not null && directory.Name != "spec") directory = directory.Parent;
            if (directory is null) throw new InvalidDataException("timezone corpus has no spec root");
            timeZones = TimeZoneData.Load(File.ReadAllBytes(Path.Combine(directory.FullName, "tz/fixtures/tzdata.jtz")));
            foreach (var zone in zones) _ = timeZones.Resolve(zone);
        }
        var fixture = Fixture(file);
        var collationNames = File.ReadLines(file).Select(line => Regex.Match(line, "^#\\s*load-collation:\\s*([^= ]+)", RegexOptions.CultureInvariant)).Where(match => match.Success).Select(match => match.Groups[1].Value).ToArray();
        UnicodeData? unicodeData = null;
        if (collationNames.Length != 0)
        {
            var directory = new DirectoryInfo(Path.GetDirectoryName(Path.GetFullPath(file))!);
            while (directory is not null && directory.Name != "spec") directory = directory.Parent;
            if (directory is null) throw new InvalidDataException("collation corpus has no spec root");
            unicodeData = UnicodeData.Load(File.ReadAllBytes(Path.Combine(directory.FullName, "collation/fixtures/unicode.jucd")));
            foreach (var name in collationNames) _ = unicodeData.Resolve(name);
        }
        var attachmentNames = File.ReadLines(file).Select(line => Regex.Match(line, "^#\\s*attach:\\s*(\\S+)\\s*$", RegexOptions.CultureInvariant)).Where(match => match.Success).Select(match => match.Groups[1].Value).ToArray();
        using var backing = fileBacked && fixture is null ? new CorpusFile() : null;
        await using var database = backing is not null ? await Database.CreateFileAsync(backing.Path, new() { Locking = FileLocking.None }).ConfigureAwait(false) : fixture is null ? attachmentNames.Length == 0 ? null : await Database.CreateInMemoryAsync().ConfigureAwait(false) : await OpenFixture(file, fixture).ConfigureAwait(false);
        if (database is not null) foreach (var attachment in attachmentNames) await database.AttachInMemoryAsync(attachment).ConfigureAwait(false);
        if (fixture?.ExpectedError is not null)
        {
            if (records.Count != 0) throw new InvalidDataException("fixture open-error case cannot contain SQL records");
            Console.WriteLine($"PASS {file}: 1 fixture open-error record"); return 1;
        }
        await using var session = database is null ? new Session(new SessionOptions { TimeZones = timeZones, UnicodeData = unicodeData }) : database.CreateSession(new SessionOptions { TimeZones = timeZones, UnicodeData = unicodeData });
        foreach (var record in records)
        {
            try { await RunRecord(session, record).ConfigureAwait(false); }
            catch (Exception error) { throw new InvalidDataException($"{file}:{record.Line}: {record.Sql}\n{error}", error); }
        }
        Console.WriteLine($"PASS {file}: {records.Count} records");
        return records.Count;
    }

    private sealed class CorpusFile : IDisposable
    {
        internal string Path { get; } = System.IO.Path.Combine(System.IO.Path.GetTempPath(), "jed-csharp-corpus-" + Guid.NewGuid().ToString("N"));
        public void Dispose() => File.Delete(Path);
    }

    private sealed record FixtureSpec(string RelativePath, string? ExpectedError);
    private static FixtureSpec? Fixture(string file)
    {
        FixtureSpec? result = null;
        foreach (var line in File.ReadLines(file))
        {
            var match = Regex.Match(line, "^#\\s*fixture:\\s*(.*?)\\s*$", RegexOptions.CultureInvariant);
            if (!match.Success) continue;
            if (result is not null) throw new InvalidDataException("duplicate fixture directive");
            var parts = match.Groups[1].Value.Split(' ', StringSplitOptions.RemoveEmptyEntries);
            if (parts.Length != 1 && (parts.Length != 3 || parts[1] != "error" || !Regex.IsMatch(parts[2], "^[0-9A-Z]{5}$", RegexOptions.CultureInvariant)))
                throw new InvalidDataException("invalid fixture directive");
            result = new(parts[0], parts.Length == 3 ? parts[2] : null);
        }
        return result;
    }
    private static async Task<Database?> OpenFixture(string file, FixtureSpec fixture)
    {
        var directory = new DirectoryInfo(Path.GetDirectoryName(Path.GetFullPath(file))!);
        while (directory is not null && directory.Name != "spec") directory = directory.Parent;
        if (directory is null) throw new InvalidDataException("fixture corpus has no spec root");
        var path = Path.GetFullPath(Path.Combine(directory.FullName, fixture.RelativePath));
        if (!path.StartsWith(directory.FullName + Path.DirectorySeparatorChar, StringComparison.Ordinal))
            throw new InvalidDataException("fixture path escapes spec root");
        var bytes = await File.ReadAllBytesAsync(path).ConfigureAwait(false);
        Database database;
        try { database = await Database.OpenStoreAsync(new MemoryBlockStore(bytes)).ConfigureAwait(false); }
        catch (SqlException error) when (fixture.ExpectedError is not null)
        { Equal(fixture.ExpectedError, error.SqlState, "fixture open SQLSTATE"); return null; }
        if (fixture.ExpectedError is not null)
        {
            await database.DisposeAsync().ConfigureAwait(false);
            throw new InvalidDataException("fixture open unexpectedly succeeded");
        }
        return database;
    }

    private static async ValueTask<SqlValue[]> ParseParameters(string text)
    {
        if (string.IsNullOrWhiteSpace(text)) return [];
        var syntax = new Parser("SELECT " + text).Parse();
        await using var session = new Session();
        var meter = new Meter(session, 0, CancellationToken.None);
        return syntax.Items.Select(item => Evaluate.Expr(Resolve.Expr(item.Expression,
            item.Expression is { Op: "array", Args.Length: 0 } ? SqlType.Array(SqlType.I64) : null), meter)).ToArray();
    }
    private static TablePrivileges ParsePrivileges(string text)
    {
        var result = TablePrivileges.None;
        foreach (var name in text.Split(',', StringSplitOptions.TrimEntries)) result |= name switch
        {
            "ALL" => TablePrivileges.All, "NONE" => TablePrivileges.None, "SELECT" => TablePrivileges.Select,
            "INSERT" => TablePrivileges.Insert, "UPDATE" => TablePrivileges.Update, "DELETE" => TablePrivileges.Delete,
            _ => throw new InvalidDataException("invalid table privilege: " + name)
        };
        return result;
    }
    internal static async Task RunRecord(Session session, CorpusRecord record)
    {
        session.ResetPrivileges(); session.ResetVariables();
        await session.SetWorkMemAsync(record.Directives.TryGetValue("work_mem", out var workMem) ? long.Parse(workMem, CultureInfo.InvariantCulture) : Database.DefaultWorkMem).ConfigureAwait(false);
        if (record.Directives.TryGetValue("max_storage_bytes", out var storageLimits)) foreach (var directive in storageLimits.Split('\n'))
        {
            var pieces = directive.Split(' ', StringSplitOptions.RemoveEmptyEntries);
            if (pieces.Length is < 1 or > 2) throw new InvalidDataException("invalid storage limit action");
            await session.SetDatabaseStorageLimitAsync(long.Parse(pieces[0], CultureInfo.InvariantCulture), pieces.Length == 2 ? pieces[1] : "main").ConfigureAwait(false);
        }
        if (record.Directives.TryGetValue("default_privileges", out var defaults)) session.SetDefaultPrivileges(ParsePrivileges(defaults));
        foreach (var key in new[] { "grant", "revoke" })
            if (record.Directives.TryGetValue(key, out var rules)) foreach (var rule in rules.Split('\n'))
            {
                var parts = rule.Split(" ON ", 2, StringSplitOptions.None);
                if (parts.Length != 2) throw new InvalidDataException("invalid privilege directive");
                if (parts[0].Trim() == "EXECUTE") { if (key == "grant") session.GrantExecute(parts[1].Trim()); else session.RevokeExecute(parts[1].Trim()); }
                else if (key == "grant") session.Grant(parts[1].Trim(), ParsePrivileges(parts[0]));
                else session.Revoke(parts[1].Trim(), ParsePrivileges(parts[0]));
            }
        if (record.Directives.TryGetValue("set", out var settings)) foreach (var setting in settings.Split(','))
        {
            var pair = setting.Split('=', 2);
            if (pair.Length != 2) throw new InvalidDataException("invalid variable directive");
            session.SetVar(pair[0].Trim(), pair[1].Trim());
        }
        session.SetAllowDdl(!record.Directives.TryGetValue("allow_ddl", out var ddl) || ddl == "on");
        session.SetAllowTempDdl(record.Directives.TryGetValue("allow_temp_ddl", out var tempDdl) ? tempDdl == "on" : ddl != "off");
        session.SetTempStorageLimit(record.Directives.TryGetValue("max_temp_storage_bytes", out var tempLimit) ? long.Parse(tempLimit, CultureInfo.InvariantCulture) : 1024L * 1024 * 1024);
        if (record.Directives.TryGetValue("upgrade-collations", out var upgrade))
        {
            if (upgrade.Length != 0) throw new InvalidDataException("upgrade-collations takes no value");
            await session.UpgradeCollationsAsync().ConfigureAwait(false);
        }
        if (record.Directives.TryGetValue("lifetime_max_cost", out var lifetime)) session.SetLifetimeCostLimit(long.Parse(lifetime, CultureInfo.InvariantCulture));
        var fields = record.Header.Split(' ', StringSplitOptions.RemoveEmptyEntries);
        var query = fields[0] == "query";
        if (query && fields.Length == 2) fields = [.. fields, "nosort"];
        if (query && (fields.Length != 3 || fields[1].Any(c => !"IBTDR".Contains(c))))
            throw new InvalidDataException("unsupported query header or render tag");
        if (!query && !(fields is ["statement", "ok"] || fields is ["statement", "error", _, ..]))
            throw new InvalidDataException("invalid statement header");
        if (!query && (record.Directives.ContainsKey("names") || record.Directives.ContainsKey("types")))
            throw new InvalidDataException("names/types directives require a query record");
        var expectedError = !query && fields[1] == "error" ? fields[2] : null;
        var cells = new List<string>();
        Rows? rows = null;
        ISqlError? failure = null;
        try
        {
            var ceiling = record.Directives.TryGetValue("max_cost", out var max) ? long.Parse(max, CultureInfo.InvariantCulture) : 0;
            Func<long>? clock = null;
            if (record.Directives.TryGetValue("clock", out var fixedClock))
            {
                var micros = long.Parse(fixedClock, CultureInfo.InvariantCulture); clock = () => micros;
            }
            if (record.Directives.TryGetValue("clock_advance", out var advancing))
            {
                if (clock is not null) throw new InvalidDataException("conflicting clock directives");
                var parts = advancing.Split(',');
                if (parts.Length != 2) throw new InvalidDataException("invalid advancing clock");
                var micros = long.Parse(parts[0], CultureInfo.InvariantCulture); var step = long.Parse(parts[1], CultureInfo.InvariantCulture);
                clock = () => { var value = micros; micros = checked(micros + step); return value; };
            }
            var options = new QueryOptions
            {
                MaxCost = ceiling, Clock = clock,
                RandomSource = record.Directives.TryGetValue("seed", out var seed) ? new SeededRandomSource(ulong.Parse(seed, CultureInfo.InvariantCulture)).Fill : null,
                Parameters = record.Directives.TryGetValue("params", out var parameters) ? await ParseParameters(parameters).ConfigureAwait(false) : [],
                TimeZone = record.Directives.GetValueOrDefault("timezone"),
                MaxQueryMemoryBytes = record.Directives.TryGetValue("max_query_memory_bytes", out var queryMemory) ? long.Parse(queryMemory, CultureInfo.InvariantCulture) : null,
                MaxScalarBytes = record.Directives.TryGetValue("max_scalar_bytes", out var scalar) ? long.Parse(scalar, CultureInfo.InvariantCulture) : 0,
                MaxSqlLength = record.Directives.TryGetValue("max_sql_length", out var input) ? long.Parse(input, CultureInfo.InvariantCulture) : null
            };
            rows = await session.QueryAsync(record.Sql, options).ConfigureAwait(false);
            if (query)
            {
                // Shared runners use coltypes only to group/render flat cells.
                // Explicit names/types directives assert result metadata; legacy
                // homogeneous records may carry one tag for several columns.
                if (record.Directives.TryGetValue("types", out var types))
                    EqualList(Split(types), rows.Columns.Select(c => c.Type.Name()).ToArray(), "types");
                if (record.Directives.TryGetValue("names", out var names))
                    EqualList(Split(names), rows.Columns.Select(c => c.Name).ToArray(), "names");
            }
            // Drain through the public async API. Execution errors and final
            // costs can occur here; QueryAsync success alone is not a pass.
            var renderContext = session.NewTemporalContext(options);
            await foreach (var row in ((IAsyncEnumerable<Row>)rows).ConfigureAwait(false))
                for (var i = 0; i < row.Count; i++) cells.Add(TemporalRender.Value(row[i], renderContext));
        }
        catch (Exception error) when (error is ISqlError sqlError) { failure = sqlError; }
        finally { if (rows is not null) await rows.DisposeAsync().ConfigureAwait(false); }

        if (expectedError is not null) Equal(expectedError, failure?.SqlState ?? "success", "SQLSTATE");
        else if (failure is not null) throw new InvalidDataException($"unexpected SQLSTATE {failure.SqlState}: {failure}");
        if (record.Directives.TryGetValue("cost", out var cost))
            Equal(long.Parse(cost, CultureInfo.InvariantCulture), rows?.Cost ?? failure?.Cost ?? 0, "cost");
        if (query && failure is null)
        {
            var actual = cells.ToArray(); var expected = record.Expected.ToArray();
            switch (fields[2])
            {
                case "nosort": break;
                case "valuesort": Array.Sort(actual, StringComparer.Ordinal); Array.Sort(expected, StringComparer.Ordinal); break;
                case "rowsort":
                    actual = SortRows(actual, fields[1].Length); expected = SortRows(expected, fields[1].Length); break;
                default: throw new InvalidDataException("unsupported sort mode");
            }
            Equal(expected.Length, actual.Length, "value count");
            for (var i = 0; i < expected.Length; i++)
                if (expected[i] != actual[i] && !(fields[1][i % fields[1].Length] == 'R' && RealEqual(expected[i], actual[i])))
                    throw new InvalidDataException($"values: cell {i}: expected {expected[i]}, got {actual[i]}");
        }
    }
    private static bool RealEqual(string expected, string actual)
    {
        if (!double.TryParse(expected, NumberStyles.Float, CultureInfo.InvariantCulture, out var first) ||
            !double.TryParse(actual, NumberStyles.Float, CultureInfo.InvariantCulture, out var second)) return false;
        if (double.IsNaN(first)) return double.IsNaN(second);
        if (double.IsNaN(second)) return false;
        if (first == second) return true;
        if (!double.IsFinite(first) || !double.IsFinite(second)) return false;
        // The existing shared corpus runners use this relative/absolute bound.
        return Math.Abs(first - second) <= 1e-9 * Math.Max(1, Math.Max(Math.Abs(first), Math.Abs(second)));
    }
    private static string[] SortRows(string[] values, int width)
    {
        if (width == 0 || values.Length % width != 0) throw new InvalidDataException("incomplete row");
        var rows = values.Chunk(width).ToArray();
        Array.Sort(rows, (a, b) =>
        {
            for (var i = 0; i < width; i++) { var order = string.CompareOrdinal(a[i], b[i]); if (order != 0) return order; }
            return 0;
        });
        return rows.SelectMany(row => row).ToArray();
    }
    private static string[] Split(string values) => values.Split(',').Select(s => s.Trim()).ToArray();
    internal static void Equal<T>(T expected, T actual, string what)
    {
        if (!EqualityComparer<T>.Default.Equals(expected, actual)) throw new InvalidDataException($"{what}: expected {expected}, got {actual}");
    }
    internal static void EqualList(string[] expected, string[] actual, string what)
    {
        if (!expected.SequenceEqual(actual)) throw new InvalidDataException($"{what}: expected [{string.Join(", ", expected)}], got [{string.Join(", ", actual)}]");
    }
}
