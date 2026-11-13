using System.Buffers.Binary;
using System.Collections.ObjectModel;
using System.Text;

namespace Jed;

/// <summary>Validated immutable JUCD data supplied as bytes by the host.</summary>
public sealed class UnicodeData
{
    private readonly Dictionary<string, Collation> collations;
    private readonly CaseEntry[] simple, special;
    private readonly byte[]? original;
    private sealed record CaseEntry(int Point, int[] Upper, int[] Lower, int[] Title);
    public string UnicodeVersion { get; }
    public string CldrVersion { get; }
    public IReadOnlyList<string> CollationNames { get; }
    public bool HasCasing { get; }
    private UnicodeData(string unicode, string cldr, Dictionary<string, Collation> collations, CaseEntry[] simple, CaseEntry[] special, bool casing, byte[]? original)
    {
        UnicodeVersion = unicode; CldrVersion = cldr; this.collations = collations; this.simple = simple; this.special = special; HasCasing = casing; this.original = original;
        CollationNames = Array.AsReadOnly(collations.Keys.Order(StringComparer.Ordinal).ToArray());
    }
    public Collation Resolve(string name) => collations.TryGetValue(name, out var collation) ? collation : throw new SqlException("42704", "collation does not exist: " + name);
    public byte[] Save() => original?.ToArray() ?? throw new InvalidOperationException("an additive Unicode environment has no single source bundle");
    public UnicodeData Add(UnicodeData other)
    {
        ArgumentNullException.ThrowIfNull(other);
        if (UnicodeVersion != other.UnicodeVersion || CldrVersion.Length != 0 && other.CldrVersion.Length != 0 && CldrVersion != other.CldrVersion) throw Collation.Bad("incompatible Unicode data versions");
        var combined = new Dictionary<string, Collation>(collations, StringComparer.Ordinal);
        foreach (var pair in other.collations)
        {
            if (combined.TryGetValue(pair.Key, out var present) && present.ContentHash != pair.Value.ContentHash) throw Collation.Bad("conflicting collation table");
            combined[pair.Key] = pair.Value;
        }
        return new(UnicodeVersion, CldrVersion.Length == 0 ? other.CldrVersion : CldrVersion, combined, other.HasCasing ? other.simple : simple, other.HasCasing ? other.special : special, HasCasing || other.HasCasing, null);
    }
    public static UnicodeData Load(ReadOnlySpan<byte> bytes)
    {
        try { return Read(bytes); }
        catch (SqlException error) when (error.SqlState == "XX001") { throw Collation.Bad("malformed JUCD bundle: " + error.Message); }
        catch (OverflowException) { throw Collation.Bad("JUCD length overflow"); }
    }
    private static UnicodeData Read(ReadOnlySpan<byte> bytes)
    {
        if (bytes.Length < 12 || bytes.Length > 64 * 1024 * 1024 || !bytes[..6].SequenceEqual("JUCD\0\0"u8) || BinaryPrimitives.ReadUInt16BigEndian(bytes[6..]) != 1) throw Collation.Bad("invalid JUCD header");
        if (StorageFormat.Crc32(bytes[..^4]) != BinaryPrimitives.ReadUInt32BigEndian(bytes[^4..])) throw Collation.Bad("JUCD checksum mismatch");
        var reader = new WireReader(bytes[8..^4]); var unicode = reader.String(); var cldr = reader.String(); var description = reader.String();
        var manifest = new (byte Kind, string Name, uint Hash, uint Raw, uint Compressed, uint Offset)[reader.U16()];
        var names = new HashSet<string>(StringComparer.Ordinal); var rootCount = 0; var propertyCount = 0;
        for (var i = 0; i < manifest.Length; i++)
        {
            var kind = reader.Byte(); var name = reader.String();
            if (kind > 2 || kind == 0 && (name.Length != 0 || ++propertyCount > 1) || kind == 1 && ++rootCount > 1) throw Collation.Bad("invalid JUCD section kind");
            if (kind != 0) { Collation.ValidateName(name); if (!names.Add(name)) throw Collation.Bad("duplicate collation name"); }
            manifest[i] = (kind, name, reader.U32(), reader.U32(), reader.U32(), reader.U32());
        }
        var payloads = new byte[manifest.Length][]; var expected = bytes.Length - 4 - reader.Remaining;
        for (var i = 0; i < manifest.Length; i++)
        {
            var section = manifest[i];
            if (section.Offset != expected || section.Raw > 16 * 1024 * 1024 || section.Compressed > bytes.Length - 4 - expected) throw Collation.Bad("invalid JUCD section bounds");
            payloads[i] = StorageLz4.Decompress(bytes.Slice(expected, (int)section.Compressed), (int)section.Raw); expected += (int)section.Compressed;
            if (StorageFormat.Crc32(payloads[i]) != section.Hash) throw Collation.Bad("JUCD section checksum mismatch");
        }
        if (expected != bytes.Length - 4) throw Collation.Bad("trailing JUCD bytes");
        var tables = new Dictionary<string, Collation>(StringComparer.Ordinal); Collation? root = null; CaseEntry[] simple = [], special = [];
        for (var i = 0; i < manifest.Length; i++)
        {
            var section = manifest[i];
            if (section.Kind == 0) (simple, special) = ReadProperties(payloads[i]);
            else if (section.Kind == 1)
            {
                var mappings = Collation.ReadTable(payloads[i], false); root = new(section.Name, unicode, cldr, description, mappings.Singles, mappings.Contractions); tables.Add(section.Name, root);
            }
        }
        for (var i = 0; i < manifest.Length; i++) if (manifest[i].Kind == 2)
        {
            if (root is null) throw Collation.Bad("tailoring without a root table");
            tables.Add(manifest[i].Name, root.Tailor(manifest[i].Name, payloads[i]));
        }
        return new(unicode, cldr, tables, simple, special, propertyCount != 0, bytes.ToArray());
    }
    private static (CaseEntry[] Simple, CaseEntry[] Special) ReadProperties(ReadOnlySpan<byte> bytes)
    {
        var reader = new WireReader(bytes); if (reader.Byte() != 1) throw Collation.Bad("unsupported property layout");
        var count = reader.U32(); if (count > reader.Remaining / 16) throw Collation.Bad("invalid simple case count");
        var simple = new CaseEntry[(int)count];
        for (var i = 0; i < simple.Length; i++)
        {
            var point = Collation.Point(reader.U32()); if (i != 0 && point <= simple[i - 1].Point) throw Collation.Bad("unordered simple case data");
            simple[i] = new(point, [Collation.Point(reader.U32())], [Collation.Point(reader.U32())], [Collation.Point(reader.U32())]);
        }
        count = reader.U32(); if (count > reader.Remaining / 7) throw Collation.Bad("invalid special case count");
        var special = new CaseEntry[(int)count];
        for (var i = 0; i < special.Length; i++)
        {
            var point = Collation.Point(reader.U32()); if (i != 0 && point <= special[i - 1].Point) throw Collation.Bad("unordered special case data");
            special[i] = new(point, CasePoints(ref reader), CasePoints(ref reader), CasePoints(ref reader));
        }
        reader.Finish(); return (simple, special);
    }
    private static int[] CasePoints(ref WireReader reader)
    {
        var points = new int[reader.Byte()]; for (var i = 0; i < points.Length; i++) points[i] = Collation.Point(reader.U32()); return points;
    }
    private static CaseEntry? Lookup(CaseEntry[] entries, int point)
    {
        var low = 0; var high = entries.Length;
        while (low < high) { var middle = low + (high - low) / 2; if (entries[middle].Point < point) low = middle + 1; else high = middle; }
        return low < entries.Length && entries[low].Point == point ? entries[low] : null;
    }
    internal static string Fold(string text, bool upper, UnicodeData? data, bool full = true)
    {
        var output = new StringBuilder(text.Length);
        foreach (var rune in text.EnumerateRunes())
        {
            var point = rune.Value;
            var mapping = data?.HasCasing == true ? (full ? Lookup(data.special, point) : null) ?? Lookup(data.simple, point) : null;
            if (mapping is not null) foreach (var changed in upper ? mapping.Upper : mapping.Lower) output.Append(new Rune(changed));
            else output.Append(new Rune(upper && point is >= 'a' and <= 'z' ? point - 32 : !upper && point is >= 'A' and <= 'Z' ? point + 32 : point));
        }
        return output.ToString();
    }
}
