namespace Jed;

internal sealed record JsonTableColumn(string Name, string Kind, string Path, JsonSqlSpec Spec, JsonTableColumn[] Children);
internal sealed record JsonTableSpec(JsonTableColumn[] Columns, string? PathName = null, JsonTablePlan? Plan = null);
internal sealed record JsonTableColumnPlan(string Kind, int Slot, SqlJsonPath Path, Resolved? Value, JsonTableColumnPlan[] Children);
internal sealed record JsonTablePlan(JsonTableColumnPlan[] Columns, SqlType[] Types);

internal sealed partial class Parser
{
    // The caller has consumed JSON_TABLE's opening parenthesis.
    private Expression JsonTable()
    {
        nesting++; Depth(nesting);
        var args = new List<Expression> { Expr() }; if (Eat("format")) Expect("json"); Expect(",");
        if (token.Kind != TokenKind.String) throw Errors.Syntax("JSON_TABLE path must be a string literal");
        args.Add(Node("text", Take().Text)); var names = new HashSet<string>(StringComparer.Ordinal);
        var pathName = Eat("as") ? Name() : null; if (pathName is not null) names.Add(pathName);
        var passingNames = new List<string>(); var formats = new List<string>();
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
                Expect("as"); passingNames.Add(Name()); formats.Add(format);
            } while (Eat(","));
        }
        var spec = new JsonSqlSpec(PassingNames: passingNames.ToArray(), PassingFormats: formats.ToArray());
        var columns = JsonTableColumns(args, names);
        if (Eat("plan")) throw Errors.Unsupported("explicit JSON_TABLE PLAN");
        Expect(")"); nesting--; return Node("json_table", "json_table", args.ToArray()) with { Json = spec with { Table = new(columns, pathName) } };
    }
    private JsonTableColumn[] JsonTableColumns(List<Expression> args, HashSet<string> names)
    {
        Expect("columns"); Expect("("); var result = new List<JsonTableColumn>(); var ordinal = false;
        do
        {
            if (Eat("nested"))
            {
                nesting++; Depth(nesting); Eat("path");
                if (token.Kind != TokenKind.String) throw Errors.Syntax("NESTED path must be a string literal"); var path = Take().Text;
                var name = Eat("as") ? Name() : ""; if (name.Length != 0 && !names.Add(name)) throw new SqlException("42712", "duplicate JSON_TABLE column or path name");
                var children = JsonTableColumns(args, names); nesting--; result.Add(new(name, "nested", path, new(), children)); continue;
            }
            var column = Name(); if (!names.Add(column)) throw new SqlException("42712", "duplicate JSON_TABLE column or path name");
            if (Eat("for"))
            {
                Expect("ordinality"); if (ordinal) throw Errors.Syntax("only one FOR ORDINALITY column is allowed"); ordinal = true;
                result.Add(new(column, "ordinality", "$", new(Returning: "i64"), [])); continue;
            }
            var typeName = TypeName(); var structural = Eat("format"); if (structural) Expect("json");
            var kind = Eat("exists") ? "exists" : structural ? "query" : "value";
            var pathText = "$." + SqlJsonNode.String(column).ToString();
            if (Eat("path")) { if (token.Kind != TokenKind.String) throw Errors.Syntax("column path must be a string literal"); pathText = Take().Text; }
            var spec = new JsonSqlSpec(Returning: typeName, Error: kind == "exists" ? "false" : "null");
            if (Eat("without")) { Eat("array"); Expect("wrapper"); kind = "query"; }
            else if (Eat("with"))
            {
                var wrapper = Eat("conditional") ? "conditional" : "with"; Eat("unconditional"); Eat("array"); Expect("wrapper"); spec = spec with { Wrapper = wrapper }; kind = "query";
            }
            if (Is("keep") || Is("omit"))
            {
                var omit = Eat("omit"); if (!omit) Expect("keep"); Expect("quotes"); if (Eat("on")) { Expect("scalar"); Expect("string"); }
                if (omit) throw Errors.Unsupported("JSON_QUERY OMIT QUOTES"); kind = "query";
            }
            var hasEmpty = false; var hasError = false;
            while (!Is(",") && !Is(")"))
            {
                var behavior = Take().Text; var index = -1;
                if (behavior == "default") { if (kind == "exists") throw Errors.Syntax("invalid JSON_EXISTS behavior"); index = args.Count; args.Add(Expr()); }
                else if (behavior == "empty") { behavior = Eat("object") ? "empty_object" : "empty_array"; Eat("array"); }
                else if (behavior is not ("error" or "null" or "true" or "false" or "unknown")) throw Errors.Syntax("invalid SQL/JSON behavior");
                Expect("on");
                if (Eat("empty")) { if (kind == "exists" || hasEmpty || hasError) throw Errors.Syntax("invalid ON EMPTY clause"); hasEmpty = true; spec = spec with { Empty = behavior, EmptyIndex = index }; }
                else { Expect("error"); if (hasError) throw Errors.Syntax("duplicate ON ERROR clause"); hasError = true; spec = spec with { Error = behavior, ErrorIndex = index }; }
            }
            result.Add(new(column, kind, pathText, spec, []));
        } while (Eat(","));
        Expect(")"); return result.ToArray();
    }
}
