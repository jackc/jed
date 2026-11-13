using System.Runtime.CompilerServices;

namespace Jed;

internal sealed record CteSyntax(string Name, string[]? Columns, SelectSyntax Query, bool? Materialized, StatementSyntax? Mutation = null);
internal sealed record WithStatementSyntax(CteSyntax[] Definitions, bool Recursive, StatementSyntax Body) : StatementSyntax;
internal sealed class QueryScope(QueryScope? parent = null, OuterFrame? frame = null)
{
    internal BoundColumn[] OuterColumns { get; } = frame is null ? parent?.OuterColumns ?? [] :
        frame.Columns.Select((column, slot) => column with { Outer = new(frame, slot), Level = 1, Projection = column.Projection is { } projection ? frame.Lift(projection) : null }).Concat((parent?.OuterColumns ?? []).Select(column => column with { Level = column.Level + 1 })).ToArray();
    private readonly Dictionary<string, PreparedRelation> relations = new(StringComparer.Ordinal);
    internal bool TryGet(string name, out PreparedRelation relation)
    {
        if (relations.TryGetValue(name, out relation!)) return true;
        if (parent is not null) return parent.TryGet(name, out relation);
        relation = null!; return false;
    }
    internal void Add(string name, PreparedRelation relation) => relations.Add(name, relation);
}

internal sealed class CteBuffer(PreparedRelation source, Meter meter)
{
    internal ValueTask InitializeConstants() => source.InitializeConstants?.Invoke() ?? ValueTask.CompletedTask;
    internal OuterFrame[] Dependencies => source.Dependencies ?? [];
    private List<SqlValue[]>? rows;
    private long[]? versions;
    private long heldBytes;
    internal void Release() { meter.Memory.Release(heldBytes); heldBytes = 0; rows = null; versions = null; }
    internal async ValueTask Materialize(CancellationToken cancellationToken)
    {
        var current = (source.Dependencies ?? []).Select(frame => frame.Version).ToArray();
        if (rows is not null && versions is not null && versions.SequenceEqual(current)) return;
        Release();
        var collected = new List<SqlValue[]>();
        await foreach (var row in source.Rows.WithCancellation(cancellationToken).ConfigureAwait(false))
        {
            if (meter.Memory.Enabled)
            {
                var bytes = MemorySize.RowBytes(row); if (!source.MemoryOutput) meter.Memory.Reserve(bytes);
                heldBytes = Meter.Add(heldBytes, bytes);
            }
            collected.Add(row);
        }
        rows = collected; versions = current;
    }
    internal async IAsyncEnumerable<SqlValue[]> Read([EnumeratorCancellation] CancellationToken cancellationToken)
    {
        await Materialize(cancellationToken).ConfigureAwait(false);
        foreach (var row in rows!) { meter.Charge(Costs.CteScanRow); meter.Checkpoint(cancellationToken); meter.Memory.ReserveRow(row); yield return row.ToArray(); }
    }
    internal static async IAsyncEnumerable<SqlValue[]> DisposeAfter(IReadOnlyList<CteBuffer> buffers, IAsyncEnumerable<SqlValue[]> source,
        [EnumeratorCancellation] CancellationToken cancellationToken)
    {
        try { await foreach (var row in source.WithCancellation(cancellationToken).ConfigureAwait(false)) yield return row; }
        finally { foreach (var buffer in buffers) buffer.Release(); }
    }
    internal static async IAsyncEnumerable<SqlValue[]> Initialize(CteBuffer[] buffers, IAsyncEnumerable<SqlValue[]> source,
        [EnumeratorCancellation] CancellationToken cancellationToken)
    {
        foreach (var buffer in buffers) await buffer.Materialize(cancellationToken).ConfigureAwait(false);
        await foreach (var row in source.WithCancellation(cancellationToken).ConfigureAwait(false)) yield return row;
    }
    internal static int References(SelectSyntax query, string name)
    {
        int Source(RelationSyntax? relation) => relation switch
        {
            LateralRelation lateral => Source(lateral.Source),
            AliasedRelation aliased => Source(aliased.Source),
            TableRelation named => named.Name == name ? 1 : 0,
            DerivedRelation derived => References(derived.Query, name),
            GroupedRelation group => Source(group.Source),
            JoinedRelation join => Source(join.Left) + Source(join.Right),
            SetRelation set => References(set.Left, name) + References(set.Right, name),
            _ => 0
        };
        if (query.With is { } locals && locals.Any(local => local.Name == name)) return 0;
        int ExpressionReferences(Expression expression) => (expression.Query is null ? 0 : References(expression.Query, name)) + expression.Args.Sum(ExpressionReferences) + (expression.Predicate is null ? 0 : ExpressionReferences(expression.Predicate));
        return Source(query.Relation) + query.Items.Sum(item => ExpressionReferences(item.Expression)) + (query.Filter is null ? 0 : ExpressionReferences(query.Filter)) + (query.Order ?? []).Sum(order => ExpressionReferences(order.Expression)) + (query.With ?? []).Sum(local => References(local.Query, name));
    }
}

internal sealed partial class Parser
{
    private SelectSyntax WithQuery() => WithStatement() as SelectSyntax ?? throw Errors.Unsupported("data-modifying WITH requires the statement top level");
    private StatementSyntax WithStatement()
    {
        var primaryInto = intoAllowed; intoAllowed = false;
        var recursive = Eat("recursive"); var definitions = new List<CteSyntax>();
        do
        {
            var name = Name(); string[]? columns = null;
            if (definitions.Any(definition => definition.Name == name)) throw new SqlException(SqlStates.DuplicateAlias, "duplicate CTE name");
            if (Eat("(")) { var names = new List<string>(); do { names.Add(Name()); } while (Eat(",")); Expect(")"); columns = names.ToArray(); }
            Expect("as"); bool? materialized = Eat("materialized") ? true : null;
            if (Eat("not")) { Expect("materialized"); materialized = false; }
            Expect("("); nesting++; Depth(nesting);
            StatementSyntax body = Is("insert") || Is("update") || Is("delete") ? OtherStatement() : Select();
            Expect(")"); nesting--;
            definitions.Add(body is SelectSyntax query ? new(name, columns, query, materialized) : new(name, columns, new([], null, null, 0), materialized, body));
        } while (Eat(","));
        var mutation = Is("insert") || Is("update") || Is("delete");
        intoAllowed = primaryInto && !mutation;
        StatementSyntax result = mutation ? OtherStatement() : Select();
        if (result is SelectSyntax { With.Length: > 0 }) throw Errors.Syntax("multiple WITH clauses");
        return result is SelectSyntax queryResult ? queryResult with { With = definitions.ToArray(), Recursive = recursive } : new WithStatementSyntax(definitions.ToArray(), recursive, result);
    }
}
