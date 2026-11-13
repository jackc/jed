using System.Globalization;

namespace Jed;

public sealed partial class ExactDecimal
{
    // These are exact-decimal algorithms. The one floating conversion is used
    // only for the PG scale estimate allowed by decimal.md §8, never a result.
    private const double Log10EEstimate = 0.434294481903252;
    private const double Log10TwoEstimate = 0.301029995663981;
    private const int MaximumResultScale = 2000;
    private const int MaximumWeight = 32767;
    private static readonly ExactDecimal Two = FromInt64(2);
    private static readonly ExactDecimal Ten = FromInt64(10);
    private static readonly ExactDecimal LowerLogReduction = new(false, [9], 1);
    private static readonly ExactDecimal UpperLogReduction = new(false, [11], 1);

    /// <summary>
    /// Exact square root. The optional step callback receives work units and
    /// cumulative logical scratch bytes to reserve BEFORE the associated work.
    /// A throwing callback aborts immediately; resource errors are never caught.
    /// </summary>
    public ExactDecimal Sqrt(Action<long, long>? meter = null)
    {
        MathStep(meter, 0, false, this);
        var scale = ResultScale(16 - (BaseWeight * 2 + 1), Scale);
        return SquareRootCore(scale, meter).CheckCap();
    }

    public ExactDecimal Exp(Action<long, long>? meter = null)
    {
        MathStep(meter, 0, false, this);
        var estimate = Math.Clamp(ToEstimate() * Log10EEstimate, -MaximumResultScale, MaximumResultScale);
        var scale = ResultScale(16 - (int)estimate, Scale);
        return ExponentialCore(scale, meter).CheckCap();
    }

    public ExactDecimal Ln(Action<long, long>? meter = null)
    {
        MathStep(meter, 0, false, this);
        CheckLogDomain();
        var scale = ResultScale(16 - LogarithmWeightEstimate(meter), Scale);
        return LogarithmCore(scale, meter).CheckCap();
    }

    /// <summary>Logarithm of this value in the specified base.</summary>
    public ExactDecimal Log(ExactDecimal @base, Action<long, long>? meter = null)
    {
        MathStep(meter, 0, false, @base, this);
        @base.CheckLogDomain();
        CheckLogDomain();
        var baseWeight = @base.LogarithmWeightEstimate(meter);
        var numberWeight = LogarithmWeightEstimate(meter);
        var resultWeight = numberWeight - baseWeight;
        var scale = ResultScale(16 - resultWeight, Math.Max(@base.Scale, Scale));
        var baseGuard = Math.Max(scale + resultWeight - baseWeight + 8, 0);
        var numberGuard = Math.Max(scale + resultWeight - numberWeight + 8, 0);
        var denominator = @base.LogarithmCore(baseGuard, meter);
        var numerator = LogarithmCore(numberGuard, meter);
        return numerator.DivideUncapped(denominator, scale).CheckCap();
    }

    public ExactDecimal Log10(Action<long, long>? meter = null)
    {
        MathStep(meter, 0, false, this);
        return Log(Ten, meter);
    }

    public ExactDecimal Power(ExactDecimal exponent, Action<long, long>? meter = null)
    {
        MathStep(meter, 0, false, this, exponent);
        if (IsZero && exponent.Negative) throw PowerDomain("zero raised to a negative power");
        var integralExponent = exponent.Rescale(0, false);
        var isInteger = integralExponent.Equals(exponent);
        if (isInteger && integralExponent.Digits <= 10)
        {
            var value = integralExponent.ToInt64();
            if (value >= int.MinValue && value <= int.MaxValue)
                return IntegerPower((int)value, exponent.Scale, meter);
        }
        if (IsZero) return new(false, [], 16);
        if (Negative && !isInteger) throw PowerDomain("negative base with a non-integer exponent");
        var negativeResult = Negative && !integralExponent.IsZero && integralExponent.coefficient[0] % 2 != 0;
        var magnitude = Abs();
        var logWeight = magnitude.LogarithmWeightEstimate(meter);
        var preliminaryScale = Math.Max(8 - logWeight, 0);
        var preliminaryLog = magnitude.LogarithmCore(preliminaryScale, meter).MultiplyAtScale(exponent, preliminaryScale);
        var estimate = preliminaryLog.ToEstimate();
        if (Math.Abs(estimate) > MaximumResultScale * 3.01)
        {
            if (estimate > 0) throw Overflow();
            return new(false, [], MaxPrecision);
        }
        var weight = (int)(estimate * Log10EEstimate);
        var scale = ResultScale(16 - weight, Math.Max(Scale, exponent.Scale));
        var guard = Math.Max(Math.Max(scale + weight, 0) - logWeight + 8, 0);
        var logarithm = magnitude.LogarithmCore(guard, meter).MultiplyAtScale(exponent, guard);
        var result = logarithm.ExponentialCore(scale, meter);
        return (negativeResult ? result.Negate() : result).CheckCap();
    }

