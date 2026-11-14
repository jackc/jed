package org.jed;

import java.util.ArrayList;
import java.util.List;

/** Literal-only WHERE proofs from planner section 3.1; never evaluates a SQL kernel. */
final class Contradictions {
    private Contradictions() {}
    private record Bound(SqlParser.Name column, String operator, SqlValue value) {}
    static boolean proven(SqlParser.Expr predicate, SqlValue[] arguments) {
        List<SqlParser.Expr> terms = new ArrayList<>(); terms(predicate, terms);
        List<Bound> bounds = new ArrayList<>();
        for (SqlParser.Expr term : terms) {
            SqlValue direct = literal(term, arguments);
            if (direct != null && (direct.isNull() || direct.type() == SqlType.BOOLEAN && !direct.asBoolean())) return true;
            if (!(term instanceof SqlParser.Binary comparison) || !List.of("=", "<", "<=", ">", ">=").contains(comparison.op())) continue;
            SqlValue left = literal(comparison.left(), arguments), right = literal(comparison.right(), arguments);
            if (left != null && right != null) {
                if (left.isNull() || right.isNull()) return true;
                Integer order = compare(left, right, comparison.op().equals("="));
                if (order != null && !accept(comparison.op(), order)) return true;
            }
            Bound bound = comparison.left() instanceof SqlParser.Name column && right != null ? new Bound(column, comparison.op(), right)
                : comparison.right() instanceof SqlParser.Name column && left != null ? new Bound(column, reverse(comparison.op()), left) : null;
            if (bound == null) continue;
            if (bound.value().isNull()) return true;
            for (Bound earlier : bounds) if (earlier.column().equals(bound.column()) && disjoint(earlier, bound)) return true;
            bounds.add(bound);
        }
        return false;
    }
    private static void terms(SqlParser.Expr value, List<SqlParser.Expr> terms) {
        if (value == null) return;
        if (value instanceof SqlParser.Binary binary && binary.op().equals("and")) { terms(binary.left(), terms); terms(binary.right(), terms); }
        else if (value instanceof SqlParser.Between between && !between.negated()) {
            terms.add(new SqlParser.Binary(">=", between.subject(), between.lower()));
            terms.add(new SqlParser.Binary("<=", between.subject(), between.upper()));
        } else terms.add(value);
    }
    private static SqlValue literal(SqlParser.Expr syntax, SqlValue[] arguments) {
        if (syntax instanceof SqlParser.Parameter parameter) return parameter.index() <= arguments.length ? arguments[parameter.index() - 1] : null;
        SqlParser.Literal literal = DefaultExpression.literal(syntax);
        if (literal == null) return null;
        try {
            return switch (literal.kind()) {
                case STRING -> SqlValue.ofText(literal.text());
                case INTEGER -> SqlValue.ofInteger(ScalarPlan.integer(literal.text()));
                case DECIMAL -> SqlValue.ofDecimal(Decimal.parse(literal.text()));
                case WORD -> switch (literal.text()) { case "true" -> SqlValue.ofBoolean(true); case "false" -> SqlValue.ofBoolean(false); case "null" -> SqlValue.nullValue(SqlType.UNKNOWN); default -> null; };
                default -> null;
            };
        } catch (SqlException failure) { return null; }
    }
    private static boolean disjoint(Bound a, Bound b) {
        Integer order = compare(a.value(), b.value(), a.operator().equals("=") && b.operator().equals("="));
        if (order == null) return false;
        if (a.operator().equals("=")) return !accept(b.operator(), order);
        if (b.operator().equals("=")) return !accept(a.operator(), -order);
        if (a.operator().startsWith("<") && b.operator().startsWith(">")) return order < 0 || order == 0 && (a.operator().equals("<") || b.operator().equals(">"));
        if (a.operator().startsWith(">") && b.operator().startsWith("<")) return order > 0 || order == 0 && (a.operator().equals(">") || b.operator().equals("<"));
        return false;
    }
    private static Integer compare(SqlValue a, SqlValue b, boolean equality) {
        if (a.type().isInteger() && b.type().isInteger()) return Long.compare(a.asInteger(), b.asInteger());
        if (a.type().kind() != b.type().kind()) return null;
        return switch (a.type().kind()) {
            case BOOLEAN -> Boolean.compare(a.asBoolean(), b.asBoolean());
            case DECIMAL -> a.asDecimal().compareTo(b.asDecimal());
            case F32, F64 -> ScalarPlan.compare(a, b, null);
            case TEXT -> equality ? SqlText.compare(a.asText(), b.asText()) : null;
            default -> null;
        };
    }
    private static boolean accept(String op, int comparison) { return switch (op) { case "=" -> comparison == 0; case "<" -> comparison < 0; case "<=" -> comparison <= 0; case ">" -> comparison > 0; case ">=" -> comparison >= 0; default -> throw new AssertionError(); }; }
    private static String reverse(String op) { return switch (op) { case "<" -> ">"; case "<=" -> ">="; case ">" -> "<"; case ">=" -> "<="; default -> op; }; }
    private static PlanNode unvisited(PlanNode node) {
        return new PlanNode(node.label(), node.detail(), 0, 0, node.children().stream().map(Contradictions::unvisited).toList(), node.lane());
    }
    static RelationPlan empty(RelationPlan source) {
        return new RelationPlan() {
            public List<ScalarPlan.Binding> bindings() { return source.bindings(); }
            public List<FunctionalKey> functionalKeys() { return source.functionalKeys(); }
            public List<String> names() { return source.names(); }
            public List<SqlType> types() { return source.types(); }
            public void touch(boolean[] columns) { source.touch(columns); }
            public boolean orderedBy(List<SqlParser.Order> order) { return source.orderedBy(order); }
            public boolean push(SqlParser.Expr expression) { return false; }
            public PlanNode explain(boolean verbose) { return unvisited(source.explain(verbose)); }
            public void requireAnalyze() { source.requireAnalyze(); }
            public PlanNode actualPlan(boolean verbose) { return unvisited(source.explain(verbose)); }
            public Row next(CostMeter meter) { return null; }
            public void close() { source.close(); }
        };
    }
}
