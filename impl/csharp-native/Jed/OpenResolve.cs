namespace Jed;

internal static class OpenResolve
{
    internal static bool TryCall(Expression expression, out Resolved result)
    {
        result = null!;
        var range = RangeCatalog.Facts.FirstOrDefault(f => f.Name == expression.Text || f.Aliases.Contains(expression.Text, StringComparer.Ordinal));
        if (range is not null)
        {
            if (expression.Args.Length is not (2 or 3)) throw Undefined();
            var type = SqlType.Range(range.Element);
            var args = expression.Args.Select((arg, index) => Resolve.Expr(arg, index < 2 ? range.Element : SqlType.Text)).ToArray();
            for (var i = 0; i < 2; i++)
            {
                if (args[i].Type != range.Element && args[i].Type != SqlType.Unknown && !(args[i].Type.IsInteger() && (range.Element.IsInteger() || range.Element == SqlType.Decimal))) throw Undefined();
                if (args[i].Type != range.Element) args[i] = new("promote", range.Element, default, [args[i]]);
            }
            if (args.Length == 3 && args[2].Type != SqlType.Text) throw Undefined();
            result = Call(range.Name, type, args, "none"); return true;
        }
        if (expression.Text == "width_bucket" && expression.Args.Length == 2)
        {
            var thresholds = Resolve.Expr(expression.Args[1]);
            if (thresholds.Type.Kind != SqlKind.Array) throw Undefined();
            var value = Resolve.Expr(expression.Args[0], thresholds.Type.Element);
            try { _ = Resolve.Common(value.Type, thresholds.Type.Element!); } catch (SqlException) { throw Undefined(); }
            result = Call("width_bucket_array", SqlType.I32, [value, thresholds], "propagates"); return true;
        }
        var facts = FunctionCatalog.Facts.Where(f => f.Name == expression.Text && f.Families.Length == expression.Args.Length && f.Families.Any(family => family is "anyarray" or "anycompatiblearray" or "anyrange")).ToArray();
        if (facts.Length == 0) return false;
        if (expression.Text == "array_to_string" && expression.Args[0].Op is "null" or "text")
            throw Errors.Type("cannot determine polymorphic element type");
        var fact = facts[0]; var resolved = expression.Args.Select(arg => Resolve.Expr(arg)).ToArray();
        // Resolve this shared overload here as well: a fallback would resolve a nested call twice.
        if (expression.Text is "lower" or "upper" && resolved[0].Type.Kind != SqlKind.Range)
        {
            if (resolved[0].Type.Kind is not (SqlKind.Text or SqlKind.Unknown)) throw Undefined();
            result = Call(expression.Text, SqlType.Text, [Resolve.Adapt(expression.Args[0], resolved[0], SqlType.Text)], "propagates"); return true;
        }
        SqlType? hint = null;
        for (var i = 0; i < resolved.Length; i++)
            if (fact.Families[i] is "anyarray" or "anycompatiblearray" or "anyrange" && resolved[i].Type.Kind is SqlKind.Array or SqlKind.Range) { hint = resolved[i].Type.Element; break; }
        if (hint is not null)
            for (var i = 0; i < resolved.Length; i++)
            {
                var family = fact.Families[i];
                if (family is "anyarray" or "anycompatiblearray")
                {
                    if (resolved[i].Type.Kind == SqlKind.Array && resolved[i].Type.Element == SqlType.Text && hint != SqlType.Text) throw Undefined();
                    resolved[i] = Resolve.Adapt(expression.Args[i], resolved[i], SqlType.Array(hint));
                }
                else if (family is "anyelement" or "anycompatible") resolved[i] = Resolve.Adapt(expression.Args[i], resolved[i], hint);
            }
        var element = SqlType.Unknown;
        for (var i = 0; i < resolved.Length; i++)
        {
            var family = fact.Families[i]; var type = resolved[i].Type;
            if (family is "anyarray" or "anycompatiblearray" or "anyrange")
            {
                if (type == SqlType.Unknown) continue;
                if (type.Kind != (family == "anyrange" ? SqlKind.Range : SqlKind.Array)) throw Undefined();
                type = type.Element!;
            }
            else if (family is not ("anyelement" or "anycompatible"))
            {
                if (type == SqlType.Unknown && family == "text") { resolved[i] = Resolve.Adapt(expression.Args[i], resolved[i], SqlType.Text); continue; }
                if (family == "integer" ? !type.IsInteger() : type.Name() != family) throw Undefined();
                continue;
            }
            if (type == SqlType.Unknown) continue;
            if (element == SqlType.Unknown) element = type;
            else if (family is "anycompatible" or "anycompatiblearray")
            { try { element = Resolve.Common(element, type); } catch (SqlException) { throw Undefined(); } }
            else if (element != type) throw Undefined();
        }
        if (element == SqlType.Unknown) throw new SqlException(expression.Text == "array_to_string" ? SqlStates.DatatypeMismatch : SqlStates.IndeterminateDatatype, "cannot determine polymorphic element type");
        for (var i = 0; i < resolved.Length; i++)
        {
            var family = fact.Families[i];
            if (family is not ("anyarray" or "anycompatiblearray" or "anyelement" or "anycompatible")) continue;
            var target = family is "anyarray" or "anycompatiblearray" ? SqlType.Array(element) : element;
            if (resolved[i].Type != target)
            {
                var sourceElement = resolved[i].Type.Kind == SqlKind.Array ? resolved[i].Type.Element! : resolved[i].Type;
                var changes = sourceElement.IsInteger() && element == SqlType.Decimal || sourceElement == SqlType.F32 && element == SqlType.F64;
                resolved[i] = new(changes ? "cast" : "promote", target, default, [resolved[i]], Target: new(target));
            }
        }
        var resultType = fact.Result switch
        {
            "anyelement" => element,
            "anyarray" or "anycompatiblearray" => SqlType.Array(element),
            "anyrange" => SqlType.Range(element),
            _ => SqlTypes.Parse(fact.Result)
        };
        result = new("call", resultType, default, resolved, expression.Text, Function: fact); return true;
    }
    private static Resolved Call(string name, SqlType type, Resolved[] args, string nulls) =>
        new("call", type, default, args, name, Function: new(name, [], type.Name(), nulls, 1, false, [], []));
    private static SqlException Undefined() => new(SqlStates.UndefinedFunction, "no matching open-type function");
}
