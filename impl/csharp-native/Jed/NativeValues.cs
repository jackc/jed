using System.Globalization;

namespace Jed;

/// <summary>Custom native parameter conversion without reflection.</summary>
public interface ISqlValueProvider { SqlValue ToSqlValue(); }

public readonly partial struct SqlValue
{
    internal sealed record NativeTime(long CalendarMicros, int Days, long? InstantMicros);
    internal NativeTime? NativeTemporal => payload as NativeTime;
    private static long Micros(DateTime time) => TemporalValue.FloorDiv(time.Ticks - DateTime.UnixEpoch.Ticks, 10);
    private static SqlValue NativeDateTime(DateTime time)
    {
        if (time.Kind == DateTimeKind.Local)
            throw Errors.Type("local DateTime requires an explicit DateTimeOffset");
        var micros = Micros(time);
        return new(time.Kind == DateTimeKind.Utc ? SqlType.Timestamptz : SqlType.Timestamp, true, micros,
            payload: new NativeTime(micros, checked((int)TemporalValue.DaysFromCivil(time.Year, time.Month, time.Day)), time.Kind == DateTimeKind.Utc ? micros : null));
    }
    private static SqlValue NativeDateTimeOffset(DateTimeOffset time) => new(SqlType.Timestamptz, true, Micros(time.UtcDateTime),
        payload: new NativeTime(Micros(time.DateTime), checked((int)TemporalValue.DaysFromCivil(time.Year, time.Month, time.Day)), Micros(time.UtcDateTime)));

    public static implicit operator SqlValue(bool value) => Boolean(value);
    public static implicit operator SqlValue(byte value) => Integer(value);
    public static implicit operator SqlValue(short value) => Integer(value);
    public static implicit operator SqlValue(int value) => Integer(value);
    public static implicit operator SqlValue(long value) => Integer(value);
    public static implicit operator SqlValue(float value) => Float(value, SqlType.F32);
    public static implicit operator SqlValue(double value) => Float(value);
    public static implicit operator SqlValue(decimal value) => Decimal(ExactDecimal.Parse(value.ToString(CultureInfo.InvariantCulture)));
    public static implicit operator SqlValue(string? value) => value is null ? Null() : Text(value);
    public static implicit operator SqlValue(byte[]? value) => value is null ? Null() : Bytes(value);
    public static implicit operator SqlValue(Guid value) => OwnedBytes(value.ToByteArray(bigEndian: true), SqlType.Uuid);
    public static implicit operator SqlValue(DateOnly value) => Date(checked((int)TemporalValue.DaysFromCivil(value.Year, value.Month, value.Day)));
    public static implicit operator SqlValue(DateTime value) => NativeDateTime(value);
    public static implicit operator SqlValue(DateTimeOffset value) => NativeDateTimeOffset(value);
    public static implicit operator SqlValue(ExactDecimal value) => Decimal(value);
    public static implicit operator SqlValue(SqlInterval value) => Interval(value);
    public static implicit operator SqlValue(SqlArray? value) => value is null ? Null() : Array(value);
    public static implicit operator SqlValue(SqlComposite? value) => value is null ? Null() : Composite(value);
    public static implicit operator SqlValue(SqlRange? value) => value is null ? Null() : Range(value);

    /// <summary>Converts native scalars and one-dimensional arrays. Binding still infers and checks SQL types.</summary>
    public static SqlValue From(object? value) => value switch
    {
        null => Null(), SqlValue sql => sql, ISqlValueProvider provider => provider.ToSqlValue(),
        bool boolean => boolean, byte number => number, sbyte number => Integer(number), short number => number,
        ushort number => Integer(number), int number => number, uint number => Integer(number), long number => number,
        ulong number when number <= long.MaxValue => Integer((long)number),
        ulong => throw new SqlException(SqlStates.NumericValueOutOfRange, "unsigned parameter exceeds i64"),
        float number => number, double number => number, decimal number => number, string text => text, byte[] bytes => bytes,
        Guid uuid => uuid, DateOnly date => date, DateTime time => time, DateTimeOffset time => time,
        ExactDecimal number => number, SqlInterval interval => interval, SqlArray array => array, SqlComposite record => record, SqlRange range => range,
        System.Array array => NativeArray(array),
        _ => throw Errors.Type("unsupported native parameter type; pass SqlValue or implement ISqlValueProvider")
    };
    private static SqlValue NativeArray(System.Array source)
    {
        if (source.Rank != 1) throw Errors.Type("native array must be one-dimensional; use SqlArray for explicit dimensions");
        var values = new SqlValue[source.Length]; var type = SqlType.Unknown; var index = 0;
        foreach (var item in source)
        {
            var value = From(item);
            if (value.Type.Kind == SqlKind.Array) throw Errors.Type("nested native arrays require an explicit rectangular SqlArray");
            if (!value.IsNull) type = type == SqlType.Unknown ? value.Type : Resolve.Common(type, value.Type);
            values[index++] = value;
        }
        if (type == SqlType.Unknown) type = SqlType.Text;
        for (var i = 0; i < values.Length; i++)
            if (!values[i].IsNull && values[i].Type != type) values[i] = Resolve.Cast(values[i], type);
        return Array(new(type, values));
    }

    /// <summary>Reads a native value with checked narrowing. Nullable and reference targets accept SQL NULL.</summary>
    public T Get<T>()
    {
        if (typeof(T) == typeof(SqlValue)) return (T)(object)this;
        if (IsNull)
        {
            if (default(T) is null) return default!;
            RequireValue();
        }
        try
        {
            object value;
            if (typeof(T) == typeof(object)) value = ToNative()!;
            else if (typeof(T) == typeof(long) || typeof(T) == typeof(long?)) value = AsInteger();
            else if (typeof(T) == typeof(int) || typeof(T) == typeof(int?)) value = checked((int)AsInteger());
            else if (typeof(T) == typeof(short) || typeof(T) == typeof(short?)) value = checked((short)AsInteger());
            else if (typeof(T) == typeof(byte) || typeof(T) == typeof(byte?)) value = checked((byte)AsInteger());
            else if (typeof(T) == typeof(bool) || typeof(T) == typeof(bool?)) value = AsBoolean();
            else if (typeof(T) == typeof(double) || typeof(T) == typeof(double?)) value = AsFloat();
            else if (typeof(T) == typeof(float) || typeof(T) == typeof(float?)) value = (float)FloatValue.Narrow(AsFloat());
            else if (typeof(T) == typeof(string)) value = AsText();
            else if (typeof(T) == typeof(byte[])) value = Type == SqlType.Bytea ? AsBytes() : throw Errors.Type("value is not bytea");
            else if (typeof(T) == typeof(Guid) || typeof(T) == typeof(Guid?)) value = Type == SqlType.Uuid ? new Guid(ByteSpan, bigEndian: true) : throw Errors.Type("value is not uuid");
            else if (typeof(T) == typeof(DateOnly) || typeof(T) == typeof(DateOnly?)) value = NativeDate();
            else if (typeof(T) == typeof(DateTime) || typeof(T) == typeof(DateTime?)) value = NativeTimestamp();
            else if (typeof(T) == typeof(DateTimeOffset) || typeof(T) == typeof(DateTimeOffset?)) value = Type == SqlType.Timestamptz ? new DateTimeOffset(NativeTimestamp()) : throw Errors.Type("value is not timestamptz");
            else if (typeof(T) == typeof(ExactDecimal)) value = AsDecimal();
            else if (typeof(T) == typeof(decimal) || typeof(T) == typeof(decimal?)) value = NativeDecimal();
            else if (typeof(T) == typeof(SqlInterval) || typeof(T) == typeof(SqlInterval?)) value = AsInterval();
            else if (typeof(T) == typeof(SqlArray)) value = AsArray();
            else if (typeof(T) == typeof(SqlRange)) value = AsRange();
            else if (typeof(T) == typeof(SqlComposite)) value = AsComposite();
            else if (typeof(T) == typeof(SqlJsonNode)) value = AsJsonb();
            else if (typeof(T) == typeof(SqlJsonPath)) value = AsJsonpath();
            else throw Errors.Type("unsupported native result type; use SqlValue or an explicit row mapping delegate");
            return (T)value;
        }
        catch (OverflowException) { throw new SqlException(SqlStates.NumericValueOutOfRange, "native result is outside the destination range"); }
    }
    private decimal NativeDecimal()
    {
        var exact = AsDecimal();
        if (!decimal.TryParse(exact.ToString(), NumberStyles.Float, CultureInfo.InvariantCulture, out var number) ||
            ExactDecimal.Parse(number.ToString(CultureInfo.InvariantCulture)).CompareTo(exact) != 0)
            throw new SqlException(SqlStates.NumericValueOutOfRange, "exact SQL decimal does not fit System.Decimal");
        return number;
    }
    private DateOnly NativeDate()
    {
        var days = AsDate();
        try { return DateOnly.FromDateTime(DateTime.UnixEpoch.AddDays(days)); }
        catch (ArgumentOutOfRangeException) { throw TemporalValue.Overflow(); }
    }
    private DateTime NativeTimestamp()
    {
        var micros = AsTimestamp();
        if (TemporalValue.IsInfinite(micros)) throw TemporalValue.Overflow();
        try { return new DateTime(checked(DateTime.UnixEpoch.Ticks + checked(micros * 10)), Type == SqlType.Timestamptz ? DateTimeKind.Utc : DateTimeKind.Unspecified); }
        catch (Exception error) when (error is OverflowException or ArgumentOutOfRangeException) { throw TemporalValue.Overflow(); }
    }
    /// <summary>Lossless native representation; temporal values outside CLR's finite range stay SqlValue.</summary>
    public object? ToNative()
    {
        if (IsNull) return null;
        if (Type.Kind is SqlKind.Date or SqlKind.Timestamp or SqlKind.Timestamptz)
        {
            try
            {
                if (Type.Kind == SqlKind.Date) return NativeDate();
                if (Type.Kind == SqlKind.Timestamp) return NativeTimestamp();
                return new DateTimeOffset(NativeTimestamp());
            }
            catch (SqlException error) when (error.SqlState == "22008") { return this; }
        }
        return Type.Kind switch
        {
            SqlKind.I16 or SqlKind.I32 or SqlKind.I64 => (object)AsInteger(), SqlKind.Boolean => (object)AsBoolean(),
            SqlKind.F32 => (object)(float)AsFloat(), SqlKind.F64 => (object)AsFloat(), SqlKind.Text => (object)AsText(),
            SqlKind.Bytea => (object)AsBytes(), SqlKind.Uuid => (object)new Guid(ByteSpan, bigEndian: true),
            SqlKind.Decimal => (object)AsDecimal(), SqlKind.Interval => (object)AsInterval(), SqlKind.Array => (object)AsArray(),
            SqlKind.Range => (object)AsRange(), SqlKind.Composite => (object)AsComposite(), _ => (object)this
        };
    }
}

public sealed partial class Row
{
    public bool IsNull(int index) => this[index].IsNull;
    public T Get<T>(int index) => this[index].Get<T>();
    public object?[] Values() => Enumerable.Range(0, Count).Select(index => this[index].ToNative()).ToArray();
}
