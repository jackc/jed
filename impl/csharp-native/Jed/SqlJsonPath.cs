namespace Jed;

/// <summary>A bounded, immutable SQL/JSON path program with normalized source text.</summary>
public sealed class SqlJsonPath
{
    public bool Strict { get; }
    public string Text { get; }
    internal PathExpr Root { get; }
    private SqlJsonPath(bool strict, PathExpr root) { Strict = strict; Root = root; Text = (strict ? "strict " : "") + root.Render(top: true); }
    public static SqlJsonPath Parse(string input)
    {
        var parsed = new JsonPathParser(input).Parse(); var path = new SqlJsonPath(parsed.Strict, parsed.Root);
        _ = new JsonPathParser(path.Text).Parse(); return path;
    }
    public override string ToString() => Text;
}
internal sealed record PathExpr(string Op, string Text, PathExpr[] Args, SqlJsonNode? Value = null)
{
    internal bool Predicate => Op is "==" or "!=" or "<>" or "<" or "<=" or ">" or ">=" or "&&" or "||" or "!" or "exists" or "unknown" or "starts" or "regex";
    internal int Priority => Op switch { "||" => 1, "&&" => 2, "==" or "!=" or "<>" or "<" or "<=" or ">" or ">=" or "starts" or "regex" => 3, "+" or "-" when Args.Length == 2 => 4, "*" or "/" or "%" => 5, "+" or "-" or "!" => 6, _ => 7 };
    internal string Render(bool top = false)
    {
        string Child(PathExpr child, int priority, bool right = false)
        {
            var text = child.Render(); return child.Priority <= priority ? "(" + text + ")" : text;
        }
        if (Op is "member" or "members" or "elements" or "index" or "filter" or "method" or "descendants")
        {
            var steps = new Stack<PathExpr>(); var seed = this;
            while (seed.Op is "member" or "members" or "elements" or "index" or "filter" or "method" or "descendants") { steps.Push(seed); seed = seed.Args[0]; }
            var output = new System.Text.StringBuilder();
            var body = seed.Render();
            output.Append(seed.Priority < 7 || seed.Predicate || seed.Op == "literal" && seed.Value!.Kind == SqlJsonKind.Number ? "(" + body + ")" : body);
            while (steps.Count != 0)
            {
                var step = steps.Pop();
                switch (step.Op)
                {
                    case "member": output.Append('.').Append(JsonImage.Quote(step.Text)); break;
                    case "members": output.Append(".*"); break;
                    case "elements": output.Append("[*]"); break;
                    case "index": output.Append('[').Append(string.Join(",", step.Args.Skip(1).Select(arg => arg.Render()))).Append(']'); break;
                    case "filter": output.Append("?(").Append(step.Args[1].Render()).Append(')'); break;
                    case "method": output.Append('.').Append(step.Text).Append('(').Append(string.Join(",", step.Args.Skip(1).Select(arg => arg.Render()))).Append(')'); break;
                    case "descendants": output.Append(".**").Append(step.Text); break;
                }
            }
            return output.ToString();
        }
        string rendered;
        switch (Op)
        {
            case "root": return "$";
            case "current": return "@";
            case "last": return "last";
            case "variable": return "$" + JsonImage.Quote(Text);
            case "literal": return Value!.ToString();
            case "range": return Args[0].Render() + " to " + Args[1].Render();
            case "exists": return "exists (" + Args[0].Render() + ")";
            case "unknown": return "(" + Args[0].Render() + ") is unknown";
            case "!": return "!(" + Args[0].Render() + ")";
            case "starts": rendered = Child(Args[0], Priority) + " starts with " + Args[1].Render(); break;
            case "regex": rendered = (Args[0].Priority < 7 ? "(" + Args[0].Render() + ")" : Args[0].Render()) + " like_regex " + Args[1].Render() + (Text.Length == 0 ? "" : " flag " + JsonImage.Quote(Text)); break;
            case "+": case "-":
                if (Args.Length == 1) { rendered = Op + Child(Args[0], Priority); break; }
                rendered = Child(Args[0], Priority) + " " + Op + " " + Child(Args[1], Priority, true); break;
            default: rendered = Child(Args[0], Priority) + " " + (Op == "<>" ? "!=" : Op) + " " + Child(Args[1], Priority, true); break;
        }
        return top ? "(" + rendered + ")" : rendered;
    }
}
