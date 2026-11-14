package org.jed;

import java.util.Arrays;
import java.util.Objects;

/** Finite, exact base-ten arithmetic; coefficient limbs are little-endian base 10^9. */
public final class Decimal implements Comparable<Decimal> {
    void writeState(java.io.DataOutput output) throws java.io.IOException {
        output.writeBoolean(negative); output.writeInt(scale); output.writeInt(limbs.length);
        for (int limb : limbs) output.writeInt(limb);
    }
    static Decimal readState(java.io.DataInput input) throws java.io.IOException {
        boolean negative = input.readBoolean(); int scale = input.readInt(), count = input.readInt();
        if (scale < 0 || count < 0 || count > 1_000_000) throw new java.io.IOException("invalid decimal scratch state");
        int[] limbs = new int[count];
        for (int i = 0; i < count; i++) {
            limbs[i] = input.readInt();
            if (limbs[i] < 0 || limbs[i] >= 1_000_000_000) throw new java.io.IOException("invalid decimal scratch limb");
        }
        if (count > 0 && limbs[count - 1] == 0) throw new java.io.IOException("noncanonical decimal scratch coefficient");
        return new Decimal(negative, scale, limbs);
    }
    public static final int MAX_PRECISION = 1000;
    public static final int MAX_SCALE = 16383;
    public static final int MAX_INTEGER_DIGITS = 131072;
    private static final long BASE = 1_000_000_000L;
    private static final int[] POW10 = {1, 10, 100, 1000, 10000, 100000, 1000000, 10000000, 100000000};
    private final boolean negative;
    private final int scale;
    private final int[] limbs;

    private Decimal(boolean negative, int scale, int[] limbs) {
        this.limbs = trim(limbs);
        this.negative = negative && this.limbs.length != 0;
        this.scale = scale;
    }

    public static Decimal of(long value) { return parse(Long.toString(value)); }
    public static Decimal fromDoubleExact(double value) {
        if (!Double.isFinite(value)) throw SqlException.numericOverflow();
        long bits = Double.doubleToRawLongBits(value);
        int exponentBits = (int) ((bits >>> 52) & 2047);
        long significand = bits & ((1L << 52) - 1);
        int exponent = -1074;
        if (exponentBits != 0) {
            significand |= 1L << 52;
            exponent = exponentBits - 1023 - 52;
        }
        if (significand == 0) return zero(0);
        while (exponent < 0 && (significand & 1) == 0) { significand >>>= 1; exponent++; }
        int[] coefficient = of(significand).limbs;
        for (int i = 0; i < Math.abs(exponent); i++) coefficient = multiplySmall(coefficient, exponent < 0 ? 5 : 2);
        return new Decimal(bits < 0, Math.max(0, -exponent), coefficient).checked();
    }
    public static Decimal zero(int scale) {
        return new Decimal(false, scale, new int[0]).checked();
    }

