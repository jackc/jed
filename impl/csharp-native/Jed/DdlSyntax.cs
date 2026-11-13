namespace Jed;

internal sealed record CreateTypeSyntax(string Name, ColumnSyntax[] Fields) : StatementSyntax;
internal sealed record DropTypeSyntax(string Name, bool IfExists, bool Cascade) : StatementSyntax;

internal sealed record ConstraintSyntax(string Kind, string? Name, string[] Columns, Expression? Expression = null,
    string? Text = null, string? Parent = null, string[]? ParentColumns = null, byte OnDelete = 0, byte OnUpdate = 0, bool MatchFull = false);
internal sealed record IndexKeySyntax(Expression Expression, string Text, byte Options);
internal sealed record CreateIndexSyntax(string? Name, string Table, IndexKeySyntax[] Keys, bool Unique, bool IfNotExists,
    string Method, Expression? Predicate = null, string? PredicateText = null) : StatementSyntax;
internal sealed record DropIndexSyntax(string Name, bool IfExists, bool Cascade) : StatementSyntax;
internal sealed record AlterBatchSyntax(AlterTableSyntax[] Actions) : StatementSyntax;
internal sealed record AlterTableSyntax(string Table, string Operation, string? Column = null, string? Name = null,
    Expression? Expression = null, string? Text = null, ColumnSyntax? Definition = null, ConstraintSyntax? Constraint = null,
    bool IfExists = false, bool Cascade = false, ConstraintSyntax[]? AddedConstraints = null, SequenceOptions? Sequence = null) : StatementSyntax;

