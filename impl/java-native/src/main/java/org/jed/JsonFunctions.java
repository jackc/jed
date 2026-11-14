package org.jed;

import java.util.ArrayList;
import java.util.List;
import java.util.Set;

/** Scalar document readers and typed variadic construction. */
final class JsonFunctions {
    private JsonFunctions() {}
    static ScalarPlan.Expression resolve(SqlParser.Call call, ScalarPlan.Resolver resolver) {
        ScalarPlan.Expression path = JsonPathFunctions.resolve(call, resolver); if (path != null) return path;
        ScalarPlan.Expression object = JsonObjects.resolve(call, resolver); if (object != null) return object;
        ScalarPlan.Expression mutation = JsonMutators.resolve(call, resolver); if (mutation != null) return mutation;
        String name = call.name();
        if (Set.of("json_build_array", "jsonb_build_array", "json_build_object", "jsonb_build_object").contains(name)) return builder(call, resolver);
        boolean image = Set.of("to_json", "to_jsonb", "array_to_json", "row_to_json").contains(name);
        boolean reader = Set.of("json_typeof", "jsonb_typeof", "json_array_length", "jsonb_array_length", "json_strip_nulls", "jsonb_strip_nulls", "jsonb_pretty").contains(name);
        if (!image && !reader) return null;
        boolean optional = Set.of("array_to_json", "row_to_json", "json_strip_nulls", "jsonb_strip_nulls").contains(name);
        if (call.args().isEmpty() || call.args().size() > (optional ? 2 : 1)) throw missing();
        SqlType source = name.startsWith("jsonb_") ? SqlType.JSONB : SqlType.JSON;
        ScalarPlan.Expression value = resolver.resolve(call.args().getFirst(), image ? null : source);
        if (reader && value.type() != source) throw missing();
        if (name.equals("array_to_json") && value.type().kind() != SqlType.Kind.ARRAY && value.type() != SqlType.UNKNOWN) throw missing();
        if (name.equals("row_to_json") && !value.type().isComposite() && value.type() != SqlType.UNKNOWN) {
            if (call.args().getFirst() instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.STRING) throw new SqlException("0A000", "input of anonymous composite types is not implemented");
            throw missing();
        }
        ScalarPlan.Expression flag = call.args().size() == 2 ? booleanArgument(call.args().get(1), resolver) : null;
        if (flag != null && flag.type() != SqlType.BOOLEAN && flag.type() != SqlType.UNKNOWN) throw missing();
        SqlType result = name.endsWith("typeof") || name.equals("jsonb_pretty") ? SqlType.TEXT : name.endsWith("array_length") ? SqlType.I32 : name.equals("to_jsonb") || name.startsWith("jsonb_") ? SqlType.JSONB : SqlType.JSON;
        return new ScalarPlan.Expression(result, meter -> {
            meter.operation(); SqlValue input = value.run(meter); if (input.isNull()) return SqlValue.nullValue(result);
            SqlValue option = flag == null ? SqlValue.ofBoolean(false) : flag.run(meter); if (option.isNull()) return SqlValue.nullValue(result);
            SqlValue output;
            if (image) output = result == SqlType.JSONB ? SqlValue.ofJsonb(JsonImages.node(input, meter)) : SqlValue.ofJson(JsonImages.text(input, meter, option.asBoolean()));
            else {
                boolean decode = !name.endsWith("typeof") && !name.endsWith("array_length");
                JsonNode node = source == SqlType.JSONB ? input.asJsonb() : JsonNode.parse(input.asJson(), decode);
                if (name.endsWith("typeof")) return SqlValue.ofText(node.kind().name().toLowerCase(java.util.Locale.ROOT));
                if (name.endsWith("array_length")) { if (node.kind() != JsonNode.Kind.ARRAY) throw new SqlException("22023", "cannot get array length of a non-array"); return SqlValue.ofInteger(SqlType.I32, node.elements().size()); }
                if (name.equals("jsonb_pretty")) {
                    meter.scalarInput(MemoryAccount.payload(input)); meter.scalarOutput(prettyBytes(node, 0)); return SqlValue.ofText(pretty(node, 0));
                }
                JsonNode stripped = strip(node, option.asBoolean()); output = result == SqlType.JSONB ? SqlValue.ofJsonb(stripped) : SqlValue.ofJson(compact(stripped));
            }
            if (!value.constant() || flag != null && !flag.constant()) meter.scalarInput(MemoryAccount.payload(output));
            return output;
        }, value.constant() && (flag == null || flag.constant()));
    }
    static ScalarPlan.Expression booleanArgument(SqlParser.Expr syntax, ScalarPlan.Resolver resolver) {
        if (syntax instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.STRING) { SqlValue parsed = resolver.literalValue(literal.text(), SqlType.BOOLEAN); return new ScalarPlan.Expression(SqlType.BOOLEAN, ignored -> parsed, true); }
        return resolver.resolve(syntax, SqlType.BOOLEAN);
    }
    private static ScalarPlan.Expression builder(SqlParser.Call call, ScalarPlan.Resolver resolver) {
        boolean binary = call.name().startsWith("jsonb"), object = call.name().endsWith("object");
        List<ScalarPlan.Expression> args = new ArrayList<>(); boolean variadic = call.args().size() == 1 && call.args().getFirst() instanceof SqlParser.Unary unary && unary.op().equals("variadic");
        if (variadic) { SqlParser.Unary unary = (SqlParser.Unary) call.args().getFirst(); ScalarPlan.Expression array = resolver.resolve(unary.value(), null); if (array.type().kind() != SqlType.Kind.ARRAY && array.type() != SqlType.UNKNOWN) throw SqlException.typeMismatch("VARIADIC argument must be array"); args.add(array); }
        else for (SqlParser.Expr syntax : call.args()) args.add(resolver.resolve(syntax, null));
        SqlType result = binary ? SqlType.JSONB : SqlType.JSON;
        return new ScalarPlan.Expression(result, meter -> {
            meter.operation(); var values = new ArrayList<SqlValue>();
            for (ScalarPlan.Expression arg : args) {
                SqlValue value = arg.run(meter);
                if (variadic) { if (value.isNull()) return SqlValue.nullValue(result); values.addAll(value.asArray().elements()); }
                else values.add(value);
            }
            if (object && values.size() % 2 != 0) throw new SqlException("22023", "argument list must have even number of elements");
            for (int i = 0; i < values.size(); i++) meter.operation();
            SqlValue output;
            if (binary) {
                if (object) { var members = new ArrayList<JsonNode.Member>(); for (int i = 0; i < values.size(); i += 2) members.add(new JsonNode.Member(JsonImages.key(values.get(i), meter, true), JsonImages.node(values.get(i + 1), meter))); output = SqlValue.ofJsonb(JsonNode.object(members)); }
                else { var items = new ArrayList<JsonNode>(); for (SqlValue value : values) items.add(JsonImages.node(value, meter)); output = SqlValue.ofJsonb(JsonNode.array(items)); }
            } else {
                StringBuilder out = new StringBuilder(object ? "{" : "[");
                for (int i = 0; i < values.size(); i += object ? 2 : 1) { if (i != 0) out.append(", "); if (object) { JsonNode.quote(out, JsonImages.key(values.get(i), meter, false)); out.append(" : "); } out.append(JsonImages.text(values.get(i + (object ? 1 : 0)), meter, false)); }
                output = SqlValue.ofJson(out.append(object ? '}' : ']').toString());
            }
            if (!args.stream().allMatch(ScalarPlan.Expression::constant)) meter.scalarInput(MemoryAccount.payload(output));
            return output;
        }, args.stream().allMatch(ScalarPlan.Expression::constant));
    }
    static JsonNode strip(JsonNode node, boolean arrays) {
        if (node.kind() == JsonNode.Kind.ARRAY) { var items = new ArrayList<JsonNode>(); for (JsonNode child : node.elements()) if (!arrays || child.kind() != JsonNode.Kind.NULL) items.add(strip(child, arrays)); return JsonNode.array(items); }
        if (node.kind() == JsonNode.Kind.OBJECT) { var members = new ArrayList<JsonNode.Member>(); for (JsonNode.Member member : node.members()) if (member.value().kind() != JsonNode.Kind.NULL) members.add(new JsonNode.Member(member.key(), strip(member.value(), arrays))); return JsonNode.object(members); }
        return node;
    }
    static String compact(JsonNode node) {
        if (node.kind() != JsonNode.Kind.ARRAY && node.kind() != JsonNode.Kind.OBJECT) return node.render();
        StringBuilder out = new StringBuilder(node.kind() == JsonNode.Kind.ARRAY ? "[" : "{");
        if (node.kind() == JsonNode.Kind.ARRAY) { for (int i = 0; i < node.elements().size(); i++) { if (i != 0) out.append(','); out.append(compact(node.elements().get(i))); } }
        else for (int i = 0; i < node.members().size(); i++) { if (i != 0) out.append(','); JsonNode.Member member = node.members().get(i); JsonNode.quote(out, member.key()); out.append(':').append(compact(member.value())); }
        return out.append(node.kind() == JsonNode.Kind.ARRAY ? ']' : '}').toString();
    }
    static long prettyBytes(JsonNode node, int depth) {
        boolean array = node.kind() == JsonNode.Kind.ARRAY, object = node.kind() == JsonNode.Kind.OBJECT;
        int count = array ? node.elements().size() : object ? node.members().size() : 0;
        if (count == 0) return array || object ? 3L + depth * 4L : node.renderedBytes();
        long bytes = 4L + depth * 4L;
        for (int i = 0; i < count; i++) {
            long entry = (depth + 1L) * 4 + (i == 0 ? 0 : 2);
            if (array) entry = MemoryAccount.add(entry, prettyBytes(node.elements().get(i), depth + 1));
            else { JsonNode.Member member = node.members().get(i); entry = MemoryAccount.add(entry, JsonNode.quotedBytes(member.key()) + 2 + prettyBytes(member.value(), depth + 1)); }
            bytes = MemoryAccount.add(bytes, entry);
        }
        return bytes;
    }
    static String pretty(JsonNode node, int depth) {
        if (node.kind() == JsonNode.Kind.ARRAY && node.elements().isEmpty()) return "[\n" + "    ".repeat(depth) + "]";
        if (node.kind() == JsonNode.Kind.OBJECT && node.members().isEmpty()) return "{\n" + "    ".repeat(depth) + "}";
        if (node.kind() == JsonNode.Kind.ARRAY && !node.elements().isEmpty()) { var parts = new ArrayList<String>(); for (JsonNode child : node.elements()) parts.add("    ".repeat(depth + 1) + pretty(child, depth + 1)); return "[\n" + String.join(",\n", parts) + "\n" + "    ".repeat(depth) + "]"; }
        if (node.kind() == JsonNode.Kind.OBJECT && !node.members().isEmpty()) { var parts = new ArrayList<String>(); for (JsonNode.Member member : node.members()) { StringBuilder key = new StringBuilder(); JsonNode.quote(key, member.key()); parts.add("    ".repeat(depth + 1) + key + ": " + pretty(member.value(), depth + 1)); } return "{\n" + String.join(",\n", parts) + "\n" + "    ".repeat(depth) + "}"; }
        return node.render();
    }
    private static SqlException missing() { return new SqlException("42883", "JSON function does not exist for argument types"); }
}
