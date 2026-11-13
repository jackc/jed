namespace Jed;

internal sealed record WindowBoundSyntax(string Kind, Expression? Offset = null);
internal sealed record WindowFrameSyntax(string Mode, WindowBoundSyntax Start, WindowBoundSyntax End, string Exclude = "none");
internal sealed record WindowSyntax(Expression[] Partition, OrderSyntax[] Order, WindowFrameSyntax? Frame = null, string? Name = null, bool Reference = false);

internal sealed partial class Parser
{
    private OrderSyntax[] OrderItems()
    {
        var order = new List<OrderSyntax>();
        do
        {
            var expression = Expr(); var descending = Eat("desc"); if (!descending) Eat("asc");
            bool? nulls = null;
            if (Eat("nulls")) { if (Eat("first")) nulls = true; else { Expect("last"); nulls = false; } }
            order.Add(new(expression, descending, nulls));
        } while (Eat(","));
        return order.ToArray();
    }
    private WindowSyntax Over()
    {
        if (!Eat("(")) return new([], [], Name: Name(), Reference: true);
        var definition = WindowDefinition(); Expect(")"); return definition;
    }
    private WindowSyntax WindowDefinition()
    {
        string? name = null;
        if (!Is(")") && !Is("partition") && !Is("order") && !Is("rows") && !Is("range") && !Is("groups")) name = Name();
        var partition = new List<Expression>();
        if (Eat("partition")) { Expect("by"); do { partition.Add(Expr()); } while (Eat(",")); }
        var order = Array.Empty<OrderSyntax>();
        if (Eat("order")) { Expect("by"); order = OrderItems(); }
        WindowFrameSyntax? frame = null;
        if (Is("rows") || Is("range") || Is("groups"))
        {
            var mode = Take().Text; WindowBoundSyntax start, end;
            if (Eat("between")) { start = FrameBound(); Expect("and"); end = FrameBound(); }
            else { start = FrameBound(); end = new("current"); }
            var exclude = "none";
            if (Eat("exclude"))
            {
                if (Eat("current")) { Expect("row"); exclude = "current"; }
                else if (Eat("group")) exclude = "group";
                else if (Eat("ties")) exclude = "ties";
                else { Expect("no"); Expect("others"); }
            }
            frame = new(mode, start, end, exclude);
        }
        return new(partition.ToArray(), order, frame, name);
    }
    private WindowBoundSyntax FrameBound()
    {
        if (Eat("unbounded"))
        {
            if (Eat("preceding")) return new("unbounded_preceding");
            Expect("following"); return new("unbounded_following");
        }
        if (Eat("current")) { Expect("row"); return new("current"); }
        var offset = Expr(21);
        if (Eat("preceding")) return new("preceding", offset);
        Expect("following"); return new("following", offset);
    }
}
