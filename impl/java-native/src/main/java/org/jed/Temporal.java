package org.jed;

import java.util.Locale;

/** Proleptic Gregorian calendar and strict ISO input; no host clock or calendar library. */
final class Temporal {
    private Temporal() {}
    static final long DAY_MICROS = 86_400_000_000L;
    record Civil(long year, int month, int day) {}
    record Fields(long year, int month, int day, int hour, int minute, int second, int micros, int offset, boolean hasOffset) {}

    static long days(long year, int month, int day) {
        long adjusted = year - (month <= 2 ? 1 : 0);
        long era = Math.floorDiv(adjusted, 400);
        long yoe = adjusted - era * 400;
        long doy = (153L * (month + (month > 2 ? -3 : 9)) + 2) / 5 + day - 1;
        return era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468;
    }
    static Civil civil(long days) {
        long z = days + 719468;
        long era = Math.floorDiv(z, 146097);
        long doe = z - era * 146097;
        long yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        long year = yoe + era * 400;
        long doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        long mp = (5 * doy + 2) / 153;
        int day = (int) (doy - (153 * mp + 2) / 5 + 1);
        int month = (int) (mp + (mp < 10 ? 3 : -9));
        return new Civil(year + (month <= 2 ? 1 : 0), month, day);
    }
    static int monthDays(long year, int month) {
        return switch (month) {
            case 2 -> year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) ? 29 : 28;
            case 4, 6, 9, 11 -> 30;
            default -> 31;
        };
    }
    static void validateDate(long year, int month, int day) {
        if (month < 1 || month > 12 || day < 1 || day > monthDays(year, month)) throw overflow();
    }
    static int parseDate(String input) {
        String text = input.trim();
        if (text.equalsIgnoreCase("epoch")) return 0;
        int infinity = infinity(text);
        if (infinity != 0) return infinity < 0 ? Integer.MIN_VALUE : Integer.MAX_VALUE;
        Fields fields = parse(text, 7);
        long result = days(fields.year, fields.month, fields.day);
        if (result <= Integer.MIN_VALUE || result >= Integer.MAX_VALUE) throw overflow();
        return (int) result;
    }
    static boolean clockDate(String input) {
        return java.util.Set.of("today", "now", "tomorrow", "yesterday").contains(input.trim().toLowerCase(Locale.ROOT));
    }
    static int clockDate(String input, CostMeter meter) {
        meter.charge(Costs.TIMEZONE); meter.guard();
        long instant = meter.zone().toLocal(meter.statementTime());
        if (TemporalOps.infinite(instant)) throw overflow();
        String word = input.trim().toLowerCase(Locale.ROOT);
        long day = Math.floorDiv(instant, DAY_MICROS) + (word.equals("tomorrow") ? 1 : word.equals("yesterday") ? -1 : 0);
        if (day <= Integer.MIN_VALUE || day >= Integer.MAX_VALUE) throw overflow();
        return (int) day;
    }
    static ScalarPlan.Expression dateLiteral(String text) {
        return new ScalarPlan.Expression(SqlType.DATE, meter -> { meter.operation(); return SqlValue.ofDate(clockDate(text, meter)); });
    }
    static long parseTimestamp(String input, boolean zoned) {
        return parseTimestamp(input, zoned, null, null);
    }
    static long parseTimestamp(String input, boolean zoned, TimeZoneData.Zone zone, CostMeter meter) {
        String text = input.trim();
        int infinity = infinity(text);
        if (infinity != 0) return infinity < 0 ? Long.MIN_VALUE : Long.MAX_VALUE;
        Fields fields = parse(text, 6);
        long time = ((fields.hour * 60L + fields.minute) * 60 + fields.second - (zoned ? fields.offset : 0)) * 1_000_000 + fields.micros;
        long result = assemble(days(fields.year, fields.month, fields.day), time);
        if (zoned && !fields.hasOffset && zone != null) {
            if (meter != null) { meter.charge(Costs.TIMEZONE); meter.guard(); }
            result = zone.toInstant(result);
        }
        return result;
    }
    static long assemble(long day, long micros) {
        try {
            day = Math.addExact(day, Math.floorDiv(micros, DAY_MICROS));
            micros = Math.floorMod(micros, DAY_MICROS);
            long result = day < 0 ? Math.addExact(Math.multiplyExact(day + 1, DAY_MICROS), micros - DAY_MICROS)
                : Math.addExact(Math.multiplyExact(day, DAY_MICROS), micros);
            if (result == Long.MIN_VALUE || result == Long.MAX_VALUE) throw overflow();
            return result;
        } catch (ArithmeticException failure) { throw overflow(); }
    }
    static long roundPrecision(long micros, int precision) {
        if (micros == Long.MIN_VALUE || micros == Long.MAX_VALUE || precision >= 6) return micros;
        long factor = 1;
        for (int i = precision; i < 6; i++) factor *= 10;
        long remainder = Math.floorMod(micros, factor);
        boolean up = remainder * 2 > factor || remainder * 2 == factor && micros >= 946684800000000L;
        try {
            long result = Math.addExact(micros, (up ? factor : 0) - remainder);
            if (result == Long.MIN_VALUE || result == Long.MAX_VALUE) throw overflow();
            return result;
        } catch (ArithmeticException failure) { throw overflow(); }
    }
    static int makeDate(long year, long month, long day) {
        if (year == 0 || year < -9_999_999 || year > 9_999_999 || month < 1 || month > 12 || day < 1 || day > 31) throw overflow();
        long astronomical = year < 0 ? year + 1 : year;
        validateDate(astronomical, (int) month, (int) day);
        long result = days(astronomical, (int) month, (int) day);
        if (result <= Integer.MIN_VALUE || result >= Integer.MAX_VALUE) throw overflow();
        return (int) result;
    }

    private static int infinity(String text) {
        return text.equalsIgnoreCase("infinity") || text.equalsIgnoreCase("+infinity") ? 1 : text.equalsIgnoreCase("-infinity") ? -1 : 0;
    }
    static boolean explicitTimestamp(String text) {
        String trimmed = text.strip();
        return trimmed.equalsIgnoreCase("infinity") || trimmed.equalsIgnoreCase("-infinity") || parse(trimmed, 6).hasOffset();
    }
    private static Fields parse(String text, int yearDigits) {
        String upper = text.toUpperCase(Locale.ROOT);
        boolean bc = false;
        if (upper.endsWith(" BC") || upper.endsWith(" AD")) {
            bc = upper.endsWith(" BC"); text = text.substring(0, text.length() - 3);
        }
        if (upper.startsWith("BC ") || upper.startsWith("AD ")) {
            bc = upper.startsWith("BC "); text = text.substring(3);
        }
        Scanner in = new Scanner(text);
        int year = in.number(yearDigits);
        in.expect('-'); int month = in.number(2);
        in.expect('-'); int day = in.number(2);
        if (year == 0) throw overflow();
        long astronomical = bc ? 1L - year : year;
        validateDate(astronomical, month, day);
        int hour = 0, minute = 0, second = 0, micros = 0, offset = 0;
        boolean fractionNonzero = false;
        if (in.take(' ') || in.take('T') || in.take('t')) {
            hour = in.number(2); in.expect(':'); minute = in.number(2);
            if (in.take(':')) {
                second = in.number(2);
                if (in.take('.')) {
                    int digits = 0;
                    while (in.digit()) {
                        int next = in.text.charAt(in.position++) - '0';
                        if (next != 0) fractionNonzero = true;
                        if (digits < 6) micros = micros * 10 + next;
                        else if (digits == 6 && next >= 5) micros++;
                        digits++;
                    }
                    while (digits++ < 6) micros *= 10;
                }
            }
        }
        if (hour > 24 || minute > 59 || second > 59 || hour == 24 && (minute != 0 || second != 0 || fractionNonzero)) throw overflow();
        boolean hasOffset = in.peek('Z') || in.peek('z') || in.peek('+') || in.peek('-');
        if (in.take('Z') || in.take('z')) { /* UTC */ }
        else if (in.peek('+') || in.peek('-')) {
            boolean negative = in.take('-'); if (!negative) in.expect('+');
            int h = in.number(2), m = 0, s = 0;
            if (in.take(':')) { m = in.number(2); if (in.take(':')) s = in.number(2); }
            if (h > 15 || m > 59 || s > 59) throw overflow();
            offset = (h * 60 + m) * 60 + s;
            if (negative) offset = -offset;
        }
        if (!in.end()) throw invalid();
        return new Fields(astronomical, month, day, hour, minute, second, micros, offset, hasOffset);
    }
    static String renderValue(SqlValue value, TimeZoneData.Zone zone) {
        if (!value.isNull() && value.type().kind() == SqlType.Kind.RANGE) return value.asRange().render(zone);
        if (!value.isNull() && value.type().isComposite()) return CompositeValues.render(value, zone);
        if (!value.isNull() && value.type().kind() == SqlType.Kind.ARRAY) return value.asArray().render(zone);
        if (value.isNull() || value.type().kind() != SqlType.Kind.TIMESTAMPTZ || TemporalOps.infinite(value.asTimestamp())) return value.toString();
        if (zone == null) return value.toString();
        long instant = value.asTimestamp();
        String text = renderTimestamp(zone.toLocal(instant), false);
        String era = text.endsWith(" BC") ? " BC" : "";
        if (!era.isEmpty()) text = text.substring(0, text.length() - 3);
        int offset = zone.offsetAt(instant).seconds(), magnitude = Math.abs(offset);
        String suffix = (offset < 0 ? "-" : "+") + String.format(java.util.Locale.ROOT, "%02d", magnitude / 3600);
        if (magnitude % 3600 != 0) suffix += String.format(java.util.Locale.ROOT, ":%02d", magnitude / 60 % 60);
        if (magnitude % 60 != 0) suffix += String.format(java.util.Locale.ROOT, ":%02d", magnitude % 60);
        return text + suffix + era;
    }
    static String renderDate(int days) {
        if (days == Integer.MIN_VALUE) return "-infinity";
        if (days == Integer.MAX_VALUE) return "infinity";
        Civil date = civil(days);
        return dateText(date) + (date.year <= 0 ? " BC" : "");
    }
    static String renderTimestamp(long micros, boolean zoned) {
        if (micros == Long.MIN_VALUE) return "-infinity";
        if (micros == Long.MAX_VALUE) return "infinity";
        Civil date = civil(Math.floorDiv(micros, DAY_MICROS));
        long time = Math.floorMod(micros, DAY_MICROS);
        int fraction = (int) (time % 1_000_000);
        long seconds = time / 1_000_000;
        String result = dateText(date) + " " + pad(seconds / 3600, 2) + ":" + pad(seconds / 60 % 60, 2) + ":" + pad(seconds % 60, 2);
        if (fraction != 0) {
            String digits = pad(fraction, 6);
            int end = digits.length(); while (digits.charAt(end - 1) == '0') end--;
            result += "." + digits.substring(0, end);
        }
        return result + (zoned ? "+00" : "") + (date.year <= 0 ? " BC" : "");
    }
    private static String dateText(Civil date) { return pad(date.year <= 0 ? 1 - date.year : date.year, 4) + "-" + pad(date.month, 2) + "-" + pad(date.day, 2); }
    private static String pad(long number, int width) { String text = Long.toString(number); return "0".repeat(Math.max(0, width - text.length())) + text; }
    static SqlException overflow() { return new SqlException("22008", "date/time field out of range"); }
    private static SqlException invalid() { return new SqlException("22007", "invalid date/time syntax"); }
    private static final class Scanner {
        final String text;
        int position;
        Scanner(String text) { this.text = text; }
        boolean end() { return position == text.length(); }
        boolean peek(char c) { return !end() && text.charAt(position) == c; }
        boolean take(char c) { if (!peek(c)) return false; position++; return true; }
        void expect(char c) { if (!take(c)) throw invalid(); }
        boolean digit() { return !end() && text.charAt(position) >= '0' && text.charAt(position) <= '9'; }
        int number(int maxDigits) {
            int start = position, number = 0;
            while (digit()) { if (position - start >= maxDigits) throw invalid(); number = number * 10 + text.charAt(position++) - '0'; }
            if (position == start) throw invalid(); return number;
        }
    }
}
