using System.Runtime.CompilerServices;

namespace Jed;

internal static class RecursiveExecution
{
    internal static PreparedRelation Prepare(CteSyntax definition, Database database, DatabaseSnapshot snapshot,
        PendingDatabase? pending, Catalog catalog, Meter meter, CancellationToken cancellationToken, QueryScope scope)
    {
        var body = definition.Query;
        if (body.Relation is not SetRelation { Operation: "union" } set)
            throw Invalid("recursive CTE requires an anchor UNION recursive term");
        if (CteBuffer.References(set.Left, definition.Name) != 0) throw Invalid("recursive reference in anchor");
        if (CteBuffer.References(set.Right, definition.Name) != 1) throw Invalid("recursive term must reference itself once");
        if (body.Order is { Length: > 0 } || body.Limit is not null || body.Offset != 0) throw Errors.Unsupported("recursive body ordering or limit");
        if (set.Right.SetQuery) throw Errors.Unsupported("set operation in recursive term");
        ValidateTerm(set.Right, definition.Name);
        var anchor = RelationExecution.Query(set.Left, database, snapshot, pending, catalog, meter, cancellationToken, scope: scope);
        var columns = RelationExecution.Alias(anchor.Columns, definition.Name, definition.Columns);
        var working = new WorkingRows(meter);
        var stepScope = new QueryScope(scope);
        stepScope.Add(definition.Name, new(columns, working.Read(cancellationToken), MemoryOutput: true));
        var step = RelationExecution.Query(set.Right, database, snapshot, pending, catalog, meter, cancellationToken, scope: stepScope);
        if (step.Columns.Length != columns.Length) throw Errors.Syntax("recursive term has different column count");
        for (var i = 0; i < columns.Length; i++)
        {
            if (Resolve.Common(columns[i].Type, step.Columns[i].Type) != columns[i].Type)
                throw Errors.Type("recursive term widens its anchor column type");
            if (!set.All) Resolve.RequireOrderable(columns[i].Type);
            var common = CollationRules.Merge(new[] { columns[i].Collation, step.Columns[i].Collation })?.Implicit();
            var anchorCollation = columns[i].Collation?.Implicit();
            if (common?.Name != anchorCollation?.Name || (common?.Conflict ?? false) != (anchorCollation?.Conflict ?? false))
                throw new SqlException("42P21", "recursive term changes its anchor collation");
        }
        async ValueTask Constants()
        {
            if (anchor.InitializeConstants is { } first) await first().ConfigureAwait(false);
            if (step.InitializeConstants is { } second) await second().ConfigureAwait(false);
        }
        return new(columns, Run(anchor, step, working, columns, set.All, meter, cancellationToken),
            Dependencies: (anchor.Dependencies ?? []).Concat(step.Dependencies ?? []).Distinct().ToArray(), InitializeConstants: Constants, MemoryOutput: true);
    }
    private sealed class WorkingRows(Meter meter)
    {
        internal List<SqlValue[]> Rows = [];
        internal async IAsyncEnumerable<SqlValue[]> Read([EnumeratorCancellation] CancellationToken cancellationToken)
        {
            await ValueTask.CompletedTask.ConfigureAwait(false);
            foreach (var row in Rows) { meter.Charge(Costs.CteScanRow); meter.Checkpoint(cancellationToken); meter.Memory.ReserveRow(row); yield return row.ToArray(); }
        }
    }
    private static async IAsyncEnumerable<SqlValue[]> Run(PreparedRelation anchor, PreparedRelation step, WorkingRows working,
        BoundColumn[] columns, bool all, Meter meter, [EnumeratorCancellation] CancellationToken cancellationToken)
    {
        var seen = new HashSet<string>(StringComparer.Ordinal); long seenBytes = 0;
        async ValueTask<List<SqlValue[]>> Collect(PreparedRelation source)
        {
            var result = new List<SqlValue[]>();
            if (source.Initialize is { } initialize) await initialize().ConfigureAwait(false);
            await foreach (var values in source.Rows.WithCancellation(cancellationToken).ConfigureAwait(false))
            {
                var row = values.Select((value, i) => Resolve.Cast(value, columns[i].Type)).ToArray();
                if (!all)
                {
                    foreach (var value in row) GroupExecution.ChargeKey(value, meter);
                    if (!seen.Add(GroupExecution.Identity(row)))
                    { if (source.MemoryOutput && meter.Memory.Enabled) meter.Memory.Release(MemorySize.RowBytes(values)); continue; }
                    if (meter.Memory.Enabled) { var bytes = MemorySize.EntryBytes(row); meter.Memory.Reserve(bytes); seenBytes = Meter.Add(seenBytes, bytes); }
                }
                if (meter.Memory.Enabled)
                {
                    var bytes = MemorySize.RowBytes(row);
                    if (!source.MemoryOutput) meter.Memory.Reserve(bytes);
                    else
                    {
                        var oldBytes = MemorySize.RowBytes(values);
                        if (bytes > oldBytes) meter.Memory.Reserve(bytes - oldBytes); else meter.Memory.Release(oldBytes - bytes);
                    }
                }
                result.Add(row);
            }
            return result;
        }
        void ReleaseRows(IEnumerable<SqlValue[]> rows)
        {
            if (meter.Memory.Enabled) foreach (var row in rows) meter.Memory.Release(MemorySize.RowBytes(row));
        }
        try
        {
            working.Rows = await Collect(anchor).ConfigureAwait(false);
            while (working.Rows.Count != 0)
            {
                foreach (var row in working.Rows) { meter.Memory.ReserveRow(row); yield return row.ToArray(); }
                meter.Checkpoint(cancellationToken);
                var next = await Collect(step).ConfigureAwait(false);
                ReleaseRows(working.Rows); working.Rows = next;
            }
        }
        finally { ReleaseRows(working.Rows); working.Rows = []; meter.Memory.Release(seenBytes); }
    }
    private static void ValidateTerm(SelectSyntax query, string name)
    {
        void Expression(Expression expression)
        {
            if (AggregateResolve.IsAggregate(expression)) throw Invalid("aggregate in recursive term");
            if (expression.Query is { } nested && CteBuffer.References(nested, name) > 0) throw Invalid("recursive reference inside expression subquery");
            foreach (var child in expression.Args) Expression(child);
            if (expression.Predicate is { } predicate) Expression(predicate);
        }
        void Relation(RelationSyntax? source, bool nullable = false)
        {
            switch (source)
            {
                case TableRelation table when table.Name == name && nullable: throw Invalid("recursive reference on nullable side of outer join");
                case DerivedRelation derived when CteBuffer.References(derived.Query, name) > 0: throw Errors.Unsupported("recursive reference inside derived table");
                case GroupedRelation group: Relation(group.Source, nullable); break;
                case JoinedRelation join:
                    Relation(join.Left, nullable || join.Kind is "right" or "full");
                    Relation(join.Right, nullable || join.Kind is "left" or "full");
                    if (join.On is { } on) Expression(on);
                    break;
                case FunctionRelation function: Expression(function.Function); break;
                case ValuesRelation values: foreach (var row in values.Values) foreach (var value in row) Expression(value); break;
            }
        }
        Relation(query.Relation);
        foreach (var item in query.Items) Expression(item.Expression);
        if (query.Filter is { } filter) Expression(filter);
        if (query.Having is { } having) Expression(having);
        foreach (var key in query.GroupBy ?? []) Expression(key);
        foreach (var key in query.Order ?? []) Expression(key.Expression);
    }
    private static SqlException Invalid(string message) => new(SqlStates.InvalidRecursion, message);
}
