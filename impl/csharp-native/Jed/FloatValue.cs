using System.Globalization;

namespace Jed;

// IEEE operations stay at the selected width. Exact dyadic decimal expansion is
// used only for specified casts and PostgreSQL's strict-shortest display interval.
internal static class FloatValue
{
    private const NumberStyles InputStyle = NumberStyles.AllowLeadingSign | NumberStyles.AllowDecimalPoint | NumberStyles.AllowExponent;
    internal const double Pi = 3.141592653589793238462643383279502884;
    private const double RadiansPerDegree = 0.017453292519943295769236907684886;

    internal static double Parse(string input, bool single = false)
    {
        var text = input.AsSpan();
        while (!text.IsEmpty && White(text[0])) text = text[1..];
        while (!text.IsEmpty && White(text[^1])) text = text[..^1];
        var spelling = TextEncoding.FoldName(text.ToString());
        var unsigned = spelling.StartsWith('+') || spelling.StartsWith('-') ? spelling[1..] : spelling;
        if (unsigned == "nan") return double.NaN;
        if (unsigned is "inf" or "infinity") return spelling.StartsWith('-') ? double.NegativeInfinity : double.PositiveInfinity;
        var position = 0; var nonzero = false; var digits = 0;
        if (position < text.Length && text[position] is '+' or '-') position++;
        while (position < text.Length && Digit(text[position])) { nonzero |= text[position++] != '0'; digits++; }
        if (position < text.Length && text[position] == '.')
        {
            position++;
            while (position < text.Length && Digit(text[position])) { nonzero |= text[position++] != '0'; digits++; }
        }
        if (digits == 0) throw Invalid();
        if (position < text.Length && text[position] is 'e' or 'E')
        {
            position++;
            if (position < text.Length && text[position] is '+' or '-') position++;
            var exponentStart = position;
            while (position < text.Length && Digit(text[position])) position++;
            if (position == exponentStart) throw Invalid();
        }
        if (position != text.Length) throw Invalid();
        double value;
        if (single)
        {
            if (!float.TryParse(text, InputStyle, CultureInfo.InvariantCulture, out var parsed)) throw Invalid();
            value = parsed; // Single.TryParse rounds directly to binary32.
        }
        else
        {
            if (!double.TryParse(text, InputStyle, CultureInfo.InvariantCulture, out value)) throw Invalid();
        }
        if (double.IsInfinity(value)) throw Overflow();
        if (value == 0 && nonzero) throw Underflow();
        return value;
    }

    internal static double FromInteger(long value, bool single = false) => Parse(value.ToString(CultureInfo.InvariantCulture), single);
    internal static double Narrow(double value)
    {
        var result = (float)value;
        if (double.IsFinite(value) && float.IsInfinity(result)) throw Overflow();
        if (value != 0 && result == 0) throw Underflow();
        return result;
    }
    internal static long ToInteger(double value)
    {
        if (!double.IsFinite(value)) throw Overflow();
        var rounded = Math.Round(value, MidpointRounding.AwayFromZero);
        if (rounded < -9223372036854775808.0 || rounded >= 9223372036854775808.0) throw Overflow();
        return (long)rounded;
    }
    internal static int Compare(double first, double second)
    {
        if (double.IsNaN(first)) return double.IsNaN(second) ? 0 : 1;
        if (double.IsNaN(second)) return -1;
        return first < second ? -1 : first > second ? 1 : 0;
    }
    internal static double Binary(string operation, double first, double second, bool single = false)
    {
        if (operation is "/" or "%" && second == 0 && !double.IsNaN(first))
            throw new SqlException(SqlStates.DivisionByZero, "division by zero");
        double result;
        if (single)
        {
            var a = (float)first; var b = (float)second;
            result = operation switch { "+" => a + b, "-" => a - b, "*" => a * b, "/" => a / b, "%" => a % b, _ => throw new ArgumentException("unknown float operator") };
        }
        else result = operation switch { "+" => first + second, "-" => first - second, "*" => first * second, "/" => first / second, "%" => first % second, _ => throw new ArgumentException("unknown float operator") };
        if (double.IsInfinity(result) && double.IsFinite(first) && double.IsFinite(second)) throw Overflow();
        if (result == 0 && (operation == "*" && first != 0 && second != 0 || operation == "/" && first != 0 && double.IsFinite(second)))
            throw Underflow();
        return result;
    }

