using System.Runtime.CompilerServices;

namespace Jed;

internal sealed record PreparedRelation(BoundColumn[] Columns, IAsyncEnumerable<SqlValue[]> Rows, Func<ValueTask>? Initialize = null, OuterFrame[]? Dependencies = null, Func<ValueTask>? InitializeConstants = null, bool NoReturning = false,
    Func<Resolved, (PreparedRelation Relation, Resolved? Residual)>? PushFilter = null, long? EstimatedRows = null, BaseRelationAccess? BaseAccess = null, bool MemoryOutput = false, Func<HashSet<int>, PreparedRelation>? WithTouched = null, CrossJoinChain? CrossChain = null, ExplainNode? Plan = null, Func<SortPlan[], long, Resolved?, PreparedRelation?>? Bounded = null, ExplainCte? Cte = null, bool BoundedOutput = false, Func<AccessPlan, PreparedRelation>? WithAccess = null, AccessPlan? SelectedAccess = null);

internal static class RelationExecution
{
    internal static PreparedRelation Prepare(RelationSyntax source, Database database, DatabaseSnapshot snapshot,
        PendingDatabase? pending, Catalog catalog, Meter meter, CancellationToken cancellationToken, QueryScope? scope = null, QueryScope? preceding = null)
    {
        if (source is JoinedRelation && NWayPlanning.Prepare(source, database, snapshot, pending, catalog, meter, cancellationToken, scope) is { } island) return island;
        switch (source)
        {
case GroupedRelation group:
{
    PreparedRelation Wrap(PreparedRelation input) => input with
    {
        Rows = GroupRows(input.Rows, meter, cancellationToken), BaseAccess = null, CrossChain = null,
        WithTouched = input.WithTouched is null ? null : mask => Wrap(input.WithTouched(mask)),
        PushFilter = input.PushFilter is null ? null : filter =>
        {
            var pushed = input.PushFilter(filter); return (Wrap(pushed.Relation), pushed.Residual);
        }
    };
    return Wrap(Prepare(group.Source, database, snapshot, pending, catalog, meter, cancellationToken, scope, preceding));
}
            case LateralRelation lateral: return Prepare(lateral.Source, database, snapshot, pending, catalog, meter, cancellationToken, preceding ?? scope);
            case AliasedRelation aliased:
            {
                var input = Prepare(aliased.Source, database, snapshot, pending, catalog, meter, cancellationToken, scope, preceding);
                var slots = Enumerable.Range(0, input.Columns.Length).Where(i => !input.Columns[i].Hidden).ToArray();
                PreparedRelation Wrap(PreparedRelation source) => source with
                {
                    Columns = Alias(slots.Select(i => source.Columns[i] with { Projection = null }).ToArray(), aliased.Alias, aliased.ColumnAliases),
                    Rows = Visible(source.Rows, slots), PushFilter = null, Bounded = null,
                    WithTouched = source.WithTouched is null ? null : mask => Wrap(source.WithTouched(mask.Select(slot => slots[slot]).ToHashSet()))
                };
                return Wrap(input);
            }
            case SetRelation set:
            {
                var left = Query(set.Left, database, snapshot, pending, catalog, meter, cancellationToken, scope: scope);
                var right = Query(set.Right, database, snapshot, pending, catalog, meter, cancellationToken, scope: scope);
                if (left.Columns.Length != right.Columns.Length) throw Errors.Syntax("set operands have different column counts");
if (set.Operation != "union" || !set.All)
    foreach (var pair in left.Columns.Zip(right.Columns))
    {
        Resolve.RequireOrderable(pair.First.Type); Resolve.RequireOrderable(pair.Second.Type);
        CollationRules.Require(CollationRules.Merge(new[] { pair.First.Collation, pair.Second.Collation }), true);
    }
var columns = left.Columns.Select((column, index) => column with { Type = Resolve.Common(column.Type, right.Columns[index].Type), Qualifier = null,
                    Collation = CollationRules.Merge(new[] { column.Collation, right.Columns[index].Collation })?.Implicit() }).ToArray();
                return SetPlanning.Build(left, right, columns, set.Operation, set.All, meter, cancellationToken);
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
                    columns[column] = new($"column{column + 1}", type, "*VALUES*", Collation: CollationRules.Merge(resolved)?.Implicit());
                    for (var row = 0; row < rows.Length; row++) rows[row][column] = resolved[row].Type == SqlType.Unknown ? Resolve.Expr(expressions[row], type) : resolved[row];
                }
                var preparedValues = Finish(columns, Values(rows, meter, cancellationToken), cells, rows.SelectMany(row => row), frame) with { EstimatedRows = rows.Length };
                return meter.Explain is null ? preparedValues : ExplainPlanning.Attach(preparedValues,
                    new("Values", "rows=" + rows.Length, rows.Length, Estimate.Add(rows.Length, ExplainPlanning.Expressions(rows.SelectMany(row => row)))), meter);
            }
            case TableRelation named:
            {
                if (scope is not null && scope.TryGet(named.Name, out var cte))
                {
                    if (cte.NoReturning) throw Errors.Unsupported("WITH query has no RETURNING clause");
                    var reference = cte with { Columns = Alias(cte.Columns, named.Alias ?? named.Name, named.ColumnAliases) };
                    if (meter.Explain is not null && cte.Cte is { } definition)
                        reference = ExplainPlanning.Attach(reference, new("CTE Scan " + definition.Name, "-", definition.Body.EstimatedRows,
                            definition.Materialized ? definition.Body.EstimatedRows : definition.Body.EstimatedCost,
                            definition.Materialized || definition.DefinitionVisible ? [] : [definition.Body]), meter);
                    return reference;
                }
                if (CatalogRelations.IsCatalogName(named.Name)) return CatalogRelations.Prepare(named, database, snapshot, pending, catalog, meter, cancellationToken);
                if (catalog.Scope?.View(named.Name) is { } scopedView)
                {
                    var home = scopedView.Domain; var viewCatalog = home.Catalog with { Scope = catalog.Scope.Stored(home.Name) };
                    var body = Query(StatementExecution.ViewQuery(scopedView.View), home.Database, home.Snapshot, home.Pending, viewCatalog, meter, cancellationToken);
                    var columns = Alias(body.Columns, scopedView.View.Name, scopedView.View.Columns.Select(column => column.Name).ToArray());
                    columns = columns.Select(column => column with { RowName = column.Name, Collation = column.Collation?.Implicit() }).ToArray();
                    return body with { Columns = Alias(columns, named.Alias ?? scopedView.View.Name, named.ColumnAliases) };
                }
                if (catalog.Scope is null && (catalog.Views ?? []).FirstOrDefault(view => view.Name == named.Name) is { } view)
                {
                    var body = Query(StatementExecution.ViewQuery(view), database, snapshot, pending, catalog, meter, cancellationToken);
                    var columns = Alias(body.Columns, named.Name, view.Columns.Select(column => column.Name).ToArray());
                    columns = columns.Select(column => column with { RowName = column.Name, Collation = column.Collation?.Implicit() }).ToArray();
                    return body with { Columns = Alias(columns, named.Alias ?? named.Name, named.ColumnAliases) };
                }
                var table = StatementExecution.ResolveTable(catalog, named.Name);
                var tableColumns = Alias(StatementExecution.Columns(table), named.Alias ?? table.Name, named.ColumnAliases);
                var tree = database.Tree(table, pending, snapshot);
                PreparedRelation Scan(Resolved? filter, HashSet<int>? touched = null, AccessPlan? forced = null)
                {
                    if (touched is not null && filter is not null) { touched = new(touched); touched.UnionWith(PredicatePlan.Slots(filter)); }
                    PreparedRelation? retained = null;
                    var projection = tableColumns.Select((column, slot) => new Projection(touched is null || touched.Contains(slot)
                        ? new("column", column.Type, default, [], Slot: slot, Collation: column.Collation)
                        : new("constant", column.Type, SqlValue.Null(column.Type), []), column.Name)).ToArray();
                    var scanPlan = new SelectPlan(projection, filter, null, 0, false) { Access = forced };
                    var storage = new QueryStorage(database, snapshot, pending, catalog, SingleRelation: false);
                    ExplainNode? scanNode = null;
                    if (meter.Explain is not null)
                    {
                        var access = forced ?? AccessPlan.Select(table, tree, filter, storage, meter);
                        scanPlan = scanPlan with { Access = access };
                        scanNode = ExplainPlanning.Scan(table, access, filter, StorageTouchedColumns.Collect(scanPlan), true, meter, catalog, tree.Height);
                    }
                    var sourceRows = QueryExecution.DeferredTableSource(table, tree, scanPlan, meter, cancellationToken, storage);
                    var rows = filter is null ? sourceRows : PredicatePlan.Rows(sourceRows, filter, meter, cancellationToken);
                    var preparedScan = new PreparedRelation(tableColumns, rows, BaseAccess: new(table, tree, filter, storage), EstimatedRows: Estimate.Rows(table, catalog, filter), PushFilter: next =>
                        (Scan(PredicatePlan.Order(filter is null ? next : PredicatePlan.Combine([filter, next])), touched), null), WithTouched: mask => touched is not null && touched.SetEquals(mask) ? retained! : Scan(filter, mask, forced), WithAccess: access => Scan(filter, touched, access), SelectedAccess: forced);
                    retained = scanNode is null ? preparedScan : ExplainPlanning.Attach(preparedScan, scanNode, meter);
                    return retained;
                }
                return Scan(null);
            }
            case FunctionRelation function:
            {
                var functionScope = preceding ?? scope;
                var cells = new List<SubqueryCell>(); var frame = new OuterFrame([]);
                var expressions = function.Function.Args.Select(value => Resolve.Bind(SubqueryExecution.Bind(value, frame, database, snapshot, pending, catalog, meter, cancellationToken, functionScope, cells), functionScope?.OuterColumns ?? [])).ToArray();
                var definitions = function.Definitions?.Select(field =>
                {
                    var spec = SqlTypes.ParseSpec(field.TypeName, catalog.NamedType);
                    return new SqlField(field.Name, spec.Type, Precision: spec.Precision, Scale: spec.Scale, Length: spec.Length, IntervalFields: spec.IntervalFields);
                }).ToArray();
                if (JsonSetFunctions.TryResolve(function.Function with { Args = expressions }, out var jsonFunction, out var jsonColumns, definitions))
                {
                    var name = function.Function.Text;
                    if (jsonColumns.Length == 1 && !name.Contains("record", StringComparison.Ordinal) && name != "json_table") jsonColumns[0] = jsonColumns[0] with { ScalarRelation = true };
                    if (jsonColumns.Length == 1 && (name.EndsWith("object_keys", StringComparison.Ordinal) || name.StartsWith("jsonb_path_query", StringComparison.Ordinal)))
                        jsonColumns[0] = jsonColumns[0] with { Name = function.Alias ?? name };
                    return ExplainPlanning.Function(Finish(Alias(jsonColumns, function.Alias ?? name, function.ColumnAliases), JsonSetFunctions.Rows(jsonFunction, meter, cancellationToken), cells, jsonFunction.Args, frame), name, [jsonFunction], meter);
                }
                if (function.Function.Text == "unnest")
                {
                    var array = OpenSetFunctions.Resolve(function.Function with { Args = expressions });
                    var fields = OpenSetFunctions.Columns(array, definitions);
                    var outputs = fields is null ? [new BoundColumn(function.Alias ?? "unnest", array.Type.Element!, Collation: array.Collation, ScalarRelation: true)] :
                        fields.Select(field => new BoundColumn(field.Name, field.Type)).ToArray();
                    return ExplainPlanning.Function(Finish(Alias(outputs, function.Alias ?? "unnest", function.ColumnAliases), OpenSetFunctions.Rows(array, meter, cancellationToken, fields), cells, [array], frame), "unnest", [array], meter);
                }
                if (function.Function.Text is "regexp_matches" or "regexp_split_to_table")
                {
                    var regex = RegexSetFunctions.Resolve(function.Function with { Args = expressions });
                    var output = new BoundColumn(function.Alias ?? function.Function.Text, regex.Type, Collation: regex.Collation, ScalarRelation: true);
                    return ExplainPlanning.Function(Finish(Alias([output], function.Alias ?? function.Function.Text, function.ColumnAliases), RegexSetFunctions.Rows(regex, meter, cancellationToken), cells, regex.Args, frame), function.Function.Text, [regex], meter);
                }
                if (function.Function.Text != "generate_series") throw new SqlException(SqlStates.UndefinedFunction, "unknown set-returning function");
                if (expressions.Length is not (2 or 3)) throw new SqlException(SqlStates.UndefinedFunction, "generate_series requires two or three arguments");
                var args = expressions.All(e => e.Op == "null") ? expressions.Select(e => Resolve.Expr(e, SqlType.I64)).ToArray() : Resolve.Unify(expressions);
                if (args.Any(a => !a.Type.IsInteger())) throw new SqlException(SqlStates.UndefinedFunction, "generate_series requires integers");
                var columns = Alias([new(function.Alias ?? "generate_series", args[0].Type, ScalarRelation: true)], function.Alias ?? "generate_series", function.ColumnAliases);
                return ExplainPlanning.Function(Finish(columns, Series(args, meter, cancellationToken), cells, args, frame), "generate_series", args, meter);
            }
            case DerivedRelation derived:
            {
                var query = Query(derived.Query, database, snapshot, pending, catalog, meter, cancellationToken, derived: true, scope: scope);
                return ExplainPlanning.Wrapper(query with { Columns = Alias(query.Columns.Select(column => column with { Collation = column.Collation?.Implicit() }).ToArray(), derived.Alias, derived.ColumnAliases) }, "Subquery" + (derived.Alias is null ? "" : " " + derived.Alias), meter);
            }
            case JoinedRelation join:
            {
                var left = Prepare(join.Left, database, snapshot, pending, catalog, meter, cancellationToken, scope, preceding);
                var lateralFrame = new OuterFrame(left.Columns);
                var rightScope = new QueryScope(preceding ?? scope, lateralFrame);
                var right = Prepare(join.Right, database, snapshot, pending, catalog, meter, cancellationToken, scope, rightScope);
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
                if (join.On is { } on && WindowResolve.Contains(on)) throw new SqlException(SqlStates.WindowingError, "window functions are not allowed in JOIN conditions");
                var predicate = join.On is null ? null : Resolve.Expr(Resolve.Bind(SubqueryExecution.Bind(join.On, frame, database, snapshot, pending, catalog, meter, cancellationToken, scope, cells), inputColumns.Concat(scope?.OuterColumns ?? []).ToArray()));
                foreach (var pair in usingSlots)
                {
                    var a = new Resolved("column", left.Columns[pair.Left].Type, default, [], Slot: pair.Left);
                    var b = new Resolved("column", right.Columns[pair.Right].Type, default, [], Slot: left.Columns.Length + pair.Right);
                    var equal = new Resolved("=", SqlType.Boolean, default, [a, b]);
                    predicate = predicate is null ? equal : new("and", SqlType.Boolean, default, [predicate, equal]);
                }
                if (predicate is not null && predicate.Type.Kind is not (SqlKind.Boolean or SqlKind.Unknown)) throw Errors.Type("JOIN ON requires boolean");
                var authoredPredicate = predicate;
                if (predicate is not null)
                {
                    var leftFilters = new List<Resolved>(); var rightFilters = new List<Resolved>(); var residual = new List<Resolved>();
                    foreach (var item in PredicatePlan.Conjuncts(PredicatePlan.Order(predicate)!))
                    {
                        var slots = PredicatePlan.Slots(item);
                        if (slots.Count != 0 && PredicatePlan.Safe(item) && slots.All(slot => slot < left.Columns.Length && !left.Columns[slot].NullExtended) && SameSource(left.Columns, slots) && join.Kind is "inner" or "right") leftFilters.Add(item);
                        else if (slots.Count != 0 && !lateral && PredicatePlan.Safe(item) && slots.All(slot => slot >= left.Columns.Length && !right.Columns[slot - left.Columns.Length].NullExtended) && SameSource(right.Columns, slots.Select(slot => slot - left.Columns.Length)) && join.Kind is "inner" or "left") rightFilters.Add(PredicatePlan.Shift(item, -left.Columns.Length));
                        else residual.Add(item);
                    }
                    if (leftFilters.Count != 0) left = PredicatePlan.Filter(left, PredicatePlan.Order(PredicatePlan.Combine(leftFilters))!, meter, cancellationToken);
                    if (rightFilters.Count != 0) right = PredicatePlan.Filter(right, PredicatePlan.Order(PredicatePlan.Combine(rightFilters))!, meter, cancellationToken);
                    predicate = PredicatePlan.Combine(residual);
                }
                var columns = usingNames.Select((name, i) => new BoundColumn(name, usingSlots[i].Type)).Concat(inputColumns.Select((c, i) => c with
                {
                    Hidden = c.Hidden || usingNames.Contains(c.Name, StringComparer.Ordinal),
                    NullExtended = c.NullExtended || (i < left.Columns.Length ? join.Kind is "right" or "full" : join.Kind is "left" or "full")
                })).ToArray();
                Resolved InputPredicate(Resolved value)
                {
                    if (value.Op == "column")
                    {
                        if (value.Slot >= usingSlots.Length) return value with { Slot = value.Slot - usingSlots.Length };
                        var pair = usingSlots[value.Slot];
                        return value with { Slot = join.Kind == "right" ? left.Columns.Length + pair.Right : pair.Left };
                    }
                    return value with { Args = value.Args.Select(InputPredicate).ToArray() };
                }
                PreparedRelation Build(PreparedRelation a, PreparedRelation b, Resolved? on, Resolved? where = null, HashSet<int>? touched = null, bool reversed = false)
                {
                    var authored = reversed ? JoinOrientation.Expression(authoredPredicate, b.Columns.Length, a.Columns.Length) : authoredPredicate;
                    if (!reversed && !lateral && usingSlots.Length == 0 && cells.Count == 0 && join.Kind is "inner" or "cross" &&
                        a.BaseAccess is not null && b.BaseAccess is not null && (a.Dependencies?.Length ?? 0) == 0 && (b.Dependencies?.Length ?? 0) == 0)
                    {
                        var leftWidth = a.Columns.Length; var rightWidth = b.Columns.Length;
                        var swappedOn = JoinOrientation.Expression(on, leftWidth, rightWidth);
                        var swappedWhere = JoinOrientation.Expression(where, leftWidth, rightWidth);
                        var originalCost = JoinOrientation.Cost(a, b, authored, on, where, join.Kind, meter);
                        var reverseCost = JoinOrientation.Cost(b, a, JoinOrientation.Expression(authored, leftWidth, rightWidth), swappedOn, swappedWhere, join.Kind, meter);
                        if (reverseCost < originalCost)
                        {
                            var swappedTouched = touched?.Select(slot => JoinOrientation.Slot(slot, leftWidth, rightWidth)).ToHashSet();
                            return JoinOrientation.Restore(Build(b, a, swappedOn, swappedWhere, swappedTouched, true), columns, leftWidth, rightWidth, cancellationToken);
                        }
                    }
                    var inl = lateral ? null : IndexNestedLoop.Select(b, a.Columns.Length, join.Kind, PredicatePlan.Combine(new[] { on, where }.OfType<Resolved>()));
                    var hash = !lateral && join.Left is not JoinedRelation && join.Right is not JoinedRelation ? HashJoinPlan.Select(authored, a.Columns.Length, join.Kind, a.EstimatedRows, b.EstimatedRows, on) : null;
                    if (join.Kind is "inner" or "cross" && a.BaseAccess is not null && a.EstimatedRows is { } outerCount && b.EstimatedRows is { } innerCount && b.BaseAccess is { } innerBase)
                    {
                        var fullCost = AccessPlan.Select(innerBase.Table, innerBase.Tree, innerBase.Filter, innerBase.Storage, meter).EstimatedCost;
                        var residual = PredicatePlan.Combine(new[] { on, where }.OfType<Resolved>());
                        var loopCost = Estimate.Add(fullCost, Estimate.Multiply(Estimate.Multiply(outerCount, innerCount), residual is null ? 0 : PredicatePlan.Nodes(residual)));
                        var hashCost = hash is null ? long.MaxValue : Estimate.Add(fullCost, HashJoinPlan.Work(outerCount, innerCount, hash.Types, residual));
                        var indexCost = inl?.EstimateWork(outerCount, residual, meter) ?? long.MaxValue;
                        if (inl is not null && indexCost <= loopCost && indexCost <= hashCost) hash = null;
                        else { inl = null; if (hashCost > loopCost) hash = null; }
                    }
                    else if (inl is not null) hash = null;
                    if (inl is not null) inl.PinnedAccess = AccessPlan.Select(inl.Source.Table, inl.Source.Tree, inl.GenericPredicate(), inl.Source.Storage, meter);
                    var chain = !lateral && inl is null && usingSlots.Length == 0 && join.Kind == "cross" && on is null && join.Left is not (GroupedRelation or AliasedRelation) && join.Right is not JoinedRelation
                        ? new CrossJoinChain((a.CrossChain?.Inputs ?? [a]).Append(b).ToArray()) : null;
                    var spill = hash is not null && database.WorkMem > 0 && database.ScratchHost is { } scratch
                        ? new SpillPolicy(scratch, database.WorkMem, null) : null;
                    if (touched is not null)
                    {
                        var mask = touched.Where(slot => slot >= usingSlots.Length).Select(slot => slot - usingSlots.Length).ToHashSet();
                        if (on is not null) mask.UnionWith(PredicatePlan.Slots(on));
                        if (where is not null) mask.UnionWith(PredicatePlan.Slots(where));
                        foreach (var pair in usingSlots) { mask.Add(pair.Left); mask.Add(a.Columns.Length + pair.Right); }
                        var leftMask = mask.Where(slot => slot < a.Columns.Length).ToHashSet();
                        var rightMask = mask.Where(slot => slot >= a.Columns.Length).Select(slot => slot - a.Columns.Length).ToHashSet();
                        if (a.WithTouched is { } touchLeft) a = touchLeft(leftMask);
                        if (b.WithTouched is { } touchRight) b = touchRight(rightMask);
                    }
                    var joinedRows = spill is null && chain is { Inputs.Length: > 2 } ? chain.Rows(meter, touched, cancellationToken) : Join(a, b, on, join.Kind, usingSlots, meter, cancellationToken, lateral ? lateralFrame : null, hash, inl, touched, spill);
                    var prepared = Finish(columns, joinedRows, cells,
                        on is null ? [] : [on], frame, Merge(a.Dependencies, b.Dependencies).Where(dependency => dependency != lateralFrame).ToArray(), CombineConstants(a, b));
                    ExplainPlanning.ScanFilterSlots(a.Plan, a.BaseAccess?.Filter, reversed ? b.Columns.Length : 0, meter);
                    ExplainPlanning.ScanFilterSlots(b.Plan, b.BaseAccess?.Filter, reversed ? 0 : a.Columns.Length, meter);
                    var estimated = inl is null ? Estimate.JoinRows(a, b, on) : inl.EstimateRows(a.EstimatedRows ?? PlannerFacts.SrfRows, on, meter);
                    if (join.Kind is "left" or "full") estimated = Math.Max(estimated, a.EstimatedRows ?? PlannerFacts.SrfRows);
                    if (join.Kind is "right" or "full") estimated = Math.Max(estimated, b.EstimatedRows ?? PlannerFacts.SrfRows);
                    prepared = ExplainPlanning.Join(prepared with { EstimatedRows = estimated }, a, b, join.Kind, on, hash, inl, meter);
                    PreparedRelation? Bounded(SortPlan[] order, long count, Resolved? finalFilter)
                    {
                        if (lateral || hash is not null || usingSlots.Length != 0 || join.Kind is not ("inner" or "cross") ||
                            a.BaseAccess is not { } first || b.BaseAccess is null) return null;
                        var keys = first.Table.PrimaryKey;
                        var driver = AccessPlan.Select(first.Table, first.Tree, first.Filter, first.Storage, meter);
                        if (driver.Kind is not (AccessKind.Primary or AccessKind.Full or AccessKind.PrimaryIntervals)) return null;
                        if (first.Table.PrimaryKey.Length == 0 || order.Length > keys.Length ||
                            order.Select((key, slot) => key.Descending || key.Expression.Op != "column" || key.Expression.Slot != keys[slot] || key.Expression.Collation?.Table is not null).Any(value => value)) return null;
                        return prepared with { Rows = spill is null
                            ? StreamingJoin.Run(a, b, on, finalFilter, join.Kind, inl, count, meter, cancellationToken)
                            : SpillStreamingJoin.Run(a, b, on, finalFilter, join.Kind, inl, count, spill, meter, cancellationToken), MemoryOutput = spill is null };
                    }
                    PreparedRelation? retained = null;
                    retained = prepared with { Bounded = Bounded, CrossChain = chain, MemoryOutput = spill is null, BoundedOutput = spill is not null, WithTouched = mask => touched is not null && touched.SetEquals(mask) ? retained! : Build(a, b, on, where, mask, reversed), PushFilter = filter =>
                    {
                        var leftFilters = new List<Resolved>(); var rightFilters = new List<Resolved>(); var residual = new List<Resolved>();
                        foreach (var item in PredicatePlan.Conjuncts(filter))
                        {
                            var input = InputPredicate(item);
                            var slots = PredicatePlan.Slots(input);
                            if (slots.Count != 0 && PredicatePlan.Safe(input) && slots.All(slot => slot < a.Columns.Length) && join.Kind is "inner" or "cross" or "left") leftFilters.Add(input);
                            else if (slots.Count != 0 && !lateral && PredicatePlan.Safe(input) && slots.All(slot => slot >= a.Columns.Length) && join.Kind is "inner" or "cross" or "right") rightFilters.Add(PredicatePlan.Shift(input, -a.Columns.Length));
                            else residual.Add(item);
                        }
                        var nextLeft = leftFilters.Count == 0 ? a : PredicatePlan.Filter(a, PredicatePlan.Order(PredicatePlan.Combine(leftFilters))!, meter, cancellationToken);
                        var nextRight = rightFilters.Count == 0 ? b : PredicatePlan.Filter(b, PredicatePlan.Order(PredicatePlan.Combine(rightFilters))!, meter, cancellationToken);
                        return (Build(nextLeft, nextRight, on, PredicatePlan.Combine(residual.Select(InputPredicate)), touched, reversed), PredicatePlan.Combine(residual));
                    } };
                    return retained;
                }
                return Build(left, right, PredicatePlan.Order(predicate));
            }
            default: throw Errors.Unsupported("relation source");
        }
    }
    private static bool SameSource(BoundColumn[] columns, IEnumerable<int> slots)
    {
        var selected = slots.Select(slot => columns[slot]).ToArray();
        if (selected.Length == 0) return false;
        return selected.All(column => selected[0].BaseSource is { } identity ? ReferenceEquals(column.BaseSource, identity) : column.Qualifier == selected[0].Qualifier);
    }
    internal static PreparedRelation Query(SelectSyntax query, Database database, DatabaseSnapshot snapshot, PendingDatabase? pending,
        Catalog catalog, Meter meter, CancellationToken cancellationToken, bool derived = false, QueryScope? scope = null)
    {
        ViewLimits.Validate(query, catalog, scope);
        if (query.With?.Any(definition => definition.Mutation is not null) == true) throw Errors.Unsupported("nested data-modifying WITH");
        query = QueryCounts.Resolve(Resolve.BindTypes(query, catalog, meter.Temporal));
        var buffers = new List<CteBuffer>();
        var ctePlans = new List<(ExplainNode Node, bool Executed)>();
        if (query.With is { Length: > 0 } definitions)
        {
            scope = new(scope);
            for (var i = 0; i < definitions.Length; i++)
            {
                var definition = definitions[i];
                var recursive = query.Recursive && CteBuffer.References(definition.Query, definition.Name) > 0;
                if (!recursive && (definition.Search is not null || definition.Cycle is not null)) throw Errors.Syntax("SEARCH/CYCLE query is not recursive");
                var body = recursive ? RecursiveExecution.Prepare(definition, database, snapshot, pending, catalog, meter, cancellationToken, scope) :
                    Query(definition.Query, database, snapshot, pending, catalog, meter, cancellationToken, scope: scope);
                body = body with { Columns = Alias(body.Columns.Select(column => column with { Collation = column.Collation?.Implicit() }).ToArray(), definition.Name, definition.Columns) };
                var references = CteBuffer.References(query with { With = null }, definition.Name) + definitions.Skip(i + 1).Sum(later => CteBuffer.References(later.Query, definition.Name));
                if (references > 0 && (recursive || definition.Materialized == true || definition.Materialized is null && references > 1))
                {
                    var buffer = new CteBuffer(body, meter); buffers.Add(buffer); body = body with { Rows = buffer.Read(cancellationToken), PushFilter = null, BaseAccess = null, MemoryOutput = true };
                }
                if (references != 1) body = body with { PushFilter = null, BaseAccess = null };
                if (meter.Explain is not null && body.Plan is { } definitionPlan)
                {
                    var materialized = references > 0 && (recursive || definition.Materialized == true || definition.Materialized is null && references > 1);
                    var inlined = references > 0 && !materialized;
                    ctePlans.Add((new("CTE " + definition.Name, materialized ? "materialized" : "inlined",
                        definitionPlan.EstimatedRows, inlined ? 0 : definitionPlan.EstimatedCost, inlined ? [] : [definitionPlan]), materialized));
                    body = body with { Cte = new(definition.Name, materialized, definitionPlan) };
                }
                scope.Add(definition.Name, body);
            }
        }
        var subqueries = new List<SubqueryCell>();
        SelectPlan plan; PreparedRelation? preparedInput = null; BaseRelationAccess? baseInput = null; OuterFrame frame; OuterFrame[] inputDependencies = []; Func<ValueTask>? inputConstants = null;
        if (query.Table is not null && !CatalogRelations.IsCatalogName(query.Table) && !(scope?.TryGet(query.Table, out _) ?? false) && (catalog.Scope is { } domains ? domains.View(query.Table) is null : !(catalog.Views ?? []).Any(view => view.Name == query.Table)))
        {
            var table = StatementExecution.ResolveTable(catalog, query.Table);
            var columns = StatementExecution.Columns(table, query.TableAlias); frame = new(columns);
            query = SubqueryExecution.Bind(query, frame, database, snapshot, pending, catalog, meter, cancellationToken, scope, subqueries);
            plan = Resolve.Select(query, columns.Concat(scope?.OuterColumns ?? []).ToArray());
            baseInput = new(table, database.Tree(table, pending, snapshot), null, new(database, snapshot, pending, catalog, MembershipBounds: !derived));
        }
        else
        {
            var input = query.Relation is null ? new PreparedRelation([], QueryExecution.ScalarSource(meter)) : Prepare(query.Relation, database, snapshot, pending, catalog, meter, cancellationToken, scope);
            frame = new(input.Columns); inputDependencies = input.Dependencies ?? []; inputConstants = input.InitializeConstants;
            query = SubqueryExecution.Bind(query, frame, database, snapshot, pending, catalog, meter, cancellationToken, scope, subqueries);
            plan = Resolve.Select(query, input.Columns.Concat(scope?.OuterColumns ?? []).ToArray());
            preparedInput = input;
        }
        if (derived && query.ValuesQuery && query.Order is not { Length: > 0 } && query.Limit is null && query.Offset == 0) plan = plan with { ChargeRows = false };

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

        var dependencies = Correlation.Dependencies(plan);
        dependencies.UnionWith(inputDependencies);
        foreach (var buffer in buffers) dependencies.UnionWith(buffer.Dependencies);
        dependencies.Remove(frame);
var outputColumns = plan.Projections.Select(projection => new BoundColumn(projection.Name, projection.Expression.Type, Collation: projection.Expression.Collation)).ToArray();
PreparedRelation Build(SelectPlan requested)
{
    var active = requested;
    if (active.Windows is null && active.DistinctOnCount == 0 && database.ScratchHost is { } scratch && database.WorkMem > 0)
        active = active with { Spill = new(scratch, database.WorkMem, baseInput?.Table) };
    IAsyncEnumerable<SqlValue[]> rows;
    var sourceMemory = false;
    var boundedInput = false;
    ExplainNode? inputPlan = null;
    ExplainNode? outputPlan = null;
    if (baseInput is { } source)
    {
        var planning = PlanTransform.CorrelatedAccess(active.Filter) ? active with { Filter = PlanTransform.GenericAccess(active.Filter!) } : active;
        if (!source.Storage.MembershipBounds && planning.Filter is not null) planning = planning with { Filter = PlanTransform.WithoutMembershipBounds(planning.Filter) };
        var pipeline = SinglePipeline.Select(source.Table, source.Tree, planning, source.Storage, meter);
        active = pipeline.Plan with { Filter = active.Filter };
        if (meter.Explain is not null)
        {
            inputPlan = ExplainPlanning.Scan(source.Table, pipeline.Access, active.Filter, StorageTouchedColumns.Collect(requested), false, meter, catalog, source.Tree.Height);
            if (pipeline.Ordered)
                inputPlan.Append(pipeline.Access.Ordered ? "ordered: index order: " + pipeline.Access.Index!.Name : "ordered: pk ordered" + (pipeline.Reverse ? " (reverse)" : ""));
        }
        rows = QueryExecution.DeferredTableSource(source.Table, source.Tree, active, meter, cancellationToken, source.Storage);
        if (inputPlan is not null) rows = ExplainObservation.Rows(rows, inputPlan, meter, cancellationToken);
        active = active with { AggregateLane = GroupExecution.Lane(active), ColumnarProjection = active.Group is null && active.Windows is null && !active.Distinct &&
            active.Limit is null && active.Offset == 0 && active.Order is not { Length: > 0 } && active.Projections.All(projection => projection.Expression.Op == "column") };
    }
    else
    {
        var input = preparedInput!;
        if (active.Filter is { } filter && input.PushFilter is { } push)
        {
            var pushed = push(filter); input = pushed.Relation; active = active with { Filter = pushed.Residual };
        }
        if (input.WithTouched is { } withTouched) input = withTouched(StorageTouchedColumns.Collect(active));
        if (active.Group is null && active.Windows is null && !active.Distinct && active.Limit is { } limit && input.Bounded is { } bounded &&
            bounded(active.Order ?? [], Estimate.Add(active.Offset, limit), active.Filter) is { } boundedRelation)
        {
            input = boundedRelation;
            if (meter.Explain is not null && active.Order is { Length: > 0 }) input.Plan?.Append("ordered: join pk ordered");
            active = active with { Order = [], Filter = null };
        }
        rows = input.Rows; sourceMemory = input.MemoryOutput; boundedInput = input.BoundedOutput;
        inputPlan = input.Plan;
        if (meter.Explain is not null && !derived && query.ValuesQuery && inputPlan is { Label: "Values" })
            inputPlan = new("Subquery *VALUES*", "-", inputPlan.EstimatedRows, inputPlan.EstimatedCost, inputPlan);
    }
    if (meter.Explain is not null)
    {
        inputPlan ??= query.Relation is null ? new ExplainNode("Result", "-", 1, 0) : throw new InvalidOperationException("missing EXPLAIN relation plan");
        active = active with { Observation = ExplainPlanning.Select(active, inputPlan, meter) };
        outputPlan = active.Observation.Root;
        if (ctePlans.Count != 0)
        {
            var child = active.Observation!.Root;
            var cost = ctePlans.Where(item => item.Executed).Aggregate(child.EstimatedCost, (sum, item) => Estimate.Add(sum, item.Node.EstimatedCost));
            active = active with { Observation = active.Observation with { Root = new("WITH", "ctes=" + ctePlans.Count,
                child.EstimatedRows, cost, ctePlans.Select(item => item.Node).Append(child).ToArray()) } };
        }
    }
    var identityValues = derived && query.ValuesQuery && active.Order is not { Length: > 0 } && active.Filter is null &&
        active.Projections.Select((projection, slot) => projection.Expression.Op == "column" && projection.Expression.Slot == slot).All(value => value);
    var result = Projected(QueryExecution.Select(active, rows, meter, cancellationToken, sourceMemory, eagerInput: baseInput is null && query.Relation is not null && !boundedInput, transferIdentity: identityValues, bufferOutput: derived || buffers.Count != 0));
    if (buffers.Count > 0 || subqueries.Count > 0) result = SubqueryExecution.Initialized(Initialize, result, cancellationToken);
    if (buffers.Count > 0) result = CteBuffer.DisposeAfter(buffers, result, cancellationToken);
    if (outputPlan is not null && !ReferenceEquals(outputPlan, active.Observation?.Root)) result = ExplainObservation.Rows(result, outputPlan, meter, cancellationToken, inclusive: true);
    if (active.Observation is { } observation) result = ExplainObservation.Rows(result, observation.Root, meter, cancellationToken, inclusive: true);
    var prepared = new PreparedRelation(outputColumns, result, Initialize, dependencies.ToArray(), Constants, EstimatedRows: Estimate.Output(requested, baseInput?.Table.RowCount ?? preparedInput?.EstimatedRows ?? (query.Relation is null ? 1 : PlannerFacts.SrfRows), baseInput?.Table, catalog), MemoryOutput: identityValues || derived || buffers.Count != 0, Plan: active.Observation?.Root);
    if (query.With is not null || query.ValuesQuery || requested.Limit is not null || requested.Offset != 0) return prepared;
    return prepared with { PushFilter = filter =>
    {
        var split = DerivedPushdown.Split(requested, filter);
        if (split.Pushed is null) return (prepared, filter);
        var combined = PredicatePlan.Order(PredicatePlan.Combine(new[] { requested.Filter, split.Pushed }.OfType<Resolved>()));
        return (Build(requested with { Filter = combined, Empty = combined is not null && Resolve.EmptyFilter(combined) }), split.Residual);
    } };
}
return Build(plan);
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
        foreach (var row in rows)
        {
            meter.Checkpoint(cancellationToken); meter.Charge(Costs.RowProduced);
            var result = new SqlValue[row.Length];
            for (var i = 0; i < row.Length; i++) result[i] = await Evaluate.ExprAsync(row[i], meter).ConfigureAwait(false);
            meter.Checkpoint(cancellationToken); yield return result;
        }
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
private static async IAsyncEnumerable<SqlValue[]> GroupRows(IAsyncEnumerable<SqlValue[]> rows, Meter meter,
    [EnumeratorCancellation] CancellationToken token)
{
    await foreach (var row in rows.WithCancellation(token).ConfigureAwait(false))
    { meter.Charge(Costs.RowProduced); meter.Checkpoint(token); yield return row; }
}
    internal static async IAsyncEnumerable<SqlValue[]> Join(PreparedRelation left, PreparedRelation right, Resolved? predicate, string kind,
        (int Left, int Right, SqlType Type)[] usingSlots, Meter meter, [EnumeratorCancellation] CancellationToken cancellationToken, OuterFrame? lateral = null, HashJoinPlan? hashPlan = null, IndexNestedLoop? inl = null, HashSet<int>? touched = null, SpillPolicy? spill = null)
    {
        using var outputMemory = new QueryInputMemory(meter);
        using var leftMemory = new QueryInputMemory(meter);
        using var rightMemory = new QueryInputMemory(meter);
        var output = new List<SqlValue[]>();
        var needed = touched is null ? Enumerable.Range(0, usingSlots.Length + left.Columns.Length + right.Columns.Length).ToHashSet() : new HashSet<int>(touched);
        if (predicate is not null) foreach (var slot in PredicatePlan.Slots(predicate)) needed.Add(slot + usingSlots.Length);
        if (lateral is not null) foreach (var slot in lateral.ReferencedSlots) needed.Add(slot + usingSlots.Length);
        if (left.BaseAccess?.Filter is { } leftFilter) foreach (var slot in PredicatePlan.Slots(leftFilter)) needed.Add(slot + usingSlots.Length);
        if (right.BaseAccess?.Filter is { } rightFilter) foreach (var slot in PredicatePlan.Slots(rightFilter)) needed.Add(slot + usingSlots.Length + left.Columns.Length);
        foreach (var pair in usingSlots) { needed.Add(usingSlots.Length + pair.Left); needed.Add(usingSlots.Length + left.Columns.Length + pair.Right); }
        var leftTouched = needed.Where(slot => slot >= usingSlots.Length && slot < usingSlots.Length + left.Columns.Length).Select(slot => slot - usingSlots.Length).ToHashSet();
        var rightTouched = needed.Where(slot => slot >= usingSlots.Length + left.Columns.Length).Select(slot => slot - usingSlots.Length - left.Columns.Length).ToHashSet();
        if (left.WithTouched is { } leftMask) left = leftMask(leftTouched);
        if (right.WithTouched is { } rightMask) right = rightMask(rightTouched);
        if (spill is not null)
        {
            await foreach (var row in SpillJoin.Run(left, right, predicate, kind, usingSlots, spill, hashPlan, inl, meter, leftTouched, rightTouched, needed, lateral, cancellationToken).ConfigureAwait(false)) yield return row;
            yield break;
        }
        var leftRows = new List<SqlValue[]>(); var rightRows = new List<SqlValue[]>();
        await foreach (var row in left.Rows.WithCancellation(cancellationToken).ConfigureAwait(false)) leftRows.Add(await leftMemory.TakeAsync(row, left.MemoryOutput, leftTouched).ConfigureAwait(false));
        if (lateral is null && inl is null) await foreach (var row in right.Rows.WithCancellation(cancellationToken).ConfigureAwait(false)) rightRows.Add(await rightMemory.TakeAsync(row, right.MemoryOutput, rightTouched).ConfigureAwait(false));
        using var hash = hashPlan is null ? null : await HashJoinTable.Build(hashPlan, rightRows, meter).ConfigureAwait(false);
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
                lateral.Assign(a); rightMemory.Dispose(); rightRows.Clear();
                if (right.Initialize is { } initialize) await initialize().ConfigureAwait(false);
                await foreach (var row in right.Rows.WithCancellation(cancellationToken).ConfigureAwait(false)) rightRows.Add(await rightMemory.TakeAsync(row, right.MemoryOutput, rightTouched).ConfigureAwait(false));
                matchedRight = new bool[rightRows.Count];
            }
if (inl is not null)
{
    rightMemory.Dispose(); rightRows.Clear();
    await foreach (var row in inl.Rows(a, meter, cancellationToken).ConfigureAwait(false)) rightRows.Add(await rightMemory.TakeAsync(row, false, rightTouched).ConfigureAwait(false));
    matchedRight = new bool[rightRows.Count];
}
var matched = false;

            IEnumerable<int> candidates = hash is null ? Enumerable.Range(0, rightRows.Count) : await hash.Probe(hashPlan!, a, meter).ConfigureAwait(false);
            foreach (var i in candidates)
            {
                meter.Checkpoint(cancellationToken); var b = rightRows[i];
                if (!await QueryExecution.KeepsAsync(predicate, a.Concat(b).ToArray(), meter).ConfigureAwait(false)) continue;
                matched = true; matchedRight[i] = true; output.Add(outputMemory.Take(Output(a, b), false, needed));
            }
            if (!matched && kind is "left" or "full") output.Add(outputMemory.Take(Output(a, right.Columns.Select(c => SqlValue.Null(c.Type)).ToArray()), false, needed));
        }
        if (kind is "right" or "full")
            for (var i = 0; i < rightRows.Count; i++)
                if (!matchedRight[i]) output.Add(outputMemory.Take(Output(left.Columns.Select(c => SqlValue.Null(c.Type)).ToArray(), rightRows[i]), false, needed));
        leftMemory.Dispose(); rightMemory.Dispose(); hash?.Dispose();
        foreach (var row in output) { outputMemory.Transfer(row); yield return row; }
    }
}
