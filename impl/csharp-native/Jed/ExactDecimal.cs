using System.Globalization;
using System.Text;

namespace Jed;

/// <summary>Finite exact decimal, with a preserved display scale and an owned base-10^9 coefficient.</summary>
public sealed partial class ExactDecimal : IComparable<ExactDecimal>, IEquatable<ExactDecimal>
{
    public const int MaxPrecision = 1000;
    public const int MaxScale = 16383;
    public const int MaxIntegerDigits = 131072;
    private readonly uint[] coefficient;
    public bool Negative { get; }
    public int Scale { get; }
    public int Digits => DecimalMagnitude.Digits(coefficient);
    public bool IsZero => coefficient.Length == 0;
    public static ExactDecimal Zero { get; } = new(false, [], 0);
    public static ExactDecimal One { get; } = new(false, [1], 0);

    // All arrays passed to this constructor are newly owned or already immutable.
    private ExactDecimal(bool negative, uint[] magnitude, int scale)
    {
        coefficient = DecimalMagnitude.Trim(magnitude);
        Negative = negative && coefficient.Length != 0;
        Scale = scale;
    }

    private static SqlException Overflow() => new(SqlStates.NumericValueOutOfRange, "numeric value out of range");
    private static SqlException InvalidInput() => new(SqlStates.InvalidTextRepresentation, "invalid input syntax for numeric");

    public ExactDecimal CheckCap()
    {
        if (Scale < 0 || Scale > MaxScale || Digits - Scale > MaxIntegerDigits) throw Overflow();
        return this;
    }

    public static ExactDecimal FromInt64(long value) => FromUnscaled(value, 0);

    public static ExactDecimal FromUnscaled(long value, int scale)
    {
        var magnitude = value < 0 ? (ulong)(-(value + 1)) + 1 : (ulong)value;
        return new ExactDecimal(value < 0, DecimalMagnitude.FromUInt64(magnitude), scale).CheckCap();
    }

    /// <summary>Read numeric input using ASCII digits, PG whitespace, separators and radix prefixes.</summary>
    public static ExactDecimal Parse(string text)
    {
        ArgumentNullException.ThrowIfNull(text);
        var start = 0;
        var end = text.Length;
        while (start < end && IsSpace(text[start])) start++;
        while (end > start && IsSpace(text[end - 1])) end--;
        var negative = start < end && text[start] == '-';
        if (start < end && text[start] is '+' or '-') start++;
        if (start == end) throw InvalidInput();
        if (start + 1 < end && text[start] == '0')
        {
            var radix = text[start + 1] switch { 'x' or 'X' => 16, 'o' or 'O' => 8, 'b' or 'B' => 2, _ => 10 };
            if (radix != 10) return ParseRadix(text.AsSpan(start + 2, end - start - 2), negative, radix);
        }

        var digits = new StringBuilder();
        var point = false;
        var fractionalDigits = 0;
        var p = start;
        while (p < end)
        {
            var c = text[p];
            if (IsDigit(c))
            {
                digits.Append(c);
                if (point) fractionalDigits++;
                p++;
            }
            else if (c == '.' && !point)
            {
                point = true;
                p++;
                if (p < end && text[p] == '_') throw InvalidInput();
            }
            else if (c == '_' && p > start && IsDigit(text[p - 1]) && p + 1 < end && IsDigit(text[p + 1])) p++;
            else break;
        }
        if (digits.Length == 0) throw InvalidInput();
        long exponent = 0;
        if (p < end && text[p] is 'e' or 'E')
        {
            p++;
            var exponentNegative = p < end && text[p] == '-';
            if (p < end && text[p] is '+' or '-') p++;
            if (p == end || !IsDigit(text[p])) throw InvalidInput();
            // Saturate beyond every meaningful exponent without overflowing the scanner.
            var limit = (long)text.Length + MaxIntegerDigits + MaxScale + 1;
            while (p < end)
            {
                if (IsDigit(text[p])) { exponent = Math.Min(limit, exponent * 10 + text[p] - '0'); p++; }
                else if (text[p] == '_' && p + 1 < end && IsDigit(text[p + 1])) p++;
                else break;
            }
            if (exponentNegative) exponent = -exponent;
        }
        if (p != end) throw InvalidInput();
        var raw = digits.ToString().TrimStart('0');
        var scale = (long)fractionalDigits - exponent;
        if (scale > MaxScale || raw.Length != 0 && raw.Length - scale > MaxIntegerDigits) throw Overflow();
        var magnitude = DecimalMagnitude.Parse(raw.AsSpan());
        if (scale < 0)
        {
            if (magnitude.Length != 0) magnitude = DecimalMagnitude.Shift(magnitude, (int)-scale);
            scale = 0;
        }
        return new ExactDecimal(negative, magnitude, (int)scale).CheckCap();
    }

