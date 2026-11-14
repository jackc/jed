package org.jed;

import java.util.List;
import java.util.Set;

/** Calendar field kernels use the same handwritten Gregorian calendar as literal input. */
final class TemporalFunctions {
    private TemporalFunctions() {}
    private static final Set<String> TIME = Set.of("microseconds", "milliseconds", "second", "minute", "hour");
    private static final Set<String> ISO = Set.of("dow", "isodow", "doy", "isoyear", "julian");
    private static final Set<String> MONOTONE = Set.of("year", "decade", "century", "millennium", "julian", "isoyear", "epoch");
    private static final Set<String> TRUNC = Set.of("microseconds", "milliseconds", "second", "minute", "hour", "day", "week", "month", "quarter", "year", "decade", "century", "millennium");
    static String unit(String input, boolean trunc, SqlType type) {
        StringBuilder folded = new StringBuilder();
        for (int i = 0; i < input.length() && i < DatetimeUnits.TOKEN_MAX_LEN; i++) {
            char c = input.charAt(i); folded.append(c >= 'A' && c <= 'Z' ? (char) (c + 32) : c);
        }
        String token = folded.toString();
        for (String table : trunc ? List.of("delta") : List.of("delta", "date")) {
            for (DatetimeUnits.Fact fact : DatetimeUnits.FACTS) {
                if (!fact.table().equals(table) || !fact.token().equals(token)) continue;
                if (fact.type().equals("units")) return fact.value();
                if (!trunc && fact.type().equals("reserv")) {
                    if (fact.value().equals("epoch")) return "epoch";
                    if (type.kind() != SqlType.Kind.INTERVAL) throw unsupported(input, type);
                }
                throw unrecognized(input, type);
            }
        }
        throw unrecognized(input, type);
    }
    static void validateExtract(String field, SqlType type) { validate(unit(field, false, type), type); }
    private static void validate(String field, SqlType type) {
        SqlType.Kind kind = type.kind();
        if (kind != SqlType.Kind.DATE && kind != SqlType.Kind.TIMESTAMP && kind != SqlType.Kind.TIMESTAMPTZ && kind != SqlType.Kind.INTERVAL)
            throw new SqlException("42883", "EXTRACT source type is unsupported");
        if (kind == SqlType.Kind.DATE && TIME.contains(field) || kind == SqlType.Kind.INTERVAL && ISO.contains(field)
            || field.startsWith("timezone") && kind != SqlType.Kind.TIMESTAMPTZ) throw unsupported(field, type);
    }
    static SqlValue extract(String requested, SqlValue value, boolean asFloat, CostMeter meter) {
        SqlType original = value.type();
        SqlType type = asFloat && original.kind() == SqlType.Kind.DATE ? SqlType.TIMESTAMP : original;
        String field = unit(requested, false, type);
        boolean date = original.kind() == SqlType.Kind.DATE;
        long instant = date ? value.asDate() : original.kind() == SqlType.Kind.INTERVAL ? 0 : value.asTimestamp();
        boolean infinite = date ? instant == Integer.MIN_VALUE || instant == Integer.MAX_VALUE : TemporalOps.infinite(instant);
        // date_part's infinite timestamp case accepts all timezone fields as oscillating NULLs.
        if (!(asFloat && infinite && field.startsWith("timezone"))) validate(field, type);
        if (infinite) {
            if (!MONOTONE.contains(field)) return SqlValue.nullValue(asFloat ? SqlType.F64 : SqlType.DECIMAL);
            if (!asFloat) throw new SqlException("22003", "cannot extract field from an infinite timestamp");
            return SqlValue.ofFloat(SqlType.F64, instant < 0 ? Double.NEGATIVE_INFINITY : Double.POSITIVE_INFINITY);
        }
        if (original.kind() == SqlType.Kind.TIMESTAMPTZ && !field.equals("epoch")) {
            meter.charge(Costs.TIMEZONE); meter.guard();
            if (field.startsWith("timezone")) {
                int offset = meter.zone().offsetAt(instant).seconds();
                int valuePart = field.equals("timezone_hour") ? offset / 3600 : field.equals("timezone_minute") ? offset / 60 % 60 : offset;
                return asFloat ? SqlValue.ofFloat(SqlType.F64, valuePart) : SqlValue.ofDecimal(Decimal.of(valuePart));
            }
            instant = meter.zone().toLocal(instant);
        }
        Decimal result;
        if (original.kind() == SqlType.Kind.INTERVAL) result = intervalField(field, value.asInterval());
        else {
            long day = date ? instant : Math.floorDiv(instant, Temporal.DAY_MICROS);
            long time = date ? 0 : Math.floorMod(instant, Temporal.DAY_MICROS);
            if (asFloat && field.equals("julian")) return SqlValue.ofFloat(SqlType.F64, day + 2440588 + (time / 1_000_000 + (time % 1_000_000) / 1_000_000.0) / 86400.0);
            result = calendarField(field, day, time, date, instant);
        }
        return asFloat ? SqlValue.ofFloat(SqlType.F64, Double.parseDouble(result.toString())) : SqlValue.ofDecimal(result);
    }
    private static Decimal calendarField(String field, long day, long time, boolean date, long instant) {
        Temporal.Civil civil = Temporal.civil(day);
        long year = civil.year(), displayed = year <= 0 ? year - 1 : year;
        long seconds = time % 60_000_000;
        return switch (field) {
            case "microseconds" -> Decimal.of(seconds);
            case "milliseconds" -> fraction(seconds, 3);
            case "second" -> fraction(seconds, 6);
            case "minute" -> Decimal.of(time / 60_000_000 % 60);
            case "hour" -> Decimal.of(time / 3_600_000_000L);
            case "day" -> Decimal.of(civil.day());
            case "month" -> Decimal.of(civil.month());
            case "quarter" -> Decimal.of((civil.month() - 1) / 3 + 1);
            case "year" -> Decimal.of(displayed);
            case "decade" -> Decimal.of(Math.floorDiv(year, 10));
            case "century" -> Decimal.of(year > 0 ? (year - 1) / 100 + 1 : year / 100 - 1);
            case "millennium" -> Decimal.of(year > 0 ? (year - 1) / 1000 + 1 : year / 1000 - 1);
            case "dow" -> Decimal.of(Math.floorMod(day + 4, 7));
            case "isodow" -> Decimal.of(isoDow(day));
            case "doy" -> Decimal.of(day - Temporal.days(year, 1, 1) + 1);
            case "isoyear" -> { long iso = isoYear(day); yield Decimal.of(iso <= 0 ? iso - 1 : iso); }
            case "week" -> Decimal.of((day - isoStart(isoYear(day))) / 7 + 1);
            case "epoch" -> date ? Decimal.of(day).multiply(Decimal.of(86400)) : fraction(instant, 6);
            case "julian" -> date ? Decimal.of(day + 2440588) : Decimal.of(day + 2440588).add(Decimal.of(time).divide(Decimal.of(Temporal.DAY_MICROS)));
            case "timezone", "timezone_hour", "timezone_minute" -> Decimal.of(0);
            default -> throw new AssertionError(field);
        };
    }
    private static Decimal intervalField(String field, SqlInterval value) {
        long seconds = value.micros() % 60_000_000;
        return switch (field) {
            case "microseconds" -> Decimal.of(seconds);
            case "milliseconds" -> fraction(seconds, 3);
            case "second" -> fraction(seconds, 6);
            case "minute" -> Decimal.of(value.micros() / 60_000_000 % 60);
            case "hour" -> Decimal.of(value.micros() / 3_600_000_000L);
            case "day" -> Decimal.of(value.days());
            case "week" -> Decimal.of(value.days() / 7);
            case "month" -> Decimal.of(value.months() % 12);
            case "quarter" -> Decimal.of(value.months() >= 0 ? value.months() % 12 / 3 + 1 : -((-(long) value.months() % 12) / 3 + 1));
            case "year" -> Decimal.of(value.months() / 12);
            case "decade" -> Decimal.of(value.months() / 120);
            case "century" -> Decimal.of(value.months() / 1200);
            case "millennium" -> Decimal.of(value.months() / 12000);
            case "epoch" -> Decimal.of(value.months() / 12L * 31557600 + value.months() % 12L * 2592000 + value.days() * 86400L).add(fraction(value.micros(), 6));
            default -> throw unsupported(field, SqlType.INTERVAL);
        };
    }
    private static Decimal fraction(long value, int digits) { return Decimal.of(value).divideInternal(Decimal.parse("1" + "0".repeat(digits)), digits); }
    private static int isoDow(long day) { return Math.floorMod(day + 3, 7) + 1; }
    private static long isoYear(long day) { return Temporal.civil(day + 4 - isoDow(day)).year(); }
    private static long isoStart(long year) { long january4 = Temporal.days(year, 1, 4); return january4 - isoDow(january4) + 1; }
    static ScalarPlan.Expression resolve(String name, List<ScalarPlan.Expression> args) {
        if (Set.of("uuidv4", "gen_random_uuid", "uuidv7").contains(name)) {
            boolean version7 = name.equals("uuidv7");
            if ((!version7 && !args.isEmpty()) || args.size() > 1 || !args.isEmpty() && args.getFirst().type().kind() != SqlType.Kind.INTERVAL)
                throw new SqlException("42883", "UUID generator argument type is unsupported");
            return new ScalarPlan.Expression(SqlType.UUID, meter -> {
                meter.operation();
                SqlInterval shift = null;
                if (!args.isEmpty()) { SqlValue value = args.getFirst().run(meter); if (value.isNull()) return SqlValue.nullValue(SqlType.UUID); shift = value.asInterval(); }
                byte[] bytes = new byte[16];
                if (version7) {
                    long instant = meter.statementTime();
                    if (shift != null) instant = TemporalOps.shift(instant, shift, false, true, meter);
                    long millis = Math.floorDiv(instant, 1000);
                    if (millis < 0 || millis >= 1L << 48) throw Temporal.overflow();
                    for (int i = 0; i < 6; i++) bytes[i] = (byte) (millis >>> (40 - i * 8));
                    int counter = meter.nextUuidCounter(); bytes[6] = (byte) (0x70 | counter >>> 8); bytes[7] = (byte) counter;
                    byte[] random = new byte[8]; meter.random(random); System.arraycopy(random, 0, bytes, 8, 8);
                } else { meter.random(bytes); bytes[6] = (byte) (bytes[6] & 15 | 0x40); }
                bytes[8] = (byte) (bytes[8] & 63 | 0x80);
                return SqlValue.ofUuid(SqlUuid.fromBytes(bytes));
            });
        }
        if (Set.of("now", "current_date", "clock_timestamp").contains(name)) {
            if (!args.isEmpty()) throw new SqlException("42883", "clock function takes no arguments");
            SqlType result = name.equals("current_date") ? SqlType.DATE : SqlType.TIMESTAMPTZ;
            return new ScalarPlan.Expression(result, meter -> {
                meter.operation(); long instant = name.equals("clock_timestamp") ? meter.clockTime() : meter.statementTime();
                if (TemporalOps.infinite(instant)) throw Temporal.overflow();
                if (!name.equals("current_date")) return SqlValue.ofTimestamp(result, instant);
                meter.charge(Costs.TIMEZONE); meter.guard();
                return SqlValue.ofDate((int) Math.floorDiv(meter.zone().toLocal(instant), Temporal.DAY_MICROS));
            });
        }
        if (name.equals("timezone")) {
            if (args.size() != 2 || (args.getFirst().type().kind() != SqlType.Kind.TEXT && args.getFirst().type() != SqlType.UNKNOWN) || !Set.of(SqlType.Kind.TIMESTAMP, SqlType.Kind.TIMESTAMPTZ).contains(args.get(1).type().kind())) throw new SqlException("42883", "AT TIME ZONE requires timestamp and text");
            SqlType result = args.get(1).type().kind() == SqlType.Kind.TIMESTAMP ? SqlType.TIMESTAMPTZ : SqlType.TIMESTAMP;
            return new ScalarPlan.Expression(result, meter -> {
                meter.operation();
                SqlValue zoneValue = args.getFirst().run(meter);
                if (zoneValue.isNull()) return SqlValue.nullValue(result);
                SqlValue source = args.get(1).run(meter);
                if (source.isNull()) return SqlValue.nullValue(result);
                meter.charge(Costs.TIMEZONE); meter.guard();
                TimeZoneData.Zone zone = meter.zone(zoneValue.asText());
                return SqlValue.ofTimestamp(result, result == SqlType.TIMESTAMP ? zone.toLocal(source.asTimestamp()) : zone.toInstant(source.asTimestamp()));
            });
        }
        if (!name.equals("date_trunc")) return null;
        if ((args.size() != 2 && args.size() != 3) || args.getFirst().type().kind() != SqlType.Kind.TEXT && args.getFirst().type() != SqlType.UNKNOWN
            || !Set.of(SqlType.Kind.TIMESTAMP, SqlType.Kind.TIMESTAMPTZ, SqlType.Kind.INTERVAL).contains(args.get(1).type().kind()))
            throw new SqlException("42883", "date_trunc argument types are unsupported");
        if (args.size() == 3 && (args.get(1).type().kind() != SqlType.Kind.TIMESTAMPTZ || args.get(2).type().kind() != SqlType.Kind.TEXT && args.get(2).type() != SqlType.UNKNOWN)) throw new SqlException("42883", "date_trunc explicit zone requires timestamptz");
        SqlType result = args.get(1).type().kind() == SqlType.Kind.INTERVAL ? SqlType.INTERVAL : args.get(1).type().kind() == SqlType.Kind.TIMESTAMP ? SqlType.TIMESTAMP : SqlType.TIMESTAMPTZ;
        return new ScalarPlan.Expression(result, meter -> {
            meter.operation(); SqlValue unit = args.getFirst().run(meter);
            if (unit.isNull()) return SqlValue.nullValue(result);
            SqlValue value = args.get(1).run(meter);
            if (value.isNull()) return SqlValue.nullValue(result);
            TimeZoneData.Zone zone = meter.zone();
            if (args.size() == 3) { SqlValue named = args.get(2).run(meter); if (named.isNull()) return SqlValue.nullValue(result); zone = meter.zone(named.asText()); }
            return truncate(unit.asText(), value, meter, zone);
        });
    }
    static SqlValue truncate(String requested, SqlValue value, CostMeter meter) {
        return truncate(requested, value, meter, meter.zone());
    }
    private static SqlValue truncate(String requested, SqlValue value, CostMeter meter, TimeZoneData.Zone zone) {
        String field = unit(requested, true, value.type());
        if (!TRUNC.contains(field) || value.type().kind() == SqlType.Kind.INTERVAL && field.equals("week")) throw unsupported(field, value.type());
        if (value.type().kind() == SqlType.Kind.INTERVAL) return SqlValue.ofInterval(truncateInterval(field, value.asInterval()));
        long instant = value.asTimestamp();
        if (TemporalOps.infinite(instant)) return value;
        if (value.type().kind() == SqlType.Kind.TIMESTAMPTZ) { meter.charge(Costs.TIMEZONE); meter.guard(); instant = zone.toLocal(instant); }
        long day = Math.floorDiv(instant, Temporal.DAY_MICROS), time = Math.floorMod(instant, Temporal.DAY_MICROS);
        Temporal.Civil civil = Temporal.civil(day);
        long year = civil.year(); int month = civil.month();
        switch (field) {
            case "microseconds" -> { }
            case "milliseconds" -> time = time / 1000 * 1000;
            case "second" -> time = time / 1_000_000 * 1_000_000;
            case "minute" -> time = time / 60_000_000 * 60_000_000;
            case "hour" -> time = time / 3_600_000_000L * 3_600_000_000L;
            case "day" -> time = 0;
            case "week" -> { day -= isoDow(day) - 1; time = 0; }
            case "month" -> { day = Temporal.days(year, month, 1); time = 0; }
            case "quarter" -> { day = Temporal.days(year, (month - 1) / 3 * 3 + 1, 1); time = 0; }
            default -> {
                if (field.equals("decade")) year = Math.floorDiv(year, 10) * 10;
                else if (field.equals("century")) year = Math.floorDiv(year - 1, 100) * 100 + 1;
                else if (field.equals("millennium")) year = Math.floorDiv(year - 1, 1000) * 1000 + 1;
                day = Temporal.days(year, 1, 1); time = 0;
            }
        }
        long result = Temporal.assemble(day, time);
        if (value.type().kind() == SqlType.Kind.TIMESTAMPTZ) result = zone.toInstant(result);
        return SqlValue.ofTimestamp(value.type().kind() == SqlType.Kind.TIMESTAMP ? SqlType.TIMESTAMP : SqlType.TIMESTAMPTZ, result);
    }
    private static SqlInterval truncateInterval(String field, SqlInterval value) {
        int months = value.months(), days = value.days(); long time = value.micros();
        switch (field) {
            case "microseconds" -> { }
            case "milliseconds" -> time = time / 1000 * 1000;
            case "second" -> time = time / 1_000_000 * 1_000_000;
            case "minute" -> time = time / 60_000_000 * 60_000_000;
            case "hour" -> time = time / 3_600_000_000L * 3_600_000_000L;
            case "day" -> time = 0;
            default -> {
                days = 0; time = 0;
                int factor = switch (field) { case "month" -> 1; case "quarter" -> 3; case "year" -> 12; case "decade" -> 120; case "century" -> 1200; case "millennium" -> 12000; default -> throw new AssertionError(field); };
                months = months / factor * factor;
            }
        }
        return new SqlInterval(months, days, time);
    }
    static SqlInterval age(SqlValue first, SqlValue second, CostMeter meter) {
        long a = first.asTimestamp(), b = second.asTimestamp();
        if (TemporalOps.infinite(a) || TemporalOps.infinite(b)) throw Temporal.overflow();
        boolean negative = a < b;
        if (first.type().kind() == SqlType.Kind.TIMESTAMPTZ) {
            meter.charge(Costs.TIMEZONE); meter.guard(); a = meter.zone().toLocal(a);
            meter.charge(Costs.TIMEZONE); meter.guard(); b = meter.zone().toLocal(b);
        }
        Temporal.Civil x = Temporal.civil(Math.floorDiv(a, Temporal.DAY_MICROS)), y = Temporal.civil(Math.floorDiv(b, Temporal.DAY_MICROS));
        long years = x.year() - y.year(), months = x.month() - y.month(), days = x.day() - y.day();
        long time = Math.floorMod(a, Temporal.DAY_MICROS) - Math.floorMod(b, Temporal.DAY_MICROS);
        if (negative) { years = -years; months = -months; days = -days; time = -time; }
        if (time < 0) { time += Temporal.DAY_MICROS; days--; }
        Temporal.Civil earlier = negative ? x : y;
        while (days < 0) { days += Temporal.monthDays(earlier.year(), earlier.month()); months--; }
        while (months < 0) { months += 12; years--; }
        long direction = negative ? -1 : 1;
        try { return new SqlInterval(Math.toIntExact((years * 12 + months) * direction), Math.toIntExact(days * direction), time * direction); }
        catch (ArithmeticException failure) { throw Temporal.overflow(); }
    }
    private static SqlException unsupported(String field, SqlType type) { return new SqlException("0A000", "unit " + field + " not supported for type " + type); }
    static SqlInterval makeInterval(SqlValue[] args) {
        try {
            int months = Math.toIntExact(Math.addExact(Math.multiplyExact(args[0].asInteger(), 12), args[1].asInteger()));
            int days = Math.toIntExact(Math.addExact(Math.multiplyExact(args[2].asInteger(), 7), args[3].asInteger()));
            long time = Math.addExact(Math.multiplyExact(args[4].asInteger(), 3_600_000_000L), Math.multiplyExact(args[5].asInteger(), 60_000_000));
            time = Math.addExact(time, secondsMicros(args[6].asFloat()));
            return new SqlInterval(months, days, time);
        } catch (ArithmeticException failure) { throw Temporal.overflow(); }
    }
    static SqlValue makeTimestamp(SqlValue[] args, boolean zoned, CostMeter meter) {
        long year = args[0].asInteger(), month = args[1].asInteger(), day = args[2].asInteger();
        long hour = args[3].asInteger(), minute = args[4].asInteger(); double second = args[5].asFloat();
        if (year < -999999 || year > 999999 || year == 0 || month < 1 || month > 12 || day < 1 || day > 31
            || hour < 0 || hour > 24 || minute < 0 || minute > 59 || !(second >= 0 && second <= 60)) throw Temporal.overflow();
        year = year < 0 ? year + 1 : year;
        Temporal.validateDate(year, (int) month, (int) day);
        long time = hour * 3_600_000_000L + minute * 60_000_000 + secondsMicros(second);
        if (time > Temporal.DAY_MICROS) throw Temporal.overflow();
        long instant = Temporal.assemble(Temporal.days(year, (int) month, (int) day), time);
        if (zoned) {
            meter.charge(Costs.TIMEZONE); meter.guard();
            instant = (args.length == 7 ? meter.constructorZone(args[6].asText()) : meter.zone()).toInstant(instant);
        }
        return SqlValue.ofTimestamp(zoned ? SqlType.TIMESTAMPTZ : SqlType.TIMESTAMP, instant);
    }
    private static long secondsMicros(double seconds) {
        double micros = seconds * 1_000_000;
        if (!Double.isFinite(micros)) throw Temporal.overflow();
        try { return Decimal.fromDoubleExact(micros).toLongRounded(); }
        catch (SqlException failure) { throw Temporal.overflow(); }
    }
    private static SqlException unrecognized(String field, SqlType type) { return new SqlException("22023", "unit " + field + " not recognized for type " + type); }
}
