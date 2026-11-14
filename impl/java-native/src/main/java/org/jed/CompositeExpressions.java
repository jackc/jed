package org.jed;

import java.util.ArrayList;
import java.util.List;

/** Resolved row constructors preserve an immutable static field list. */
final class CompositeExpressions {
    private CompositeExpressions() {}
    static ScalarPlan.Expression constructor(SqlParser.RowConstructor row, SqlType context, ScalarPlan.Resolver resolver) { return constructor(row, context, resolver, false); }
    static ScalarPlan.Expression constructor(SqlParser.RowConstructor row, SqlType context, ScalarPlan.Resolver resolver, boolean explicit) {
        boolean named = context != null && context.isComposite();
        if (named && context.fields().size() != row.values().size()) throw CompositeValues.cannotCast();
        var expressions = new ArrayList<ScalarPlan.Expression>(); var fields = new ArrayList<SqlType.Field>();
        for (int i = 0; i < row.values().size(); i++) {
            SqlType wanted = named ? context.fields().get(i).type() : null;
            ScalarPlan.Expression expression;
            try { expression = wanted == null ? resolver.resolve(row.values().get(i), null) : explicit ? resolver.resolve(row.values().get(i), wanted) : Engine.assignment(resolver, row.values().get(i), wanted); }
            catch (SqlException failure) { if (failure.sqlState().equals("42804") || failure.sqlState().equals("0A000")) throw CompositeValues.cannotCast(); throw failure; }
            if (expression.type() == SqlType.UNKNOWN) expression = ScalarPlan.constant(SqlValue.nullValue(wanted == null ? SqlType.TEXT : wanted));
            if (wanted != null && !SqlCasts.allowed(expression.type(), wanted)) throw CompositeValues.cannotCast();
            expressions.add(expression); fields.add(new SqlType.Field("f" + (i + 1), wanted == null ? expression.type() : wanted));
        }
        SqlType type = named ? context : SqlType.record(fields);
        return new ScalarPlan.Expression(type, meter -> {
            meter.operation(); var values = new ArrayList<SqlValue>(expressions.size());
            for (int i = 0; i < expressions.size(); i++) values.add(SqlCasts.cast(expressions.get(i).run(meter), type.fields().get(i).type(), meter));
            return SqlValue.ofComposite(type, values);
        }, expressions.stream().allMatch(ScalarPlan.Expression::constant));
    }
    static ScalarPlan.Expression field(SqlParser.Field access, ScalarPlan.Resolver resolver) {
        ScalarPlan.Expression base = resolver.resolve(access.base(), null);
        if (!base.type().isComposite()) throw new SqlException("42809", "field selection requires composite type");
        for (int i = 0; i < base.type().fields().size(); i++) if (base.type().fields().get(i).name().equals(access.name())) {
            int index = i; SqlType type = base.type().fields().get(i).type();
            return new ScalarPlan.Expression(type, meter -> { meter.operation(); SqlValue value = base.run(meter); return value.isNull() ? SqlValue.nullValue(type) : value.asComposite().get(index); }, base.constant());
        }
        throw new SqlException("42703", "field does not exist: " + access.name());
    }
    static SqlParser.Expr comparisonNulls(SqlParser.Expr syntax, SqlType other) {
        if (!(syntax instanceof SqlParser.RowConstructor row) || !other.isComposite() || row.values().size() != other.fields().size()) return syntax;
        var values = new ArrayList<SqlParser.Expr>();
        for (int i = 0; i < row.values().size(); i++) {
            SqlParser.Expr value = row.values().get(i); SqlType type = other.fields().get(i).type();
            if (value instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.WORD && literal.text().equals("null")) value = new SqlParser.StoredValue(SqlValue.nullValue(type));
            else value = comparisonNulls(value, type);
            values.add(value);
        }
        return new SqlParser.RowConstructor(values);
    }
    static void compatible(SqlType a, SqlType b) {
        if (!a.isComposite() || !b.isComposite() || a.fields().size() != b.fields().size()) throw SqlException.typeMismatch("incompatible record types");
        if (a.kind() == SqlType.Kind.COMPOSITE && b.kind() == SqlType.Kind.COMPOSITE && !a.name().equals(b.name())) throw SqlException.typeMismatch("different named composite types");
        for (int i = 0; i < a.fields().size(); i++) {
            SqlType x = a.fields().get(i).type(), y = b.fields().get(i).type();
            try { ScalarPlan.compatible(x, y); } catch (SqlException failure) { throw SqlException.typeMismatch("incompatible record fields"); }
        }
    }
    static SqlType common(SqlType a, SqlType b) {
        compatible(a, b);
        if (a.kind() != SqlType.Kind.RECORD || b.kind() != SqlType.Kind.RECORD) { if (!a.equals(b)) throw SqlException.typeMismatch("different named composite types"); return a; }
        var fields = new ArrayList<SqlType.Field>();
        for (int i = 0; i < a.fields().size(); i++) fields.add(new SqlType.Field(a.fields().get(i).name(), ScalarPlan.common(a.fields().get(i).type(), b.fields().get(i).type())));
        return SqlType.record(fields);
    }
}
