package org.jed;

import java.util.Locale;

/** Explicit primitive casts. Resolution validates the shared cast matrix before execution. */
public final class SqlCasts {
    private SqlCasts() {}
    public static SqlValue literal(String text, SqlType target) {
        return literal(text, target, null);
    }
    static SqlValue literal(String text, SqlType target, CostMeter meter) {
        if (meter != null && meter.immutableInput()) IndexDependencies.validateLiteral(text, target);
        if (target.kind() == SqlType.Kind.RANGE) return SqlValue.ofRange(target, RangeInput.parse(text, target.element(), meter));
        if (target.kind() == SqlType.Kind.COMPOSITE) return CompositeValues.parse(text, target, meter);
        if (target.kind() == SqlType.Kind.ARRAY) return SqlValue.ofArray(target, ArrayInput.parse(text, target.element(), meter));
        if (target.kind() == SqlType.Kind.INTERVAL) return SqlValue.ofInterval(target, IntervalInput.parse(text, target.intervalFields()));
        if (target.kind() == SqlType.Kind.TIMESTAMPTZ && meter != null) return SqlValue.ofTimestamp(target, Temporal.parseTimestamp(text, true, meter.zone(), null));
        return cast(SqlValue.ofText(text), target);
    }

    public static SqlValue cast(SqlValue source, SqlType target) {
        return cast(source, target, null);
    }
    static SqlValue cast(SqlValue source, SqlType target, CostMeter meter) {
        SqlType from = source.type();
        if (target.kind() == SqlType.Kind.RANGE && !from.equals(target) && from != SqlType.UNKNOWN) throw new SqlException("0A000", "runtime range casts are not supported");
        if (!allowed(from, target)) throw SqlException.typeMismatch("cannot cast " + from + " to " + target);
        if (source.isNull()) return SqlValue.nullValue(target);
        if (target == SqlType.JSONPATH) {
            if (from == SqlType.JSONPATH) return source;
            String text = source.asText();
            if (meter != null) { meter.charge(Costs.JSONPATH_COMPILE * SqlText.encodedLength(text)); meter.guard(); }
            return SqlValue.ofJsonPath(JsonPath.compile(text));
        }
        if (target == SqlType.JSON) return from == SqlType.JSON ? source : SqlValue.ofJson(from == SqlType.JSONB ? source.asJsonb().render() : source.asText());
        if (target == SqlType.JSONB) return from == SqlType.JSONB ? source : SqlValue.ofJsonb(JsonNode.parse(from == SqlType.JSON ? source.asJson() : source.asText(), true));
        if (from.equals(target) && (from.kind() == SqlType.Kind.ARRAY || from.isComposite())) return source;
        if (target.isComposite()) {
            if (from.kind() == SqlType.Kind.TEXT) return CompositeValues.parse(source.asText(), target, meter);
            var fields = new java.util.ArrayList<SqlValue>();
            for (int i = 0; i < target.fields().size(); i++) fields.add(cast(source.asComposite().get(i), target.fields().get(i).type(), meter));
            return SqlValue.ofComposite(target, fields);
        }
        if (target.kind() == SqlType.Kind.ARRAY) return SqlValue.ofArray(target, from.kind() == SqlType.Kind.ARRAY ? source.asArray().cast(target.element(), meter) : ArrayInput.parse(source.asText(), target.element(), meter));
        if (target.kind() == SqlType.Kind.TEXT) {
            String text;
            if ((from.kind() == SqlType.Kind.ARRAY || from.isComposite()) && meter != null) {
                CompositeValues.chargeZones(source, meter);
                text = Temporal.renderValue(source, meter.zone());
            } else if (from.kind() == SqlType.Kind.TIMESTAMPTZ && meter != null) {
                if (!TemporalOps.infinite(source.asTimestamp())) { meter.charge(Costs.TIMEZONE); meter.guard(); }
                text = Temporal.renderValue(source, meter.zone());
            } else text = from.kind() == SqlType.Kind.TEXT ? source.asText() : source.toString();
            if (target.maxLength() >= 0 && SqlText.length(text) > target.maxLength()) {
                text = text.substring(0, text.offsetByCodePoints(0, target.maxLength()));
            }
            return SqlValue.ofText(target, text);
        }
        if (target.isInteger()) {
            long number;
            if (from.isInteger()) number = source.asInteger();
            else if (from.kind() == SqlType.Kind.DECIMAL) number = source.asDecimal().toLongRounded();
            else if (from.isFloat()) number = Decimal.fromDoubleExact(source.asFloat()).toLongRounded();
            else if (from.kind() == SqlType.Kind.BOOLEAN) number = source.asBoolean() ? 1 : 0;
            else number = parseInteger(source.asText(), target);
            return SqlValue.ofInteger(target, number);
        }
        if (target.kind() == SqlType.Kind.DECIMAL) {
            Decimal value;
            if (from.kind() == SqlType.Kind.DECIMAL) value = source.asDecimal();
            else if (from.isInteger()) value = Decimal.of(source.asInteger());
            else if (from.isFloat()) value = Decimal.fromDoubleExact(source.asFloat());
            else value = Decimal.parse(source.asText());
            return SqlValue.ofDecimal(target, value);
        }
        if (target.isFloat()) {
            if (from.isFloat()) return SqlValue.ofFloat(target, source.asFloat());
            if (from.kind() == SqlType.Kind.TEXT) return parseFloat(source.asText(), target);
            if (from.isInteger()) return SqlValue.ofFloat(target, target == SqlType.F32 ? (double)(float)source.asInteger() : (double)source.asInteger());
            return parseFloat(source.asDecimal().toString(), target);
        }
        if (target.kind() == SqlType.Kind.BOOLEAN) {
            if (from.kind() == SqlType.Kind.BOOLEAN) return source;
            if (from.kind() == SqlType.Kind.I32) return SqlValue.ofBoolean(source.asInteger() != 0);
            return SqlValue.ofBoolean(parseBoolean(source.asText()));
        }
        if (target.kind() == SqlType.Kind.INTERVAL) {
            if (from.kind() == SqlType.Kind.INTERVAL) return SqlValue.ofInterval(target, source.asInterval());
            return SqlValue.ofInterval(target, SqlInterval.parse(source.asText()));
        }
        if (target.kind() == SqlType.Kind.DATE) {
            if (from.kind() == SqlType.Kind.DATE) return source;
            if (from.kind() == SqlType.Kind.TEXT) return SqlValue.ofDate(meter != null && Temporal.clockDate(source.asText()) ? Temporal.clockDate(source.asText(), meter) : Temporal.parseDate(source.asText()));
            long micros = source.asTimestamp();
            if (from.kind() == SqlType.Kind.TIMESTAMPTZ && meter != null && !TemporalOps.infinite(micros)) { meter.charge(Costs.TIMEZONE); meter.guard(); micros = meter.zone().toLocal(micros); }
            return SqlValue.ofDate(micros == Long.MIN_VALUE ? Integer.MIN_VALUE : micros == Long.MAX_VALUE ? Integer.MAX_VALUE : (int) Math.floorDiv(micros, Temporal.DAY_MICROS));
        }
        if (target.kind() == SqlType.Kind.TIMESTAMP || target.kind() == SqlType.Kind.TIMESTAMPTZ) {
            long micros;
            if (from.kind() == SqlType.Kind.TEXT) micros = Temporal.parseTimestamp(source.asText(), target.kind() == SqlType.Kind.TIMESTAMPTZ, meter == null ? null : meter.zone(), meter);
            else if (from.kind() == SqlType.Kind.DATE) {
                int day = source.asDate();
                micros = day == Integer.MIN_VALUE ? Long.MIN_VALUE : day == Integer.MAX_VALUE ? Long.MAX_VALUE : Temporal.assemble(day, 0);
            } else micros = source.asTimestamp();
            if (meter != null && from.kind() != SqlType.Kind.TEXT && !TemporalOps.infinite(micros)) {
                if (from.kind() == SqlType.Kind.TIMESTAMPTZ && target.kind() == SqlType.Kind.TIMESTAMP) { meter.charge(Costs.TIMEZONE); meter.guard(); micros = meter.zone().toLocal(micros); }
                else if (from.kind() != SqlType.Kind.TIMESTAMPTZ && target.kind() == SqlType.Kind.TIMESTAMPTZ) { meter.charge(Costs.TIMEZONE); meter.guard(); micros = meter.zone().toInstant(micros); }
            }
            return SqlValue.ofTimestamp(target, micros);
        }
        if (target.kind() == SqlType.Kind.UUID) {
            if (from.kind() == SqlType.Kind.UUID) return source;
            return SqlValue.ofUuid(from.kind() == SqlType.Kind.BYTEA ? SqlUuid.fromBytes(source.asBytes()) : SqlUuid.parse(source.asText()));
        }
        if (target.kind() == SqlType.Kind.BYTEA) {
            if (from.kind() == SqlType.Kind.UUID) return SqlValue.ofBytes(source.asUuid().toBytes());
            if (from.kind() == SqlType.Kind.BYTEA) return source;
            return SqlValue.ofBytes(parseBytes(source.asText()));
        }
        if (from.equals(target)) return source;
        throw new SqlException("0A000", "cast not yet implemented: " + from + " to " + target);
    }

