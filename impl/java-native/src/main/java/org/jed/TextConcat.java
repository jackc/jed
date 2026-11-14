package org.jed;

/** Resolves text's any-non-array concatenation overload before scalar literal coercion. */
final class TextConcat {
    private TextConcat() {}
    static ScalarPlan.Expression resolve(SqlParser.Binary syntax, ScalarPlan.Resolver resolver) {
        if (!syntax.op().equals("||")) return null;
        ScalarPlan.Expression a = resolver.resolve(syntax.left(), null), b = resolver.resolve(syntax.right(), null);
        if (a.type().kind() == SqlType.Kind.ARRAY || b.type().kind() == SqlType.Kind.ARRAY) return null;
        boolean typedText = a.type().kind() == SqlType.Kind.TEXT && !bare(syntax.left()) || b.type().kind() == SqlType.Kind.TEXT && !bare(syntax.right());
        if (!typedText && (a.type() == SqlType.JSONB || b.type() == SqlType.JSONB || a.type() == SqlType.BYTEA || b.type() == SqlType.BYTEA)) return null;
        boolean textLike = a.type().kind() == SqlType.Kind.TEXT || b.type().kind() == SqlType.Kind.TEXT || a.type() == SqlType.UNKNOWN || b.type() == SqlType.UNKNOWN || syntax.left() instanceof SqlParser.Parameter || syntax.right() instanceof SqlParser.Parameter;
        if (!textLike) return null;
        ScalarPlan.Expression left = argument(syntax.left(), a, resolver), right = argument(syntax.right(), b, resolver);
        return new ScalarPlan.Expression(SqlType.TEXT, meter -> {
            meter.operation(); SqlValue x = left.run(meter); if (x.isNull()) return SqlValue.nullValue(SqlType.TEXT);
            SqlValue y = right.run(meter); if (y.isNull()) return SqlValue.nullValue(SqlType.TEXT);
            String output = x.asText() + y.asText();
            if (!left.constant() || !right.constant()) meter.scalarInput(SqlText.encodedLength(output));
            return SqlValue.ofText(output);
        }, left.constant() && right.constant());
    }
    private static ScalarPlan.Expression argument(SqlParser.Expr syntax, ScalarPlan.Expression input, ScalarPlan.Resolver resolver) {
        if (input.type() == SqlType.UNKNOWN || syntax instanceof SqlParser.Parameter) return resolver.resolve(syntax, SqlType.TEXT);
        if (input.type().kind() == SqlType.Kind.TEXT) return input;
        if (input.type().isComposite() || input.type().kind() == SqlType.Kind.RANGE) throw new SqlException("0A000", "text concatenation of composites or ranges is not supported");
        return new ScalarPlan.Expression(SqlType.TEXT, meter -> { meter.operation(); return SqlCasts.cast(input.run(meter), SqlType.TEXT, meter); }, input.constant());
    }
    private static boolean bare(SqlParser.Expr syntax) { return syntax instanceof SqlParser.Literal || syntax instanceof SqlParser.Parameter; }
}
