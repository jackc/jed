package org.jed;

/** Bounded exact rational implementation of the two PostgreSQL TSROUND quantizations. */
final class IntervalScale {
    private IntervalScale() {}
    static SqlInterval apply(SqlInterval value, Decimal factor, boolean divide) {
        if (divide && factor.isZero()) throw new SqlException("22012", "division by zero");
        if (value.months() == 0 && value.days() == 0 && value.micros() == 0) return value;
        String digits = factor.coefficientDigits();
        int scale = factor.scale();
        while (scale > 0 && digits.endsWith("0")) { digits = digits.substring(0, digits.length() - 1); scale--; }
        if (Math.max(0, digits.length() - scale) > 18 || scale > 18)
            throw new SqlException("22008", "interval factor has too many digits");
        Decimal numerator = Decimal.parse((factor.negative() ? "-" : "") + digits);
        Decimal denominator = Decimal.parse("1" + "0".repeat(scale));
        if (divide && numerator.isZero()) throw new SqlException("22012", "division by zero");
        Rational ratio = divide ? new Rational(denominator, numerator) : new Rational(numerator, denominator);
        try {
            Rational months = ratio.multiply(value.months()), days = ratio.multiply(value.days());
            int wholeMonths = integer(months.truncate()), wholeDays = integer(days.truncate());
            Rational monthDays = new Rational(months.subtract(wholeMonths).multiply(30_000_000).roundEven(), Decimal.of(1_000_000));
            long extraDays = monthDays.truncate().toLongExact();
            Rational remainder = monthDays.subtract(extraDays).add(days.subtract(wholeDays)).multiply(Temporal.DAY_MICROS);
            long remainderMicros = remainder.roundEven().toLongExact();
            long carried = remainderMicros / Temporal.DAY_MICROS;
            remainderMicros %= Temporal.DAY_MICROS;
            int resultDays = Math.toIntExact(Math.addExact(Math.addExact(wholeDays, extraDays), carried));
            long resultMicros = ratio.multiply(value.micros()).add(Rational.of(remainderMicros)).roundEven().toLongExact();
            return new SqlInterval(wholeMonths, resultDays, resultMicros);
        } catch (ArithmeticException failure) { throw Temporal.overflow(); }
        catch (SqlException failure) { if (failure.sqlState().equals("22003")) throw Temporal.overflow(); throw failure; }
    }
    private static int integer(Decimal value) { return Math.toIntExact(value.toLongExact()); }
    private record Rational(Decimal numerator, Decimal denominator) {
        Rational {
            if (denominator.negative()) { numerator = numerator.negate(); denominator = denominator.negate(); }
        }
        static Rational of(long value) { return new Rational(Decimal.of(value), Decimal.of(1)); }
        Rational multiply(long value) { return new Rational(numerator.multiply(Decimal.of(value)), denominator); }
        Rational add(Rational other) {
            return new Rational(numerator.multiply(other.denominator).add(other.numerator.multiply(denominator)), denominator.multiply(other.denominator));
        }
        Rational subtract(long value) { return new Rational(numerator.subtract(denominator.multiply(Decimal.of(value))), denominator); }
        Decimal truncate() { return numerator.subtract(numerator.modulo(denominator)).divideInternal(denominator, 0); }
        Decimal roundEven() {
            Decimal whole = truncate(), remainder = numerator.modulo(denominator);
            int comparison = remainder.abs().multiply(Decimal.of(2)).compareTo(denominator);
            if (comparison > 0 || comparison == 0 && !whole.modulo(Decimal.of(2)).isZero())
                return whole.add(Decimal.of(numerator.negative() ? -1 : 1));
            return whole;
        }
    }
}
