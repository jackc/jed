using System.Globalization;

namespace Jed;

/// <summary>Locale-independent temporal text input and output. Session clocks and named zones are supplied above this layer.</summary>
public static class TemporalInput
{
    private readonly record struct Fields(long Year, int Month, int Day, long Hour, long Minute, long Second, long Micros, int Offset, bool HasOffset);
    internal static string Trim(string text) => text.Trim(' ', '\t', '\n', '\f', '\r');
    private static bool Infinity(string text, out long value)
    {
        value = 0;
        if (text.Equals("infinity", StringComparison.OrdinalIgnoreCase) || text.Equals("+infinity", StringComparison.OrdinalIgnoreCase)) value = long.MaxValue;
        else if (text.Equals("-infinity", StringComparison.OrdinalIgnoreCase)) value = long.MinValue;
        else return false;
        return true;
    }
    public static int ParseDate(string text)
    {
        text = Trim(text);
        if (Infinity(text, out var infinity)) return infinity < 0 ? int.MinValue : int.MaxValue;
        if (text.Equals("epoch", StringComparison.OrdinalIgnoreCase)) return 0;
        var fields = ParseFields(text, 9_999_999);
        return TemporalValue.FiniteDate(TemporalValue.DaysFromCivil(fields.Year, fields.Month, fields.Day));
    }
    public static long ParseTimestamp(string text, bool withZone = false, int defaultOffsetSeconds = 0)
    {
        var parsed = ParseTimestampInput(text);
        if (parsed.Infinite) return parsed.Micros;
        return TemporalValue.FiniteTimestamp((Int128)parsed.Micros - (withZone ? (long)(parsed.HasOffset ? parsed.OffsetSeconds : defaultOffsetSeconds) * TemporalValue.MicrosPerSecond : 0));
    }
    /// <summary>Returns a wall clock and explicit offset separately so named-zone resolution and charging occur only when required.</summary>
    public static (long Micros, bool HasOffset, int OffsetSeconds, bool Infinite) ParseTimestampInput(string text)
    {
        text = Trim(text);
        if (Infinity(text, out var infinity)) return (infinity, false, 0, true);
        var fields = ParseFields(text, 999_999);
        var value = (Int128)TemporalValue.DaysFromCivil(fields.Year, fields.Month, fields.Day) * TemporalValue.MicrosPerDay
            + ((Int128)fields.Hour * 3600 + fields.Minute * 60 + fields.Second) * TemporalValue.MicrosPerSecond + fields.Micros;
        // A wall reading may coincide with a sentinel before its offset is applied.
        if (value < long.MinValue || value > long.MaxValue) throw TemporalValue.Overflow();
        return ((long)value, fields.HasOffset, fields.Offset, false);
    }
    private static Fields ParseFields(string text, long maximumYear)
    {
        var bc = text.EndsWith(" bc", StringComparison.OrdinalIgnoreCase);
        if (bc || text.EndsWith(" ad", StringComparison.OrdinalIgnoreCase)) text = Trim(text[..^3]);
        var position = 0;
        var year = Number(text, ref position);
        Expect(text, ref position, '-');
        var month = Number(text, ref position);
        Expect(text, ref position, '-');
        var day = Number(text, ref position);
        long hour = 0, minute = 0, second = 0, micros = 0;
        if (position < text.Length && text[position] is ' ' or 'T' or 't')
        {
            position++;
            hour = Number(text, ref position);
            Expect(text, ref position, ':');
            minute = Number(text, ref position);
            if (position < text.Length && text[position] == ':')
            {
                position++;
                second = Number(text, ref position);
                if (position < text.Length && text[position] == '.')
                {
                    position++;
                    var start = position;
                    while (position < text.Length && text[position] is >= '0' and <= '9') position++;
                    if (position == start) throw TemporalValue.Invalid();
                    for (var i = 0; i < 6; i++) micros = micros * 10 + (i < position - start ? text[start + i] - '0' : 0);
                    if (position - start > 6 && text[start + 6] >= '5') micros++;
                }
            }
        }
        var offset = 0;
        var hasOffset = position < text.Length;
        if (hasOffset)
        {
            var signChar = text[position++];
            if (signChar is not ('Z' or 'z'))
            {
                if (signChar is not ('+' or '-')) throw TemporalValue.Invalid();
                var offsetHours = Number(text, ref position);
                long offsetMinutes = 0, offsetSeconds = 0;
                if (position < text.Length && text[position] == ':')
                {
                    position++;
                    offsetMinutes = Number(text, ref position);
                    if (position < text.Length && text[position] == ':') { position++; offsetSeconds = Number(text, ref position); }
                }
                if (offsetHours > 15 || offsetMinutes > 59 || offsetSeconds > 59) throw TemporalValue.Overflow();
                offset = (int)(offsetHours * 3600 + offsetMinutes * 60 + offsetSeconds) * (signChar == '-' ? -1 : 1);
            }
        }
        if (position != text.Length) throw TemporalValue.Invalid();
        if (year < 1 || year > maximumYear) throw TemporalValue.Overflow();
        if (bc) year = 1 - year;
        TemporalValue.ValidateCivil(year, month, day);
        if (hour > 23 && !(hour == 24 && minute == 0 && second == 0 && micros == 0) || minute > 59 || second > 59) throw TemporalValue.Overflow();
        return new(year, (int)month, (int)day, hour, minute, second, micros, offset, hasOffset);
    }
    private static long Number(string text, ref int position)
    {
        var start = position;
        long value = 0;
        while (position < text.Length && text[position] is >= '0' and <= '9')
        {
            var digit = text[position++] - '0';
            if (value > (long.MaxValue - digit) / 10) throw TemporalValue.Overflow();
            value = value * 10 + digit;
        }
        if (start == position) throw TemporalValue.Invalid();
        return value;
    }
    private static void Expect(string text, ref int position, char expected)
    {
        if (position == text.Length || text[position++] != expected) throw TemporalValue.Invalid();
    }
    private static string CivilDate(long year, int month, int day) => string.Create(CultureInfo.InvariantCulture, $"{(year <= 0 ? 1 - year : year):D4}-{month:D2}-{day:D2}");
    public static string RenderDate(int days)
    {
        if (TemporalValue.IsInfiniteDate(days)) return days < 0 ? "-infinity" : "infinity";
        var civil = TemporalValue.CivilFromDays(days);
        return CivilDate(civil.Year, civil.Month, civil.Day) + (civil.Year <= 0 ? " BC" : "");
    }
    public static string RenderTimestamp(long micros, bool withZone = false, int offsetSeconds = 0)
    {
        if (TemporalValue.IsInfinite(micros)) return micros < 0 ? "-infinity" : "infinity";
        if (withZone) micros = TemporalValue.FiniteTimestamp((Int128)micros + (long)offsetSeconds * TemporalValue.MicrosPerSecond);
        var civil = TemporalValue.CivilFromDays(TemporalValue.FloorDiv(micros, TemporalValue.MicrosPerDay));
        var remainder = TemporalValue.FloorMod(micros, TemporalValue.MicrosPerDay);
        var seconds = remainder / TemporalValue.MicrosPerSecond;
        var result = CivilDate(civil.Year, civil.Month, civil.Day) + string.Create(CultureInfo.InvariantCulture, $" {seconds / 3600:D2}:{seconds / 60 % 60:D2}:{seconds % 60:D2}");
        if (remainder % TemporalValue.MicrosPerSecond != 0) result += "." + (remainder % TemporalValue.MicrosPerSecond).ToString("D6", CultureInfo.InvariantCulture).TrimEnd('0');
        if (withZone)
        {
            var offset = Math.Abs((long)offsetSeconds);
            result += (offsetSeconds < 0 ? "-" : "+") + (offset / 3600).ToString("D2", CultureInfo.InvariantCulture);
            if (offset % 3600 != 0) result += ":" + (offset / 60 % 60).ToString("D2", CultureInfo.InvariantCulture);
            if (offset % 60 != 0) result += ":" + (offset % 60).ToString("D2", CultureInfo.InvariantCulture);
        }
        return result + (civil.Year <= 0 ? " BC" : "");
    }
}