    public static Decimal parse(String input) {
        Objects.requireNonNull(input, "input");
        int begin = 0, end = input.length();
        while (begin < end && asciiSpace(input.charAt(begin))) begin++;
        while (end > begin && asciiSpace(input.charAt(end - 1))) end--;
        boolean negative = false;
        if (begin < end && (input.charAt(begin) == '+' || input.charAt(begin) == '-')) {
            negative = input.charAt(begin++) == '-';
        }
        if (begin == end) throw invalidInput();
        if (begin + 2 <= end && input.charAt(begin) == '0') {
            int radix = switch (input.charAt(begin + 1)) {
                case 'x', 'X' -> 16;
                case 'o', 'O' -> 8;
                case 'b', 'B' -> 2;
                default -> 0;
            };
            if (radix != 0) return parseRadix(input, begin + 2, end, negative, radix);
        }
        StringBuilder digits = new StringBuilder();
        int fractional = 0;
        boolean point = false, previousDigit = false;
        int i = begin;
        for (; i < end; i++) {
            char c = input.charAt(i);
            if (c >= '0' && c <= '9') {
                digits.append(c);
                if (point) fractional++;
                previousDigit = true;
            } else if (c == '_' && previousDigit && i + 1 < end && isDigit(input.charAt(i + 1))) {
                previousDigit = false;
            } else if (c == '.' && !point) {
                point = true;
                previousDigit = false;
            } else break;
        }
        if (digits.isEmpty()) throw invalidInput();
        int exponent = 0;
        if (i < end && (input.charAt(i) == 'e' || input.charAt(i) == 'E')) {
            i++;
            boolean exponentNegative = false;
            if (i < end && (input.charAt(i) == '+' || input.charAt(i) == '-')) {
                exponentNegative = input.charAt(i++) == '-';
            }
            if (i == end || !isDigit(input.charAt(i))) throw invalidInput();
            previousDigit = false;
            for (; i < end; i++) {
                char c = input.charAt(i);
                if (isDigit(c)) {
                    exponent = Math.min(MAX_INTEGER_DIGITS + MAX_SCALE + 2, exponent * 10 + c - '0');
                    previousDigit = true;
                } else if (c == '_' && previousDigit && i + 1 < end && isDigit(input.charAt(i + 1))) {
                    previousDigit = false;
                } else throw invalidInput();
            }
            if (exponentNegative) exponent = -exponent;
        }
        if (i != end) throw invalidInput();
        int resultScale = fractional - exponent;
        if (resultScale > MAX_SCALE) throw SqlException.numericOverflow();
        int first = 0;
        while (first < digits.length() && digits.charAt(first) == '0') first++;
        if (first == digits.length()) return zero(Math.max(0, resultScale));
        int integerDigits = digits.length() - first - resultScale;
        if (integerDigits > MAX_INTEGER_DIGITS) throw SqlException.numericOverflow();
        int[] coefficient = fromDigits(digits.substring(first));
        if (resultScale < 0) coefficient = shift(coefficient, -resultScale);
        return new Decimal(negative, Math.max(0, resultScale), coefficient).checked();
    }

    private static Decimal parseRadix(String input, int begin, int end, boolean negative, int radix) {
        if (begin < end && input.charAt(begin) == '_') begin++;
        StringBuilder digits = new StringBuilder();
        boolean previousDigit = false;
        boolean significant = false;
        for (int i = begin; i < end; i++) {
            char c = input.charAt(i);
            int digit = asciiDigit(c);
            if (digit >= 0 && digit < radix) {
                significant |= digit != 0;
                if (significant) digits.append(c);
                previousDigit = true;
            } else if (c == '_' && previousDigit && i + 1 < end
                    && asciiDigit(input.charAt(i + 1)) >= 0 && asciiDigit(input.charAt(i + 1)) < radix) {
                previousDigit = false;
            } else throw invalidInput();
        }
        if (!previousDigit) throw invalidInput();
        int bits = radix == 16 ? 4 : radix == 8 ? 3 : 1;
        if ((long) (digits.length() - 1) * bits >= 435412) throw SqlException.numericOverflow();
        int[] coefficient = new int[0];
        int group = 28 / bits;
        for (int start = 0; start < digits.length(); start += group) {
            int stop = Math.min(digits.length(), start + group);
            int value = 0, multiplier = 1;
            for (int i = start; i < stop; i++) {
                value = value * radix + asciiDigit(digits.charAt(i));
                multiplier *= radix;
            }
            coefficient = add(multiplySmall(coefficient, multiplier), value == 0 ? new int[0] : new int[]{value});
        }
        return new Decimal(negative, 0, coefficient).checked();
    }

