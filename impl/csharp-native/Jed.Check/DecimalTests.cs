using System.Globalization;
using System.Numerics;
using Jed;

internal static class DecimalTests
{
    internal static void Run()
    {
        // BigInteger is an independent test oracle only; it is not linked from the core.
        // Exercise carry, borrow and long-division correction across many limb boundaries.
        var random = new Random(0x5eed123);
        for (var iteration = 0; iteration < 4000; iteration++)
        {
            var a = Magnitude(random, random.Next(1, 130));
            var b = Magnitude(random, random.Next(1, 100));
            if (iteration % 2 == 0) a = -a;
            if (iteration % 3 == 0) b = -b;
            var aScale = random.Next(0, 35);
            var bScale = random.Next(0, 35);
            var left = ExactDecimal.Parse(Render(a, aScale));
            var right = ExactDecimal.Parse(Render(b, bScale));
            var alignScale = Math.Max(aScale, bScale);
            var alignedA = a * BigInteger.Pow(10, alignScale - aScale);
            var alignedB = b * BigInteger.Pow(10, alignScale - bScale);
            Equal(Render(alignedA + alignedB, alignScale), left.Add(right), "limb addition");
            Equal(Render(alignedA - alignedB, alignScale), left.Subtract(right), "limb subtraction");
            Equal(Render(a * b, aScale + bScale), left.Multiply(right), "limb multiplication");
            Corpus.Equal(Math.Sign(alignedA.CompareTo(alignedB)), Math.Sign(left.CompareTo(right)), "limb comparison");
            if (iteration % 16 == 0)
            {
                var gcd = BigInteger.GreatestCommonDivisor(alignedA, alignedB);
                Equal(Render(gcd, alignScale), left.GreatestCommonDivisor(right), "aligned coefficient gcd");
                var lcm = gcd.IsZero ? BigInteger.Zero : BigInteger.Abs(alignedA / gcd * alignedB);
                Equal(Render(lcm, alignScale), left.LeastCommonMultiple(right), "aligned coefficient lcm");
                var groups = left.ExportBase10000();
                Equal(left.ToString(), ExactDecimal.FromBase10000(left.Negative, left.Scale, groups), "base-10000 codec regrouping");
                if (groups.Length > 0) groups[0] = 0;
                Equal(Render(a, aScale), left, "exported groups never alias coefficient storage");
            }
            if (!b.IsZero)
            {
                Equal(Render(alignedA % alignedB, alignScale), left.Remainder(right), "limb remainder");
                var resultScale = DivisionScale(a, aScale, b, bScale);
                var shift = resultScale + bScale - aScale;
                var numerator = BigInteger.Abs(a) * BigInteger.Pow(10, Math.Max(shift, 0));
                var denominator = BigInteger.Abs(b) * BigInteger.Pow(10, Math.Max(-shift, 0));
                var quotient = BigInteger.DivRem(numerator, denominator, out var remainder);
                if (2 * remainder >= denominator) quotient++;
                if (a.Sign * b.Sign < 0) quotient = -quotient;
                Equal(Render(quotient, resultScale), left.Divide(right), "limb division and half-away rounding");
            }
            var original = left.ToString();
            _ = left.Add(right); _ = left.TrimScale(); _ = left.Round(2);
            Corpus.Equal(original, left.ToString(), "immutable coefficient ownership");
        }

        // Powers of the limb radix and their neighbors hit quotient normalization
        // and the multiply-subtract correction paths that random inputs undersample.
        for (var exponent = 9; exponent <= 180; exponent += 9)
        {
            var power = BigInteger.Pow(10, exponent);
            foreach (var delta in new[] { -1, 0, 1 })
            foreach (var divisorDelta in new[] { -1, 0, 1 })
            {
                var a = power * power + delta;
                var b = power / 2 + divisorDelta;
                var left = ExactDecimal.Parse(a.ToString(CultureInfo.InvariantCulture));
                var right = ExactDecimal.Parse(b.ToString(CultureInfo.InvariantCulture));
                Equal((a % b).ToString(CultureInfo.InvariantCulture), left.Remainder(right), "radix-boundary remainder");
            }
        }
        foreach (var value in new[] { long.MinValue, long.MinValue + 1, -1L, 0L, 1L, long.MaxValue - 1, long.MaxValue })
        {
            var number = ExactDecimal.FromInt64(value);
            Corpus.Equal(value, number.ToInt64(), "i64 exact conversion");
            Corpus.Equal(value.ToString(CultureInfo.InvariantCulture), number.ToString(), "i64 render");
        }
        Error(SqlStates.NumericValueOutOfRange, () => ExactDecimal.Parse("9223372036854775807.5").ToInt64());
        Error(SqlStates.NumericValueOutOfRange, () => ExactDecimal.Parse("-9223372036854775808.5").ToInt64());
        Corpus.Equal(long.MinValue, ExactDecimal.Parse("-9223372036854775808.4").ToInt64(), "negative i64 boundary rounding");
        var canonical = ExactDecimal.Parse("-1000000000.500000000000000000");
        var shortForm = ExactDecimal.Parse("-1000000000.5");
        Corpus.Equal(true, canonical.Equals(shortForm), "decimal value equality");
        Corpus.Equal(canonical.GetHashCode(), shortForm.GetHashCode(), "canonical hash across trailing zero limbs");
        Corpus.Equal(0, ExactDecimal.Parse("-0.000").CompareTo(ExactDecimal.Zero), "negative zero normalization");

        // Cap-size zero and underflow checks avoid quadratic work while proving
        // that allocation and scale are bounded even for adversarial exponents.
        Error(SqlStates.NumericValueOutOfRange, () => ExactDecimal.Parse("1e99999999999999999999999999999"));
        Error(SqlStates.NumericValueOutOfRange, () => ExactDecimal.Parse("1e-99999999999999999999999999999"));
        Error(SqlStates.NumericValueOutOfRange, () => ExactDecimal.Parse("1e-16384"));
        var tiny = ExactDecimal.Parse("1e-10000").Multiply(ExactDecimal.Parse("1e-10000"));
        Corpus.Equal(true, tiny.IsZero, "multiplication rounds at scale cap");
        Corpus.Equal(ExactDecimal.MaxScale, tiny.Scale, "multiplication cap display scale");
        Corpus.Equal(1000, ExactDecimal.Parse("5e-1001").Divide(ExactDecimal.One).Scale, "division display cap");
        Corpus.Equal(ExactDecimal.Parse("1e-1000"), ExactDecimal.Parse("5e-1001").Divide(ExactDecimal.One), "division negative shift half away");
        var maximumInteger = ExactDecimal.Parse(new string('9', ExactDecimal.MaxIntegerDigits));
        Error(SqlStates.NumericValueOutOfRange, () => maximumInteger.Add(ExactDecimal.One));
        var intermediate = maximumInteger.AddUncapped(ExactDecimal.One);
        Error(SqlStates.NumericValueOutOfRange, () => intermediate.CheckCap());
        Corpus.Equal(maximumInteger, intermediate.AddUncapped(ExactDecimal.One.Negate()).CheckCap(), "aggregate can recover from over-cap intermediate");
        Corpus.Equal(ExactDecimal.Zero, canonical.Round(long.MinValue), "minimum round places bounded");
        Corpus.Equal(ExactDecimal.Zero, canonical.Truncate(long.MinValue), "minimum trunc places bounded");
        Corpus.Equal(150L, ExactDecimal.Parse("1.500").ToUnscaledInt64(2), "fixed-slot exact rescale");
        Error(SqlStates.NumericValueOutOfRange, () => ExactDecimal.Parse("1.501").ToUnscaledInt64(2));
        Error(SqlStates.DataCorrupted, () => ExactDecimal.FromBase10000(false, 0, new ushort[] { 65535 }));
        Corpus.Equal(false, ExactDecimal.FromBase10000(true, 5, new ushort[] { 0, 0 }).Negative, "raw codec zero is normalized");
        Corpus.Equal(ushort.MaxValue, ExactDecimal.FromBase10000(false, ushort.MaxValue, ReadOnlySpan<ushort>.Empty).Scale, "raw codec retains scale for integrity reporting");
        CheckExactRoots(random);
        CheckMathMeter();
        Console.WriteLine("PASS decimal internals: 4000 independent bignum oracle trials, limb boundaries, immutable coefficients, canonical hashes, i64 and format caps");
    }