    internal static double Unary(string function, double value)
    {
        if (double.IsNaN(value)) return value;
        switch (function)
        {
            case "abs": return Math.Abs(value);
            case "ceil": case "ceiling": return Math.Ceiling(value);
            case "floor": return Math.Floor(value);
            case "trunc": return Math.Truncate(value);
            case "round": return Round(value);
            case "sign": return value == 0 ? 0 : value < 0 ? -1 : 1;
            case "sqrt":
                if (value < 0) throw new SqlException(SqlStates.InvalidArgumentForPowerFunction, "cannot take square root of a negative number");
                return Math.Sqrt(value);
            case "ln": case "log": case "log10":
                if (value <= 0) throw new SqlException(SqlStates.InvalidArgumentForLog, "cannot take logarithm of zero or a negative number");
                return function == "ln" ? Math.Log(value) : Math.Log10(value);
            case "exp":
            {
                var result = Math.Exp(value);
                if (double.IsFinite(value)) { if (double.IsInfinity(result)) throw Overflow(); if (result == 0) throw Underflow(); }
                return result;
            }
            case "radians": return Binary("*", value, RadiansPerDegree);
            case "degrees": return Binary("/", value, RadiansPerDegree);
            case "cbrt": return Math.Cbrt(value);
            case "sin": case "cos": case "tan": case "cot":
                if (double.IsInfinity(value)) throw OutOfRange();
                return function switch { "sin" => Math.Sin(value), "cos" => Math.Cos(value), "tan" => Math.Tan(value), _ => 1 / Math.Tan(value) };
            case "asin": case "acos":
                if (value is < -1 or > 1) throw OutOfRange();
                return function == "asin" ? Math.Asin(value) : Math.Acos(value);
            case "atan": return Math.Atan(value);
            case "sinh": return Math.Sinh(value);
            case "cosh": return Math.Cosh(value);
            case "tanh": return Math.Tanh(value);
            case "asinh": return Math.Asinh(value);
            case "acosh":
                if (value < 1) throw OutOfRange();
                return Math.Acosh(value);
            case "atanh":
                if (value is < -1 or > 1) throw OutOfRange();
                return Math.Atanh(value);
            default: throw new ArgumentException("unknown float function", nameof(function));
        }
    }
    internal static double Atan2(double y, double x) => Math.Atan2(y, x);
    internal static double Power(double basis, double exponent)
    {
        if (exponent == 0 || basis == 1) return 1;
        if (double.IsNaN(basis) || double.IsNaN(exponent)) return double.NaN;
        if (basis == 0 && exponent < 0) throw new SqlException(SqlStates.InvalidArgumentForPowerFunction, "zero raised to a negative power is undefined");
        if (double.IsInfinity(basis) || double.IsInfinity(exponent)) return Math.Pow(basis, exponent);
        if (basis < 0 && Math.Truncate(exponent) != exponent)
            throw new SqlException(SqlStates.InvalidArgumentForPowerFunction, "a negative number raised to a non-integer power yields a complex result");
        var result = Math.Pow(basis, exponent);
        if (double.IsInfinity(result)) throw Overflow();
        if (result == 0 && basis != 0) throw Underflow();
        return result;
    }
    internal static double Round(double value, long places = 0)
    {
        if (!double.IsFinite(value) || value == 0 || places >= 324) return value;
        if (places < -308) return Math.CopySign(0, value);
        if (places > 308)
        {
            var exact = ToDecimal(value).Round(places);
            return exact.IsZero ? Math.CopySign(0, value) : Parse(exact.ToString());
        }
        var factor = Math.Pow(10, Math.Abs(places));
        var scaled = places >= 0 ? value * factor : value / factor;
        if (double.IsInfinity(scaled)) return value;
        var integral = Math.Round(scaled, MidpointRounding.AwayFromZero);
        var result = places >= 0 ? integral / factor : integral * factor;
        if (double.IsInfinity(result)) throw Overflow();
        return result == 0 ? Math.CopySign(0, value) : result;
    }
    internal static long WidthBucket(double value, double low, double high, long count)
    {
        if (count <= 0 || count > int.MaxValue || double.IsNaN(value) || !double.IsFinite(low) || !double.IsFinite(high) || low == high)
            throw new SqlException(SqlStates.InvalidArgumentForWidthBucketFunction, "invalid width_bucket argument");
        if (low < high)
        {
            if (value < low) return 0;
            if (value >= high) return count == int.MaxValue ? throw Overflow() : count + 1;
        }
        else
        {
            if (value > low) return 0;
            if (value <= high) return count == int.MaxValue ? throw Overflow() : count + 1;
        }
        var width = high - low;
        var fraction = double.IsInfinity(width) ? (value / 2 - low / 2) / (high / 2 - low / 2) : (value - low) / width;
        return Math.Min(count, (long)Math.Floor(fraction * count) + 1);
    }

    internal static ExactDecimal ToDecimal(double value)
    {
        if (!double.IsFinite(value)) throw Overflow();
        if (value == 0) return ExactDecimal.Zero;
        var parts = Parts(Math.Abs(value), false);
        var exact = Dyadic(parts.Mantissa, parts.Exponent);
        return value < 0 ? exact.Negate() : exact;
    }