    private static int asciiDigit(char c) {
        if (isDigit(c)) return c - '0';
        if (c >= 'a' && c <= 'f') return c - 'a' + 10;
        if (c >= 'A' && c <= 'F') return c - 'A' + 10;
        return -1;
    }
    private static boolean isDigit(char c) { return c >= '0' && c <= '9'; }
    private static boolean asciiSpace(char c) { return c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '\f' || c == 11; }
    private static SqlException invalidInput() { return new SqlException("22P02", "invalid input syntax for type decimal"); }

    public boolean negative() { return negative; }
    public int scale() { return scale; }
    public boolean isZero() { return limbs.length == 0; }
    public int precision() { return digitCount(limbs); }
    public int integerDigits() { return Math.max(0, precision() - scale); }

    public Decimal checked() {
        if (scale < 0 || scale > MAX_SCALE || integerDigits() > MAX_INTEGER_DIGITS) {
            throw SqlException.numericOverflow();
        }
        return this;
    }

    public Decimal negate() { return new Decimal(!negative, scale, limbs); }
    public Decimal abs() { return negative ? negate() : this; }

    public Decimal add(Decimal other) { return addUncapped(other).checked(); }
    public Decimal subtract(Decimal other) { return add(other.negate()); }

    /** SUM/AVG check the final value, not a possibly over-cap intermediate. */
    public Decimal addUncapped(Decimal other) {
        int s = Math.max(scale, other.scale);
        int[] a = shift(limbs, s - scale), b = shift(other.limbs, s - other.scale);
        if (negative == other.negative) return new Decimal(negative, s, add(a, b));
        int order = compare(a, b);
        return order >= 0 ? new Decimal(negative, s, subtract(a, b))
                          : new Decimal(other.negative, s, subtract(b, a));
    }

    public Decimal multiply(Decimal other) {
        Decimal product = new Decimal(negative != other.negative, scale + other.scale, multiply(limbs, other.limbs));
        return product.scale > MAX_SCALE ? product.rescale(MAX_SCALE) : product.checked();
    }

    /** Exact moment accumulators may exceed the SQL value cap until finalization. */
    Decimal multiplyUncapped(Decimal other) {
        return new Decimal(negative != other.negative, Math.addExact(scale, other.scale), multiply(limbs, other.limbs));
    }

    public Decimal divide(Decimal other) { return divide(other, divisionScale(other)); }

    public Decimal divide(Decimal other, int resultScale) {
        if (resultScale < 0 || resultScale > MAX_SCALE) throw SqlException.numericOverflow();
        return divideInternal(other, resultScale).checked();
    }

    Decimal divideInternal(Decimal other, int resultScale) {
        if (other.isZero()) throw new SqlException("22012", "division by zero");
        if (resultScale < 0) throw new IllegalArgumentException("negative division scale");
        int exponent = resultScale + other.scale - scale;
        int[] numerator = exponent >= 0 ? shift(limbs, exponent) : limbs;
        int[] denominator = exponent < 0 ? shift(other.limbs, -exponent) : other.limbs;
        Division result = divideMagnitude(numerator, denominator);
        int[] quotient = result.quotient;
        if (compare(multiplySmall(result.remainder, 2), denominator) >= 0) quotient = add(quotient, new int[]{1});
        return new Decimal(negative != other.negative, resultScale, quotient);
    }

    public Decimal modulo(Decimal other) {
        if (other.isZero()) throw new SqlException("22012", "division by zero");
        int s = Math.max(scale, other.scale);
        return new Decimal(negative, s, divideMagnitude(shift(limbs, s - scale),
            shift(other.limbs, s - other.scale)).remainder).checked();
    }

    public int divisionScale(Decimal other) {
        int[] a = leadingGroup(), b = other.leadingGroup();
        int weight = a[0] - b[0] - (a[1] <= b[1] ? 1 : 0);
        return Math.min(MAX_PRECISION, Math.max(Math.max(16 - 4 * weight, 0), Math.max(scale, other.scale)));
    }

