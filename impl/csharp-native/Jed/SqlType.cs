namespace Jed;

public enum SqlKind { Unknown, I16, I32, I64, Boolean, Text, Decimal, Bytea, Uuid, Timestamp, Timestamptz, Interval, F32, F64, Date, Json, Jsonb, Jsonpath, Array, Range, Composite }

public sealed record SqlField(string Name, SqlType Type, bool NotNull = false, int? Precision = null, int? Scale = null, int? Length = null, byte IntervalFields = 0);

/// <summary>A resolved SQL type; recursive types retain their element or catalog identity.</summary>
public sealed record SqlType
{
    public SqlKind Kind { get; }
    public SqlType? Element { get; }
    public string? Identity { get; }
    public IReadOnlyList<SqlField> Fields { get; }
    internal int CompositeDepth { get; }
    private SqlType(SqlKind kind, SqlType? element = null, string? identity = null, IEnumerable<SqlField>? fields = null)
    {
        Kind = kind; Element = element; Identity = identity; Fields = System.Array.AsReadOnly(fields?.ToArray() ?? []);
        CompositeDepth = kind == SqlKind.Array ? element!.CompositeDepth : kind == SqlKind.Composite ? 1 + Fields.Select(field => field.Type.CompositeDepth).DefaultIfEmpty().Max() : 0;
        if (CompositeDepth > 32) throw new SqlException(SqlStates.StatementTooComplex, "composite nesting exceeds 32");
    }

    public static readonly SqlType Unknown = new(SqlKind.Unknown);
    public static readonly SqlType I16 = new(SqlKind.I16);
    public static readonly SqlType I32 = new(SqlKind.I32);
    public static readonly SqlType I64 = new(SqlKind.I64);
    public static readonly SqlType Boolean = new(SqlKind.Boolean);
    public static readonly SqlType Text = new(SqlKind.Text);
    public static readonly SqlType Decimal = new(SqlKind.Decimal);
    public static readonly SqlType Bytea = new(SqlKind.Bytea);
    public static readonly SqlType Uuid = new(SqlKind.Uuid);
    public static readonly SqlType Timestamp = new(SqlKind.Timestamp);
    public static readonly SqlType Timestamptz = new(SqlKind.Timestamptz);
    public static readonly SqlType Interval = new(SqlKind.Interval);
    public static readonly SqlType F32 = new(SqlKind.F32);
    public static readonly SqlType F64 = new(SqlKind.F64);
    public static readonly SqlType Date = new(SqlKind.Date);
    public static readonly SqlType Json = new(SqlKind.Json);
    public static readonly SqlType Jsonb = new(SqlKind.Jsonb);
    public static readonly SqlType Jsonpath = new(SqlKind.Jsonpath);

    public static SqlType Array(SqlType element) => new(SqlKind.Array, element.Kind == SqlKind.Array ? element.Element : element);
    public static SqlType Range(SqlType element)
    {
        var fact = RangeCatalog.Facts.FirstOrDefault(f => f.Element == element) ?? throw Errors.Type("type has no range");
        return new(SqlKind.Range, element, fact.Name);
    }
    internal static SqlType Range(SqlType element, string identity) => new(SqlKind.Range, element, identity);
    public static SqlType Composite(string identity, IEnumerable<SqlField>? fields = null) => new(SqlKind.Composite, identity: identity, fields: fields);
    public static SqlType Record(IEnumerable<SqlField> fields) => new(SqlKind.Composite, identity: "record", fields: fields);
    public bool Equals(SqlType? other)
    {
        if (ReferenceEquals(this, other)) return true;
        if (other is null || Kind != other.Kind || Element != other.Element || Identity != other.Identity) return false;
        if (Kind != SqlKind.Composite || Identity != "record") return true;
        return Fields.Count == other.Fields.Count && Fields.Select(f => f.Type).SequenceEqual(other.Fields.Select(f => f.Type));
    }
    public override int GetHashCode()
    {
        unchecked
        {
            var hash = (uint)Kind + 2166136261u;
            if (Element is not null) hash = (hash ^ (uint)Element.GetHashCode()) * 16777619u;
            if (Identity is not null) foreach (var character in Identity) hash = (hash ^ character) * 16777619u;
            if (Kind == SqlKind.Composite && Identity == "record") foreach (var field in Fields) hash = (hash ^ (uint)field.Type.GetHashCode()) * 16777619u;
            return (int)hash;
        }
    }
    public override string ToString() => this.Name();
}

internal sealed record ScalarFact(SqlType Type, string Name, string Family, long Min, long Max, int Rank, string[] Aliases);
internal sealed record RangeFact(string Name, SqlType Element, string[] Aliases, bool Discrete);
internal sealed record TypeSpec(SqlType Type, int? Precision = null, int? Scale = null, int? Length = null, byte IntervalFields = 0);

