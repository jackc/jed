using System.Globalization;

internal sealed record SimulationStep(int Index, int Line, string Operation, string[] Arguments, string? Sql = null);

internal sealed record SimulationTrace(string Name, ulong Seed, int PageSize, long CacheBytes, long Clock,
    int Sector, string Synchronous, long FlushIntervalMilliseconds, long WorkMemory, long TempBuffers,
    long MaxQueryMemory, long MaxStorage, bool Encrypted, SimulationStep[] Steps)
{
    internal static SimulationTrace Read(string path)
    {
        var settings = new Dictionary<string, string>(StringComparer.Ordinal);
        var steps = new List<SimulationStep>();
        var lines = File.ReadAllLines(path);
        for (var line = 0; line < lines.Length; line++)
        {
            var text = lines[line].Trim();
            if (text.Length == 0) continue;
            if (text.StartsWith('#'))
            {
                var colon = text.IndexOf(':');
                if (colon > 1)
                {
                    var key = text[1..colon].Trim();
                    if (key is "format" or "profile" or "seed" or "page_size" or "cache_bytes" or "clock" or "sector" or "synchronous" or
                        "flush_interval_ms" or "work_mem" or "temp_buffers" or "max_query_memory_bytes" or "max_storage_bytes" or "encrypted")
                    {
                        if (!settings.TryAdd(key, text[(colon + 1)..].Trim())) throw Invalid(line, "duplicate header " + key);
                    }
                }
                continue;
            }
            var fields = text.Split(' ', StringSplitOptions.RemoveEmptyEntries);
            if (fields[0] == "on")
            {
                if (fields.Length < 3) throw Invalid(line, "missing session or SQL");
                var first = text.IndexOf(' '); var second = text.IndexOf(' ', first + 1);
                while (second >= 0 && text[(first + 1)..second].Trim().Length == 0) { first = second; second = text.IndexOf(' ', first + 1); }
                if (second < 0) throw Invalid(line, "missing SQL");
                steps.Add(new(steps.Count, line + 1, "on", [fields[1]], text[(second + 1)..].TrimStart()));
                continue;
            }
            var minimum = fields[0] switch
            {
                "open" => 3, "commit" or "rollback" or "close" or "clock" or "attach" or "restore" or "replay" or "spawn" or "exit" or "kill" or "disconnect" => 2,
                "fault" or "crash" or "backup" or "change_key" or "rekey" => 2,
                "reopen" or "compact" or "check" or "corrupt" => 1,
                _ => throw Invalid(line, "unknown operation " + fields[0])
            };
            if (fields.Length < minimum) throw Invalid(line, "missing arguments for " + fields[0]);
            steps.Add(new(steps.Count, line + 1, fields[0], fields[1..]));
        }
        if (settings.GetValueOrDefault("format") != "simulation") throw Invalid(0, "missing simulation format header");
        long Number(string key, long fallback) => settings.TryGetValue(key, out var value) ? long.Parse(value, CultureInfo.InvariantCulture) : fallback;
        var pageSize = checked((int)Number("page_size", 256)); var sector = checked((int)Number("sector", 64));
        if (!Jed.StorageFormat.ValidPageSize(pageSize) || sector <= 0 || sector > pageSize || pageSize % sector != 0) throw Invalid(0, "invalid page or sector size");
        var synchronous = settings.GetValueOrDefault("synchronous", "on");
        if (synchronous is not ("on" or "off")) throw Invalid(0, "invalid synchronous setting");
        return new(Path.GetFileName(path), settings.TryGetValue("seed", out var seed) ? ulong.Parse(seed, CultureInfo.InvariantCulture) : 1,
            pageSize, Number("cache_bytes", 0), Number("clock", 1700000000000000), sector, synchronous,
            Number("flush_interval_ms", 0), Number("work_mem", 0), Number("temp_buffers", 0), Number("max_query_memory_bytes", 0),
            Number("max_storage_bytes", 0), Number("encrypted", 0) == 1, steps.ToArray());

        InvalidDataException Invalid(int line, string message) => new($"{path}:{line + 1}: {message}");
    }
}
