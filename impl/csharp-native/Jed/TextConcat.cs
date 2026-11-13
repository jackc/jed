namespace Jed;

internal static class TextConcat
{
    internal static Resolved Resolve(Expression expression)
    {
        var leftSyntax = expression.Args[0]; var rightSyntax = expression.Args[1];
        var left = Jed.Resolve.Expr(leftSyntax);
        var right = Jed.Resolve.Expr(rightSyntax);
        if (left.Type.Kind == SqlKind.Array || right.Type.Kind == SqlKind.Array)
        {
            var type = left.Type.Kind == SqlKind.Array ? left.Type : right.Type;
            if (leftSyntax.Op == "array" && right.Type.Kind == SqlKind.Array && rightSyntax.Op != "array")
                left = Jed.Resolve.Adapt(leftSyntax, left, type = right.Type);
            if (rightSyntax.Op == "array" && left.Type.Kind == SqlKind.Array && leftSyntax.Op != "array")
                right = Jed.Resolve.Adapt(rightSyntax, right, type = left.Type);
            if (leftSyntax.Op is "text" or "null" or "parameter") left = Jed.Resolve.Expr(leftSyntax, type);
            if (rightSyntax.Op is "text" or "null" or "parameter") right = Jed.Resolve.Expr(rightSyntax, type);
            if (leftSyntax.Op is "integer" or "decimal") left = Jed.Resolve.Expr(leftSyntax, type.Element);
            if (rightSyntax.Op is "integer" or "decimal") right = Jed.Resolve.Expr(rightSyntax, type.Element);
            try { if (OpenOperators.TryResolve(expression, left, right, out var array)) return array; }
            catch (SqlException error) when (error.SqlState == SqlStates.DatatypeMismatch) { throw Missing(); }
            throw Missing();
        }
        static bool TypedText(Expression syntax, Resolved value) => syntax.Op is not ("text" or "null" or "parameter") && value.Type == SqlType.Text;
        var special = left.Type == SqlType.Jsonb || right.Type == SqlType.Jsonb ? SqlType.Jsonb : left.Type == SqlType.Bytea || right.Type == SqlType.Bytea ? SqlType.Bytea : SqlType.Unknown;
        if (special != SqlType.Unknown && !TypedText(leftSyntax, left) && !TypedText(rightSyntax, right))
        {
            left = Jed.Resolve.Adapt(leftSyntax, left, special); right = Jed.Resolve.Adapt(rightSyntax, right, special);
            if (left.Type != special || right.Type != special) throw Missing();
            return special == SqlType.Jsonb ? new("json_binary", special, default, [left, right], "||") : new("||", special, default, [left, right]);
        }
        if (left.Type != SqlType.Text && right.Type != SqlType.Text && left.Type != SqlType.Unknown && right.Type != SqlType.Unknown) throw Missing();
        Resolved Text(Expression syntax, Resolved value)
        {
            if (syntax.Op is "text" or "null" or "parameter") return Jed.Resolve.Expr(syntax, SqlType.Text);
            if (value.Type == SqlType.Text) return value;
            if (value.Type.Kind is SqlKind.Composite or SqlKind.Range) throw Errors.Unsupported("composite or range text concatenation");
            return new("cast", SqlType.Text, default, [value], Target: new(SqlType.Text));
        }
        return new("||", SqlType.Text, default, [Text(leftSyntax, left), Text(rightSyntax, right)]);
    }
    private static SqlException Missing() => new(SqlStates.UndefinedFunction, "no matching concatenation operator");
}