public static class SqlTypes
{
    public static string Name(this SqlType type) => type.Kind switch
    {
        SqlKind.Unknown => "unknown",
        SqlKind.Array => type.Element!.Name() + "[]",
        SqlKind.Range or SqlKind.Composite => type.Identity!,
        _ => Fact(type).Name
    };
    internal static ScalarFact Fact(SqlType type) => ScalarCatalog.Facts.Single(f => f.Type == type);
    internal static bool IsInteger(this SqlType type) => type.Kind is SqlKind.I16 or SqlKind.I32 or SqlKind.I64;
    internal static bool IsFloat(this SqlType type) => type.Kind is SqlKind.F32 or SqlKind.F64;
    internal static bool IsNumeric(this SqlType type) => type.IsInteger() || type.IsFloat() || type == SqlType.Decimal;
    internal static SqlType Parse(string name, Func<string, SqlType?>? namedType = null) => ParseSpec(name, namedType).Type;
    internal static TypeSpec ParseSpec(string name, Func<string, SqlType?>? namedType = null)
    {
        if (name == "interval" || name.StartsWith("interval ", StringComparison.Ordinal) || name.StartsWith("interval(", StringComparison.Ordinal))
        {
            if (name.EndsWith("[]", StringComparison.Ordinal))
            {
                if (name != "interval[]") throw Errors.Unsupported("interval modifier on array");
                return new(SqlType.Array(SqlType.Interval));
            }
            var tail = name[8..].Trim();
            var precisionAt = tail.IndexOf('('); int? precision = null;
            if (precisionAt >= 0)
            {
                if (!tail.EndsWith(')') || !int.TryParse(tail.AsSpan(precisionAt + 1, tail.Length - precisionAt - 2), System.Globalization.NumberStyles.AllowLeadingSign, System.Globalization.CultureInfo.InvariantCulture, out var parsed)) throw Errors.Syntax("invalid interval precision");
                if (parsed < 0) throw new SqlException(SqlStates.InvalidParameterValue, "negative interval precision");
                precision = Math.Min(parsed, 6); tail = tail[..precisionAt].Trim();
            }
            byte fields = 0;
            if (tail.Length != 0)
            {
                var qualifier = DatetimeUnits.Qualifiers.FirstOrDefault(q => q.Name == tail);
                if (qualifier.Code == 0 || precision is not null && qualifier.Least != "second") throw Errors.Syntax("invalid interval qualifier");
                fields = qualifier.Code;
            }
            return new(SqlType.Interval, Precision: precision, IntervalFields: fields);
        }
        if (name.EndsWith("[]", StringComparison.Ordinal))
        {
            if (name.Contains('(')) throw Errors.Unsupported("type modifier on array");
            return new(SqlType.Array(Parse(name[..^2], namedType)));
        }
        var open = name.IndexOf('(');
        var baseName = open < 0 ? name : name[..open];
        var range = RangeCatalog.Facts.FirstOrDefault(f => f.Name == baseName || f.Aliases.Contains(baseName, StringComparer.Ordinal));
        if (range is not null)
        {
            if (open >= 0) throw Errors.Syntax("range type does not take a modifier");
            return new(SqlType.Range(range.Element));
        }
        var fact = ScalarCatalog.Facts.FirstOrDefault(f => f.Aliases.Contains(baseName, StringComparer.Ordinal));
        if (fact is null)
        {
            var composite = namedType?.Invoke(baseName);
            if (composite is null) throw new SqlException(SqlStates.UndefinedObject, $"type does not exist: {baseName}");
            if (open >= 0) throw Errors.Syntax("composite type does not take a modifier");
            return new(composite);
        }
        if (open < 0) return new(fact.Type);
        var args = name[(open + 1)..^1].Split(',');
        var values = new int[args.Length];
        for (var i = 0; i < args.Length; i++)
            if (!int.TryParse(args[i], System.Globalization.NumberStyles.AllowLeadingSign, System.Globalization.CultureInfo.InvariantCulture, out values[i]))
                throw new SqlException(SqlStates.InvalidParameterValue, "invalid type modifier");
        if (fact.Type == SqlType.Decimal && values.Length is 1 or 2)
        {
            var precision = values[0]; var scale = values.Length == 2 ? values[1] : 0;
            if (precision is < 1 or > 1000 || scale < 0 || scale > precision)
                throw new SqlException(SqlStates.InvalidParameterValue, "invalid decimal precision or scale");
            return new(fact.Type, precision, scale);
        }
        if (fact.Type == SqlType.Text && values.Length == 1 && baseName != "text")
        {
            if (values[0] is < 1 or > 10485760) throw new SqlException(SqlStates.InvalidParameterValue, "invalid varchar length");
            return new(fact.Type, Length: values[0]);
        }
        if (fact.Type.Kind is SqlKind.Timestamp or SqlKind.Timestamptz)
        {
            if (values.Length != 1 || values[0] < 0) throw Errors.Syntax("timestamp requires one unsigned precision");
            return new(fact.Type, Precision: Math.Min(values[0], 6));
        }
        throw Errors.Unsupported("modifier for this type");
    }

    internal static long Check(long value, SqlType type)
    {
        var fact = Fact(type);
        if (value < fact.Min || value > fact.Max) throw Errors.Overflow();
        return value;
    }
}
