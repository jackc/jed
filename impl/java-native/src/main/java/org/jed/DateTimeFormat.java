package org.jed;

import java.util.List;
import java.util.Locale;
import java.util.Set;

/** Calendar formatting over the engine's proleptic Gregorian primitives. */
final class DateTimeFormat {
    private DateTimeFormat() {}
    static String render(SqlValue value, String template, CostMeter meter) {
        if (template.isEmpty()) return null;
        boolean interval = value.type().kind() == SqlType.Kind.INTERVAL;
        long year, hour, minute, second, micro, dayCount; int month, day, offset = 0; String zone = "";
        if (interval) {
            SqlInterval span = value.asInterval(); year = span.months() / 12; month = span.months() % 12; day = span.days();
            hour = span.micros() / 3_600_000_000L; minute = span.micros() / 60_000_000 % 60; second = span.micros() / 1_000_000 % 60; micro = span.micros() % 1_000_000;
            dayCount = Temporal.days(year, month, day);
        } else {
            long instant = value.asTimestamp(); if (TemporalOps.infinite(instant)) return null;
            if (value.type().kind() == SqlType.Kind.TIMESTAMPTZ) {
                meter.charge(Costs.TIMEZONE); meter.guard(); TimeZoneData.Offset info = meter.zone().offsetAt(instant); offset = info.seconds(); zone = info.abbreviation();
                if (meter.zone().name().startsWith("+") || meter.zone().name().startsWith("-")) zone = "";
                instant = meter.zone().toLocal(instant);
            }
            dayCount = Math.floorDiv(instant, Temporal.DAY_MICROS); Temporal.Civil civil = Temporal.civil(dayCount); year = civil.year(); month = civil.month(); day = civil.day();
            long time = Math.floorMod(instant, Temporal.DAY_MICROS); hour = time / 3_600_000_000L; minute = time / 60_000_000 % 60; second = time / 1_000_000 % 60; micro = time % 1_000_000;
        }
        long doy = interval ? (year * 12 + month) * 30 + day : dayCount - Temporal.days(year, 1, 1) + 1;
        int dow = Math.floorMod(dayCount + 4, 7); long isoYear = Temporal.civil(dayCount + 4 - isoDow(dayCount)).year();
        long isoDay = dayCount - isoStart(isoYear) + 1, displayYear = interval || year > 0 ? year : 1 - year;
        StringBuilder output = new StringBuilder();
        for (Formatting.Node node : Formatting.parse(template, true)) {
            if (node.keyword() == null) { output.append(node.text()); continue; }
            String id = node.id(), upper = id.toUpperCase(Locale.ROOT), text = null; long number = 0; int width = 0;
            if (interval && Set.of("MONTH", "MON", "DAY", "DY", "D", "ID", "AD", "BC", "A_D", "B_C", "TZ", "OF", "TZH", "TZM").contains(upper)) throw new SqlException("22007", "invalid format specification for an interval value");
            switch (upper) {
                case "HH", "HH12" -> { number = hour % 12 == 0 ? 12 : hour % 12; width = 2; }
                case "HH24" -> { number = hour; width = 2; }
                case "MI" -> { number = minute; width = 2; }
                case "SS" -> { number = second; width = 2; }
                case "MS" -> { number = micro / 1000; width = 3; }
                case "US" -> { number = micro; width = 6; }
                case "FF1", "FF2", "FF3", "FF4", "FF5", "FF6" -> { width = id.charAt(2) - '0'; number = micro; for (int i = width; i < 6; i++) number /= 10; }
                case "SSSS" -> number = hour * 3600 + minute * 60 + second;
                case "AM", "PM", "A_M", "P_M" -> { text = hour >= 12 ? "PM" : "AM"; if (upper.contains("_")) text = text.charAt(0) + ".M."; text = casing(text, node.text()); }
                case "AD", "BC", "A_D", "B_C" -> { text = year <= 0 ? "BC" : "AD"; if (upper.contains("_")) text = text.charAt(0) + "." + text.charAt(1) + "."; text = casing(text, node.text()); }
                case "YYYY" -> { number = displayYear; width = 4 + (interval && number < 0 ? 1 : 0); }
                case "YYY" -> { number = displayYear % 1000; width = 3; }
                case "YY" -> { number = displayYear % 100; width = 2; }
                case "Y" -> number = displayYear % 10;
                case "Y_YYY" -> text = displayYear / 1000 + "," + Formatting.number(displayYear % 1000, 3);
                case "IYYY", "IYY", "IY", "I" -> { width = upper.length(); number = interval || isoYear > 0 ? isoYear : 1 - isoYear; if (width < 4) { long divisor = 1; for (int i = 0; i < width; i++) divisor *= 10; number %= divisor; } }
                case "CC" -> { number = interval ? year / 100 : year > 0 ? (year - 1) / 100 + 1 : -((1 - year) / 100 + 1); width = 2 + (number < 0 ? 1 : 0); }
                case "MONTH", "MON" -> { text = casing((upper.equals("MONTH") ? FormattingCatalog.MONTHS_FULL : FormattingCatalog.MONTHS_ABBREV).get(month - 1), node.text()); if (upper.equals("MONTH") && !node.fill() && !node.translated()) text += " ".repeat(Math.max(0, 9 - text.length())); }
                case "DAY", "DY" -> { text = casing((upper.equals("DAY") ? FormattingCatalog.DAYS_FULL : FormattingCatalog.DAYS_ABBREV).get(dow), node.text()); if (upper.equals("DAY") && !node.fill() && !node.translated()) text += " ".repeat(Math.max(0, 9 - text.length())); }
                case "MM" -> { number = month; width = 2 + (number < 0 ? 1 : 0); }
                case "RM" -> {
                    int romanMonth = month > 0 ? month : month < 0 ? 12 + month + 1 : year > 0 ? 12 : year < 0 ? 1 : 0;
                    text = romanMonth == 0 ? "" : (id.equals("rm") ? FormattingCatalog.RM_MONTHS_LOWER : FormattingCatalog.RM_MONTHS_UPPER).get(12 - romanMonth);
                    if (!text.isEmpty() && !node.fill()) text += " ".repeat(4 - text.length());
                }
                case "DDD" -> { number = doy; width = 3; }
                case "IDDD" -> { number = isoDay; width = 3; }
                case "DD" -> { number = day; width = 2; }
                case "D" -> number = dow + 1;
                case "ID" -> number = isoDow(dayCount);
                case "W" -> number = (day - 1) / 7 + 1;
                case "WW" -> { number = (doy - 1) / 7 + 1; width = 2; }
                case "IW" -> { number = (isoDay - 1) / 7 + 1; width = 2; }
                case "Q" -> number = (month - 1) / 3 + 1;
                case "J" -> number = dayCount + 2440588;
                case "TZ" -> text = id.equals("tz") ? zone.toLowerCase(Locale.ROOT) : zone;
                case "TZH" -> text = (offset < 0 ? "-" : "+") + Formatting.number(Math.abs(offset) / 3600, node.fill() ? 0 : 2);
                case "TZM" -> { number = Math.abs(offset) / 60 % 60; width = 2; }
                case "OF" -> text = (offset < 0 ? "-" : "+") + Formatting.number(Math.abs(offset) / 3600, node.fill() ? 0 : 2) + (offset % 3600 == 0 ? "" : ":" + Formatting.number(Math.abs(offset) / 60 % 60, 2));
                case "FX" -> text = "";
                default -> throw new SqlException("0A000", "datetime formatting token is not implemented: " + id);
            }
            if (text == null) { if (interval && number < 0 && Set.of("HH", "HH12", "HH24", "MI", "SS").contains(upper)) width++; text = Formatting.number(number, node.fill() ? 0 : width); if (node.suffix().equals("TH") || node.suffix().equals("th")) text += Formatting.ordinal(number, node.suffix().equals("TH")); }
            output.append(text);
        }
        return output.toString();
    }
    static int isoDow(long day) { return Math.floorMod(day + 3, 7) + 1; }
    static long isoStart(long year) { long fourth = Temporal.days(year, 1, 4); return fourth - isoDow(fourth) + 1; }
    private static String casing(String text, String pattern) {
        if (pattern.equals(pattern.toLowerCase(Locale.ROOT))) return text.toLowerCase(Locale.ROOT);
        if (pattern.equals(pattern.toUpperCase(Locale.ROOT))) return text.toUpperCase(Locale.ROOT);
        return text;
    }
    static SqlValue read(String input, String template, SqlType type, CostMeter meter) {
        return DateTimeInput.read(input, template, type, meter);
    }
}
