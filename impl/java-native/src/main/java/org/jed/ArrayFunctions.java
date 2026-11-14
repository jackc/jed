package org.jed;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Set;

/** Handwritten polymorphic binding over catalog signatures, followed by array value kernels. */
final class ArrayFunctions {
    private static final Set<String> NAMES = Set.of("array_ndims", "array_length", "array_lower", "array_upper", "cardinality", "array_dims", "array_append", "array_prepend", "array_cat", "array_remove", "array_replace", "array_position", "array_positions");
    private ArrayFunctions() {}
    static ScalarPlan.Expression resolve(SqlParser.Call call, ScalarPlan.Resolver resolver) {
        ScalarPlan.Expression conversion = ArrayConversions.resolve(call, resolver);
        if (conversion != null) return conversion;
        if (!NAMES.contains(call.name())) return null;
        FunctionCatalog.Fact fact = null;
        for (FunctionCatalog.Fact candidate : FunctionCatalog.FACTS) if (candidate.name().equals(call.name()) && candidate.arity() == call.args().size()) { fact = candidate; break; }
        if (fact == null) throw missing();
        List<ScalarPlan.Expression> args = new ArrayList<>();
        for (SqlParser.Expr syntax : call.args()) args.add(resolver.resolve(syntax, null));
        SqlType hint = null;
        for (int i = 0; i < args.size(); i++) if (fact.families().get(i).endsWith("array") && args.get(i).type().kind() == SqlType.Kind.ARRAY) { hint = args.get(i).type().element(); break; }
        if (hint != null) for (int i = 0; i < args.size(); i++) {
            String family = fact.families().get(i);
            if (family.startsWith("any")) {
                try { args.set(i, resolver.resolve(call.args().get(i), family.endsWith("array") ? SqlType.array(hint) : hint)); }
                catch (SqlException failure) { if (failure.sqlState().equals("42804")) throw missing(); throw failure; }
            }
        }
        return bind(call.name(), fact, args);
    }
    static ScalarPlan.Expression binary(SqlParser.Binary syntax, ScalarPlan.Resolver resolver) {
        if (!Set.of("||", "@>", "<@", "&&").contains(syntax.op())) return null;
        ScalarPlan.Expression left = resolver.resolve(syntax.left(), null), right = resolver.resolve(syntax.right(), null);
        boolean la = left.type().kind() == SqlType.Kind.ARRAY, ra = right.type().kind() == SqlType.Kind.ARRAY;
        if (!la && !ra) return null;
        if (syntax.op().equals("||")) {
            String name = (la || left.type() == SqlType.UNKNOWN || syntax.left() instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.STRING) && (ra || right.type() == SqlType.UNKNOWN || syntax.right() instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.STRING) ? "array_cat" : la ? "array_append" : "array_prepend";
            return resolve(new SqlParser.Call(name, List.of(syntax.left(), syntax.right())), resolver);
        }
        SqlType type = la ? left.type() : right.type();
        try { left = resolver.resolve(syntax.left(), type); right = resolver.resolve(syntax.right(), type); }
        catch (SqlException failure) { if (failure.sqlState().equals("42804")) throw missing(); throw failure; }
        if (!left.type().equals(right.type()) || left.type().kind() != SqlType.Kind.ARRAY) throw missing();
        CollationDerivation.comparison(left.collation(), right.collation()).require();
        ScalarPlan.Expression a = left, b = right;
        return new ScalarPlan.Expression(SqlType.BOOLEAN, meter -> {
            meter.operation(); SqlValue x = a.run(meter), y = b.run(meter);
            if (x.isNull() || y.isNull()) return SqlValue.nullValue(SqlType.BOOLEAN);
            SqlArray needles = syntax.op().equals("<@") ? x.asArray() : y.asArray(), haystack = syntax.op().equals("<@") ? y.asArray() : x.asArray();
            boolean overlap = syntax.op().equals("&&");
            for (SqlValue needle : needles.elements()) {
                boolean found = false;
                if (!needle.isNull()) for (SqlValue item : haystack.elements()) if (!item.isNull() && equal(needle, item, null)) { found = true; break; }
                if (overlap && found) return SqlValue.ofBoolean(true);
                if (!overlap && !found) return SqlValue.ofBoolean(false);
            }
            return SqlValue.ofBoolean(!overlap);
        });
    }