    public static boolean allowed(SqlType from, SqlType to) {
        if (from.kind() == SqlType.Kind.UNKNOWN) return true;
        if (to == SqlType.JSONPATH) return from.kind() == SqlType.Kind.TEXT || from == SqlType.JSONPATH;
        if (to == SqlType.JSON || to == SqlType.JSONB) return from == SqlType.TEXT || from == SqlType.JSON || from == SqlType.JSONB;
        if (to.kind() == SqlType.Kind.RANGE) return from.equals(to) || from.kind() == SqlType.Kind.TEXT;
        if (from.kind() == SqlType.Kind.RANGE) return from.equals(to);
        if (from.isComposite() || to.isComposite()) {
            if (to.kind() == SqlType.Kind.TEXT || from.kind() == SqlType.Kind.TEXT && to.kind() == SqlType.Kind.COMPOSITE) return true;
            if (from.equals(to)) return true;
            if (from.kind() != SqlType.Kind.RECORD || !to.isComposite() || from.fields().size() != to.fields().size()) return false;
            for (int i = 0; i < from.fields().size(); i++) if (!allowed(from.fields().get(i).type(), to.fields().get(i).type())) return false;
            return true;
        }
        if (from.kind() == SqlType.Kind.ARRAY && to.kind() == SqlType.Kind.ARRAY) return allowed(from.element(), to.element());
        if (from.kind() == SqlType.Kind.ARRAY && to.kind() == SqlType.Kind.TEXT || from.kind() == SqlType.Kind.TEXT && to.kind() == SqlType.Kind.ARRAY) return true;
        if (from.kind() == to.kind()) return true;
        String source = from.kind().name().toLowerCase(Locale.ROOT);
        String target = to.kind().name().toLowerCase(Locale.ROOT);
        for (CastCatalog.Fact fact : CastCatalog.FACTS) {
            if (fact.from().equals(source) && fact.to().equals(target)) return true;
        }
        return false;
    }

