namespace Jed;

internal static class JsonResolve
{
    internal static SqlType? OperandHint(string? operation, SqlType concrete, bool literalLeft = false)
    {
        if (literalLeft && concrete == SqlType.Jsonpath && operation is "@?" or "@@") return SqlType.Jsonb;
        if (concrete.Kind is not (SqlKind.Json or SqlKind.Jsonb)) return null;
        if (literalLeft) return concrete;
        return operation switch
        {
            "@?" or "@@" => SqlType.Jsonpath,
            "->" or "->>" or "?" or "-" => SqlType.Text,
            "#>" or "#>>" or "#-" or "?|" or "?&" => SqlType.Array(SqlType.Text),
            _ => concrete
        };
    }
    internal static bool TryBinary(Expression expression, Resolved left, Resolved right, out Resolved result)
    {
        result = null!; var op = expression.Op;
        if (op is "=" or "<>" or "!=" or "<" or "<=" or ">" or ">=" or "distinct" or "not_distinct")
        {
            if (left.Type.Kind is SqlKind.Json or SqlKind.Jsonpath || right.Type.Kind is SqlKind.Json or SqlKind.Jsonpath) throw Undefined();
            return false;
        }
        if (op is "@?" or "@@")
        {
            if (left.Type != SqlType.Jsonb || right.Type != SqlType.Jsonpath) throw Undefined();
            result = new("json_path_binary", SqlType.Boolean, default, [left, right], op); return true;
        }
        var special = op is "->" or "->>" or "#>" or "#>>" or "#-" or "?" or "?|" or "?&";
        if (!special && left.Type != SqlType.Jsonb && right.Type != SqlType.Jsonb) return false;
        var type = SqlType.Boolean; var valid = false;
        if (op is "->" or "->>" or "#>" or "#>>")
        {
            valid = left.Type.Kind is SqlKind.Json or SqlKind.Jsonb && (op[0] == '#' ? right.Type == SqlType.Array(SqlType.Text) : right.Type == SqlType.Text || right.Type.IsInteger());
            type = op.EndsWith(">>", StringComparison.Ordinal) ? SqlType.Text : left.Type;
        }
        else if (op is "@>" or "<@" or "||") { valid = left.Type == SqlType.Jsonb && right.Type == SqlType.Jsonb; if (op == "||") type = SqlType.Jsonb; }
        else if (op == "?") valid = left.Type == SqlType.Jsonb && right.Type == SqlType.Text;
        else if (op is "?|" or "?&" or "#-") { valid = left.Type == SqlType.Jsonb && right.Type == SqlType.Array(SqlType.Text); if (op == "#-") type = SqlType.Jsonb; }
        else if (op == "-") { valid = left.Type == SqlType.Jsonb && (right.Type == SqlType.Text || right.Type.IsInteger() || right.Type == SqlType.Array(SqlType.Text)); type = SqlType.Jsonb; }
        if (!valid) throw Undefined();
        result = new("json_binary", type, default, [left, right], op); return true;
    }
    internal static bool TryCall(Expression expression, out Resolved result)
    {
        result = null!; var name = expression.Text;
        if (JsonPathFunctions.TryResolve(expression, out result)) return true;
        if (name is "json_build_array" or "jsonb_build_array" or "json_build_object" or "jsonb_build_object")
        {
            if (expression.Args.Any(arg => arg.Op == "variadic"))
            {
                if (expression.Args.Length != 1) throw Undefined();
                var array = Resolve.Expr(expression.Args[0].Args[0]);
                if (array.Type.Kind != SqlKind.Array) throw new SqlException("42804", "VARIADIC argument must be an array");
                result = Call(name, name.StartsWith("jsonb", StringComparison.Ordinal) ? SqlType.Jsonb : SqlType.Json, [array], "propagates");
                result = result with { Function = result.Function! with { Variadic = true } }; return true;
            }
            var values = expression.Args.Select(arg => Resolve.Expr(arg)).ToArray();
            result = Call(name, name.StartsWith("jsonb", StringComparison.Ordinal) ? SqlType.Jsonb : SqlType.Json, values, "none"); return true;
        }
        if (name is "array_to_json" or "row_to_json")
        {
            if (expression.Args.Length is not (1 or 2)) throw Undefined();
            if (name == "row_to_json" && expression.Args[0].Op == "text") throw Errors.Unsupported("input of anonymous composite types");
            var value = Resolve.Expr(expression.Args[0]);
            if (value.Type != SqlType.Unknown && value.Type.Kind != (name == "array_to_json" ? SqlKind.Array : SqlKind.Composite)) throw Undefined();
            var values = new List<Resolved> { value };
            if (expression.Args.Length == 2)
            {
                var flagSource = expression.Args[1];
                var flag = flagSource.Op == "text" ? Resolve.Expr(new("cast", "boolean", [flagSource], Type: new(SqlType.Boolean))) : Resolve.Expr(flagSource, SqlType.Boolean); if (flag.Type != SqlType.Boolean) throw Undefined(); values.Add(flag);
            }
            result = Call(name, SqlType.Json, values.ToArray(), "propagates"); return true;
        }
        if (name is "json_object" or "jsonb_object")
        {
            if (expression.Args.Length is not (1 or 2)) throw Undefined();
            var values = expression.Args.Select(arg => Resolve.Expr(arg, SqlType.Array(SqlType.Text))).ToArray();
            if (values.Any(value => value.Type != SqlType.Array(SqlType.Text))) throw Undefined();
            result = Call(name, name == "jsonb_object" ? SqlType.Jsonb : SqlType.Json, values, "propagates"); return true;
        }
        if (name is "jsonb_set" or "jsonb_insert" or "jsonb_set_lax")
        {
            var names = name == "jsonb_set_lax" ? new[] { "jsonb_in", "path", "replacement", "create_if_missing", "null_value_treatment" } : new[] { "jsonb_in", "path", "replacement", name == "jsonb_insert" ? "insert_after" : "create_if_missing" };
            if (expression.Args.Length > names.Length) throw Undefined();
            var arguments = new Expression?[names.Length]; var named = false;
            for (var i = 0; i < expression.Args.Length; i++)
            {
                var value = expression.Args[i]; var index = i;
                if (value.Op == "named") { named = true; index = System.Array.IndexOf(names, value.Text); if (index < 0) throw Undefined(); value = value.Args[0]; }
                else if (named) throw Errors.Syntax("positional argument follows named argument");
                if (arguments[index] is not null) throw Errors.Syntax("duplicate argument"); arguments[index] = value;
            }
            if (arguments.Take(3).Any(argument => argument is null)) throw Undefined();
            arguments[3] ??= new("boolean", name == "jsonb_insert" ? "false" : "true", []);
            if (arguments.Length == 5) arguments[4] ??= new("text", "use_json_null", []);
            var types = new[] { SqlType.Jsonb, SqlType.Array(SqlType.Text), SqlType.Jsonb, SqlType.Boolean, SqlType.Text };
            var values = arguments.Select((argument, index) => Resolve.Expr(argument!, types[index])).ToArray();
            if (values.Where((value, index) => value.Type != types[index]).Any()) throw Undefined();
            result = Call(name, SqlType.Jsonb, values, name == "jsonb_set_lax" ? "none" : "propagates"); return true;
        }
        return false;
    }
    private static Resolved Call(string name, SqlType type, Resolved[] args, string nulls) => new("call", type, default, args, name, Function: new(name, [], type.Name(), nulls, 1, false, [], []));
    private static SqlException Undefined() => new("42883", "no matching JSON function or operator");
}
