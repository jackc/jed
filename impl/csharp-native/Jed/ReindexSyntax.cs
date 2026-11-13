namespace Jed;

internal sealed record ReindexSyntax(string Kind, string? Name, bool Concurrently) : StatementSyntax;

internal sealed partial class Parser
{
    private StatementSyntax ReindexStatement()
    {
        var concurrently = false;
        if (Eat("("))
        {
            do
            {
                var option = Name();
                if (option == "tablespace") throw Errors.Unsupported("tablespaces");
                if (option is not ("verbose" or "concurrently")) throw Errors.Syntax("unrecognized REINDEX option");
                var value = true;
                if (!Is(",") && !Is(")"))
                {
                    var text = TextEncoding.FoldName(Take().Text);
                    value = text switch { "true" or "on" or "1" => true, "false" or "off" or "0" => false, _ => throw Errors.Syntax("REINDEX option requires a Boolean value") };
                }
                if (option == "concurrently") concurrently = value;
            } while (Eat(","));
            Expect(")");
        }
        var kind = Name();
        if (kind == "schema") throw Errors.Unsupported("REINDEX SCHEMA");
        if (kind is not ("index" or "table" or "database" or "system")) throw Errors.Syntax("expected REINDEX target kind");
        if (Is("concurrently") && (kind is "database" or "system" || Peek().Kind is TokenKind.Word or TokenKind.QuotedName)) { Take(); concurrently = true; }
        var name = token.Kind == TokenKind.End || Is(";") ? null : QualifiedName();
        if (name is null && kind is "index" or "table") throw Errors.Syntax("REINDEX requires a relation name");
        if (kind == "system" && concurrently) throw Errors.Unsupported("cannot reindex system catalogs concurrently");
        return new ReindexSyntax(kind, name, concurrently);
    }
}
