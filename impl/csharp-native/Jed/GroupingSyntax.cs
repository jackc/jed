namespace Jed;

internal sealed partial class Parser
{
    private (Expression[] Keys, int[][] Sets, bool Expansion) GroupingClause()
    {
        const int maximum = 4096;
        var expansion = false;
        List<Expression[]> Term()
        {
            nesting++; Depth(nesting);
            try
            {
                if (Is("grouping") && Peek().Text == "sets")
                {
                    Take(); expansion = true;
                    Expect("sets"); Expect("("); var alternatives = new List<Expression[]>();
                    do { var next = Term(); if (alternatives.Count + next.Count > maximum) throw Complex(); alternatives.AddRange(next); } while (Eat(","));
                    Expect(")"); return alternatives;
                }
                if (Is("rollup") || Is("cube"))
                {
                    expansion = true;
                    var cube = Take().Text == "cube"; Expect("("); var groups = new List<Expression[]>();
                    do { var next = Term(); if (next.Count != 1) throw Errors.Syntax("nested grouping expansion"); groups.Add(next[0]); } while (Eat(","));
                    Expect(")");
                    if (cube && groups.Count > 12 || !cube && groups.Count >= maximum) throw Complex();
                    var output = new List<Expression[]>();
                    if (cube)
                    {
                        for (var mask = (1 << groups.Count) - 1; mask >= 0; mask--)
                            output.Add(groups.Where((_, i) => (mask & (1 << (groups.Count - 1 - i))) != 0).SelectMany(group => group).ToArray());
                    }
                    else for (var count = groups.Count; count >= 0; count--) output.Add(groups.Take(count).SelectMany(group => group).ToArray());
                    return output;
                }
                if (Is("(") && Peek().Text == ")") { Take(); Take(); return [[]]; }
                var expression = Expr();
                return [expression.Op == "row" ? expression.Args : [expression]];
            }
            finally { nesting--; }
        }
        static SqlException Complex() => new(SqlStates.StatementTooComplex, "grouping-set expansion exceeds 4096");
        var expanded = new List<Expression[]> { Array.Empty<Expression>() };
        do
        {
            var term = Term();
            if ((long)expanded.Count * term.Count > maximum) throw Complex();
            expanded = expanded.SelectMany(prefix => term.Select(suffix => prefix.Concat(suffix).ToArray())).ToList();
        } while (Eat(","));
        var keys = new List<Expression>();
        static bool Same(Expression a, Expression b) => a.Op == b.Op && a.Text == b.Text && a.Args.Length == b.Args.Length && a.Args.Zip(b.Args).All(pair => Same(pair.First, pair.Second));
        var sets = expanded.Select(group => group.Select(expression =>
        {
            var slot = keys.FindIndex(key => Same(key, expression));
            if (slot >= 0) return slot;
            keys.Add(expression); return keys.Count - 1;
        }).Distinct().ToArray()).ToArray();
        return (keys.ToArray(), sets, expansion);
    }
}
