package org.jed;

import java.util.ArrayList;

/** The one- and two-array json_object overloads preserve the array shape checks. */
final class JsonObjects {
    private JsonObjects() {}
    static ScalarPlan.Expression resolve(SqlParser.Call call, ScalarPlan.Resolver resolver) {
        if (!call.name().equals("json_object") && !call.name().equals("jsonb_object")) return null;
        if (call.args().isEmpty() || call.args().size() > 2) throw new SqlException("42883", "invalid JSON object arguments");
        var args = new ArrayList<ScalarPlan.Expression>(); SqlType array = SqlType.array(SqlType.TEXT);
        for (SqlParser.Expr syntax : call.args()) { var expression = resolver.resolve(syntax, array); if (!expression.type().equals(array)) throw new SqlException("42883", "JSON object requires text arrays"); args.add(expression); }
        boolean binary = call.name().equals("jsonb_object"); SqlType type = binary ? SqlType.JSONB : SqlType.JSON;
        return new ScalarPlan.Expression(type, meter -> {
            meter.operation(); SqlValue first = args.getFirst().run(meter); if (first.isNull()) return SqlValue.nullValue(type);
            SqlArray a = first.asArray(), b = null;
            if (args.size() == 2) { SqlValue second = args.get(1).run(meter); if (second.isNull()) return SqlValue.nullValue(type); b = second.asArray(); }
            if (b != null && (a.dimensions() > 1 || b.dimensions() > 1 || a.size() != b.size())) throw dimensions();
            if (b == null && (a.dimensions() > 2 || a.dimensions() == 2 && a.length(1) != 2 || a.size() % 2 != 0)) throw dimensions();
            var members = new ArrayList<JsonNode.Member>(); StringBuilder text = new StringBuilder("{"); int count = b == null ? a.size() / 2 : a.size();
            for (int i = 0; i < count; i++) {
                meter.operation();
                SqlValue key = a.element(b == null ? i * 2 : i), value = b == null ? a.element(i * 2 + 1) : b.element(i);
                if (key.isNull()) throw new SqlException("22004", "null value not allowed for object key");
                JsonNode node = value.isNull() ? JsonNode.NULL : JsonNode.string(value.asText());
                members.add(new JsonNode.Member(key.asText(), node));
                if (i != 0) text.append(", "); JsonNode.quote(text, key.asText()); text.append(" : "); node.render(text);
            }
            SqlValue output = binary ? SqlValue.ofJsonb(JsonNode.object(members)) : SqlValue.ofJson(text.append('}').toString());
            if (!args.stream().allMatch(ScalarPlan.Expression::constant)) meter.scalarInput(MemoryAccount.payload(output)); return output;
        }, args.stream().allMatch(ScalarPlan.Expression::constant));
    }
    private static SqlException dimensions() { return new SqlException("2202E", "invalid JSON object array dimensions"); }
}
