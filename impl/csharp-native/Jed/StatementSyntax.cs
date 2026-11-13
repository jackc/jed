namespace Jed;

internal sealed record ColumnSyntax(string Name, string Type, bool NotNull, bool PrimaryKey, Expression? Default, string? DefaultText = null, bool Identity = false, bool IdentityAlways = false, SequenceOptions? Sequence = null, string? Collation = null);
internal sealed record CreateTableSyntax(string Name, ColumnSyntax[] Columns, string[] PrimaryKey, bool IfNotExists, ConstraintSyntax[]? Constraints = null, bool Temporary = false, byte OnCommit = 0) : StatementSyntax;
internal sealed record DropTablesSyntax(string[] Names, bool IfExists, bool Cascade = false) : StatementSyntax;
internal sealed record DropTableSyntax(string Name, bool IfExists, bool Cascade = false) : StatementSyntax;
internal sealed record InsertSyntax(string Table, string[]? Columns, Expression[][] Values, SelectItem[] Returning, SelectSyntax? Source = null, string? Overriding = null, ConflictSyntax? Conflict = null, string? Alias = null, Expression[]? ViewChecks = null, ViewBinding? ViewBinding = null, Dictionary<int, Expression>? ViewDefaults = null, ReturningAliases? ReturningAliases = null) : StatementSyntax;
internal sealed record ReturningAliases(string? Old, string? New);
internal sealed record AssignmentSyntax(string Column, Expression Value);
internal sealed record UpdateSyntax(string Table, AssignmentSyntax[] Assignments, Expression? Filter, SelectItem[] Returning, Expression[]? ViewChecks = null, ViewBinding? ViewBinding = null, ReturningAliases? ReturningAliases = null) : StatementSyntax;
internal sealed record DeleteSyntax(string Table, Expression? Filter, SelectItem[] Returning, ViewBinding? ViewBinding = null, ReturningAliases? ReturningAliases = null) : StatementSyntax;
internal sealed record TransactionSyntax(string Operation, string? Name = null, bool ReadOnly = false) : StatementSyntax;

