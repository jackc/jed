package org.jed;

import java.util.ArrayList;
import java.util.List;

/** Exact host overloads share the catalog coercion costs with built-in families. */
final class HostFunctions {
    private HostFunctions() {}
    static ScalarPlan.Expression resolve(SqlParser.Call syntax, ScalarPlan.Resolver resolver) {
        CostMeter meter = resolver.meter();
        if (resolver.hostFunctions().stream().noneMatch(function -> function.name().equals(syntax.name()))) return null;
        if (syntax.distinct() || syntax.star() || syntax.filter() != null || !syntax.order().isEmpty()) return null;
        var arguments = new ArrayList<ScalarPlan.Expression>();
        for (SqlParser.Expr arg : syntax.args()) { if (arg instanceof SqlParser.NamedArgument) return null; arguments.add(resolver.resolve(arg, null)); }
        HostFunction selected = null; int best = Integer.MAX_VALUE; boolean ambiguous = false;
        for (HostFunction candidate : resolver.hostFunctions()) {
            if (!candidate.name().equals(syntax.name()) || candidate.arguments().size() != arguments.size()) continue;
            int score = 0;
            for (int i = 0; i < arguments.size(); i++) { int cost = conversion(arguments.get(i).type(), candidate.arguments().get(i), origin(syntax.args().get(i))); if (cost == Integer.MAX_VALUE) { score = cost; break; } score += cost; }
            if (score < best) { best = score; selected = candidate; ambiguous = false; }
            else if (score == best && score != Integer.MAX_VALUE) ambiguous = true;
        }
        if (selected == null || builtinScore(syntax, arguments) <= best) return null;
        if (ambiguous) throw new SqlException("42725", "ambiguous host function: " + syntax.name());
        for (int i = 0; i < arguments.size(); i++) {
            SqlParser.Literal literal = DefaultExpression.literal(syntax.args().get(i));
            if (literal != null && literal.kind() == Lexer.Kind.INTEGER && selected.arguments().get(i).isInteger()) SqlValue.ofInteger(selected.arguments().get(i), ScalarPlan.integer(literal.text()));
        }
        return new ScalarPlan.Expression(selected.result(), new Call(selected, arguments));
    }
    static final class Call implements ScalarPlan.Eval {
        ScalarPlan.Frame frame;
        final java.util.IdentityHashMap<List<SqlValue>, HostBatch.Outcome> prefetched = new java.util.IdentityHashMap<>();
        final HostFunction function;
        final List<ScalarPlan.Expression> arguments;
        Call(HostFunction function, List<ScalarPlan.Expression> arguments) { this.function = function; this.arguments = List.copyOf(arguments); }
        @Override public SqlValue run(CostMeter meter) {
            meter.charge(function.cost()); meter.guard();
            HostBatch.Outcome cached = frame == null ? null : prefetched.get(frame.values);
            if (cached != null) { if (cached.error() != null) throw cached.error(); return checked(function, cached.value()); }
            var columns = new ArrayList<List<SqlValue>>(arguments.size());
            for (int i = 0; i < arguments.size(); i++) {
                SqlValue value = arguments.get(i).run(meter); if (value.isNull()) return SqlValue.nullValue(function.result());
                columns.add(List.of(SqlCasts.cast(value, function.arguments().get(i), meter)));
            }
            Batch result = invoke(function, List.copyOf(columns), 1);
            if (result.error() != null && result.values().isEmpty()) throw result.error();
            return checked(function, result.values().getFirst());
        }
    }
    record Batch(List<SqlValue> values, SqlException error) {}
    static Batch invoke(HostFunction function, List<List<SqlValue>> columns, int count) {
        var output = new ArrayList<SqlValue>(); SqlException error = null;
        try { function.kernel().apply(columns, output); }
        catch (SqlException failure) { error = failure; }
        catch (RuntimeException failure) { error = new SqlException("38000", "host function " + function.name() + " failed: " + failure.getClass().getSimpleName() + ": " + failure.getMessage()); }
        if (output.size() > count || error != null && output.size() >= count) return new Batch(List.of(), new SqlException("22000", "host batch result count is invalid"));
        if (error == null && output.size() < count) error = new SqlException("22000", "host batch returned too few results");
        return new Batch(java.util.Collections.unmodifiableList(new ArrayList<>(output)), error);
    }
    static SqlValue checked(HostFunction function, SqlValue value) {
        SqlType type = function.result();
        if (value == null) throw new SqlException("22000", "host function returned a null Java reference");
        if (value.isNull()) return SqlValue.nullValue(type);
        if (type.isInteger() && value.type().isInteger()) return SqlValue.ofInteger(type, value.asInteger());
        if (value.type().kind() != type.kind()) throw new SqlException("22000", "host function returned the wrong value kind");
        return value.relabel(type);
    }
    private static int builtinScore(SqlParser.Call syntax, List<ScalarPlan.Expression> arguments) {
        int best = Integer.MAX_VALUE;
        for (FunctionCatalog.Fact fact : FunctionCatalog.FACTS) {
            if (!fact.name().equals(syntax.name()) || fact.arity() != arguments.size()) continue;
            int total = 0;
            for (int i = 0; i < arguments.size(); i++) {
                SqlType source = arguments.get(i).type(); String family = fact.families().get(i);
                if (source == SqlType.UNKNOWN || family(family, source)) continue;
                int score = Integer.MAX_VALUE;
                for (ArgCoercionCatalog.Fact conversion : ArgCoercionCatalog.FACTS) {
                    if (!conversion.from().equals(source.kind().name().toLowerCase(java.util.Locale.ROOT)) && !conversion.from().equals(origin(syntax.args().get(i)))) continue;
                    if (family(family, SqlType.named(conversion.to()))) score = Math.min(score, conversion.cost());
                }
                if (score == Integer.MAX_VALUE) { total = score; break; } total += score;
            }
            best = Math.min(best, total);
        }
        return best;
    }
    private static int conversion(SqlType from, SqlType to, String origin) {
        if (from == SqlType.UNKNOWN) return Integer.MAX_VALUE;
        if (from.kind() == to.kind()) return 0;
        int best = Integer.MAX_VALUE;
        for (ArgCoercionCatalog.Fact fact : ArgCoercionCatalog.FACTS) if ((fact.from().equals(from.kind().name().toLowerCase(java.util.Locale.ROOT)) || fact.from().equals(origin)) && fact.to().equals(to.kind().name().toLowerCase(java.util.Locale.ROOT))) best = Math.min(best, fact.cost());
        return best;
    }
    private static String origin(SqlParser.Expr argument) {
        if (argument instanceof SqlParser.Literal literal) return switch (literal.kind()) { case INTEGER -> "integer_literal"; case DECIMAL -> "decimal_literal"; default -> "value"; };
        return "value";
    }
    private static boolean family(String family, SqlType type) {
        return switch (family) {
            case "any" -> true;
            case "integer" -> type.isInteger();
            case "float" -> type.isFloat();
            case "anyarray" -> type.kind() == SqlType.Kind.ARRAY;
            case "anyrange" -> type.kind() == SqlType.Kind.RANGE;
            case "anycomposite" -> type.isComposite();
            default -> family.equals(type.kind().name().toLowerCase(java.util.Locale.ROOT));
        };
    }
}
