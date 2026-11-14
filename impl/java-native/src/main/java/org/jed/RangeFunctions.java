package org.jed;

import java.util.ArrayList;
import java.util.List;
import java.util.Set;

/** Range overload binding uses the shared six subtype descriptors. */
final class RangeFunctions {
    private RangeFunctions() {}
    static ScalarPlan.Expression resolve(SqlParser.Call call, ScalarPlan.Resolver resolver) {
        for (RangeCatalog.Fact fact : RangeCatalog.FACTS) if (fact.aliases().contains(call.name())) return constructor(call, SqlType.range(SqlType.named(fact.element())), resolver);
        if (!Set.of("lower", "upper", "isempty", "lower_inc", "upper_inc", "lower_inf", "upper_inf", "range_merge").contains(call.name())) return null;
        if (call.args().size() != (call.name().equals("range_merge") ? 2 : 1)) throw missing();
        ScalarPlan.Expression first = resolver.resolve(call.args().getFirst(), null);
        if (first.type().kind() != SqlType.Kind.RANGE) {
            if (call.name().equals("lower") || call.name().equals("upper")) return null;
            if (first.type() == SqlType.UNKNOWN) throw new SqlException("42P18", "cannot determine range subtype");
            throw missing();
        }
        if (call.name().equals("range_merge")) return binary(new SqlParser.Binary("range_merge", call.args().get(0), call.args().get(1)), resolver);
        SqlType result = call.name().equals("lower") || call.name().equals("upper") ? first.type().element() : SqlType.BOOLEAN;
        return new ScalarPlan.Expression(result, meter -> {
            meter.operation(); SqlValue value = first.run(meter); if (value.isNull()) return SqlValue.nullValue(result);
            SqlRange range = value.asRange();
            return switch (call.name()) {
                case "lower" -> range.lower() == null ? SqlValue.nullValue(result) : range.lower();
                case "upper" -> range.upper() == null ? SqlValue.nullValue(result) : range.upper();
                case "isempty" -> SqlValue.ofBoolean(range.isEmpty());
                case "lower_inc" -> SqlValue.ofBoolean(range.lowerInclusive());
                case "upper_inc" -> SqlValue.ofBoolean(range.upperInclusive());
                case "lower_inf" -> SqlValue.ofBoolean(!range.isEmpty() && range.lower() == null);
                case "upper_inf" -> SqlValue.ofBoolean(!range.isEmpty() && range.upper() == null);
                default -> throw new AssertionError(call.name());
            };
        }, first.constant());
    }
    private static ScalarPlan.Expression constructor(SqlParser.Call call, SqlType type, ScalarPlan.Resolver resolver) {
        if (call.args().size() < 2 || call.args().size() > 3) throw missing();
        List<ScalarPlan.Expression> args = new ArrayList<>();
        args.add(bound(call.args().get(0), type.element(), resolver)); args.add(bound(call.args().get(1), type.element(), resolver));
        if (call.args().size() == 3) {
            ScalarPlan.Expression flags = resolver.resolve(call.args().get(2), SqlType.TEXT);
            if (flags.type().kind() != SqlType.Kind.TEXT && flags.type() != SqlType.UNKNOWN) throw missing();
            args.add(flags);
        }
        return new ScalarPlan.Expression(type, meter -> {
            meter.operation(); SqlValue lower = args.get(0).run(meter), upper = args.get(1).run(meter);
            String flags = "[)";
            if (args.size() == 3) { SqlValue value = args.get(2).run(meter); if (value.isNull()) throw new SqlException("22000", "NULL range bound flags"); flags = value.asText(); }
            if (!Set.of("[)", "[]", "(]", "()").contains(flags)) throw new SqlException("42601", "invalid range bound flags");
            return SqlValue.ofRange(type, SqlRange.create(type.element(), lower, upper, flags.charAt(0) == '[', flags.charAt(1) == ']'));
        }, args.stream().allMatch(ScalarPlan.Expression::constant));
    }
    private static ScalarPlan.Expression bound(SqlParser.Expr syntax, SqlType element, ScalarPlan.Resolver resolver) {
        ScalarPlan.Expression arg = resolver.resolve(syntax, element);
        SqlType source = arg.type();
        boolean temporal = element == SqlType.DATE || element == SqlType.TIMESTAMP || element == SqlType.TIMESTAMPTZ;
        if (source != SqlType.UNKNOWN && !source.equals(element) && !(element.isInteger() && source.isInteger()) && !(element == SqlType.DECIMAL && source.isInteger()) && !(temporal && source.kind() == SqlType.Kind.TEXT)) throw missing();
        return new ScalarPlan.Expression(element, meter -> SqlCasts.cast(arg.run(meter), element, meter), arg.constant());
    }
    static ScalarPlan.Expression binary(SqlParser.Binary syntax, ScalarPlan.Resolver resolver) {
        String op = syntax.op();
        if (!Set.of("@>", "<@", "&&", "<<", ">>", "&<", "&>", "-|-", "+", "-", "*", "range_merge").contains(op)) return null;
        ScalarPlan.Expression a = resolver.resolve(syntax.left(), null), b = resolver.resolve(syntax.right(), null);
        boolean ar = a.type().kind() == SqlType.Kind.RANGE, br = b.type().kind() == SqlType.Kind.RANGE;
        if (!ar && !br) return null;
        SqlType type = ar ? a.type() : b.type();
        if (a.type() == SqlType.UNKNOWN || syntax.left() instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.STRING) { a = resolver.resolve(syntax.left(), type); ar = true; }
        if (b.type() == SqlType.UNKNOWN || syntax.right() instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.STRING) { b = resolver.resolve(syntax.right(), type); br = true; }
        boolean element = false;
        if (op.equals("@>") && ar && !br) { b = bound(syntax.right(), type.element(), resolver); element = true; }
        else if (op.equals("<@") && !ar && br) { a = bound(syntax.left(), type.element(), resolver); element = true; }
        else if (!a.type().equals(type) || !b.type().equals(type)) throw missing();
        ScalarPlan.Expression left = a, right = b; boolean point = element;
        boolean set = Set.of("+", "-", "*", "range_merge").contains(op); SqlType result = set ? type : SqlType.BOOLEAN;
        return new ScalarPlan.Expression(result, meter -> {
            meter.operation(); SqlValue x = left.run(meter); if (x.isNull()) return SqlValue.nullValue(result);
            SqlValue y = right.run(meter); if (y.isNull()) return SqlValue.nullValue(result);
            if (point) return SqlValue.ofBoolean(op.equals("@>") ? x.asRange().contains(y) : y.asRange().contains(x));
            SqlRange r = x.asRange(), s = y.asRange();
            if (set) return SqlValue.ofRange(type, switch (op) { case "+" -> r.union(s, true); case "range_merge" -> r.union(s, false); case "*" -> r.intersection(s); case "-" -> r.subtract(s); default -> throw new AssertionError(op); });
            return SqlValue.ofBoolean(switch (op) {
                case "@>" -> r.contains(s); case "<@" -> s.contains(r); case "&&" -> r.overlaps(s);
                case "<<" -> r.before(s); case ">>" -> s.before(r); case "-|-" -> r.adjacent(s);
                case "&<" -> !r.isEmpty() && !s.isEmpty() && r.upperCompare(s) <= 0;
                case "&>" -> !r.isEmpty() && !s.isEmpty() && r.lowerCompare(s) >= 0;
                default -> throw new AssertionError(op);
            });
        }, left.constant() && right.constant());
    }
    private static SqlException missing() { return new SqlException("42883", "range function or operator does not exist for argument types"); }
}
