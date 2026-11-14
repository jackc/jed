package org.jed;

/** Metered exact-numeric functions. Internal scratch uses the same scalar allowance as amplifiers. */
final class DecimalFunctions {
    private DecimalFunctions() {}

    static Decimal sqrt(Decimal value, CostMeter meter) {
        step(meter, 0, false, value);
        int scale = Math.min(Decimal.MAX_PRECISION,
            Math.max(Math.max(16 - (value.base10000Weight() * 2 + 1), value.scale()), 0));
        if (value.negative()) throw new SqlException("2201F", "cannot take square root of a negative number");
        if (!value.isZero()) step(meter, scale, true, value);
        return value.squareRootAtScale(scale).checked();
    }

    private static final Decimal ONE = Decimal.of(1);
    private static final Decimal TWO = Decimal.of(2);
    private static final Decimal TEN = Decimal.of(10);
    private static final Decimal LOWER = Decimal.parse("0.9");
    private static final Decimal UPPER = Decimal.parse("1.1");

    private static double estimate(Decimal value) { return Double.parseDouble(value.toString()); }
    private static int displayScale(int weight, Decimal... values) {
        int scale = Math.max(16 - weight, 0);
        for (Decimal value : values) scale = Math.max(scale, value.scale());
        return Math.min(1000, scale);
    }
    private static void positiveLog(Decimal value) {
        if (value.isZero()) throw new SqlException("2201E", "cannot take logarithm of zero");
        if (value.negative()) throw new SqlException("2201E", "cannot take logarithm of a negative number");
    }
    private static Decimal root(Decimal value, int scale, CostMeter meter) {
        if (!value.isZero()) step(meter, scale, true, value);
        return value.squareRootAtScale(scale);
    }

    static Decimal exp(Decimal value, CostMeter meter) {
        step(meter, 0, false, value);
        double weight = Math.max(-2000, Math.min(2000, estimate(value) * 0.434294481903252));
        return exponential(value, displayScale((int) weight, value), meter).checked();
    }

    private static Decimal exponential(Decimal value, int scale, CostMeter meter) {
        step(meter, scale, false, value);
        double estimate = estimate(value);
        if (Math.abs(estimate) >= 6000) {
            if (estimate > 0) throw SqlException.numericOverflow();
            return Decimal.zero(Math.max(scale, 0));
        }
        int weight = (int) (estimate * 0.434294481903252);
        int halves = 0;
        while (Math.abs(estimate) > 0.01) { halves++; estimate /= 2; }
        Decimal x = halves == 0 ? value : value.divideInternal(Decimal.of(1L << halves), value.scale() + halves);
        int significant = Math.max(1 + weight + scale + (int) (halves * 0.301029995663981), 0) + 8;
        int local = significant - 1;
        Decimal result = ONE.addUncapped(x);
        Decimal term = x.multiplyInternal(x, local).divideInternal(TWO, local);
        long denominator = 2;
        while (!term.isZero()) {
            step(meter, local, false, result, term, x);
            result = result.addUncapped(term);
            term = term.multiplyInternal(x, local).divideInternal(Decimal.of(++denominator), local);
        }
        for (int i = 0; i < halves; i++) {
            step(meter, scale, false, result);
            result = result.multiplyInternal(result, Math.max(significant - result.base10000Weight() * 8, 0));
        }
        return result.roundInternal(scale);
    }

    private static Decimal logarithm(Decimal value, int scale, CostMeter meter) {
        step(meter, scale, false, value);
        Decimal x = value;
        Decimal factor = TWO;
        int roots = 0;
        while (x.compareTo(LOWER) <= 0) {
            x = root(x, scale - x.base10000Weight() * 2 + 8, meter);
            factor = factor.multiplyInternal(TWO, 0); roots++;
        }
        while (x.compareTo(UPPER) >= 0) {
            x = root(x, scale - x.base10000Weight() * 2 + 8, meter);
            factor = factor.multiplyInternal(TWO, 0); roots++;
        }
        int local = scale + (int) ((roots + 1) * 0.301029995663981) + 8;
        Decimal result = x.addUncapped(ONE.negate()).divideInternal(x.addUncapped(ONE), local);
        Decimal power = result;
        Decimal square = result.multiplyInternal(result, local);
        long denominator = 1;
        while (true) {
            step(meter, local, false, result, power, square);
            denominator += 2;
            power = power.multiplyInternal(square, local);
            Decimal term = power.divideInternal(Decimal.of(denominator), local);
            if (term.isZero()) break;
            result = result.addUncapped(term);
            if (term.base10000Weight() < result.base10000Weight() - local * 2 / 4) break;
        }
        return result.multiplyInternal(factor, scale);
    }

    private static int logarithmWeight(Decimal value, CostMeter meter) {
        if (value.compareTo(LOWER) >= 0 && value.compareTo(UPPER) <= 0) {
            Decimal difference = value.addUncapped(ONE.negate());
            return difference.isZero() ? 0 : difference.precision() - 1 - difference.scale();
        }
        Decimal log = logarithm(value, 20, meter);
        if (log.isZero()) return 0;
        int weight = log.precision() - 1 - log.scale();
        return weight < 0 ? weight + 1 : weight;
    }

