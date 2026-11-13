using System.Globalization;
using System.Text.Json;
using Jed;

internal static class PageWireTests
{
    internal static async Task Run(string root)
    {
        var sections = Read(Path.Combine(root, "spec/pageservice/fixtures.toml"));
        var frames = 0; var owners = 0; var malformed = 0;
        foreach (var (section, values) in sections)
        {
            var bytes = Convert.FromHexString(values["hex"]); var name = values["name"];
            if (section == "owner_record")
            {
                var expected = new PageOwnerRecord(values["f_flags"] == "1", uint.Parse(values["f_uid"], CultureInfo.InvariantCulture),
                    Convert.FromHexString(values["f_incarnation"]), uint.Parse(values["f_pid"], CultureInfo.InvariantCulture), values["f_endpoint"]);
                Equal(bytes, PageWire.EncodeOwner(expected), name + " owner encode");
                var decoded = PageWire.DecodeOwner(bytes) ?? throw new InvalidDataException("owner did not decode");
                Corpus.Equal(expected.Serving, decoded.Serving, name); Corpus.Equal(expected.Uid, decoded.Uid, name);
                Corpus.Equal(expected.Pid, decoded.Pid, name); Corpus.Equal(expected.Endpoint, decoded.Endpoint, name);
                Equal(expected.Incarnation, decoded.Incarnation, name);
                for (var i = 0; i < bytes.Length; i++)
                {
                    bytes[i] ^= 1; Corpus.Equal<PageOwnerRecord?>(null, PageWire.DecodeOwner(bytes), "owner CRC rejects corruption"); bytes[i] ^= 1;
                }
                owners++; continue;
            }
            var pageSize = int.Parse(values["page_size"], CultureInfo.InvariantCulture);
            if (section == "malformed")
            {
                Reject(() => PageWire.Decode(bytes, pageSize), name); malformed++; continue;
            }
            var definition = PageWire.Definitions.Single(d => d.Name == values["message"]);
            var expectedFields = new Dictionary<string, object>(StringComparer.Ordinal);
            foreach (var field in definition.Fields.Split(' ', StringSplitOptions.RemoveEmptyEntries))
            {
                var parts = field.Split(':'); expectedFields.Add(parts[0], Value(parts[1], values["f_" + parts[0]], pageSize));
            }
            var message = new PageWireMessage(definition.Code, uint.Parse(values["request_id"], CultureInfo.InvariantCulture), expectedFields);
            Equal(bytes, PageWire.Encode(message, pageSize), name + " encode");
            var actual = PageWire.Decode(bytes, pageSize);
            Corpus.Equal(message.Code, actual.Code, name); Corpus.Equal(message.RequestId, actual.RequestId, name);
            // Canonical re-encoding checks every decoded field, including lengths,
            // ownership copies, ordered lists and embedded pages.
            Equal(bytes, PageWire.Encode(actual, pageSize), name + " decode");
            using var fragmented = new FragmentedStream(bytes);
            var asynchronous = await PageWire.ReadAsync(fragmented, pageSize) ?? throw new InvalidDataException("missing frame");
            Equal(bytes, PageWire.Encode(asynchronous, pageSize), name + " fragmented read");
            Corpus.Equal<PageWireMessage?>(null, await PageWire.ReadAsync(fragmented, pageSize), "clean stream EOF");
            frames++;
        }
        if (frames != 27 || owners != 2 || malformed != 14) throw new InvalidDataException($"changed wire fixture coverage: {frames}/{owners}/{malformed}");
        using (var hostile = new FragmentedStream(Convert.FromHexString("0100000100000000")))
        {
            try { _ = await PageWire.ReadAsync(hostile); }
            catch (SqlException error) when (error.SqlState == SqlStates.ProtocolViolation)
            {
                Corpus.Equal(4L, hostile.Position, "reject hostile size before body read"); goto Rejected;
            }
            throw new InvalidDataException("accepted hostile stream size");
        }
        Rejected:
        Console.WriteLine($"PASS page-service codec: {frames} shared frames, {owners} owner records, {malformed} malformed frames and bounded fragmented I/O");
    }
    private static object Value(string kind, string value, int pageSize) => kind switch
    {
        "u8" => byte.Parse(value, CultureInfo.InvariantCulture), "u16" => ushort.Parse(value, CultureInfo.InvariantCulture),
        "u32" => uint.Parse(value, CultureInfo.InvariantCulture), "u64" => ulong.Parse(value, CultureInfo.InvariantCulture),
        "bool" => bool.Parse(value), "string" or "sqlstate" => value, "id16" => Convert.FromHexString(value),
        "page" => Page(value, pageSize),
        "u32_list" => Split(value, ',').Select(v => uint.Parse(v, CultureInfo.InvariantCulture)).ToArray(),
        "digest_list" => Split(value, ',').Select(v => { var pair = v.Split('='); return new PageWireDigest(uint.Parse(pair[0], CultureInfo.InvariantCulture), ulong.Parse(pair[1], CultureInfo.InvariantCulture)); }).ToArray(),
        "page_list" => Split(value, ';').Select(v => Page(v, pageSize)).ToArray(),
        "indexed_page_list" => Split(value, ';').Select(v => { var pair = v.Split('='); return new PageWirePage(uint.Parse(pair[0], CultureInfo.InvariantCulture), Page(pair[1], pageSize)); }).ToArray(),
        "credential" => value.Length == 0 ? new PageWireCredential(0, []) : value.StartsWith("key:", StringComparison.Ordinal) ?
            new PageWireCredential(2, Convert.FromHexString(value[4..])) : new PageWireCredential(1, PageWire.Utf8.GetBytes(value[11..])),
        _ => throw new InvalidDataException("unknown fixture field")
    };
    private static string[] Split(string value, char delimiter) => value.Length == 0 ? [] : value.Split(delimiter);
    private static byte[] Page(string value, int size)
    {
        var pair = value.Split('/'); var page = new byte[size]; Array.Fill(page, Convert.ToByte(pair[1], 16)); Convert.FromHexString(pair[0]).CopyTo(page, 0); return page;
    }
    private static void Equal(byte[] expected, byte[] actual, string name) => Corpus.Equal(Convert.ToHexString(expected), Convert.ToHexString(actual), name);
    private static void Reject(Action action, string name)
    {
        try { action(); } catch (SqlException error) when (error.SqlState == SqlStates.ProtocolViolation) { return; }
        throw new InvalidDataException("accepted malformed frame: " + name);
    }
    private static List<(string Section, Dictionary<string, string> Values)> Read(string path)
    {
        var sections = new List<(string, Dictionary<string, string>)>(); Dictionary<string, string>? current = null;
        foreach (var raw in File.ReadLines(path))
        {
            var line = raw.Trim(); if (line.Length == 0 || line.StartsWith('#') || line == "schema_version = 1") continue;
            if (line.StartsWith("[[", StringComparison.Ordinal)) { current = new(StringComparer.Ordinal); sections.Add((line[2..^2], current)); continue; }
            var pair = line.Split(" = ", 2); if (pair.Length != 2 || current is null) throw new InvalidDataException("invalid wire fixture line");
            using var document = JsonDocument.Parse(pair[1]); current.Add(pair[0], document.RootElement.GetString()!);
        }
        return sections;
    }
    private sealed class FragmentedStream(byte[] bytes) : MemoryStream(bytes, writable: false)
    {
        public override ValueTask<int> ReadAsync(Memory<byte> buffer, CancellationToken cancellationToken = default) =>
            base.ReadAsync(buffer[..Math.Min(buffer.Length, 1)], cancellationToken);
    }
}
