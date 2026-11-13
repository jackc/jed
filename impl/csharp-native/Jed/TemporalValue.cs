using System.Globalization;
using System.Text;

namespace Jed;

/// <summary>A calendar interval. Fields preserve their representation; comparison uses the SQL 30-day month span.</summary>
public readonly struct SqlInterval(int months, int days, long micros) : IComparable<SqlInterval>, IEquatable<SqlInterval>
{
    public int Months { get; } = months;
    public int Days { get; } = days;
    public long Micros { get; } = micros;
    public Int128 Span => ((Int128)Months * 30 + Days) * TemporalValue.MicrosPerDay + Micros;
    public int CompareTo(SqlInterval other) => Span.CompareTo(other.Span);
    public bool Equals(SqlInterval other) => Span == other.Span;
    public override bool Equals(object? other) => other is SqlInterval interval && Equals(interval);
    public override int GetHashCode() => Span.GetHashCode();
    public SqlInterval Add(SqlInterval other) => Create((long)Months + other.Months, (long)Days + other.Days, (Int128)Micros + other.Micros);
    public SqlInterval Subtract(SqlInterval other) => Create((long)Months - other.Months, (long)Days - other.Days, (Int128)Micros - other.Micros);
    public SqlInterval Negate() => Create(-(long)Months, -(long)Days, -(Int128)Micros);
    internal static SqlInterval Create(long months, long days, Int128 micros)
    {
        if (months < int.MinValue || months > int.MaxValue || days < int.MinValue || days > int.MaxValue || micros < long.MinValue || micros > long.MaxValue)
            throw TemporalValue.Overflow();
        return new((int)months, (int)days, (long)micros);
    }
    public SqlInterval JustifyHours()
    {
        var days = (long)Days + Micros / TemporalValue.MicrosPerDay;
        var micros = Micros % TemporalValue.MicrosPerDay;
        // Check before borrowing: this intermediate boundary is observable in SQL.
        _ = Create(Months, days, micros);
        if (days > 0 && micros < 0) { days--; micros += TemporalValue.MicrosPerDay; }
        else if (days < 0 && micros > 0) { days++; micros -= TemporalValue.MicrosPerDay; }
        return Create(Months, days, micros);
    }
    public SqlInterval JustifyDays()
    {
        var months = (long)Months + Days / 30;
        var days = Days % 30;
        _ = Create(months, days, Micros);
        if (months > 0 && days < 0) { months--; days += 30; }
        else if (months < 0 && days > 0) { months++; days -= 30; }
        return Create(months, days, Micros);
    }
    public SqlInterval Justify()
    {
        long months = Months, days = Days, micros = Micros;
        if (days > 0 && micros > 0 || days < 0 && micros < 0)
        {
            months += days / 30; days %= 30;
            _ = Create(months, days, micros);
        }
        days += micros / TemporalValue.MicrosPerDay; micros %= TemporalValue.MicrosPerDay;
        months += days / 30; days %= 30;
        _ = Create(months, days, micros);
        if (months > 0 && (days < 0 || days == 0 && micros < 0)) { months--; days += 30; }
        else if (months < 0 && (days > 0 || days == 0 && micros > 0)) { months++; days -= 30; }
        if (days > 0 && micros < 0) { days--; micros += TemporalValue.MicrosPerDay; }
        else if (days < 0 && micros > 0) { days++; micros -= TemporalValue.MicrosPerDay; }
        return Create(months, days, micros);
    }
    public override string ToString()
    {
        var output = new StringBuilder();
        var previousNegative = false;
        void Part(long value, string unit)
        {
            if (value == 0) return;
            if (output.Length != 0) output.Append(' ');
            if (previousNegative && value > 0) output.Append('+');
            output.Append(value.ToString(CultureInfo.InvariantCulture)).Append(' ').Append(unit);
            if (value != 1) output.Append('s');
            previousNegative = value < 0;
        }
        Part(Months / 12, "year"); Part(Months % 12, "mon"); Part(Days, "day");
        if (Micros != 0 || output.Length == 0)
        {
            if (output.Length != 0) output.Append(' ');
            if (Micros < 0) output.Append('-');
            else if (previousNegative) output.Append('+');
            var absolute = Micros < 0 ? (ulong)(-(Int128)Micros) : (ulong)Micros;
            output.Append((absolute / 3_600_000_000).ToString("D2", CultureInfo.InvariantCulture)).Append(':');
            output.Append((absolute / 60_000_000 % 60).ToString("D2", CultureInfo.InvariantCulture)).Append(':');
            output.Append((absolute / 1_000_000 % 60).ToString("D2", CultureInfo.InvariantCulture));
            if (absolute % 1_000_000 != 0) output.Append('.').Append((absolute % 1_000_000).ToString("D6", CultureInfo.InvariantCulture).TrimEnd('0'));
        }
        return output.ToString();
    }
}

