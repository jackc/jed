namespace Jed;

internal sealed record CreateViewSyntax(string Name, string[]? Columns, SelectSyntax Query, string Definition,
    bool Replace, bool IfNotExists, byte CheckOption, (string Name, string Value)[] Options, bool Temporary = false) : StatementSyntax;
internal sealed record DropViewSyntax(string[] Names, bool IfExists, bool Cascade) : StatementSyntax;
internal sealed record AlterViewSyntax(string Name, string Operation, string? Column = null, string? Value = null,
    Expression? Default = null, string? DefaultText = null, (string Name, string Value)[]? Options = null, bool IfExists = false, AlterViewSyntax[]? Actions = null) : StatementSyntax;

internal sealed partial class Parser
{
    private (string Name, string Value)[] ViewOptions(bool reset = false)
    {
        Expect("("); var options = new List<(string, string)>();
        do
        {
            var name = Name(); var value = reset ? "reset" : Eat("=") ? Take().Text : "true";
            options.Add((name, value));
        } while (Eat(","));
        Expect(")"); return options.ToArray();
    }
    private CreateViewSyntax CreateView(bool replace, bool temporary = false)
    {
        var optional = Is("if") && Peek().Text == "not";
        if (optional) { Take(); Expect("not"); Expect("exists"); }
        if (replace && optional) throw Errors.Syntax("OR REPLACE and IF NOT EXISTS are incompatible");
        var name = QualifiedName();
        if (temporary && DatabaseDomains.Name(name).Domain is { } domain && domain != "temp") throw Errors.Syntax("TEMP VIEW requires the temp domain");
        var columns = Is("(") ? ColumnNames() : null;
        var options = Eat("with") ? ViewOptions() : [];
        Expect("as"); var tokens = new List<Token>();
        while (token.Kind != TokenKind.End && !Is(";")) tokens.Add(Take());
        byte check = 0;
        if (tokens.Count >= 3 && tokens[^2].Text == "check" && tokens[^1].Text == "option")
        {
            var length = tokens[^3].Text == "with" ? 3 : tokens.Count >= 4 && tokens[^4].Text == "with" && tokens[^3].Text is "local" or "cascaded" ? 4 : 0;
            if (length != 0) { check = length == 4 && tokens[^3].Text == "local" ? (byte)1 : (byte)2; tokens.RemoveRange(tokens.Count - length, length); }
        }
        var text = string.Join(' ', tokens.Select(RenderToken));
        return new(name, columns, new Parser(text).Parse(), text, replace, optional, check, options, temporary);
    }
    private DropViewSyntax DropView()
    {
        var optional = Eat("if"); if (optional) Expect("exists");
        var names = new List<string>(); do { names.Add(QualifiedName()); } while (Eat(","));
        var cascade = Eat("cascade"); if (!cascade) Eat("restrict"); return new(names.ToArray(), optional, cascade);
    }
    private AlterViewSyntax AlterView()
    {
        var optional = Is("if") && Peek().Text == "exists";
        if (optional) { Take(); Expect("exists"); }
        var name = QualifiedName(); var actions = new List<AlterViewSyntax>();
        do { actions.Add(Action()); } while (Eat(","));
        return actions.Count == 1 ? actions[0] : actions[0] with { Actions = actions.ToArray() };
        AlterViewSyntax Action()
        {
            if (Eat("rename"))
            {
                if (Eat("to")) return new(name, "rename", Value: Name(), IfExists: optional);
                Eat("column"); var column = Name(); Expect("to"); return new(name, "rename_column", column, Name(), IfExists: optional);
            }
            if (Eat("set")) return new(name, "options", Options: ViewOptions(), IfExists: optional);
            if (Eat("reset")) return new(name, "options", Options: ViewOptions(true), IfExists: optional);
            Expect("alter"); Eat("column"); var target = Name();
            if (Eat("drop")) { Expect("default"); return new(name, "default", target, IfExists: optional); }
            Expect("set"); Expect("default"); var (expression, text) = CapturedExpression();
            return new(name, "default", target, Default: expression, DefaultText: text, IfExists: optional);
        }
    }
}
