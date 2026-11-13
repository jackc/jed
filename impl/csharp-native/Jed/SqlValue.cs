using System.Globalization;
using System.Text;

namespace Jed;

/// <summary>An immutable, owned scalar value. default(SqlValue) is untyped SQL NULL.</summary>
public readonly partial struct SqlValue
{
    private readonly bool present;
    private readonly long integer;
    private readonly string? text;
    private readonly object? payload;
    internal int TextBytes { get; }
    private readonly SqlType? type;
    public SqlType Type => type ?? SqlType.Unknown;
    public bool IsNull => !present;
    internal bool IsDeferred => payload is DeferredSqlValue;
    internal static SqlValue Deferred(SqlType type, DeferredSqlValue value) => new(type, true, payload: value);
    internal ValueTask<SqlValue> ResolveAsync(CancellationToken cancellationToken = default) => payload is DeferredSqlValue deferred ? deferred.ResolveAsync(cancellationToken) : new(this);

    private SqlValue(SqlType type, bool present, long integer = 0, string? text = null, int textBytes = 0, object? payload = null)
    {
        this.type = type;
        this.present = present;
        this.integer = integer;
        this.text = text;
        TextBytes = textBytes;
        this.payload = payload;
    }

    public static SqlValue Null(SqlType? type = null) => new(type ?? SqlType.Unknown, false);
    public static SqlValue Integer(long value, SqlType? type = null)
    {
        type ??= SqlType.I64;
        if (!type.IsInteger()) throw Errors.Type("integer value requires an integer type");
        return new(type, true, SqlTypes.Check(value, type));
    }
    public static SqlValue Json(string value)
    {
        _ = JsonInput.Parse(value, decode: false, canonical: false);
        return StoredJson(value);
    }
    internal static SqlValue StoredJson(string value) => new(SqlType.Json, true, text: value, textBytes: TextEncoding.ByteCount(value));
    public static SqlValue Jsonpath(SqlJsonPath value) => new(SqlType.Jsonpath, true, text: value.Text, textBytes: TextEncoding.ByteCount(value.Text), payload: value);
    public SqlJsonPath AsJsonpath() { RequireValue(); if (Type != SqlType.Jsonpath) throw Errors.Type("value is not jsonpath"); return (SqlJsonPath)payload!; }
    public static SqlValue Jsonb(SqlJsonNode value) => new(SqlType.Jsonb, true, payload: value);
    public string AsJsonText() { RequireValue(); if (Type != SqlType.Json) throw Errors.Type("value is not json"); return text!; }
    public SqlJsonNode AsJsonb() { RequireValue(); if (Type != SqlType.Jsonb) throw Errors.Type("value is not jsonb"); return (SqlJsonNode)payload!; }
    public static SqlValue Boolean(bool value) => new(SqlType.Boolean, true, value ? 1 : 0);
    public static SqlValue Date(int days) => new(SqlType.Date, true, days);
    public static SqlValue Timestamp(long micros, bool withZone = false) => new(withZone ? SqlType.Timestamptz : SqlType.Timestamp, true, micros);
    public static SqlValue Interval(SqlInterval interval) => new(SqlType.Interval, true, payload: interval);
    public static SqlValue Array(SqlArray array) => new(SqlType.Array(array.ElementType), true, payload: array);
    public static SqlValue Range(SqlRange range) => new(range.Type, true, payload: range);
    public static SqlValue Composite(SqlComposite composite) => new(composite.Type, true, payload: composite);
    public SqlArray AsArray()
    {
        RequireValue(); if (Type.Kind != SqlKind.Array) throw Errors.Type("value is not an array"); return (SqlArray)payload!;
    }
    public SqlRange AsRange()
    {
        RequireValue(); if (Type.Kind != SqlKind.Range) throw Errors.Type("value is not a range"); return (SqlRange)payload!;
    }
    public SqlComposite AsComposite()
    {
        RequireValue(); if (Type.Kind != SqlKind.Composite) throw Errors.Type("value is not a record"); return (SqlComposite)payload!;
    }
    public int AsDate()
    {
        RequireValue();
        if (Type != SqlType.Date) throw Errors.Type("value is not a date");
        return (int)integer;
    }
    public long AsTimestamp()
    {
        RequireValue();
        if (Type.Kind is not (SqlKind.Timestamp or SqlKind.Timestamptz)) throw Errors.Type("value is not a timestamp");
        return integer;
    }
    public SqlInterval AsInterval()
    {
        RequireValue();
        if (Type != SqlType.Interval) throw Errors.Type("value is not an interval");
        return (SqlInterval)payload!;
    }
    public static SqlValue Decimal(ExactDecimal value) => new(SqlType.Decimal, true, payload: value.CheckCap());
    internal static SqlValue StoredDecimal(ExactDecimal value) => new(SqlType.Decimal, true, payload: value);
    public static SqlValue Float(double value, SqlType? type = null)
    {
        type ??= SqlType.F64;
        if (!type.IsFloat()) throw Errors.Type("float value requires a floating point type");
        if (type == SqlType.F32) value = FloatValue.Narrow(value);
        return new(type, true, BitConverter.DoubleToInt64Bits(value));
    }
    public double AsFloat()
    {
        RequireValue();
        if (!Type.IsFloat()) throw Errors.Type("value is not floating point");
        return BitConverter.Int64BitsToDouble(integer);
    }
    public static SqlValue Bytes(ReadOnlySpan<byte> value) => new(SqlType.Bytea, true, payload: value.ToArray());
    internal static SqlValue OwnedBytes(byte[] value, SqlType? type = null) => new(type ?? SqlType.Bytea, true, payload: value);
    public ExactDecimal AsDecimal()
    {
        RequireValue();
        if (Type != SqlType.Decimal) throw Errors.Type("value is not decimal");
        return (ExactDecimal)payload!;
    }
    public byte[] AsBytes() => ByteSpan.ToArray();
    internal ReadOnlySpan<byte> ByteSpan
    {
        get
        {
            RequireValue();
            if (Type.Kind is not (SqlKind.Bytea or SqlKind.Uuid)) throw Errors.Type("value is not binary");
            return (byte[])payload!;
        }
    }
    public static SqlValue Text(string value)
    {
        ArgumentNullException.ThrowIfNull(value);
        var bytes = TextEncoding.ByteCount(value);
        return new(SqlType.Text, true, text: value, textBytes: bytes);
    }

    public long AsInteger()
    {
        RequireValue();
        if (!Type.IsInteger()) throw Errors.Type("value is not an integer");
        return integer;
    }
    public bool AsBoolean()
    {
        RequireValue();
        if (Type != SqlType.Boolean) throw Errors.Type("value is not a boolean");
        return integer != 0;
    }
    public string AsText()
    {
        RequireValue();
        if (Type != SqlType.Text) throw Errors.Type("value is not text");
        return text!;
    }
    private void RequireValue()
    {
        if (IsDeferred) throw new InvalidOperationException("deferred SQL value must be resolved before access");
        if (IsNull) throw new SqlException(SqlStates.NullValueNotAllowed, "cannot read SQL NULL as a nonnullable value");
    }
    public override string ToString() => IsNull ? "NULL" : Type.Kind switch
    {
        SqlKind.Boolean => integer == 0 ? "false" : "true",
        SqlKind.Text or SqlKind.Json or SqlKind.Jsonpath => text!,
        SqlKind.Jsonb => ((SqlJsonNode)payload!).ToString(),
        SqlKind.Decimal => ((ExactDecimal)payload!).ToString(),
        SqlKind.Date => TemporalInput.RenderDate((int)integer),
        SqlKind.Timestamp or SqlKind.Timestamptz => TemporalInput.RenderTimestamp(integer, Type == SqlType.Timestamptz),
        SqlKind.Interval => ((SqlInterval)payload!).ToString(),
        SqlKind.Array => ((SqlArray)payload!).ToString(),
        SqlKind.Range => ((SqlRange)payload!).ToString(),
        SqlKind.Composite => ((SqlComposite)payload!).ToString(),
        SqlKind.F32 or SqlKind.F64 => FloatValue.Render(AsFloat(), Type == SqlType.F32),
        SqlKind.Bytea => "\\x" + Convert.ToHexStringLower((byte[])payload!),
        SqlKind.Uuid => new Guid((byte[])payload!, bigEndian: true).ToString("D", CultureInfo.InvariantCulture),
        _ => integer.ToString(CultureInfo.InvariantCulture)
    };
}

internal static class TextEncoding
{
    internal static readonly UTF8Encoding Utf8 = new(false, true);
    internal static void Validate(string text)
    {
        _ = ByteCount(text);
    }
    internal static int ByteCount(string text)
    {
        try { return Utf8.GetByteCount(text); }
        catch (EncoderFallbackException) { throw new SqlException(SqlStates.CharacterNotInRepertoire, "invalid Unicode scalar in host string"); }
    }

    // ASCII-only identifier folding is independent of the host's Unicode tables.
    internal static string FoldName(string value) => string.Create(value.Length, value, (span, input) =>
    {
        for (var i = 0; i < input.Length; i++)
            span[i] = input[i] is >= 'A' and <= 'Z' ? (char)(input[i] + ('a' - 'A')) : input[i];
    });
}
