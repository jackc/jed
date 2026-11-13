using System.Globalization;
using System.Text;

namespace Jed;

internal sealed class JsonPathParser
{
    private readonly List<PathToken> tokens;
    private int position, depth, filters, subscripts;
    internal JsonPathParser(string input) { TextEncoding.Validate(input); tokens = JsonPathTokens.Scan(input); }
    private PathToken Current => position < tokens.Count ? tokens[position] : new("end", "");
    private bool Is(string text) => Current.Kind is not ("string" or "variable" or "number") && Current.Text.Equals(text, StringComparison.OrdinalIgnoreCase);
    private bool Take(string text) { if (!Is(text)) return false; position++; return true; }
    private void Expect(string text) { if (!Take(text)) throw Syntax(); }
    private static SqlException Syntax() => new("42601", "syntax error in jsonpath");
    private void Enter() { if (++depth > 256) throw new SqlException("54001", "jsonpath nesting depth exceeds the maximum of 256"); }
    internal (bool Strict, PathExpr Root) Parse()
    {
        if (tokens.Count == 0) throw new SqlException("22P02", "invalid input syntax for type jsonpath");
        var strict = Take("strict"); if (!strict) Take("lax");
        var root = Expr(1); if (Current.Kind != "end") throw Syntax(); return (strict, root);
    }
    private PathExpr Expr(int minimum)
    {
        var left = Primary(); var chain = 0;
        while (true)
        {
            var op = Current.Kind is "symbol" or "name" ? TextEncoding.FoldName(Current.Text) : "";
            var precedence = op switch { "||" => 1, "&&" => 2, "==" or "!=" or "<>" or "<" or "<=" or ">" or ">=" or "starts" or "like_regex" => 3, "+" or "-" => 4, "*" or "/" or "%" => 5, _ => 0 };
            if (precedence < minimum) break;
            position++;
            if (precedence != 3) { Enter(); chain++; }
            PathExpr right;
            if (op == "starts")
            {
                Expect("with"); if (Current.Kind is not ("variable" or "string")) throw Syntax();
                right = Current.Kind == "variable" ? new("variable", Current.Text, []) : new("literal", "", [], SqlJsonNode.String(Current.Text)); position++;
            }
            else if (op == "like_regex")
            {
                if (Current.Kind != "string") throw Syntax(); right = new("literal", "", [], SqlJsonNode.String(Current.Text)); position++;
                var flags = "";
                if (Take("flag")) { if (Current.Kind != "string") throw Syntax(); flags = Current.Text; position++; }
                if (flags.Any(c => !"ismxq".Contains(c))) throw new SqlException("42601", "invalid regular expression option");
                if (flags.Contains('x') && !flags.Contains('q')) throw Errors.Unsupported("expanded JSON path regular expressions");
                _ = RegexProgram.Compile(right.Value!.Text!, flags, jsonPath: true);
                if (left.Predicate) throw Syntax(); left = new("regex", string.Concat("ismxq".Where(flags.Contains)), [left, right]); continue;
            }
            else right = Expr(precedence + 1);
            if (op is "&&" or "||") { if (!left.Predicate || !right.Predicate) throw Syntax(); }
            else if (left.Predicate || right.Predicate) throw Syntax();
            left = new(op == "starts" ? "starts" : op, "", [left, right]);
        }
        depth -= chain; return left;
    }
    private PathExpr Primary(bool allowUnknown = true)
    {
        var token = Current; position++; PathExpr result; var grouped = false;
        if (token.Kind == "symbol" && token.Text is "+" or "-")
        {
            Enter(); var argument = Primary(); depth--; if (argument.Predicate) throw Syntax();
            if (argument.Op == "literal" && argument.Value!.Kind == SqlJsonKind.Number) result = new("literal", "", [], SqlJsonNode.Numeric(token.Text == "-" ? argument.Value.Number!.Negate() : argument.Value.Number!));
            else result = new(token.Text, "", [argument]);
            return result;
        }
        if (token.Kind == "symbol" && token.Text == "!")
        {
            if (!Is("(") && !Is("exists")) throw Syntax(); Enter();
            PathExpr predicate;
            if (Take("(")) { predicate = Expr(1); Expect(")"); }
            else predicate = Primary(false);
            depth--; if (!predicate.Predicate) throw Syntax(); return new("!", "", [predicate]);
        }
        if (token.Kind == "symbol" && token.Text == "(") { Enter(); result = Expr(1); Expect(")"); depth--; grouped = true; }
        else if (token.Kind == "string") result = new("literal", "", [], SqlJsonNode.String(token.Text));
        else if (token.Kind == "number") result = new("literal", "", [], SqlJsonNode.Numeric(ExactDecimal.Parse(token.Text)));
        else if (token.Kind == "variable") result = new("variable", token.Text, []);
        else if (token.Text == "$") result = new("root", "", []);
        else if (token.Text == "@") { if (filters == 0) throw Syntax(); result = new("current", "", []); }
        else if (token.Text.Equals("last", StringComparison.OrdinalIgnoreCase)) { if (subscripts == 0) throw Syntax(); result = new("last", "", []); }
        else if (token.Text is "null" or "true" or "false") result = new("literal", "", [], token.Text == "null" ? SqlJsonNode.Null : SqlJsonNode.Boolean(token.Text == "true"));
        else if (token.Text.Equals("exists", StringComparison.OrdinalIgnoreCase)) { Expect("("); Enter(); var value = Expr(1); Expect(")"); depth--; if (value.Predicate) throw Syntax(); result = new("exists", "", [value]); }
        else throw Syntax();
        if (allowUnknown && grouped && Take("is")) { Expect("unknown"); if (!result.Predicate) throw Syntax(); result = new("unknown", "", [result]); grouped = false; }
        while (true)
        {
            if (Take("."))
            {
                if (result.Predicate && !grouped) throw Syntax();
                if (Take("*")) { result = new("members", "", [result]); continue; }
                if (Take("**"))
                {
                    var bounds = "";
                    if (Take("{"))
                    {
                        var first = Level(); var last = Take("to") ? Level() : first; Expect("}");
                        if (first != "0" || last != "last") bounds = "{" + first + (first == last ? "" : " to " + last) + "}";
                    }
                    result = new("descendants", bounds, [result]); continue;
                }
                if (Current.Kind is not ("name" or "string")) throw Syntax(); var member = Current.Text; var quoted = Current.Kind == "string"; position++;
                if (!quoted && Take("("))
                {
                    member = TextEncoding.FoldName(member); var arguments = new List<PathExpr> { result };
                    if (!Take(")"))
                    {
                        do
                        {
                            if (member == "datetime")
                            {
                                if (Current.Kind != "string") throw Syntax(); arguments.Add(new("literal", "", [], SqlJsonNode.String(Current.Text))); position++;
                            }
                            else
                            {
                                var negative = Take("-"); if (!negative) Take("+");
                                if (Current.Kind != "number" || Current.Text.Any(c => !char.IsAsciiDigit(c)) || Current.Text.Length > 1 && Current.Text[0] == '0') throw Syntax();
                                var number = ExactDecimal.Parse(Current.Text); position++; arguments.Add(new("literal", "", [], SqlJsonNode.Numeric(negative ? number.Negate() : number)));
                            }
                        } while (Take(","));
                        Expect(")");
                    }
                    var supported = new[] { "type", "size", "double", "ceiling", "floor", "abs", "keyvalue", "bigint", "boolean", "integer", "number", "string", "date", "decimal", "datetime", "time", "time_tz", "timestamp", "timestamp_tz" };
                    if (!supported.Contains(member, StringComparer.Ordinal)) throw Syntax();
                    var maximum = member == "decimal" ? 3 : member is "datetime" or "time" or "time_tz" or "timestamp" or "timestamp_tz" ? 2 : 1;
                    if (arguments.Count > maximum) throw Syntax();
                    if (arguments.Skip(1).Any(arg => arg.Op != "literal" || arg.Value!.Kind != (member == "datetime" ? SqlJsonKind.String : SqlJsonKind.Number))) throw Syntax();
                    result = new("method", member, arguments.ToArray());
                }
                else result = new("member", member, [result]);
            }
            else if (Take("["))
            {
                Enter(); subscripts++;
                if (Take("*")) { Expect("]"); result = new("elements", "", [result]); }
                else
                {
                    var indexes = new List<PathExpr> { result };
                    do { var from = Expr(1); if (from.Predicate) throw Syntax(); if (Take("to")) { var to = Expr(1); if (to.Predicate) throw Syntax(); from = new("range", "", [from, to]); } indexes.Add(from); } while (Take(","));
                    Expect("]"); result = new("index", "", indexes.ToArray());
                }
                subscripts--; depth--;
            }
            else if (Take("?")) { Expect("("); Enter(); filters++; var predicate = Expr(1); filters--; depth--; Expect(")"); if (!predicate.Predicate) throw Syntax(); result = new("filter", "", [result, predicate]); }
            else break;
        }
        return result;
    }
    private string Level()
    {
        if (Take("last")) return "last";
        var token = Current; if (token.Kind != "number" || token.Text.Any(c => !char.IsAsciiDigit(c))) throw Syntax(); position++;
        if (!int.TryParse(token.Text, NumberStyles.None, CultureInfo.InvariantCulture, out var number)) throw Errors.Overflow();
        return number.ToString(CultureInfo.InvariantCulture);
    }
}
