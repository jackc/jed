package org.jed;

/** PostgreSQL float text: shortest decimal strictly inside both rounding bounds. */
final class SqlFloat {
    private SqlFloat() {}

    static String render(double number, boolean narrow) {
        if (Double.isNaN(number)) return "NaN";
        if (number == Double.POSITIVE_INFINITY) return "Infinity";
        if (number == Double.NEGATIVE_INFINITY) return "-Infinity";
        boolean negative = Double.doubleToRawLongBits(number) < 0;
        if (number == 0) return negative ? "-0" : "0";
        double magnitude = Math.abs(number);
        Decimal value = Decimal.fromDoubleExact(magnitude);
        double previous = narrow ? Math.nextDown((float) magnitude) : Math.nextDown(magnitude);
        double next = narrow ? Math.nextUp((float) magnitude) : Math.nextUp(magnitude);
        Decimal lower = midpoint(value, Decimal.fromDoubleExact(previous));
        Decimal upper = Double.isInfinite(next)
            ? value.add(value.subtract(Decimal.fromDoubleExact(previous)).multiply(Decimal.parse("0.5")))
            : midpoint(value, Decimal.fromDoubleExact(next));
        String digits = value.coefficientDigits();
        int exponent = digits.length() - value.scale() - 1;
        for (int count = 1; count <= (narrow ? 9 : 17); count++) {
            int take = Math.min(count, digits.length());
            String coefficient = digits.substring(0, take) + "0".repeat(count - take);
            int power = exponent + 1 - count;
            Decimal down = Decimal.parse(coefficient + "e" + power);
            Decimal up = down.add(Decimal.parse("1e" + power));
            boolean downFits = down.compareTo(lower) > 0 && down.compareTo(upper) < 0;
            boolean upFits = up.compareTo(lower) > 0 && up.compareTo(upper) < 0;
            if (!downFits && !upFits) continue;
            Decimal chosen = downFits ? down : up;
            if (downFits && upFits) {
                int distance = value.subtract(down).compareTo(up.subtract(value));
                if (distance > 0 || (distance == 0 && (coefficient.charAt(coefficient.length() - 1) - '0') % 2 != 0)) chosen = up;
            }
            return (negative ? "-" : "") + layout(chosen, narrow);
        }
        throw new IllegalStateException("no shortest float representation");
    }

    private static Decimal midpoint(Decimal a, Decimal b) { return a.add(b).multiply(Decimal.parse("0.5")); }

    private static String layout(Decimal value, boolean narrow) {
        String digits = value.coefficientDigits();
        int exponent = digits.length() - value.scale() - 1;
        int end = digits.length();
        while (end > 1 && digits.charAt(end - 1) == '0') end--;
        digits = digits.substring(0, end);
        if (exponent >= -4 && exponent < (narrow ? 6 : 15)) {
            int point = exponent + 1;
            if (point <= 0) return "0." + "0".repeat(-point) + digits;
            if (point >= digits.length()) return digits + "0".repeat(point - digits.length());
            return digits.substring(0, point) + "." + digits.substring(point);
        }
        String power = Integer.toString(Math.abs(exponent));
        return digits.substring(0, 1) + (digits.length() > 1 ? "." + digits.substring(1) : "")
            + "e" + (exponent < 0 ? "-" : "+") + (power.length() < 2 ? "0" : "") + power;
    }
}
