package org.jed;

/** Floating kernels and their SQL domain checks; transcendentals use the declared libm exemption. */
final class FloatFunctions {
    private FloatFunctions() {}
    private static final double RADIANS_PER_DEGREE = 0.017453292519943295;

    static double evaluate(String name, SqlValue[] values) {
        if (name.equals("pi")) return Math.PI;
        double x = values[0].asFloat();
        double y = values.length > 1 ? values[1].asFloat() : 0;
        return switch (name) {
            case "sqrt" -> {
                if (x < 0) throw new SqlException("2201F", "cannot take square root of a negative number");
                yield Math.sqrt(x);
            }
            case "ln", "log10" -> {
                if (x <= 0) throw new SqlException("2201E", "cannot take logarithm of a non-positive number");
                yield name.equals("ln") ? Math.log(x) : Math.log10(x);
            }
            case "exp" -> checked(Math.exp(x), x, true);
            case "pow", "power" -> power(x, y);
            case "radians" -> checked(x * RADIANS_PER_DEGREE, x, x != 0);
            case "degrees" -> checked(x / RADIANS_PER_DEGREE, x, x != 0);
            case "sin", "cos", "tan", "cot" -> {
                if (Double.isInfinite(x)) throw SqlException.numericOverflow();
                yield switch (name) {
                    case "sin" -> Math.sin(x);
                    case "cos" -> Math.cos(x);
                    case "tan" -> Math.tan(x);
                    default -> 1 / Math.tan(x);
                };
            }
            case "asin", "acos" -> {
                if (x < -1 || x > 1) throw SqlException.numericOverflow();
                yield name.equals("asin") ? Math.asin(x) : Math.acos(x);
            }
            case "atan" -> Math.atan(x);
            case "atan2" -> Math.atan2(x, y);
            case "cbrt" -> Math.cbrt(x);
            case "sinh" -> Math.sinh(x);
            case "cosh" -> Math.cosh(x);
            case "tanh" -> Math.tanh(x);
            case "asinh" -> {
                double magnitude = Math.abs(x);
                double result = magnitude > 1e154 ? Math.log(magnitude) + Math.log(2)
                    : Math.log1p(magnitude + magnitude * magnitude / (1 + Math.sqrt(1 + magnitude * magnitude)));
                yield Math.copySign(result, x);
            }
            case "acosh" -> {
                if (x < 1 || Double.isInfinite(x)) throw SqlException.numericOverflow();
                yield x > 1e154 ? Math.log(x) + Math.log(2) : Math.log1p(x - 1 + Math.sqrt(x - 1) * Math.sqrt(x + 1));
            }
            case "atanh" -> {
                if (x < -1 || x > 1) throw SqlException.numericOverflow();
                yield 0.5 * Math.log1p(2 * x / (1 - x));
            }
            default -> throw new IllegalStateException("missing floating kernel: " + name);
        };
    }

    private static double checked(double result, double operand, boolean nonzero) {
        if (Double.isFinite(operand) && (Double.isInfinite(result) || nonzero && result == 0)) {
            throw SqlException.numericOverflow();
        }
        return result;
    }

    private static double power(double x, double y) {
        if (y == 0 || x == 1) return 1;
        if (Double.isNaN(x) || Double.isNaN(y)) return Double.NaN;
        if (x == 0 && y < 0) throw new SqlException("2201F", "zero raised to a negative power is undefined");
        if (Double.isInfinite(y)) {
            double magnitude = Math.abs(x);
            if (magnitude == 1) return 1;
            return (magnitude > 1) == (y > 0) ? Double.POSITIVE_INFINITY : 0;
        }
        boolean integral = Math.floor(y) == y;
        boolean odd = integral && Math.abs(y) < 0x1.0p53 && ((long) Math.abs(y) & 1) != 0;
        if (Double.isInfinite(x)) {
            double result = y > 0 ? Double.POSITIVE_INFINITY : 0;
            return x < 0 && odd ? -result : result;
        }
        if (x < 0 && !integral) throw new SqlException("2201F", "a negative number raised to a non-integer power yields a complex result");
        double result = Math.pow(x, y);
        if (Double.isInfinite(result) || x != 0 && result == 0) throw SqlException.numericOverflow();
        return result;
    }
}