internal sealed partial class Parser
{
    private StatementSyntax OtherStatement()
    {
        if (Eat("explain")) return ExplainStatement();
        if (Eat("analyze")) return AnalyzeStatement();
        if (Eat("reindex")) return ReindexStatement();
        if (Eat("create")) return CreateDefinition();
        if (Eat("alter")) return AlterDefinition();
        if (Eat("drop"))
        {
            if (Eat("view")) return DropView();
            if (Eat("sequence")) return DropSequence();
            if (Eat("type"))
            {
                var optional = Eat("if"); if (optional) Expect("exists"); var name = QualifiedName();
                var cascade = Eat("cascade"); if (!cascade) Eat("restrict"); return new DropTypeSyntax(name, optional, cascade);
            }
            if (Eat("index"))
            {
                var optional = Eat("if"); if (optional) Expect("exists");
                var name = QualifiedName(); var cascade = Eat("cascade"); if (!cascade) Eat("restrict");
                return new DropIndexSyntax(name, optional, cascade);
            }
            Expect("table"); var ifExists = Eat("if"); if (ifExists) Expect("exists");
            var tables = new List<string>(); do { tables.Add(QualifiedName()); } while (Eat(","));
            var all = Eat("cascade"); if (!all) Eat("restrict");
            return tables.Count == 1 ? new DropTableSyntax(tables[0], ifExists, all) : new DropTablesSyntax(tables.ToArray(), ifExists, all);
        }
        if (Eat("insert"))
        {
            Expect("into"); var name = QualifiedName(); var alias = Eat("as") ? Name() : null; string[]? columns = null;
            if (Is("(") && Peek().Text is not ("select" or "with" or "values" or "(")) { Take(); var names = new List<string>(); do { names.Add(Name()); } while (Eat(",")); Expect(")"); columns = names.ToArray(); }
            string? overriding = null;
            if (Eat("overriding")) { overriding = Eat("system") ? "system" : Eat("user") ? "user" : throw Errors.Syntax("expected SYSTEM or USER"); Expect("value"); }
            var values = new List<Expression[]>(); SelectSyntax? source = null;
            if (Eat("default")) { if (columns is not null) throw Errors.Syntax("DEFAULT VALUES cannot have a column list"); Expect("values"); values.Add([]); }
            else if (Is("select") || Is("with") || Is("(")) source = Select();
            else
            {
                Expect("values");
                do
                {
                    Expect("("); var row = new List<Expression>();
                    do { row.Add(Eat("default") ? Node("default", "") : Expr()); } while (Eat(","));
                    Expect(")"); values.Add(row.ToArray());
                } while (Eat(","));
            }
            var conflict = OnConflict();
            var returning = Returning();
            return new InsertSyntax(name, columns, values.ToArray(), returning.Items, source, overriding, conflict, alias, ReturningAliases: returning.Aliases);
        }
        if (Eat("update"))
        {
            var name = QualifiedName(); Expect("set"); var assignments = new List<AssignmentSyntax>();
            do { var column = Name(); Expect("="); assignments.Add(new(column, Eat("default") ? Node("default", "") : Expr())); } while (Eat(","));
            var filter = Eat("where") ? Expr() : null;
            var returning = Returning();
            return new UpdateSyntax(name, assignments.ToArray(), filter, returning.Items, ReturningAliases: returning.Aliases);
        }
        if (Eat("delete"))
        {
            Expect("from"); var name = QualifiedName(); var filter = Eat("where") ? Expr() : null;
            var returning = Returning();
            return new DeleteSyntax(name, filter, returning.Items, ReturningAliases: returning.Aliases);
        }
        if (Eat("begin")) { if (!Eat("transaction")) Eat("work"); return BeginTransaction(); }
        if (Eat("start")) { Expect("transaction"); return BeginTransaction(); }
        if (Eat("commit") || Eat("end")) { if (!Eat("transaction")) Eat("work"); return new TransactionSyntax("commit"); }
        if (Eat("rollback"))
        {
            if (!Eat("transaction")) Eat("work");
            return Eat("to") ? new TransactionSyntax("rollback_to", SavepointName()) : new TransactionSyntax("rollback");
        }
        if (Eat("savepoint")) return new TransactionSyntax("savepoint", Name());
        if (Eat("release")) return new TransactionSyntax("release", SavepointName());
        throw Errors.Syntax($"unexpected statement: {token.Text}");
    }
    private TransactionSyntax BeginTransaction()
    {
        var readOnly = false;
        if (Eat("read")) { readOnly = Eat("only"); if (!readOnly) Expect("write"); }
        return new("begin", ReadOnly: readOnly);
    }
    private string SavepointName()
    {
        var name = Name();
        if (name == "savepoint" && token.Kind is TokenKind.Word or TokenKind.QuotedName) name = Name();
        return name;
    }
    private (SelectItem[] Items, ReturningAliases? Aliases) Returning()
    {
        if (!Eat("returning")) return ([], null);
        ReturningAliases? aliases = null;
        if (Is("with") && Peek().Text == "(" && Peek(2).Text is "old" or "new" && Peek(3).Text == "as")
        {
            Take(); Expect("("); string? old = null, current = null;
            do
            {
                var version = Name();
                if (version is not ("old" or "new")) throw Errors.Syntax("expected OLD or NEW");
                Expect("as"); var alias = Name();
                if (version == "old") { if (old is not null) throw Errors.Syntax("duplicate OLD alias"); old = alias; }
                else { if (current is not null) throw Errors.Syntax("duplicate NEW alias"); current = alias; }
            } while (Eat(","));
            Expect(")"); aliases = new(old, current);
        }
        var items = new List<SelectItem>();
        do
        {
            var expression = Eat("*") ? Node("star", "") : Expr();
            items.Add(new(expression, Eat("as") ? Name() : null));
        } while (Eat(","));
        return (items.ToArray(), aliases);
    }
}
