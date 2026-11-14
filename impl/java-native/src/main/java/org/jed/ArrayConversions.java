package org.jed;

import java.util.ArrayList;
import java.util.List;

/** Array/text conversion and scalar-to-threshold search, with their distinct NULL rules. */
final class ArrayConversions {
    private ArrayConversions() {}
    static ScalarPlan.Expression resolve(SqlParser.Call call, ScalarPlan.Resolver resolver) {
        if (call.name().equals("num_nulls") || call.name().equals("num_nonnulls")) return nullCount(call, resolver);
        if (call.args().stream().anyMatch(arg -> arg instanceof SqlParser.Unary unary && unary.op().equals("variadic"))) throw missing();
        if (call.name().equals("width_bucket") && call.args().size() == 2) return bucket(call, resolver);
        if (!call.name().equals("array_to_string") && !call.name().equals("string_to_array")) return null;
        if (call.args().size() < 2 || call.args().size() > 3) throw missing();
        boolean join = call.name().equals("array_to_string");
        List<ScalarPlan.Expression> args = new ArrayList<>();
        for (int i = 0; i < call.args().size(); i++) args.add(resolver.resolve(call.args().get(i), join && i == 0 ? null : SqlType.TEXT));
        if (join && args.getFirst().type().kind() != SqlType.Kind.ARRAY) {
            if (args.getFirst().type() == SqlType.UNKNOWN || call.args().getFirst() instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.STRING || call.args().getFirst() instanceof SqlParser.Parameter) throw new SqlException("42804", "cannot determine polymorphic array type");
            throw missing();
        }
        for (int i = join ? 1 : 0; i < args.size(); i++) if (args.get(i).type().kind() != SqlType.Kind.TEXT) throw missing();
        SqlType result = join ? SqlType.TEXT : SqlType.array(SqlType.TEXT);
        return new ScalarPlan.Expression(result, meter -> {
            meter.operation(); SqlValue source = args.getFirst().run(meter); if (source.isNull()) return SqlValue.nullValue(result);
            SqlValue separator = args.get(1).run(meter); if (join && separator.isNull()) return SqlValue.nullValue(result);
            SqlValue nullString = args.size() == 3 ? args.get(2).run(meter) : SqlValue.nullValue(SqlType.TEXT);
            if (join) {
                meter.scalarInput(MemoryAccount.payload(source));
                List<String> parts = new ArrayList<>(); long bytes = 0;
                for (SqlValue item : source.asArray().elements()) {
                    if (item.isNull() && nullString.isNull()) continue;
                    String text;
                    if (item.isNull()) text = nullString.asText();
                    else {
                        if (item.type().kind() == SqlType.Kind.TIMESTAMPTZ && !TemporalOps.infinite(item.asTimestamp())) { meter.charge(Costs.TIMEZONE); meter.guard(); }
                        text = item.type().kind() == SqlType.Kind.BOOLEAN ? item.asBoolean() ? "t" : "f" : Temporal.renderValue(item, meter.zone());
                    }
                    if (!parts.isEmpty()) bytes = CostMeter.saturatingAdd(bytes, SqlText.encodedLength(separator.asText()));
                    bytes = CostMeter.saturatingAdd(bytes, SqlText.encodedLength(text)); parts.add(text);
                }
                if (bytes > 0x3fffffffL) throw new SqlException("54000", "array text result too large");
                meter.scalarOutput(bytes); return SqlValue.ofText(String.join(separator.asText(), parts));
            }
            String text = source.asText(), delimiter = separator.isNull() ? null : separator.asText(); List<SqlValue> values = new ArrayList<>();
            if (!text.isEmpty()) {
                if (delimiter == null) {
                    for (int at = 0; at < text.length();) { int next = at + Character.charCount(text.codePointAt(at)); values.add(piece(text.substring(at, next), nullString)); at = next; }
                } else if (delimiter.isEmpty()) values.add(piece(text, nullString));
                else {
                    int at = 0, next;
                    while ((next = text.indexOf(delimiter, at)) >= 0) { values.add(piece(text.substring(at, next), nullString)); at = next + delimiter.length(); }
                    values.add(piece(text.substring(at), nullString));
                }
            }
            return SqlValue.ofArray(result, SqlArray.vector(values));
        });
    }
    private static ScalarPlan.Expression nullCount(SqlParser.Call call, ScalarPlan.Resolver resolver) {
        if (call.args().isEmpty()) throw missing();
        boolean variadic = call.args().getLast() instanceof SqlParser.Unary unary && unary.op().equals("variadic");
        if (variadic && call.args().size() != 1) throw missing();
        List<ScalarPlan.Expression> args = new ArrayList<>();
        for (SqlParser.Expr arg : call.args()) args.add(resolver.resolve(variadic ? ((SqlParser.Unary) arg).value() : arg, null));
        if (variadic && args.getFirst().type().kind() != SqlType.Kind.ARRAY) throw SqlException.typeMismatch("VARIADIC argument must be an array");
        return new ScalarPlan.Expression(SqlType.I32, meter -> {
            meter.operation(); int count = 0; boolean nulls = call.name().equals("num_nulls");
            if (variadic) {
                SqlValue value = args.getFirst().run(meter); if (value.isNull()) return SqlValue.nullValue(SqlType.I32);
                for (SqlValue item : value.asArray().elements()) if (item.isNull() == nulls) count++;
            } else for (ScalarPlan.Expression arg : args) if (arg.run(meter).isNull() == nulls) count++;
            return SqlValue.ofInteger(SqlType.I32, count);
        });
    }
    private static SqlValue piece(String text, SqlValue nullString) { return !nullString.isNull() && text.equals(nullString.asText()) ? SqlValue.nullValue(SqlType.TEXT) : SqlValue.ofText(text); }
    private static ScalarPlan.Expression bucket(SqlParser.Call call, ScalarPlan.Resolver resolver) {
        ScalarPlan.Expression left = resolver.resolve(call.args().get(0), null), right = resolver.resolve(call.args().get(1), null);
        if (left.type() == SqlType.UNKNOWN && right.type() == SqlType.UNKNOWN) throw new SqlException("42P18", "cannot determine threshold type");
        if (right.type() == SqlType.UNKNOWN || call.args().get(1) instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.STRING) right = resolver.resolve(call.args().get(1), SqlType.array(left.type()));
        if (right.type().kind() != SqlType.Kind.ARRAY || left.type().kind() == SqlType.Kind.ARRAY) throw missing();
        left = resolver.resolve(call.args().get(0), right.type().element());
        try { ScalarPlan.compatible(left.type(), right.type().element()); } catch (SqlException failure) { throw missing(); }
        if (left.type() != SqlType.UNKNOWN) right = resolver.resolve(call.args().get(1), SqlType.array(left.type()));
        try { ScalarPlan.compatible(left.type(), right.type().element()); } catch (SqlException failure) { throw missing(); }
        String collationName = CollationDerivation.comparison(left.collation(), right.collation()).require();
        Collation collation = resolver.meter() == null ? Collation.C : resolver.meter().environment().collation(collationName);
        ScalarPlan.Expression operand = left, thresholds = right;
        return new ScalarPlan.Expression(SqlType.I32, meter -> {
            meter.operation(); SqlValue value = operand.run(meter); if (value.isNull()) return SqlValue.nullValue(SqlType.I32);
            SqlValue source = thresholds.run(meter); if (source.isNull()) return SqlValue.nullValue(SqlType.I32);
            SqlArray array = source.asArray(); if (array.dimensions() > 1) throw SqlArray.shape();
            for (SqlValue item : array.elements()) if (item.isNull()) throw new SqlException("22004", "thresholds cannot contain NULLs");
            int low = 0, high = array.size();
            if (value.type().isFloat() && Double.isNaN(value.asFloat())) return SqlValue.ofInteger(SqlType.I32, high);
            while (low < high) { int middle = low + (high - low) / 2; meter.operation(); if (CollationDerivation.compare(value, array.element(middle), meter, collation) < 0) high = middle; else low = middle + 1; }
            return SqlValue.ofInteger(SqlType.I32, low);
        });
    }
    private static SqlException missing() { return new SqlException("42883", "array conversion function does not exist for argument types"); }
}