    private static ExactDecimal ParseRadix(ReadOnlySpan<char> text, bool negative, int radix)
    {
        uint[] magnitude = [];
        var seen = false;
        var significantDigits = 0;
        var bits = radix == 16 ? 4 : radix == 8 ? 3 : 1;
        uint group = 0;
        uint multiplier = 1;
        var groupSize = 0;
        for (var i = 0; i < text.Length; i++)
        {
            var c = text[i];
            if (c == '_' && i + 1 < text.Length && RadixDigit(text[i + 1]) is var next && next >= 0 && next < radix) continue;
            var digit = RadixDigit(c);
            if (digit < 0 || digit >= radix) throw InvalidInput();
            seen = true;
            if (digit != 0 || significantDigits != 0) significantDigits++;
            if ((long)(significantDigits - 1) * bits >= 435412) throw Overflow();
            group = group * (uint)radix + (uint)digit;
            multiplier *= (uint)radix;
            groupSize++;
            if (groupSize == 28 / bits)
            {
                magnitude = DecimalMagnitude.Add(DecimalMagnitude.MultiplySmall(magnitude, multiplier), DecimalMagnitude.FromUInt64(group));
                group = 0; multiplier = 1; groupSize = 0;
            }
        }
        if (!seen) throw InvalidInput();
        if (groupSize != 0) magnitude = DecimalMagnitude.Add(DecimalMagnitude.MultiplySmall(magnitude, multiplier), DecimalMagnitude.FromUInt64(group));
        return new ExactDecimal(negative, magnitude, 0).CheckCap();
    }

    private static int RadixDigit(char c) => c is >= '0' and <= '9' ? c - '0' : c is >= 'a' and <= 'f' ? c - 'a' + 10 : c is >= 'A' and <= 'F' ? c - 'A' + 10 : -1;
    private static bool IsDigit(char c) => c is >= '0' and <= '9';
    private static bool IsSpace(char c) => c is ' ' or '\t' or '\n' or '\r' or '\v' or '\f';

    public ExactDecimal Negate() => new(!Negative, coefficient, Scale);
    public ExactDecimal Abs() => Negative ? new(false, coefficient, Scale) : this;
    public ExactDecimal Add(ExactDecimal other) => AddUncapped(other).CheckCap();
    public ExactDecimal Subtract(ExactDecimal other) => Add(other.Negate());

    /// <summary>SUM/AVG folds check the format cap only on their final result.</summary>
    public ExactDecimal AddUncapped(ExactDecimal other)
    {
        var scale = Math.Max(Scale, other.Scale);
        var a = DecimalMagnitude.Shift(coefficient, scale - Scale);
        var b = DecimalMagnitude.Shift(other.coefficient, scale - other.Scale);
        if (Negative == other.Negative) return new(Negative, DecimalMagnitude.Add(a, b), scale);
        var comparison = DecimalMagnitude.Compare(a, b);
        return comparison >= 0 ? new(Negative, DecimalMagnitude.Subtract(a, b), scale) : new(other.Negative, DecimalMagnitude.Subtract(b, a), scale);
    }

    public ExactDecimal Multiply(ExactDecimal other)
    {
        var value = new ExactDecimal(Negative != other.Negative, DecimalMagnitude.Multiply(coefficient, other.coefficient), Scale + other.Scale);
        return (value.Scale > MaxScale ? value.Rescale(MaxScale, true) : value).CheckCap();
    }

    public ExactDecimal Divide(ExactDecimal other) => DivideAtScale(other, DivisionScale(other));

