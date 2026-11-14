package org.jed;

import java.util.ArrayList;
import java.util.List;
import java.util.Set;

/** Path replacement and insertion, retaining PostgreSQL's missing-intermediate rule. */
final class JsonMutators {
    private JsonMutators() {}
    static ScalarPlan.Expression resolve(SqlParser.Call call, ScalarPlan.Resolver resolver) {
        String name = call.name(); if (!Set.of("jsonb_set", "jsonb_set_lax", "jsonb_insert").contains(name)) return null;
        if (call.args().size() < 3 || call.args().size() > (name.equals("jsonb_set_lax") ? 5 : 4)) throw missing();
        var args = new ArrayList<ScalarPlan.Expression>(); SqlType[] types = { SqlType.JSONB, SqlType.array(SqlType.TEXT), SqlType.JSONB, SqlType.BOOLEAN, SqlType.TEXT };
        for (int i = 0; i < call.args().size(); i++) { var arg = resolver.resolve(call.args().get(i), types[i]); if (!arg.type().equals(types[i])) throw missing(); args.add(arg); }
        return new ScalarPlan.Expression(SqlType.JSONB, meter -> {
            meter.operation(); SqlValue[] values = new SqlValue[5];
            for (int i = 0; i < args.size(); i++) values[i] = args.get(i).run(meter);
            if (values[3] == null) values[3] = SqlValue.ofBoolean(!name.equals("jsonb_insert"));
            if (values[4] == null) values[4] = SqlValue.ofText("use_json_null");
            if (values[0].isNull() || values[1].isNull() || values[3].isNull() || !name.equals("jsonb_set_lax") && values[2].isNull()) return SqlValue.nullValue(SqlType.JSONB);
            if (name.equals("jsonb_set_lax") && values[4].isNull()) throw new SqlException("22023", "invalid null_value_treatment");
            JsonNode base = values[0].asJsonb(); SqlArray path = values[1].asArray();
            if (values[2].isNull()) {
                switch (values[4].asText()) {
                    case "return_target" -> { return values[0]; }
                    case "delete_key" -> { return SqlValue.ofJsonb(JsonOperators.deletePath(base, path, 0)); }
                    case "raise_exception" -> throw new SqlException("22004", "JSON value must not be null");
                    case "use_json_null" -> { }
                    default -> throw new SqlException("22023", "invalid null_value_treatment");
                }
            }
            if (path.dimensions() > 1) throw new SqlException("2202E", "wrong number of array subscripts");
            if (base.kind() != JsonNode.Kind.ARRAY && base.kind() != JsonNode.Kind.OBJECT) throw new SqlException("22023", "cannot set path in scalar");
            JsonNode replacement = values[2].isNull() ? JsonNode.NULL : values[2].asJsonb();
            JsonNode changed = set(base, path, 0, replacement, values[3].asBoolean(), name.equals("jsonb_insert"));
            SqlValue result = SqlValue.ofJsonb(changed); if (!args.stream().allMatch(ScalarPlan.Expression::constant)) meter.scalarInput(MemoryAccount.payload(result)); return result;
        }, args.stream().allMatch(ScalarPlan.Expression::constant));
    }
    static JsonNode set(JsonNode node, SqlArray path, int at, JsonNode value, boolean flag, boolean insert) {
        if (at >= path.size()) return node;
        if (!insert && !flag && (node.kind() == JsonNode.Kind.OBJECT && node.members().isEmpty() || node.kind() == JsonNode.Kind.ARRAY && node.elements().isEmpty())) return node;
        SqlValue step = path.element(at); if (step.isNull()) throw new SqlException("22004", "path element is NULL");
        boolean last = at + 1 == path.size();
        if (node.kind() == JsonNode.Kind.OBJECT) {
            String key = step.asText(); boolean found = false; var members = new ArrayList<JsonNode.Member>();
            for (JsonNode.Member member : node.members()) {
                if (!member.key().equals(key)) { members.add(member); continue; }
                found = true;
                if (last && insert) throw new SqlException("22023", "cannot replace existing key");
                members.add(new JsonNode.Member(key, last ? value : set(member.value(), path, at + 1, value, flag, insert)));
            }
            if (!found && last && (insert || flag)) members.add(new JsonNode.Member(key, value));
            return JsonNode.object(members).canonical();
        }
        if (node.kind() != JsonNode.Kind.ARRAY) return node;
        long index;
        try { index = Integer.parseInt(step.asText().stripLeading()); } catch (NumberFormatException failure) { throw new SqlException("22P02", "path element is not an integer"); }
        boolean negative = index < 0; if (negative) index += node.elements().size();
        var elements = new ArrayList<>(node.elements());
        if (index >= 0 && index < elements.size()) {
            int i = (int) index;
            if (!last) elements.set(i, set(elements.get(i), path, at + 1, value, flag, insert));
            else if (insert) elements.add(i + (flag ? 1 : 0), value);
            else elements.set(i, value);
        } else if (last && (insert || flag)) elements.add(negative ? 0 : elements.size(), value);
        return JsonNode.array(elements);
    }
    private static SqlException missing() { return new SqlException("42883", "JSON mutator does not exist for argument types"); }
}