    internal static string Render(double value, bool single = false)
    {
        if (single) value = (float)value;
        if (double.IsNaN(value)) return "NaN";
        if (double.IsPositiveInfinity(value)) return "Infinity";
        if (double.IsNegativeInfinity(value)) return "-Infinity";
        var negative = BitConverter.DoubleToInt64Bits(value) < 0;
        if (value == 0) return negative ? "-0" : "0";
        value = Math.Abs(value);
        var parts = Parts(value, single);
        var exact = Dyadic(parts.Mantissa, parts.Exponent);
        var lower = parts.ShortLower ? Dyadic(parts.Mantissa * 4 - 1, parts.Exponent - 2) : Dyadic(parts.Mantissa * 2 - 1, parts.Exponent - 1);
        var upper = Dyadic(parts.Mantissa * 2 + 1, parts.Exponent - 1);
        var exponent = exact.Digits - exact.Scale - 1;
        var native = single ? ((float)value).ToString("R", CultureInfo.InvariantCulture) : value.ToString("R", CultureInfo.InvariantCulture);
        var nativeMantissa = native.Split('E', 'e')[0].Replace(".", "", StringComparison.Ordinal).TrimStart('0').TrimEnd('0');
        for (var precision = Math.Max(1, nativeMantissa.Length); precision <= (single ? 9 : 17); precision++)
        {
            var scale = precision - 1 - exponent;
            var below = exact.Truncate(scale);
            var above = below.AddUncapped(ExactDecimal.Parse("1e" + (-scale).ToString(CultureInfo.InvariantCulture)));
            var belowFits = below.CompareTo(lower) > 0 && below.CompareTo(upper) < 0;
            var aboveFits = above.CompareTo(lower) > 0 && above.CompareTo(upper) < 0;
            if (!belowFits && !aboveFits) continue;
            var chosen = belowFits ? below : above;
            if (belowFits && aboveFits)
            {
                var comparison = exact.Subtract(below).CompareTo(above.Subtract(exact));
                if (comparison > 0 || comparison == 0 && !LastDigitEven(below, precision)) chosen = above;
            }
            return (negative ? "-" : "") + Layout(chosen, single);
        }
        throw new InvalidOperationException("no strict-shortest float rendering found");
    }

    private static bool LastDigitEven(ExactDecimal candidate, int precision)
    {
        var digits = candidate.ToString().Replace(".", "", StringComparison.Ordinal).TrimStart('0');
        return precision > digits.Length || (digits[precision - 1] - '0') % 2 == 0;
    }
    private static string Layout(ExactDecimal value, bool single)
    {
        var exponent = value.Digits - value.Scale - 1;
        var digits = value.ToString().Replace(".", "", StringComparison.Ordinal).TrimStart('0').TrimEnd('0');
        if (exponent >= -4 && exponent < (single ? 6 : 15))
        {
            var point = exponent + 1;
            if (point <= 0) return "0." + new string('0', -point) + digits;
            if (point >= digits.Length) return digits + new string('0', point - digits.Length);
            return digits[..point] + "." + digits[point..];
        }
        return digits[0] + (digits.Length == 1 ? "" : "." + digits[1..]) + "e" + (exponent < 0 ? "-" : "+") + Math.Abs(exponent).ToString("D2", CultureInfo.InvariantCulture);
    }
    private static (ulong Mantissa, int Exponent, bool ShortLower) Parts(double value, bool single)
    {
        if (single)
        {
            var bits = BitConverter.SingleToUInt32Bits((float)value); var exponent = (int)((bits >> 23) & 255);
            var fraction = bits & 0x7fffff;
            return (fraction | (exponent == 0 ? 0u : 1u << 23), (exponent == 0 ? 1 : exponent) - 127 - 23, fraction == 0 && exponent > 1);
        }
        else
        {
            var bits = BitConverter.DoubleToUInt64Bits(value); var exponent = (int)((bits >> 52) & 2047);
            var fraction = bits & 0xfffffffffffff;
            return (fraction | (exponent == 0 ? 0ul : 1ul << 52), (exponent == 0 ? 1 : exponent) - 1023 - 52, fraction == 0 && exponent > 1);
        }
    }
    private static ExactDecimal Dyadic(ulong mantissa, int exponent)
    {
        var coefficient = DecimalMagnitude.FromUInt64(mantissa);
        var factor = exponent < 0 ? 5u : 2u;
        for (var i = 0; i < Math.Abs(exponent); i++) coefficient = DecimalMagnitude.MultiplySmall(coefficient, factor);
        var digits = DecimalMagnitude.Render(coefficient);
        return ExactDecimal.Parse(exponent < 0 ? digits + "e" + exponent.ToString(CultureInfo.InvariantCulture) : digits);
    }
    private static bool White(char c) => c is ' ' or '\t' or '\n' or '\r' or '\v' or '\f';
    private static bool Digit(char c) => c is >= '0' and <= '9';
    private static SqlException Invalid() => new(SqlStates.InvalidTextRepresentation, "invalid input syntax for floating point");
    private static SqlException Overflow() => new(SqlStates.NumericValueOutOfRange, "value out of range: overflow");
    private static SqlException Underflow() => new(SqlStates.NumericValueOutOfRange, "value out of range: underflow");
    private static SqlException OutOfRange() => new(SqlStates.NumericValueOutOfRange, "input is out of range");
}
