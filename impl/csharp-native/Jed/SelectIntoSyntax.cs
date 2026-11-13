namespace Jed;

internal sealed partial class Parser
{
    private bool intoAllowed;
    private (string Name, bool Temporary)? intoTarget;

    // Only the first SELECT core of a query statement owns INTO. The flag is
    // consumed before parsing its expressions, so every nested query is denied.
    // WITH separately denies its bodies and restores ownership for the primary.
    private StatementSyntax QueryStatement()
    {
        var previousAllowed = intoAllowed; var previousTarget = intoTarget;
        intoAllowed = true; intoTarget = null;
        try
        {
            var statement = Eat("with") ? WithStatement() : Select();
            if (intoTarget is not { } target) return statement;
            if (statement is not SelectSyntax query) throw Errors.Syntax("SELECT INTO is not allowed here");
            return new CreateTableAsSyntax(target.Name, null, query, false, true, target.Temporary);
        }
        finally { intoAllowed = previousAllowed; intoTarget = previousTarget; }
    }

    private void IntoClause(bool allowed)
    {
        if (!Eat("into")) return;
        if (!allowed) throw Errors.Syntax("SELECT INTO is only allowed on the first SELECT of a query statement");
        if (Is("unlogged") && Peek().Text != ".") throw Errors.Unsupported("unlogged tables");
        var temporary = (Is("temp") || Is("temporary")) && Peek().Text != ".";
        if (temporary) Take();
        if (Is("table") && Peek().Text != ".") Take();
        var name = QualifiedName();
        if (temporary && DatabaseDomains.Name(name).Domain is { } domain && domain != "temp")
            throw Errors.Syntax("temporary target cannot name a persistent database");
        intoTarget = (name, temporary);
    }
}