    private int[] leadingGroup() {
        if (isZero()) return new int[]{0, 0};
        int weight = Math.floorDiv(precision() - 1 - scale, 4);
        int exponent = 4 * weight + scale;
        String digits = coefficientDigits();
        int lead = 0;
        int count = digits.length() - exponent;
        for (int i = 0; i < count; i++) lead = lead * 10 + (i < digits.length() ? digits.charAt(i) - '0' : 0);
        return new int[]{weight, lead};
    }

    public Decimal rescale(int targetScale) { return rescale(targetScale, true); }
    public Decimal truncate(int targetScale) { return rescale(targetScale, false); }

    public Decimal roundPlaces(long places) { return places(places, true); }
    public Decimal truncatePlaces(long places) { return places(places, false); }
    private Decimal places(long places, boolean round) {
        if (places >= 0) return rescale((int) Math.min(places, MAX_SCALE), round);
        if (places < -MAX_INTEGER_DIGITS - 1L) return zero(0);
        int left = (int) -places;
        // Move the radix point rightward in the scale descriptor, round, then restore it.
        Decimal shifted = new Decimal(negative, scale + left, limbs);
        Decimal integral = shifted.rescale(0, round);
        return new Decimal(integral.negative, 0, shift(integral.limbs, left)).checked();
    }

    public Decimal floor() {
        Decimal integral = truncate(0);
        return negative && compareTo(integral) != 0 ? integral.subtract(of(1)) : integral;
    }
    public Decimal ceil() {
        Decimal integral = truncate(0);
        return !negative && compareTo(integral) != 0 ? integral.add(of(1)) : integral;
    }
    public int minimumScale() {
        if (isZero()) return 0;
        String digits = coefficientDigits();
        int end = digits.length(), result = scale;
        while (result > 0 && digits.charAt(end - 1) == '0') { result--; end--; }
        return result;
    }

    int base10000Weight() {
        return isZero() ? 0 : Math.floorDiv(precision() - 1 - scale, 4);
    }

    Decimal squareRootAtScale(int resultScale) {
        if (negative) throw new SqlException("2201F", "cannot take square root of a negative number");
        if (isZero()) return new Decimal(false, Math.max(resultScale, 0), new int[0]);
        int guardScale = Math.max(resultScale, 0) + 1;
        if (2 * guardScale < scale) guardScale = (scale + 1) / 2 + 1;
        int[] magnitude = shift(limbs, 2 * guardScale - scale);
        int[] root = shift(new int[]{1}, (digitCount(magnitude) + 1) / 2);
        while (true) {
            int[] quotient = divideMagnitude(magnitude, root).quotient;
            int[] next = divideSmall(add(root, quotient), 2).quotient;
            if (compare(next, root) >= 0) break;
            root = next;
        }
        return new Decimal(false, guardScale, root).roundInternal(resultScale);
    }

    private Decimal rescale(int targetScale, boolean round) {
        if (targetScale < 0 || targetScale > MAX_SCALE) throw SqlException.numericOverflow();
        return scaleInternal(targetScale, round).checked();
    }

    Decimal roundInternal(int targetScale) {
        if (targetScale >= 0) return scaleInternal(targetScale, true);
        int left = -targetScale;
        Decimal shifted = new Decimal(negative, scale + left, limbs).scaleInternal(0, true);
        return new Decimal(shifted.negative, 0, shift(shifted.limbs, left));
    }

    Decimal multiplyInternal(Decimal other, int targetScale) {
        return new Decimal(negative != other.negative, scale + other.scale, multiply(limbs, other.limbs)).roundInternal(targetScale);
    }

