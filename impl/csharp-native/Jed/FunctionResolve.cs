namespace Jed;

internal static class FunctionResolve
{
    internal static Resolved Call(Expression expression)
    {
        if (JsonResolve.TryCall(expression, out var jsonCall)) return jsonCall;
        if (OpenResolve.TryCall(expression, out var openCall)) return openCall;
        var name = expression.Text;
        if (name == "date_trunc")
        {
            if (expression.Args.Length is not (2 or 3)) throw new SqlException(SqlStates.UndefinedFunction, "invalid date_trunc arity");
            var unit = Resolve.Expr(expression.Args[0], SqlType.Text);
            var value = Resolve.Expr(expression.Args[1]);
            if (unit.Type != SqlType.Text || value.Type.Kind is not (SqlKind.Timestamp or SqlKind.Timestamptz or SqlKind.Interval) || expression.Args.Length == 3 && value.Type != SqlType.Timestamptz)
                throw new SqlException(SqlStates.UndefinedFunction, "invalid date_trunc operands");
            var operands = expression.Args.Length == 2 ? new[] { unit, value } : new[] { unit, value, Resolve.Expr(expression.Args[2], SqlType.Text) };
            var fact = new FunctionFact(name, [], value.Type.Name(), "propagates", 1, false, [], []);
            return new("call", value.Type, default, operands, name, Function: fact);
        }
        if (name is "mod" or "div" or "gcd" or "lcm" or "width_bucket")
        {
            var arity = name == "width_bucket" ? 4 : 2;
            if (expression.Args.Length != arity) throw new SqlException(SqlStates.UndefinedFunction, $"invalid arity for {name}");
            var operands = Resolve.Unify(expression.Args.Take(name == "width_bucket" ? 3 : 2).ToArray());
            if (!operands[0].Type.IsNumeric()) throw Errors.Type("numeric operands required");
            if (name == "width_bucket")
            {
                var count = Resolve.Expr(expression.Args[3]);
                if (!count.Type.IsInteger() && count.Type != SqlType.Unknown) throw Errors.Type("bucket count requires integer");
                operands = [.. operands, count];
            }
            var specialResult = name == "width_bucket" ? SqlType.I32 : name == "div" ? SqlType.Decimal : operands[0].Type;
            var fact = new FunctionFact(name, [], specialResult.Name(), "propagates", 1, false, [], []);
            return new("call", specialResult, default, operands, name, Function: fact);
        }
        if (name is "coalesce" or "greatest" or "least" or "nullif")
        {
            if (name == "nullif" && expression.Args.Length != 2) throw Errors.Syntax("NULLIF requires two arguments");
            if (name == "nullif")
            {
                var (left, right) = Resolve.Pair(expression.Args[0], expression.Args[1]);
                _ = Resolve.Common(left.Type, right.Type);
                return new(name, left.Type, default, [left, right]);
            }
            var unified = Resolve.Unify(expression.Args);
            return new(name, unified[0].Type, default, unified);
        }
        // A nonliteral argument may itself contain overloaded calls. Resolve it
        // once, then adapt its literal leaves after choosing the signature.
        var initial = new Dictionary<Expression, Resolved>(ReferenceEqualityComparer.Instance);
        Resolved Initial(Expression argument)
        {
            if (!initial.TryGetValue(argument, out var value))
            {
                value = Resolve.Expr(argument);
                initial.Add(argument, value);
            }
            return value;
        }
        var candidates = new List<(FunctionFact Fact, Expression[] Args, int Score)>();
        foreach (var fact in FunctionCatalog.Facts.Where(f => f.Name == name))
        {
            var args = Arrange(fact, expression.Args);
            if (args is null) continue;
            var score = 0; var valid = true;
            for (var i = 0; i < args.Length; i++)
            {
                var family = fact.Families[Math.Min(i, fact.Families.Length - 1)];
                if (args[i].Op is "null" or "text" or "parameter" || family == "any") continue;
                var type = Initial(args[i]).Type;
                if (Matches(type, family)) continue;
                if (type.IsInteger() && family == "decimal") { score++; continue; }
                if (args[i].Op is "integer" or "decimal" && family == "float") { score += 2; continue; }
                valid = false; break;
            }
            if (valid) candidates.Add((fact, args, score));
        }
        if (candidates.Count == 0) throw new SqlException(SqlStates.UndefinedFunction, $"no matching function: {name}");
        var bestScore = candidates.Min(c => c.Score);
        candidates.RemoveAll(c => c.Score != bestScore);
        // Unknown literals prefer text if any candidate admits it. Otherwise the
        // surviving signature must determine one family, without a host default.
        for (var i = 0; i < expression.Args.Length && candidates.Count > 1; i++)
        {
            if (expression.Args[i].Op is not ("null" or "text" or "parameter")) continue;
            var position = i;
            if (candidates.Any(c => c.Fact.Families[Math.Min(position, c.Fact.Families.Length - 1)] == "text"))
                candidates.RemoveAll(c => c.Fact.Families[Math.Min(position, c.Fact.Families.Length - 1)] != "text");
        }
        if (candidates.Count != 1) throw new SqlException(SqlStates.AmbiguousFunction, $"ambiguous function: {name}");
        var chosen = candidates[0];
        var resolved = new Resolved[chosen.Args.Length];
        for (var i = 0; i < resolved.Length; i++)
        {
            var family = chosen.Fact.Families[Math.Min(i, chosen.Fact.Families.Length - 1)];
            var arg = chosen.Args[i];
            var hint = FamilyType(family);
            resolved[i] = arg.Op is "null" or "text" or "parameter" or "integer" or "decimal"
                ? Resolve.Expr(arg, hint)
                : Resolve.Adapt(arg, Initial(arg), hint);
            if (family == "decimal" && resolved[i].Type.IsInteger())
                resolved[i] = new("promote", SqlType.Decimal, default, [resolved[i]]);
        }
        var result = chosen.Fact.Result == "promoted" ? resolved[0].Type : ResultType(chosen.Fact.Result);
        if (result == SqlType.F64)
            for (var i = 0; i < resolved.Length; i++)
                if (resolved[i].Type == SqlType.F32) resolved[i] = new("float_widen", SqlType.F64, default, [resolved[i]]);
        return new("call", result, default, resolved, name, Function: chosen.Fact);
    }

