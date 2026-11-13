namespace Jed;

internal abstract record RelationSyntax;
internal sealed record GroupedRelation(RelationSyntax Source) : RelationSyntax;
internal sealed record LateralRelation(RelationSyntax Source) : RelationSyntax;
internal sealed record AliasedRelation(RelationSyntax Source, string Alias, string[]? ColumnAliases) : RelationSyntax;
internal sealed record SetRelation(SelectSyntax Left, SelectSyntax Right, string Operation, bool All) : RelationSyntax;
internal sealed record ValuesRelation(Expression[][] Values) : RelationSyntax;
internal sealed record TableRelation(string Name, string? Alias = null, string[]? ColumnAliases = null) : RelationSyntax
{ internal int ParseDepth { get; init; } = 1; }
internal sealed record RecordColumnSyntax(string Name, string TypeName);
internal sealed record FunctionRelation(Expression Function, string? Alias = null, string[]? ColumnAliases = null, RecordColumnSyntax[]? Definitions = null) : RelationSyntax;
internal sealed record DerivedRelation(SelectSyntax Query, string? Alias = null, string[]? ColumnAliases = null) : RelationSyntax;
internal sealed record JoinedRelation(RelationSyntax Left, RelationSyntax Right, string Kind, Expression? On, string[]? Using = null, bool Natural = false) : RelationSyntax;

internal sealed partial class Parser
{
    private RelationSyntax FromRelation()
    {
        var left = JoinedFromRelation();
        while (Eat(",")) left = new JoinedRelation(left, JoinedFromRelation(), "cross", null);
        return left;
    }
    private RelationSyntax JoinedFromRelation()
    {
        var left = RelationFactor(); if (left is GroupedRelation leading) left = leading.Source;
        while (true)
        {
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
            if ((kind == "cross" || natural) && (Is("on") || Is("using"))) throw Errors.Syntax("CROSS/NATURAL JOIN cannot have ON or USING");
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
    private bool insertValuesSource;
    private ValuesRelation ValuesBody()
    {
        Expect("values"); var rows = new List<Expression[]>();
        do
        {
            Expect("("); var values = new List<Expression>();
            do { if (Is("default") && !insertValuesSource) throw Errors.Syntax("DEFAULT is only valid in INSERT VALUES"); values.Add(Eat("default") ? Node("default", "") : Expr()); } while (Eat(","));
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
nesting++; Depth(nesting);
if (Is("select") || Is("values") || Is("with")) source = new DerivedRelation(Select());
else
{
    var group = JoinedFromRelation();
    if (group is not JoinedRelation) throw Errors.Syntax("parenthesized FROM requires a join");
    source = new GroupedRelation(group);
}
nesting--;
            Expect(")");
        }
        else
        {
            var name = QualifiedName();
            if (Eat("("))
            {
                if (DatabaseDomains.Name(name).Domain is not null) throw Errors.Syntax("qualified function calls are not supported");
                if (name == "json_table") source = new FunctionRelation(JsonTable());
                else
                {
                var args = new List<Expression>();
                if (!Eat(")"))
                {
                    do
                    {
                        if (token.Kind is TokenKind.Word or TokenKind.QuotedName && Peek().Text == "=>") { var label = Name(); Expect("=>"); args.Add(Node("named", label, Expr())); }
                        else args.Add(Expr());
                    } while (Eat(","));
                    Expect(")");
                }
                source = new FunctionRelation(Node("call", name, args.ToArray()));
                }
            }
            else source = new TableRelation(name) { ParseDepth = nesting + 1 };
        }
        var explicitAlias = Eat("as");
        var alias = explicitAlias && Is("(") ? null : explicitAlias ? Name() : token.Kind == TokenKind.QuotedName || token.Kind == TokenKind.Word && !Clause(token.Text) ? Name() : null;
        string[]? columns = null; RecordColumnSyntax[]? definitions = null;
        if ((alias is not null || explicitAlias) && Eat("("))
        {
            var names = new List<string>(); var fields = new List<RecordColumnSyntax>(); bool? typed = null;
            do
            {
                var fieldName = Name(); var hasType = !Is(",") && !Is(")");
                if (typed.HasValue && typed.Value != hasType) throw Errors.Syntax("inconsistent column definition list"); typed = hasType;
                if (hasType) fields.Add(new(fieldName, TypeName())); else names.Add(fieldName);
            } while (Eat(","));
            Expect(")");
            if (typed == true)
            {
                if (source is not FunctionRelation { Function.Text: "unnest" or "json_to_record" or "json_to_recordset" or "jsonb_to_record" or "jsonb_to_recordset" }) throw Errors.Syntax("column definition list requires a record-returning function");
                if (fields.Select(field => field.Name).Distinct(StringComparer.Ordinal).Count() != fields.Count) throw new SqlException("42701", "duplicate column name");
                definitions = fields.ToArray();
            }
            else columns = names.ToArray();
        }
        if (lateral && source is not (DerivedRelation or FunctionRelation)) throw Errors.Syntax("LATERAL requires query or function");
        source = source switch
        {
            TableRelation table => table with { Alias = alias, ColumnAliases = columns },
            FunctionRelation function => function with { Alias = alias, ColumnAliases = columns, Definitions = definitions },
            DerivedRelation derived => derived with { Alias = alias, ColumnAliases = columns },
            _ => alias is null ? source : new AliasedRelation(source, alias, columns)
        };
        return lateral ? new LateralRelation(source) : source;
    }
}
