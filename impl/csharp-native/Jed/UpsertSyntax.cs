namespace Jed;

internal sealed record ConflictElement(string Column, string? Collation = null, string? Opclass = null, bool Ordered = false);
internal sealed record ConflictSyntax(ConflictElement[]? Elements, string? Constraint, Expression? Predicate, string? PredicateText,
    AssignmentSyntax[]? Assignments, Expression? Filter);

internal sealed partial class Parser
{
    private ConflictSyntax? OnConflict()
    {
        if (!Eat("on")) return null;
        Expect("conflict"); ConflictElement[]? elements = null; string? constraint = null;
        Expression? predicate = null; string? predicateText = null;
        if (Eat("("))
        {
            var entries = new List<ConflictElement>();
            do
            {
                var column = Name(); var collation = Eat("collate") ? Name() : null;
                string? opclass = null; var ordered = false;
                if (token.Kind is TokenKind.Word or TokenKind.QuotedName && !Is("asc") && !Is("desc") && !Is("nulls")) opclass = Name();
                if (Eat("asc") || Eat("desc")) ordered = true;
                if (Eat("nulls")) { ordered = true; if (!Eat("first")) Expect("last"); }
                entries.Add(new(column, collation, opclass, ordered));
            } while (Eat(","));
            Expect(")"); elements = entries.ToArray();
            if (Eat("where")) (predicate, predicateText) = CapturedExpression();
        }
        else if (Eat("on")) { Expect("constraint"); constraint = Name(); }
        Expect("do");
        if (Eat("nothing")) return new(elements, constraint, predicate, predicateText, null, null);
        Expect("update"); Expect("set"); var assignments = new List<AssignmentSyntax>();
        do { var column = Name(); Expect("="); assignments.Add(new(column, Eat("default") ? Node("default", "") : Expr())); } while (Eat(","));
        var filter = Eat("where") ? Expr() : null;
        return new(elements, constraint, predicate, predicateText, assignments.ToArray(), filter);
    }
}
