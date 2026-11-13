using System.Globalization;
using System.Text;

namespace Jed;

public enum SqlType { Unknown, I16, I32, I64, Boolean, Text }

internal sealed record ScalarFact(SqlType Type, string Name, string Family, long Min, long Max, int Rank, string[] Aliases);

public static class SqlTypes
{
    public static string Name(this SqlType type) => type == SqlType.Unknown ? "unknown" : Fact(type).Name;
    internal static ScalarFact Fact(SqlType type) => ScalarCatalog.Facts.Single(f => f.Type == type);
    internal static bool IsInteger(this SqlType type) => type is SqlType.I16 or SqlType.I32 or SqlType.I64;
    internal static SqlType Parse(string name)
    {
        foreach (var fact in ScalarCatalog.Facts)
            if (fact.Aliases.Contains(name, StringComparer.Ordinal)) return fact.Type;
        if (ScalarCatalog.KnownNames.Contains(name, StringComparer.Ordinal)) throw Errors.Unsupported($"type {name}");
        // No invented type mapping: later slices extend the generated scalar set.
        throw new SqlException(SqlStates.UndefinedObject, $"type does not exist: {name}");
    }

    internal static long Check(long value, SqlType type)
    {
        var fact = Fact(type);
        if (value < fact.Min || value > fact.Max) throw Errors.Overflow();
        return value;
    }
}

/// <summary>An immutable, owned scalar value. default(SqlValue) is untyped SQL NULL.</summary>
public readonly struct SqlValue
{
    private readonly bool present;
    private readonly long integer;
    private readonly string? text;
    internal int TextBytes { get; }
    public SqlType Type { get; }
    public bool IsNull => !present;

    private SqlValue(SqlType type, bool present, long integer = 0, string? text = null, int textBytes = 0)
    {
        Type = type;
        this.present = present;
        this.integer = integer;
        this.text = text;
        TextBytes = textBytes;
    }

    public static SqlValue Null(SqlType type = SqlType.Unknown) => new(type, false);
    public static SqlValue Integer(long value, SqlType type = SqlType.I64)
    {
        if (!type.IsInteger()) throw Errors.Type("integer value requires an integer type");
        return new(type, true, SqlTypes.Check(value, type));
    }
    public static SqlValue Boolean(bool value) => new(SqlType.Boolean, true, value ? 1 : 0);
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
        if (IsNull) throw new SqlException(SqlStates.NullValueNotAllowed, "cannot read SQL NULL as a nonnullable value");
    }
    public override string ToString() => IsNull ? "NULL" : Type switch
    {
        SqlType.Boolean => integer == 0 ? "false" : "true",
        SqlType.Text => text!,
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
