package org.jed;

/** Calendar months, calendar days and an independent microsecond duration. */
public record SqlInterval(int months, int days, long micros) implements Comparable<SqlInterval> {
    private static final long DAY = Temporal.DAY_MICROS;
    public long spanLow() { return (months * 30L + days) * DAY + micros; }
    public long spanHigh() {
        long count = months * 30L + days;
        long low = count * DAY;
        long high = Math.multiplyHigh(count, DAY) + (micros < 0 ? -1 : 0);
        return high + (Long.compareUnsigned(low + micros, low) < 0 ? 1 : 0);
    }
    @Override public int compareTo(SqlInterval other) {
        int high = Long.compare(spanHigh(), other.spanHigh());
        return high == 0 ? Long.compareUnsigned(spanLow(), other.spanLow()) : high;
    }
    @Override public boolean equals(Object other) { return other instanceof SqlInterval value && compareTo(value) == 0; }
    @Override public int hashCode() { return 31 * Long.hashCode(spanHigh()) + Long.hashCode(spanLow()); }
    public SqlInterval add(SqlInterval other) {
        try { return new SqlInterval(Math.addExact(months, other.months), Math.addExact(days, other.days), Math.addExact(micros, other.micros)); }
        catch (ArithmeticException failure) { throw Temporal.overflow(); }
    }
    public SqlInterval subtract(SqlInterval other) {
        try { return new SqlInterval(Math.subtractExact(months, other.months), Math.subtractExact(days, other.days), Math.subtractExact(micros, other.micros)); }
        catch (ArithmeticException failure) { throw Temporal.overflow(); }
    }
    public SqlInterval negate() {
        try { return new SqlInterval(Math.negateExact(months), Math.negateExact(days), Math.negateExact(micros)); }
        catch (ArithmeticException failure) { throw Temporal.overflow(); }
    }
    public SqlInterval justifyHours() {
        try {
            int day = Math.addExact(days, (int) (micros / DAY));
            long time = micros % DAY;
            if (day > 0 && time < 0) { day--; time += DAY; }
            else if (day < 0 && time > 0) { day++; time -= DAY; }
            return new SqlInterval(months, day, time);
        } catch (ArithmeticException failure) { throw Temporal.overflow(); }
    }
    public SqlInterval justifyDays() {
        try {
            int month = Math.addExact(months, days / 30), day = days % 30;
            if (month > 0 && day < 0) { month--; day += 30; }
            else if (month < 0 && day > 0) { month++; day -= 30; }
            return new SqlInterval(month, day, micros);
        } catch (ArithmeticException failure) { throw Temporal.overflow(); }
    }
    public SqlInterval justify() {
        try {
            int month = months, day = days;
            long time = micros;
            if (day > 0 && time > 0 || day < 0 && time < 0) {
                month = Math.addExact(month, day / 30); day %= 30;
            }
            day = Math.addExact(day, (int) (time / DAY)); time %= DAY;
            month = Math.addExact(month, day / 30); day %= 30;
            if (month > 0 && (day < 0 || day == 0 && time < 0)) { month--; day += 30; }
            else if (month < 0 && (day > 0 || day == 0 && time > 0)) { month++; day -= 30; }
            if (day > 0 && time < 0) { day--; time += DAY; }
            else if (day < 0 && time > 0) { day++; time -= DAY; }
            return new SqlInterval(month, day, time);
        } catch (ArithmeticException failure) { throw Temporal.overflow(); }
    }
    public static SqlInterval parse(String text) { return IntervalInput.parse(text); }
    public SqlInterval constrain(SqlType type) {
        int month = months, day = days; long time = micros;
        String least = type.intervalFields() == 0 ? "second" : DatetimeUnits.QUALIFIERS.get(type.intervalFields() - 1).least();
        switch (least) {
            case "year" -> { month = month / 12 * 12; day = 0; time = 0; }
            case "month" -> { day = 0; time = 0; }
            case "day" -> time = 0;
            case "hour" -> time = time / 3_600_000_000L * 3_600_000_000L;
            case "minute" -> time = time / 60_000_000 * 60_000_000;
            default -> { }
        }
        if (type.precision() >= 0 && type.precision() < 6) {
            long factor = 1;
            for (int i = type.precision(); i < 6; i++) factor *= 10;
            long remainder = time % factor;
            long delta = Math.abs(remainder) * 2 >= factor ? (time < 0 ? -factor : factor) - remainder : -remainder;
            try { time = Math.addExact(time, delta); }
            catch (ArithmeticException failure) { throw Temporal.overflow(); }
        }
        return new SqlInterval(month, day, time);
    }
    @Override public String toString() {
        StringBuilder text = new StringBuilder();
        boolean negative = part(text, months / 12, "year", false);
        negative = part(text, months % 12, "mon", negative);
        negative = part(text, days, "day", negative);
        if (micros != 0 || text.isEmpty()) {
            if (!text.isEmpty()) text.append(' ');
            if (micros < 0) text.append('-'); else if (negative) text.append('+');
            // Keep a negative magnitude so Long.MIN_VALUE never needs a signed absolute value.
            long magnitude = micros > 0 ? -micros : micros;
            long seconds = -(magnitude / 1_000_000);
            int fraction = (int) -(magnitude % 1_000_000);
            text.append(pad(seconds / 3600, 2)).append(':').append(pad(seconds / 60 % 60, 2)).append(':').append(pad(seconds % 60, 2));
            if (fraction != 0) {
                String digits = pad(fraction, 6); int end = 6;
                while (digits.charAt(end - 1) == '0') end--;
                text.append('.').append(digits, 0, end);
            }
        }
        return text.toString();
    }
    private static boolean part(StringBuilder text, int value, String unit, boolean negative) {
        if (value == 0) return negative;
        if (!text.isEmpty()) text.append(' ');
        if (value > 0 && negative) text.append('+');
        text.append(value).append(' ').append(unit);
        if (value != 1) text.append('s');
        return value < 0;
    }
    private static String pad(long value, int count) { String text = Long.toString(value); return "0".repeat(Math.max(0, count - text.length())) + text; }
}