    private static Expression[]? Arrange(FunctionFact fact, Expression[] supplied)
    {
        if (fact.Variadic)
        {
            if (supplied.Length < fact.Families.Length || supplied.Any(e => e.Op == "named")) return null;
            return supplied;
        }
        if (supplied.Length > fact.Families.Length) return null;
        var result = new Expression?[fact.Families.Length];
        var named = false;
        for (var i = 0; i < supplied.Length; i++)
        {
            var arg = supplied[i]; var index = i;
            if (arg.Op == "named")
            {
                named = true; index = Array.IndexOf(fact.ArgumentNames, arg.Text);
                if (index < 0) return null;
                arg = arg.Args[0];
            }
            else if (named) throw Errors.Syntax("positional argument follows named argument");
            if (result[index] is not null) throw Errors.Syntax("duplicate argument");
            result[index] = arg;
        }
        var firstDefault = result.Length - fact.Defaults.Length;
        for (var i = 0; i < result.Length; i++)
        {
            if (result[i] is not null) continue;
            if (i < firstDefault) return null;
            result[i] = new("integer", fact.Defaults[i - firstDefault], []);
        }
        return result.Select(e => e!).ToArray();
    }

    private static bool Matches(SqlType type, string family) => family switch
    {
        "any" => true,
        "integer" => type.IsInteger(),
        "float" => type.IsFloat(),
        "array" => type.Kind == SqlKind.Array,
        "range" => type.Kind == SqlKind.Range,
        "composite" => type.Kind == SqlKind.Composite,
        _ => type.Name() == family
    };
    private static SqlType FamilyType(string family) => family switch
    {
        "integer" => SqlType.I64, "float" => SqlType.F64,
        "any" or "array" or "range" or "composite" => SqlType.Unknown,
        _ => SqlTypes.Parse(family)
    };
    private static SqlType ResultType(string result)
    {
        if (ScalarCatalog.KnownNames.Contains(result, StringComparer.Ordinal)) return SqlTypes.Parse(result);
        throw Errors.Unsupported($"function result {result}");
    }
}
