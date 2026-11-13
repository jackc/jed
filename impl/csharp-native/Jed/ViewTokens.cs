namespace Jed;

// Stored definitions remain token text. These helpers visit lexical query scopes without
// regenerating expressions, preserving the canonical spelling of unaffected tokens.
internal static class ViewTokens
{
    internal static List<Token> Read(string text)
    {
        var result = new List<Token>(); var lexer = new Lexer(text); Token token;
        while ((token = lexer.Next()).Kind != TokenKind.End) result.Add(token);
        return result;
    }
    internal static string Render(IEnumerable<Token> tokens) => string.Join(' ', tokens.Select(Parser.RenderToken));
    internal static int Close(IReadOnlyList<Token> tokens, int start)
    {
        var depth = 0;
        for (var i = start; i < tokens.Count; i++)
        {
            if (tokens[i].Kind != TokenKind.Symbol) continue;
            if (tokens[i].Text == "(") depth++;
            else if (tokens[i].Text == ")" && --depth == 0) return i;
        }
        throw Errors.Syntax("unbalanced stored query");
    }
    internal static void Nested(List<Token> tokens, int start, Func<List<Token>, List<Token>> transform)
    {
        for (var i = start; i < tokens.Count; i++)
        {
            if (tokens[i].Kind != TokenKind.Symbol || tokens[i].Text != "(") continue;
            var close = Close(tokens, i); var inner = tokens.GetRange(i + 1, close - i - 1);
            if (inner.Count != 0 && inner[0].Kind == TokenKind.Word && inner[0].Text is "select" or "with" or "values") inner = transform(inner);
            else Nested(inner, 0, transform);
            tokens.RemoveRange(i + 1, close - i - 1); tokens.InsertRange(i + 1, inner); i += inner.Count + 1;
        }
    }
}
