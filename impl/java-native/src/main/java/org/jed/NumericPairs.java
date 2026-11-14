package org.jed;

import java.util.List;
import java.util.Set;

/** Numeric functions whose argument and result types are determined as an operand pair. */
final class NumericPairs {
    private NumericPairs() {}
    private static final Set<String> PAIRS = Set.of("mod", "div", "gcd", "lcm");

    static ScalarPlan.Expression resolve(String name, List<ScalarPlan.Expression> arguments, List<String> origins) {
        if (name.equals("width_bucket")) return widthBucket(arguments, origins);
        if (!PAIRS.contains(name)) return null;
        if (arguments.size() != 2) throw missing(name);
        SqlType left = arguments.get(0).type(), right = arguments.get(1).type();
        if (origins.get(0).equals("unknown") && origins.get(1).equals("unknown")) left = right = SqlType.I64;
        if (origins.get(0).equals("unknown")) left = right;
        if (origins.get(1).equals("unknown")) right = left;
        if (!(left.isInteger() || left.kind() == SqlType.Kind.DECIMAL) || !(right.isInteger() || right.kind() == SqlType.Kind.DECIMAL)) throw missing(name);
        SqlType result = name.equals("div") || left.kind() == SqlType.Kind.DECIMAL || right.kind() == SqlType.Kind.DECIMAL ? SqlType.DECIMAL : ScalarPlan.common(left, right);
        return new ScalarPlan.Expression(result, meter -> {
            meter.operation();
            SqlValue a = arguments.get(0).run(meter);
            if (a.isNull()) return SqlValue.nullValue(result);
            SqlValue b = arguments.get(1).run(meter);
            if (b.isNull()) return SqlValue.nullValue(result);
            if (result.isInteger()) {
                long x = a.asInteger(), y = b.asInteger(), value;
                if (name.equals("mod")) {
                    if (y == 0) throw new SqlException("22012", "division by zero");
                    value = x % y;
                } else if (name.equals("gcd")) value = gcd(x, y);
                else if (x == 0 || y == 0) value = 0;
                else {
                    try { value = Math.multiplyExact(x / gcd(x, y), y); }
                    catch (ArithmeticException failure) { throw SqlException.numericOverflow(); }
                    if (value == Long.MIN_VALUE) throw SqlException.numericOverflow();
                    value = Math.abs(value);
                }
                return SqlValue.ofInteger(result, value);
            }
            Decimal x = decimal(a), y = decimal(b);
            Decimal value;
            if (name.equals("mod")) {
                if (y.isZero()) throw new SqlException("22012", "division by zero");
                int scale = Math.max(x.scale(), y.scale());
                long work = x.groupsAtScale(scale) * y.groupsAtScale(scale);
                meter.charge((work - 1) * Costs.DECIMAL_WORK); meter.guard();
                value = x.modulo(y);
            } else if (name.equals("div")) value = x.addUncapped(x.modulo(y).negate()).divideInternal(y, 0).checked();
            else if (name.equals("gcd")) value = gcd(x, y);
            else {
                int scale = Math.max(x.scale(), y.scale());
                value = x.isZero() || y.isZero() ? Decimal.zero(scale) : x.divide(gcd(x, y)).multiply(y).abs().rescale(scale);
            }
            return SqlValue.ofDecimal(value);
        });
    }

    private static long gcd(long x, long y) {
        while (y != 0) { long remainder = x % y; x = y; y = remainder; }
        if (x == Long.MIN_VALUE) throw SqlException.numericOverflow();
        return Math.abs(x);
    }
    private static Decimal gcd(Decimal x, Decimal y) {
        int scale = Math.max(x.scale(), y.scale());
        while (!y.isZero()) { Decimal remainder = x.modulo(y); x = y; y = remainder; }
        return x.abs().rescale(scale);
    }
    private static Decimal decimal(SqlValue value) { return value.type().isInteger() ? Decimal.of(value.asInteger()) : value.asDecimal(); }
    private static SqlException missing(String name) { return new SqlException("42883", "function does not exist for argument types: " + name); }
    private static SqlException bucketError(String text) { return new SqlException("2201G", text); }

