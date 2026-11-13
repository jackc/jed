namespace Jed;

internal sealed record ExplainedStatement(ExplainNode Root, string Lane, Func<ValueTask<long>> Run, bool Empty = false);

internal static class ExplainExecution
{
    internal static bool Writes(StatementSyntax statement, Catalog catalog) => statement is not SelectSyntax query || SequenceExecution.Writes(query, catalog);
    internal static ExplainedStatement Prepare(StatementSyntax statement, Database database, DatabaseSnapshot snapshot,
        PendingDatabase? pending, Catalog catalog, Meter meter, CancellationToken token, QueryScope? scope = null)
    {
        if (statement is WithStatementSyntax || statement is SelectSyntax writing && WritableCteExecution.HasWrites(writing))
        {
            var definitions = statement is WithStatementSyntax wrapped ? wrapped.Definitions : ((SelectSyntax)statement).With!;
            var recursive = statement is WithStatementSyntax with ? with.Recursive : ((SelectSyntax)statement).Recursive;
            var primary = statement is WithStatementSyntax body ? body.Body : ((SelectSyntax)statement) with { With = null };
            var execution = pending ?? new PendingDatabase(snapshot) { Catalog = catalog };
            var prepared = WritableCteExecution.Prepare(database, execution, definitions, recursive, primary, meter, token);
            async ValueTask<long> RunWith()
            {
                var result = await prepared.Run().ConfigureAwait(false);
                foreach (var row in result.Rows) row.ReleaseMemory();
                return result.Rows.Length == 0 ? result.Affected : result.Rows.LongLength;
            }
            return new(prepared.Plan ?? throw new InvalidOperationException("writable WITH lacks a plan"), "buffered", RunWith);
        }
        if (statement is SelectSyntax query && !WritableCteExecution.HasWrites(query))
        {
            var prepared = RelationExecution.Query(query, database, snapshot, pending, catalog, meter, token, scope: scope);
            var root = prepared.Plan ?? throw new InvalidOperationException("query lacks retained EXPLAIN plan");
            var lane = Writes(query, catalog) ? "buffered" : query.With is not null || query.SetQuery ? "deferred" :
                root.Traverse().Any(item => item.Node.Label is "Sort" or "Aggregate" or "Window" or "Distinct" or "Nested Loop" or "Hash Join") ? "buffered" : "streaming";
            async ValueTask<long> Run()
            {
                if (prepared.Initialize is { } initialize) await initialize().ConfigureAwait(false);
                long count = 0;
                await foreach (var unused in prepared.Rows.WithCancellation(token).ConfigureAwait(false)) count = Estimate.Add(count, 1);
                return count;
            }
            return new(root, lane, Run);
        }
        if (statement is CreateTableAsSyntax create)
        {
            var body = Prepare(create.Query, database, snapshot, pending, catalog, meter, token, scope);
            if (StatementExecution.CreateTableAsExists(create, catalog))
                return new(body.Root, "buffered", () => ValueTask.FromResult(0L), Empty: true);
            return new(body.Root, "buffered", () => Mutation(statement, body.Root));
        }
        if (statement is InsertSyntax insert)
        {
            var table = StatementExecution.ResolveTable(catalog, insert.Table);
            var source = insert.Source is { } sourceQuery ? Prepare(sourceQuery, database, snapshot, pending, catalog, meter, token, scope).Root :
                new ExplainNode("Values", "rows=" + insert.Values.Length, insert.Values.Length, 0);
            var root = new ExplainNode("Insert " + table.Name, insert.Conflict is null ? "-" : insert.Conflict.Assignments is null ? "on conflict do nothing" : "on conflict do update", source.EstimatedRows, source.EstimatedCost, source);
            return new(root, "buffered", () => Mutation(statement, root));
        }
        if (statement is UpdateSyntax or DeleteSyntax)
        {
            var name = statement is UpdateSyntax update ? update.Table : ((DeleteSyntax)statement).Table;
            var table = StatementExecution.ResolveTable(catalog, name);
            var columns = StatementExecution.Columns(table);
            var syntaxFilter = statement is UpdateSyntax u ? u.Filter : ((DeleteSyntax)statement).Filter;
            var binder = new SubqueryBinder(columns, database, snapshot, pending, catalog, meter, token, scope);
            var filter = syntaxFilter is null ? null : Resolve.Expr(binder.Bind(syntaxFilter), SqlType.Boolean);
            var touched = new HashSet<int>();
            void Touch(Resolved expression)
            {
                if (expression.Op == "column") touched.Add(expression.Slot % columns.Length);
                foreach (var argument in expression.Args) Touch(argument);
            }
            if (filter is not null) Touch(filter);
            if (statement is UpdateSyntax assignments)
                foreach (var assignment in assignments.Assignments)
                    if (assignment.Value.Op != "default") Touch(Resolve.Expr(binder.Bind(assignment.Value)));
            var returning = statement is UpdateSyntax returningUpdate ? returningUpdate.Returning : ((DeleteSyntax)statement).Returning;
            if (returning.Length != 0)
                foreach (var projection in Resolve.Select(new(returning, null, null, 0), columns).Projections) Touch(projection.Expression);
            var tree = database.Tree(table, pending, snapshot);
            var access = AccessPlan.Select(table, tree, filter, new(database, snapshot, pending, catalog), meter);
            var scan = ExplainPlanning.Scan(table, access, filter, touched, false, meter, catalog, tree.Height);
            var child = filter is null ? scan : new ExplainNode("Filter", ExplainPlanning.Predicate(filter, meter.Explain!.Verbose),
                Math.Min(scan.EstimatedRows, Estimate.Rows(table, catalog, filter)),
                Estimate.Add(scan.EstimatedCost, Estimate.Multiply(scan.EstimatedRows, ExplainPlanning.Expressions([filter]))), scan);
            if (filter is not null) child.EstimatedCost = Estimate.Add(child.EstimatedCost, ExplainPlanning.Uncorrelated([filter]));
            var root = new ExplainNode((statement is UpdateSyntax ? "Update " : "Delete ") + table.Name, statement is UpdateSyntax updated ? "sets=" + updated.Assignments.Length : "-", child.EstimatedRows, child.EstimatedCost, child);
            return new(root, "buffered", () => Mutation(statement, root));
        }
        throw Errors.Unsupported("EXPLAIN statement plan");

        async ValueTask<long> Mutation(StatementSyntax mutation, ExplainNode root)
        {
            if (pending is null) throw new InvalidOperationException("ANALYZE mutation requires writer state");
            var before = meter.Accrued;
            var result = await StatementExecution.Execute(database, pending, mutation, meter, token).ConfigureAwait(false);
            root.Attribute(meter.Accrued - before);
            foreach (var row in result.Rows) row.ReleaseMemory();
            return result.Rows.Length == 0 ? result.Affected : result.Rows.LongLength;
        }
    }
}