    private static ScalarPlan.Expression bind(String name, FunctionCatalog.Fact fact, List<ScalarPlan.Expression> args) {
        SqlType element = null;
        for (int i = 0; i < args.size(); i++) {
            String family = fact.families().get(i); SqlType type = args.get(i).type();
            if (!family.startsWith("any")) { if (type != SqlType.UNKNOWN && !(family.equals("integer") && type.isInteger())) throw missing(); continue; }
            if (type == SqlType.UNKNOWN) continue;
            if (family.endsWith("array")) { if (type.kind() != SqlType.Kind.ARRAY) throw missing(); type = type.element(); }
            if (element == null) element = type;
            else if (family.startsWith("anycompatible")) { try { element = ScalarPlan.common(element, type); } catch (SqlException failure) { throw missing(); } }
            else if (!element.equals(type)) throw missing();
        }
        if (element == null) throw new SqlException("42P18", "cannot determine array element type");
        SqlType arrayType = SqlType.array(element);
        SqlType result = switch (fact.result()) { case "anyarray", "anycompatiblearray" -> arrayType; case "i32[]" -> SqlType.array(SqlType.I32); default -> SqlType.named(fact.result()); };
        List<ScalarPlan.Expression> converted = new ArrayList<>();
        for (int i = 0; i < args.size(); i++) {
            String family = fact.families().get(i); ScalarPlan.Expression arg = args.get(i);
            SqlType target = family.startsWith("any") ? family.endsWith("array") ? arrayType : element : arg.type() == SqlType.UNKNOWN ? SqlType.I64 : arg.type();
            boolean cost = !arg.type().equals(target) && arg.type() != SqlType.UNKNOWN && !(arg.type().isInteger() && target.isInteger())
                && !(arg.type().kind() == SqlType.Kind.ARRAY && arg.type().element().isInteger() && target.element().isInteger());
            converted.add(new ScalarPlan.Expression(target, meter -> { if (cost) meter.operation(); return SqlCasts.cast(arg.run(meter), target, meter); }, arg.constant()));
        }
        return new ScalarPlan.Expression(result, meter -> {
            meter.operation(); SqlValue[] values = new SqlValue[converted.size()];
            boolean introspect = name.equals("cardinality") || name.equals("array_dims") || name.equals("array_ndims") || name.equals("array_length") || name.equals("array_lower") || name.equals("array_upper");
            for (int i = 0; i < values.length; i++) { values[i] = converted.get(i).run(meter); if (introspect && values[i].isNull()) return SqlValue.nullValue(result); }
            return evaluate(name, values, result, meter, !converted.stream().allMatch(ScalarPlan.Expression::constant));
        });
    }
    private static SqlException missing() { return new SqlException("42883", "array function does not exist for argument types"); }
    private static SqlValue integer(long value) { return SqlValue.ofInteger(SqlType.I32, value); }
    private static SqlValue evaluate(String name, SqlValue[] v, SqlType result, CostMeter meter, boolean growth) {
        if (name.equals("array_append") || name.equals("array_prepend")) {
            boolean prepend = name.equals("array_prepend"); SqlValue source = v[prepend ? 1 : 0], item = v[prepend ? 0 : 1];
            SqlArray a = source.isNull() ? SqlArray.empty() : source.asArray();
            if (a.dimensions() > 1) throw new SqlException("22000", "array must be empty or one-dimensional");
            int lower = a.size() == 0 ? 1 : a.lowerBound(0);
            if (prepend && lower == Integer.MIN_VALUE || !prepend && (long) lower + a.size() > Integer.MAX_VALUE) throw SqlException.numericOverflow();
            if (!prepend && (long) lower + a.size() + 1 > Integer.MAX_VALUE) throw new SqlException("54000", "array lower bound too large");
            long payload = MemorySchedule.ARRAY_DIM + MemoryAccount.value(item); for (SqlValue old : a.elements()) payload = MemoryAccount.add(payload, MemoryAccount.value(old)); if (growth) meter.scalarInput(payload);
            List<SqlValue> items = new ArrayList<>(a.size() + 1); if (prepend) items.add(item); items.addAll(a.elements()); if (!prepend) items.add(item);
            return SqlValue.ofArray(result, new SqlArray(new int[]{items.size()}, new int[]{lower}, items));
        }
        if (name.equals("array_cat")) {
            if (v[0].isNull() && v[1].isNull()) return SqlValue.nullValue(result);
            if (v[0].isNull()) { if (growth) meter.scalarInput(MemoryAccount.payload(v[1])); return v[1]; }
            if (v[1].isNull()) { if (growth) meter.scalarInput(MemoryAccount.payload(v[0])); return v[0]; }
            if (v[0].asArray().size() == 0) { if (growth) meter.scalarInput(MemoryAccount.payload(v[1])); return v[1]; }
            if (v[1].asArray().size() == 0) { if (growth) meter.scalarInput(MemoryAccount.payload(v[0])); return v[0]; }
            SqlArray a = v[0].asArray(), b = v[1].asArray(); int delta = a.dimensions() - b.dimensions();
            if (Math.abs(delta) > 1) throw SqlArray.shape();
            SqlArray shape = delta >= 0 ? a : b; int[] dims = shape.dims(), lower = shape.lowerBounds();
            if (delta == 0) {
                for (int d = 1; d < dims.length; d++) if (a.length(d) != b.length(d) || a.lowerBound(d) != b.lowerBound(d)) throw SqlArray.shape();
                dims[0] = checkedLength((long) a.length(0) + b.length(0));
            } else {
                SqlArray small = delta > 0 ? b : a;
                for (int d = 0; d < small.dimensions(); d++) if (small.length(d) != dims[d + 1] || small.lowerBound(d) != lower[d + 1]) throw SqlArray.shape();
                dims[0] = checkedLength((long) dims[0] + 1);
            }
            if ((long) lower[0] + dims[0] > Integer.MAX_VALUE) throw new SqlException("54000", "array lower bound too large");
            long payload = MemorySchedule.ARRAY_DIM * dims.length;
            for (SqlValue item : a.elements()) payload = MemoryAccount.add(payload, MemoryAccount.value(item));
            for (SqlValue item : b.elements()) payload = MemoryAccount.add(payload, MemoryAccount.value(item)); if (growth) meter.scalarInput(payload);
            List<SqlValue> items = new ArrayList<>(a.size() + b.size()); items.addAll(a.elements()); items.addAll(b.elements());
            return SqlValue.ofArray(result, new SqlArray(dims, lower, items));
        }
        if (v[0].isNull()) return SqlValue.nullValue(result);
        SqlArray a = v[0].asArray();
        if (name.equals("array_replace") || name.equals("array_remove") || name.equals("array_position") || name.equals("array_positions")) {
            if (!name.equals("array_replace") && a.dimensions() > 1) throw new SqlException("0A000", "search requires a one-dimensional array");
            long start = Long.MIN_VALUE;
            if (v.length == 3 && name.equals("array_position")) { if (v[2].isNull()) throw new SqlException("22004", "array search start cannot be NULL"); start = v[2].asInteger(); }
            List<SqlValue> output = new ArrayList<>(); long payload = MemorySchedule.ARRAY_DIM * a.dimensions();
            if (name.equals("array_replace")) {
                meter.scalarInput(MemoryAccount.payload(v[0]));
                for (SqlValue item : a.elements()) payload = MemoryAccount.add(payload, MemoryAccount.value(equal(item, v[1], meter) ? v[2] : item));
                meter.scalarOutput(payload);
            }
            for (int i = 0; i < a.size(); i++) {
                SqlValue item = a.element(i); boolean match = equal(item, v[1], meter); long subscript = (long) a.lowerBound(0) + i;
                switch (name) {
                    case "array_position" -> { if (match && subscript >= start) return integer(subscript); }
                    case "array_positions" -> { if (match) output.add(integer(subscript)); }
                    case "array_remove" -> { if (!match) output.add(item); }
                    case "array_replace" -> output.add(match ? v[2] : item);
                    default -> throw new AssertionError(name);
                }
            }
            if (name.equals("array_position")) return SqlValue.nullValue(result);
            SqlArray array = name.equals("array_replace") ? new SqlArray(a.dims(), a.lowerBounds(), output) : output.isEmpty() ? SqlArray.empty()
                : new SqlArray(new int[]{output.size()}, new int[]{name.equals("array_positions") ? 1 : a.lowerBound(0)}, output);
            return SqlValue.ofArray(result, array);
        }
        if (name.equals("cardinality")) return integer(a.size());
        if (a.dimensions() == 0) return SqlValue.nullValue(result);
        if (name.equals("array_ndims")) return integer(a.dimensions());
        if (name.equals("array_dims")) { StringBuilder out = new StringBuilder(); for (int d = 0; d < a.dimensions(); d++) out.append('[').append(a.lowerBound(d)).append(':').append((long) a.lowerBound(d) + a.length(d) - 1).append(']'); return SqlValue.ofText(out.toString()); }
        long dim = v[1].asInteger() - 1;
        if (dim < 0 || dim >= a.dimensions()) return SqlValue.nullValue(result);
        return integer(switch (name) { case "array_length" -> a.length((int) dim); case "array_lower" -> a.lowerBound((int) dim); case "array_upper" -> (long) a.lowerBound((int) dim) + a.length((int) dim) - 1; default -> throw new AssertionError(name); });
    }
    private static int checkedLength(long value) { if (value > Integer.MAX_VALUE) throw new SqlException("54000", "array too large"); return (int) value; }
    static boolean equal(SqlValue a, SqlValue b, CostMeter meter) { return a.isNull() ? b.isNull() : !b.isNull() && ScalarPlan.compare(a, b, meter) == 0; }
}