    private static ScalarPlan.Expression widthBucket(List<ScalarPlan.Expression> arguments, List<String> origins) {
        if (arguments.size() != 4) throw missing("width_bucket");
        boolean floating = false;
        for (int i = 0; i < 3; i++) if (arguments.get(i).type().isFloat()) floating = true;
        for (int i = 0; i < 3; i++) {
            SqlType type = arguments.get(i).type();
            if (origins.get(i).equals("unknown")) continue;
            if (floating && !type.isFloat() && !origins.get(i).equals("integer_literal") && !origins.get(i).equals("decimal_literal")) throw missing("width_bucket");
            if (!type.isFloat() && !type.isInteger() && type.kind() != SqlType.Kind.DECIMAL) throw missing("width_bucket");
        }
        if (!arguments.get(3).type().isInteger() && !origins.get(3).equals("unknown")) throw missing("width_bucket");
        boolean useFloat = floating;
        return new ScalarPlan.Expression(SqlType.I32, meter -> {
            meter.operation();
            SqlValue[] values = new SqlValue[4];
            for (int i = 0; i < 4; i++) {
                values[i] = arguments.get(i).run(meter);
                if (values[i].isNull()) return SqlValue.nullValue(SqlType.I32);
                if (i < 3 && useFloat) values[i] = SqlCasts.cast(values[i], SqlType.F64);
            }
            long count = values[3].asInteger();
            if (count <= 0) throw bucketError("count must be greater than zero");
            long result = useFloat ? bucketFloat(values[0].asFloat(), values[1].asFloat(), values[2].asFloat(), count)
                : bucketDecimal(decimal(values[0]), decimal(values[1]), decimal(values[2]), count);
            return SqlValue.ofInteger(SqlType.I32, result);
        });
    }
    private static long beyond(long count) { return count == Long.MAX_VALUE ? count : count + 1; }
    private static long bucketFloat(double operand, double low, double high, long count) {
        if (Double.isNaN(operand) || !Double.isFinite(low) || !Double.isFinite(high) || low == high) throw bucketError("invalid width_bucket bounds or operand");
        if (low < high) { if (operand < low) return 0; if (operand >= high) return beyond(count); }
        else { if (operand > low) return 0; if (operand <= high) return beyond(count); }
        double fraction = Double.isInfinite(high - low) ? (operand / 2 - low / 2) / (high / 2 - low / 2) : (operand - low) / (high - low);
        double bucket = Math.floor(fraction * count);
        if (bucket >= count) return count;
        return (long) bucket + 1;
    }
    private static long bucketDecimal(Decimal operand, Decimal low, Decimal high, long count) {
        int direction = low.compareTo(high);
        if (direction == 0) throw bucketError("lower bound cannot equal upper bound");
        Decimal numerator, denominator;
        if (direction < 0) {
            if (operand.compareTo(low) < 0) return 0;
            if (operand.compareTo(high) >= 0) return beyond(count);
            numerator = operand.addUncapped(low.negate()); denominator = high.addUncapped(low.negate());
        } else {
            if (operand.compareTo(low) > 0) return 0;
            if (operand.compareTo(high) <= 0) return beyond(count);
            numerator = low.addUncapped(operand.negate()); denominator = low.addUncapped(high.negate());
        }
        numerator = numerator.multiplyInternal(Decimal.of(count), numerator.scale());
        long left = 0, right = count;
        while (right - left > 1) {
            long middle = left + (right - left) / 2;
            Decimal product = denominator.multiplyInternal(Decimal.of(middle), denominator.scale());
            if (product.compareTo(numerator) <= 0) left = middle; else right = middle;
        }
        return left + 1;
    }
}
