namespace Jed;

internal sealed record ExplainSyntax(StatementSyntax Inner, bool Analyze = false, bool Verbose = false,
    bool Costs = true, bool Lane = false) : StatementSyntax;

internal sealed partial class Parser
{
    private ExplainSyntax ExplainStatement()
    {
        var analyze = false; var verbose = false; var costs = true; var lane = false;
        if (Is("(") && Peek().Text is not ("select" or "values" or "with" or "("))
        {
            Take();
            var seen = new HashSet<string>(StringComparer.Ordinal);
            do
            {
                var option = Name();
                if (option is not ("analyze" or "verbose" or "costs" or "lane") || !seen.Add(option))
                    throw Errors.Syntax("unknown or repeated EXPLAIN option");
                var enabled = true;
                if (Eat("true") || Eat("on")) enabled = true;
                else if (Eat("false") || Eat("off")) enabled = false;
                switch (option)
                {
                    case "analyze": analyze = enabled; break;
                    case "verbose": verbose = enabled; break;
                    case "costs": costs = enabled; break;
                    case "lane": lane = enabled; break;
                }
            } while (Eat(","));
            Expect(")");
        }
        else analyze = Eat("analyze");
        if (Is("explain")) throw Errors.Syntax("nested EXPLAIN");
        StatementSyntax inner = Is("with") || Is("select") || Is("values") || Is("(") ? QueryStatement() : OtherStatement();
        if (inner is not (SelectSyntax or InsertSyntax or UpdateSyntax or DeleteSyntax or MergeSyntax or CreateTableAsSyntax or WithStatementSyntax))
            throw Errors.Syntax("EXPLAIN requires a query or data manipulation statement");
        return new(inner, analyze, verbose, costs, lane);
    }
}
