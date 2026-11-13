using System.Runtime.CompilerServices;

namespace Jed;

internal sealed record PreparedRelation(BoundColumn[] Columns, IAsyncEnumerable<SqlValue[]> Rows, Func<ValueTask>? Initialize = null, OuterFrame[]? Dependencies = null, Func<ValueTask>? InitializeConstants = null);

internal static class RelationExecution
{
    internal static PreparedRelation Prepare(RelationSyntax source, Database database, DatabaseSnapshot snapshot,
        PendingDatabase? pending, Catalog catalog, Meter meter, CancellationToken cancellationToken, QueryScope? scope = null)
    {
        switch (source)
        {
            case LateralRelation lateral: return Prepare(lateral.Source, database, snapshot, pending, catalog, meter, cancellationToken, scope);
            case AliasedRelation aliased:
            {
                var input = Prepare(aliased.Source, database, snapshot, pending, catalog, meter, cancellationToken, scope);
                var slots = Enumerable.Range(0, input.Columns.Length).Where(i => !input.Columns[i].Hidden).ToArray();
                return input with { Columns = Alias(slots.Select(i => input.Columns[i]).ToArray(), aliased.Alias, aliased.ColumnAliases), Rows = Visible(input.Rows, slots) };
            }
            case SetRelation set:
            {
                var left = Query(set.Left, database, snapshot, pending, catalog, meter, cancellationToken, scope: scope);
                var right = Query(set.Right, database, snapshot, pending, catalog, meter, cancellationToken, scope: scope);
                if (left.Columns.Length != right.Columns.Length) throw Errors.Syntax("set operands have different column counts");
                var columns = left.Columns.Select((column, index) => column with { Type = Resolve.Common(column.Type, right.Columns[index].Type), Qualifier = null }).ToArray();
                return new(columns, SetExecution.Run(left.Rows, right.Rows, columns.Select(column => column.Type).ToArray(), set.Operation, set.All, meter, cancellationToken), Dependencies: Merge(left.Dependencies, right.Dependencies), InitializeConstants: CombineConstants(left, right));
            }
            case ValuesRelation values:
            {
                var cells = new List<SubqueryCell>(); var frame = new OuterFrame([]);
                var syntaxRows = values.Values.Select(row => row.Select(value => Resolve.Bind(SubqueryExecution.Bind(value, frame, database, snapshot, pending, catalog, meter, cancellationToken, scope, cells), scope?.OuterColumns ?? [])).ToArray()).ToArray();
                var rows = syntaxRows.Select(row => new Resolved[row.Length]).ToArray();
                var columns = new BoundColumn[rows[0].Length];
                for (var column = 0; column < columns.Length; column++)
                {
                    var expressions = syntaxRows.Select(row => row[column]).ToArray();
                    var resolved = Resolve.Unify(expressions).Select(value => value.Op == "float_widen" ? value with { Op = "promote" } : value).ToArray();
                    var type = resolved[0].Type == SqlType.Unknown ? SqlType.Text : resolved[0].Type;
                    columns[column] = new($"column{column + 1}", type, "*VALUES*");
                    for (var row = 0; row < rows.Length; row++) rows[row][column] = resolved[row].Type == SqlType.Unknown ? Resolve.Expr(expressions[row], type) : resolved[row];
                }
                return Finish(columns, Values(rows, meter, cancellationToken), cells, rows.SelectMany(row => row), frame);
            }
            case TableRelation named:
            {
                if (scope is not null && scope.TryGet(named.Name, out var cte))
                    return cte with { Columns = Alias(cte.Columns, named.Alias ?? named.Name, named.ColumnAliases) };
                if ((catalog.Views ?? []).FirstOrDefault(view => view.Name == named.Name) is { } view)
                {
                    var body = Query(StatementExecution.ViewQuery(view), database, snapshot, pending, catalog, meter, cancellationToken);
                    var columns = Alias(body.Columns, named.Name, view.Columns.Select(column => column.Name).ToArray());
                    return body with { Columns = Alias(columns, named.Alias ?? named.Name, named.ColumnAliases) };
                }
                var table = StatementExecution.Table(catalog, named.Name);

                return new(Alias(StatementExecution.Columns(table), named.Alias ?? named.Name, named.ColumnAliases),
                    QueryExecution.TableSource(table, database.Tree(table, pending, snapshot), meter, cancellationToken));
            }
            case FunctionRelation function:
            {
                if (function.Function.Text != "generate_series") throw new SqlException(SqlStates.UndefinedFunction, "unknown set-returning function");
                var cells = new List<SubqueryCell>(); var frame = new OuterFrame([]);
                var expressions = function.Function.Args.Select(value => Resolve.Bind(SubqueryExecution.Bind(value, frame, database, snapshot, pending, catalog, meter, cancellationToken, scope, cells), scope?.OuterColumns ?? [])).ToArray();
                if (expressions.Length is not (2 or 3)) throw new SqlException(SqlStates.UndefinedFunction, "generate_series requires two or three arguments");
                var args = expressions.All(e => e.Op == "null") ? expressions.Select(e => Resolve.Expr(e, SqlType.I64)).ToArray() : Resolve.Unify(expressions);
                if (args.Any(a => !a.Type.IsInteger())) throw new SqlException(SqlStates.UndefinedFunction, "generate_series requires integers");
                var columns = Alias([new(function.Alias ?? "generate_series", args[0].Type)], function.Alias ?? "generate_series", function.ColumnAliases);
                return Finish(columns, Series(args, meter, cancellationToken), cells, args, frame);
            }
            case DerivedRelation derived:
            {
                var query = Query(derived.Query, database, snapshot, pending, catalog, meter, cancellationToken, derived: true, scope: scope);
                return query with { Columns = Alias(query.Columns, derived.Alias, derived.ColumnAliases) };
            }
            case JoinedRelation join:
            {
                var left = Prepare(join.Left, database, snapshot, pending, catalog, meter, cancellationToken, scope);
                var lateralFrame = new OuterFrame(left.Columns);
                var rightScope = join.Right is LateralRelation or FunctionRelation ? new QueryScope(scope, lateralFrame) : scope;
                var right = Prepare(join.Right, database, snapshot, pending, catalog, meter, cancellationToken, rightScope);
                var lateral = (right.Dependencies ?? []).Contains(lateralFrame);
                if (lateral && join.Kind is "right" or "full") throw new SqlException(SqlStates.InvalidColumnReference, "LATERAL reference requires an inner or left join");

                var leftAliases = left.Columns.Where(c => c.Qualifier is not null).Select(c => c.Qualifier).ToHashSet(StringComparer.Ordinal);
                if (right.Columns.Any(c => c.Qualifier is not null && leftAliases.Contains(c.Qualifier)))
                    throw new SqlException(SqlStates.DuplicateAlias, "relation alias appears twice");
                var inputColumns = left.Columns.Concat(right.Columns).ToArray();
                var usingNames = join.Natural ? left.Columns.Where(c => !c.Hidden && right.Columns.Any(r => !r.Hidden && r.Name == c.Name)).Select(c => c.Name).Distinct(StringComparer.Ordinal).ToArray() : join.Using ?? [];
                if (usingNames.Distinct(StringComparer.Ordinal).Count() != usingNames.Length) throw new SqlException(SqlStates.DuplicateColumn, "duplicate USING column");
                var usingSlots = usingNames.Select(name =>
                {
                    var a = Enumerable.Range(0, left.Columns.Length).Where(i => !left.Columns[i].Hidden && left.Columns[i].Name == name).ToArray();
                    var b = Enumerable.Range(0, right.Columns.Length).Where(i => !right.Columns[i].Hidden && right.Columns[i].Name == name).ToArray();
                    if (a.Length == 0 || b.Length == 0) throw new SqlException(SqlStates.UndefinedColumn, "USING column missing from input");
                    if (a.Length != 1 || b.Length != 1) throw new SqlException(SqlStates.AmbiguousColumn, "ambiguous USING column");
                    var common = Resolve.Common(left.Columns[a[0]].Type, right.Columns[b[0]].Type);
                    return (Left: a[0], Right: b[0], Type: common);
                }).ToArray();
                var cells = new List<SubqueryCell>(); var frame = new OuterFrame(inputColumns);
                var predicate = join.On is null ? null : Resolve.Expr(Resolve.Bind(SubqueryExecution.Bind(join.On, frame, database, snapshot, pending, catalog, meter, cancellationToken, scope, cells), inputColumns.Concat(scope?.OuterColumns ?? []).ToArray()));
                foreach (var pair in usingSlots)
                {
                    var a = new Resolved("column", left.Columns[pair.Left].Type, default, [], Slot: pair.Left);
                    var b = new Resolved("column", right.Columns[pair.Right].Type, default, [], Slot: left.Columns.Length + pair.Right);
                    var equal = new Resolved("=", SqlType.Boolean, default, [a, b]);
                    predicate = predicate is null ? equal : new("and", SqlType.Boolean, default, [predicate, equal]);
                }
                if (predicate is not null && predicate.Type.Kind is not (SqlKind.Boolean or SqlKind.Unknown)) throw Errors.Type("JOIN ON requires boolean");
                var columns = usingNames.Select((name, i) => new BoundColumn(name, usingSlots[i].Type)).Concat(inputColumns.Select(c => c with { Hidden = c.Hidden || usingNames.Contains(c.Name, StringComparer.Ordinal) })).ToArray();
                return Finish(columns, Join(left, right, predicate, join.Kind, usingSlots, meter, cancellationToken, lateral ? lateralFrame : null), cells, predicate is null ? [] : [predicate], frame, Merge(left.Dependencies, right.Dependencies).Where(dependency => dependency != lateralFrame).ToArray(), CombineConstants(left, right));
            }
            default: throw Errors.Unsupported("relation source");
        }
    }
    internal static PreparedRelation Query(SelectSyntax query, Database database, DatabaseSnapshot snapshot, PendingDatabase? pending,
        Catalog catalog, Meter meter, CancellationToken cancellationToken, bool derived = false, QueryScope? scope = null)
    {
        query = Resolve.BindTypes(query, catalog, meter.Temporal);
        var buffers = new List<CteBuffer>();
        if (query.With is { Length: > 0 } definitions)
        {
            scope = new(scope);
            for (var i = 0; i < definitions.Length; i++)
            {
                var definition = definitions[i];
                var recursive = query.Recursive && CteBuffer.References(definition.Query, definition.Name) > 0;
                var body = recursive ? RecursiveExecution.Prepare(definition, database, snapshot, pending, catalog, meter, cancellationToken, scope) :
                    Query(definition.Query, database, snapshot, pending, catalog, meter, cancellationToken, scope: scope);
                body = body with { Columns = Alias(body.Columns, definition.Name, definition.Columns) };
                var references = CteBuffer.References(query with { With = null }, definition.Name) + definitions.Skip(i + 1).Sum(later => CteBuffer.References(later.Query, definition.Name));
                if (references > 0 && (recursive || definition.Materialized == true || definition.Materialized is null && references > 1))
                {
                    var buffer = new CteBuffer(body, meter); buffers.Add(buffer); body = body with { Rows = buffer.Read(cancellationToken) };
                }
                scope.Add(definition.Name, body);
            }
        }
        var subqueries = new List<SubqueryCell>();
        SelectPlan plan; IAsyncEnumerable<SqlValue[]> rows; OuterFrame frame; OuterFrame[] inputDependencies = []; Func<ValueTask>? inputConstants = null;
        if (query.Table is not null && !(scope?.TryGet(query.Table, out _) ?? false) && !(catalog.Views ?? []).Any(view => view.Name == query.Table))
        {
            var table = StatementExecution.Table(catalog, query.Table);
            var columns = StatementExecution.Columns(table, query.TableAlias); frame = new(columns);
            query = SubqueryExecution.Bind(query, frame, database, snapshot, pending, catalog, meter, cancellationToken, scope, subqueries);
            plan = Resolve.Select(query, columns.Concat(scope?.OuterColumns ?? []).ToArray());
            rows = QueryExecution.DeferredTableSource(table, database.Tree(table, pending, snapshot), plan, meter, cancellationToken);
            plan = TableAccess.Prepare(plan, table).Plan;
        }
        else
        {
            var input = query.Relation is null ? new PreparedRelation([], QueryExecution.ScalarSource()) : Prepare(query.Relation, database, snapshot, pending, catalog, meter, cancellationToken, scope);
            frame = new(input.Columns); inputDependencies = input.Dependencies ?? []; inputConstants = input.InitializeConstants;
            query = SubqueryExecution.Bind(query, frame, database, snapshot, pending, catalog, meter, cancellationToken, scope, subqueries);
            plan = Resolve.Select(query, input.Columns.Concat(scope?.OuterColumns ?? []).ToArray()); rows = input.Rows;
        }
        if (derived && query.ValuesQuery && query.Order is not { Length: > 0 } && query.Limit is null && query.Offset == 0) plan = plan with { ChargeRows = false };
        var result = Projected(QueryExecution.Select(plan, rows, meter, cancellationToken));
        async ValueTask Constants()
        {
            if (inputConstants is not null) await inputConstants().ConfigureAwait(false);
            foreach (var buffer in buffers) await buffer.InitializeConstants().ConfigureAwait(false);
            foreach (var subquery in subqueries) await subquery.Initialize().ConfigureAwait(false);
        }
        async ValueTask Initialize()
        {
            await Constants().ConfigureAwait(false);
            foreach (var buffer in buffers) await buffer.Materialize(cancellationToken).ConfigureAwait(false);
        }

        if (buffers.Count > 0 || subqueries.Count > 0) result = SubqueryExecution.Initialized(Initialize, result, cancellationToken);
        var dependencies = Correlation.Dependencies(plan);
        dependencies.UnionWith(inputDependencies);
        foreach (var buffer in buffers) dependencies.UnionWith(buffer.Dependencies);
        dependencies.Remove(frame);
        return new(plan.Projections.Select(projection => new BoundColumn(projection.Name, projection.Expression.Type)).ToArray(), result, Initialize, dependencies.ToArray(), Constants);
    }
    private static Func<ValueTask> CombineConstants(params PreparedRelation[] inputs) => async () =>
    {
        foreach (var input in inputs) if (input.InitializeConstants is { } initialize) await initialize().ConfigureAwait(false);
    };
    private static OuterFrame[] Merge(params OuterFrame[]?[] sources) => sources.SelectMany(source => source ?? []).Distinct().ToArray();
    private static PreparedRelation Finish(BoundColumn[] columns, IAsyncEnumerable<SqlValue[]> rows, List<SubqueryCell> cells, IEnumerable<Resolved> expressions, OuterFrame frame, OuterFrame[]? inputDependencies = null, Func<ValueTask>? inputConstants = null)
    {
        async ValueTask Initialize() { if (inputConstants is not null) await inputConstants().ConfigureAwait(false); foreach (var cell in cells) await cell.Initialize().ConfigureAwait(false); }
        var dependencies = new HashSet<OuterFrame>(inputDependencies ?? []);
        foreach (var expression in expressions) dependencies.UnionWith(Correlation.Dependencies(expression));
        dependencies.Remove(frame);
        return new(columns, cells.Count == 0 ? rows : SubqueryExecution.Initialized(Initialize, rows, default), Initialize, dependencies.ToArray(), Initialize);
    }
    internal static async IAsyncEnumerable<Row> PublicRows(IAsyncEnumerable<SqlValue[]> source)
    {
        await foreach (var row in source.ConfigureAwait(false)) yield return new Row(row);
    }
    internal static BoundColumn[] Alias(BoundColumn[] columns, string? alias, string[]? names)
    {
        if (names is not null && names.Length > columns.Length) throw new SqlException(SqlStates.InvalidColumnReference, "too many column aliases");
        return columns.Select((column, index) => column with { Name = names is not null && index < names.Length ? names[index] : column.Name, Qualifier = alias }).ToArray();
    }
    private static async IAsyncEnumerable<SqlValue[]> Values(Resolved[][] rows, Meter meter, [EnumeratorCancellation] CancellationToken cancellationToken)
    {
        var materialized = new List<SqlValue[]>();
        foreach (var row in rows)
        {
            meter.Checkpoint(cancellationToken); meter.Charge(Costs.RowProduced);
            var result = new SqlValue[row.Length];
            for (var i = 0; i < row.Length; i++) result[i] = await Evaluate.ExprAsync(row[i], meter).ConfigureAwait(false);
            meter.Checkpoint(cancellationToken); materialized.Add(result);
        }
        foreach (var row in materialized) yield return row;
    }
    private static async IAsyncEnumerable<SqlValue[]> Series(Resolved[] args, Meter meter, [EnumeratorCancellation] CancellationToken cancellationToken)
    {
        await ValueTask.CompletedTask.ConfigureAwait(false);
        var values = new SqlValue[args.Length];
        for (var i = 0; i < args.Length; i++) values[i] = await Evaluate.ExprAsync(args[i], meter).ConfigureAwait(false);
        if (values.Any(v => v.IsNull)) yield break;
        var start = values[0].AsInteger(); var end = values[1].AsInteger(); var step = values.Length == 3 ? values[2].AsInteger() : 1;
        if (step == 0) throw new SqlException(SqlStates.InvalidParameterValue, "series step cannot be zero");
        for (var current = start; step > 0 ? current <= end : current >= end;)
        {
            meter.Checkpoint(cancellationToken); meter.Charge(Costs.GeneratedRow); meter.Guard();
            yield return [SqlValue.Integer(current, args[0].Type)];
            var next = (Int128)current + step;
            if (next < long.MinValue || next > long.MaxValue) yield break;
            current = (long)next;
        }
    }
    private static async IAsyncEnumerable<SqlValue[]> Visible(IAsyncEnumerable<SqlValue[]> source, int[] slots)
    {
        await foreach (var row in source.ConfigureAwait(false)) yield return slots.Select(i => row[i]).ToArray();
    }
    private static async IAsyncEnumerable<SqlValue[]> Projected(IAsyncEnumerable<Row> source)
    {
        await foreach (var row in source.ConfigureAwait(false)) yield return Enumerable.Range(0, row.Count).Select(i => row[i]).ToArray();
    }
    private static async IAsyncEnumerable<SqlValue[]> Join(PreparedRelation left, PreparedRelation right, Resolved? predicate, string kind,
        (int Left, int Right, SqlType Type)[] usingSlots, Meter meter, [EnumeratorCancellation] CancellationToken cancellationToken, OuterFrame? lateral = null)
    {
        var leftRows = new List<SqlValue[]>(); var rightRows = new List<SqlValue[]>();
        await foreach (var row in left.Rows.WithCancellation(cancellationToken).ConfigureAwait(false)) leftRows.Add(row);
        if (lateral is null) await foreach (var row in right.Rows.WithCancellation(cancellationToken).ConfigureAwait(false)) rightRows.Add(row);
        var matchedRight = new bool[rightRows.Count];
        SqlValue[] Output(SqlValue[] a, SqlValue[] b)
        {
            var row = new SqlValue[usingSlots.Length + a.Length + b.Length];
            for (var i = 0; i < usingSlots.Length; i++)
            {
                var pair = usingSlots[i]; row[i] = Resolve.Cast(a[pair.Left].IsNull ? b[pair.Right] : a[pair.Left], pair.Type);
            }
            a.CopyTo(row, usingSlots.Length); b.CopyTo(row, usingSlots.Length + a.Length); return row;
        }
foreach (var a in leftRows)
{
    if (lateral is not null)
    {
        lateral.Assign(a); rightRows.Clear();
        if (right.Initialize is { } initialize) await initialize().ConfigureAwait(false);
        await foreach (var row in right.Rows.WithCancellation(cancellationToken).ConfigureAwait(false)) rightRows.Add(row);
        matchedRight = new bool[rightRows.Count];
    }
    var matched = false;

            for (var i = 0; i < rightRows.Count; i++)
            {
                meter.Checkpoint(cancellationToken); var b = rightRows[i];
                if (!await QueryExecution.KeepsAsync(predicate, a.Concat(b).ToArray(), meter).ConfigureAwait(false)) continue;
                matched = true; matchedRight[i] = true; yield return Output(a, b);
            }
            if (!matched && kind is "left" or "full") yield return Output(a, right.Columns.Select(c => SqlValue.Null(c.Type)).ToArray());
        }
        if (kind is "right" or "full")
            for (var i = 0; i < rightRows.Count; i++)
                if (!matchedRight[i]) yield return Output(left.Columns.Select(c => SqlValue.Null(c.Type)).ToArray(), rightRows[i]);
    }
}
