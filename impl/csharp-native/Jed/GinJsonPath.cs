namespace Jed;

// Sound JSONpath term tree. Unsupported path shapes retain only independently
// useful filter predicates; the residual path evaluator decides every candidate.
internal static class GinJsonPath
{
    private sealed record PathState(byte[][] Keys, string Last);
    private sealed record WalkResult(PathState State, GinQueryNode? Filters, bool Complete);
    internal static GinQueryNode? Extract(SqlJsonPath path, bool predicate)
    {
        if (predicate != path.Root.Predicate) return null;
        var state = new PathState([], "root");
        return predicate ? Predicate(path.Root, state, path.Strict, false) : Walk(path.Root, state, path.Strict).Filters;
    }
    private static GinQueryNode? Combine(bool all, GinQueryNode? left, GinQueryNode? right)
    {
        if (!all && (left is null || right is null)) return null;
        if (left is null) return right;
        if (right is null) return left;
        return GinQueryNode.Make(all, [], [left, right]);
    }
    private static GinQueryNode? Predicate(PathExpr expression, PathState state, bool strict, bool negate)
    {
        if (expression.Op == "!") return Predicate(expression.Args[0], state, strict, !negate);
        if (expression.Op is "&&" or "||")
            return Combine((expression.Op == "&&") != negate, Predicate(expression.Args[0], state, strict, negate), Predicate(expression.Args[1], state, strict, negate));
        if (negate) return null;
        if (expression.Op == "exists") return Walk(expression.Args[0], state, strict).Filters;
        if (expression.Op != "==") return null;
        var literal = expression.Args[0]; var operand = expression.Args[1];
        if (literal.Op != "literal") { literal = expression.Args[1]; operand = expression.Args[0]; }
        if (literal.Op != "literal" || literal.Value!.Kind is SqlJsonKind.Array or SqlJsonKind.Object) return null;
        var walked = Walk(operand, state, strict);
        if (!walked.Complete) return walked.Filters;
        var leaves = new List<byte[]>(walked.State.Keys); var children = new List<GinQueryNode>();
        var scalar = literal.Value;
        if (scalar.Kind != SqlJsonKind.String) leaves.Add(StatisticsTerms.Scalar(scalar));
        else if (strict && walked.State.Last is "member" or "members") leaves.Add(StatisticsTerms.Scalar(scalar, false));
        else if (strict && walked.State.Last is "elements" or "index") leaves.Add(StatisticsTerms.Scalar(scalar));
        else children.Add(GinQueryNode.Make(false, [StatisticsTerms.Scalar(scalar), StatisticsTerms.Scalar(scalar, false)]));
        if (walked.Filters is not null) children.Add(walked.Filters);
        return GinQueryNode.Make(true, leaves, children);
    }
    private static WalkResult Walk(PathExpr expression, PathState context, bool strict)
    {
        if (expression.Op == "root") return new(new([], "root"), null, true);
        if (expression.Op == "current") return new(context, null, true);
        if (expression.Op is not ("member" or "members" or "elements" or "index" or "filter" or "descendants" or "method")) return new(context, null, false);
        var previous = Walk(expression.Args[0], context, strict);
        if (!previous.Complete) return previous;
        if (expression.Op == "method") return previous with { Complete = false };
        if (expression.Op == "filter") return previous with { Filters = Combine(true, previous.Filters, Predicate(expression.Args[1], previous.State, strict, false)) };
        var keys = previous.State.Keys;
        if (expression.Op == "member") keys = [..keys, StatisticsTerms.Scalar(SqlJsonNode.String(expression.Text))];
        return previous with { State = new(keys, expression.Op) };
    }
}
