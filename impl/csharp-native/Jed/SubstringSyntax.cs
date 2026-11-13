namespace Jed;

internal sealed partial class Parser
{
    private Expression SubstringSyntax()
    {
        nesting++; Depth(nesting);
        var values = new List<Expression>();
        if (!Eat(")"))
        {
            values.Add(Expr(36));
            if (Eat("from"))
            {
                values.Add(Expr()); if (Eat("for")) values.Add(Expr());
            }
            else if (Eat("similar")) { values.Add(Expr(36)); Expect("escape"); values.Add(Expr()); }
            else if (Eat("for")) { var length = Expr(); values.Add(Eat("from") ? Expr() : Node("integer", "1")); values.Add(length); }
            else while (Eat(",")) values.Add(Expr());
            Expect(")");
        }
        nesting--; return Node("call", "substring", values.ToArray());
    }
    private Expression OverlaySyntax()
    {
        nesting++; Depth(nesting);
        var values = new List<Expression> { Expr() };
        if (Eat("placing"))
        {
            values.Add(Expr()); Expect("from"); values.Add(Expr());
            if (Eat("for")) values.Add(Expr());
        }
        else while (Eat(",")) values.Add(Expr());
        Expect(")"); nesting--; return Node("call", "overlay", values.ToArray());
    }
    private Expression PositionSyntax()
    {
        nesting++; Depth(nesting);
        var needle = Expr(36); Expect("in"); var haystack = Expr(); Expect(")");
        nesting--; return Node("call", "position", haystack, needle);
    }

}
