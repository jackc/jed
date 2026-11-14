package org.jed;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

/** Canonical disjoint intervals for a single primary-key column. Bounds are half open. */
final class KeyIntervals {
    record Interval(byte[] lower, byte[] upper, byte[] point) {}
    record Tuple(List<Interval> intervals, SqlParser.Expr predicate) {}
    static Tuple tuple(SqlParser.Expr expression, Engine.Table table, List<String> names, String alias, ScalarPlan.Resolver resolver, CostMeter meter, QueryEnvironment environment) {
        List<byte[]> prefixes = new ArrayList<>(); prefixes.add(new byte[0]);
        List<Interval> result = null; SqlParser.Expr access = null; boolean hasSet = false;
        KeyIntervals last = null;
        for (int ordinal : table.primaryKey()) {
            SqlParser.Expr previousAccess = access; boolean previousSet = hasSet;
            Engine.Column definition = table.columns().get(ordinal);
            KeyIntervals planner = new KeyIntervals(names.get(ordinal), alias, definition.type(), resolver, meter, environment, definition.collation());
            planner.preservePoints = true; last = planner;
            List<Interval> member = null;
            for (SqlParser.Expr term : FilterRules.terms(expression)) {
                List<Interval> part = planner.reduce(term);
                if (part == null) continue;
                member = member == null ? part : planner.intersect(member, part);
                access = FilterRules.and(access, term); hasSet |= setSyntax(term);
            }
            if (member != null && member.isEmpty()) return hasSet ? new Tuple(List.of(), access) : null;
            if (member == null || member.stream().anyMatch(interval -> interval.point() == null)) {
                result = new ArrayList<>();
                if (member == null) member = List.of(new Interval(null, null, null));
                for (byte[] prefix : prefixes) for (Interval interval : member) {
                    byte[] lower = interval.lower() == null ? prefix : concat(prefix, interval.lower());
                    byte[] upper = interval.upper() == null ? successor(prefix) : concat(prefix, interval.upper());
                    result.add(new Interval(lower, upper, null));
                }
                break;
            }
            if ((long)prefixes.size() * member.size() > 65536) {
                if (prefixes.size() == 1 && prefixes.getFirst().length == 0) return null;
                access = previousAccess; hasSet = previousSet; result = new ArrayList<>();
                for (byte[] prefix : prefixes) result.add(new Interval(prefix, successor(prefix), null));
                break;
            }
            List<byte[]> product = new ArrayList<>();
            for (byte[] prefix : prefixes) for (Interval interval : member) product.add(concat(prefix, interval.point()));
            prefixes = product;
        }
        if (!hasSet || last == null) return null;
        if (result == null) { result = new ArrayList<>(); for (byte[] prefix : prefixes) result.add(new Interval(prefix, successor(prefix), prefix)); }
        last.preservePoints = false;
        return new Tuple(List.copyOf(last.canonical(result)), access);
    }
    private static byte[] concat(byte[] a, byte[] b) { byte[] result = Arrays.copyOf(a, a.length + b.length); System.arraycopy(b, 0, result, a.length, b.length); return result; }
    private final String name, alias, collation;
    private final SqlType type;
    private final ScalarPlan.Resolver resolver;
    private final CostMeter meter;
    private final QueryEnvironment environment;
    private boolean preservePoints;
    private KeyIntervals(String name, String alias, SqlType type, ScalarPlan.Resolver resolver, CostMeter meter, QueryEnvironment environment, String collation) {
        this.collation = collation; this.name = name; this.alias = alias; this.type = type; this.resolver = resolver; this.meter = meter; this.environment = environment;
    }
    static List<Interval> find(SqlParser.Expr expression, String name, String alias, SqlType type, ScalarPlan.Resolver resolver, CostMeter meter, QueryEnvironment environment) {
        return find(expression, name, alias, type, resolver, meter, environment, "C");
    }
    static List<Interval> find(SqlParser.Expr expression, String name, String alias, SqlType type, ScalarPlan.Resolver resolver, CostMeter meter, QueryEnvironment environment, String collation) {
        return find(expression, name, alias, type, resolver, meter, environment, collation, false);
    }
    static List<Interval> points(SqlParser.Expr expression, String name, String alias, SqlType type, ScalarPlan.Resolver resolver, CostMeter meter, String collation) {
        return find(expression, name, alias, type, resolver, meter, resolver.queryEnvironment(), collation, true);
    }
    private static List<Interval> find(SqlParser.Expr expression, String name, String alias, SqlType type, ScalarPlan.Resolver resolver, CostMeter meter, QueryEnvironment environment, String collation, boolean preservePoints) {
        KeyIntervals planner = new KeyIntervals(name, alias, type, resolver, meter, environment, collation); planner.preservePoints = preservePoints;
        List<SqlParser.Expr> terms = FilterRules.terms(expression);
        List<Interval> selected = null;
        for (SqlParser.Expr term : terms) {
            if (!setSyntax(term)) continue;
            List<Interval> candidate = planner.reduce(term);
            if (candidate != null) { selected = candidate; break; }
        }
        if (selected == null) return null;
        for (SqlParser.Expr term : terms) {
            if (setSyntax(term)) continue;
            List<Interval> clip = planner.reduce(term);
            if (clip != null) selected = planner.intersect(selected, clip);
        }
        return planner.canonical(selected);
    }
    private static boolean setSyntax(SqlParser.Expr expression) {
        return expression instanceof SqlParser.In || expression instanceof SqlParser.Quantified || expression instanceof SqlParser.Binary binary && binary.op().equals("or");
    }
    private boolean column(SqlParser.Expr expression) {
        return expression instanceof SqlParser.Name reference && reference.parts().getLast().equals(name)
            && (reference.parts().size() == 1 || reference.parts().size() == 2 && reference.parts().getFirst().equals(alias));
    }
    private List<Interval> reduce(SqlParser.Expr expression) {
        if (expression instanceof SqlParser.Quantified quantified) {
            if (!quantified.operator().equals("=") || quantified.quantifier().equals("all") || !column(quantified.subject())) return null;
            return quantified.query() != null ? folded(expression) : array(quantified.array());
        }
        if (expression instanceof SqlParser.In in) {
            if (in.negated() || !column(in.subject())) return null;
            if (in.query() != null) return folded(expression);
            List<Interval> result = new ArrayList<>();
            for (SqlParser.Expr value : in.values()) {
                List<Interval> point = comparison("=", value);
                if (point == null) return null;
                result.addAll(point);
            }
            return canonical(result);
        }
        if (expression instanceof SqlParser.Between between && !between.negated() && column(between.subject())) {
            List<Interval> lower = comparison(">=", between.lower()), upper = comparison("<=", between.upper());
            return lower == null || upper == null ? null : intersect(lower, upper);
        }
        if (!(expression instanceof SqlParser.Binary binary)) return null;
        String op = binary.op();
        if (op.equals("and") && arraysInLeaf(expression) > 1) return null;
        if (op.equals("or") || op.equals("and")) {
            List<Interval> left = reduce(binary.left()), right = reduce(binary.right());
            if (left == null || right == null) return null;
            if (op.equals("and")) return intersect(left, right);
            List<Interval> union = new ArrayList<>(left); union.addAll(right); return canonical(union);
        }
        if (!List.of("=", "<", "<=", ">", ">=").contains(op)) return null;
        if (column(binary.left())) return comparison(op, binary.right());
        if (!column(binary.right())) return null;
        return comparison(switch (op) { case "<" -> ">"; case "<=" -> ">="; case ">" -> "<"; case ">=" -> "<="; default -> op; }, binary.left());
    }
    private static int arraysInLeaf(SqlParser.Expr expression) {
        if (expression instanceof SqlParser.Quantified value && value.query() == null) return 1;
        if (expression instanceof SqlParser.Binary value && value.op().equals("and")) return arraysInLeaf(value.left()) + arraysInLeaf(value.right());
        return 0;
    }
    private List<Interval> array(SqlParser.Expr syntax) {
        boolean constructor = syntax instanceof SqlParser.ArrayConstructor;
        if (constructor) {
            SqlParser.ArrayConstructor array = (SqlParser.ArrayConstructor)syntax;
            if (array.query() != null || array.values().stream().anyMatch(value -> !(value instanceof SqlParser.Literal || value instanceof SqlParser.Parameter || value instanceof SqlParser.Name))) return null;
            List<Interval> result = new ArrayList<>();
            for (SqlParser.Expr item : array.values()) {
                ScalarPlan.Expression value = TableScan.constant(resolver, item, type); if (value == null) return null;
                if (value.type() != SqlType.UNKNOWN && value.type().kind() != type.kind() && !(item instanceof SqlParser.Parameter)) return null;
                List<Interval> point = pointValue(meter.unmetered(() -> value.run(meter))); if (point == null) return null; result.addAll(point);
            }
            return canonical(result);
        } else if (!(syntax instanceof SqlParser.Parameter || syntax instanceof SqlParser.Name || syntax instanceof SqlParser.StoredValue || syntax instanceof SqlParser.Cast cast && cast.value() instanceof SqlParser.Literal)) return null;
        ScalarPlan.Expression resolved;
        try { resolved = resolver.resolve(syntax, SqlType.array(type)); }
        catch (SqlException failure) { if (failure.sqlState().equals("42703") || failure.sqlState().equals("42P01")) return null; throw failure; }
        ScalarPlan.Expression valueExpression = resolved;
        SqlValue array = meter.unmetered(() -> valueExpression.run(meter));
        if (array.isNull()) return List.of();
        if (array.type() == SqlType.TEXT && syntax instanceof SqlParser.Parameter) array = SqlCasts.cast(array, SqlType.array(type));
        if (array.type().kind() != SqlType.Kind.ARRAY || constructor && array.type().element().kind() != type.kind()) return null;
        List<Interval> result = new ArrayList<>();
        for (SqlValue value : array.asArray().elements()) {
            List<Interval> point = pointValue(value); if (point == null) return null; result.addAll(point);
        }
        return canonical(result);
    }
    private List<Interval> pointValue(SqlValue value) {
        if (value.isNull()) return List.of();
        if (value.type().kind() != type.kind()) {
            if (!((value.type().isInteger() || value.type().kind() == SqlType.Kind.DECIMAL) && (type.isInteger() || type.kind() == SqlType.Kind.DECIMAL))) return null;
            try {
                SqlValue converted = SqlCasts.cast(value, type);
                if (value.type().kind() == SqlType.Kind.DECIMAL && type.isInteger() && value.asDecimal().compareTo(Decimal.of(converted.asInteger())) != 0) return List.of();
                value = converted;
            } catch (SqlException failure) { if (failure.sqlState().equals("22003")) return List.of(); throw failure; }
        }
        byte[] key = IndexCollations.key(value, collation, meter.environment()); return List.of(new Interval(key, successor(key), key));
    }
    private List<Interval> folded(SqlParser.Expr syntax) {
        RowSpool rows = environment == null ? null : environment.foldedMembership(syntax);
        if (rows == null) return null;
        List<Interval> result = new ArrayList<>();
        try (RowSpool.Cursor cursor = rows.cursor()) {
            List<SqlValue> row;
            while ((row = cursor.next()) != null) {
                SqlValue value = row.getFirst();
                List<Interval> points = pointValue(value); if (points == null) return null; result.addAll(points);
            }
        }
        return canonical(result);
    }
    private List<Interval> comparison(String operator, SqlParser.Expr syntax) {
        SqlParser.Literal literal = DefaultExpression.literal(syntax);
        if (literal != null) syntax = literal;
        SqlValue value;
        try {
            ScalarPlan.Expression resolved = TableScan.constant(resolver, syntax, type);
            if (resolved == null || resolved.type().kind() != type.kind() && !(resolved.type().isInteger() && type.isInteger())) return null;
            if (!operator.equals("=") && !CollationDerivation.merge(CollationDerivation.implicit(collation), resolved.collation()).require().equals(collation)) return null;
            value = resolved.run(meter);
        } catch (SqlException failure) {
            if (failure.sqlState().equals("22003")) return operator.equals("=") ? List.of() : null;
            throw failure;
        }
        if (value.isNull()) return List.of();
        if (type.isInteger() && value.type().isInteger()) {
            if (!type.contains(value.asInteger())) {
                boolean below = value.asInteger() < 0;
                return operator.equals("=") || operator.startsWith(">") && !below || operator.startsWith("<") && below ? List.of() : List.of(new Interval(null, null, null));
            }
            value = SqlValue.ofInteger(type, value.asInteger());
        }
        value = TableScan.keyValue(value, type);
        if (value == null) return operator.equals("=") ? List.of() : List.of(new Interval(null, null, null));
        byte[] key = IndexCollations.key(value, collation, meter.environment()), after = successor(key);
        return switch (operator) {
            case "=" -> List.of(new Interval(key, after, key));
            case ">" -> after == null ? List.of() : List.of(new Interval(after, null, null));
            case ">=" -> List.of(new Interval(key, null, null));
            case "<" -> List.of(new Interval(null, key, null));
            case "<=" -> List.of(new Interval(null, after, null));
            default -> throw new IllegalArgumentException("comparison");
        };
    }
    private List<Interval> intersect(List<Interval> left, List<Interval> right) {
        List<Interval> result = new ArrayList<>();
        for (Interval a : left) for (Interval b : right) {
            byte[] lower = a.lower() == null ? b.lower() : b.lower() == null ? a.lower() : compare(a.lower(), b.lower()) >= 0 ? a.lower() : b.lower();
            byte[] upper = a.upper() == null ? b.upper() : b.upper() == null ? a.upper() : compare(a.upper(), b.upper()) <= 0 ? a.upper() : b.upper();
            if (lower != null && upper != null && compare(lower, upper) >= 0) continue;
            result.add(new Interval(lower, upper, a.point() != null ? a.point() : b.point()));
        }
        return canonical(result);
    }
    private List<Interval> canonical(List<Interval> input) {
        List<Interval> sorted = new ArrayList<>(input);
        sorted.sort((a, b) -> a.lower() == null ? b.lower() == null ? 0 : -1 : b.lower() == null ? 1 : compare(a.lower(), b.lower()));
        List<Interval> result = new ArrayList<>();
        for (Interval next : sorted) {
            if (result.isEmpty()) { result.add(next); continue; }
            Interval previous = result.getLast();
            if (!(preservePoints && previous.point() != null && next.point() != null && !Arrays.equals(previous.point(), next.point())) && (previous.upper() == null || next.lower() == null || compare(previous.upper(), next.lower()) >= 0)) {
                byte[] upper = previous.upper() == null || next.upper() == null ? null : compare(previous.upper(), next.upper()) >= 0 ? previous.upper() : next.upper();
                byte[] point = previous.point() != null && next.point() != null && Arrays.equals(previous.point(), next.point()) ? previous.point() : null;
                result.set(result.size() - 1, new Interval(previous.lower(), upper, point));
            } else result.add(next);
        }
        return result;
    }
    private static int compare(byte[] a, byte[] b) { return Arrays.compareUnsigned(a, b); }
    private static byte[] successor(byte[] key) {
        byte[] result = key.clone();
        for (int i = result.length - 1; i >= 0; i--) if ((result[i] & 255) != 255) { result[i]++; return Arrays.copyOf(result, i + 1); }
        return null;
    }
}
