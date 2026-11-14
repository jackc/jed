package org.jed;

import java.util.ArrayList;
import java.util.List;
import java.util.Set;

/** Non-trapping scan rewrites; unsupported expression forms stay in their original clause. */
final class FilterRules {
    private FilterRules() {}
    static List<SqlParser.Expr> terms(SqlParser.Expr expression) {
        List<SqlParser.Expr> terms = new ArrayList<>(); flatten(expression, terms); return terms;
    }
    private static void flatten(SqlParser.Expr expression, List<SqlParser.Expr> terms) {
        if (expression instanceof SqlParser.Binary b && b.op().equals("and")) { flatten(b.left(), terms); flatten(b.right(), terms); }
        else if (expression != null) terms.add(expression);
    }
    static SqlParser.Expr and(SqlParser.Expr a, SqlParser.Expr b) { return a == null ? b : b == null ? a : new SqlParser.Binary("and", a, b); }
    static boolean safe(SqlParser.Expr expression) { return safe(expression, null); }
    static boolean safe(SqlParser.Expr expression, ScalarPlan.Resolver resolver) {
        if (expression instanceof SqlParser.Name || expression instanceof SqlParser.ColumnSlot || expression instanceof SqlParser.Literal || expression instanceof SqlParser.Parameter || expression instanceof SqlParser.StoredValue) return true;
        if (expression instanceof SqlParser.Unary u) return Set.of("not", "is null", "is not null", "is unknown", "is not unknown", "is true", "is false", "is not true", "is not false").contains(u.op()) && safe(u.value(), resolver);
        if (expression instanceof SqlParser.Binary b) {
            if (Set.of("like", "not like", "ilike", "not ilike").contains(b.op())) return safe(b.left(), resolver) && provenPattern(b.right(), resolver);
            if (!Set.of("and", "or", "=", "<>", "!=", "<", ">", "<=", ">=", "is distinct from", "is not distinct from").contains(b.op()) || !safe(b.left(), resolver) || !safe(b.right(), resolver)) return false;
            if (resolver != null && !Set.of("and", "or").contains(b.op())) {
                ScalarPlan.Expression a = resolver.resolve(b.left(), null), c = resolver.resolve(b.right(), null);
                if (Set.of(SqlType.Kind.ARRAY, SqlType.Kind.RANGE, SqlType.Kind.JSON, SqlType.Kind.JSONB, SqlType.Kind.JSONPATH).contains(a.type().kind()) || Set.of(SqlType.Kind.ARRAY, SqlType.Kind.RANGE, SqlType.Kind.JSON, SqlType.Kind.JSONB, SqlType.Kind.JSONPATH).contains(c.type().kind())) return false;
                if (Set.of("<", ">", "<=", ">=").contains(b.op()) && !CollationDerivation.comparison(a.collation(), c.collation()).require().equals("C")) return false;
            }
            return true;
        }
        if (expression instanceof SqlParser.In in) return in.query() == null && safe(in.subject(), resolver) && in.values().stream().allMatch(value -> safe(value, resolver));
        if (expression instanceof SqlParser.Between between) return safe(between.subject(), resolver) && safe(between.lower(), resolver) && safe(between.upper(), resolver);
        if (expression instanceof SqlParser.Cast cast) {
            if (resolver == null || !safe(cast.value(), resolver)) return false;
            SqlType from = resolver.resolve(cast.value(), null).type(), to = resolver.type(cast.target());
            if (to.kind() == SqlType.Kind.DECIMAL && to.precision() >= 0) return false;
            String source = from.kind().name().toLowerCase(java.util.Locale.ROOT), target = to.kind().name().toLowerCase(java.util.Locale.ROOT);
            return CastCatalog.FACTS.stream().anyMatch(fact -> fact.from().equals(source) && fact.to().equals(target) && !fact.traps());
        }
        if (expression instanceof SqlParser.Call call) return !call.distinct() && !call.star() && call.filter() == null && call.order().isEmpty()
            && FunctionCatalog.FACTS.stream().anyMatch(fact -> fact.name().equals(call.name()) && !fact.traps()) && call.args().stream().allMatch(value -> safe(value, resolver));
        return false;
    }
    private static boolean provenPattern(SqlParser.Expr expression, ScalarPlan.Resolver resolver) {
        SqlValue pattern;
        if (expression instanceof SqlParser.Parameter parameter) pattern = resolver == null ? null : resolver.planParameter(parameter.index());
        else if (expression instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.STRING) pattern = SqlValue.ofText(literal.text());
        else if (expression instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.WORD && literal.text().equals("null")) pattern = SqlValue.nullValue(SqlType.TEXT);
        else return false;
        if (pattern == null) return false;
        if (pattern.isNull()) return true;
        if (pattern.type().kind() != SqlType.Kind.TEXT) return false;
        String text = pattern.asText();
        for (int i = 0; i < text.length(); i++) if (text.charAt(i) == '\\' && ++i == text.length()) return false;
        return true;
    }
    static boolean preservesEquality(SqlParser.Expr expression, ScalarPlan.Resolver resolver) {
        if (expression == null) return true;
        if (expression instanceof SqlParser.Cast cast) {
            String source = resolver.resolve(cast.value(), null).type().kind().name().toLowerCase(java.util.Locale.ROOT);
            String target = resolver.type(cast.target()).kind().name().toLowerCase(java.util.Locale.ROOT);
            if (!CastCatalog.FACTS.stream().anyMatch(fact -> fact.from().equals(source) && fact.to().equals(target) && fact.preservesEquality())) return false;
        }
        if (expression instanceof SqlParser.Call call && !FunctionCatalog.FACTS.stream().anyMatch(fact -> fact.name().equals(call.name()) && fact.preservesEquality())) return false;
        for (SqlParser.Expr child : DefaultExpression.children(expression)) if (!preservesEquality(child, resolver)) return false;
        return true;
    }
    static boolean nullRejecting(SqlParser.Expr expression) {
        if (expression instanceof SqlParser.Binary binary) {
            if (binary.op().equals("and")) return nullRejecting(binary.left()) || nullRejecting(binary.right());
            if (binary.op().equals("or")) return nullRejecting(binary.left()) && nullRejecting(binary.right());
            return Set.of("=", "<>", "!=", "<", "<=", ">", ">=", "like", "ilike", "not like", "not ilike").contains(binary.op())
                && (binary.left() instanceof SqlParser.Name || binary.right() instanceof SqlParser.Name);
        }
        if (expression instanceof SqlParser.Unary unary) return unary.op().equals("is not null") && unary.value() instanceof SqlParser.Name;
        if (expression instanceof SqlParser.In in) return in.subject() instanceof SqlParser.Name;
        if (expression instanceof SqlParser.Between between) return between.subject() instanceof SqlParser.Name;
        if (expression instanceof SqlParser.Quantified quantified) return quantified.quantifier().equals("any") && quantified.subject() instanceof SqlParser.Name;
        return false;
    }
    static boolean accessHint(SqlParser.Expr expression) {
        if (expression instanceof SqlParser.Cast) return false;
        for (SqlParser.Expr child : DefaultExpression.children(expression)) if (child != null && !accessHint(child)) return false;
        return true;
    }
    static boolean belongs(SqlParser.Expr expression, List<ScalarPlan.Binding> bindings) {
        List<SqlParser.Name> names = new ArrayList<>(); names(expression, names);
        if (names.isEmpty()) return false;
        for (SqlParser.Name name : names) {
            int count = 0;
            for (ScalarPlan.Binding binding : bindings) {
                if (!name.parts().getLast().equals(binding.name())) continue;
                if (name.parts().size() == 1 ? binding.qualifiedOnly() : !name.parts().subList(0, name.parts().size() - 1).equals(binding.qualifier())) continue;
                count++;
            }
            if (count != 1) return false;
        }
        return true;
    }
    static boolean correlated(SqlParser.Expr expression, QueryEnvironment environment) {
        if (expression instanceof SqlParser.Subquery || expression instanceof SqlParser.Exists
                || expression instanceof SqlParser.In in && in.query() != null
                || expression instanceof SqlParser.Quantified quantified && quantified.query() != null)
            return environment == null || !environment.hasInit(expression);
        for (SqlParser.Expr child : DefaultExpression.children(expression)) if (child != null && correlated(child, environment)) return true;
        return false;
    }
    static long nodes(SqlParser.Expr expression, QueryEnvironment environment) {
        if (expression == null || expression instanceof SqlParser.Literal || expression instanceof SqlParser.Name || expression instanceof SqlParser.ColumnSlot
                || expression instanceof SqlParser.Parameter || expression instanceof SqlParser.StoredValue || expression instanceof SqlParser.Star) return 0;
        if (expression instanceof SqlParser.StoredExpression stored) return nodes(stored.expression(), environment);
        if (expression instanceof SqlParser.Subquery || expression instanceof SqlParser.Exists) return environment != null && environment.hasInit(expression) ? 0 : 1;
        if (expression instanceof SqlParser.In in) {
            if (in.query() != null) {
                RowSpool folded = environment == null ? null : environment.foldedMembership(expression);
                return 1 + nodes(in.subject(), environment) + (folded == null ? 0 : folded.size());
            }
            long cost = Math.max(0, in.values().size() - 1L) + in.values().size() * (1 + nodes(in.subject(), environment)) + (in.negated() ? 1 : 0);
            for (SqlParser.Expr value : in.values()) cost += nodes(value, environment);
            return cost;
        }
        if (expression instanceof SqlParser.Between between) return 3 + (between.negated() ? 1 : 0) + 2 * nodes(between.subject(), environment) + nodes(between.lower(), environment) + nodes(between.upper(), environment);
        long count = 1;
        for (SqlParser.Expr child : DefaultExpression.children(expression)) count += nodes(child, environment);
        return count;
    }
    private static void names(SqlParser.Expr expression, List<SqlParser.Name> names) {
        if (expression instanceof SqlParser.Name name) names.add(name);
        else if (expression != null) for (SqlParser.Expr child : DefaultExpression.children(expression)) if (child != null) names(child, names);
    }
}