    private static SqlException PowerDomain(string message) => new(SqlStates.InvalidArgumentForPowerFunction, message);
    private void CheckLogDomain()
    {
        if (IsZero || Negative) throw new SqlException(SqlStates.InvalidArgumentForLog, "logarithm requires a positive value");
    }

    private int BaseWeight => IsZero ? 0 : FloorFourth(Digits - 1 - Scale);
    private static int FloorFourth(int exponent) => exponent >= 0 ? exponent / 4 : (exponent - 3) / 4;
    private static int ResultScale(int estimate, int minimum) => Math.Clamp(Math.Max(estimate, minimum), 0, MaxPrecision);
    private double ToEstimate() => double.Parse(ToString(), NumberStyles.Float, CultureInfo.InvariantCulture);
    private ExactDecimal MultiplyExact(ExactDecimal other) => new(Negative != other.Negative, DecimalMagnitude.Multiply(coefficient, other.coefficient), Scale + other.Scale);
    private ExactDecimal MultiplyAtScale(ExactDecimal other, int scale) => MultiplyExact(other).RoundUncapped(scale);
    private ExactDecimal SubtractUncapped(ExactDecimal other) => AddUncapped(other.Negate());

    private ExactDecimal RoundUncapped(int scale)
    {
        if (scale >= 0) return Rescale(scale, true);
        var leftPlaces = -scale;
        if (leftPlaces > Digits + 1) return Zero;
        var divisor = DecimalMagnitude.Shift([1], Scale + leftPlaces);
        var (quotient, remainder) = DecimalMagnitude.DivRem(coefficient, divisor);
        if (DecimalMagnitude.Compare(DecimalMagnitude.MultiplySmall(remainder, 2), divisor) >= 0) quotient = DecimalMagnitude.Add(quotient, [1]);
        return new(Negative, DecimalMagnitude.Shift(quotient, leftPlaces), 0);
    }

    private ExactDecimal SquareRootCore(int scale, Action<long, long>? meter)
    {
        if (IsZero) return new(false, [], Math.Max(scale, 0));
        if (Negative) throw PowerDomain("square root requires a nonnegative value");
        MathStep(meter, scale, true, this);
        var guardScale = Math.Max(Math.Max(scale, 0) + 1, (Scale + 1) / 2 + 1);
        var scaled = DecimalMagnitude.Shift(coefficient, 2 * guardScale - Scale);
        var guess = DecimalMagnitude.Shift([1], (DecimalMagnitude.Digits(scaled) + 1) / 2);
        while (true)
        {
            var (quotient, _) = DecimalMagnitude.DivRem(scaled, guess);
            var (next, _) = DecimalMagnitude.DivRem(DecimalMagnitude.Add(guess, quotient), [2]);
            if (DecimalMagnitude.Compare(next, guess) >= 0)
                return new ExactDecimal(false, guess, guardScale).RoundUncapped(scale);
            guess = next;
        }
    }

    private ExactDecimal ExponentialCore(int scale, Action<long, long>? meter)
    {
        MathStep(meter, scale, false, this);
        var estimate = ToEstimate();
        if (Math.Abs(estimate) >= MaximumResultScale * 3)
        {
            if (estimate > 0) throw Overflow();
            return new(false, [], Math.Max(scale, 0));
        }
        var weight = (int)(estimate * Log10EEstimate);
        var divisions = 0;
        var reducedEstimate = estimate;
        while (Math.Abs(reducedEstimate) > 0.01) { divisions++; reducedEstimate /= 2; }
        var reduced = divisions == 0 ? this : DivideUncapped(FromInt64(1L << divisions), Scale + divisions);
        var significantDigits = Math.Max(1 + weight + scale + (int)(divisions * Log10TwoEstimate), 0) + 8;
        var seriesScale = significantDigits - 1;
        var result = One.AddUncapped(reduced);
        var term = reduced.MultiplyAtScale(reduced, seriesScale).DivideUncapped(Two, seriesScale);
        long factorialTerm = 2;
        while (!term.IsZero)
        {
            MathStep(meter, seriesScale, false, result, term, reduced);
            result = result.AddUncapped(term);
            factorialTerm++;
            term = term.MultiplyAtScale(reduced, seriesScale).DivideUncapped(FromInt64(factorialTerm), seriesScale);
        }
        for (var i = 0; i < divisions; i++)
        {
            MathStep(meter, scale, false, result);
            result = result.MultiplyAtScale(result, Math.Max(significantDigits - 8 * result.BaseWeight, 0));
        }
        return result.RoundUncapped(scale);
    }

