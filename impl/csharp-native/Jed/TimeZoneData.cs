using System.Buffers.Binary;
using System.Globalization;

namespace Jed;

/// <summary>Immutable host-loaded JTZ data. SQL can only consult zones already loaded here.</summary>
public sealed class TimeZoneData
{
    private readonly byte[]? bundle;
    private readonly Dictionary<string, TimeZoneRule> zones;
    private readonly Dictionary<string, string> links;
    public string Version { get; }
    public IReadOnlyList<string> ZoneNames { get; }
    public IReadOnlyDictionary<string, string> Links { get; }
    private TimeZoneData(byte[]? bundle, string version, Dictionary<string, TimeZoneRule> zones, Dictionary<string, string> links)
    {
        this.bundle = bundle; Version = version; this.zones = zones; this.links = links;
        ZoneNames = Array.AsReadOnly(zones.Keys.Order(StringComparer.Ordinal).ToArray());
        Links = new System.Collections.ObjectModel.ReadOnlyDictionary<string, string>(links);
    }
    public byte[] Save() => bundle?.ToArray() ?? throw new InvalidOperationException("an additive time-zone environment has no single source bundle");
    public TimeZoneData Add(TimeZoneData other)
    {
        ArgumentNullException.ThrowIfNull(other);
        var mergedZones = new Dictionary<string, TimeZoneRule>(zones, StringComparer.Ordinal);
        var mergedLinks = new Dictionary<string, string>(links, StringComparer.Ordinal);
        foreach (var (name, zone) in other.zones) if (!mergedZones.ContainsKey(name) && !mergedLinks.ContainsKey(name)) mergedZones.Add(name, zone);
        foreach (var (name, target) in other.links) if (!mergedZones.ContainsKey(name) && !mergedLinks.ContainsKey(name)) mergedLinks.Add(name, target);
        return new(null, Version, mergedZones, mergedLinks);
    }
    public static TimeZoneData Load(ReadOnlySpan<byte> bytes)
    {
        try { return Read(bytes); }
        catch (SqlException error) when (error.SqlState == SqlStates.DataCorrupted) { throw TimeZoneRule.Bad("malformed JTZ bundle: " + error.Message); }
        catch (OverflowException) { throw TimeZoneRule.Bad("JTZ length overflow"); }
    }
    private static TimeZoneData Read(ReadOnlySpan<byte> bytes)
    {
        if (bytes.Length < 12 || !bytes[..6].SequenceEqual("JTZ\0\0\0"u8) || BinaryPrimitives.ReadUInt16BigEndian(bytes[6..]) != 1) throw TimeZoneRule.Bad("invalid JTZ header");
        if (StorageFormat.Crc32(bytes[..^4]) != BinaryPrimitives.ReadUInt32BigEndian(bytes[^4..])) throw TimeZoneRule.Bad("JTZ checksum mismatch");
        var reader = new WireReader(bytes[8..^4]); var version = reader.String(); _ = reader.String();
        var zoneCount = reader.U16(); var linkCount = reader.U16();
        var manifest = new (string Name, uint Hash, uint Raw, uint Compressed, uint Offset)[zoneCount];
        string? previous = null;
        for (var i = 0; i < manifest.Length; i++)
        {
            var name = reader.String();
            if (name.Length == 0 || previous is not null && StringComparer.Ordinal.Compare(previous, name) >= 0) throw TimeZoneRule.Bad("unordered JTZ zones");
            manifest[i] = (name, reader.U32(), reader.U32(), reader.U32(), reader.U32()); previous = name;
        }
        var links = new Dictionary<string, string>(StringComparer.Ordinal); previous = null;
        for (var i = 0; i < linkCount; i++)
        {
            var alias = reader.String(); var target = reader.String();
            if (alias.Length == 0 || previous is not null && StringComparer.Ordinal.Compare(previous, alias) >= 0 || manifest.Any(section => section.Name == alias)) throw TimeZoneRule.Bad("invalid JTZ alias ordering");
            if (!manifest.Any(section => section.Name == target)) throw TimeZoneRule.Bad("dangling JTZ alias");
            links.Add(alias, target); previous = alias;
        }
        var bodyStart = bytes.Length - 4 - reader.Remaining;
        var expected = bodyStart; var zones = new Dictionary<string, TimeZoneRule>(StringComparer.Ordinal);
        foreach (var section in manifest)
        {
            if (section.Offset != expected || section.Compressed > bytes.Length - 4 - expected || section.Raw > 16 * 1024 * 1024) throw TimeZoneRule.Bad("invalid JTZ section bounds");
            var payload = Lz4(bytes.Slice(expected, (int)section.Compressed), (int)section.Raw); expected += (int)section.Compressed;
            if (StorageFormat.Crc32(payload) != section.Hash) throw TimeZoneRule.Bad("JTZ section checksum mismatch");
            zones.Add(section.Name, new(section.Name, version, section.Hash, payload));
        }
        if (expected != bytes.Length - 4) throw TimeZoneRule.Bad("trailing JTZ body bytes");
        return new(bytes.ToArray(), version, zones, links);
    }
    // Standard LZ4 block decoding, bounded by the declared destination and source spans.
    private static byte[] Lz4(ReadOnlySpan<byte> source, int rawLength)
    {
        var output = new byte[rawLength]; var input = 0; var position = 0;
        while (input < source.Length)
        {
            var token = source[input++]; var literals = (int)(token >> 4);
            if (literals == 15) Extend(source, ref input, ref literals);
            if (literals > source.Length - input || literals > output.Length - position) throw TimeZoneRule.Bad("invalid LZ4 literals");
            source.Slice(input, literals).CopyTo(output.AsSpan(position)); input += literals; position += literals;
            if (input == source.Length) break;
            if (input + 2 > source.Length) throw TimeZoneRule.Bad("truncated LZ4 offset");
            var offset = BinaryPrimitives.ReadUInt16LittleEndian(source[input..]); input += 2;
            if (offset == 0 || offset > position) throw TimeZoneRule.Bad("invalid LZ4 match offset");
            var length = (int)(token & 15); if (length == 15) Extend(source, ref input, ref length); length += 4;
            if (length > output.Length - position) throw TimeZoneRule.Bad("oversize LZ4 match");
            for (var i = 0; i < length; i++) output[position + i] = output[position + i - offset]; position += length;
        }
        if (position != rawLength) throw TimeZoneRule.Bad("LZ4 output length mismatch");
        return output;
    }
    private static void Extend(ReadOnlySpan<byte> source, ref int position, ref int length)
    {
        byte next;
        do { if (position == source.Length) throw TimeZoneRule.Bad("truncated LZ4 length"); next = source[position++]; length = checked(length + next); } while (next == 255);
    }
    internal TimeZoneRule LoadedRule(string name) => zones.TryGetValue(name, out var rule) ? rule : links.TryGetValue(name, out var target) ? zones[target] : throw TimeZoneRule.Bad("time zone is not loaded: " + name);
    public TimeZoneRule Resolve(string name) => Resolve(name, this);
    internal static TimeZoneRule Resolve(string name, TimeZoneData? data)
    {
        var folded = TextEncoding.FoldName(name);
        if (folded == "utc") return new("UTC", 0);
        if (Fixed(name, out var offset)) return new(name, offset);
        if (data is not null)
        {
            if (data.zones.TryGetValue(name, out var exact)) return exact;
            if (data.links.TryGetValue(name, out var target)) return data.zones[target];
            foreach (var candidate in data.zones.Keys.Concat(data.links.Keys).Order(StringComparer.Ordinal))
                if (TextEncoding.FoldName(candidate) == folded) return data.zones.TryGetValue(candidate, out var zone) ? zone : data.zones[data.links[candidate]];
        }
        throw TimeZoneRule.Bad($"time zone is not loaded: {name}");
    }
    private static bool Fixed(string name, out int offset)
    {
        offset = 0; if (name.Length < 2 || name[0] is not ('+' or '-')) return false;
        var parts = name[1..].Split(':'); if (parts.Length > 3) return false;
        var values = new int[3];
        for (var i = 0; i < parts.Length; i++)
        {
            if (parts[i].Length == 0 || !int.TryParse(parts[i], NumberStyles.None, CultureInfo.InvariantCulture, out values[i]) || values[i] > (i == 0 ? 167 : i == 1 ? 59 : 60)) return false;
        }
        offset = (name[0] == '-' ? 1 : -1) * (values[0] * 3600 + values[1] * 60 + values[2]); return true;
    }
}
