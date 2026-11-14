package org.jed;

import java.util.ArrayList;

/** One typed JSON image path shared by scalar builders and aggregate consumers. */
final class JsonImages {
    private JsonImages() {}
    static JsonNode node(SqlValue value, CostMeter meter) {
        if (value.isNull()) return JsonNode.NULL;
        return switch (value.type().kind()) {
            case JSONB -> value.asJsonb();
            case JSON -> JsonNode.parse(value.asJson(), true).canonical();
            case BOOLEAN -> JsonNode.bool(value.asBoolean());
            case I16, I32, I64 -> JsonNode.number(Decimal.of(value.asInteger()));
            case DECIMAL -> JsonNode.number(value.asDecimal());
            case F32, F64 -> Double.isFinite(value.asFloat()) ? JsonNode.number(Decimal.parse(value.toString())) : JsonNode.string(value.toString());
            case TEXT -> JsonNode.string(value.asText());
            case ARRAY -> array(value.asArray(), 0, 0, meter);
            case COMPOSITE, RECORD -> {
                var members = new ArrayList<JsonNode.Member>();
                for (int i = 0; i < value.asComposite().size(); i++) members.add(new JsonNode.Member(value.type().fields().get(i).name(), node(value.asComposite().get(i), meter)));
                yield JsonNode.object(members);
            }
            default -> JsonNode.string(scalar(value, meter));
        };
    }
    private static JsonNode array(SqlArray array, int dim, int offset, CostMeter meter) {
        var items = new ArrayList<JsonNode>(); if (array.dimensions() == 0) return JsonNode.array(items);
        int stride = 1; for (int d = dim + 1; d < array.dimensions(); d++) stride *= array.length(d);
        for (int i = 0; i < array.length(dim); i++) items.add(dim + 1 == array.dimensions() ? node(array.element(offset + i), meter) : array(array, dim + 1, offset + i * stride, meter));
        return JsonNode.array(items);
    }
    static String scalar(SqlValue value, CostMeter meter) {
        CompositeValues.chargeZones(value, meter);
        String text = Temporal.renderValue(value, meter.zone());
        if (value.type().kind() == SqlType.Kind.TIMESTAMP || value.type().kind() == SqlType.Kind.TIMESTAMPTZ) {
            if (TemporalOps.infinite(value.asTimestamp())) return text;
            int space = text.indexOf(' '); if (space >= 0) text = text.substring(0, space) + 'T' + text.substring(space + 1);
            if (value.type().kind() == SqlType.Kind.TIMESTAMPTZ) {
                String suffix = text.endsWith(" BC") ? " BC" : ""; if (!suffix.isEmpty()) text = text.substring(0, text.length() - 3);
                int sign = Math.max(text.lastIndexOf('+'), text.lastIndexOf('-')); if (text.length() - sign == 3) text += ":00";
                text += suffix;
            }
        }
        return text;
    }
    static String key(SqlValue value, CostMeter meter, boolean binary) {
        if (value.isNull()) throw new SqlException(binary ? "22023" : "22004", "null value not allowed for object key");
        if (value.type().isComposite() || value.type().kind() == SqlType.Kind.ARRAY || value.type() == SqlType.JSON || value.type() == SqlType.JSONB) throw new SqlException("22023", "key value must be scalar");
        return scalar(value, meter);
    }
    static String text(SqlValue value, CostMeter meter, boolean pretty) {
        if (value.isNull()) return "null";
        if (value.type() == SqlType.JSON) return value.asJson();
        if (value.type() == SqlType.JSONB) return value.asJsonb().render();
        if (value.type().isFloat() && Double.isFinite(value.asFloat())) return value.toString();
        if (value.type().isComposite()) {
            StringBuilder out = new StringBuilder("{");
            for (int i = 0; i < value.asComposite().size(); i++) { if (i != 0) out.append(pretty ? ",\n " : ","); JsonNode.quote(out, value.type().fields().get(i).name()); out.append(':').append(text(value.asComposite().get(i), meter, false)); }
            return out.append('}').toString();
        }
        if (value.type().kind() == SqlType.Kind.ARRAY) return arrayText(value.asArray(), 0, 0, meter, pretty);
        return node(value, meter).render();
    }
    private static String arrayText(SqlArray array, int dim, int offset, CostMeter meter, boolean pretty) {
        if (array.dimensions() == 0) return "[]";
        StringBuilder out = new StringBuilder("["); int stride = 1; for (int d = dim + 1; d < array.dimensions(); d++) stride *= array.length(d);
        for (int i = 0; i < array.length(dim); i++) { if (i != 0) out.append(pretty ? ",\n " : ","); out.append(dim + 1 == array.dimensions() ? text(array.element(offset + i), meter, false) : arrayText(array, dim + 1, offset + i * stride, meter, false)); }
        return out.append(']').toString();
    }
}