internal sealed partial class Parser
{
    internal static Expression ParseExpression(string text)
    {
        var parser = new Parser(text); var expression = parser.Expr();
        if (parser.token.Kind != TokenKind.End) throw Errors.Syntax("unexpected token after expression");
        return expression;
    }
    private (Expression Expression, string Text) CapturedExpression()
    {
        var previous = capturedTokens; var capture = new List<Token>(); capturedTokens = capture;
        try { var expression = Expr(); return (expression, string.Join(' ', capture.Select(RenderToken))); }
        catch (SqlException error) when (error.SqlState == SqlStates.SyntaxError && capture.Any(token => token.Kind == TokenKind.Word && token.Text == "select"))
        { throw Errors.Unsupported("subquery in stored expression"); }
        finally { capturedTokens = previous; previous?.AddRange(capture); }
    }
    internal static string RenderToken(Token token) => token.Kind switch
    {
        TokenKind.QuotedName => "\"" + token.Text.Replace("\"", "\"\"", StringComparison.Ordinal) + "\"",
        TokenKind.String => "'" + token.Text.Replace("'", "''", StringComparison.Ordinal) + "'",
        TokenKind.Parameter => "$" + token.Text,
        TokenKind.Integer => token.Text.TrimStart('0') is { Length: > 0 } integer ? integer : "0",
        TokenKind.Decimal => RenderDecimal(token.Text),
        _ => token.Text == "!=" ? "<>" : token.Text
    };
    private static string RenderDecimal(string text)
    {
        var value = ExactDecimal.Parse(text).ToString();
        if (!value.Contains('.')) value += ".";
        return value.StartsWith("0.", StringComparison.Ordinal) ? value[1..] : value;
    }
    private string[] ColumnNames()
    {
        Expect("("); var result = new List<string>(); do { result.Add(Name()); } while (Eat(",")); Expect(")"); return result.ToArray();
    }
    private StatementSyntax CreateDefinition()
    {
        if (Eat("materialized")) throw Errors.Unsupported("materialized views");
        var replaceView = Eat("or"); if (replaceView) Expect("replace");
        if (Eat("view")) return CreateView(replaceView);
        if (replaceView) throw Errors.Syntax("expected VIEW");
        if (Eat("sequence")) return CreateSequence();
        if (Eat("type"))
        {
            var name = QualifiedName(); Expect("as"); Expect("("); var fields = new List<ColumnSyntax>();
            do
            {
                var field = Name(); var type = TypeName(); var notNull = Eat("not"); if (notNull) Expect("null");
                fields.Add(new(field, type, notNull, false, null));
            } while (Eat(","));
            Expect(")"); return new CreateTypeSyntax(name, fields.ToArray());
        }
        var unique = Eat("unique");
        if (Eat("index"))
        {
            var optional = Eat("if"); if (optional) { Expect("not"); Expect("exists"); }
            var name = Is("on") ? null : QualifiedName(); if (optional && name is null) throw Errors.Syntax("IF NOT EXISTS requires an index name");
            Expect("on"); var table = QualifiedName(); var method = Eat("using") ? Name() : "btree";
            Expect("("); var keys = new List<IndexKeySyntax>();
            do
            {
                var parenthesized = Is("("); var (expression, text) = CapturedExpression();
                if (!parenthesized && expression.Op is not ("name" or "call")) throw Errors.Syntax("index expression requires parentheses");
                var descending = Eat("desc"); if (!descending) Eat("asc");
                var first = descending;
                if (Eat("nulls")) { if (Eat("first")) first = true; else { Expect("last"); first = false; } }
                keys.Add(new(expression, text, (byte)((descending ? 1 : 0) | (first ? 2 : 0))));
            } while (Eat(","));
            Expect(")"); Expression? predicate = null; string? predicateText = null;
            if (Eat("where")) (predicate, predicateText) = CapturedExpression();
            return new CreateIndexSyntax(name, table, keys.ToArray(), unique, optional, method, predicate, predicateText);
        }
        if (unique) throw Errors.Syntax("expected INDEX after UNIQUE");
        Expect("table"); var ifAbsent = Eat("if"); if (ifAbsent) { Expect("not"); Expect("exists"); }
        var tableName = QualifiedName();
        if (Is("as")) return TableAs(tableName, null, ifAbsent);
        Expect("(");
        if (token.Kind is TokenKind.Word or TokenKind.QuotedName && Peek().Text is "," or ")")
        {
            var aliases = new List<string>(); do { aliases.Add(Name()); } while (Eat(",")); Expect(")");
            return TableAs(tableName, aliases.ToArray(), ifAbsent);
        }
        var columns = new List<ColumnSyntax>(); var constraints = new List<ConstraintSyntax>();
        do
        {
            if (IsTableConstraint()) constraints.Add(ReadConstraint(null));
            else columns.Add(ColumnDefinition(constraints));
        } while (Eat(","));
        Expect(")");
        return new CreateTableSyntax(tableName, columns.ToArray(), [], ifAbsent, constraints.ToArray());
    }
    private bool IsTableConstraint() => Is("constraint") || Is("primary") && Peek().Text == "key" || Is("unique") && Peek().Text == "(" ||
        Is("check") && Peek().Text == "(" || Is("foreign") && Peek().Text == "key";
    private ColumnSyntax ColumnDefinition(List<ConstraintSyntax> constraints)
    {
        var name = Name(); var type = TypeName(); var notNull = false; var primary = false;
        Expression? value = null; string? defaultText = null; var identity = false; var always = false; SequenceOptions? sequence = null;
        while (true)
        {
            if (Eat("not")) { Expect("null"); notNull = true; }
            else if (Eat("null")) { }
            else if (Eat("default")) { if (value is not null || identity) throw Errors.Syntax("multiple column defaults"); (value, defaultText) = CapturedExpression(); }
            else if (Eat("generated"))
            {
                if (identity || value is not null) throw Errors.Syntax("multiple column defaults or identities");
                identity = true; always = Eat("always"); if (!always) { Expect("by"); Expect("default"); }
                Expect("as"); Expect("identity");
                if (Eat("(")) { sequence = ReadSequenceOptions(false, true); Expect(")"); }
            }
            else if (Is("primary"))
            {
                Expect("primary"); Expect("key"); if (!primary) constraints.Add(new("primary", null, [name])); primary = true; notNull = true;
            }
            else if (Is("check") || Is("unique") || Is("constraint") || Is("references")) constraints.Add(ReadConstraint(name));
            else break;
        }
        return new(name, type, notNull, false, value, defaultText, identity, always, sequence);
    }
    private ConstraintSyntax ReadConstraint(string? column)
    {
        var name = Eat("constraint") ? Name() : null;
        if (Eat("check"))
        {
            Expect("("); var (expression, text) = CapturedExpression(); Expect(")");
            return new("check", name, column is null ? [] : [column], expression, text);
        }
        if (Eat("primary")) { Expect("key"); return new("primary", name, column is null ? ColumnNames() : [column]); }
        if (Eat("unique")) return new("unique", name, column is null ? ColumnNames() : [column]);
        var local = column is null ? ReadForeignColumns() : new[] { column };
        Expect("references"); var parent = QualifiedName(); var remote = Is("(") ? ColumnNames() : null;
        byte delete = 0, update = 0; var full = false; var sawDelete = false; var sawUpdate = false;
        if (Eat("match")) { if (Eat("full")) full = true; else Expect("simple"); }
        while (Eat("on"))
        {
            var deleting = Eat("delete"); if (!deleting) Expect("update");
            if (deleting ? sawDelete : sawUpdate) throw Errors.Syntax("duplicate referential action");
            var action = ReferentialAction();
            if (deleting) { delete = action; sawDelete = true; } else { update = action; sawUpdate = true; }
        }
        return new("foreign", name, local, Parent: parent, ParentColumns: remote, OnDelete: delete, OnUpdate: update, MatchFull: full);
    }
    private string[] ReadForeignColumns() { Expect("foreign"); Expect("key"); return ColumnNames(); }
    private byte ReferentialAction()
    {
        if (Eat("no")) { Expect("action"); return 0; }
        if (Eat("restrict")) return 1;
        if (Eat("cascade")) return 2;
        Expect("set"); if (Eat("null")) return 3; Expect("default"); return 4;
    }
    private StatementSyntax AlterDefinition()
    {
        if (Eat("view")) return AlterView();
        if (Eat("sequence")) return AlterSequence();
        Expect("table"); var table = QualifiedName();
        var actions = new List<AlterTableSyntax> { AlterAction(table) };
        if (actions[0].Operation.StartsWith("rename", StringComparison.Ordinal)) return actions[0];
        while (Eat(",")) actions.Add(AlterAction(table));
        return actions.Count == 1 ? actions[0] : new AlterBatchSyntax(actions.ToArray());
    }
    private AlterTableSyntax AlterAction(string table)
    {
        if (Eat("rename"))
        {
            if (Eat("to")) return new AlterTableSyntax(table, "rename", Name: Name());
            var constraint = Eat("constraint"); if (!constraint) Eat("column");
            var column = Name(); Expect("to"); return new AlterTableSyntax(table, constraint ? "rename_constraint" : "rename_column", column, Name());
        }
        if (Eat("add"))
        {
            if (IsTableConstraint()) return new AlterTableSyntax(table, "add_constraint", Constraint: ReadConstraint(null));
            Eat("column"); var optional = Eat("if"); if (optional) { Expect("not"); Expect("exists"); }
            var constraints = new List<ConstraintSyntax>(); var column = ColumnDefinition(constraints);
            return new AlterTableSyntax(table, "add_column", Definition: column, IfExists: optional, AddedConstraints: constraints.ToArray());
        }
        if (Eat("drop"))
        {
            if (Eat("primary")) { Expect("key"); var all = Eat("cascade"); if (!all) Eat("restrict"); return new AlterTableSyntax(table, "drop_primary", Cascade: all); }
            var constraint = Eat("constraint"); if (!constraint) Eat("column");
            var optional = Eat("if"); if (optional) Expect("exists"); var name = Name(); var cascade = Eat("cascade"); if (!cascade) Eat("restrict");
            return new AlterTableSyntax(table, constraint ? "drop_constraint" : "drop_column", Column: name, IfExists: optional, Cascade: cascade);
        }
        Expect("alter"); Eat("column"); var target = Name();
        if (Eat("add")) return AddIdentity(table, target);
        if (Is("restart")) return IdentityChanges(table, target, false);
        if (Eat("set"))
        {
            if (Eat("not")) { Expect("null"); return new AlterTableSyntax(table, "not_null", target); }
            if (Eat("data")) return AlterType(table, target);
            if (!Is("default")) return IdentityChanges(table, target, true);
            Expect("default"); var (expression, text) = CapturedExpression(); return new AlterTableSyntax(table, "default", target, Expression: expression, Text: text);
        }
        if (Eat("drop"))
        {
            if (Eat("not")) { Expect("null"); return new AlterTableSyntax(table, "nullable", target); }
            if (Eat("identity")) { var optional = Eat("if"); if (optional) Expect("exists"); return new AlterTableSyntax(table, "drop_identity", target, IfExists: optional); }
            Expect("default"); return new AlterTableSyntax(table, "drop_default", target);
        }
        return AlterType(table, target);
    }
    private AlterTableSyntax AlterType(string table, string target)
    {
        Expect("type"); var type = TypeName();
        var expression = Eat("using") ? Expr() : null;
        return new AlterTableSyntax(table, "type", target, Name: type, Expression: expression);
    }
}
