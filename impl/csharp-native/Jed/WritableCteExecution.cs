using System.Runtime.CompilerServices;

namespace Jed;

internal sealed record WritableCtePlan(BoundColumn[] Columns, Func<ValueTask<StatementResult>> Run, ExplainNode? Plan = null);

internal static class WritableCteExecution
{
    internal static bool HasWrites(SelectSyntax query) => query.With?.Any(definition => definition.Mutation is not null) == true;
    internal static async IAsyncEnumerable<SqlValue[]> Rows(WritableCtePlan plan)
    {
        var result = await plan.Run().ConfigureAwait(false);
        foreach (var row in result.Rows) yield return Enumerable.Range(0, row.Count).Select(index => row[index]).ToArray();
    }
    internal static WritableCtePlan Prepare(Database database, PendingDatabase pending, CteSyntax[] definitions, bool recursive,
        StatementSyntax primary, Meter meter, CancellationToken token)
    {
        var read = pending.Catalog.Scope is { } domains
            ? DomainExecution.ReadPending(domains.Fork(), domains.Items.FirstOrDefault(domain => ReferenceEquals(domain.Pending, pending))?.Name ?? "main") : pending.Fork();
        var scope = new QueryScope(); var context = new StatementContext(read, scope);
        var steps = new List<Func<ValueTask>>(); var releases = new List<Action>(); var inference = new Parameters();
        var explanations = new List<(ExplainNode Node, bool Executed)>();
        for (var i = 0; i < definitions.Length; i++)
        {
            var definition = definitions[i]; PreparedRelation body;
            var references = References(primary, definition.Name) + definitions.Skip(i + 1).Sum(later => References(later.Mutation ?? later.Query, definition.Name));
            var materialized = definition.Mutation is not null;
            if (definition.Mutation is { } mutation)
            {
                var columns = inference.Infer(mutation, database, read.Before, read, read.Catalog, meter, token, scope);
                var partContext = context.InScope(scope);
                var mergePlan = mutation is MergeSyntax merge ? StatementExecution.PrepareMerge(database, pending, merge, meter, token, partContext) : null;
                var explained = mergePlan is not null || meter.Explain is null ? null : ExplainExecution.Prepare(mutation, database, read.Before, pending, read.Catalog, meter, token, scope);
                var mutationPlan = mergePlan?.Plan ?? explained?.Root;
                async ValueTask<StatementResult> ExecuteMutation()
                {
                    var beforeCost = meter.Accrued;
                    var result = mergePlan is not null ? await mergePlan.Run().ConfigureAwait(false) : explained?.Mutation is { } execute
                        ? await execute(partContext).ConfigureAwait(false) : await StatementExecution.Execute(database, pending, mutation, meter, token, partContext).ConfigureAwait(false);
                    mutationPlan?.SetInclusive(meter.Accrued - beforeCost); return result;
                }
                var buffer = new MutationBuffer(ExecuteMutation, meter);
                steps.Add(buffer.Initialize); releases.Add(buffer.Release);
                body = new(columns, buffer.Read(token), NoReturning: columns.Length == 0, MemoryOutput: true, Plan: mutationPlan);
            }
            else
            {
                var isRecursive = recursive && CteBuffer.References(definition.Query, definition.Name) > 0;
                if (!isRecursive && (definition.Search is not null || definition.Cycle is not null)) throw Errors.Syntax("SEARCH/CYCLE query is not recursive");
                body = isRecursive ? RecursiveExecution.Prepare(definition, database, read.Before, read, read.Catalog, meter, token, scope) :
                    RelationExecution.Query(definition.Query, database, read.Before, read, read.Catalog, meter, token, scope: scope);
                materialized = references > 0 && (isRecursive || definition.Materialized == true || definition.Materialized is null && references > 1);
                if (materialized)
                {
                    var buffer = new CteBuffer(body, meter); steps.Add(() => buffer.Materialize(token)); releases.Add(buffer.Release);
                    body = body with { Rows = buffer.Read(token), MemoryOutput = true };
                }
            }
            var mutationReference = primary is not SelectSyntax && References(primary, definition.Name) > 0 ||
                definitions.Any(later => later.Mutation is { } mutation && References(mutation, definition.Name) > 0);
            if (materialized || references != 1 || mutationReference) body = body with { PushFilter = null, BaseAccess = null };
            if (meter.Explain is not null && body.Plan is { } definitionPlan)
            {
                var inlined = references > 0 && !materialized;
                explanations.Add((new("CTE " + definition.Name, materialized ? "materialized" : "inlined",
                    definitionPlan.EstimatedRows, inlined ? 0 : definitionPlan.EstimatedCost, definitionPlan), materialized));
                body = body with { Cte = new(definition.Name, materialized, definitionPlan, DefinitionVisible: true) };
            }
            body = body with { Columns = RelationExecution.Alias(body.Columns, definition.Name, definition.Columns) };
            var nextScope = new QueryScope(scope); nextScope.Add(definition.Name, body); scope = nextScope;
        }
        PreparedRelation? selected = null; BoundColumn[] output;
        if (primary is SelectSyntax query)
        {
            selected = RelationExecution.Query(query, database, read.Before, read, read.Catalog, meter, token, scope: scope);
            output = selected.Columns;
        }
        else output = inference.Infer(primary, database, read.Before, read, read.Catalog, meter, token, scope);
        var primaryPlan = meter.Explain is null ? null : selected?.Plan ??
            ExplainExecution.Prepare(primary, database, read.Before, read, read.Catalog, meter, token, scope).Root;
        var rootPlan = primaryPlan is null ? null : new ExplainNode("WITH", "ctes=" + definitions.Length,
            primaryPlan.EstimatedRows, explanations.Where(item => item.Executed).Aggregate(primaryPlan.EstimatedCost, (sum, item) => Estimate.Add(sum, item.Node.EstimatedCost)),
            explanations.Select(item => item.Node).Append(primaryPlan).ToArray())
            {
                SumChildren = false,
                HiddenActual = () => explanations.Where(item => item.Executed).Aggregate(primaryPlan.ActualCost, (sum, item) => Estimate.Add(sum, item.Node.ActualCost))
            };
        async ValueTask<StatementResult> Run()
        {
            try
            {
            foreach (var step in steps) await step().ConfigureAwait(false);
            StatementResult result;
            if (selected is not null)
            {
                if (selected.Initialize is not null) await selected.Initialize().ConfigureAwait(false);
                var rows = new List<Row>();
                await foreach (var values in selected.Rows.WithCancellation(token).ConfigureAwait(false)) rows.Add(new Row(values).RetainMemory(meter));
                result = new(output.Select(column => new Column(column.Name, column.Type)).ToArray(), rows.ToArray());
            }
            else
            {
                var beforeCost = meter.Accrued;
                result = await StatementExecution.Execute(database, pending, primary, meter, token, context.InScope(scope)).ConfigureAwait(false);
                primaryPlan?.SetInclusive(meter.Accrued - beforeCost);
            }
            await context.Settle().ConfigureAwait(false);
            return result;
            }
            finally { foreach (var release in releases) release(); }
        }
        return new(output, Run, rootPlan);
    }
    private sealed class MutationBuffer(Func<ValueTask<StatementResult>> run, Meter meter)
    {
        private Row[]? rows;
        internal void Release() { if (rows is not null) foreach (var row in rows) row.ReleaseMemory(); rows = null; }
        internal async ValueTask Initialize()
        {
            if (rows is not null) return;
            rows = (await run().ConfigureAwait(false)).Rows;
        }
        internal async IAsyncEnumerable<SqlValue[]> Read([EnumeratorCancellation] CancellationToken token)
        {
            await Initialize().ConfigureAwait(false);
            foreach (var row in rows!)
            {
                meter.Charge(Costs.CteScanRow); meter.Checkpoint(token);
                var values = Enumerable.Range(0, row.Count).Select(index => row[index]).ToArray();
                meter.Memory.ReserveRow(values); yield return values;
            }
        }
    }
    private static int References(StatementSyntax statement, string name)
    {
        int ExpressionReferences(Expression expression) => expression.Args.Sum(ExpressionReferences) +
            (expression.Predicate is null ? 0 : ExpressionReferences(expression.Predicate)) + (expression.Query is null ? 0 : CteBuffer.References(expression.Query, name));
        int ReturningReferences(SelectItem[] items) => items.Sum(item => ExpressionReferences(item.Expression));
        return statement switch
        {
            SelectSyntax query => CteBuffer.References(query, name),
            MergeSyntax merge => CteBuffer.References(new SelectSyntax([new(new("star", "", []), null)], null, null, 0, Relation: merge.Source), name) + ExpressionReferences(merge.On) + ReturningReferences(merge.Returning) + merge.Clauses.Sum(clause => (clause.Condition is null ? 0 : ExpressionReferences(clause.Condition)) + (clause.Values ?? []).Sum(ExpressionReferences) + (clause.Assignments ?? []).Sum(item => ExpressionReferences(item.Value))),
            InsertSyntax insert => (insert.Source is null ? 0 : CteBuffer.References(insert.Source, name)) + insert.Values.Sum(row => row.Sum(ExpressionReferences)) +
                ReturningReferences(insert.Returning) + (insert.Conflict?.Assignments ?? []).Sum(assignment => ExpressionReferences(assignment.Value)) +
                (insert.Conflict?.Filter is { } filter ? ExpressionReferences(filter) : 0),
            UpdateSyntax update => update.Assignments.Sum(assignment => ExpressionReferences(assignment.Value)) + (update.Filter is null ? 0 : ExpressionReferences(update.Filter)) + ReturningReferences(update.Returning),
            DeleteSyntax delete => (delete.Filter is null ? 0 : ExpressionReferences(delete.Filter)) + ReturningReferences(delete.Returning),
            _ => 0
        };
    }
}
