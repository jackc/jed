package org.jed;

import java.util.List;
import java.util.stream.Collectors;

/** Canonical spelling after names and types have been resolved by the owning plan. */
final class ExplainExpression {
    private final List<ScalarPlan.Binding> bindings;
    private final ScalarPlan.Resolver resolver;
    private final CostMeter meter;
    ExplainExpression(List<ScalarPlan.Binding> bindings, ScalarPlan.Resolver resolver, CostMeter meter) { this.bindings = bindings; this.resolver = resolver; this.meter = meter; }
    String list(List<SqlParser.Expr> expressions) { return expressions.stream().map(this::print).collect(Collectors.joining(", ")); }
    static String quote(String text) { return "'" + text.replace("\\", "\\\\").replace("'", "''") + "'"; }
    String print(SqlParser.Expr expression) {
        expression = DefaultExpression.unwrap(expression);
        if (expression instanceof SqlParser.Literal literal) return switch (literal.kind()) {
            case STRING -> quote(literal.text());
            case INTEGER -> Long.toString(ScalarPlan.integer(literal.text()));
            case DECIMAL -> Decimal.parse(literal.text()).toString();
            default -> literal.text().equals("null") ? "NULL" : literal.text();
        };
        if (expression instanceof SqlParser.StoredValue stored) return value(stored.value());
        if (expression instanceof SqlParser.UnparsedExpression unparsed) throw StoredDefinitions.error(unparsed.detail());
        if (expression instanceof SqlParser.ColumnSlot slot) return "#" + slot.index();
        if (expression instanceof SqlParser.Parameter parameter) return "$" + parameter.index();
        if (expression instanceof SqlParser.Name name) return name(name);
        if (expression instanceof SqlParser.Cast cast) {
            SqlType type = resolver.type(cast.target());
            if (cast.value() instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.STRING) {
                if (type == SqlType.DATE && Temporal.clockDate(literal.text())) return dateClock(literal.text());
                SqlValue value = meter.unmetered(() -> resolver.resolve(cast, null).run(meter));
                String text = value.type().kind() == SqlType.Kind.BYTEA ? "\\x" + java.util.HexFormat.of().formatHex(value.asBytes()) : SqlCasts.cast(value, SqlType.TEXT).asText();
                return value.type().isFloat() || value.type().isInteger() || value.type().kind() == SqlType.Kind.DECIMAL || value.type() == SqlType.BOOLEAN ? text : quote(text);
            }
            SqlType source = resolver.resolve(cast.value(), null).type();
            if (type.kind() == SqlType.Kind.ARRAY && source.kind() == SqlType.Kind.ARRAY)
                return type.equals(source) ? print(cast.value()) : "array_cast(" + print(cast.value()) + ")";
            return "cast(" + print(cast.value()) + " as " + type + ")";
        }
        if (expression instanceof SqlParser.Binary binary) {
            String left = print(binary.left()), right = print(binary.right());
            if (binary.op().equals("||")) {
                SqlType a = resolver.resolve(binary.left(), null).type(), b = resolver.resolve(binary.right(), null).type();
                if (a.kind() == SqlType.Kind.TEXT && b.kind() != SqlType.Kind.TEXT) right = "cast(" + right + " as text)";
                else if (b.kind() == SqlType.Kind.TEXT && a.kind() != SqlType.Kind.TEXT) left = "cast(" + left + " as text)";
            }
            return "(" + left + " " + binary.op() + " " + right + ")";
        }
        if (expression instanceof SqlParser.Unary unary) {
            if (unary.op().equals("+")) return print(unary.value());
            if (unary.op().equals("not") && unary.value() instanceof SqlParser.Exists) return "not exists(<subquery>)";
            if (unary.op().equals("-")) return "(-" + print(unary.value()) + ")";
            return unary.op().startsWith("is ") ? "(" + print(unary.value()) + " " + unary.op() + ")" : "(" + unary.op() + " " + print(unary.value()) + ")";
        }
        if (expression instanceof SqlParser.Subquery) return "scalar(<subquery>)";
        if (expression instanceof SqlParser.Exists) return "exists(<subquery>)";
        if (expression instanceof SqlParser.Quantified quantified) return print(quantified.subject()) + " " + quantified.operator() + " " + quantified.quantifier() + "(" + (quantified.query() == null ? print(quantified.array()) : "<subquery>") + ")";
        if (expression instanceof SqlParser.Between between) {
            String subject = print(between.subject());
            String comparison = "((" + subject + " >= " + print(between.lower()) + ") and (" + subject + " <= " + print(between.upper()) + "))";
            return between.negated() ? "(not " + comparison + ")" : comparison;
        }
        if (expression instanceof SqlParser.Field field) {
            List<SqlType.Field> fields = resolver.resolve(field.base(), null).type().fields();
            for (int i = 0; i < fields.size(); i++) if (fields.get(i).name().equals(field.name())) return print(field.base()) + ".field" + i;
            throw new IllegalStateException("unresolved composite field in EXPLAIN");
        }
        if (expression instanceof SqlParser.Access access) {
            StringBuilder result = new StringBuilder(print(access.base()));
            for (SqlParser.Subscript subscript : access.subscripts()) {
                result.append('[');
                if (subscript.lower() != null) result.append(print(subscript.lower()));
                if (subscript.slice()) { result.append(':'); if (subscript.upper() != null) result.append(print(subscript.upper())); }
                result.append(']');
            }
            return result.toString();
        }
        if (expression instanceof SqlParser.JsonSql json) return json(json);
        if (expression instanceof SqlParser.Call call && JsonPathFunctions.recognized(call.name())) {
            SqlParser.Call normalized = JsonPathFunctions.normalize(call);
            SqlType[] types = {SqlType.JSONB, SqlType.JSONPATH, SqlType.JSONB, SqlType.BOOLEAN};
            java.util.ArrayList<String> args = new java.util.ArrayList<>();
            for (int i = 0; i < types.length; i++) args.add(typed(normalized.args().get(i), types[i]));
            return call.name() + "(" + String.join(", ", args) + ")";
        }
        if (expression instanceof SqlParser.Call call) {
            call = NamedFunctions.normalize(call);
            return (call.name().equals("substring") ? "substr" : call.name().equals("position") ? "strpos" : call.name()) + "(" + (call.distinct() ? "distinct " : "") + (call.star() ? "*" : list(call.args())) + ")";
        }
        if (expression instanceof SqlParser.Extrema extrema) return extrema.kind() + "(" + list(extrema.args()) + ")";
        if (expression instanceof SqlParser.ArrayConstructor array && array.query() == null) return "array[" + list(array.values()) + "]";
        if (expression instanceof SqlParser.RowConstructor row) return "row(" + list(row.values()) + ")";
        if (expression instanceof SqlParser.Conditional conditional) {
            StringBuilder text = new StringBuilder("(case");
            for (int i = 0; i < conditional.conditions().size(); i++) {
                String condition = print(conditional.conditions().get(i));
                if (conditional.input() != null) condition = "(" + print(conditional.input()) + " = " + condition + ")";
                text.append(" when ").append(condition).append(" then ").append(print(conditional.results().get(i)));
            }
            text.append(" else ").append(conditional.otherwise() == null ? "NULL" : print(conditional.otherwise()));
            return text.append(" end)").toString();
        }
        if (expression instanceof SqlParser.In in) {
            if (in.query() != null) return print(in.subject()) + (in.negated() ? " not in (<subquery>)" : " in (<subquery>)");
            String result = null;
            for (SqlParser.Expr member : in.values()) {
                String comparison = "(" + print(in.subject()) + " = " + print(member) + ")";
                result = result == null ? comparison : "(" + result + " or " + comparison + ")";
            }
            return in.negated() ? "(not " + result + ")" : result;
        }
        if (expression instanceof SqlParser.Collate collate) return print(collate.value());
        throw new SqlException("0A000", "EXPLAIN expression metadata is not connected for " + expression.getClass().getSimpleName());
    }
    private static String value(SqlValue value) {
        if (value.isNull()) return "NULL";
        return value.type().isNumeric() || value.type() == SqlType.BOOLEAN ? value.toString() : quote(value.toString());
    }
    private static String slot(int level, int index) { return level == 0 ? "#" + index : "outer(" + level + "," + index + ")"; }
    private String name(SqlParser.Name name) {
        for (boolean whole : new boolean[]{false, true}) {
            int level = 0; ScalarPlan.Resolver scope = resolver;
            for (List<ScalarPlan.Binding> columns = bindings; scope != null; scope = scope.metadataOuter(), level++, columns = scope == null ? List.of() : scope.metadataBindings()) {
                List<String> prefix = name.parts().subList(0, name.parts().size() - 1);
                int found = -1;
                for (int i = 0; i < columns.size(); i++) {
                    ScalarPlan.Binding binding = columns.get(i);
                    boolean qualified = prefix.isEmpty() ? !binding.qualifiedOnly() : prefix.equals(binding.qualifier()) || prefix.size() == 1 && !binding.qualifier().isEmpty() && prefix.getFirst().equals(binding.qualifier().getLast());
                    boolean matches = whole ? name.parts().size() == 1 && binding.scalarFunction() && binding.qualifier().equals(name.parts()) : binding.name().equals(name.parts().getLast()) && qualified;
                    if (matches) { if (found >= 0) throw new SqlException("42702", "ambiguous column in EXPLAIN"); found = i; }
                }
                if (found >= 0) return slot(level, found);
                if (whole && name.parts().size() == 1) {
                    java.util.ArrayList<String> fields = new java.util.ArrayList<>(); boolean nullable = false; int sentinel = -1;
                    for (int i = 0; i < columns.size(); i++) {
                        ScalarPlan.Binding binding = columns.get(i);
                        if (!binding.whole().allowed() || !binding.qualifier().equals(name.parts())) continue;
                        fields.add(slot(level, i)); nullable |= binding.whole().nullable();
                        if (sentinel < 0 && binding.whole().notNull()) sentinel = i;
                    }
                    if (!fields.isEmpty()) {
                        String row = "row(" + String.join(", ", fields) + ")";
                        return nullable && sentinel >= 0 ? "(case when (" + slot(level, sentinel) + " is null) then NULL else " + row + " end)" : row;
                    }
                }
                if (!prefix.isEmpty() && columns.stream().anyMatch(binding -> prefix.equals(binding.qualifier()) || prefix.size() == 1 && !binding.qualifier().isEmpty() && prefix.getFirst().equals(binding.qualifier().getLast()))) break;
            }
        }
        throw new IllegalStateException("unresolved column in EXPLAIN: " + String.join(".", name.parts()));
    }
    private static String dateClock(String input) {
        String word = input.trim().toLowerCase(java.util.Locale.ROOT);
        return "date_clock(" + (word.equals("tomorrow") ? 1 : word.equals("yesterday") ? -1 : 0) + ")";
    }
    private String typed(SqlParser.Expr syntax, SqlType type) {
        if (syntax instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.STRING) {
            if (type == SqlType.DATE && Temporal.clockDate(literal.text())) return dateClock(literal.text());
            SqlValue value = meter.unmetered(() -> resolver.resolve(syntax, type).run(meter));
            return quote(SqlCasts.cast(value, SqlType.TEXT).asText());
        }
        SqlType source = resolver.resolve(syntax, null).type();
        String text = print(syntax);
        return source == SqlType.JSON && type == SqlType.JSONB ? "cast(" + text + " as jsonb)" : text;
    }
    private String json(SqlParser.JsonSql json) {
        SqlType type = resolver.resolve(json, null).type();
        if (json.path() == null) return json.kind() + "(" + print(json.context()) + " returning " + type + ")";
        StringBuilder result = new StringBuilder(json.kind()).append('(').append(typed(json.context(), SqlType.JSONB))
            .append(", ").append(quote(JsonPath.compile(json.path()).toString()));
        for (int i = 0; i < json.passing().size(); i++) {
            SqlParser.JsonPassing passing = json.passing().get(i);
            SqlType source = resolver.resolve(passing.value(), null).type();
            String value = source == SqlType.JSON || passing.format() ? typed(passing.value(), SqlType.JSONB) : print(passing.value());
            result.append(i == 0 ? " passing " : ", ").append(value).append(" as ").append(TextQuotes.identifier(passing.name()));
        }
        if (!json.kind().equals("json_exists")) result.append(" returning ").append(type);
        if (json.kind().equals("json_query")) {
            result.append(json.wrapper().equals("without") ? " without array wrapper" : " with " + json.wrapper() + " array wrapper");
            result.append(json.omitQuotes() ? " omit quotes on scalar string" : " keep quotes on scalar string");
        }
        if (!json.kind().equals("json_exists")) result.append(' ').append(behavior(json.onEmpty(), type, "null")).append(" on empty");
        result.append(' ').append(behavior(json.onError(), type, json.kind().equals("json_exists") ? "false" : "null")).append(" on error)");
        return result.toString();
    }
    private String behavior(SqlParser.JsonBehavior behavior, SqlType output, String fallback) {
        if (behavior == null) return fallback;
        if (!behavior.kind().equals("default")) return (behavior.kind().equals("array") || behavior.kind().equals("object") ? "empty " : "") + behavior.kind().replace('_', ' ');
        SqlParser.Expr value = behavior.value();
        if (value instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.STRING) return "default " + typed(value, output);
        return "default cast(" + print(value) + " as " + output + ")";
    }

}
