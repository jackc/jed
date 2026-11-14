package org.jed;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

/** Structural expression resolution and evaluation for array constructors and access. */
final class ArrayExpressions {
    private ArrayExpressions() {}
    static boolean needsContext(SqlParser.ArrayConstructor syntax) {
        return syntax.values().stream().allMatch(value -> value instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.WORD && literal.text().equals("null") || value instanceof SqlParser.ArrayConstructor array && needsContext(array));
    }
    static ScalarPlan.Expression constructor(SqlParser.ArrayConstructor syntax, SqlType context, ScalarPlan.Resolver resolver) {
        if (syntax.query() != null) throw new SqlException("0A000", "array subquery is not connected yet");
        SqlType hint = context != null && context.kind() == SqlType.Kind.ARRAY ? context.element() : null;
        SqlType element = null;
        List<ScalarPlan.Expression> args = new ArrayList<>(); Boolean nested = null;
        for (SqlParser.Expr item : syntax.values()) {
            boolean bareNull = item instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.WORD && literal.text().equals("null");
            ScalarPlan.Expression arg = resolver.resolve(item, bareNull ? null : item instanceof SqlParser.ArrayConstructor ? context : hint);
            boolean isArray = arg.type().kind() == SqlType.Kind.ARRAY;
            if (arg.type() != SqlType.UNKNOWN) {
                if (nested != null && nested != isArray) throw SqlException.typeMismatch("mixed array and scalar constructor elements");
                nested = isArray;
                SqlType candidate = isArray ? arg.type().element() : arg.type();
                try { element = element == null ? candidate : ScalarPlan.common(element, candidate); }
                catch (SqlException failure) { throw SqlException.typeMismatch("array elements have incompatible types"); }
            }
            args.add(arg);
        }
        if (element == null || element == SqlType.UNKNOWN) element = hint == null ? SqlType.TEXT : hint;
        if (context == null || context.kind() != SqlType.Kind.ARRAY) element = switch (element.kind()) {
            case TEXT -> SqlType.TEXT; case DECIMAL -> SqlType.DECIMAL; case TIMESTAMP -> SqlType.TIMESTAMP;
            case TIMESTAMPTZ -> SqlType.TIMESTAMPTZ; case INTERVAL -> SqlType.INTERVAL; default -> element;
        };
        SqlType type = SqlType.array(element); boolean stack = Boolean.TRUE.equals(nested); boolean constant = args.stream().allMatch(ScalarPlan.Expression::constant);
        return new ScalarPlan.Expression(type, meter -> {
            meter.operation();
            List<SqlValue> values = new ArrayList<>(); int[] dims = null, lower = null;
            for (ScalarPlan.Expression arg : args) {
                SqlValue value = arg.run(meter);
                if (stack) {
                    if (value.isNull()) throw SqlArray.shape();
                    SqlArray array = value.asArray();
                    if (dims == null) { dims = array.dims(); lower = array.lowerBounds(); }
                    else if (!Arrays.equals(dims, array.dims()) || !Arrays.equals(lower, array.lowerBounds())) throw SqlArray.shape();
                    for (SqlValue item : array.elements()) values.add(SqlCasts.cast(item, type.element(), meter));
                } else values.add(SqlCasts.cast(value, type.element(), meter));
            }
            SqlArray output;
            if (stack && dims != null && dims.length != 0) {
                int[] full = new int[dims.length + 1], bounds = new int[dims.length + 1]; full[0] = args.size(); bounds[0] = 1;
                System.arraycopy(dims, 0, full, 1, dims.length); System.arraycopy(lower, 0, bounds, 1, lower.length);
                output = new SqlArray(full, bounds, values);
            } else output = stack ? SqlArray.empty() : SqlArray.vector(values);
            SqlValue result = SqlValue.ofArray(type, output); if (!constant) meter.scalarInput(MemoryAccount.payload(result)); return result;
        }, args.stream().allMatch(ScalarPlan.Expression::constant));
    }
    static ScalarPlan.Expression quantified(SqlParser.Quantified syntax, ScalarPlan.Resolver resolver) {
        if (syntax.query() != null) throw new SqlException("0A000", "quantified subquery is not connected yet");
        ScalarPlan.Expression subject = resolver.resolve(syntax.subject(), null);
        ScalarPlan.Expression array;
        try { array = resolver.resolve(syntax.array(), subject.type() == SqlType.UNKNOWN ? null : SqlType.array(subject.type())); }
        catch (SqlException failure) { if (failure.sqlState().equals("42804")) throw new SqlException("42883", "quantified comparison element types are incompatible"); throw failure; }
        if (syntax.array() instanceof SqlParser.Parameter && subject.type() != SqlType.UNKNOWN && (array.type() == SqlType.UNKNOWN || array.type() == SqlType.TEXT)) {
            ScalarPlan.Expression original = array; SqlType target = SqlType.array(subject.type());
            array = new ScalarPlan.Expression(target, meter -> { SqlValue value = original.run(meter); return value.isNull() ? SqlValue.nullValue(target) : SqlCasts.cast(value, target); }, original.constant());
        }
        if (array.type() == SqlType.UNKNOWN) throw new SqlException("42P18", "cannot determine array element type");
        if (array.type().kind() != SqlType.Kind.ARRAY) throw new SqlException("42809", "quantified comparison requires array operand");
        subject = resolver.resolve(syntax.subject(), array.type().element());
        try { if (subject.type() != SqlType.UNKNOWN && !(syntax.array() instanceof SqlParser.Parameter)) array = resolver.resolve(syntax.array(), SqlType.array(subject.type())); }
        catch (SqlException failure) { if (failure.sqlState().equals("42804")) throw new SqlException("42883", "quantified comparison element types are incompatible"); throw failure; }
        try { ScalarPlan.compatible(subject.type(), array.type().element()); }
        catch (SqlException failure) { throw new SqlException("42883", "quantified comparison element types are incompatible"); }
        String collationName = CollationDerivation.comparison(subject.collation(), array.collation()).require();
        Collation collation = resolver.meter() == null ? Collation.C : resolver.meter().environment().collation(collationName);
        ScalarPlan.Expression source = array, left = subject; boolean all = syntax.quantifier().equals("all");
        return new ScalarPlan.Expression(SqlType.BOOLEAN, meter -> {
            meter.operation(); SqlValue x = left.run(meter), value = source.run(meter);
            if (value.isNull()) return SqlValue.nullValue(SqlType.BOOLEAN);
            boolean unknown = false;
            for (SqlValue item : value.asArray().elements()) {
                meter.operation();
                if (x.isNull() || item.isNull()) { unknown = true; continue; }
                // Quantified membership meters each element and decimal work; its internal
                // text/bytea comparison has no standalone comparison-operator node.
                int c = !List.of("=", "!=", "<>").contains(syntax.operator()) && collation != Collation.C ? CollationDerivation.compare(x, item, meter, collation)
                    : ScalarPlan.compare(x, item, x.type().kind() == SqlType.Kind.TEXT || x.type().kind() == SqlType.Kind.BYTEA ? null : meter);
                boolean match = switch (syntax.operator()) { case "=" -> c == 0; case "!=", "<>" -> c != 0; case "<" -> c < 0; case "<=" -> c <= 0; case ">" -> c > 0; case ">=" -> c >= 0; default -> throw new AssertionError(syntax.operator()); };
                if (match != all) return SqlValue.ofBoolean(match);
            }
            return unknown ? SqlValue.nullValue(SqlType.BOOLEAN) : SqlValue.ofBoolean(all);
        });
    }

