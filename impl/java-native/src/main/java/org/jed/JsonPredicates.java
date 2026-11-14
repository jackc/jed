package org.jed;

import java.util.HashSet;

/** Lexical validation stays independent of JSONB's Unicode decoding and number range. */
final class JsonPredicates {
    private JsonPredicates() {}
    static ScalarPlan.Expression resolve(SqlParser.Unary syntax, ScalarPlan.Resolver resolver) {
        if (!syntax.op().startsWith("is json:")) return null;
        String[] options = syntax.op().split(":"); boolean unique = Boolean.parseBoolean(options[2]), negate = Boolean.parseBoolean(options[3]);
        ScalarPlan.Expression input = resolver.resolve(syntax.value(), SqlType.TEXT);
        if (input.type().kind() != SqlType.Kind.TEXT && input.type() != SqlType.JSON && input.type() != SqlType.JSONB && input.type() != SqlType.UNKNOWN) throw new SqlException("42804", "IS JSON requires text or JSON");
        return new ScalarPlan.Expression(SqlType.BOOLEAN, meter -> {
            meter.operation(); SqlValue value = input.run(meter); if (value.isNull()) return SqlValue.nullValue(SqlType.BOOLEAN);
            boolean matches;
            try {
                JsonNode node = value.type() == SqlType.JSONB ? value.asJsonb() : unique ? JsonInput.decodedValidation(value.type() == SqlType.JSON ? value.asJson() : value.asText()) : JsonNode.parse(value.type() == SqlType.JSON ? value.asJson() : value.asText(), false);
                matches = switch (options[1]) { case "array" -> node.kind() == JsonNode.Kind.ARRAY; case "object" -> node.kind() == JsonNode.Kind.OBJECT; case "scalar" -> node.kind() != JsonNode.Kind.ARRAY && node.kind() != JsonNode.Kind.OBJECT; default -> true; };
                if (unique && !unique(node)) matches = false;
            } catch (SqlException error) { if (!error.sqlState().startsWith("22")) throw error; matches = false; }
            return SqlValue.ofBoolean(negate != matches);
        }, input.constant());
    }
    static boolean unique(JsonNode node) {
        if (node.kind() == JsonNode.Kind.OBJECT) {
            HashSet<String> seen = new HashSet<>();
            for (JsonNode.Member member : node.members()) if (!seen.add(member.key()) || !unique(member.value())) return false;
        } else if (node.kind() == JsonNode.Kind.ARRAY) for (JsonNode item : node.elements()) if (!unique(item)) return false;
        return true;
    }
}