    // Explicit result scale is also used by the exact transcendental kernels.
    internal ExactDecimal DivideAtScale(ExactDecimal other, int scale, bool round = true)
        => DivideUncapped(other, scale, round).CheckCap();

    private ExactDecimal DivideUncapped(ExactDecimal other, int scale, bool round = true)
    {
        if (other.IsZero) throw new SqlException(SqlStates.DivisionByZero, "division by zero");
        var shift = scale + other.Scale - Scale;
        var numerator = shift >= 0 ? DecimalMagnitude.Shift(coefficient, shift) : coefficient;
        var denominator = shift < 0 ? DecimalMagnitude.Shift(other.coefficient, -shift) : other.coefficient;
        var (quotient, remainder) = DecimalMagnitude.DivRem(numerator, denominator);
        if (round && DecimalMagnitude.Compare(DecimalMagnitude.MultiplySmall(remainder, 2), denominator) >= 0)
            quotient = DecimalMagnitude.Add(quotient, [1]);
        return new ExactDecimal(Negative != other.Negative, quotient, scale);
    }

    public ExactDecimal Remainder(ExactDecimal other)
    {
        if (other.IsZero) throw new SqlException(SqlStates.DivisionByZero, "division by zero");
        var scale = Math.Max(Scale, other.Scale);
        var (_, remainder) = DecimalMagnitude.DivRem(DecimalMagnitude.Shift(coefficient, scale - Scale), DecimalMagnitude.Shift(other.coefficient, scale - other.Scale));
        return new ExactDecimal(Negative, remainder, scale).CheckCap();
    }

    public int DivisionScale(ExactDecimal other)
    {
        var (weight, first) = LeadingGroup();
        var (otherWeight, otherFirst) = other.LeadingGroup();
        var quotientWeight = weight - otherWeight - (first <= otherFirst ? 1 : 0);
        return Math.Min(MaxPrecision, Math.Max(Math.Max(16 - 4 * quotientWeight, 0), Math.Max(Scale, other.Scale)));
    }

    private (int Weight, uint First) LeadingGroup()
    {
        if (IsZero) return (0, 0);
        var exponent = Digits - 1 - Scale;
        var weight = exponent >= 0 ? exponent / 4 : (exponent - 3) / 4;
        var places = 4 * weight + Scale;
        // At most four digits survive. Read them directly, without allocating a large power of ten.
        var take = Digits - places;
        var topDigits = coefficient[^1].ToString(CultureInfo.InvariantCulture);
        if (topDigits.Length < Math.Min(take, Digits) && coefficient.Length > 1)
            topDigits += coefficient[^2].ToString("D9", CultureInfo.InvariantCulture);
        uint first = 0;
        for (var i = 0; i < take; i++) first = first * 10 + (uint)(i < Digits ? topDigits[i] - '0' : 0);
        return (weight, first);
    }

    /// <summary>Extra decimal_work units (the first group rides operator_eval).</summary>
    public long DecimalWork(ExactDecimal other, string operation)
    {
        if (operation is "/" or "%" && other.IsZero) return 0;
        static long Groups(int digits) => Math.Max(1, (digits + 3L) / 4);
        long work;
        if (operation == "*") work = Groups(Digits) * Groups(other.Digits);
        else if (operation == "/")
        {
            var shift = DivisionScale(other) + other.Scale - Scale;
            work = Groups(Digits + Math.Max(shift, 0)) * Groups(other.Digits + Math.Max(-shift, 0));
        }
        else
        {
            var scale = Math.Max(Scale, other.Scale);
            var a = Groups(Digits + scale - Scale);
            var b = Groups(other.Digits + scale - other.Scale);
            work = operation == "%" ? a * b : Math.Max(a, b);
        }
        return work - 1;
    }

    public ExactDecimal Round(long places = 0) => AtPlaces(places, true);
    public ExactDecimal Truncate(long places = 0) => AtPlaces(places, false);

