package org.jed;

import java.util.ArrayList;
import java.util.List;
import java.util.Locale;

/** Exact interval field reader. Calendar fractions cascade independently for each input field. */
final class IntervalInput {
    private IntervalInput() {}
    private static final List<String> UNITS = List.of("millennium", "century", "decade", "year", "month", "week", "day", "hour", "minute", "second", "milliseconds", "microseconds");
    private static final int TIME_MASK = bit("hour") | bit("minute") | bit("second") | bit("milliseconds") | bit("microseconds");
    private static int bit(String unit) { return 1 << UNITS.indexOf(unit); }
    static SqlInterval parse(String input) {
        return parse(input, 0);
    }
    static SqlInterval parse(String input, int qualifier) {
        try {
            if (input.startsWith("P")) return iso(input);
            return fields(input, qualifier);
        } catch (SqlException failure) {
            if (failure.sqlState().equals("22003") || failure.sqlState().equals("22008")) throw Temporal.overflow();
            if (failure.sqlState().equals("22P02")) throw invalid();
            throw failure;
        } catch (ArithmeticException failure) { throw Temporal.overflow(); }
    }
    private static SqlInterval fields(String input, int qualifier) {
        List<String> fields = tokenize(input);
        if (fields.isEmpty()) throw invalid();
        boolean ago = fields.getLast().equals("ago");
        if (ago) fields.removeLast();
        if (fields.isEmpty()) throw invalid();
        if (ago && !letter(fields.getLast().charAt(0)) && fields.getLast().indexOf(':') < 0) throw invalid();
        Accumulator value = new Accumulator();
        String unit = qualifier == 0 ? "second" : DatetimeUnits.QUALIFIERS.get(qualifier - 1).least();
        boolean pending = false;
        int mask = 0;
        for (int i = fields.size() - 1; i >= 0; i--) {
            String field = fields.get(i);
            if (field.equals("at") || field.equals("on")) { unit = "second"; pending = false; continue; }
            if (letter(field.charAt(0))) {
                if (pending) throw invalid();
                unit = unit(field); pending = true; continue;
            }
            int claims;
            if (field.indexOf(':') >= 0) {
                value.micros = time(field, qualifier == 13);
                claims = TIME_MASK; unit = "day";
            } else {
                int dash = field.indexOf('-', field.charAt(0) == '-' || field.charAt(0) == '+' ? 1 : 0);
                if (dash >= 0) {
                    long years = Decimal.parse(field.substring(0, dash)).toLongExact();
                    long months = dash + 1 == field.length() ? 0 : Decimal.parse(field.substring(dash + 1)).toLongExact();
                    if (months < 0 || months > 11) throw Temporal.overflow();
                    value.months = Math.addExact(value.months, checkedInt(Math.addExact(Math.multiplyExact(years, 12), field.startsWith("-") ? -months : months)));
                    claims = bit("month"); unit = "month";
                } else {
                    Decimal number = number(field);
                    value.apply(unit, number);
                    claims = bit(unit);
                    if (unit.equals("second") && nonzeroFraction(field)) claims |= bit("milliseconds") | bit("microseconds");
                    if (unit.equals("hour")) unit = "day";
                }
            }
            if ((mask & claims) != 0) throw invalid();
            mask |= claims; pending = false;
        }
        if (pending) throw invalid();
        SqlInterval result = value.finish();
        return ago ? result.negate() : result;
    }
    private static List<String> tokenize(String input) {
        List<String> fields = new ArrayList<>(); int position = 0, bytes = 0;
        while (position < input.length()) {
            char c = input.charAt(position);
            if (space(c)) { position++; continue; }
            if (c > 127 || c < 32) throw invalid();
            StringBuilder field = new StringBuilder();
            if (c == '+' || c == '-') {
                field.append(c); position++;
                while (position < input.length() && space(input.charAt(position))) position++;
            }
            if (position == input.length()) throw invalid();
            c = input.charAt(position);
            if (digit(c) || c == '.') {
                while (position < input.length()) {
                    c = input.charAt(position);
                    if (!digit(c) && c != '.' && c != ':' && c != '-' && c != '/') break;
                    field.append(c); position++;
                }
            } else if (letter(c)) {
                while (position < input.length() && letter(input.charAt(position))) field.append(Character.toLowerCase(input.charAt(position++)));
                if (position < input.length()) {
                    c = input.charAt(position);
                    if (digit(c) || c == '-' || c == '+' || c == '.' || c == '/') throw invalid();
                }
            } else {
                if (!field.isEmpty()) throw invalid();
                position++; continue;
            }
            bytes += field.length() + 1;
            if (bytes > 256 || fields.size() == 25) throw invalid();
            fields.add(field.toString());
        }
        return fields;
    }
    private static boolean nonzeroFraction(String text) {
        int point = text.indexOf('.');
        if (point < 0) return false;
        for (int i = point + 1; i < text.length(); i++) if (text.charAt(i) != '0') return true;
        return false;
    }
    private static Decimal number(String text) {
        int point = text.indexOf('.');
        if (point >= 0 && text.length() > point + 26) text = text.substring(0, point + 26);
        return Decimal.parse(text);
    }
    private static String unit(String word) {
        if (word.equals("m")) throw invalid();
        String token = word.substring(0, Math.min(word.length(), DatetimeUnits.TOKEN_MAX_LEN));
        for (DatetimeUnits.Fact fact : DatetimeUnits.FACTS) {
            if (fact.token().equals(token) && fact.type().equals("units") && UNITS.contains(fact.value())) return fact.value();
        }
        throw invalid();
    }
    private static long time(String input, boolean minuteSecond) {
        boolean negative = input.startsWith("-");
        String text = input.startsWith("-") || input.startsWith("+") ? input.substring(1) : input;
        String[] parts = text.split(":", -1);
        if (parts.length < 2 || parts.length > 3) throw invalid();
        Decimal first = number(parts[0]), second = number(parts[1]);
        if (first.negative() || first.scale() != 0 || second.negative()) throw invalid();
        Decimal seconds;
        if (parts.length == 2 && (parts[1].indexOf('.') >= 0 || minuteSecond)) {
            if (minuteSecond && first.compareTo(Decimal.of(59)) > 0) throw Temporal.overflow();
            if (second.compareTo(Decimal.of(60)) > 0) throw Temporal.overflow();
            seconds = first.multiply(Decimal.of(60)).add(second);
        } else {
            if (second.scale() != 0 || second.compareTo(Decimal.of(59)) > 0) throw Temporal.overflow();
            Decimal last = parts.length == 3 ? number(parts[2]) : Decimal.of(0);
            if (last.negative() || last.compareTo(Decimal.of(60)) > 0) throw Temporal.overflow();
            seconds = first.multiply(Decimal.of(3600)).add(second.multiply(Decimal.of(60))).add(last);
        }
        long result = seconds.multiply(Decimal.of(1_000_000)).toLongRounded();
        return negative ? Math.negateExact(result) : result;
    }
    private static final class Accumulator {
        int years, months, days;
        long micros;
        void apply(String unit, Decimal value) {
            switch (unit) {
                case "millennium" -> apply("year", value.multiply(Decimal.of(1000)));
                case "century" -> apply("year", value.multiply(Decimal.of(100)));
                case "decade" -> apply("year", value.multiply(Decimal.of(10)));
                case "year" -> {
                    Decimal whole = value.truncate(0);
                    years = Math.addExact(years, checkedInt(whole.toLongExact()));
                    Decimal fraction = value.subtract(whole).multiply(Decimal.of(12));
                    long rounded = halfEven(fraction);
                    months = Math.addExact(months, checkedInt(rounded));
                }
                case "month" -> {
                    Decimal whole = value.truncate(0);
                    months = Math.addExact(months, checkedInt(whole.toLongExact()));
                    apply("day", value.subtract(whole).multiply(Decimal.of(30)));
                }
                case "week" -> apply("day", value.multiply(Decimal.of(7)));
                case "day" -> {
                    Decimal whole = value.truncate(0);
                    days = Math.addExact(days, checkedInt(whole.toLongExact()));
                    micros = Math.addExact(micros, value.subtract(whole).multiply(Decimal.of(Temporal.DAY_MICROS)).toLongRounded());
                }
                case "hour" -> micro(value, 3_600_000_000L);
                case "minute" -> micro(value, 60_000_000);
                case "second" -> micro(value, 1_000_000);
                case "milliseconds" -> micro(value, 1000);
                case "microseconds" -> micro(value, 1);
                default -> throw invalid();
            }
        }
        void micro(Decimal value, long factor) { micros = Math.addExact(micros, value.multiply(Decimal.of(factor)).toLongRounded()); }
        SqlInterval finish() { return new SqlInterval(checkedInt(years * 12L + months), days, micros); }
    }
    private static long halfEven(Decimal value) {
        long rounded = value.toLongRounded();
        Decimal fraction = value.abs().subtract(value.abs().truncate(0));
        if (fraction.compareTo(Decimal.parse("0.5")) == 0 && (rounded & 1) != 0) rounded += value.negative() ? 1 : -1;
        return rounded;
    }
    private static SqlInterval iso(String text) {
        Accumulator value = new Accumulator();
        if (text.equals("PT")) return value.finish();
        int position = 1; boolean inTime = false, any = false;
        while (position < text.length()) {
            if (text.charAt(position) == 'T') { if (inTime) throw invalid(); inTime = true; position++; continue; }
            int start = position;
            IsoNumber parsed = isoNumber(text, position);
            position = parsed.end;
            if (position == text.length() || text.charAt(position) == '-' || text.charAt(position) == ':' || text.charAt(position) == 'T') {
                return isoAlternative(text);
            }
            Decimal number = parsed.value;
            char designator = text.charAt(position++);
            String unit = switch (designator) {
                case 'Y' -> inTime ? "" : "year";
                case 'M' -> inTime ? "minute" : "month";
                case 'W' -> inTime ? "" : "week";
                case 'D' -> inTime ? "" : "day";
                case 'H' -> inTime ? "hour" : "";
                case 'S' -> inTime ? "second" : "";
                default -> "";
            };
            if (unit.isEmpty()) throw invalid();
            value.apply(unit, number); any = true;
        }
        if (!any) throw invalid();
        return value.finish();
    }
    private static SqlInterval isoAlternative(String input) {
        String[] halves = input.substring(1).split("T", -1);
        if (halves.length > 2) throw invalid();
        Accumulator value = new Accumulator();
        String date = halves[0];
        if (!date.isEmpty()) {
            String[] fields = date.split("-", -1);
            if (fields.length == 1 && date.length() == 8 && date.chars().allMatch(c -> c >= '0' && c <= '9')) fields = new String[]{date.substring(0, 4), date.substring(4, 6), date.substring(6)};
            if (fields.length > 3) throw invalid();
            String[] units = {"year", "month", "day"};
            for (int i = 0; i < fields.length; i++) value.apply(units[i], number(fields[i]));
        }
        if (halves.length == 2 && !halves[1].isEmpty()) {
            String time = halves[1];
            String[] fields = time.split(":", -1);
            int point = time.indexOf('.');
            String integral = point < 0 ? time : time.substring(0, point);
            if (fields.length == 1 && integral.length() == 6 && integral.chars().allMatch(c -> c >= '0' && c <= '9')) {
                fields = new String[]{integral.substring(0, 2), integral.substring(2, 4), integral.substring(4)};
                if (point >= 0) value.apply("microseconds", number("0" + time.substring(point)));
            }
            if (fields.length > 3) throw invalid();
            String[] units = {"hour", "minute", "second"};
            for (int i = 0; i < fields.length; i++) value.apply(units[i], number(fields[i]));
        }
        return value.finish();
    }
    private record IsoNumber(Decimal value, int end) {}
    private static IsoNumber isoNumber(String text, int start) {
        int position = start;
        boolean negative = text.charAt(position) == '-';
        if (negative) position++;
        if (position >= text.length()) throw invalid();
        if (text.startsWith("inf", position) || text.startsWith("nan", position)) throw Temporal.overflow();
        boolean hex = text.startsWith("0x", position) || text.startsWith("0X", position);
        if (hex) {
            position += 2;
            StringBuilder digits = new StringBuilder();
            int fractional = 0; boolean point = false;
            while (position < text.length()) {
                char c = text.charAt(position);
                if (c == '.' && !point) { point = true; position++; continue; }
                if (Character.digit(c, 16) < 0 || c > 127) break;
                digits.append(c); if (point) fractional++; position++;
            }
            if (digits.isEmpty()) throw invalid();
            int exponent = 0;
            if (position < text.length() && (text.charAt(position) == 'p' || text.charAt(position) == 'P')) {
                int exponentStart = ++position;
                if (position < text.length() && (text.charAt(position) == '-' || text.charAt(position) == '+')) position++;
                int firstDigit = position;
                while (position < text.length() && digit(text.charAt(position))) position++;
                if (firstDigit == position) throw invalid();
                try { exponent = Integer.parseInt(text.substring(exponentStart, position)); }
                catch (NumberFormatException failure) { throw invalid(); }
            }
            int first = 0; while (first < digits.length() && digits.charAt(first) == '0') first++;
            if (first == digits.length()) return new IsoNumber(Decimal.of(0), position);
            int bits = (digits.length() - first - 1) * 4 + 32 - Integer.numberOfLeadingZeros(Character.digit(digits.charAt(first), 16));
            long shift = (long) exponent - fractional * 4L;
            long weight = bits - 1L + shift;
            int last = digits.length() - 1; while (digits.charAt(last) == '0') last--;
            int trailing = (digits.length() - last - 1) * 4 + Integer.numberOfTrailingZeros(Character.digit(digits.charAt(last), 16));
            if (weight > 1023 || weight < -1074 || weight < -1022 && shift + trailing < -1074) throw invalid();
            if (weight > 50) throw Temporal.overflow();
            if (weight < -100) return new IsoNumber(Decimal.of(0), position);
            Decimal value = Decimal.parse("0x" + digits);
            Decimal factor = Decimal.of(1), power = Decimal.of(2);
            long remaining = Math.abs(shift);
            while (remaining != 0) {
                if ((remaining & 1) != 0) factor = factor.multiply(power);
                remaining >>>= 1;
                if (remaining != 0) power = power.multiply(power);
            }
            value = shift >= 0 ? value.multiply(factor) : value.divideInternal(factor, 25);
            return new IsoNumber(negative ? value.negate() : value, position);
        }
        int digitsStart = position;
        boolean point = false;
        while (position < text.length()) {
            char c = text.charAt(position);
            if (c == '.' && !point) { point = true; position++; continue; }
            if (!digit(c)) break;
            position++;
        }
        if (position == digitsStart || position == digitsStart + 1 && text.charAt(digitsStart) == '.') throw invalid();
        int significandEnd = position;
        if (position < text.length() && (text.charAt(position) == 'e' || text.charAt(position) == 'E')) {
            position++;
            if (position < text.length() && (text.charAt(position) == '-' || text.charAt(position) == '+')) position++;
            int exponentStart = position;
            while (position < text.length() && digit(text.charAt(position))) position++;
            if (position == exponentStart) throw invalid();
        }
        boolean zero = true;
        for (int i = digitsStart; i < significandEnd; i++) if (text.charAt(i) != '0' && text.charAt(i) != '.') zero = false;
        if (zero) return new IsoNumber(Decimal.of(0), position);
        Decimal value;
        try { value = Decimal.parse(text.substring(start, position)); }
        catch (SqlException failure) { throw invalid(); }
        Decimal magnitude = value.abs();
        if (magnitude.compareTo(Decimal.parse("1.797693134862315708145274237317043567981e308")) > 0 || magnitude.compareTo(Decimal.parse("2.225073858507201383090232717332404064219e-308")) < 0) throw invalid();
        if (magnitude.compareTo(Decimal.of(1_000_000_000_000_000L)) > 0) throw Temporal.overflow();
        return new IsoNumber(value, position);
    }
    private static int checkedInt(long value) { if (value < Integer.MIN_VALUE || value > Integer.MAX_VALUE) throw Temporal.overflow(); return (int) value; }
    private static boolean digit(char c) { return c >= '0' && c <= '9'; }
    private static boolean letter(char c) { return c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z'; }
    private static boolean space(char c) { return c == ' ' || c >= '\t' && c <= '\r'; }
    private static SqlException invalid() { return new SqlException("22007", "invalid input syntax for type interval"); }
}