    private static long parseInteger(String input, SqlType target) {
        String text = trim(input); int at = 0; boolean negative = false;
        if (!text.isEmpty() && (text.charAt(0) == '+' || text.charAt(0) == '-')) negative = text.charAt(at++) == '-';
        int radix = 10;
        if (at + 1 < text.length() && text.charAt(at) == '0') {
            radix = switch (text.charAt(at + 1)) { case 'x', 'X' -> 16; case 'o', 'O' -> 8; case 'b', 'B' -> 2; default -> 10; };
            if (radix != 10) at += 2;
        }
        long minimum = target == SqlType.I16 ? Short.MIN_VALUE : target == SqlType.I32 ? Integer.MIN_VALUE : Long.MIN_VALUE;
        long value = 0; boolean digitSeen = false, separated = false;
        for (; at < text.length(); at++) {
            char c = text.charAt(at);
            if (c == '_') {
                if (separated || !digitSeen && radix == 10) throw invalid("integer");
                separated = true; continue;
            }
            int digit = c >= '0' && c <= '9' ? c - '0' : c >= 'a' && c <= 'f' ? c - 'a' + 10 : c >= 'A' && c <= 'F' ? c - 'A' + 10 : -1;
            if (digit < 0 || digit >= radix) throw invalid("integer");
            if (value < minimum / radix || value * radix < minimum + digit) throw SqlException.numericOverflow();
            value = value * radix - digit; digitSeen = true; separated = false;
        }
        if (!digitSeen || separated) throw invalid("integer");
        if (!negative && value == minimum) throw SqlException.numericOverflow();
        return negative ? value : -value;
    }

