namespace Jed;

internal sealed record Expression(string Op, string Text, Expression[] Args, int Height = 0);
internal sealed record SelectItem(Expression Expression, string? Alias);
internal sealed record SelectSyntax(SelectItem[] Items, Expression? Filter, long? Limit, long Offset);

internal sealed class Parser
{
    private const int MaxDepth = 256; // spec/design/cost.md §7, fixed across cores.
    private readonly Lexer lexer;
    private Token token;
    private int nesting;
    internal Parser(string sql) { lexer = new(sql); token = lexer.Next(); }
    private bool Is(string text) => token.Kind is TokenKind.Word or TokenKind.Symbol && token.Text == text;
    private Token Take() { var result = token; token = lexer.Next(); return result; }
    private bool Eat(string text) { if (!Is(text)) return false; Take(); return true; }
    private void Expect(string text) { if (!Eat(text)) throw Errors.Syntax($"expected {text}"); }
    private static void Depth(int depth)
    {
        if (depth > MaxDepth) throw new SqlException(SqlStates.StatementTooComplex, "expression nesting exceeds 256");
    }
    private Expression Node(string op, string text, params Expression[] args)
    {
        var height = args.Length == 0 ? 0 : args.Max(a => a.Height) + 1;
        Depth(nesting + height);
        return new(op, text, args, height);
    }
    private string Name()
    {
        if (token.Kind is not (TokenKind.Word or TokenKind.QuotedName)) throw Errors.Syntax("expected identifier");
        return Take().Text;
    }

    internal SelectSyntax Parse()
    {
        if (!Eat("select")) throw Errors.Unsupported("statements other than SELECT");
        if (Is("distinct")) throw Errors.Unsupported("DISTINCT");
        var items = new List<SelectItem>();
        do
        {
            if (Is("*")) throw Errors.Syntax("SELECT * with no tables specified is not valid");
            var expression = Expr();
            var alias = Eat("as") ? Name() : null;
            items.Add(new(expression, alias));
        } while (Eat(","));
        if (Is("from")) throw Errors.Unsupported("table access");
        var filter = Eat("where") ? Expr() : null;
        long? limit = null;
        long offset = 0;
        if (Eat("limit")) limit = Count();
        if (Eat("offset")) offset = Count();
        Eat(";");
        if (token.Kind != TokenKind.End) throw Errors.Syntax($"unexpected token: {token.Text}");
        return new(items.ToArray(), filter, limit, offset);
    }
    private long Count()
    {
        if (token.Kind != TokenKind.Integer) throw Errors.Syntax("expected nonnegative integer");
        if (!long.TryParse(Take().Text, System.Globalization.NumberStyles.None, System.Globalization.CultureInfo.InvariantCulture, out var value))
            throw Errors.Overflow();
        return value;
    }

    private Expression Expr(int minimum = 0)
    {
        Depth(nesting);
        Expression left;
        if (Eat("("))
        {
            nesting++; Depth(nesting);
            left = Expr();
            Expect(")"); nesting--;
        }
        else if (Is("-") || Is("+") || Is("not"))
        {
            var op = Take().Text;
            nesting++; Depth(nesting);
            if (op is "-" or "+" && token.Kind == TokenKind.Integer)
            {
                var text = (op == "-" ? "-" : "") + Take().Text;
                nesting--;
                left = Node("integer", text);
            }
            else
            {
                var operand = Expr(op == "not" ? 30 : 70);
                nesting--;
                left = Node(op == "-" ? "neg" : op == "+" ? "pos" : "not", "", operand);
            }
        }
        else if (Eat("cast"))
        {
            Expect("("); nesting++; Depth(nesting);
            var inner = Expr(); Expect("as");
            var target = TypeName(); Expect(")"); nesting--;
            left = Node("cast", target, inner);
        }
        else if (token.Kind == TokenKind.Integer) left = Node("integer", Take().Text);
        else if (token.Kind == TokenKind.String) left = Node("text", Take().Text);
        else if (Eat("null")) left = Node("null", "");
        else if (Is("true") || Is("false")) left = Node("boolean", Take().Text);
        else if (token.Kind is TokenKind.Word or TokenKind.QuotedName) left = Node("name", Name());
        else throw Errors.Syntax($"expected expression, found {token.Text}");

        while (true)
        {
            if (Is("::") && minimum <= 80)
            {
                Take(); left = Node("cast", TypeName(), left); continue;
            }
            if (Is("is") && minimum <= 40)
            {
                Take(); var negate = Eat("not");
                if (Eat("null")) left = Node(negate ? "is_not_null" : "is_null", "", left);
                else if (Eat("distinct"))
                {
                    Expect("from"); nesting++; Depth(nesting);
                    var right = Expr(41); nesting--;
                    left = Node(negate ? "not_distinct" : "distinct", "", left, right);
                }
                else throw Errors.Unsupported("IS predicates other than NULL/DISTINCT FROM");
                continue;
            }
            var precedence = token.Kind is TokenKind.Word or TokenKind.Symbol ? token.Text switch
            {
                "or" => 10, "and" => 20,
                "=" or "<>" or "!=" or "<" or "<=" or ">" or ">=" => 40,
                "+" or "-" => 50, "*" or "/" or "%" => 60,
                _ => -1
            } : -1;
            if (precedence < minimum) break;
            var operation = Take().Text;
            nesting++; Depth(nesting);
            var operand = Expr(precedence + 1);
            nesting--;
            left = Node(operation, "", left, operand);
        }
        return left;
    }
    private string TypeName()
    {
        var name = Name();
        if (name == "character" && Eat("varying")) name += " varying";
        if (Is("(") || Is("[")) throw Errors.Unsupported("type modifiers and array casts");
        return name;
    }
}
