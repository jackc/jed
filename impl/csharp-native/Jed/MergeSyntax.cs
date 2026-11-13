namespace Jed;

internal sealed record MergeSyntax(string Table, string? Alias, RelationSyntax Source, Expression On,
    MergeClauseSyntax[] Clauses, SelectItem[] Returning, ReturningAliases? ReturningAliases = null, ViewWritePlan? View = null) : StatementSyntax;
internal sealed record MergeClauseSyntax(string Match, Expression? Condition, string Action,
    AssignmentSyntax[]? Assignments = null, string[]? Columns = null, Expression[]? Values = null, string? Overriding = null);

internal sealed partial class Parser
{
    private MergeSyntax MergeStatement()
    {
        Expect("into"); var table = QualifiedName();
        var alias = Eat("as") ? Name() : !Is("using") ? Name() : null;
        Expect("using"); var source = JoinedFromRelation(); Expect("on"); var on = Expr();
        var clauses = new List<MergeClauseSyntax>(); var unconditional = new HashSet<string>(StringComparer.Ordinal);
        while (Eat("when"))
        {
            var match = Eat("not") ? "target" : "matched"; Expect("matched");
            if (match == "target" && Eat("by")) match = Eat("source") ? "source" : Eat("target") ? "target" : throw Errors.Syntax("expected SOURCE or TARGET");
            if (unconditional.Contains(match)) throw Errors.Syntax("unreachable MERGE clause");
            var condition = Eat("and") ? Expr() : null; Expect("then");
            if (condition is null) unconditional.Add(match);
            if (Eat("do")) { Expect("nothing"); clauses.Add(new(match, condition, "nothing")); continue; }
            if (Eat("delete"))
            {
                if (match == "target") throw Errors.Syntax("DELETE requires a target row");
                clauses.Add(new(match, condition, "delete")); continue;
            }
            if (Eat("update"))
            {
                if (match == "target") throw Errors.Syntax("UPDATE requires a target row");
                Expect("set"); var assignments = new List<AssignmentSyntax>();
                do
                {
                    var column = Name();
                    if (Eat(".")) throw new SqlException(SqlStates.UndefinedColumn, "MERGE assignment column cannot be qualified");
                    Expect("="); assignments.Add(new(column, Eat("default") ? Node("default", "") : Expr()));
                } while (Eat(","));
                clauses.Add(new(match, condition, "update", assignments.ToArray())); continue;
            }
            Expect("insert"); if (match != "target") throw Errors.Syntax("INSERT requires an unmatched source row");
            string[]? columns = null; string? overriding = null;
            if (Eat("(")) { var names = new List<string>(); do { names.Add(Name()); } while (Eat(",")); Expect(")"); columns = names.ToArray(); }
            if (Eat("overriding")) { overriding = Eat("system") ? "system" : Eat("user") ? "user" : throw Errors.Syntax("expected SYSTEM or USER"); Expect("value"); }
            var values = new List<Expression>();
            if (Eat("default")) { if (columns is not null) throw Errors.Syntax("DEFAULT VALUES cannot have a column list"); Expect("values"); }
            else
            {
                Expect("values"); Expect("(");
                do { values.Add(Eat("default") ? Node("default", "") : Expr()); } while (Eat(","));
                Expect(")");
            }
            clauses.Add(new(match, condition, "insert", Columns: columns, Values: values.ToArray(), Overriding: overriding));
        }
        if (clauses.Count == 0) throw Errors.Syntax("MERGE requires WHEN");
        var returning = Returning(); return new(table, alias, source, on, clauses.ToArray(), returning.Items, returning.Aliases);
    }
}
