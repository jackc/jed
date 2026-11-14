package org.jed;

import java.util.HashMap;
import java.util.List;
import java.util.Locale;
import java.util.Map;

/** Reads formatting fields in source order, then assembles a checked calendar value. */
final class DateTimeInput {
    private final byte[] input;
    private int at, digits, zoneSign = 1;
    private final Map<String, Integer> fields = new HashMap<>();
    private boolean fixed, twelve, iso, gregorian, yearSet, era, zone;
    private int yearWidth = 4, precision = 6;
    private DateTimeInput(String input) { this.input = SqlText.encode(input); }
    static SqlValue read(String input, String template, SqlType type, CostMeter meter) {
        return new DateTimeInput(input).read(Formatting.parse(template, true), type, meter);
    }
    private SqlValue read(List<Formatting.Node> nodes, SqlType type, CostMeter meter) {
        for (int i = 0; i < nodes.size() && at < input.length; i++) {
            Formatting.Node node = nodes.get(i);
            if (node.keyword() == null) {
                if (node.suffix().equals("quoted")) { at = (int) Math.min(input.length, at + SqlText.encodedLength(node.text())); continue; }
                int c = node.text().codePointAt(0);
                if (fixed) { at++; continue; }
                if (space(c) || separator(c)) {
                    while (at < input.length && space(input[at] & 255)) at++;
                    if (at < input.length && (space(input[at] & 255) || separator(input[at] & 255))) {
                        if (input[at] == '-' && i + 1 < nodes.size() && List.of("TZH", "TZ", "tz", "OF").contains(nodes.get(i + 1).id())) zoneSign = -1;
                        at++;
                    }
                } else at = (int) Math.min(input.length, at + SqlText.encodedLength(node.text()));
                continue;
            }
            if (!fixed) while (at < input.length && space(input[at] & 255)) at++;
            if (at >= input.length) break;
            String id = node.id().toUpperCase(Locale.ROOT);
            if (id.equals("FX")) { fixed = true; continue; }
            if (node.keyword().mode().equals("isoweek")) iso = true;
            if (node.keyword().mode().equals("gregorian")) gregorian = true;
            if (iso && gregorian) throw invalid("invalid combination of date conventions");
            boolean followingDigit = i + 1 < nodes.size() && nodes.get(i + 1).keyword() != null && nodes.get(i + 1).keyword().digit();
            boolean full = node.fill() || !followingDigit || !node.suffix().isEmpty();
            int width = switch (id) { case "HH", "HH12", "HH24", "MI", "SS", "MM", "DD", "WW", "IW", "CC", "YY", "IY", "TZH", "TZM" -> 2; case "DDD", "YYY", "IYY", "MS", "IDDD" -> 3; case "YYYY", "IYYY" -> 4; case "US" -> 6; default -> id.startsWith("FF") ? id.charAt(2) - '0' : 1; };
            switch (id) {
                case "MONTH", "MON" -> set("month", word(id.equals("MONTH") ? FormattingCatalog.MONTHS_FULL : FormattingCatalog.MONTHS_ABBREV) + 1);
                case "DAY", "DY" -> { gregorian = true; if (iso) throw invalid("invalid combination of date conventions"); set("weekday", word(id.equals("DAY") ? FormattingCatalog.DAYS_FULL : FormattingCatalog.DAYS_ABBREV)); }
                case "RM" -> set("month", 12 - word(FormattingCatalog.RM_MONTHS_UPPER));
                case "AM", "PM", "A_M", "P_M" -> { set("pm", word(id.contains("_") ? FormattingCatalog.AMPM_LONG : FormattingCatalog.AMPM) % 2); twelve = true; }
                case "BC", "AD", "B_C", "A_D" -> { set("bc", word(id.contains("_") ? FormattingCatalog.ADBC_LONG : FormattingCatalog.ADBC) % 2); era = true; }
                case "YYYY", "YYY", "YY", "Y", "IYYY", "IYY", "IY", "I" -> { set("year", number(width, full, node.text())); yearWidth = width; yearSet = true; }
                case "Y_YYY" -> { int first = number(1, true, node.text()); if (at < input.length && input[at] == ',') at++; int rest = number(3, true, node.text()); set("year", checked(first * 1000L + rest)); yearSet = true; }
                case "CC" -> set("century", number(width, full, node.text()));
                case "MM" -> set("month", number(width, full, node.text()));
                case "DD" -> set("day", number(width, full, node.text()));
                case "DDD", "IDDD" -> set("doy", number(width, full, node.text()));
                case "WW", "IW" -> set("week", number(width, full, node.text()));
                case "W" -> set("monthweek", number(width, full, node.text()));
                case "D", "ID" -> set("dow", number(width, full, node.text()));
                case "Q" -> number(width, full, node.text());
                case "J" -> set("julian", number(width, full, node.text()));
                case "HH", "HH12", "HH24" -> { set("hour", number(width, full, node.text())); twelve |= !id.equals("HH24"); }
                case "MI" -> set("minute", number(width, full, node.text()));
                case "SS" -> set("second", number(width, full, node.text()));
                case "SSSS" -> set("seconds", number(width, full, node.text()));
                case "MS", "US", "FF1", "FF2", "FF3", "FF4", "FF5", "FF6" -> {
                    int part = number(width, full, node.text()); int length = digits;
                    if (id.startsWith("FF")) precision = width;
                    int target = id.equals("MS") ? 3 : 6;
                    for (int p = length; p < target; p++) part = checked(part * 10L);
                    if (target == 3) part = checked(part * 1000L);
                    set(id.equals("MS") ? "millis" : "micros", part);
                }
                case "TZH" -> { zone = true; if (at < input.length && (input[at] == '-' || input[at] == '+')) { zoneSign = input[at++] == '-' ? -1 : 1; } set("zonehour", number(2, full, node.text())); }
                case "TZM" -> { zone = true; set("zoneminute", number(2, full, node.text())); }
                case "TZ", "OF" -> {
                    zone = true;
                    if (id.equals("TZ") && at < input.length && letter(input[at])) throw new SqlException("0A000", "time zone abbreviations are not supported");
                    if (at < input.length && (input[at] == '-' || input[at] == '+')) zoneSign = input[at++] == '-' ? -1 : 1;
                    set("zonehour", number(2, true, node.text()));
                    if (at < input.length && input[at] == ':') { at++; set("zoneminute", number(2, true, node.text())); }
                }
                default -> throw new SqlException("0A000", "datetime input token is not implemented: " + id);
            }
            if (node.suffix().equals("TH") || node.suffix().equals("th")) at = Math.min(input.length, at + 2);
        }
        long year = get("year", 0); boolean negative = year < 0;
        if (yearWidth < 4 && yearSet && !fields.containsKey("century")) {
            if (year >= 0 && year < 70) year += 2000; else if (year < 100) year += 1900; else if (year < 520) year += 2000; else if (year < 1000) year += 1000;
        }
        if (fields.containsKey("century") && (!yearSet || yearWidth <= 2)) year = checked((get("century", 1) - 1L) * 100 + (yearSet ? year == 0 ? 100 : year : get("bc", 0) == 1 ? 100 : 1));
        if (negative) year = -year;
        if (negative ^ get("bc", 0) != 0) year = 1 - year;
        else if (year == 0 && yearSet) year = 0;
        int month = get("month", 1), day = get("day", 1);
        long days;
        if (get("julian", 0) != 0) days = get("julian", 0) - 2440588L;
        else if (iso && (fields.containsKey("week") || fields.containsKey("doy"))) {
            int dow = get("dow", 1); if (dow < 1 || dow > 7) throw Temporal.overflow();
            days = DateTimeFormat.isoStart(year) + (fields.containsKey("doy") ? get("doy", 1) - 1 : (get("week", 1) - 1) * 7L + dow - 1);
        } else {
            if (fields.containsKey("doy")) {
                if (!yearSet && !fields.containsKey("century")) throw invalid("cannot calculate day of year without year information");
                if (get("doy", 1) < 1 || get("doy", 1) > Temporal.days(year + 1, 1, 1) - Temporal.days(year, 1, 1)) throw Temporal.overflow();
                days = Temporal.days(year, 1, 1) + get("doy", 1) - 1;
            } else if (fields.containsKey("week")) days = Temporal.days(year, 1, 1) + checked((get("week", 1) - 1L) * 7);
            else {
                if (fields.containsKey("monthweek")) day = checked((get("monthweek", 1) - 1L) * 7 + 1);
                if (yearSet || fields.containsKey("century")) Temporal.validateDate(year, month, day);
                else if (month < 1 || month > 12 || day < 1 || day > 31) throw Temporal.overflow();
                days = Temporal.days(year, month, day);
            }
        }
        if (type == SqlType.DATE) { if (days <= Integer.MIN_VALUE || days >= Integer.MAX_VALUE) throw Temporal.overflow(); return SqlValue.ofDate((int) days); }
        int seconds = get("seconds", 0), hour = get("hour", seconds / 3600), minute = get("minute", seconds / 60 % 60), second = get("second", seconds % 60);
        if (twelve) { if (hour < 1 || hour > 12) throw invalid("hour must be between 1 and 12"); hour = hour % 12 + get("pm", 0) * 12; }
        int micros = get("micros", 0) + get("millis", 0);
        if (hour < 0 || hour > 23 || minute < 0 || minute > 59 || second < 0 || second > 59 || micros < 0 || micros > 999999) throw Temporal.overflow();
        int zoneHour = get("zonehour", 0), zoneMinute = get("zoneminute", 0);
        if (zoneHour < 0 || zoneHour > 15 || zoneMinute < 0 || zoneMinute > 59) throw new SqlException("22009", "time zone displacement out of range");
        long instant = Temporal.assemble(days, ((hour * 60L + minute) * 60 + second) * 1_000_000L + micros);
        if (zone) { try { instant = Math.subtractExact(instant, zoneSign * (zoneHour * 3600L + zoneMinute * 60) * 1_000_000); } catch (ArithmeticException error) { throw Temporal.overflow(); } }
        else { meter.charge(Costs.TIMEZONE); meter.guard(); instant = meter.zone().toInstant(instant); }
        return SqlValue.ofTimestamp(type, Temporal.roundPrecision(instant, precision));
    }
    private int get(String field, int fallback) { return fields.getOrDefault(field, fallback); }
    private static int checked(long value) { if (value < Integer.MIN_VALUE || value > Integer.MAX_VALUE) throw Temporal.overflow(); return (int) value; }
    private void set(String field, int value) { Integer old = fields.putIfAbsent(field, value); if (old != null && old != value) throw invalid("conflicting values for " + field); }
    private int number(int width, boolean full, String field) {
        while (at < input.length && space(input[at] & 255)) at++;
        int begin = at; boolean negative = at < input.length && input[at] == '-';
        if (at < input.length && (input[at] == '-' || input[at] == '+')) at++;
        int start = at; long number = 0; int limit = full ? input.length : Math.min(input.length, begin + width);
        while (at < limit && input[at] >= '0' && input[at] <= '9') { number = number * 10 + input[at++] - '0'; if (number > 2147483648L) throw Temporal.overflow(); }
        digits = at - start;
        if (digits == 0 || !full && at - begin < width) throw invalid("invalid value for " + field);
        if (negative) number = -number; if (number < Integer.MIN_VALUE || number > Integer.MAX_VALUE) throw Temporal.overflow(); return (int) number;
    }
    private int word(List<String> words) {
        for (int i = 0; i < words.size(); i++) {
            String word = words.get(i); boolean matches = at + word.length() <= input.length;
            for (int j = 0; matches && j < word.length(); j++) { int c = input[at + j] & 255; if (c >= 'A' && c <= 'Z') c += 32; matches = c == Character.toLowerCase(word.charAt(j)); }
            if (matches) { at += word.length(); return i; }
        }
        throw invalid("invalid value for date/time field");
    }
    private static boolean letter(int value) { return value >= 'A' && value <= 'Z' || value >= 'a' && value <= 'z'; }
    private static boolean space(int value) { return value == ' ' || value >= 9 && value <= 13; }
    private static boolean separator(int value) { return value >= 32 && value <= 126 && !letter(value) && !(value >= '0' && value <= '9'); }
    private static SqlException invalid(String message) { return new SqlException("22007", message); }
}
