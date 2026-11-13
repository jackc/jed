namespace Jed;

/// <summary>Exact scalar numeric functions; overload resolution remains in the resolver.</summary>
internal static class DecimalFunctions
{
    internal static bool TryCall(string name, SqlValue[] args, SqlType result, Meter meter, out SqlValue value)
    {
        value = default;
        if (name is not ("factorial" or "gcd" or "lcm" or "width_bucket" or "div" or "mod")) return false;
        if (args.Any(a => a.Type.IsFloat())) return false;
        if (name == "width_bucket" && args.Length != 4) return false; // array threshold overload has its own kernel
        if (args.Any(a => a.IsNull)) { value = SqlValue.Null(result); return true; }
        if (name == "factorial")
        {
            var n = args[0].AsInteger();
            if (n < 0) throw new SqlException(SqlStates.NumericValueOutOfRange, "factorial requires a nonnegative argument");
            var product = ExactDecimal.One;
            for (long k = 2; k <= n; k++)
            {
                var factor = ExactDecimal.FromInt64(k);
                meter.Charge(Costs.DecimalWork * product.DecimalWork(factor, "*"));
                meter.Guard();
                product = product.Multiply(factor);
            }
            value = SqlValue.Decimal(product);
            return true;
        }
        if (name == "width_bucket")
        {
            value = SqlValue.Integer(Decimal(args[0]).WidthBucket(Decimal(args[1]), Decimal(args[2]), args[3].AsInteger()), SqlType.I32);
            return true;
        }
        if (name == "div")
        {
            value = SqlValue.Decimal(Decimal(args[0]).DivideAtScale(Decimal(args[1]), 0, round: false));
            return true;
        }
        if (args[0].Type.IsInteger() && args[1].Type.IsInteger())
        {
            var a = args[0].AsInteger();
            var b = args[1].AsInteger();
            if (name == "mod")
            {
                if (b == 0) throw new SqlException(SqlStates.DivisionByZero, "division by zero");
                value = SqlValue.Integer(b == -1 ? 0 : a % b, result);
                return true;
            }
            var magnitudeA = Magnitude(a);
            var magnitudeB = Magnitude(b);
            var gcd = Gcd(magnitudeA, magnitudeB);
            if (name == "gcd")
            {
                if (gcd > long.MaxValue) throw Errors.Overflow();
                value = SqlValue.Integer((long)gcd, result);
            }
            else
            {
                if (magnitudeA == 0 || magnitudeB == 0) { value = SqlValue.Integer(0, result); return true; }
                var quotient = magnitudeA / gcd;
                if (quotient > (ulong)long.MaxValue / magnitudeB) throw Errors.Overflow();
                value = SqlValue.Integer((long)(quotient * magnitudeB), result);
            }
            return true;
        }
        var left = Decimal(args[0]);
        var right = Decimal(args[1]);
        if (name == "mod")
        {
            meter.Charge(Costs.DecimalWork * left.DecimalWork(right, "%"));
            meter.Guard();
        }
        value = SqlValue.Decimal(name switch
        {
            "mod" => left.Remainder(right),
            "gcd" => left.GreatestCommonDivisor(right),
            "lcm" => left.LeastCommonMultiple(right),
            _ => throw new InvalidOperationException("unresolved exact numeric function")
        });
        return true;
    }

    private static ExactDecimal Decimal(SqlValue value) => value.Type.IsInteger() ? ExactDecimal.FromInt64(value.AsInteger()) : value.AsDecimal();
    private static ulong Magnitude(long value) => value < 0 ? (ulong)(-(value + 1)) + 1 : (ulong)value;
    private static ulong Gcd(ulong a, ulong b)
    {
        while (b != 0) { var remainder = a % b; a = b; b = remainder; }
        return a;
    }
}

public sealed partial class ExactDecimal
{
    public ExactDecimal GreatestCommonDivisor(ExactDecimal other)
    {
        var scale = Math.Max(Scale, other.Scale);
        var a = DecimalMagnitude.Shift(coefficient, scale - Scale);
        var b = DecimalMagnitude.Shift(other.coefficient, scale - other.Scale);
        return new ExactDecimal(false, MagnitudeGcd(a, b), scale).CheckCap();
    }

    public ExactDecimal LeastCommonMultiple(ExactDecimal other)
    {
        var scale = Math.Max(Scale, other.Scale);
        if (IsZero || other.IsZero) return new(false, [], scale);
        var a = DecimalMagnitude.Shift(coefficient, scale - Scale);
        var b = DecimalMagnitude.Shift(other.coefficient, scale - other.Scale);
        var gcd = MagnitudeGcd(a, b);
        var (quotient, _) = DecimalMagnitude.DivRem(a, gcd);
        return new ExactDecimal(false, DecimalMagnitude.Multiply(quotient, b), scale).CheckCap();
    }

    private static uint[] MagnitudeGcd(uint[] a, uint[] b)
    {
        while (b.Length != 0)
        {
            var (_, remainder) = DecimalMagnitude.DivRem(a, b);
            a = b; b = remainder;
        }
        return a;
    }

    /// <summary>Exact equal-width bucket, with uncapped internal differences and products.</summary>
    public int WidthBucket(ExactDecimal low, ExactDecimal high, long count)
    {
        if (count <= 0 || low.Equals(high))
            throw new SqlException(SqlStates.InvalidArgumentForWidthBucketFunction, "width_bucket requires positive count and distinct bounds");
        var ascending = low.CompareTo(high) < 0;
        if (ascending ? CompareTo(low) < 0 : CompareTo(low) > 0) return 0;
        if (ascending ? CompareTo(high) >= 0 : CompareTo(high) <= 0)
        {
            if (count >= int.MaxValue) throw Overflow();
            return (int)count + 1;
        }
        var numerator = (ascending ? SubtractUncapped(low) : low.SubtractUncapped(this)).MultiplyExact(FromInt64(count));
        var denominator = ascending ? high.SubtractUncapped(low) : low.SubtractUncapped(high);
        long left = 0;
        var right = count;
        while (right - left > 1)
        {
            var middle = left + (right - left) / 2;
            if (denominator.MultiplyExact(FromInt64(middle)).CompareTo(numerator) <= 0) left = middle;
            else right = middle;
        }
        if (left >= int.MaxValue) throw Overflow();
        return (int)left + 1;
    }
}
