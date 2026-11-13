using System.Text;

namespace Jed;

/// <summary>Immutable, host-supplied UCA weights. No operating-system locale is consulted.</summary>
public sealed class Collation
{
    internal readonly record struct Element(byte Flags, ushort Primary, ushort Secondary, ushort Tertiary);
    internal sealed record Mapping(int[] Points, Element[] Elements);
    private readonly Mapping[] singles, contractions;
    private readonly byte[]? original;
    public string Name { get; }
    public string UnicodeVersion { get; }
    public string CldrVersion { get; }
    public string Description { get; }
    public uint ContentHash { get; }
    internal Collation(string name, string unicodeVersion, string cldrVersion, string description, Mapping[] singles, Mapping[] contractions, byte[]? original = null)
    {
        Name = name; UnicodeVersion = unicodeVersion; CldrVersion = cldrVersion; Description = description;
        this.singles = singles; this.contractions = contractions; this.original = original;
        ContentHash = StorageFormat.Crc32(TableBytes());
    }
    public static Collation Open(ReadOnlySpan<byte> bytes)
    {
        try
        {
            var reader = new WireReader(bytes);
            if (!reader.Bytes(6).SequenceEqual("JCOLL\0"u8) || reader.U16() != 1) throw Bad("invalid JCOLL header");
            var name = reader.String(); var unicode = reader.String(); var cldr = reader.String(); var description = reader.String();
            var hash = reader.U32(); var raw = reader.U32(); var compressed = reader.U32();
            if (raw > 16 * 1024 * 1024 || compressed > reader.Remaining) throw Bad("invalid collation section bounds");
            var table = StorageLz4.Decompress(reader.Bytes((int)compressed), (int)raw); reader.Finish();
            if (StorageFormat.Crc32(table) != hash) throw Bad("collation checksum mismatch");
            var mappings = ReadTable(table, false); ValidateName(name);
            return new(name, unicode, cldr, description, mappings.Singles, mappings.Contractions, bytes.ToArray());
        }
        catch (SqlException error) when (error.SqlState == "XX001") { throw Bad(error.Message); }
        catch (OverflowException) { throw Bad("collation length overflow"); }
    }
    public byte[] Save()
    {
        if (original is not null) return original.ToArray();
        var writer = new WireWriter(); writer.Bytes("JCOLL\0"u8); writer.U16(1); writer.String(Name); writer.String(UnicodeVersion); writer.String(CldrVersion); writer.String(Description);
        var table = TableBytes(); var compressed = StorageLz4.Compress(table);
        writer.U32(ContentHash); writer.U32((uint)table.Length); writer.U32((uint)compressed.Length); writer.Bytes(compressed); return writer.ToArray();
    }
    public byte[] SortKey(string text)
    {
        TextEncoding.Validate(text); var points = text.EnumerateRunes().Select(rune => rune.Value).ToArray(); var elements = new List<Element>();
        for (var at = 0; at < points.Length;)
        {
            Mapping? best = null;
            var low = 0; var high = contractions.Length;
            while (low < high) { var middle = low + (high - low) / 2; if (contractions[middle].Points[0] < points[at]) low = middle + 1; else high = middle; }
            for (var i = low; i < contractions.Length && contractions[i].Points[0] == points[at]; i++)
            {
                var candidate = contractions[i];
                if (candidate.Points.Length <= points.Length - at && (best is null || candidate.Points.Length > best.Points.Length) && points.AsSpan(at, candidate.Points.Length).SequenceEqual(candidate.Points)) best = candidate;
            }
            if (best is null)
            {
                low = 0; high = singles.Length;
                while (low < high) { var middle = low + (high - low) / 2; if (singles[middle].Points[0] < points[at]) low = middle + 1; else high = middle; }
                if (low == singles.Length || singles[low].Points[0] != points[at]) throw Errors.Unsupported("implicit Unicode collation weight");
                best = singles[low];
            }
            elements.AddRange(best.Elements); at += best.Points.Length;
        }
        var writer = new WireWriter();
        for (var level = 0; level < 3; level++)
        {
            foreach (var element in elements)
            {
                var weight = level == 0 ? element.Primary : level == 1 ? element.Secondary : element.Tertiary;
                if (weight != 0) writer.U16(weight);
            }
            writer.U16(0);
        }
        writer.Bytes(KeyEncoding.Encode(SqlValue.Text(text))); return writer.ToArray();
    }
    internal byte[] TableBytes()
    {
        var writer = new WireWriter(); writer.Byte(1); writer.U32((uint)singles.Length); writer.U32((uint)contractions.Length);
        foreach (var mapping in singles) WriteMapping(writer, mapping, false);
        foreach (var mapping in contractions) WriteMapping(writer, mapping, true);
        return writer.ToArray();
    }
    internal static (Mapping[] Singles, Mapping[] Contractions) ReadTable(ReadOnlySpan<byte> bytes, bool delta)
    {
        var reader = new WireReader(bytes); if (reader.Byte() != 1) throw Bad("unsupported collation layout");
        var singlesCount = reader.U32(); var contractionsCount = reader.U32();
        var singles = ReadMappings(ref reader, singlesCount, false);
        var contractions = ReadMappings(ref reader, contractionsCount, true); reader.Finish(); return (singles, contractions);
    }
    private static Mapping[] ReadMappings(ref WireReader reader, uint count, bool contraction)
    {
        if (count > reader.Remaining / (contraction ? 17 : 12)) throw Bad("invalid collation mapping count");
        var mappings = new Mapping[(int)count];
        for (var i = 0; i < mappings.Length; i++)
        {
            var length = contraction ? reader.Byte() : 1; if (contraction && length < 2) throw Bad("invalid contraction length");
            var points = new int[length]; for (var j = 0; j < length; j++) points[j] = Point(reader.U32());
            if (i > 0 && ComparePoints(mappings[i - 1].Points, points) >= 0) throw Bad("unordered collation mapping");
            var elements = new Element[reader.Byte()]; if (elements.Length == 0) throw Bad("empty collation mapping");
            for (var j = 0; j < elements.Length; j++)
            {
                var flags = reader.Byte(); if (flags > 1) throw Bad("unknown collation flags"); elements[j] = new(flags, reader.U16(), reader.U16(), reader.U16());
            }
            mappings[i] = new(points, elements);
        }
        return mappings;
    }
    private static void WriteMapping(WireWriter writer, Mapping mapping, bool contraction)
    {
        if (contraction) writer.Byte((byte)mapping.Points.Length);
        foreach (var point in mapping.Points) writer.U32((uint)point);
        writer.Byte((byte)mapping.Elements.Length);
        foreach (var element in mapping.Elements) { writer.Byte(element.Flags); writer.U16(element.Primary); writer.U16(element.Secondary); writer.U16(element.Tertiary); }
    }
    internal Collation Tailor(string name, ReadOnlySpan<byte> delta)
    {
        var overrides = ReadTable(delta, true);
        static Mapping[] Merge(Mapping[] basis, Mapping[] changes)
        {
            var result = new List<Mapping>(); var a = 0; var b = 0;
            while (a < basis.Length || b < changes.Length)
            {
                if (b == changes.Length) { result.Add(basis[a++]); continue; }
                if (a == basis.Length) { result.Add(changes[b++]); continue; }
                var order = ComparePoints(basis[a].Points, changes[b].Points);
                if (order < 0) result.Add(basis[a++]); else { if (order == 0) a++; result.Add(changes[b++]); }
            }
            return result.ToArray();
        }
        return new(name, UnicodeVersion, CldrVersion, Description, Merge(singles, overrides.Singles), Merge(contractions, overrides.Contractions));
    }
    internal static int ComparePoints(int[] left, int[] right) => left.AsSpan().SequenceCompareTo(right);
    internal static int Point(uint value) => value <= 0x10ffff && value is not (>= 0xd800 and <= 0xdfff) ? (int)value : throw Bad("invalid Unicode scalar");
    internal static void ValidateName(string name) { if (name.Length == 0 || name == "C" || name.Contains('\0')) throw Bad("invalid collation name"); }
    internal static SqlException Bad(string message) => new("22023", message);
}
