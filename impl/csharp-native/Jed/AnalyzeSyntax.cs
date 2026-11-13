namespace Jed;

internal sealed record AnalyzeSyntax(string Table, string[]? Columns) : StatementSyntax;

internal sealed partial class Parser
{
    private StatementSyntax AnalyzeStatement()
    {
        if (token.Kind is not (TokenKind.Word or TokenKind.QuotedName)) throw Errors.Syntax("ANALYZE requires a table");
        var table = QualifiedName(); string[]? columns = null;
        if (Eat("("))
        {
            var names = new List<string>();
            do { names.Add(Name()); } while (Eat(","));
            Expect(")"); columns = names.ToArray();
        }
        return new AnalyzeSyntax(table, columns);
    }
}
