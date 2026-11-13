using System.Globalization;

namespace Jed;

internal sealed record SequenceOptions(string? Type = null, long? Increment = null, long? Minimum = null, long? Maximum = null,
    bool NoMinimum = false, bool NoMaximum = false, long? Start = null, long? Cache = null, bool? Cycle = null,
    bool Restart = false, long? RestartValue = null, string? SequenceName = null);
internal sealed record CreateSequenceSyntax(string Name, bool IfNotExists, SequenceOptions Options) : StatementSyntax;
internal sealed record AlterSequenceSyntax(string Name, bool IfExists, SequenceOptions Options, string? Rename = null) : StatementSyntax;
internal sealed record DropSequenceSyntax(string[] Names, bool IfExists, bool Cascade) : StatementSyntax;

internal sealed partial class Parser
{
    private StatementSyntax CreateSequence()
    {
        var optional = Eat("if"); if (optional) { Expect("not"); Expect("exists"); }
        var name = QualifiedName(); return new CreateSequenceSyntax(name, optional, ReadSequenceOptions(false));
    }
    private StatementSyntax AlterSequence()
    {
        var optional = Eat("if"); if (optional) Expect("exists"); var name = QualifiedName();
        if (Eat("rename")) { Expect("to"); return new AlterSequenceSyntax(name, optional, new(), Name()); }
        var options = ReadSequenceOptions(true);
        if (options == new SequenceOptions()) throw Errors.Syntax("ALTER SEQUENCE requires an action");
        return new AlterSequenceSyntax(name, optional, options);
    }
    private StatementSyntax DropSequence()
    {
        var optional = Eat("if"); if (optional) Expect("exists"); var names = new List<string>();
        do { names.Add(QualifiedName()); } while (Eat(","));
        var cascade = Eat("cascade"); if (!cascade) Eat("restrict");
        return new DropSequenceSyntax(names.ToArray(), optional, cascade);
    }
    private long SequenceInteger()
    {
        var sign = Eat("-") ? "-" : Eat("+") ? "+" : "";
        if (token.Kind != TokenKind.Integer) throw Errors.Syntax("sequence option requires an integer");
        if (!long.TryParse(sign + Take().Text, NumberStyles.AllowLeadingSign, CultureInfo.InvariantCulture, out var value)) throw Errors.Overflow();
        return value;
    }
    private SequenceOptions ReadSequenceOptions(bool alter, bool identity = false, bool allowSetTail = false)
    {
        var options = new SequenceOptions(); var seen = new HashSet<string>(StringComparer.Ordinal);
        while (true)
        {
            var no = Eat("no"); string option;
            if (Eat("as")) option = "type";
            else if (Eat("increment")) option = "increment";
            else if (Eat("minvalue")) option = "minimum";
            else if (Eat("maxvalue")) option = "maximum";
            else if (Eat("start")) option = "start";
            else if (Eat("cache")) option = "cache";
            else if (Eat("cycle")) option = "cycle";
            else if (alter && Eat("restart")) option = "restart";
            else if (identity && Eat("sequence")) { Expect("name"); option = "name"; }
            else { if (no) throw Errors.Syntax("invalid NO sequence option"); break; }
            if (!seen.Add(option) || no && option is not ("minimum" or "maximum" or "cycle")) throw Errors.Syntax("conflicting sequence options");
            switch (option)
            {
                case "type":
                    if (identity) throw Errors.Syntax("identity fixes the sequence type");
                    if (alter) throw Errors.Unsupported("ALTER SEQUENCE AS");
                    options = options with { Type = TypeName() }; break;
                case "increment": Eat("by"); options = options with { Increment = SequenceInteger() }; break;
                case "minimum": options = no ? options with { NoMinimum = true } : options with { Minimum = SequenceInteger() }; break;
                case "maximum": options = no ? options with { NoMaximum = true } : options with { Maximum = SequenceInteger() }; break;
                case "start": Eat("with"); options = options with { Start = SequenceInteger() }; break;
                case "cache": options = options with { Cache = SequenceInteger() }; break;
                case "cycle": options = options with { Cycle = !no }; break;
                case "name": options = options with { SequenceName = QualifiedName() }; break;
                case "restart":
                    var withValue = Eat("with");
                    var restart = withValue || token.Kind == TokenKind.Integer || Is("-") || Is("+") ? SequenceInteger() : (long?)null;
                    options = options with { Restart = true, RestartValue = restart }; break;
            }
        }
        if (Is("owned") || Is("owner") || alter && !allowSetTail && Is("set")) throw Errors.Unsupported("sequence ownership or schema option");
        return options;
    }
}
