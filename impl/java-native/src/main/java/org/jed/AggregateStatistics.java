package org.jed;

import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.util.Set;

/** Exact decimal moments and separately rounded Youngs–Cramer floating moments. */
final class AggregateStatistics {
    private static final Set<String> UNARY = Set.of("stddev", "stddev_pop", "stddev_samp", "variance", "var_pop", "var_samp");
    private static final Set<String> BINARY = Set.of("corr", "covar_pop", "covar_samp", "regr_count", "regr_avgx", "regr_avgy", "regr_sxx", "regr_syy", "regr_sxy", "regr_slope", "regr_intercept", "regr_r2");
    static boolean unary(String name) { return UNARY.contains(name); }
    static boolean binary(String name) { return BINARY.contains(name); }
    static boolean accepts(String name) { return unary(name) || binary(name); }
    private final String name;
    private final boolean exact;
    private Decimal sum = Decimal.zero(0), squares = Decimal.zero(0);
    private double sx, sy, sxx, syy, sxy;
    AggregateStatistics(String name, SqlType type) { this.name = name; exact = type == SqlType.DECIMAL; }
    void add(SqlValue value, long count, CostMeter meter) {
        if (exact) {
            Decimal x = value.type().isInteger() ? Decimal.of(value.asInteger()) : value.asDecimal();
            ScalarPlan.decimalWork("*", x, x, meter); Decimal square = x.multiplyUncapped(x);
            ScalarPlan.decimalWork("+", sum, x, meter); sum = sum.addUncapped(x);
            ScalarPlan.decimalWork("+", squares, square, meter); squares = squares.addUncapped(square); return;
        }
        if (name.equals("regr_count")) return;
        if (!binary(name)) {
            double x = floating(value), old = sx; sx += x;
            if (count == 1) { if (!Double.isFinite(x)) sxx = Double.NaN; }
            else {
                double delta = x * count - sx; sxx += delta * delta / ((double)count * (count - 1));
                if (Double.isInfinite(sx) || Double.isInfinite(sxx)) { if (Double.isFinite(old) && Double.isFinite(x)) throw SqlException.numericOverflow(); sxx = Double.NaN; }
            }
            return;
        }
        double y = floating(value.asComposite().get(0)), x = floating(value.asComposite().get(1));
        double oldX = sx, oldY = sy; sx += x; sy += y;
        if (count == 1) { if (!Double.isFinite(x)) sxx = sxy = Double.NaN; if (!Double.isFinite(y)) syy = sxy = Double.NaN; return; }
        double tx = x * count - sx, ty = y * count - sy, factor = 1.0 / ((double)count * (count - 1));
        sxx += tx * tx * factor; syy += ty * ty * factor; sxy += tx * ty * factor;
        if (Double.isInfinite(sx) || Double.isInfinite(sxx)) { if (Double.isFinite(oldX) && Double.isFinite(x)) throw SqlException.numericOverflow(); sxx = Double.NaN; }
        if (Double.isInfinite(sy) || Double.isInfinite(syy)) { if (Double.isFinite(oldY) && Double.isFinite(y)) throw SqlException.numericOverflow(); syy = Double.NaN; }
        if (Double.isInfinite(sxy)) { if (Double.isFinite(oldX) && Double.isFinite(x) && Double.isFinite(oldY) && Double.isFinite(y)) throw SqlException.numericOverflow(); sxy = Double.NaN; }
    }
    private static double floating(SqlValue value) { return value.type().isFloat() ? value.asFloat() : value.type().isInteger() ? value.asInteger() : Double.parseDouble(value.asDecimal().toString()); }
    SqlValue finish(long count) {
        if (name.equals("regr_count")) return SqlValue.ofInteger(count);
        SqlType type = exact ? SqlType.DECIMAL : SqlType.F64;
        boolean sample = name.equals("var_samp") || name.equals("variance") || name.equals("stddev_samp") || name.equals("stddev") || name.equals("covar_samp");
        if (count == 0 || sample && count < 2) return SqlValue.nullValue(type);
        if (exact) {
            Decimal n = Decimal.of(count), numerator = n.multiplyUncapped(squares).addUncapped(sum.multiplyUncapped(sum).negate());
            if (numerator.isZero() || numerator.negative()) return SqlValue.ofDecimal(Decimal.zero(0));
            Decimal denominator = n.multiplyUncapped(Decimal.of(sample ? count - 1 : count));
            int scale = numerator.divisionScale(denominator); Decimal variance = numerator.divideInternal(denominator, scale);
            return SqlValue.ofDecimal((name.startsWith("stddev") ? variance.squareRootAtScale(scale) : variance).checked());
        }
        if (!binary(name)) { double variance = sxx / (sample ? count - 1 : count); return SqlValue.ofFloat(SqlType.F64, name.startsWith("stddev") ? Math.sqrt(variance) : variance); }
        if ((name.equals("corr") && (sxx == 0 || syy == 0)) || Set.of("regr_slope", "regr_intercept", "regr_r2").contains(name) && sxx == 0) return SqlValue.nullValue(type);
        double result = switch (name) {
            case "covar_pop" -> sxy / count; case "covar_samp" -> sxy / (count - 1);
            case "corr" -> sxy / Math.sqrt(sxx * syy);
            case "regr_avgx" -> sx / count; case "regr_avgy" -> sy / count;
            case "regr_sxx" -> sxx; case "regr_syy" -> syy; case "regr_sxy" -> sxy;
            case "regr_slope" -> sxy / sxx; case "regr_intercept" -> (sy - sx * sxy / sxx) / count;
            case "regr_r2" -> syy == 0 ? 1 : sxy * sxy / (sxx * syy);
            default -> throw new AssertionError(name);
        };
        return SqlValue.ofFloat(SqlType.F64, result);
    }
    long packedBytes() { return exact ? 3 * MemorySchedule.VALUE + decimalBytes(sum) + decimalBytes(squares) : (name.equals("regr_count") ? 1 : binary(name) ? 6 : 3) * MemorySchedule.VALUE; }
    private static long decimalBytes(Decimal value) { return value.isZero() ? 0 : ((value.precision() + 3L) / 4) * MemorySchedule.DECIMAL_GROUP; }
    void write(DataOutputStream out) throws IOException { sum.writeState(out); squares.writeState(out); out.writeDouble(sx); out.writeDouble(sy); out.writeDouble(sxx); out.writeDouble(syy); out.writeDouble(sxy); }
    void read(DataInputStream in) throws IOException { sum = Decimal.readState(in); squares = Decimal.readState(in); sx = in.readDouble(); sy = in.readDouble(); sxx = in.readDouble(); syy = in.readDouble(); sxy = in.readDouble(); }
}