    private static BigInteger Magnitude(Random random, int digits)
    {
        var text = new char[digits];
        for (var i = 0; i < digits; i++) text[i] = (char)('0' + random.Next(10));
        return BigInteger.Parse(text, CultureInfo.InvariantCulture);
    }

    private static int DivisionScale(BigInteger a, int aScale, BigInteger b, int bScale)
    {
        static (int Weight, BigInteger First) Group(BigInteger coefficient, int scale)
        {
            coefficient = BigInteger.Abs(coefficient);
            if (coefficient.IsZero) return (0, 0);
            var exponent = coefficient.ToString(CultureInfo.InvariantCulture).Length - 1 - scale;
            var weight = (int)Math.Floor(exponent / 4.0);
            var shift = 4 * weight + scale;
            return (weight, shift >= 0 ? coefficient / BigInteger.Pow(10, shift) : coefficient * BigInteger.Pow(10, -shift));
        }
        var (aw, af) = Group(a, aScale);
        var (bw, bf) = Group(b, bScale);
        var weight = aw - bw - (af <= bf ? 1 : 0);
        return Math.Min(1000, Math.Max(Math.Max(16 - 4 * weight, 0), Math.Max(aScale, bScale)));
    }

    private static string Render(BigInteger coefficient, int scale)
    {
        var negative = coefficient.Sign < 0;
        var text = BigInteger.Abs(coefficient).ToString(CultureInfo.InvariantCulture);
        if (scale != 0) text = text.Length <= scale ? "0." + new string('0', scale - text.Length) + text : text.Insert(text.Length - scale, ".");
        return negative ? "-" + text : text;
    }

