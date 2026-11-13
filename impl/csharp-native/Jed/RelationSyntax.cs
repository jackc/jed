namespace Jed;

internal abstract record RelationSyntax;
internal sealed record LateralRelation(RelationSyntax Source) : RelationSyntax;
internal sealed record AliasedRelation(RelationSyntax Source, string Alias, string[]? ColumnAliases) : RelationSyntax;
internal sealed record SetRelation(SelectSyntax Left, SelectSyntax Right, string Operation, bool All) : RelationSyntax;
internal sealed record ValuesRelation(Expression[][] Values) : RelationSyntax;
internal sealed record TableRelation(string Name, string? Alias = null, string[]? ColumnAliases = null) : RelationSyntax;
internal sealed record FunctionRelation(Expression Function, string? Alias = null, string[]? ColumnAliases = null) : RelationSyntax;
internal sealed record DerivedRelation(SelectSyntax Query, string? Alias = null, string[]? ColumnAliases = null) : RelationSyntax;
internal sealed record JoinedRelation(RelationSyntax Left, RelationSyntax Right, string Kind, Expression? On, string[]? Using = null, bool Natural = false) : RelationSyntax;

internal sealed partial class Parser
{
    private RelationSyntax FromRelation()
    {
        var left = RelationFactor();
        while (true)
        {
            if (Eat(",")) { left = new JoinedRelation(left, RelationFactor(), "cross", null); continue; }
            var natural = Eat("natural");
            var explicitKind = Is("left") || Is("right") || Is("full") || Is("inner") || Is("cross");
            var kind = explicitKind ? Take().Text : "inner";
            if (kind is "left" or "right" or "full") Eat("outer");
            if (!Eat("join"))
            {
                if (natural || explicitKind) throw Errors.Syntax("expected JOIN");
                break;
            }
            var right = RelationFactor(); Expression? predicate = null; string[]? usingColumns = null;
            if (Eat("on")) predicate = Expr();
            else if (Eat("using"))
            {
                Expect("("); var names = new List<string>(); do { names.Add(Name()); } while (Eat(",")); Expect(")"); usingColumns = names.ToArray();
            }
            else if (kind != "cross" && !natural) throw Errors.Syntax("JOIN requires ON or USING");
            left = new JoinedRelation(left, right, kind, predicate, usingColumns, natural);
        }
        return left;
    }
    private ValuesRelation ValuesBody()
    {
        Expect("values"); var rows = new List<Expression[]>();
        do
        {
            Expect("("); var values = new List<Expression>();
            do { if (Is("default")) throw Errors.Syntax("DEFAULT is only valid in INSERT VALUES"); values.Add(Expr()); } while (Eat(","));
            Expect(")");
            if (rows.Count != 0 && values.Count != rows[0].Length) throw Errors.Syntax("VALUES rows have different lengths");
            rows.Add(values.ToArray());
        } while (Eat(","));
        return new(rows.ToArray());
    }
    private RelationSyntax RelationFactor()
    {
        var lateral = Eat("lateral");
        RelationSyntax source;
        if (Eat("("))
        {
            source = Is("select") || Is("values") || Is("with") ? new DerivedRelation(Select()) : FromRelation();
            Expect(")");
        }
        else
        {
            var name = QualifiedName();
            if (Eat("("))
            {
                var args = new List<Expression>();
                if (!Eat(")")) { do { args.Add(Expr()); } while (Eat(",")); Expect(")"); }
                source = new FunctionRelation(Node("call", name, args.ToArray()));
            }
            else source = new TableRelation(name);
        }
        var alias = Eat("as") ? Name() : token.Kind == TokenKind.QuotedName || token.Kind == TokenKind.Word && !Clause(token.Text) ? Name() : null;
        string[]? columns = null;
        if (alias is not null && Eat("(")) { var names = new List<string>(); do { names.Add(Name()); } while (Eat(",")); Expect(")"); columns = names.ToArray(); }
        if (lateral && source is not (DerivedRelation or FunctionRelation)) throw Errors.Syntax("LATERAL requires query or function");
        source = source switch
        {
            TableRelation table => table with { Alias = alias, ColumnAliases = columns },
            FunctionRelation function => function with { Alias = alias, ColumnAliases = columns },
            DerivedRelation derived => derived with { Alias = alias, ColumnAliases = columns },
            _ => alias is null ? source : new AliasedRelation(source, alias, columns)
        };
        return lateral ? new LateralRelation(source) : source;
    }
}
