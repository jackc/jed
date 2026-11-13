namespace Jed;

internal static class JsonPathFunctions
{
    internal static SqlValue Operator(string operation, SqlValue target, SqlValue path, Meter meter)
    {
        if (target.IsNull || path.IsNull) return SqlValue.Null(SqlType.Boolean);
        var outcome = new JsonPathRun(path.AsJsonpath(), target.AsJsonb(), SqlJsonNode.Object([]), meter, 0).Run(true, operation == "@?");
        if (operation == "@?") return outcome.Failed ? SqlValue.Null(SqlType.Boolean) : SqlValue.Boolean(outcome.Items.Count != 0);
        return outcome.Items.Count == 1 && outcome.Items[0].Kind is SqlJsonKind.False or SqlJsonKind.True ? SqlValue.Boolean(outcome.Items[0].Kind == SqlJsonKind.True) : SqlValue.Null(SqlType.Boolean);
    }
    internal static bool TryCall(string name, SqlValue[] args, Meter meter, out SqlValue result)
    {
        result = default;
        if (!name.StartsWith("jsonb_path_", StringComparison.Ordinal)) return false;
        if (name.EndsWith("_tz", StringComparison.Ordinal)) name = name[..^3];
        var exists = name == "jsonb_path_exists";
        var outcome = new JsonPathRun(args[1].AsJsonpath(), args[0].AsJsonb(), args[2].AsJsonb(), meter).Run(args[3].AsBoolean(), exists);
        if (exists) result = outcome.Failed ? SqlValue.Null(SqlType.Boolean) : SqlValue.Boolean(outcome.Items.Count != 0);
        else if (name == "jsonb_path_match")
        {
            if (outcome.Items.Count == 1 && outcome.Items[0].Kind is SqlJsonKind.True or SqlJsonKind.False) result = SqlValue.Boolean(outcome.Items[0].Kind == SqlJsonKind.True);
            else if (outcome.Items.Count == 1 && outcome.Items[0].Kind == SqlJsonKind.Null || args[3].AsBoolean()) result = SqlValue.Null(SqlType.Boolean);
            else throw new SqlException("22038", "single boolean result is expected");
        }
        else if (name == "jsonb_path_query_first") result = outcome.Items.Count == 0 ? SqlValue.Null(SqlType.Jsonb) : SqlValue.Jsonb(outcome.Items[0]);
        else if (name == "jsonb_path_query_array") result = SqlValue.Jsonb(SqlJsonNode.Array(outcome.Items));
        else return false;
        return true;
    }
    internal static bool TryResolve(Expression expression, out Resolved result)
    {
        result = null!; var name = expression.Text;
        var basic = name.EndsWith("_tz", StringComparison.Ordinal) ? name[..^3] : name;
        if (basic is not ("jsonb_path_exists" or "jsonb_path_match" or "jsonb_path_query_first" or "jsonb_path_query_array")) return false;
        var names = new[] { "target", "path", "vars", "silent" }; var arguments = new Expression?[4]; var named = false;
        if (expression.Args.Length > 4) throw new SqlException("42883", "no matching JSON path function");
        for (var i = 0; i < expression.Args.Length; i++)
        {
            var argument = expression.Args[i]; var index = i;
            if (argument.Op == "named") { named = true; index = System.Array.IndexOf(names, argument.Text); if (index < 0) throw new SqlException("42883", "unknown argument"); argument = argument.Args[0]; }
            else if (named) throw Errors.Syntax("positional argument follows named argument");
            if (arguments[index] is not null) throw Errors.Syntax("duplicate argument"); arguments[index] = argument;
        }
        if (arguments[0] is null || arguments[1] is null) throw new SqlException("42883", "missing argument");
        arguments[2] ??= new("text", "{}", []); arguments[3] ??= new("boolean", "false", []);
        var types = new[] { SqlType.Jsonb, SqlType.Jsonpath, SqlType.Jsonb, SqlType.Boolean };
        var values = arguments.Select((argument, index) => Resolve.Expr(argument!, types[index])).ToArray();
        if (values.Where((value, index) => value.Type != types[index]).Any()) throw new SqlException("42883", "invalid JSON path argument type");
        var type = basic is "jsonb_path_exists" or "jsonb_path_match" ? SqlType.Boolean : SqlType.Jsonb;
        result = new("call", type, default, values, name, Function: new(name, [], type.Name(), "propagates", 1, false, [], [])); return true;
    }
}