    static Decimal ln(Decimal value, CostMeter meter) {
        step(meter, 0, false, value); positiveLog(value);
        return logarithm(value, displayScale(logarithmWeight(value, meter), value), meter).checked();
    }
    static Decimal log10(Decimal value, CostMeter meter) {
        step(meter, 0, false, value);
        return log(TEN, value, meter);
    }
    static Decimal log(Decimal base, Decimal value, CostMeter meter) {
        step(meter, 0, false, base, value); positiveLog(base); positiveLog(value);
        int baseWeight = logarithmWeight(base, meter);
        int valueWeight = logarithmWeight(value, meter);
        int weight = valueWeight - baseWeight;
        int scale = displayScale(weight, base, value);
        Decimal baseLog = logarithm(base, Math.max(scale + weight - baseWeight + 8, 0), meter);
        Decimal valueLog = logarithm(value, Math.max(scale + weight - valueWeight + 8, 0), meter);
        return valueLog.divideInternal(baseLog, scale).checked();
    }

    static Decimal power(Decimal base, Decimal exponent, CostMeter meter) {
        step(meter, 0, false, base, exponent);
        if (base.isZero() && exponent.negative()) throw new SqlException("2201F", "zero raised to a negative power is undefined");
        Decimal integral = exponent.truncate(0);
        boolean isInteger = integral.compareTo(exponent) == 0;
        if (isInteger && exponent.compareTo(Decimal.of(Integer.MIN_VALUE)) >= 0 && exponent.compareTo(Decimal.of(Integer.MAX_VALUE)) <= 0) {
            return integerPower(base, (int) exponent.toLongExact(), exponent.scale(), meter);
        }
        if (base.isZero()) return Decimal.zero(16);
        boolean negative = false;
        if (base.negative()) {
            if (!isInteger) throw new SqlException("2201F", "a negative number raised to a non-integer power yields a complex result");
            String coefficient = integral.coefficientDigits();
            negative = (coefficient.charAt(coefficient.length() - 1) - '0') % 2 != 0;
            base = base.abs();
        }
        int logWeight = logarithmWeight(base, meter);
        int local = Math.max(8 - logWeight, 0);
        Decimal log = logarithm(base, local, meter).multiplyInternal(exponent, local);
        double estimate = estimate(log);
        if (Math.abs(estimate) > 6020) {
            if (estimate > 0) throw SqlException.numericOverflow();
            return Decimal.zero(1000);
        }
        int weight = (int) (estimate * 0.434294481903252);
        int scale = displayScale(weight, base, exponent);
        local = Math.max(Math.max(scale + weight, 0) - logWeight + 8, 0);
        log = logarithm(base, local, meter).multiplyInternal(exponent, local);
        Decimal result = exponential(log, scale, meter);
        return (negative ? result.negate() : result).checked();
    }

    private static Decimal integerPower(Decimal base, int exponent, int exponentScale, CostMeter meter) {
        double estimate = 0;
        if (!base.isZero()) {
            Decimal log = logarithm(base.abs(), 30, meter).divideInternal(logarithm(TEN, 30, meter), 30);
            estimate = estimate(log.multiplyInternal(Decimal.of(exponent), log.scale()));
        }
        if (estimate > 131072) throw SqlException.numericOverflow();
        if (estimate + 1 < -1000) return Decimal.zero(1000);
        int weight = (int) estimate;
        int scale = Math.min(1000, Math.max(displayScale(weight, base), exponentScale));
        if (exponent == 0) return ONE.roundInternal(scale);
        if (exponent == 1) return base.roundInternal(scale);
        if (exponent == -1) return ONE.divideInternal(base, scale).checked();
        if (exponent == 2) return base.multiplyInternal(base, scale).checked();
        if (base.isZero()) return Decimal.zero(scale);
        long mask = Math.abs((long) exponent);
        int guard = (int) logarithm(Decimal.of(mask), 12, meter).truncate(0).toLongExact();
        int significant = 1 + scale + weight + guard + 8;
        Decimal product = base;
        Decimal result = (mask & 1) == 1 ? base : ONE;
        boolean overflowed = false;
        while ((mask >>>= 1) != 0) {
            step(meter, scale, false, product, result);
            int local = Math.max(Math.min(significant - 8 * product.base10000Weight(), 2 * product.scale()), 0);
            product = product.multiplyInternal(product, local);
            if ((mask & 1) == 1) {
                local = Math.max(Math.min(significant - 4 * (product.base10000Weight() + result.base10000Weight()), product.scale() + result.scale()), 0);
                result = product.multiplyInternal(result, local);
            }
            if (product.base10000Weight() > 32767 || result.base10000Weight() > 32767) {
                if (exponent > 0) throw SqlException.numericOverflow();
                result = Decimal.zero(0); overflowed = true; break;
            }
        }
        return (exponent < 0 && !overflowed ? ONE.divideInternal(result, scale) : result.roundInternal(scale)).checked();
    }

    static void step(CostMeter meter, int scale, boolean squareRoot, Decimal... operands) {
        long digits = 32L + Math.abs((long) scale);
        for (Decimal value : operands) digits += value.precision() + (long) value.scale();
        long groups = (digits + 3) / 4;
        long work = CostMeter.saturatingMultiply(groups, groups);
        if (squareRoot) work = CostMeter.saturatingMultiply(work, 64 - Long.numberOfLeadingZeros(digits));
        meter.charge(CostMeter.saturatingMultiply(Costs.DECIMAL_TRANSCEND, work)); meter.guard();
        meter.scalarReserve(CostMeter.saturatingMultiply(8, digits));
    }
}