    private Decimal scaleInternal(int targetScale, boolean round) {
        if (targetScale >= scale) return new Decimal(negative, targetScale, shift(limbs, targetScale - scale));
        int discarded = scale - targetScale;
        int groups = discarded / 9, digits = discarded % 9;
        int[] quotient = groups >= limbs.length ? new int[0] : Arrays.copyOfRange(limbs, groups, limbs.length);
        if (digits != 0) quotient = divideSmall(quotient, POW10[digits]).quotient;
        int roundPosition = discarded - 1;
        int limb = roundPosition / 9;
        int next = limb < limbs.length ? limbs[limb] / POW10[roundPosition % 9] % 10 : 0;
        if (round && next >= 5) quotient = add(quotient, new int[]{1});
        return new Decimal(negative, targetScale, quotient);
    }

    public Decimal constrain(int precision, int scale) {
        SqlType.decimal(precision, scale);
        Decimal result = rescale(scale);
        if (result.integerDigits() > precision - scale) throw new SqlException("22003", "numeric field overflow");
        return result;
    }

    public long toLongRounded() { return rescale(0).toLongExact(); }
    public long toLongExact() {
        Decimal integer = truncate(0);
        if (compareTo(integer) != 0) throw SqlException.numericOverflow();
        // Negative accumulation permits Long.MIN_VALUE without unsigned arithmetic.
        long result = 0;
        long limit = negative ? Long.MIN_VALUE : -Long.MAX_VALUE;
        for (int i = integer.limbs.length - 1; i >= 0; i--) {
            if (result < limit / BASE) throw SqlException.numericOverflow();
            result *= BASE;
            if (result < limit + integer.limbs[i]) throw SqlException.numericOverflow();
            result -= integer.limbs[i];
        }
        return negative ? result : -result;
    }

    @Override public int compareTo(Decimal other) {
        if (negative != other.negative) return negative ? -1 : 1;
        int s = Math.max(scale, other.scale);
        int order = compare(shift(limbs, s - scale), shift(other.limbs, s - other.scale));
        return negative ? -order : order;
    }

    public String coefficientDigits() {
        if (isZero()) return "0";
        StringBuilder text = new StringBuilder(precision());
        text.append(limbs[limbs.length - 1]);
        for (int i = limbs.length - 2; i >= 0; i--) {
            String part = Integer.toString(limbs[i]);
            text.append("0".repeat(9 - part.length())).append(part);
        }
        return text.toString();
    }

    public String canonicalString() {
        if (isZero()) return "+0e0";
        String digits = coefficientDigits();
        int end = digits.length(), s = scale;
        while (s > 0 && digits.charAt(end - 1) == '0') { end--; s--; }
        return (negative ? "-" : "+") + digits.substring(0, end) + "e" + s;
    }

    @Override public String toString() {
        String digits = coefficientDigits();
        if (scale > 0) {
            if (digits.length() <= scale) digits = "0".repeat(scale + 1 - digits.length()) + digits;
            int point = digits.length() - scale;
            digits = digits.substring(0, point) + "." + digits.substring(point);
        }
        return negative ? "-" + digits : digits;
    }

    @Override public boolean equals(Object other) { return other instanceof Decimal d && compareTo(d) == 0; }
    @Override public int hashCode() { return canonicalString().hashCode(); }

    /** Logical base-10^4 groups for the shared decimal_work cost; independent of limb layout. */
    public long groupsAtScale(int alignedScale) {
        return Math.max(1L, (precision() + (long) alignedScale - scale + 3) / 4);
    }

    private static int digitCount(int[] a) {
        if (a.length == 0) return 0;
        int top = a[a.length - 1], digits = 1;
        while (top >= 10) { top /= 10; digits++; }
        return (a.length - 1) * 9 + digits;
    }

    private static int[] fromDigits(String text) {
        int[] result = new int[(text.length() + 8) / 9];
        for (int i = 0, end = text.length(); end > 0; i++, end -= 9) {
            result[i] = Integer.parseInt(text.substring(Math.max(0, end - 9), end));
        }
        return trim(result);
    }

    private static int[] trim(int[] a) {
        int size = a.length;
        while (size > 0 && a[size - 1] == 0) size--;
        return size == a.length ? a : Arrays.copyOf(a, size);
    }