/// <summary>Exact proleptic Gregorian arithmetic, independent of host calendar and timezone limits.</summary>
public static class TemporalValue
{
    public const long MicrosPerSecond = 1_000_000;
    public const long MicrosPerDay = 86_400_000_000;
    public const long PostgresEpochMicros = 946_684_800_000_000;
    internal static SqlException Overflow() => new("22008", "datetime field out of range");
    internal static SqlException Invalid() => new("22007", "invalid datetime syntax");
    public static bool IsInfinite(long value) => value is long.MinValue or long.MaxValue;
    public static bool IsInfiniteDate(int value) => value is int.MinValue or int.MaxValue;
    internal static long FiniteTimestamp(Int128 value)
    {
        if (value <= long.MinValue || value >= long.MaxValue) throw Overflow();
        return (long)value;
    }
    internal static int FiniteDate(long value)
    {
        if (value <= int.MinValue || value >= int.MaxValue) throw Overflow();
        return (int)value;
    }
    internal static long FloorDiv(long value, long divisor) => value / divisor - (value % divisor < 0 ? 1 : 0);
    internal static long FloorMod(long value, long divisor) { var remainder = value % divisor; return remainder < 0 ? remainder + divisor : remainder; }
    public static int DaysInMonth(long year, int month) => month switch
    {
        2 => year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) ? 29 : 28,
        4 or 6 or 9 or 11 => 30,
        >= 1 and <= 12 => 31,
        _ => throw Overflow()
    };
    internal static void ValidateCivil(long year, long month, long day)
    {
        if (month < 1 || month > 12 || day < 1 || day > DaysInMonth(year, (int)month)) throw Overflow();
    }
    public static long DaysFromCivil(long year, int month, int day)
    {
        ValidateCivil(year, month, day);
        year -= month <= 2 ? 1 : 0;
        var era = FloorDiv(year, 400);
        var yearOfEra = year - era * 400;
        var dayOfYear = (153 * (month + (month > 2 ? -3 : 9)) + 2) / 5 + day - 1;
        return era * 146097 + yearOfEra * 365 + yearOfEra / 4 - yearOfEra / 100 + dayOfYear - 719468;
    }
    public static (long Year, int Month, int Day) CivilFromDays(long days)
    {
        var adjusted = days + 719468;
        var era = FloorDiv(adjusted, 146097);
        var dayOfEra = adjusted - era * 146097;
        var yearOfEra = (dayOfEra - dayOfEra / 1460 + dayOfEra / 36524 - dayOfEra / 146096) / 365;
        var year = yearOfEra + era * 400;
        var dayOfYear = dayOfEra - (365 * yearOfEra + yearOfEra / 4 - yearOfEra / 100);
        var monthPrime = (5 * dayOfYear + 2) / 153;
        var day = dayOfYear - (153 * monthPrime + 2) / 5 + 1;
        var month = monthPrime + (monthPrime < 10 ? 3 : -9);
        return (year + (month <= 2 ? 1 : 0), (int)month, (int)day);
    }
    public static int MakeDate(int year, int month, int day)
    {
        if (year == 0) throw Overflow();
        return FiniteDate(DaysFromCivil(year < 0 ? (long)year + 1 : year, month, day));
    }
    public static long DateToTimestamp(int days) => IsInfiniteDate(days) ? days < 0 ? long.MinValue : long.MaxValue : FiniteTimestamp((Int128)days * MicrosPerDay);
    public static int TimestampToDate(long micros) => IsInfinite(micros) ? micros < 0 ? int.MinValue : int.MaxValue : FiniteDate(FloorDiv(micros, MicrosPerDay));
    public static int AddDays(int date, long days)
    {
        if (IsInfiniteDate(date)) return date;
        var result = (Int128)date + days;
        if (result <= int.MinValue || result >= int.MaxValue) throw Overflow();
        return (int)result;
    }
    public static int SubtractDates(int left, int right)
    {
        if (IsInfiniteDate(left) || IsInfiniteDate(right)) throw Overflow();
        var result = (long)left - right;
        if (result < int.MinValue || result > int.MaxValue) throw Overflow();
        return (int)result;
    }
    public static long RoundTimestamp(long micros, int precision)
    {
        if (precision < 0) throw Overflow();
        if (precision >= 6 || IsInfinite(micros)) return micros;
        long step = 1;
        for (var i = precision; i < 6; i++) step *= 10;
        var relative = (Int128)micros - PostgresEpochMicros;
        var rounded = relative < 0 ? -((-relative + step / 2) / step * step) : (relative + step / 2) / step * step;
        return FiniteTimestamp(rounded + PostgresEpochMicros);
    }
    public static long ShiftTimestamp(long micros, SqlInterval interval, bool subtract = false)
    {
        if (IsInfinite(micros)) return micros;
        var sign = subtract ? -1L : 1L;
        if (interval.Months != 0)
        {
            var civil = CivilFromDays(FloorDiv(micros, MicrosPerDay));
            var monthIndex = civil.Year * 12 + civil.Month - 1 + sign * interval.Months;
            var year = FloorDiv(monthIndex, 12);
            var month = (int)FloorMod(monthIndex, 12) + 1;
            micros = FiniteTimestamp((Int128)DaysFromCivil(year, month, Math.Min(civil.Day, DaysInMonth(year, month))) * MicrosPerDay + FloorMod(micros, MicrosPerDay));
        }
        micros = FiniteTimestamp((Int128)micros + sign * (Int128)interval.Days * MicrosPerDay);
        return FiniteTimestamp((Int128)micros + sign * (Int128)interval.Micros);
    }
    public static SqlInterval SubtractTimestamps(long left, long right)
    {
        if (IsInfinite(left) || IsInfinite(right)) throw Overflow();
        var difference = (Int128)left - right;
        if (difference < long.MinValue || difference > long.MaxValue) throw Overflow();
        return SqlInterval.Create(0, (long)(difference / MicrosPerDay), difference % MicrosPerDay);
    }
}
