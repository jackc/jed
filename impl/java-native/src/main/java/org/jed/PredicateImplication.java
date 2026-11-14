package org.jed;

import java.util.List;

/** Bounded, conservative predicate implication (indexes.md §9.1). */
final class PredicateImplication {
    private int steps = 10_000;
    private final ScalarPlan.Resolver constants;
    private final CostMeter meter;
    private PredicateImplication(QueryEnvironment environment) { constants = environment.resolver(); meter = environment.meter(); }
    private PredicateImplication(ScalarPlan.Resolver constants, CostMeter meter) { this.constants = constants; this.meter = meter; }
    static boolean normalized(SqlParser.Expr actual, SqlParser.Expr required, ScalarPlan.Resolver constants, CostMeter meter) {
        return new PredicateImplication(constants, meter).conj(FilterRules.terms(expand(actual)), expand(required));
    }
    private static SqlParser.Expr expand(SqlParser.Expr expression) {
        return WindowSyntax.map(expression, node -> {
            if (node instanceof SqlParser.Between between && !between.negated()) return new SqlParser.Binary("and", new SqlParser.Binary(">=", between.subject(), between.lower()), new SqlParser.Binary("<=", between.subject(), between.upper()));
            if (node instanceof SqlParser.Quantified q && q.query() == null && q.operator().equals("=") && !q.quantifier().equals("all") && q.array() instanceof SqlParser.ArrayConstructor array) {
                SqlParser.Expr result = null;
                for (SqlParser.Expr value : array.values()) {
                    if (value instanceof SqlParser.Literal literal && literal.text().equals("null")) continue;
                    SqlParser.Expr equal = new SqlParser.Binary("=", q.subject(), value); result = result == null ? equal : new SqlParser.Binary("or", result, equal);
                }
                if (result != null) return result;
            }
            if (node instanceof SqlParser.In in && in.query() == null && !in.negated()) {
                SqlParser.Expr result = null;
                for (SqlParser.Expr value : in.values()) { SqlParser.Expr equal = new SqlParser.Binary("=", in.subject(), value); result = result == null ? equal : new SqlParser.Binary("or", result, equal); }
                if (result != null) return result;
            }
            return node;
        });
    }
    static boolean implies(SqlParser.Expr actual, SqlParser.Expr required, Engine.Table table, String alias, QueryEnvironment environment) {
        PredicateImplication prover = new PredicateImplication(environment);
        return prover.conj(FilterRules.terms(normalize(actual, table, alias)), normalize(required, table, table.name()));
    }
    private static SqlParser.Expr normalize(SqlParser.Expr expression, Engine.Table table, String alias) {
        if (expression == null) return null;
        return WindowSyntax.map(DefaultExpression.unwrap(expression), node -> {
            if (node instanceof SqlParser.Name name && (name.parts().size() == 1 || name.parts().size() == 2 && name.parts().getFirst().equals(alias == null ? table.name() : alias))) {
                for (int i = 0; i < table.columns().size(); i++) if (table.columns().get(i).name().equals(name.parts().getLast())) return new SqlParser.ColumnSlot(i);
            }
            if (node instanceof SqlParser.In in && in.query() == null && !in.negated()) {
                SqlParser.Expr result = null;
                for (SqlParser.Expr value : in.values()) { SqlParser.Expr comparison = new SqlParser.Binary("=", in.subject(), value); result = result == null ? comparison : new SqlParser.Binary("or", result, comparison); }
                if (result != null) return result;
            }
            if (node instanceof SqlParser.Between between && !between.negated()) return new SqlParser.Binary("and", new SqlParser.Binary(">=", between.subject(), between.lower()), new SqlParser.Binary("<=", between.subject(), between.upper()));
            return node;
        });
    }
    private boolean step() { return steps-- > 0; }
    private boolean conj(List<SqlParser.Expr> premises, SqlParser.Expr required) {
        if (!step() || required == null) return false;
        if (required instanceof SqlParser.Binary b && b.op().equals("and")) return conj(premises, b.left()) && conj(premises, b.right());
        for (SqlParser.Expr premise : premises) if (clause(premise, required)) return true;
        return required instanceof SqlParser.Binary b && b.op().equals("or") && (conj(premises, b.left()) || conj(premises, b.right()));
    }
    private boolean clause(SqlParser.Expr premise, SqlParser.Expr required) {
        if (!step()) return false;
        if (premise.equals(required)) return true;
        if (required instanceof SqlParser.Binary b && b.op().equals("and")) return clause(premise, b.left()) && clause(premise, b.right());
        if (premise instanceof SqlParser.Binary b && b.op().equals("or")) return clause(b.left(), required) && clause(b.right(), required);
        if (premise instanceof SqlParser.Binary b && b.op().equals("and")) return clause(b.left(), required) || clause(b.right(), required);
        if (required instanceof SqlParser.Binary b && b.op().equals("or") && (clause(premise, b.left()) || clause(premise, b.right()))) return true;
        Atom atom = comparison(premise); return atom != null && atom(atom, required);
    }
    private record Atom(SqlParser.Expr expression, String operator, SqlValue constant) {}
    private Atom comparison(SqlParser.Expr expression) {
        if (!(expression instanceof SqlParser.Binary b) || !List.of("=", "<>", "!=", "<", "<=", ">", ">=").contains(b.op())) return null;
        SqlValue right = constant(b.right()); if (right != null) return new Atom(b.left(), b.op().equals("!=") ? "<>" : b.op(), right);
        SqlValue left = constant(b.left()); if (left == null) return null;
        return new Atom(b.right(), switch (b.op()) { case "<" -> ">"; case ">" -> "<"; case "<=" -> ">="; case ">=" -> "<="; case "!=" -> "<>"; default -> b.op(); }, left);
    }
    private SqlValue constant(SqlParser.Expr expression) {
        if (!(expression instanceof SqlParser.Literal) && !(expression instanceof SqlParser.StoredValue)) return null;
        try {
            SqlValue value = meter.unmetered(() -> constants.resolve(expression, null).run(meter));
            if (value.isNull()) return null;
            return switch (value.type().kind()) {
                case I16, I32, I64, DECIMAL, TEXT, DATE, TIMESTAMP, TIMESTAMPTZ, BOOLEAN, UUID -> value;
                default -> null;
            };
        } catch (SqlException failure) { return null; }
    }
    private boolean atom(Atom premise, SqlParser.Expr required) {
        if (!step()) return false;
        if (required instanceof SqlParser.Binary b && b.op().equals("and")) return atom(premise, b.left()) && atom(premise, b.right());
        if (required instanceof SqlParser.Binary b && b.op().equals("or")) return atom(premise, b.left()) || atom(premise, b.right());
        if (required instanceof SqlParser.Unary u && u.op().equals("is not null") && premise.expression().equals(u.value())) return true;
        Atom conclusion = comparison(required);
        if (conclusion == null || !premise.expression().equals(conclusion.expression())) return false;
        SqlValue a = premise.constant(), b = conclusion.constant();
        int order;
        if (a.type().isInteger() && b.type().isInteger()) order = Long.compare(a.asInteger(), b.asInteger());
        else if (a.type().kind() != b.type().kind()) return false;
        else order = java.util.Arrays.compareUnsigned(Encoding.key(a), Encoding.key(b));
        String op = conclusion.operator();
        return switch (premise.operator()) {
            case "=" -> switch (op) { case "=" -> order == 0; case "<>" -> order != 0; case "<" -> order < 0; case "<=" -> order <= 0; case ">" -> order > 0; case ">=" -> order >= 0; default -> false; };
            case "<" -> List.of("<", "<=", "<>").contains(op) && order <= 0;
            case "<=" -> List.of("<", "<>").contains(op) && order < 0 || op.equals("<=") && order <= 0;
            case ">" -> List.of(">", ">=", "<>").contains(op) && order >= 0;
            case ">=" -> List.of(">", "<>").contains(op) && order > 0 || op.equals(">=") && order >= 0;
            case "<>" -> op.equals("<>") && order == 0;
            default -> false;
        };
    }
}
