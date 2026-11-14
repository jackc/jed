package org.jed;

import java.util.List;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.function.Function;

/** Window discovery and expression rewriting stops at nested query boundaries. */
final class WindowSyntax {
    private WindowSyntax() {}
    static boolean contains(SqlParser.Expr expression) {
        boolean[] found = {false};
        map(expression, value -> { if (value instanceof SqlParser.Window) found[0] = true; return value; });
        return found[0];
    }
    static boolean needed(SqlParser.Select query) {
        return !query.windows().isEmpty() || query.items().stream().anyMatch(item -> contains(item.expression())) || query.order().stream().anyMatch(item -> contains(item.expression()))
            || query.distinctOn().stream().anyMatch(WindowSyntax::contains) || contains(query.where()) || contains(query.having()) || query.group().stream().anyMatch(WindowSyntax::contains);
    }
    static SqlParser.Expr map(SqlParser.Expr expression, Function<SqlParser.Expr, SqlParser.Expr> visitor) {
        if (expression == null) return null;
        SqlParser.Expr replaced = visitor.apply(expression);
        if (replaced != expression) return replaced;
        if (expression instanceof SqlParser.Window x) return x;
        if (expression instanceof SqlParser.Unary x) return new SqlParser.Unary(x.op(), map(x.value(), visitor));
        if (expression instanceof SqlParser.Binary x) return new SqlParser.Binary(x.op(), map(x.left(), visitor), map(x.right(), visitor));
        if (expression instanceof SqlParser.Cast x) return new SqlParser.Cast(map(x.value(), visitor), x.target());
        if (expression instanceof SqlParser.Collate x) return new SqlParser.Collate(map(x.value(), visitor), x.name());
        if (expression instanceof SqlParser.JsonSql x) return x.map(arg -> map(arg, visitor));
        if (expression instanceof SqlParser.Extrema x) return x.map(arg -> map(arg, visitor));
        if (expression instanceof SqlParser.Call x) return new SqlParser.Call(x.name(), x.args().stream().map(arg -> map(arg, visitor)).toList(), x.distinct(), x.star(), orders(x.order(), visitor), map(x.filter(), visitor), x.withinGroup());
        if (expression instanceof SqlParser.In x) return new SqlParser.In(map(x.subject(), visitor), x.values().stream().map(arg -> map(arg, visitor)).toList(), x.query(), x.negated());
        if (expression instanceof SqlParser.Between x) return new SqlParser.Between(map(x.subject(), visitor), map(x.lower(), visitor), map(x.upper(), visitor), x.negated());
        if (expression instanceof SqlParser.Conditional x) return new SqlParser.Conditional(map(x.input(), visitor), x.conditions().stream().map(arg -> map(arg, visitor)).toList(), x.results().stream().map(arg -> map(arg, visitor)).toList(), map(x.otherwise(), visitor));
        if (expression instanceof SqlParser.RowConstructor x) return new SqlParser.RowConstructor(x.values().stream().map(arg -> map(arg, visitor)).toList());
        if (expression instanceof SqlParser.ArrayConstructor x) return new SqlParser.ArrayConstructor(x.values().stream().map(arg -> map(arg, visitor)).toList(), x.query());
        if (expression instanceof SqlParser.Field x) return new SqlParser.Field(map(x.base(), visitor), x.name());
        if (expression instanceof SqlParser.Access x) return new SqlParser.Access(map(x.base(), visitor), x.subscripts().stream().map(sub -> new SqlParser.Subscript(map(sub.lower(), visitor), map(sub.upper(), visitor), sub.slice())).toList());
        if (expression instanceof SqlParser.Quantified x) return new SqlParser.Quantified(x.operator(), map(x.subject(), visitor), x.quantifier(), map(x.array(), visitor), x.query());
        if (expression instanceof SqlParser.NamedArgument x) return new SqlParser.NamedArgument(x.name(), map(x.value(), visitor));
        return expression;
    }
    static List<SqlParser.Order> orders(List<SqlParser.Order> values, Function<SqlParser.Expr, SqlParser.Expr> visitor) {
        return values.stream().map(x -> new SqlParser.Order(map(x.expression(), visitor), x.descending(), x.nullsFirst())).toList();
    }
    static LinkedHashMap<String, SqlParser.WindowSpec> definitions(SqlParser.Select query) {
        var names = new LinkedHashMap<String, SqlParser.WindowSpec>();
        for (SqlParser.NamedWindow named : query.windows()) {
            if (names.containsKey(named.name())) throw new SqlException("42P20", "window name is specified more than once");
            names.put(named.name(), merge(named.spec(), names));
        }
        return names;
    }
    static SqlParser.WindowSpec merge(SqlParser.WindowSpec spec, java.util.Map<String, SqlParser.WindowSpec> names) {
        if (spec.base() == null) return spec;
        SqlParser.WindowSpec base = names.get(spec.base());
        if (base == null) throw new SqlException("42704", "window does not exist");
        if (!spec.partition().isEmpty()) throw new SqlException("42P20", "cannot override PARTITION BY of window");
        if (!spec.order().isEmpty() && !base.order().isEmpty()) throw new SqlException("42P20", "cannot override ORDER BY of window");
        if (base.frame() != null) throw new SqlException("42P20", "cannot copy window with a frame clause");
        return new SqlParser.WindowSpec(null, base.partition(), spec.order().isEmpty() ? base.order() : spec.order(), spec.frame());
    }
}
