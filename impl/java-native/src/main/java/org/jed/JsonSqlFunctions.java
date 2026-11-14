package org.jed;

import java.util.ArrayList;
import java.util.List;
import java.util.Set;

/** SQL/JSON constructor and query syntax, with independently resolved result/default types. */
final class JsonSqlFunctions {
    private JsonSqlFunctions() {}
    record Behavior(String kind, ScalarPlan.Expression value) {}
    static ScalarPlan.Expression resolve(SqlParser.JsonSql syntax, ScalarPlan.Resolver resolver) {
        boolean query = syntax.path() != null;
        SqlType output = syntax.returning() == null ? switch (syntax.kind()) { case "json_exists" -> SqlType.BOOLEAN; case "json_value", "json_serialize" -> SqlType.TEXT; case "json_query" -> SqlType.JSONB; default -> SqlType.JSON; } : resolver.type(syntax.returning());
        ScalarPlan.Expression input = resolver.resolve(syntax.context(), query ? SqlType.JSONB : syntax.kind().equals("json_scalar") ? null : SqlType.TEXT);
        if (!syntax.kind().equals("json_scalar") && input.type().kind() != SqlType.Kind.TEXT && input.type() != SqlType.JSON && input.type() != SqlType.JSONB && input.type() != SqlType.UNKNOWN && input.type() != SqlType.BYTEA) throw new SqlException("42804", "JSON context must be text or JSON");
        if (!query) return constructor(syntax, input, output);
        JsonPath path = JsonPath.compile(syntax.path());
        ArrayList<ScalarPlan.Expression> passing = new ArrayList<>();
        for (SqlParser.JsonPassing argument : syntax.passing()) {
            ScalarPlan.Expression expression = resolver.resolve(argument.value(), argument.format() ? SqlType.JSONB : null);
            if (!argument.format() && Set.of(SqlType.Kind.DATE, SqlType.Kind.TIMESTAMP, SqlType.Kind.TIMESTAMPTZ).contains(expression.type().kind())) throw new SqlException("0A000", "JSON passing type is not supported");
            if (argument.encoding() != null) { if (expression.type() != SqlType.BYTEA) throw new SqlException("42804", "JSON ENCODING requires bytea"); if (!argument.encoding().equalsIgnoreCase("utf8")) throw new SqlException("22023", "JSON encoding conversion is not supported"); }
            passing.add(expression);
        }
        Behavior empty = behavior(syntax.onEmpty(), output, resolver, "null"), error = behavior(syntax.onError(), output, resolver, syntax.kind().equals("json_exists") ? "false" : "null");
        boolean constant = input.constant() && passing.stream().allMatch(ScalarPlan.Expression::constant);
        return new ScalarPlan.Expression(output, meter -> {
            meter.operation(); SqlValue source = input.run(meter); if (source.isNull()) return SqlValue.nullValue(output);
            // Context conversion errors precede path evaluation and are outside ON ERROR.
            JsonNode document = document(source);
            ArrayList<JsonNode.Member> variables = new ArrayList<>();
            for (int i = 0; i < passing.size(); i++) { SqlValue value = passing.get(i).run(meter); JsonNode node = passing(value, syntax.passing().get(i).format(), meter); String name = syntax.passing().get(i).name(); if (variables.stream().noneMatch(member -> member.key().equals(name))) variables.add(new JsonNode.Member(name, node)); }
            List<JsonNode> items;
            try {
                items = JsonPathEval.evaluate(path, document, JsonNode.object(variables).canonical(), false, syntax.kind().equals("json_exists"), meter, passing.size() + 1, syntax.passing().stream().map(SqlParser.JsonPassing::name).toList()).items();
                if (syntax.kind().equals("json_exists")) return SqlValue.ofBoolean(!items.isEmpty());
            } catch (SqlException failure) { if (!failure.sqlState().startsWith("22")) throw failure; return executeBehavior(error, output, meter, failure); }
            if (items.isEmpty()) return executeBehavior(empty, output, meter, new SqlException("22035", "JSON path yielded no item"));
            try {
                if (syntax.kind().equals("json_value")) {
                    if (items.size() != 1) throw new SqlException("22034", "JSON path yielded more than one item");
                    JsonNode item = items.getFirst();
                    if (item.kind() == JsonNode.Kind.ARRAY || item.kind() == JsonNode.Kind.OBJECT) throw new SqlException("2203F", "JSON_VALUE requires a scalar");
                    return scalar(item, output, meter);
                }
                boolean wrap = syntax.wrapper().equals("unconditional") || syntax.wrapper().equals("conditional") && items.size() != 1;
                if (!wrap && items.size() != 1) throw new SqlException("22034", "JSON path yielded more than one item");
                JsonNode node = wrap ? JsonNode.array(items) : items.getFirst();
                SqlValue value;
                if (syntax.omitQuotes() && node.kind() == JsonNode.Kind.STRING && output.kind() == SqlType.Kind.TEXT) value = SqlValue.ofText(output, node.string());
                else value = image(node, output, meter);
                if (wrap && !constant) meter.scalarInput(MemoryAccount.payload(value));
                return value;
            } catch (SqlException failure) { if (!failure.sqlState().startsWith("22")) throw failure; return executeBehavior(error, output, meter, failure); }
        }, constant);
    }
    private static ScalarPlan.Expression constructor(SqlParser.JsonSql syntax, ScalarPlan.Expression input, SqlType output) {
        if (syntax.kind().equals("json") && output != SqlType.JSON && output != SqlType.JSONB) throw new SqlException("42804", "JSON RETURNING requires JSON type");
        if (syntax.kind().equals("json_serialize") && output.kind() != SqlType.Kind.TEXT && output != SqlType.BYTEA) throw new SqlException("42804", "JSON_SERIALIZE RETURNING requires text or bytea");
        return new ScalarPlan.Expression(output, meter -> {
            meter.operation(); SqlValue value = input.run(meter); if (value.isNull()) return SqlValue.nullValue(output);
            SqlValue result;
            if (syntax.kind().equals("json_scalar")) result = SqlValue.ofJson(JsonImages.text(value, meter, false));
            else {
                String text = value.type() == SqlType.JSON ? value.asJson() : value.type() == SqlType.JSONB ? value.asJsonb().render() : value.type() == SqlType.BYTEA ? SqlText.decode(value.asBytes()) : value.asText();
                JsonNode validated = syntax.unique() ? JsonInput.decodedValidation(text) : JsonNode.parse(text, false);
                if (syntax.unique() && !JsonPredicates.unique(validated)) throw new SqlException("22030", "duplicate JSON object key");
                result = syntax.kind().equals("json") ? output == SqlType.JSON ? SqlValue.ofJson(text) : SqlValue.ofJsonb(JsonNode.parse(text, true)) : output == SqlType.BYTEA ? SqlValue.ofBytes(SqlText.encode(text)) : SqlValue.ofText(output, text);
            }
            if (!input.constant() && (syntax.kind().equals("json_scalar") || syntax.kind().equals("json_serialize"))) meter.scalarInput(MemoryAccount.payload(result));
            return result;
        }, input.constant());
    }
    static JsonNode passing(SqlValue value, boolean format, CostMeter meter) {
        if (value.isNull()) return JsonNode.NULL;
        if (format) return value.type().kind() == SqlType.Kind.TEXT || value.type() == SqlType.BYTEA || value.type() == SqlType.JSON || value.type() == SqlType.JSONB ? document(value) : JsonImages.node(value, meter);
        if (value.type().isFloat()) {
            double number = value.asFloat(); if (!Double.isFinite(number)) throw new SqlException("0A000", "nonfinite jsonpath number is not supported");
            Decimal exact = Decimal.fromDoubleExact(number); int places = (value.type() == SqlType.F32 ? 6 : 15) - exact.precision() + exact.scale();
            Decimal rounded = exact.roundPlaces(places), unit = Decimal.parse("1e" + -places);
            if (exact.subtract(rounded).abs().multiply(Decimal.of(2)).compareTo(unit) == 0 && (rounded.divide(unit, 0).toLongExact() & 1) != 0) rounded = rounded.subtract(exact.negative() ? unit.negate() : unit);
            return JsonNode.number(rounded.rescale(rounded.minimumScale()));
        }
        return JsonImages.node(value, meter);
    }
    static JsonNode document(SqlValue value) { return value.type() == SqlType.JSONB ? value.asJsonb() : JsonNode.parse(value.type() == SqlType.JSON ? value.asJson() : value.type() == SqlType.BYTEA ? SqlText.decode(value.asBytes()) : value.asText(), true).canonical(); }
    static SqlValue scalar(JsonNode node, SqlType type, CostMeter meter) {
        if (node.kind() == JsonNode.Kind.NULL) return SqlValue.nullValue(type);
        if (type == SqlType.JSON || type == SqlType.JSONB) return image(node, type, meter);
        SqlValue source = switch (node.kind()) { case NUMBER -> SqlValue.ofDecimal(node.number()); case BOOLEAN -> SqlValue.ofBoolean(node.booleanValue()); case STRING -> SqlValue.ofText(node.string()); default -> throw new SqlException("2203F", "JSON item is not scalar"); };
        try { return SqlCasts.allowed(source.type(), type) ? SqlCasts.cast(source, type, meter) : SqlCasts.literal(source.toString(), type, meter); }
        catch (SqlException failure) { if (Set.of(SqlType.Kind.DATE, SqlType.Kind.TIMESTAMP, SqlType.Kind.TIMESTAMPTZ).contains(type.kind()) || !failure.sqlState().startsWith("22") && !failure.sqlState().equals("42846")) throw failure; throw new SqlException("2203A", "JSON item cannot be coerced to result type"); }
    }
    static SqlValue image(JsonNode node, SqlType type, CostMeter meter) {
        if (type == SqlType.JSONB) return SqlValue.ofJsonb(node);
        if (type == SqlType.JSON) return SqlValue.ofJson(node.render());
        if (type == SqlType.BYTEA) return SqlValue.ofBytes(SqlText.encode(node.render()));
        return SqlCasts.cast(SqlValue.ofText(node.render()), type, meter);
    }
    static Behavior behavior(SqlParser.JsonBehavior syntax, SqlType output, ScalarPlan.Resolver resolver, String fallback) {
        if (syntax == null) return new Behavior(fallback, null);
        if (!syntax.kind().equals("default")) return new Behavior(syntax.kind(), null);
        validateDefault(syntax.value());
        ScalarPlan.Expression value;
        if (syntax.value() instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.STRING) { SqlValue parsed = resolver.literalValue(literal.text(), output); value = new ScalarPlan.Expression(output, ignored -> parsed, true); }
        else value = resolver.resolve(syntax.value(), output);
        if (value.type() != SqlType.JSONB && value.type() != SqlType.BOOLEAN && !SqlCasts.allowed(value.type(), output)) throw new SqlException("42846", "JSON default cannot be coerced");
        return new Behavior("default", value);
    }
    private static void validateDefault(SqlParser.Expr syntax) {
        SqlParser.Expr root = syntax;
        while (root instanceof SqlParser.Cast || root instanceof SqlParser.Collate) root = root instanceof SqlParser.Cast cast ? cast.value() : ((SqlParser.Collate) root).value();
        boolean shape = root instanceof SqlParser.Literal || root instanceof SqlParser.StoredValue || root instanceof SqlParser.Call || root instanceof SqlParser.JsonSql
            || root instanceof SqlParser.Unary unary && !unary.op().equals("not")
            || root instanceof SqlParser.Binary binary && !Set.of("and", "or").contains(binary.op());
        if (!shape) throw new SqlException("42804", "JSON default must be a constant expression");
        defaultColumns(syntax); DefaultExpression.validate(syntax);
    }
    private static void defaultColumns(SqlParser.Expr syntax) {
        if (syntax instanceof SqlParser.Name) throw new SqlException("42804", "JSON default cannot reference a column");
        for (SqlParser.Expr child : DefaultExpression.children(syntax)) if (child != null) defaultColumns(child);
    }
    static SqlValue executeBehavior(Behavior behavior, SqlType type, CostMeter meter, SqlException failure) {
        switch (behavior.kind) {
            case "error" -> throw failure;
            case "null", "unknown" -> { return SqlValue.nullValue(type); }
            case "true", "false" -> { return SqlCasts.cast(SqlValue.ofBoolean(behavior.kind.equals("true")), type, meter); }
            case "array", "object" -> { return image(behavior.kind.equals("array") ? JsonNode.array(List.of()) : JsonNode.object(List.of()), type, meter); }
            case "default" -> {
                SqlValue value = behavior.value.run(meter); if (value.isNull()) return SqlValue.nullValue(type);
                if (value.type() == SqlType.BOOLEAN && (type == SqlType.JSON || type == SqlType.JSONB)) return image(JsonNode.bool(value.asBoolean()), type, meter);
                if ((value.type() == SqlType.JSONB || value.type() == SqlType.BOOLEAN && type != SqlType.I32 && type != SqlType.BOOLEAN) && type != SqlType.JSON && type != SqlType.JSONB) {
                    try { return scalar(value.type() == SqlType.JSONB ? value.asJsonb() : JsonNode.bool(value.asBoolean()), type, meter); }
                    catch (SqlException error) { if (!error.sqlState().startsWith("22")) throw error; throw new SqlException("42804", "JSON default cannot be coerced"); }
                }
                return SqlCasts.cast(value, type, meter);
            }
            default -> throw new AssertionError();
        }
    }
}