    private static void Equal(string expected, ExactDecimal actual, string label) => Corpus.Equal(expected, actual.ToString(), label);

    private static void CheckExactRoots(Random random)
    {
        for (var i = 0; i < 100; i++)
        {
            var coefficient = Magnitude(random, random.Next(1, 160));
            var scale = random.Next(0, 70);
            var number = ExactDecimal.Parse(Render(coefficient, scale));
            var weight = coefficient.IsZero ? 0 : (int)Math.Floor((coefficient.ToString(CultureInfo.InvariantCulture).Length - 1 - scale) / 4.0);
            var expectedScale = Math.Clamp(Math.Max(16 - (weight * 2 + 1), scale), 0, 1000);
            var shift = 2 * expectedScale - scale;
            var numerator = coefficient * BigInteger.Pow(10, Math.Max(shift, 0));
            var denominator = BigInteger.Pow(10, Math.Max(-shift, 0));
            var root = IntegerRoot(numerator / denominator);
            // Compare to the EXACT half-integer boundary; no guard-digit algorithm here.
            if (4 * numerator >= denominator * BigInteger.Pow(2 * root + 1, 2)) root++;
            Equal(Render(root, expectedScale), number.Sqrt(), "exact root rounding against rational midpoint");
        }
    }

    private static BigInteger IntegerRoot(BigInteger number)
    {
        if (number.IsZero) return number;
        var guess = BigInteger.One << (int)((number.GetBitLength() + 1) / 2);
        while (true)
        {
            var next = (guess + number / guess) / 2;
            if (next >= guess) return guess;
            guess = next;
        }
    }

    private static void CheckMathMeter()
    {
        var charges = new List<(long Work, long Bytes)>();
        _ = ExactDecimal.Parse("2.0").Sqrt((work, bytes) => charges.Add((work, bytes)));
        Corpus.Equal(2, charges.Count, "sqrt charges entry and guarded integer kernel");
        Corpus.Equal((81L, 280L), charges[0], "sqrt entry logical work");
        Corpus.Equal((1014L, 400L), charges[1], "sqrt guard logical work and bit-length factor");

        var input = ExactDecimal.Parse("2.0");
        var sentinel = new InvalidOperationException("test meter stop");
        foreach (var body in new Func<Action<long, long>, ExactDecimal>[]
        {
            input.Sqrt, input.Exp, input.Ln, input.Log10,
            meter => input.Log(ExactDecimal.Parse("3.0"), meter),
            meter => input.Power(ExactDecimal.Parse("3.5"), meter),
            meter => input.Power(ExactDecimal.FromInt64(99), meter),
        })
        {
            foreach (var stopAt in new[] { 1, 2 })
            {
                var calls = 0;
                try
                {
                    body((work, bytes) =>
                    {
                        if (work <= 0 || bytes <= 0) throw new InvalidDataException("invalid math meter charge");
                        if (++calls == stopAt) throw sentinel;
                    });
                    throw new InvalidDataException("decimal kernel swallowed the resource stop");
                }
                catch (InvalidOperationException error) when (ReferenceEquals(error, sentinel))
                {
                    Corpus.Equal(stopAt, calls, "resource stop propagates without fallback or replay");
                }
            }
        }
    }

    private static void Error(string state, Action body)
    {
        try { body(); }
        catch (SqlException error) { Corpus.Equal(state, error.SqlState, "decimal error"); return; }
        throw new InvalidDataException($"expected decimal error {state}");
    }
}
