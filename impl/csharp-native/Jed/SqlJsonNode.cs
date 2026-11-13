using System.Collections.ObjectModel;
using System.Globalization;
using System.Text;

namespace Jed;

public enum SqlJsonKind : byte { Null, False, True, Number, String, Array = 6, Object = 7 }
public readonly record struct SqlJsonMember(string Key, SqlJsonNode Value);

/// <summary>An immutable exact JSON value. Object members use PostgreSQL's length-first canonical key order.</summary>
public sealed class SqlJsonNode
{
    public const int MaximumDepth = 256;
    public SqlJsonKind Kind { get; }
    public ExactDecimal? Number { get; }
    public string? Text { get; }
    public IReadOnlyList<SqlJsonNode> Elements { get; }
    public IReadOnlyList<SqlJsonMember> Members { get; }
    public int Depth { get; }
    internal string? Source { get; }
    internal int Start { get; }
    internal int Length { get; }
    public static SqlJsonNode Null { get; } = new(SqlJsonKind.Null);
    public static SqlJsonNode Boolean(bool value) => new(value ? SqlJsonKind.True : SqlJsonKind.False);
    public static SqlJsonNode Numeric(ExactDecimal value) => new(SqlJsonKind.Number, number: value.CheckCap());
    public static SqlJsonNode String(string value) { TextEncoding.Validate(value); if (value.Contains('\0')) throw new SqlException("22P05", "unsupported Unicode escape sequence"); return new(SqlJsonKind.String, text: value); }
    public static SqlJsonNode Array(IEnumerable<SqlJsonNode> elements) => new(SqlJsonKind.Array, elements: elements.ToArray());
    public static SqlJsonNode Object(IEnumerable<SqlJsonMember> members) => new(SqlJsonKind.Object, members: Canonicalize(members));
    internal SqlJsonNode(SqlJsonKind kind, ExactDecimal? number = null, string? text = null, SqlJsonNode[]? elements = null, SqlJsonMember[]? members = null, string? source = null, int start = 0, int length = 0)
    {
        Kind = kind; Number = number; Text = text; Elements = System.Array.AsReadOnly(elements ?? []); Members = System.Array.AsReadOnly(members ?? []);
        Depth = kind switch { SqlJsonKind.Array => 1 + (elements!.Length == 0 ? 0 : elements.Max(e => e.Depth)), SqlJsonKind.Object => 1 + (members!.Length == 0 ? 0 : members.Max(m => m.Value.Depth)), _ => 0 };
        if (Depth > MaximumDepth) throw TooDeep(); Source = source; Start = start; Length = length;
    }
    internal static SqlException TooDeep() => new("54001", "json nesting depth exceeds the maximum of 256");
    internal static SqlJsonMember[] Canonicalize(IEnumerable<SqlJsonMember> members)
    {
        var unique = new Dictionary<string, SqlJsonNode>(StringComparer.Ordinal);
        foreach (var member in members) { TextEncoding.Validate(member.Key); if (member.Key.Contains('\0')) throw new SqlException("22P05", "unsupported Unicode escape sequence"); unique[member.Key] = member.Value; }
        return unique.Select(pair => new SqlJsonMember(pair.Key, pair.Value)).OrderBy(member => member.Key, JsonKeyComparer.Instance).ToArray();
    }
    internal string Verbatim => Source is null ? ToString() : Source.Substring(Start, Length);
    internal long PayloadBytes => 32 + (Kind switch
    {
        SqlJsonKind.Number => 4L * Number!.ExportBase10000().Length,
        SqlJsonKind.String => TextEncoding.ByteCount(Text!),
        SqlJsonKind.Array => Elements.Sum(element => element.PayloadBytes),
        SqlJsonKind.Object => Members.Sum(member => TextEncoding.ByteCount(member.Key) + member.Value.PayloadBytes),
        _ => 0
    });
    public override string ToString() { var output = new StringBuilder(); Render(output); return output.ToString(); }
    internal void Render(StringBuilder output, bool spaces = true, bool objectColonSpaces = false)
    {
        switch (Kind)
        {
            case SqlJsonKind.Null: output.Append("null"); break;
            case SqlJsonKind.False: output.Append("false"); break;
            case SqlJsonKind.True: output.Append("true"); break;
            case SqlJsonKind.Number: output.Append(Number?.ToString() ?? Text!); break;
            case SqlJsonKind.String: Quote(output, Text!); break;
            case SqlJsonKind.Array:
                output.Append('['); for (var i = 0; i < Elements.Count; i++) { if (i != 0) output.Append(spaces ? ", " : ","); Elements[i].Render(output, spaces, objectColonSpaces); } output.Append(']'); break;
            case SqlJsonKind.Object:
                output.Append('{'); for (var i = 0; i < Members.Count; i++) { if (i != 0) output.Append(spaces ? ", " : ","); Quote(output, Members[i].Key); output.Append(objectColonSpaces ? " : " : spaces ? ": " : ":"); Members[i].Value.Render(output, spaces, objectColonSpaces); } output.Append('}'); break;
        }
    }
    internal static void Quote(StringBuilder output, string text)
    {
        output.Append('"');
        foreach (var character in text)
            switch (character)
            {
                case '"': output.Append("\\\""); break; case '\\': output.Append("\\\\"); break;
                case '\b': output.Append("\\b"); break; case '\f': output.Append("\\f"); break; case '\n': output.Append("\\n"); break; case '\r': output.Append("\\r"); break; case '\t': output.Append("\\t"); break;
                default: if (character < 32) output.Append("\\u").Append(((int)character).ToString("x4", CultureInfo.InvariantCulture)); else output.Append(character); break;
            }
        output.Append('"');
    }
}
internal sealed class JsonKeyComparer : IComparer<string>
{
    internal static readonly JsonKeyComparer Instance = new();
    public int Compare(string? left, string? right)
    {
        var a = TextEncoding.Utf8.GetBytes(left!); var b = TextEncoding.Utf8.GetBytes(right!);
        var length = a.Length.CompareTo(b.Length); return length != 0 ? length : a.AsSpan().SequenceCompareTo(b);
    }
}