    private static int compare(int[] a, int[] b) {
        if (a.length != b.length) return Integer.compare(a.length, b.length);
        for (int i = a.length - 1; i >= 0; i--) if (a[i] != b[i]) return Integer.compare(a[i], b[i]);
        return 0;
    }

    private static int[] shift(int[] a, int places) {
        if (a.length == 0 || places == 0) return a;
        int groups = places / 9, remainder = places % 9;
        int[] product = remainder == 0 ? a : multiplySmall(a, POW10[remainder]);
        int[] result = new int[product.length + groups];
        System.arraycopy(product, 0, result, groups, product.length);
        return result;
    }

    private static int[] add(int[] a, int[] b) {
        int size = Math.max(a.length, b.length);
        int[] result = new int[size + 1];
        long carry = 0;
        for (int i = 0; i < size; i++) {
            long value = carry + (i < a.length ? a[i] : 0) + (i < b.length ? b[i] : 0);
            result[i] = (int) (value % BASE);
            carry = value / BASE;
        }
        result[size] = (int) carry;
        return trim(result);
    }

    private static int[] subtract(int[] a, int[] b) {
        int[] result = new int[a.length];
        long borrow = 0;
        for (int i = 0; i < a.length; i++) {
            long value = a[i] - borrow - (i < b.length ? b[i] : 0);
            borrow = value < 0 ? 1 : 0;
            result[i] = (int) (value < 0 ? value + BASE : value);
        }
        return trim(result);
    }

    private static int[] multiplySmall(int[] a, int b) {
        if (a.length == 0 || b == 0) return new int[0];
        int[] result = new int[a.length + 1];
        long carry = 0;
        for (int i = 0; i < a.length; i++) {
            long value = (long) a[i] * b + carry;
            result[i] = (int) (value % BASE);
            carry = value / BASE;
        }
        result[a.length] = (int) carry;
        return trim(result);
    }

    private static int[] multiply(int[] a, int[] b) {
        if (a.length == 0 || b.length == 0) return new int[0];
        int[] result = new int[a.length + b.length];
        for (int i = 0; i < a.length; i++) {
            long carry = 0;
            for (int j = 0; j < b.length; j++) {
                long value = (long) a[i] * b[j] + result[i + j] + carry;
                result[i + j] = (int) (value % BASE);
                carry = value / BASE;
            }
            result[i + b.length] = (int) carry;
        }
        return trim(result);
    }

    private record Division(int[] quotient, int[] remainder) {}

    private static Division divideSmall(int[] a, int divisor) {
        int[] quotient = new int[a.length];
        long remainder = 0;
        for (int i = a.length - 1; i >= 0; i--) {
            long value = remainder * BASE + a[i];
            quotient[i] = (int) (value / divisor);
            remainder = value % divisor;
        }
        return new Division(trim(quotient), remainder == 0 ? new int[0] : new int[]{(int) remainder});
    }

    private static Division divideMagnitude(int[] a, int[] b) {
        if (b.length == 1) return divideSmall(a, b[0]);
        if (compare(a, b) < 0) return new Division(new int[0], a);
        int[] quotient = new int[a.length];
        int[] remainder = new int[0];
        for (int i = a.length - 1; i >= 0; i--) {
            int[] next = new int[remainder.length + 1];
            System.arraycopy(remainder, 0, next, 1, remainder.length);
            next[0] = a[i];
            remainder = trim(next);
            int low = 0, high = (int) BASE - 1;
            while (low < high) {
                int candidate = low + (high - low + 1) / 2;
                if (compare(multiplySmall(b, candidate), remainder) <= 0) low = candidate;
                else high = candidate - 1;
            }
            quotient[i] = low;
            if (low != 0) remainder = subtract(remainder, multiplySmall(b, low));
        }
        return new Division(trim(quotient), remainder);
    }
}