    static ScalarPlan.Expression access(SqlParser.Access syntax, ScalarPlan.Resolver resolver) {
        ScalarPlan.Expression base = resolver.resolve(syntax.base(), null);
        if (base.type().kind() != SqlType.Kind.ARRAY) throw SqlException.typeMismatch("cannot subscript a non-array");
        boolean slice = syntax.subscripts().stream().anyMatch(SqlParser.Subscript::slice);
        List<ScalarPlan.Expression> low = new ArrayList<>(), high = new ArrayList<>();
        for (SqlParser.Subscript spec : syntax.subscripts()) {
            low.add(spec.lower() == null ? null : resolver.resolve(spec.lower(), SqlType.I64));
            high.add(spec.upper() == null ? null : resolver.resolve(spec.upper(), SqlType.I64));
        }
        for (ScalarPlan.Expression arg : low) if (arg != null && !arg.type().isInteger()) throw SqlException.typeMismatch("array index must be integer");
        for (ScalarPlan.Expression arg : high) if (arg != null && !arg.type().isInteger()) throw SqlException.typeMismatch("array index must be integer");
        SqlType result = slice ? base.type() : base.type().element();
        return new ScalarPlan.Expression(result, meter -> {
            meter.operation(); SqlValue source = base.run(meter); if (source.isNull()) return SqlValue.nullValue(result);
            long[] starts = new long[low.size()], ends = new long[low.size()];
            for (int i = 0; i < low.size(); i++) {
                SqlValue l = low.get(i) == null ? null : low.get(i).run(meter), h = high.get(i) == null ? null : high.get(i).run(meter);
                if (l != null && l.isNull() || h != null && h.isNull()) return SqlValue.nullValue(result);
                starts[i] = l == null ? Long.MIN_VALUE : l.asInteger(); ends[i] = h == null ? Long.MAX_VALUE : h.asInteger();
                if (slice && !syntax.subscripts().get(i).slice()) { ends[i] = starts[i]; starts[i] = 1; }
            }
            return slice ? SqlValue.ofArray(result, source.asArray().slice(starts, ends)) : source.asArray().subscript(result, starts);
        });
    }
}