    private ExactDecimal AtPlaces(long places, bool round)
    {
        if (places >= 0) return Rescale((int)Math.Min(places, MaxScale), round).CheckCap();
        // A requested position beyond the leading digit always produces zero; avoids negating MinValue.
        if (places < -(long)(Digits + 1)) return Zero;
        var leftPlaces = (int)-places;
        var divisor = DecimalMagnitude.Shift([1], Scale + leftPlaces);
        var (quotient, remainder) = DecimalMagnitude.DivRem(coefficient, divisor);
        if (round && DecimalMagnitude.Compare(DecimalMagnitude.MultiplySmall(remainder, 2), divisor) >= 0) quotient = DecimalMagnitude.Add(quotient, [1]);
        return new ExactDecimal(Negative, DecimalMagnitude.Shift(quotient, leftPlaces), 0).CheckCap();
    }

    internal ExactDecimal Rescale(int target, bool round)
    {
        if (target >= Scale) return new(Negative, DecimalMagnitude.Shift(coefficient, target - Scale), target);
        var divisor = DecimalMagnitude.Shift([1], Scale - target);
        var (quotient, remainder) = DecimalMagnitude.DivRem(coefficient, divisor);
        if (round && DecimalMagnitude.Compare(DecimalMagnitude.MultiplySmall(remainder, 2), divisor) >= 0) quotient = DecimalMagnitude.Add(quotient, [1]);
        return new(Negative, quotient, target);
    }

    public ExactDecimal Ceiling() => Bound(false);
    public ExactDecimal Floor() => Bound(true);
    private ExactDecimal Bound(bool floor)
    {
        var (quotient, remainder) = DecimalMagnitude.DivRem(coefficient, DecimalMagnitude.Shift([1], Scale));
        if (remainder.Length != 0 && Negative == floor) quotient = DecimalMagnitude.Add(quotient, [1]);
        return new ExactDecimal(Negative, quotient, 0).CheckCap();
    }

    public ExactDecimal Coerce(int precision, int scale)
    {
        if (precision < 1 || precision > MaxPrecision || scale < 0 || scale > precision)
            throw new SqlException(SqlStates.InvalidParameterValue, "invalid numeric precision or scale");
        var result = Rescale(scale, true);
        if (result.Digits - result.Scale > precision - scale) throw Overflow();
        return result.CheckCap();
    }

    public long ToInt64()
    {
        var rounded = Rescale(0, true);
        var bound = Negative ? 9223372036854775808UL : 9223372036854775807UL;
        ulong magnitude = 0;
        for (var i = rounded.coefficient.Length - 1; i >= 0; i--)
        {
            if (magnitude > (bound - rounded.coefficient[i]) / DecimalMagnitude.Radix) throw Overflow();
            magnitude = magnitude * DecimalMagnitude.Radix + rounded.coefficient[i];
        }
        return Negative ? magnitude == 9223372036854775808UL ? long.MinValue : -(long)magnitude : (long)magnitude;
    }

    /// <summary>A value canonical form for equality keys; only fractional zeroes are removed.</summary>
    public ExactDecimal TrimScale()
    {
        if (IsZero) return Zero;
        var removable = 0;
        foreach (var limb in coefficient)
        {
            if (limb == 0) { removable += 9; continue; }
            var n = limb;
            while (n % 10 == 0) { removable++; n /= 10; }
            break;
        }
        return Rescale(Scale - Math.Min(removable, Scale), false);
    }

    public int CompareTo(ExactDecimal? other)
    {
        if (other is null) return 1;
        if (Negative != other.Negative) return Negative ? -1 : 1;
        if (IsZero || other.IsZero) return IsZero ? other.IsZero ? 0 : -1 : 1;
        var sign = Negative ? -1 : 1;
        var exponentComparison = (Digits - Scale).CompareTo(other.Digits - other.Scale);
        if (exponentComparison != 0) return sign * exponentComparison;
        var scale = Math.Max(Scale, other.Scale);
        return sign * DecimalMagnitude.Compare(DecimalMagnitude.Shift(coefficient, scale - Scale), DecimalMagnitude.Shift(other.coefficient, scale - other.Scale));
    }

