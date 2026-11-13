namespace Jed;

internal sealed record JsonSqlSpec(string Kind = "value", bool Unique = false, bool Negated = false, string? Returning = null,
    string Wrapper = "without", string Empty = "null", string Error = "null", int EmptyIndex = -1, int ErrorIndex = -1,
    string[]? PassingNames = null, string[]? PassingFormats = null, JsonTableSpec? Table = null)
{
    internal int MainCount(string name) => name is "json_value" or "json_query" or "json_exists" or "json_table" ? 2 + (PassingNames?.Length ?? 0) : 1;
}

internal sealed partial class Parser
{
    private Expression JsonPredicate(Expression operand, bool negated)
    {
        var kind = Is("value") || Is("scalar") || Is("array") || Is("object") ? Take().Text : "value";
        var unique = JsonUnique(); return Node("is_json", "", operand) with { Json = new(Kind: kind, Unique: unique, Negated: negated) };
    }
    private bool JsonUnique()
    {
        if (!Is("with") && !Is("without")) return false;
        var unique = Eat("with"); if (!unique) Expect("without"); Expect("unique"); Eat("keys"); return unique;
    }
    // Opening parenthesis has been consumed by the ordinary primary-expression dispatcher.
    private Expression JsonSpecial(string name)
    {
        nesting++; Depth(nesting); var args = new List<Expression> { Expr() }; var spec = new JsonSqlSpec(Error: name == "json_exists" ? "false" : "null");
        if (Eat("format")) { Expect("json"); }
        if (name is "json_value" or "json_query" or "json_exists")
        {
            Expect(","); if (token.Kind != TokenKind.String) throw Errors.Syntax("SQL/JSON path must be a string literal"); args.Add(Node("text", Take().Text));
            var names = new List<string>(); var formats = new List<string>();
            if (Eat("passing"))
            {
                do
                {
                    args.Add(Expr()); var format = "";
                    if (Eat("format"))
                    {
                        Expect("json"); format = "json";
                        if (Eat("encoding")) { format = Name(); if (format is not ("utf8" or "utf16" or "utf32")) throw new SqlException("22023", "unrecognized JSON encoding"); }
                    }
                    Expect("as"); names.Add(Name()); formats.Add(format);
                } while (Eat(","));
            }
            spec = spec with { PassingNames = names.ToArray(), PassingFormats = formats.ToArray() };
        }
        if (Eat("returning"))
        {
            if (name == "json_exists") throw Errors.Syntax("JSON_EXISTS does not take RETURNING");
            spec = spec with { Returning = TypeName() }; if (Eat("format")) Expect("json");
        }
        if (name == "json") spec = spec with { Unique = JsonUnique() };
        if (name == "json_query")
        {
            if (Eat("without")) { Eat("array"); Expect("wrapper"); }
            else if (Eat("with"))
            {
                var wrapper = Eat("conditional") ? "conditional" : "with"; Eat("unconditional"); Eat("array"); Expect("wrapper"); spec = spec with { Wrapper = wrapper };
            }
            if (Is("keep") || Is("omit"))
            {
                var omit = Eat("omit"); if (!omit) Expect("keep"); Expect("quotes"); if (Eat("on")) { Expect("scalar"); Expect("string"); }
                if (omit) throw Errors.Unsupported("JSON_QUERY OMIT QUOTES");
            }
        }
        var hasEmpty = false; var hasError = false;
        if (name is "json_value" or "json_query" or "json_exists")
        {
            while (!Is(")"))
            {
                var behavior = Take().Text; var index = -1;
                if (behavior == "default") { if (name == "json_exists") throw Errors.Syntax("invalid JSON_EXISTS behavior"); index = args.Count; args.Add(Expr()); }
                else if (behavior == "empty") { behavior = Eat("object") ? "empty_object" : "empty_array"; Eat("array"); }
                else if (behavior is not ("error" or "null" or "true" or "false" or "unknown")) throw Errors.Syntax("invalid SQL/JSON behavior");
                Expect("on");
                if (Eat("empty")) { if (name == "json_exists" || hasEmpty || hasError) throw Errors.Syntax("invalid ON EMPTY clause"); hasEmpty = true; spec = spec with { Empty = behavior, EmptyIndex = index }; }
                else { Expect("error"); if (hasError) throw Errors.Syntax("duplicate ON ERROR clause"); hasError = true; spec = spec with { Error = behavior, ErrorIndex = index }; }
            }
        }
        Expect(")"); nesting--; return Node("json_sql", name, args.ToArray()) with { Json = spec };
    }
}
