package org.jed;

import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

/** Collect contextual constraints for the whole statement, then bind once before any execution. */
final class ParameterBindings {
    private final Engine.Snapshot snapshot;
    private final CostMeter meter;
    private final SqlType[] types;
    private final boolean[] required;
    private final Map<String,List<ScalarPlan.Binding>> ctes = new HashMap<>();
    private final QueryEnvironment environment;
    private ParameterBindings(Engine.Snapshot snapshot, SqlValue[] args, CostMeter meter) {
        this.snapshot = snapshot; this.meter = meter; types = new SqlType[args.length]; required = new boolean[args.length];
        environment = new QueryEnvironment((query, scope) -> { throw new SqlException("42P18", "parameter subquery type is unresolved"); }, args, meter);
        environment.namedTypes(snapshot.types());
    }
    static void bind(SqlParser.Statement statement, Engine.Snapshot snapshot, SqlValue[] args, int parameterCount, CostMeter meter) {
        if (parameterCount == 0) { if (args.length != 0) throw new SqlException("42601", "parameter count mismatch"); return; }
        SqlValue[] shape = new SqlValue[parameterCount]; java.util.Arrays.fill(shape, SqlValue.nullValue(SqlType.UNKNOWN));
        ParameterBindings binder = new ParameterBindings(snapshot, shape, meter);
        try {
            // A second pass propagates constraints from later CTEs or expression siblings.
            binder.statement(statement, List.of(), List.of()); binder.statement(statement, List.of(), List.of());
            for (int i = 0; i < parameterCount; i++) if (binder.types[i] == null || binder.types[i] == SqlType.UNKNOWN || binder.types[i].kind() == SqlType.Kind.RECORD) throw new SqlException("42P18", "could not determine type of parameter $" + (i + 1));
            if (args.length == 0 && statement instanceof SqlParser.Explain explain && !explain.analyze()) return;
            if (args.length != parameterCount) throw new SqlException("42601", "parameter count mismatch");
            SqlValue[] converted = new SqlValue[args.length];
            for (int i = 0; i < args.length; i++) {
                int index = i; converted[i] = meter.unmetered(() -> value(args[index], binder.types[index], meter));
                if (converted[i].isNull() && binder.required[i]) throw new SqlException("23502", "NULL parameter for NOT NULL column");
            }
            System.arraycopy(converted, 0, args, 0, args.length);
        } finally { binder.environment.close(); }
    }
    static SqlValue value(SqlValue value, SqlType target, CostMeter meter) {
        if (value.isNull()) return SqlValue.nullValue(target);
        if (value.type().kind() == SqlType.Kind.TEXT && target.kind() == SqlType.Kind.RANGE) return SqlCasts.literal(value.asText(), target, meter);
        SqlType source = value.type();
        if (target.kind() == SqlType.Kind.RANGE && source.kind() == SqlType.Kind.RANGE) {
            SqlRange range = value.asRange();
            return SqlValue.ofRange(target, range.isEmpty() ? SqlRange.empty() : SqlRange.create(target.element(), range.lower() == null ? null : value(range.lower(), target.element(), meter), range.upper() == null ? null : value(range.upper(), target.element(), meter), range.lowerInclusive(), range.upperInclusive()));
        }
        if (target.isComposite() && source.isComposite()) {
            if (value.asComposite().size() != target.fields().size()) throw SqlException.typeMismatch("composite parameter field count differs");
            var fields = new ArrayList<SqlValue>();
            for (int i = 0; i < target.fields().size(); i++) fields.add(value(value.asComposite().get(i), target.fields().get(i).type(), meter));
            return SqlValue.ofComposite(target, fields);
        }
        if (target.kind() == SqlType.Kind.ARRAY && source.kind() == SqlType.Kind.ARRAY) {
            var elements = new ArrayList<SqlValue>(); for (SqlValue element : value.asArray().elements()) elements.add(value(element, target.element(), meter));
            return SqlValue.ofArray(target, new SqlArray(value.asArray().dims(), value.asArray().lowerBounds(), elements));
        }
        boolean allowed = source.kind() == target.kind() || source.isInteger() && (target.isInteger() || target == SqlType.DECIMAL || target.isFloat()) || (source.isFloat() || source == SqlType.DECIMAL) && target.isFloat()
            || source.kind() == SqlType.Kind.TEXT && (target.kind() == SqlType.Kind.ARRAY || target.kind() == SqlType.Kind.RANGE || target.isComposite() || List.of(SqlType.Kind.BYTEA, SqlType.Kind.UUID, SqlType.Kind.DATE, SqlType.Kind.TIMESTAMP, SqlType.Kind.TIMESTAMPTZ, SqlType.Kind.INTERVAL, SqlType.Kind.JSON, SqlType.Kind.JSONB, SqlType.Kind.JSONPATH).contains(target.kind()));
        if (!allowed) throw SqlException.typeMismatch("parameter value is incompatible with " + target);
        return SqlCasts.cast(value, target, meter);
    }
    private SqlType constrain(SqlParser.Parameter parameter, SqlType hint, boolean notNull) {
        int index = parameter.index() - 1;
        if (index >= types.length) throw new SqlException("42601", "parameter count mismatch");
        if (hint != null && hint != SqlType.UNKNOWN && hint.kind() != SqlType.Kind.RECORD) types[index] = merge(types[index], hint);
        required[index] |= notNull;
        return types[index] == null ? SqlType.UNKNOWN : types[index];
    }
    private static SqlType merge(SqlType a, SqlType b) {
        if (a == null || a == SqlType.UNKNOWN) return b;
        if (b == null || b == SqlType.UNKNOWN) return a;
        if (a.equals(b)) return a;
        if (a.isInteger() && b.isInteger()) return a == SqlType.I64 || b == SqlType.I64 ? SqlType.I64 : a == SqlType.I32 || b == SqlType.I32 ? SqlType.I32 : SqlType.I16;
        if (a.kind() == b.kind() && !a.isComposite() && a.kind() != SqlType.Kind.ARRAY && a.kind() != SqlType.Kind.RANGE) return a;
        throw SqlException.typeMismatch("conflicting parameter types: " + a + " and " + b);
    }
    private SqlType type(SqlParser.TypeName name) { return environment.resolver().type(name); }
    private List<ScalarPlan.Binding> statement(SqlParser.Statement syntax, List<ScalarPlan.Binding> outer, List<Engine.Column> targets) {
        if (syntax instanceof SqlParser.Explain explain) return statement(explain.inner(), outer, targets);
        if (syntax instanceof SqlParser.With with) {
            Map<String,List<ScalarPlan.Binding>> saved = new HashMap<>(ctes);
            try { for (var cte : with.ctes()) ctes.put(cte.name(), alias(statement(cte.body(), outer, List.of()), cte.name(), cte.columns())); return statement(with.body(), outer, targets); }
            finally { ctes.clear(); ctes.putAll(saved); }
        }
        if (syntax instanceof SqlParser.CreateTable create) return create.asQuery() == null ? List.of() : statement(create.asQuery(), outer, List.of());
        if (syntax instanceof SqlParser.SetOperation set) { var left = statement(set.left(), outer, targets); statement(set.right(), outer, targets); return left; }
        if (syntax instanceof SqlParser.QueryTail tail) { var result = statement(tail.query(), outer, targets); expr(tail.limit(), outer, SqlType.I64); expr(tail.offset(), outer, SqlType.I64); for (var order : tail.order()) expr(order.expression(), result, null); return result; }
        if (syntax instanceof SqlParser.Select select) {
            var bindings = new ArrayList<ScalarPlan.Binding>(); for (var from : select.from()) bindings.addAll(from(from, concat(bindings, outer)));
            var scope = concat(bindings, outer); expr(select.where(), scope, SqlType.BOOLEAN); expr(select.having(), scope, SqlType.BOOLEAN);
            for (var group : select.group()) expr(group, scope, null);
            expr(select.limit(), scope, SqlType.I64); expr(select.offset(), scope, SqlType.I64);
            List<ScalarPlan.Binding> outputs = items(select.items(), scope, targets);
            for (var order : select.order()) expr(order.expression(), concat(outputs, scope), null);
            for (var distinct : select.distinctOn()) expr(distinct, scope, null);
            for (var window : select.windows()) window(window.spec(), scope);
            return outputs;
        }
        if (syntax instanceof SqlParser.Values values) {
            List<ScalarPlan.Binding> result = List.of(); for (var row : values.rows()) result = items(row.stream().map(value -> new SqlParser.Item(value, null)).toList(), outer, targets);
            expr(values.limit(), outer, SqlType.I64); expr(values.offset(), outer, SqlType.I64); return result;
        }
        if (syntax instanceof SqlParser.Insert insert) {
            List<Engine.Column> columns = columns(insert.table()); var bindings = bindings(columns, insert.alias() == null ? insert.table().parts().getLast() : insert.alias());
            List<Engine.Column> slots = slots(columns, insert.columns());
            for (var row : insert.values()) for (int i = 0; i < Math.min(row.size(), slots.size()); i++) assigned(row.get(i), slots.get(i), outer);
            if (insert.query() != null) statement(insert.query(), outer, slots);
            if (insert.conflict() != null) { var scope = concat(bindings, alias(bindings, "excluded", List.of())); expr(insert.conflict().predicate(), scope, SqlType.BOOLEAN); expr(insert.conflict().where(), scope, SqlType.BOOLEAN); for (var assignment : insert.conflict().assignments()) assigned(assignment.expression(), column(columns, assignment.column()), scope); }
            return items(insert.returning(), concat(bindings, outer), List.of());
        }
        if (syntax instanceof SqlParser.Update update) {
            var columns = columns(update.table()); var scope = concat(bindings(columns, update.alias() == null ? update.table().parts().getLast() : update.alias()), outer);
            for (var from : update.from()) scope = concat(scope, from(from, scope));
            for (var assignment : update.assignments()) assigned(assignment.expression(), column(columns, assignment.column()), scope);
            expr(update.where(), scope, SqlType.BOOLEAN); return items(update.returning(), scope, List.of());
        }
        if (syntax instanceof SqlParser.Delete delete) { var scope = concat(bindings(columns(delete.table()), delete.alias() == null ? delete.table().parts().getLast() : delete.alias()), outer); for (var from : delete.using()) scope = concat(scope, from(from, scope)); expr(delete.where(), scope, SqlType.BOOLEAN); return items(delete.returning(), scope, List.of()); }
        if (syntax instanceof SqlParser.Merge merge) {
            var columns = columns(merge.table()); var scope = concat(bindings(columns, merge.alias() == null ? merge.table().parts().getLast() : merge.alias()), from(merge.source(), outer));
            expr(merge.on(), scope, SqlType.BOOLEAN); expr(merge.targetWhere(), scope, SqlType.BOOLEAN);
            for (var clause : merge.clauses()) { expr(clause.condition(), scope, SqlType.BOOLEAN); for (var assignment : clause.assignments()) assigned(assignment.expression(), column(columns, assignment.column()), scope); var slots = slots(columns, clause.columns()); for (int i = 0; i < Math.min(slots.size(), clause.values().size()); i++) assigned(clause.values().get(i), slots.get(i), scope); }
            return items(merge.returning(), scope, List.of());
        }
        return List.of();
    }
    private List<ScalarPlan.Binding> from(SqlParser.From syntax, List<ScalarPlan.Binding> outer) {
        if (syntax instanceof SqlParser.Table table) { String name = table.name().parts().getLast(), alias = table.alias() == null ? name : table.alias(); if (table.name().parts().size() == 1 && ctes.containsKey(name)) return alias(ctes.get(name), alias, table.columns()); return alias(bindings(columns(table.name()), alias), alias, table.columns()); }
        if (syntax instanceof SqlParser.Derived derived) return alias(statement(derived.query(), outer, List.of()), derived.alias(), derived.columns());
        if (syntax instanceof SqlParser.Grouped grouped) return alias(from(grouped.source(), outer), grouped.alias(), grouped.columns());
        if (syntax instanceof SqlParser.Join join) { var left = from(join.left(), outer); var scope = concat(left, from(join.right(), concat(left, outer))); expr(join.on(), concat(scope, outer), SqlType.BOOLEAN); return scope; }
        if (syntax instanceof SqlParser.TableFunction function) {
            SqlType result = expr(function.call(), outer, null); String name = function.alias() == null ? function.call().name() : function.alias();
            if (function.call().name().equals("generate_series")) result = SqlType.I64;
            if (function.call().name().equals("unnest") && result.kind() == SqlType.Kind.ARRAY) result = result.element();
            return List.of(new ScalarPlan.Binding(List.of(name), function.columns().isEmpty() ? name : function.columns().getFirst(), result));
        }
        if (syntax instanceof SqlParser.JsonTable table) { expr(table.context(), outer, SqlType.JSONB); for (var passing : table.passing()) expr(passing.value(), outer, null); var result = new ArrayList<ScalarPlan.Binding>(); for (var column : table.columns()) if (column.type() != null) result.add(new ScalarPlan.Binding(List.of(table.alias()), column.name(), type(column.type()))); return result; }
        return List.of();
    }
    private List<Engine.Column> columns(SqlParser.Name name) {
        Engine.Snapshot domain = snapshot;
        if (name.parts().size() > 1) domain = snapshot.domain(name.parts().getFirst());
        String relation = name.parts().getLast();
        Engine.Table table = domain.tables().get(relation);
        if (table == null && name.parts().size() == 1 && snapshot.domains().containsKey("temp")) table = snapshot.domain("temp").tables().get(relation);
        if (table != null) return table.columns();
        Engine.View view = domain.views().get(relation);
        if (view != null) { var output = statement(view.query(), List.of(), List.of()); var result = new ArrayList<Engine.Column>(); for (int i = 0; i < output.size(); i++) result.add(new Engine.Column(view.columns().get(i).name(), output.get(i).type(), false, null)); return result; }
        throw new SqlException("42P01", "relation does not exist: " + relation);
    }
    private List<ScalarPlan.Binding> items(List<SqlParser.Item> items, List<ScalarPlan.Binding> scope, List<Engine.Column> targets) {
        var output = new ArrayList<ScalarPlan.Binding>();
        for (var item : items) {
            if (item.expression() instanceof SqlParser.Star star) { for (var binding : scope) if (star.qualifier().isEmpty() || star.qualifier().equals(binding.qualifier())) output.add(binding); continue; }
            SqlType hint = output.size() < targets.size() && DefaultExpression.unwrap(item.expression()) instanceof SqlParser.Parameter ? targets.get(output.size()).type() : null;
            if (hint != null) assigned(item.expression(), targets.get(output.size()), scope);
            SqlType type = expr(item.expression(), scope, hint); output.add(new ScalarPlan.Binding(List.of(), item.alias() == null ? ScalarPlan.outputName(item.expression()) : item.alias(), type));
        }
        return output;
    }
    private void assigned(SqlParser.Expr value, Engine.Column target, List<ScalarPlan.Binding> scope) {
        if (DefaultExpression.unwrap(value) instanceof SqlParser.Parameter parameter) constrain(parameter, target.type(), target.notNull());
        expr(value, scope, target.type());
    }
    private static Engine.Column column(List<Engine.Column> columns, String name) { return columns.stream().filter(column -> column.name().equals(name)).findFirst().orElseThrow(() -> new SqlException("42703", "unknown column: " + name)); }
    private static List<Engine.Column> slots(List<Engine.Column> columns, List<String> names) { return names.isEmpty() ? columns : names.stream().map(name -> column(columns, name)).toList(); }
    private static List<ScalarPlan.Binding> bindings(List<Engine.Column> columns, String alias) { return columns.stream().map(column -> new ScalarPlan.Binding(List.of(alias), column.name(), column.type())).toList(); }
    private static List<ScalarPlan.Binding> concat(List<ScalarPlan.Binding> a, List<ScalarPlan.Binding> b) { var result = new ArrayList<>(a); result.addAll(b); return result; }
    private static List<ScalarPlan.Binding> alias(List<ScalarPlan.Binding> values, String alias, List<String> names) { if (alias == null) return values; var result = new ArrayList<ScalarPlan.Binding>(); for (int i = 0; i < values.size(); i++) result.add(new ScalarPlan.Binding(List.of(alias), i < names.size() ? names.get(i) : values.get(i).name(), values.get(i).type())); return result; }
    private void window(SqlParser.WindowSpec spec, List<ScalarPlan.Binding> scope) { for (var key : spec.partition()) expr(key, scope, null); for (var key : spec.order()) expr(key.expression(), scope, null); if (spec.frame() != null) { expr(spec.frame().start().offset(), scope, SqlType.I64); if (spec.frame().end() != null) expr(spec.frame().end().offset(), scope, SqlType.I64); } }
    private SqlType expr(SqlParser.Expr syntax, List<ScalarPlan.Binding> scope, SqlType hint) {
        if (syntax == null) return SqlType.UNKNOWN;
        SqlParser.Expr expression = DefaultExpression.unwrap(syntax);
        if (expression instanceof SqlParser.Parameter parameter) return constrain(parameter, hint, false);
        if (expression instanceof SqlParser.Name name) {
            for (var binding : scope) if (binding.name().equals(name.parts().getLast()) && (name.parts().size() == 1 || name.parts().subList(0, name.parts().size() - 1).equals(binding.qualifier()))) return binding.type();
            return SqlType.UNKNOWN;
        }
        if (expression instanceof SqlParser.ColumnSlot slot) return scope.get(slot.index()).type();
        if (expression instanceof SqlParser.StoredValue value) return value.value().type();
        if (expression instanceof SqlParser.Literal literal) return switch (literal.kind()) { case INTEGER -> SqlType.I64; case DECIMAL -> SqlType.DECIMAL; case STRING -> hint == null ? SqlType.TEXT : hint; default -> literal.text().equals("null") ? SqlType.UNKNOWN : SqlType.BOOLEAN; };
        if (expression instanceof SqlParser.Cast cast) { SqlType target = type(cast.target()); expr(cast.value(), scope, target); return target; }
        if (expression instanceof SqlParser.Collate collate) return expr(collate.value(), scope, hint);
        if (expression instanceof SqlParser.Unary unary) {
            if (unary.op().equals("not")) { expr(unary.value(), scope, SqlType.BOOLEAN); return SqlType.BOOLEAN; }
            if (unary.op().contains("null") || unary.op().contains("true") || unary.op().contains("false") || unary.op().contains("unknown")) { expr(unary.value(), scope, null); return SqlType.BOOLEAN; }
            return expr(unary.value(), scope, hint);
        }
        if (expression instanceof SqlParser.Binary binary) {
            if (List.of("and", "or").contains(binary.op())) { expr(binary.left(), scope, SqlType.BOOLEAN); expr(binary.right(), scope, SqlType.BOOLEAN); return SqlType.BOOLEAN; }
            if (List.of("like", "not like", "ilike", "not ilike", "~", "~*", "!~", "!~*").contains(binary.op())) { expr(binary.left(), scope, SqlType.TEXT); expr(binary.right(), scope, SqlType.TEXT); return SqlType.BOOLEAN; }
            SqlType a = expr(binary.left(), scope, null), b = expr(binary.right(), scope, null);
            if (List.of("@>", "<@", "&&", "||", "=", "<>", "!=").contains(binary.op())) {
                if (a.kind() == SqlType.Kind.ARRAY && binary.right() instanceof SqlParser.ArrayConstructor) b = expr(binary.right(), scope, a);
                if (b.kind() == SqlType.Kind.ARRAY && binary.left() instanceof SqlParser.ArrayConstructor) a = expr(binary.left(), scope, b);
            }
            if (binary.op().equals("||") && a.kind() != SqlType.Kind.ARRAY && b.kind() != SqlType.Kind.ARRAY) {
                if (binary.left() instanceof SqlParser.Parameter) expr(binary.left(), scope, SqlType.TEXT);
                if (binary.right() instanceof SqlParser.Parameter) expr(binary.right(), scope, SqlType.TEXT);
                return SqlType.TEXT;
            }
            if (a == SqlType.UNKNOWN && b == SqlType.UNKNOWN || binary.left() instanceof SqlParser.Parameter && binary.right() instanceof SqlParser.Parameter) a = b = SqlType.I64;
            if (binary.left() instanceof SqlParser.Parameter) a = expr(binary.left(), scope, b == SqlType.UNKNOWN ? a : b);
            if (binary.right() instanceof SqlParser.Parameter) b = expr(binary.right(), scope, a == SqlType.UNKNOWN ? b : a);
            if (List.of("=", "<>", "!=", "<", ">", "<=", ">=", "@>", "<@", "&&", "is distinct from", "is not distinct from").contains(binary.op())) return SqlType.BOOLEAN;
            if (binary.op().equals("||")) return a == SqlType.UNKNOWN ? b : a;
            if (a == SqlType.UNKNOWN) return b; if (b == SqlType.UNKNOWN) return a;
            if (a.isInteger() && b.isInteger()) return merge(a,b);
            if (a == SqlType.DECIMAL || b == SqlType.DECIMAL) return SqlType.DECIMAL;
            return a;
        }
        if (expression instanceof SqlParser.In in) {
            SqlType type = expr(in.subject(), scope, null); if (in.query() != null) { var result = statement(in.query(), scope, List.of()); if (!result.isEmpty()) type = result.getFirst().type(); }
            for (var item : in.values()) { SqlType other = expr(item, scope, type); if (type == SqlType.UNKNOWN) type = other; }
            expr(in.subject(), scope, type); return SqlType.BOOLEAN;
        }
        if (expression instanceof SqlParser.Between between) {
            SqlType type = expr(between.subject(), scope, null); SqlType lower = expr(between.lower(), scope, type), upper = expr(between.upper(), scope, type);
            if (type == SqlType.UNKNOWN) { type = lower == SqlType.UNKNOWN ? upper : lower; expr(between.subject(), scope, type); } return SqlType.BOOLEAN;
        }
        if (expression instanceof SqlParser.Quantified quantified) {
            SqlType subject = expr(quantified.subject(), scope, null), array;
            if (quantified.query() != null) { var columns = statement(quantified.query(), scope, List.of()); array = columns.isEmpty() ? SqlType.UNKNOWN : SqlType.array(columns.getFirst().type()); }
            else array = expr(quantified.array(), scope, subject == SqlType.UNKNOWN ? null : SqlType.array(subject));
            if (array.kind() == SqlType.Kind.ARRAY) expr(quantified.subject(), scope, array.element()); return SqlType.BOOLEAN;
        }
        if (expression instanceof SqlParser.Subquery subquery) { var result = statement(subquery.query(), scope, List.of()); return result.isEmpty() ? SqlType.UNKNOWN : result.getFirst().type(); }
        if (expression instanceof SqlParser.Exists exists) { statement(exists.query(), scope, List.of()); return SqlType.BOOLEAN; }
        if (expression instanceof SqlParser.Conditional conditional) {
            SqlType input = expr(conditional.input(), scope, null);
            for (var test : conditional.conditions()) expr(test, scope, conditional.input() == null ? SqlType.BOOLEAN : input);
            var values = new ArrayList<>(conditional.results()); if (conditional.otherwise() != null) values.add(conditional.otherwise()); return unified(values, scope, hint);
        }
        if (expression instanceof SqlParser.Extrema extrema) return unified(extrema.args(), scope, hint);
        if (expression instanceof SqlParser.ArrayConstructor array) {
            if (array.query() != null) { var result = statement(array.query(), scope, List.of()); return result.isEmpty() ? SqlType.UNKNOWN : SqlType.array(result.getFirst().type()); }
            SqlType element = unified(array.values(), scope, hint != null && hint.kind() == SqlType.Kind.ARRAY ? hint.element() : null); return element == SqlType.UNKNOWN ? SqlType.UNKNOWN : SqlType.array(element);
        }
        if (expression instanceof SqlParser.RowConstructor row) {
            var fields = new ArrayList<SqlType.Field>(); for (int i = 0; i < row.values().size(); i++) fields.add(new SqlType.Field("f" + (i + 1), expr(row.values().get(i), scope, hint != null && hint.isComposite() && i < hint.fields().size() ? hint.fields().get(i).type() : null), false)); return SqlType.record(fields);
        }
        if (expression instanceof SqlParser.Window window) { window(window.spec(), scope); return expr(window.function(), scope, hint); }
        if (expression instanceof SqlParser.Call original) {
            SqlParser.Call call = NamedFunctions.normalize(original);
            if (call.name().equals("generate_series")) {
                SqlType input = null;
                for (SqlParser.Expr argument : call.args()) { SqlType candidate = expr(argument, scope, null); if (candidate != SqlType.UNKNOWN && (input == null || candidate.isInteger() && input.isInteger())) input = candidate; }
                if (input == null) input = SqlType.I64;
                for (SqlParser.Expr argument : call.args()) expr(argument, scope, input);
                return input;
            }
            if (List.of("coalesce", "nullif").contains(call.name())) return unified(call.args(), scope, hint);
            List<SqlType> arguments = new ArrayList<>(); for (var arg : call.args()) arguments.add(expr(arg instanceof SqlParser.NamedArgument named ? named.value() : arg, scope, null));
            List<ScalarPlan.Expression> probes = arguments.stream().map(type -> ScalarPlan.constant(SqlValue.nullValue(type))).toList();
            List<String> origins = new ArrayList<>(); for (int i = 0; i < arguments.size(); i++) origins.add(arguments.get(i) == SqlType.UNKNOWN ? "unknown" : call.args().get(i) instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.INTEGER ? "integer_literal" : "value");
            List<SqlType> hints = ScalarFunctions.parameterHints(call.name(), probes, origins);
            if (hints == null && origins.contains("unknown")) {
                var hosts = meter.environment().functions().stream().filter(function -> function.name().equals(call.name()) && function.arguments().size() == arguments.size()).toList();
                if (hosts.size() == 1) hints = hosts.getFirst().arguments();
                else if (hosts.size() > 1) throw new SqlException("42725", "host function parameter is ambiguous: " + call.name());
            }
            if (hints != null) for (int i = 0; i < hints.size(); i++) if (hints.get(i) != null) arguments.set(i, expr(call.args().get(i), scope, hints.get(i)));
            if (call.filter() != null) expr(call.filter(), scope, SqlType.BOOLEAN); for (var order : call.order()) expr(order.expression(), scope, null);
            if (call.name().equals("count") || call.name().equals("regr_count")) return SqlType.I64;
            if (call.name().equals("to_json") || call.name().equals("json_agg") || call.name().equals("json_build_array")) return SqlType.JSON;
            if (call.name().equals("to_jsonb") || call.name().equals("jsonb_agg") || call.name().equals("jsonb_build_array")) return SqlType.JSONB;
            for (var fact : FunctionCatalog.FACTS) if (fact.name().equals(call.name()) && fact.arity() == arguments.size()) {
                if (fact.result().equals("promoted")) return arguments.isEmpty() ? SqlType.UNKNOWN : arguments.getFirst();
                try { return SqlType.named(fact.result()); } catch (SqlException ignored) { }
            }
            return arguments.isEmpty() ? SqlType.UNKNOWN : arguments.getFirst();
        }
        for (var child : DefaultExpression.children(expression)) expr(child, scope, null);
        return hint == null ? SqlType.UNKNOWN : hint;
    }
    private SqlType unified(List<SqlParser.Expr> values, List<ScalarPlan.Binding> scope, SqlType hint) {
        SqlType common = hint;
        for (var value : values) {
            SqlType type = expr(value, scope, null); if (type == SqlType.UNKNOWN || DefaultExpression.unwrap(value) instanceof SqlParser.Parameter) continue;
            if (common == null || common == SqlType.UNKNOWN) common = type;
            else if (common.isInteger() && type.isInteger()) common = merge(common,type);
            else if (common.isNumeric() && type.isNumeric() && (common == SqlType.DECIMAL || type == SqlType.DECIMAL)) common = SqlType.DECIMAL;
        }
        for (var value : values) expr(value, scope, common);
        return common == null ? SqlType.UNKNOWN : common;
    }
}
