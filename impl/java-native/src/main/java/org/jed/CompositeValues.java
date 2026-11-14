package org.jed;

import java.util.ArrayList;
import java.util.List;

/** Recursive record text I/O and the separate SQL and sorting comparisons. */
final class CompositeValues {
    private CompositeValues() {}
    static String render(SqlValue value, TimeZoneData.Zone zone) {
        StringBuilder out = new StringBuilder("(");
        List<SqlValue> fields = value.asComposite();
        for (int i = 0; i < fields.size(); i++) {
            if (i != 0) out.append(',');
            SqlValue field = fields.get(i); if (field.isNull()) continue;
            String text = field.type() == SqlType.BOOLEAN ? field.asBoolean() ? "t" : "f" : Temporal.renderValue(field, zone);
            boolean quote = text.isEmpty();
            for (int j = 0; j < text.length(); j++) if (",()\"\\".indexOf(text.charAt(j)) >= 0 || Character.isWhitespace(text.charAt(j))) quote = true;
            if (quote) out.append('"');
            for (int j = 0; j < text.length(); j++) { char c = text.charAt(j); if (quote && (c == '"' || c == '\\')) out.append(c); out.append(c); }
            if (quote) out.append('"');
        }
        return out.append(')').toString();
    }
    static SqlValue parse(String source, SqlType type, CostMeter meter) {
        String text = source.strip();
        if (text.length() < 2 || text.charAt(0) != '(' || text.charAt(text.length() - 1) != ')') throw malformed();
        var fields = new ArrayList<SqlValue>();
        if (text.length() == 2 && type.fields().isEmpty()) return SqlValue.ofComposite(type, fields);
        int p = 1;
        while (true) {
            StringBuilder token = new StringBuilder(); boolean quoted = false, anyQuote = false;
            while (p < text.length() - 1) {
                char c = text.charAt(p++);
                if (c == '\\') { if (p >= text.length() - 1) throw malformed(); token.append(text.charAt(p++)); }
                else if (c == '"') {
                    anyQuote = true;
                    if (quoted && p < text.length() - 1 && text.charAt(p) == '"') { token.append('"'); p++; }
                    else quoted = !quoted;
                } else if (!quoted && c == ',') { p--; break; }
                else if (!quoted && (c == '(' || c == ')')) throw malformed();
                else token.append(c);
            }
            if (quoted || fields.size() >= type.fields().size()) throw malformed();
            SqlType fieldType = type.fields().get(fields.size()).type();
            fields.add(token.isEmpty() && !anyQuote ? SqlValue.nullValue(fieldType) : SqlCasts.literal(token.toString(), fieldType, meter));
            if (p == text.length() - 1) break;
            p++;
        }
        if (fields.size() != type.fields().size()) throw malformed();
        return SqlValue.ofComposite(type, fields);
    }
    static SqlException malformed() { return new SqlException("22P02", "malformed record literal"); }
    static SqlException cannotCast() { return new SqlException("42846", "cannot cast composite value"); }
    static boolean allNull(SqlValue value) { return value.isNull() || value.asComposite().stream().allMatch(SqlValue::isNull); }
    static boolean allPresent(SqlValue value) { return !value.isNull() && value.asComposite().stream().noneMatch(SqlValue::isNull); }
    static int compare(SqlValue a, SqlValue b, CostMeter meter) {
        for (int i = 0; i < a.asComposite().size(); i++) {
            SqlValue x = a.asComposite().get(i), y = b.asComposite().get(i);
            int order = x.isNull() ? y.isNull() ? 0 : 1 : y.isNull() ? -1 : ScalarPlan.compare(x, y, meter);
            if (order != 0) return order;
        }
        return 0;
    }
    static SqlValue sqlCompare(String op, SqlValue a, SqlValue b, CostMeter meter) {
        boolean equality = op.equals("=") || op.equals("!=") || op.equals("<>");
        boolean unknown = false; int order = 0;
        for (int i = 0; i < a.asComposite().size(); i++) {
            SqlValue x = a.asComposite().get(i), y = b.asComposite().get(i);
            if (x.isNull() || y.isNull()) { if (!equality) return SqlValue.nullValue(SqlType.BOOLEAN); unknown = true; continue; }
            if (x.type().isComposite()) {
                SqlValue equal = sqlCompare("=", x, y, meter);
                if (equality) {
                    if (equal.isNull()) { unknown = true; continue; }
                    if (!equal.asBoolean()) { order = 1; break; }
                    continue;
                }
                if (!equal.isNull() && equal.asBoolean()) continue;
                return sqlCompare(op, x, y, meter);
            }
            order = ScalarPlan.compare(x, y, meter);
            if (order != 0) break;
        }
        if (order == 0 && unknown) return SqlValue.nullValue(SqlType.BOOLEAN);
        return SqlValue.ofBoolean(switch (op) { case "=" -> order == 0; case "!=", "<>" -> order != 0; case "<" -> order < 0; case ">" -> order > 0; case "<=" -> order <= 0; case ">=" -> order >= 0; default -> throw new AssertionError(op); });
    }
    static void chargeZones(SqlValue value, CostMeter meter) {
        if (value.isNull()) return;
        if (value.type().kind() == SqlType.Kind.TIMESTAMPTZ && !TemporalOps.infinite(value.asTimestamp())) { meter.charge(Costs.TIMEZONE); meter.guard(); }
        else if (value.type().kind() == SqlType.Kind.RANGE) { if (value.asRange().lower() != null) chargeZones(value.asRange().lower(), meter); if (value.asRange().upper() != null) chargeZones(value.asRange().upper(), meter); }
        else if (value.type().isComposite()) for (SqlValue field : value.asComposite()) chargeZones(field, meter);
        else if (value.type().kind() == SqlType.Kind.ARRAY) for (SqlValue item : value.asArray().elements()) chargeZones(item, meter);
    }
}