    public bool Equals(ExactDecimal? other) => other is not null && CompareTo(other) == 0;
    public override bool Equals(object? other) => other is ExactDecimal value && Equals(value);
    public override int GetHashCode()
    {
        var canonical = TrimScale();
        // Fixed algorithm: host-randomized hashing must not become an execution-order input.
        unchecked
        {
            var hash = canonical.Negative ? -1 : 1;
            hash = hash * 31 + canonical.Scale;
            foreach (var limb in canonical.coefficient) hash = hash * 31 + (int)limb;
            return hash;
        }
    }

    public override string ToString()
    {
        var raw = DecimalMagnitude.Render(coefficient);
        if (Scale > 0)
            raw = raw.Length > Scale ? raw.Insert(raw.Length - Scale, ".") : "0." + new string('0', Scale - raw.Length) + raw;
        return Negative ? "-" + raw : raw;
    }
}

// No host bignum participates in the value path. Every limb stays below 10^9;
// products plus carry fit in UInt64 even with checked arithmetic enabled.
internal static class DecimalMagnitude
{
    internal const ulong Radix = 1_000_000_000;
    private static readonly uint[] Powers = [1, 10, 100, 1000, 10000, 100000, 1000000, 10000000, 100000000];

    internal static uint[] Trim(uint[] value)
    {
        var length = value.Length;
        while (length > 0 && value[length - 1] == 0) length--;
        return length == value.Length ? value : value[..length];
    }

    internal static uint[] FromUInt64(ulong value)
    {
        if (value == 0) return [];
        if (value < Radix) return [(uint)value];
        if (value < Radix * Radix) return [(uint)(value % Radix), (uint)(value / Radix)];
        return [(uint)(value % Radix), (uint)(value / Radix % Radix), (uint)(value / (Radix * Radix))];
    }

    internal static uint[] Parse(ReadOnlySpan<char> digits)
    {
        var result = new uint[(digits.Length + 8) / 9];
        var index = 0;
        for (var end = digits.Length; end > 0; end -= 9)
        {
            uint value = 0;
            for (var i = Math.Max(0, end - 9); i < end; i++) value = value * 10 + (uint)(digits[i] - '0');
            result[index++] = value;
        }
        return Trim(result);
    }

    internal static string Render(uint[] value)
    {
        if (value.Length == 0) return "0";
        var text = new StringBuilder(value[^1].ToString(CultureInfo.InvariantCulture));
        for (var i = value.Length - 2; i >= 0; i--) text.Append(value[i].ToString("D9", CultureInfo.InvariantCulture));
        return text.ToString();
    }

    internal static int Digits(uint[] value)
    {
        if (value.Length == 0) return 0;
        var count = (value.Length - 1) * 9;
        var top = value[^1];
        do { count++; top /= 10; } while (top != 0);
        return count;
    }

    internal static int Compare(uint[] a, uint[] b)
    {
        if (a.Length != b.Length) return a.Length.CompareTo(b.Length);
        for (var i = a.Length - 1; i >= 0; i--) if (a[i] != b[i]) return a[i].CompareTo(b[i]);
        return 0;
    }

    internal static uint[] Add(uint[] a, uint[] b)
    {
        var length = Math.Max(a.Length, b.Length);
        var result = new uint[length + 1];
        ulong carry = 0;
        for (var i = 0; i < length; i++)
        {
            var sum = carry + (i < a.Length ? a[i] : 0UL) + (i < b.Length ? b[i] : 0UL);
            result[i] = (uint)(sum % Radix); carry = sum / Radix;
        }
        result[length] = (uint)carry;
        return Trim(result);
    }

    internal static uint[] Subtract(uint[] a, uint[] b)
    {
        var result = new uint[a.Length];
        long borrow = 0;
        for (var i = 0; i < a.Length; i++)
        {
            var difference = a[i] - (i < b.Length ? (long)b[i] : 0) - borrow;
            borrow = difference < 0 ? 1 : 0;
            result[i] = (uint)(difference + borrow * (long)Radix);
        }
        if (borrow != 0) throw new InvalidOperationException("negative decimal magnitude subtraction");
        return Trim(result);
    }