    private static boolean parseBoolean(String input) {
        String text = trim(input).toLowerCase(Locale.ROOT);
        if (text.equals("1")) return true;
        if (text.equals("0")) return false;
        if (!text.isEmpty()) {
            boolean yes = "true".startsWith(text) || "yes".startsWith(text) || "on".startsWith(text);
            boolean no = "false".startsWith(text) || "no".startsWith(text) || "off".startsWith(text);
            if (yes != no) return yes;
        }
        throw invalid("boolean");
    }

    private static SqlValue parseFloat(String input, SqlType type) {
        String text = trim(input);
        int i = 0;
        boolean negative = false;
        if (!text.isEmpty() && (text.charAt(0) == '-' || text.charAt(0) == '+')) negative = text.charAt(i++) == '-';
        String word = text.substring(i).toLowerCase(Locale.ROOT);
        if (word.equals("nan")) return SqlValue.ofFloat(type, Double.NaN);
        if (word.equals("inf") || word.equals("infinity")) {
            return SqlValue.ofFloat(type, negative ? Double.NEGATIVE_INFINITY : Double.POSITIVE_INFINITY);
        }
        boolean point = false, nonzero = false, digitSeen = false;
        for (; i < text.length(); i++) {
            char c = text.charAt(i);
            if (c >= '0' && c <= '9') { digitSeen = true; nonzero |= c != '0'; }
            else if (c == '.' && !point) point = true;
            else break;
        }
        if (!digitSeen) throw invalid(type.toString());
        if (i < text.length() && (text.charAt(i) == 'e' || text.charAt(i) == 'E')) {
            i++;
            if (i < text.length() && (text.charAt(i) == '+' || text.charAt(i) == '-')) i++;
            int digits = i;
            while (i < text.length() && text.charAt(i) >= '0' && text.charAt(i) <= '9') i++;
            if (i == digits) throw invalid(type.toString());
        }
        if (i != text.length()) throw invalid(type.toString());
        double number;
        try { number = type.kind() == SqlType.Kind.F32 ? Float.parseFloat(text) : Double.parseDouble(text); }
        catch (NumberFormatException failure) { throw invalid(type.toString()); }
        if (!Double.isFinite(number) || (number == 0 && nonzero)) throw SqlException.numericOverflow();
        return SqlValue.ofFloat(type, number);
    }

    private static byte[] parseBytes(String text) {
        try {
            return text.startsWith("\\x") ? BinaryText.decode(text.substring(2), "hex") : BinaryText.decode(text, "escape");
        } catch (SqlException failure) {
            throw new SqlException("22P02", failure.getMessage());
        }
    }

    private static boolean space(char c) { return c == ' ' || (c >= 9 && c <= 13); }
    private static String trim(String text) {
        int start = 0, end = text.length();
        while (start < end && space(text.charAt(start))) start++;
        while (end > start && space(text.charAt(end - 1))) end--;
        return text.substring(start, end);
    }
    private static SqlException invalid(String type) { return new SqlException("22P02", "invalid input syntax for type " + type); }
}
