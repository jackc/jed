namespace Jed;

internal static class IntervalMath
{
    // Fixed integer numerators over a positive denominator. The decimal engine's
    // native limbs avoid a machine-integer ceiling on products of the factor.
    internal static SqlInterval Scale(SqlInterval interval, ExactDecimal factor, bool divide = false, bool integerFactor = false)
    {
        factor = factor.TrimScale();
        if (!integerFactor && (Math.Max(0, factor.Digits - factor.Scale) > 18 || factor.Scale > 18)) throw TemporalValue.Overflow();
        var numerator = ExactDecimal.Parse((factor.Negative ? "-" : "") + factor.CoefficientDigits());
        var denominator = ExactDecimal.Parse("1" + new string('0', factor.Scale));
        if (divide)
        {
            if (numerator.IsZero) throw new SqlException(SqlStates.DivisionByZero, "division by zero");
            (numerator, denominator) = (denominator, numerator);
            if (denominator.Negative) { numerator = numerator.Negate(); denominator = denominator.Negate(); }
        }
        static ExactDecimal Integer(long value) => ExactDecimal.FromInt64(value);
        static ExactDecimal Quotient(ExactDecimal a, ExactDecimal b) => a.DivideAtScale(b, 0, round: false);
        static ExactDecimal Even(ExactDecimal value, ExactDecimal denominator)
        {
            var whole = Quotient(value, denominator);
            var remainder = value.Subtract(whole.Multiply(denominator));
            var comparison = remainder.Abs().Multiply(Integer(2)).CompareTo(denominator);
            if (comparison > 0 || comparison == 0 && !whole.Remainder(Integer(2)).IsZero)
                whole = whole.Add(Integer(value.Negative ? -1 : 1));
            return whole;
        }
        var monthProduct = Integer(interval.Months).Multiply(numerator);
        var months = Quotient(monthProduct, denominator);
        var monthFraction = monthProduct.Subtract(months.Multiply(denominator));
        var microDays = Even(monthFraction.Multiply(Integer(30_000_000)), denominator);
        var monthDays = Quotient(microDays, Integer(1_000_000));
        var fractionMicroDays = microDays.Subtract(monthDays.Multiply(Integer(1_000_000)));
        var dayProduct = Integer(interval.Days).Multiply(numerator);
        var days = Quotient(dayProduct, denominator);
        var dayFraction = dayProduct.Subtract(days.Multiply(denominator));
        var remainder = Even(dayFraction.Multiply(Integer(TemporalValue.MicrosPerDay)).Add(fractionMicroDays.Multiply(Integer(86400)).Multiply(denominator)), denominator);
        var wholeDays = Quotient(remainder, Integer(TemporalValue.MicrosPerDay));
        days = days.Add(wholeDays).Add(monthDays);
        remainder = remainder.Subtract(wholeDays.Multiply(Integer(TemporalValue.MicrosPerDay)));
        var micros = Even(Integer(interval.Micros).Multiply(numerator).Add(remainder.Multiply(denominator)), denominator);
        try { return SqlInterval.Create(months.ToInt64(), days.ToInt64(), micros.ToInt64()); }
        catch (SqlException error) when (error.SqlState == SqlStates.NumericValueOutOfRange) { throw TemporalValue.Overflow(); }
    }

    internal static SqlInterval Age(long first, long second, long? firstWall = null, long? secondWall = null)
    {
        if (TemporalValue.IsInfinite(first) || TemporalValue.IsInfinite(second)) throw TemporalValue.Overflow();
        var left = firstWall ?? first;
        var right = secondWall ?? second;
        var a = TemporalValue.CivilFromDays(TemporalValue.FloorDiv(left, TemporalValue.MicrosPerDay));
        var b = TemporalValue.CivilFromDays(TemporalValue.FloorDiv(right, TemporalValue.MicrosPerDay));
        var sign = first < second ? -1L : 1L;
        var years = (a.Year - b.Year) * sign;
        long months = (a.Month - b.Month) * sign, days = (a.Day - b.Day) * sign;
        var micros = (TemporalValue.FloorMod(left, TemporalValue.MicrosPerDay) - TemporalValue.FloorMod(right, TemporalValue.MicrosPerDay)) * sign;
        if (micros < 0) { micros += TemporalValue.MicrosPerDay; days--; }
        var earlier = sign < 0 ? a : b;
        while (days < 0) { days += TemporalValue.DaysInMonth(earlier.Year, earlier.Month); months--; }
        while (months < 0) { months += 12; years--; }
        return SqlInterval.Create((years * 12 + months) * sign, days * sign, micros * sign);
    }
}
