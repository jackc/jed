package org.jed;

import java.util.ArrayList;
import java.util.List;

/** DISTINCT ON shares the effective sort keys and keeps only the previous key tuple. */
final class DistinctOn {
    private final int prefix;
    private List<SqlValue> previous;
    final List<SqlParser.Order> order;
    final List<SqlParser.Expr> keys;
    DistinctOn(List<SqlParser.Expr> on, List<SqlParser.Order> order, List<SqlParser.Expr> projected,
               List<String> names, List<ScalarPlan.Binding> bindings, ScalarPlan.Resolver resolver) {
        List<SqlParser.Expr> keys = new ArrayList<>();
        List<SqlParser.Order> effective = new ArrayList<>();
        for (SqlParser.Order term : order) {
            SqlParser.Expr key = resolve(term.expression(), projected, names);
            if (resolver.resolve(key, null).type() == SqlType.JSON) throw new SqlException("42883", "json has no ordering operator");
            effective.add(new SqlParser.Order(key, term.descending(), term.nullsFirst()));
        }
        for (SqlParser.Expr raw : on) {
            SqlParser.Expr key = resolve(raw, projected, names); resolver.resolve(key, null);
            if (resolver.resolve(key, null).type() == SqlType.JSON) throw new SqlException("42883", "json has no equality operator");
            key = canonical(key, bindings); if (!keys.contains(key)) keys.add(key);
        }
        boolean nonMatching = false; int prefix = 0;
        List<SqlParser.Expr> matched = new ArrayList<>();
        for (SqlParser.Order term : effective) {
            SqlParser.Expr key = canonical(term.expression(), bindings);
            if (keys.contains(key)) { if (nonMatching) throw mismatch(); prefix++; matched.add(key); }
            else nonMatching = true;
        }
        for (int i = 0; i < on.size(); i++) {
            SqlParser.Expr expression = resolve(on.get(i), projected, names), key = canonical(expression, bindings);
            if (!matched.contains(key)) { if (nonMatching) throw mismatch(); effective.add(new SqlParser.Order(expression, false, null)); matched.add(key); prefix++; }
        }
        this.prefix = prefix; this.order = List.copyOf(effective); this.keys = List.copyOf(keys);
    }
    static SqlParser.Expr resolve(SqlParser.Expr expression, List<SqlParser.Expr> projected, List<String> names) {
        if (expression instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.INTEGER) {
            int index; try { index = ScalarPlan.ordinal(literal.text()); } catch (NumberFormatException error) { throw new SqlException("42P10", "sort key position out of range"); }
            if (index < 1 || index > projected.size()) throw new SqlException("42P10", "sort key position out of range"); return projected.get(index - 1);
        }
        if (expression instanceof SqlParser.Name name && name.parts().size() == 1 && names.contains(name.parts().getFirst())) {
            int first = names.indexOf(name.parts().getFirst());
            for (int i = first + 1; i < names.size(); i++) if (names.get(i).equals(name.parts().getFirst()) && !projected.get(i).equals(projected.get(first))) throw new SqlException("42702", "ambiguous sort key name");
            return projected.get(first);
        }
        return expression;
    }
    private static SqlParser.Expr canonical(SqlParser.Expr expression, List<ScalarPlan.Binding> bindings) {
        return WindowSyntax.map(expression, value -> {
            if (!(value instanceof SqlParser.Name name)) return value;
            for (int i = 0; i < bindings.size(); i++) { var binding = bindings.get(i); if (binding.name().equals(name.parts().getLast()) && (name.parts().size() == 1 && !binding.qualifiedOnly() || name.parts().size() > 1 && binding.qualifier().equals(name.parts().subList(0, name.parts().size() - 1)))) return new SqlParser.ColumnSlot(i); }
            return value;
        });
    }
    boolean accept(List<SqlValue> row, int firstKey) {
        List<SqlValue> key = row.subList(firstKey, firstKey + prefix);
        boolean equal = previous != null;
        for (int i = 0; equal && i < key.size(); i++) { SqlValue a = key.get(i), b = previous.get(i); equal = a.isNull() || b.isNull() ? a.isNull() == b.isNull() : ScalarPlan.compare(a, b, null) == 0; }
        if (equal) return false; previous = List.copyOf(key); return true;
    }
    private static SqlException mismatch() { return new SqlException("42P10", "SELECT DISTINCT ON expressions must match initial ORDER BY expressions"); }
}
