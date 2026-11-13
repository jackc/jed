namespace Jed;

internal static class VariadicResolve
{
    internal static bool TryCall(Expression expression, out Resolved result)
    {
        result = null!; var name = expression.Text;
        if (name is not ("num_nulls" or "num_nonnulls" or "concat" or "concat_ws" or "format")) return false;
        var fixedCount = name is "concat_ws" or "format" ? 1 : 0;
        var minimum = name == "format" ? 1 : fixedCount + 1;
        var arrayForm = expression.Args.Any(argument => argument.Op == "variadic");
        if (expression.Args.Length < minimum || expression.Args.Any(argument => argument.Op == "named") || arrayForm &&
            (expression.Args.Length != fixedCount + 1 || expression.Args[^1].Op != "variadic" || expression.Args.Take(fixedCount).Any(argument => argument.Op == "variadic")))
            throw new SqlException(SqlStates.UndefinedFunction, "invalid variadic function arguments");
        var args = new Resolved[expression.Args.Length];
        for (var i = 0; i < args.Length; i++)
        {
            var syntax = expression.Args[i]; args[i] = Resolve.Expr(syntax.Op == "variadic" ? syntax.Args[0] : syntax, i < fixedCount ? SqlType.Text : SqlType.Unknown);
            if (i < fixedCount && args[i].Type != SqlType.Text) throw new SqlException(SqlStates.UndefinedFunction, "variadic fixed argument requires text");
        }
        if (arrayForm && args[^1].Type.Kind != SqlKind.Array) throw Errors.Type("VARIADIC requires an array");
        var type = name is "num_nulls" or "num_nonnulls" ? SqlType.I32 : SqlType.Text;
        var fact = FunctionCatalog.Facts.Single(function => function.Name == name);
        result = new("call", type, default, args, name, Function: fact, ArrayArguments: arrayForm); return true;
    }
}