    internal static uint[] MultiplySmall(uint[] a, uint multiplier)
    {
        if (a.Length == 0 || multiplier == 0) return [];
        if (multiplier == 1) return a;
        var result = new uint[a.Length + 1];
        ulong carry = 0;
        for (var i = 0; i < a.Length; i++)
        {
            var product = (ulong)a[i] * multiplier + carry;
            result[i] = (uint)(product % Radix); carry = product / Radix;
        }
        result[a.Length] = (uint)carry;
        return Trim(result);
    }

    internal static uint[] Multiply(uint[] a, uint[] b)
    {
        if (a.Length == 0 || b.Length == 0) return [];
        var result = new uint[a.Length + b.Length];
        for (var i = 0; i < a.Length; i++)
        {
            ulong carry = 0;
            for (var j = 0; j < b.Length; j++)
            {
                var product = (ulong)a[i] * b[j] + result[i + j] + carry;
                result[i + j] = (uint)(product % Radix); carry = product / Radix;
            }
            result[i + b.Length] = (uint)carry;
        }
        return Trim(result);
    }

    internal static uint[] Shift(uint[] a, int places)
    {
        if (places < 0) throw new ArgumentOutOfRangeException(nameof(places));
        if (a.Length == 0 || places == 0) return a;
        var scaled = MultiplySmall(a, Powers[places % 9]);
        if (places < 9) return scaled;
        var result = new uint[scaled.Length + places / 9];
        scaled.CopyTo(result, places / 9);
        return result;
    }

    private static (uint[] Quotient, uint Remainder) DivideSmall(uint[] a, uint divisor)
    {
        var result = new uint[a.Length];
        ulong remainder = 0;
        for (var i = a.Length - 1; i >= 0; i--)
        {
            var current = remainder * Radix + a[i];
            result[i] = (uint)(current / divisor); remainder = current % divisor;
        }
        return (Trim(result), (uint)remainder);
    }

    internal static (uint[] Quotient, uint[] Remainder) DivRem(uint[] numerator, uint[] denominator)
    {
        if (denominator.Length == 0) throw new DivideByZeroException();
        if (Compare(numerator, denominator) < 0) return ([], numerator);
        if (denominator.Length == 1)
        {
            var (quotient, remainder) = DivideSmall(numerator, denominator[0]);
            return (quotient, FromUInt64(remainder));
        }
        // Knuth's normalized long division. The trial digit uses two limbs and
        // is corrected with a third before subtracting; an add-back fixes the
        // remaining one-unit overestimate. Intermediate buffers are private.
        var n = denominator.Length;
        var m = numerator.Length - n;
        var factor = (uint)(Radix / ((ulong)denominator[^1] + 1));
        var divisor = MultiplySmall(denominator, factor);
        var dividend = new uint[numerator.Length + 1];
        MultiplySmall(numerator, factor).CopyTo(dividend, 0);
        var result = new uint[m + 1];
        for (var j = m; j >= 0; j--)
        {
            var leading = (ulong)dividend[j + n] * Radix + dividend[j + n - 1];
            var trial = leading / divisor[n - 1];
            var remainder = leading % divisor[n - 1];
            while (trial >= Radix || remainder < Radix && trial * divisor[n - 2] > remainder * Radix + dividend[j + n - 2])
            {
                trial--; remainder += divisor[n - 1];
                if (remainder >= Radix) break;
            }
            ulong borrow = 0;
            for (var i = 0; i < n; i++)
            {
                var product = trial * divisor[i] + borrow;
                var difference = (long)dividend[j + i] - (long)(product % Radix);
                borrow = product / Radix;
                if (difference < 0) { difference += (long)Radix; borrow++; }
                dividend[j + i] = (uint)difference;
            }
            var top = (long)dividend[j + n] - (long)borrow;
            if (top < 0)
            {
                trial--;
                ulong carry = 0;
                for (var i = 0; i < n; i++)
                {
                    var sum = (ulong)dividend[j + i] + divisor[i] + carry;
                    dividend[j + i] = (uint)(sum % Radix); carry = sum / Radix;
                }
                top += (long)carry;
            }
            dividend[j + n] = (uint)top;
            result[j] = (uint)trial;
        }
        var (rest, _) = DivideSmall(Trim(dividend[..n]), factor);
        return (Trim(result), rest);
    }
}