    private ExactDecimal LogarithmCore(int scale, Action<long, long>? meter)
    {
        MathStep(meter, scale, false, this);
        var reduced = this;
        var compensation = Two;
        var roots = 0;
        while (reduced.CompareTo(LowerLogReduction) <= 0 || reduced.CompareTo(UpperLogReduction) >= 0)
        {
            var rootScale = scale - 2 * reduced.BaseWeight + 8;
            reduced = reduced.SquareRootCore(rootScale, meter);
            compensation = compensation.MultiplyAtScale(Two, 0);
            roots++;
        }
        var seriesScale = scale + (int)((roots + 1) * Log10TwoEstimate) + 8;
        var ratio = reduced.SubtractUncapped(One).DivideUncapped(reduced.AddUncapped(One), seriesScale);
        var result = ratio;
        var power = ratio;
        var square = ratio.MultiplyAtScale(ratio, seriesScale);
        long denominator = 1;
        while (true)
        {
            MathStep(meter, seriesScale, false, result, power, square);
            denominator += 2;
            power = power.MultiplyAtScale(square, seriesScale);
            var term = power.DivideUncapped(FromInt64(denominator), seriesScale);
            if (term.IsZero) break;
            result = result.AddUncapped(term);
            if (term.BaseWeight < result.BaseWeight - seriesScale * 2 / 4) break;
        }
        return result.MultiplyAtScale(compensation, scale);
    }

    private int LogarithmWeightEstimate(Action<long, long>? meter)
    {
        if (CompareTo(LowerLogReduction) >= 0 && CompareTo(UpperLogReduction) <= 0)
        {
            var difference = SubtractUncapped(One);
            return difference.IsZero ? 0 : difference.Digits - 1 - difference.Scale;
        }
        var logarithm = LogarithmCore(20, meter);
        if (logarithm.IsZero) return 0;
        var weight = logarithm.Digits - 1 - logarithm.Scale;
        return weight < 0 ? weight + 1 : weight;
    }

    private ExactDecimal IntegerPower(int exponent, int exponentScale, Action<long, long>? meter)
    {
        double estimate = 0;
        if (!IsZero)
        {
            // A deterministic exact log10 replaces libm in this estimate.
            var ln = Abs().LogarithmCore(30, meter);
            var lnTen = Ten.LogarithmCore(30, meter);
            estimate = ln.DivideUncapped(lnTen, 30).MultiplyExact(FromInt64(exponent)).ToEstimate();
        }
        if (estimate > (MaximumWeight + 1) * 4) throw Overflow();
        if (estimate + 1 < -MaxPrecision) return new(false, [], MaxPrecision);
        var weight = (int)estimate;
        var scale = ResultScale(16 - weight, Math.Max(Scale, exponentScale));
        switch (exponent)
        {
            case 0: return One.RoundUncapped(scale);
            case 1: return RoundUncapped(scale).CheckCap();
            case -1: return One.DivideUncapped(this, scale).CheckCap();
            case 2: return MultiplyAtScale(this, scale).CheckCap();
        }
        if (IsZero) return new(false, [], scale);
        var mask = exponent < 0 ? (uint)-(long)exponent : (uint)exponent;
        var guard = mask <= 1 ? 0 : (int)FromInt64(mask).LogarithmCore(12, meter).Rescale(0, false).ToInt64();
        var significantDigits = 1 + scale + weight + guard + 8;
        var product = this;
        var result = (mask & 1) == 1 ? this : One;
        while ((mask >>= 1) != 0)
        {
            MathStep(meter, scale, false, product, result);
            var productScale = Math.Max(Math.Min(significantDigits - 8 * product.BaseWeight, 2 * product.Scale), 0);
            product = product.MultiplyAtScale(product, productScale);
            if ((mask & 1) == 1)
            {
                var resultScale = Math.Max(Math.Min(significantDigits - 4 * (product.BaseWeight + result.BaseWeight), product.Scale + result.Scale), 0);
                result = product.MultiplyAtScale(result, resultScale);
            }
            if (product.BaseWeight > MaximumWeight || result.BaseWeight > MaximumWeight)
            {
                if (exponent > 0) throw Overflow();
                return new(false, [], scale);
            }
        }
        return (exponent < 0 ? One.DivideUncapped(result, scale) : result.RoundUncapped(scale)).CheckCap();
    }

    private static void MathStep(Action<long, long>? meter, int scale, bool squareRoot, params ExactDecimal[] arguments)
    {
        if (meter is null) return;
        long digits = 32 + Math.Abs((long)scale);
        foreach (var argument in arguments) digits += argument.Digits + argument.Scale;
        var groups = (digits + 3) / 4;
        var work = groups * groups;
        if (squareRoot)
        {
            var bits = 0;
            for (var count = digits; count > 0; count >>= 1) bits++;
            work *= bits;
        }
        meter(work, 8 * digits);
    }
}
