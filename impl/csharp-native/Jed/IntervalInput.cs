using System.Globalization;

namespace Jed;

// PG-style fields are scanned once and folded right-to-left. Fractions stay exact
// to the shared 25-digit bound; all observable range checks precede duplicate checks.
public static class IntervalInput
{
    private enum Unit { Millennium, Century, Decade, Year, Month, Week, Day, Hour, Minute, Second, Millisecond, Microsecond, None }
    private readonly record struct Amount(long Whole, Int128 Fraction, Int128 Denominator, bool HasFraction)
    {
        internal Amount Negate() => new(-Whole, -Fraction, Denominator, HasFraction);
    }
    public static SqlInterval Parse(string input, byte qualifier = 0, int? precision = null)
    {
        SqlInterval result;
        try { result = ParseFields(input, qualifier); }
        catch (SqlException error) when (error.SqlState == "22007") { result = ParseIso(input); }
        return Adjust(result, qualifier, precision);
    }
    public static SqlInterval Adjust(SqlInterval interval, byte qualifier = 0, int? precision = null)
    {
        var fields = Qualifier(qualifier);
        var months = interval.Months; var days = interval.Days; var micros = interval.Micros;
        switch (fields.Least)
        {
            case "year": months = months / 12 * 12; days = 0; micros = 0; break;
            case "month": days = 0; micros = 0; break;
            case "day": micros = 0; break;
            case "hour": micros = micros / 3_600_000_000 * 3_600_000_000; break;
            case "minute": micros = micros / 60_000_000 * 60_000_000; break;
        }
        if (precision < 0) throw new SqlException(SqlStates.InvalidParameterValue, "negative interval precision");
        if (precision is >= 0 and < 6)
        {
            long step = 1;
            for (var i = precision.Value; i < 6; i++) step *= 10;
            var shifted = (Int128)micros + (micros < 0 ? -step / 2 : step / 2);
            if (shifted < long.MinValue || shifted > long.MaxValue) throw TemporalValue.Overflow();
            micros = (long)(shifted / step * step);
        }
        return new(months, days, micros);
    }
    private static (byte Code, string Name, string Least, bool MinuteSecond) Qualifier(byte code)
    {
        if (code == 0) return (0, "", "second", false);
        var found = DatetimeUnits.Qualifiers.FirstOrDefault(q => q.Code == code);
        return found.Code == 0 ? throw new SqlException(SqlStates.InvalidParameterValue, "invalid interval qualifier") : found;
    }
    private static bool White(char c) => c is ' ' or '\t' or '\n' or '\r' or '\f' or '\v';
    private static bool Digit(char c) => c is >= '0' and <= '9';
    private static bool Letter(char c) => c is >= 'A' and <= 'Z' or >= 'a' and <= 'z';
    private static List<string> Scan(string input)
    {
        var fields = new List<string>();
        var position = 0; var bytes = 0;
        while (position < input.Length)
        {
            var c = input[position];
            if (c > 127 || c < 32 && !White(c)) throw TemporalValue.Invalid();
            if (White(c) || !Letter(c) && !Digit(c) && c is not ('.' or '+' or '-')) { position++; continue; }
            var start = position;
            string field;
            if (Letter(c))
            {
                while (position < input.Length && !White(input[position]) && input[position] is not (';' or ',' or '@')) position++;
                field = TextEncoding.FoldName(input[start..position]);
            }
            else
            {
                var sign = "";
                if (c is '+' or '-')
                {
                    sign = c.ToString(); position++;
                    while (position < input.Length && White(input[position])) position++;
                }
                start = position;
                while (position < input.Length && (Digit(input[position]) || input[position] is '.' or ':' or '-')) position++;
                if (position == start) throw TemporalValue.Invalid();
                field = sign + input[start..position];
            }
            bytes += field.Length + 1;
            if (bytes > 256 || fields.Count == 25) throw TemporalValue.Invalid();
            fields.Add(field);
        }
        return fields;
    }
    private static SqlInterval ParseFields(string input, byte qualifier)
    {
        var fields = Scan(input);
        if (fields.Count == 0) throw TemporalValue.Invalid();
        var ago = fields[^1] == "ago";
        if (ago) fields.RemoveAt(fields.Count - 1);
        var fieldsInfo = Qualifier(qualifier);
        var unit = ago ? Unit.None : ReadUnit(fieldsInfo.Least);
        var needsNumber = false;
        var mask = 0;
        var result = new SqlInterval();
        for (var index = fields.Count - 1; index >= 0; index--)
        {
            var field = fields[index];
            if (Letter(field[0]))
            {
                if (field is "at" or "on") { unit = Unit.Second; needsNumber = false; continue; }
                if (needsNumber) throw TemporalValue.Invalid();
                unit = ReadUnit(field); needsNumber = true; continue;
            }
            int claims;
            if (field.Contains(':'))
            {
                result = new(result.Months, result.Days, Time(field, fieldsInfo.MinuteSecond));
                claims = Bits(Unit.Hour, Unit.Minute, Unit.Second, Unit.Millisecond, Unit.Microsecond);
                unit = Unit.Day;
            }
            else
            {
                var firstDigit = field[0] is '+' or '-' ? 1 : 0;
                var hyphen = field.IndexOf('-', firstDigit);
                if (hyphen >= 0)
                {
                    var years = ReadAmount(field[..hyphen]);
                    if (years.HasFraction) throw TemporalValue.Invalid();
                    var monthText = field[(hyphen + 1)..];
                    var months = monthText.Length == 0 ? new Amount(0, 0, 1, false) : ReadAmount(monthText);
                    if (months.HasFraction || months.Whole < 0 || months.Whole > 11) throw TemporalValue.Overflow();
                    var total = (Int128)years.Whole * 12 + (field[0] == '-' ? -months.Whole : months.Whole);
                    if (total < int.MinValue || total > int.MaxValue) throw TemporalValue.Overflow();
                    result = SqlInterval.Create((long)result.Months + (long)total, result.Days, result.Micros);
                    claims = Bits(Unit.Month); unit = Unit.Month;
                }
                else
                {
                    if (unit == Unit.None) throw TemporalValue.Invalid();
                    var amount = ReadAmount(field);
                    result = Add(result, unit, amount);
                    claims = unit == Unit.Second && amount.HasFraction ? Bits(Unit.Second, Unit.Millisecond, Unit.Microsecond) : Bits(unit);
                    if (unit == Unit.Hour) unit = Unit.Day;
                }
            }
            if ((mask & claims) != 0) throw TemporalValue.Invalid();
            mask |= claims; needsNumber = false;
        }
        if (needsNumber || mask == 0) throw TemporalValue.Invalid();
        return ago ? result.Negate() : result;
    }
    private static int Bits(params Unit[] units)
    {
        var bits = 0;
        foreach (var unit in units) bits |= 1 << (int)unit;
        return bits;
    }
    private static Unit ReadUnit(string word) => (word.Length > 10 ? word[..10] : word) switch
    {
        "millennium" or "millennia" or "mil" or "mils" => Unit.Millennium,
        "century" or "centuries" or "cent" or "c" => Unit.Century,
        "decade" or "decades" or "dec" or "decs" => Unit.Decade,
        "year" or "years" or "yr" or "yrs" or "y" => Unit.Year,
        "month" or "months" or "mon" or "mons" => Unit.Month,
        "week" or "weeks" or "w" => Unit.Week,
        "day" or "days" or "d" => Unit.Day,
        "hour" or "hours" or "hr" or "hrs" or "h" => Unit.Hour,
        "minute" or "minutes" or "min" or "mins" or "mm" => Unit.Minute,
        "second" or "seconds" or "sec" or "secs" or "s" => Unit.Second,
        "millisecon" or "msec" or "msecs" or "msecond" or "mseconds" or "ms" => Unit.Millisecond,
        "microsecon" or "usec" or "usecs" or "usecond" or "useconds" or "us" => Unit.Microsecond,
        _ => throw TemporalValue.Invalid()
    };
    private static Amount ReadAmount(string text)
    {
        var position = 0;
        var negative = text.StartsWith('-');
        if (text.StartsWith('-') || text.StartsWith('+')) position++;
        var start = position;
        Int128 whole = 0, fraction = 0, denominator = 1;
        while (position < text.Length && Digit(text[position]))
        {
            whole = whole * 10 + text[position++] - '0';
            if (whole > (Int128)long.MaxValue + (negative ? 1 : 0)) throw TemporalValue.Overflow();
        }
        var digits = position - start;
        var nonzero = false;
        if (position < text.Length && text[position] == '.')
        {
            position++; var fractionalDigits = 0;
            while (position < text.Length && Digit(text[position]))
            {
                var digit = text[position++] - '0'; nonzero |= digit != 0; digits++;
                if (fractionalDigits++ < 25) { fraction = fraction * 10 + digit; denominator *= 10; }
            }
        }
        if (digits == 0 || position != text.Length) throw TemporalValue.Invalid();
        return new((long)(negative ? -whole : whole), negative ? -fraction : fraction, denominator, nonzero);
    }
    private static Int128 Rounded(Int128 numerator, Int128 denominator, bool even = false)
    {
        var negative = numerator < 0;
        if (negative) numerator = -numerator;
        var whole = numerator / denominator;
        var remainder = numerator % denominator;
        if (remainder > denominator / 2 || remainder * 2 == denominator && (!even || (whole & 1) != 0)) whole++;
        return negative ? -whole : whole;
    }
    private static SqlInterval Add(SqlInterval value, Unit unit, Amount amount)
    {
        Int128 months = value.Months, days = value.Days, micros = value.Micros;
        if (unit <= Unit.Year)
        {
            var factor = unit switch { Unit.Millennium => 12000, Unit.Century => 1200, Unit.Decade => 120, _ => 12 };
            months += (Int128)amount.Whole * factor + Rounded(amount.Fraction * factor, amount.Denominator, even: true);
        }
        else if (unit == Unit.Month)
        {
            months += amount.Whole;
            var fractionalDays = amount.Fraction * 30;
            days += fractionalDays / amount.Denominator;
            micros += Rounded(fractionalDays % amount.Denominator * TemporalValue.MicrosPerDay, amount.Denominator);
        }
        else if (unit is Unit.Week or Unit.Day)
        {
            var factor = unit == Unit.Week ? 7 : 1;
            days += (Int128)amount.Whole * factor + amount.Fraction * factor / amount.Denominator;
            micros += Rounded(amount.Fraction * factor % amount.Denominator * TemporalValue.MicrosPerDay, amount.Denominator);
        }
        else
        {
            var factor = unit switch { Unit.Hour => 3_600_000_000L, Unit.Minute => 60_000_000L, Unit.Second => 1_000_000L, Unit.Millisecond => 1000L, Unit.Microsecond => 1L, _ => throw TemporalValue.Invalid() };
            micros += (Int128)amount.Whole * factor + Rounded(amount.Fraction * factor, amount.Denominator);
        }
        if (months < int.MinValue || months > int.MaxValue || days < int.MinValue || days > int.MaxValue) throw TemporalValue.Overflow();
        return SqlInterval.Create((long)months, (long)days, micros);
    }
    private static long Time(string text, bool minuteSecond)
    {
        var negative = text.StartsWith('-');
        if (text.StartsWith('-') || text.StartsWith('+')) text = text[1..];
        var parts = text.Split(':');
        if (parts.Length is not (2 or 3)) throw TemporalValue.Invalid();
        var numbers = parts.Select(ReadAmount).ToArray();
        if (numbers.Any(value => value.Whole < 0 || value.Fraction < 0)) throw TemporalValue.Overflow();
        for (var i = 0; i + 1 < numbers.Length; i++) if (numbers[i].HasFraction || parts[i].Contains('.')) throw TemporalValue.Invalid();
        Int128 total;
        if (parts.Length == 2 && (parts[1].Contains('.') || minuteSecond))
        {
            if (numbers[0].Whole > 59 || numbers[1].Whole > 60) throw TemporalValue.Overflow();
            total = (Int128)numbers[0].Whole * 60_000_000 + (Int128)numbers[1].Whole * 1_000_000 + Rounded(numbers[1].Fraction * 1_000_000, numbers[1].Denominator);
        }
        else
        {
            if (numbers[1].Whole > 59 || parts.Length == 3 && numbers[2].Whole > 60) throw TemporalValue.Overflow();
            total = (Int128)numbers[0].Whole * 3_600_000_000 + (Int128)numbers[1].Whole * 60_000_000;
            if (parts.Length == 3) total += (Int128)numbers[2].Whole * 1_000_000 + Rounded(numbers[2].Fraction * 1_000_000, numbers[2].Denominator);
        }
        return SqlInterval.Create(0, 0, negative ? -total : total).Micros;
    }
    private static SqlInterval ParseIso(string text)
    {
        if (text.Length < 2 || text[0] != 'P') throw TemporalValue.Invalid();
        var position = 1;
        var time = false; var haveField = false;
        var result = new SqlInterval();
        while (position < text.Length)
        {
            if (text[position] == 'T') { time = true; haveField = false; position++; continue; }
            var start = position;
            var amount = IsoNumber(text, ref position);
            var end = position;
            var unitChar = position < text.Length ? text[position++] : '\0';
            var unit = (time, unitChar) switch
            {
                (false, 'Y') => Unit.Year, (false, 'M') => Unit.Month, (false, 'W') => Unit.Week, (false, 'D') => Unit.Day,
                (true, 'H') => Unit.Hour, (true, 'M') => Unit.Minute, (true, 'S') => Unit.Second,
                _ => Unit.None
            };
            if (unit != Unit.None) { result = Add(result, unit, amount); haveField = true; continue; }
            if (haveField || (time ? unitChar is not ('\0' or ':') : unitChar is not ('\0' or 'T' or '-'))) throw TemporalValue.Invalid();
            var digits = 0;
            for (var i = start; i < end && Digit(text[i]); i++) digits++;
            if (!time && digits == 8 && unitChar != '-')
            {
                result = Add(result, Unit.Year, new(amount.Whole / 10000, 0, 1, false));
                result = Add(result, Unit.Month, new(amount.Whole / 100 % 100, 0, 1, false));
                result = Add(result, Unit.Day, amount with { Whole = amount.Whole % 100 });
                if (unitChar == '\0') return result;
                time = true; haveField = false; continue;
            }
            if (time && digits == 6 && unitChar == '\0')
            {
                result = Add(result, Unit.Hour, new(amount.Whole / 10000, 0, 1, false));
                result = Add(result, Unit.Minute, new(amount.Whole / 100 % 100, 0, 1, false));
                result = Add(result, Unit.Second, new(amount.Whole % 100, 0, 1, false));
                return Add(result, Unit.Microsecond, amount with { Whole = 0 });
            }
            result = Add(result, time ? Unit.Hour : Unit.Year, amount);
            if (unitChar == '\0') return result;
            if (unitChar == 'T') { time = true; haveField = false; continue; }
            result = Add(result, time ? Unit.Minute : Unit.Month, IsoNumber(text, ref position));
            if (position == text.Length) return result;
            if (!time && text[position] == 'T') continue;
            if (text[position++] != (time ? ':' : '-')) throw TemporalValue.Invalid();
            result = Add(result, time ? Unit.Second : Unit.Day, IsoNumber(text, ref position));
            if (position == text.Length) return result;
            if (!time && text[position] == 'T') continue;
            throw TemporalValue.Invalid();
        }
        return result;
    }
    private static Amount IsoNumber(string text, ref int position)
    {
        var start = position;
        if (position < text.Length && text[position] == '-') position++;
        var magnitudeStart = position;
        if (text.AsSpan(position).StartsWith("inf", StringComparison.OrdinalIgnoreCase) || text.AsSpan(position).StartsWith("nan", StringComparison.OrdinalIgnoreCase)) throw TemporalValue.Overflow();
        if (position + 1 < text.Length && text[position] == '0' && text[position + 1] is 'x' or 'X') return IsoHex(text, start, ref position);
        var nonzero = false; var digits = 0;
        while (position < text.Length && Digit(text[position])) { nonzero |= text[position] != '0'; digits++; position++; }
        if (position < text.Length && text[position] == '.')
        {
            position++;
            while (position < text.Length && Digit(text[position])) { nonzero |= text[position] != '0'; digits++; position++; }
        }
        if (digits == 0) throw TemporalValue.Invalid();
        if (position < text.Length && text[position] is 'e' or 'E')
        {
            position++;
            if (position < text.Length && text[position] is '+' or '-') position++;
            var exponentStart = position;
            while (position < text.Length && Digit(text[position])) position++;
            if (position == exponentStart) throw TemporalValue.Invalid();
        }
        var spelling = text[start..position];
        if (!double.TryParse(spelling, NumberStyles.Float, CultureInfo.InvariantCulture, out var gate) || double.IsInfinity(gate) || gate == 0 && nonzero) throw TemporalValue.Invalid();
        if (Math.Abs(gate) > 1e15) throw TemporalValue.Overflow();
        if (!nonzero) return new(0, 0, 1, false);
        ExactDecimal number;
        try { number = ExactDecimal.Parse(spelling); }
        catch (SqlException) { throw TemporalValue.Invalid(); }
        if (Math.Abs(gate) < 2.2250738585072014e-308 && !number.Equals(FloatValue.ToDecimal(gate))) throw TemporalValue.Invalid();
        _ = magnitudeStart;
        return ReadAmount(number.ToString());
    }
    private static Amount IsoHex(string text, int start, ref int position)
    {
        position += 2;
        UInt128 mantissa = 0;
        var digits = 0; var fractionDigits = 0; var point = false;
        while (position < text.Length)
        {
            var c = text[position];
            var digit = c is >= '0' and <= '9' ? c - '0' : c is >= 'a' and <= 'f' ? c - 'a' + 10 : c is >= 'A' and <= 'F' ? c - 'A' + 10 : -1;
            if (digit >= 0)
            {
                if (digits >= 30) throw TemporalValue.Invalid();
                mantissa = mantissa * 16 + (uint)digit; digits++; if (point) fractionDigits++; position++;
            }
            else if (c == '.' && !point) { point = true; position++; }
            else break;
        }
        if (digits == 0) throw TemporalValue.Invalid();
        var exponent = -fractionDigits * 4;
        if (position < text.Length && text[position] is 'p' or 'P')
        {
            position++; var negative = position < text.Length && text[position] == '-';
            if (position < text.Length && text[position] is '+' or '-') position++;
            var exponentStart = position; var parsed = 0;
            while (position < text.Length && Digit(text[position])) parsed = Math.Min(100000, parsed * 10 + text[position++] - '0');
            if (position == exponentStart) throw TemporalValue.Invalid();
            exponent += negative ? -parsed : parsed;
        }
        if (mantissa == 0) return new(0, 0, 1, false);
        while ((mantissa & 1) == 0) { mantissa >>= 1; exponent++; }
        var bits = 0; for (var copy = mantissa; copy != 0; copy >>= 1) bits++;
        if (exponent + bits > 1024 || exponent < -1074) throw TemporalValue.Invalid();
        if (exponent + bits > 50) throw TemporalValue.Overflow();
        Int128 whole = 0, fraction = 0, denominator = 1;
        if (exponent >= 0) whole = (Int128)mantissa << exponent;
        else
        {
            var shift = -exponent;
            if (shift < 128) whole = (Int128)(mantissa >> shift);
            if (shift <= 64)
            {
                denominator = (Int128)1 << shift;
                fraction = (Int128)(mantissa & (((UInt128)1 << shift) - 1));
            }
            else if (shift < 192)
            {
                denominator = (Int128)1 << 64;
                fraction = shift - 64 < 128 ? (Int128)(mantissa >> (shift - 64)) : 0;
            }
        }
        if (whole > 1_000_000_000_000_000L) throw TemporalValue.Overflow();
        return text[start] == '-' ? new(-(long)whole, -fraction, denominator, exponent < 0) : new((long)whole, fraction, denominator, exponent < 0);
    }
}
