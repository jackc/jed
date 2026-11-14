package org.jed;

import java.util.ArrayList;
import java.util.List;

/** Only an unconsumed plain SELECT literal remains adaptable at the storage boundary. */
final class AssignmentSources {
    private AssignmentSources() {}
    static SqlParser.Literal stringLiteral(SqlParser.Statement query, int column) {
        List<SqlParser.Order> order = new ArrayList<>();
        while (true) {
            if (query instanceof SqlParser.With with) query = with.body();
            else if (query instanceof SqlParser.QueryTail tail) { order.addAll(tail.order()); query = tail.query(); }
            else break;
        }
        if (!(query instanceof SqlParser.Select select) || select.distinct() || !select.distinctOn().isEmpty() || column >= select.items().size()) return null;
        if (select.items().stream().anyMatch(item -> item.expression() instanceof SqlParser.Star)) return null;
        SqlParser.Item item = select.items().get(column);
        if (!(item.expression() instanceof SqlParser.Literal literal) || literal.kind() != Lexer.Kind.STRING) return null;
        order.addAll(select.order());
        for (SqlParser.Order key : order) if (references(key.expression(), item, column)) return null;
        for (SqlParser.Expr key : select.group()) if (references(key, item, column)) return null;
        return literal;
    }
    private static boolean references(SqlParser.Expr expression, SqlParser.Item item, int column) {
        if (expression instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.INTEGER) return ScalarPlan.integer(literal.text()) == column + 1L;
        if (expression instanceof SqlParser.Name name && name.parts().size() == 1 && item.alias() != null) return name.parts().getFirst().equals(item.alias());
        return expression.equals(item.expression());
    }
    static boolean stringNative(SqlType type) {
        return switch (type.kind()) { case UUID, BYTEA, DATE, TIMESTAMP, TIMESTAMPTZ, INTERVAL -> true; default -> false; };
    }
}
