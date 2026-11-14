package org.jed;

import java.util.ArrayList;
import java.util.List;

/** Structural default restrictions precede all name and type resolution. */
final class DefaultExpression {
    private DefaultExpression() {}

    static SqlParser.Expr unwrap(SqlParser.Expr expression) {
        return expression instanceof SqlParser.StoredExpression stored ? stored.expression() : expression;
    }

    static void validate(SqlParser.Expr expression) {
        validate(expression, false);
    }

    static void validateCheck(SqlParser.Expr expression) { validate(expression, true); }

    private static void validate(SqlParser.Expr expression, boolean check) {
        expression = unwrap(expression);
        if (expression instanceof SqlParser.Window) throw new SqlException("42P20", "window functions are not allowed in stored constraints");
        if (!check && expression instanceof SqlParser.Name) throw new SqlException("0A000", "cannot use column reference in DEFAULT expression");
        if (expression instanceof SqlParser.Parameter parameter) throw new SqlException("42P02", "there is no parameter $" + parameter.index());
        if (expression instanceof SqlParser.Subquery || expression instanceof SqlParser.Exists
                || expression instanceof SqlParser.In x && x.query() != null
                || expression instanceof SqlParser.Quantified x && x.query() != null
                || expression instanceof SqlParser.ArrayConstructor x && x.query() != null)
            throw new SqlException("0A000", "cannot use subquery in DEFAULT expression");
        if (expression instanceof SqlParser.Call call && AggregatePlan.aggregateName(call.name()))
            throw new SqlException("42803", "aggregate functions are not allowed in DEFAULT expressions");
        for (SqlParser.Expr child : children(expression)) if (child != null) validate(child, check);
    }

    static SqlParser.Literal literal(SqlParser.Expr expression) {
        expression = unwrap(expression);
        if (expression instanceof SqlParser.Literal literal) return literal;
        if (expression instanceof SqlParser.Unary unary && unary.op().equals("-")
                && unary.value() instanceof SqlParser.Literal literal
                && (literal.kind() == Lexer.Kind.INTEGER || literal.kind() == Lexer.Kind.DECIMAL))
            return new SqlParser.Literal(literal.kind(), "-" + literal.text());
        return null;
    }

    static List<SqlParser.Expr> children(SqlParser.Expr expression) {
        List<SqlParser.Expr> result = new ArrayList<>();
        switch (expression) {
            case SqlParser.Unary x -> result.add(x.value());
            case SqlParser.Binary x -> { result.add(x.left()); result.add(x.right()); }
            case SqlParser.Cast x -> result.add(x.value());
            case SqlParser.Collate x -> result.add(x.value());
            case SqlParser.NamedArgument x -> result.add(x.value());
            case SqlParser.JsonSql x -> result.addAll(x.children());
            case SqlParser.Extrema x -> result.addAll(x.args());
            case SqlParser.Call x -> { result.addAll(x.args()); for (SqlParser.Order order : x.order()) result.add(order.expression()); result.add(x.filter()); }
            case SqlParser.Conditional x -> { result.add(x.input()); result.addAll(x.conditions()); result.addAll(x.results()); result.add(x.otherwise()); }
            case SqlParser.In x -> { result.add(x.subject()); result.addAll(x.values()); }
            case SqlParser.Between x -> { result.add(x.subject()); result.add(x.lower()); result.add(x.upper()); }
            case SqlParser.Quantified x -> { result.add(x.subject()); result.add(x.array()); }
            case SqlParser.ArrayConstructor x -> result.addAll(x.values());
            case SqlParser.RowConstructor x -> result.addAll(x.values());
            case SqlParser.Access x -> { result.add(x.base()); for (SqlParser.Subscript sub : x.subscripts()) { result.add(sub.lower()); result.add(sub.upper()); } }
            case SqlParser.Field x -> result.add(x.base());
            default -> { }
        }
        return result;
    }
}
