namespace Jed;

internal sealed record PreparedMerge(BoundColumn[] Columns, Func<ValueTask<StatementResult>> Run, ExplainNode? Plan = null);

internal static partial class StatementExecution
{
    private sealed record MergeClausePlan(MergeClauseSyntax Syntax, Resolved? Condition, BoundColumn[] Columns,
        (int Slot, Expression Value)[] Assignments, SubqueryBinder Binder);
    private sealed record MergeAction(MergeClausePlan Clause, StorageRecord? Old, SqlValue[]? Original,
        SqlValue[]? Values, SqlValue[] Source, StorageRecord? Placed);

    internal static PreparedMerge PrepareMerge(Database database, PendingDatabase pending, MergeSyntax syntax,
        Meter meter, CancellationToken token, StatementContext? context = null, bool localDomain = false)
    {
        if (!localDomain && pending.Catalog.Scope is { } domains)
        {
            var home = DomainExecution.ResolveTarget(syntax, domains);
            var owner = home.Pending ?? new PendingDatabase(home.Snapshot);
            owner.Catalog = domains.BindCatalog(home);
            return PrepareMerge(home.Database, owner, (MergeSyntax)DomainExecution.Local(syntax, home.Name, domains), meter, token, context?.ForDomain(home.Name), true);
        }
        if ((pending.Catalog.Views ?? []).FirstOrDefault(view => view.Name == syntax.Table) is { } namedView)
            return PrepareMerge(database, pending, (MergeSyntax)RewriteView(database, pending, syntax, namedView, meter, token), meter, token, context);
        var table = Table(pending.Catalog, syntax.Table); var read = context?.Read ?? pending.Fork();
        var target = table with { Name = syntax.Alias ?? table.Name };
        var source = RelationExecution.Prepare(syntax.Source, database, pending.Before, read, read.Catalog, meter, token, context?.Scope);
        var label = syntax.Source switch
        {
            TableRelation named => named.Alias ?? DatabaseDomains.Name(named.Name).Local,
            DerivedRelation derived => derived.Alias, FunctionRelation function => function.Alias ?? function.Function.Text,
            AliasedRelation aliased => aliased.Alias, _ => null
        };
        if (label == target.Name) throw new SqlException(SqlStates.DuplicateAlias, "MERGE source and target have the same label");
        var width = table.Columns.Length;
        Resolved Shift(Resolved expression, int offset) => expression with
        {
            Slot = expression.Op == "column" ? expression.Slot + offset : expression.Slot,
            Args = expression.Args.Select(child => Shift(child, offset)).ToArray()
        };
        BoundColumn[] Positioned(BoundColumn[] columns, int offset) => columns.Select((column, index) => column with
        {
            Projection = column.Projection is { } projection ? Shift(projection, offset) :
                new("column", column.Type, default, [], Slot: index + offset, Collation: column.Collation)
        }).ToArray();
        var view = syntax.View;
        var targetColumns = view is null ? Positioned(Columns(target), 0) : view.View.Columns.Select((column, index) => new BoundColumn(column.Name,
            view.Projection[index].Type, target.Name, Projection: view.Projection[index], Collation: view.Projection[index].Collation, WholeRow: false)).ToArray();
        var sourceColumns = Positioned(source.Columns, width);
        var all = targetColumns.Concat(sourceColumns).ToArray();
        var binder = new SubqueryBinder(all, database, pending.Before, read, read.Catalog, meter, token, context?.Scope);
        Resolved Condition(Expression expression, SubqueryBinder binding)
        {
            ValidateReturning([new(expression, null)]);
            var value = Resolve.Expr(binding.Bind(expression), SqlType.Boolean);
            if (value.Type.Kind is not (SqlKind.Boolean or SqlKind.Unknown)) throw Errors.Type("MERGE condition requires boolean");
            return PredicatePlan.Order(value)!;
        }
        void Ambiguous(Expression expression, bool both)
        {
            if (both && source.Columns.Any(column => column.Qualifier == target.Name) &&
                (expression.Op == "name" && expression.Text.StartsWith(target.Name + ".", StringComparison.Ordinal) || expression.Op == "star" && expression.Text == target.Name))
                throw new SqlException(SqlStates.AmbiguousAlias, "MERGE relation reference is ambiguous");
            foreach (var child in expression.Args) Ambiguous(child, both);
        }
        Ambiguous(syntax.On, true);
        var on = Condition(syntax.On, binder);
        if (view?.Filter is { } restrict) on = new("and", SqlType.Boolean, default, [restrict, on]);
        var clauses = syntax.Clauses.Select(clause =>
        {
            var visible = clause.Match == "target" ? sourceColumns : clause.Match == "source" ? targetColumns : all;
            var binding = new SubqueryBinder(visible, database, pending.Before, read, read.Catalog, meter, token, context?.Scope);
            if (clause.Condition is { } conditional) Ambiguous(conditional, clause.Match == "matched");
            var condition = clause.Condition is null ? null : Condition(clause.Condition, binding);
            var assignments = new List<(int Slot, Expression Value)>();
            if (clause.Action == "update") assignments.AddRange(clause.Assignments!.Select(item => (ColumnIndex(table, item.Column), item.Value)));
            if (clause.Action == "insert")
            {
                var slots = clause.Columns is null ? Enumerable.Range(0, width).ToArray() : clause.Columns.Select(name => ColumnIndex(table, name)).ToArray();
                if (slots.Distinct().Count() != slots.Length) throw new SqlException(SqlStates.DuplicateColumn, "duplicate MERGE insert column");
                if (clause.Values!.Length > slots.Length || clause.Columns is not null && clause.Values.Length != slots.Length)
                    throw Errors.Syntax("MERGE insert value count differs from target");
                assignments.AddRange(clause.Values.Select((value, index) => (slots[index], value)));
            }
            if (assignments.Select(item => item.Slot).Distinct().Count() != assignments.Count) throw new SqlException(SqlStates.DuplicateColumn, "column assigned twice");
            for (var i = 0; i < assignments.Count; i++)
            {
                var (slot, expression) = assignments[i]; if (expression.Op == "default") continue;
                var declaration = table.Columns[slot];
                if (declaration.IdentityAlways && !(clause.Action == "insert" && clause.Overriding is "system" or "user"))
                    throw new SqlException(SqlStates.GeneratedAlways, "explicit value for GENERATED ALWAYS identity");
                Ambiguous(expression, clause.Match == "matched");
                ValidateReturning([new(expression, null)]);
                if (expression.Parameter is { } parameter) parameter.NotNull |= declaration.NotNull;
                expression = binding.Bind(AssignmentLiteral(expression, declaration.Type));
                Assignable(Resolve.Expr(expression, declaration.Type.Type).Type, declaration.Type.Type);
                assignments[i] = (slot, expression);
            }
            return new MergeClausePlan(clause, condition, visible, assignments.ToArray(), binding);
        }).ToArray();
        var targetReturning = ReturningColumns(target, syntax.ReturningAliases);
        if (view is not null)
        {
            var pseudo = new CatalogTable(target.Name, view.View.Columns.Select((column, index) => new CatalogColumn(column.Name, new(view.Projection[index].Type))).ToArray(), []);
            targetReturning = ReturningColumns(pseudo, syntax.ReturningAliases).Select((column, index) => column with
            { Projection = Shift(view.Projection[index % view.Projection.Length], index / view.Projection.Length * width) }).ToArray();
        }
        var returnColumns = Positioned(source.Columns, 0).Concat(Positioned(targetReturning, source.Columns.Length)).ToArray();
        if (syntax.ReturningAliases is null)
            returnColumns = returnColumns.Select((column, index) => index >= source.Columns.Length + width &&
                source.Columns.Any(other => other.Qualifier == column.Qualifier) ? column with { Qualifier = null } : column).ToArray();
        else if (source.Columns.Any(column => column.Qualifier == syntax.ReturningAliases.Old || column.Qualifier == syntax.ReturningAliases.New))
            throw new SqlException(SqlStates.DuplicateAlias, "RETURNING alias collides with source");
        var returnBinder = new SubqueryBinder(returnColumns, database, pending.Before, read, read.Catalog, meter, token, context?.Scope);
        Expression Action(Expression expression)
        {
            if (expression.Op == "call" && expression.Text == "merge_action")
            {
                if (expression.Args.Length != 0) throw new SqlException(SqlStates.UndefinedFunction, "merge_action takes no arguments");
                return new("bound", "merge_action", [], Binding: new("column", SqlType.Text, default, [], Slot: source.Columns.Length + width * 3));
            }
            return expression with { Args = expression.Args.Select(Action).ToArray() };
        }
        foreach (var item in syntax.Returning) Ambiguous(item.Expression, true);
        ValidateReturning(syntax.Returning);
        var returning = syntax.Returning.Length == 0 ? [] : Resolve.Select(new(syntax.Returning.Select(item => item with
        { Expression = item.Expression.Op == "star" ? item.Expression : returnBinder.Bind(Action(item.Expression)) }).ToArray(), null, null, 0), returnColumns).Projections;
        var touched = new HashSet<int>();
        void Touch(Resolved node, int sourceOffset = 0)
        {
            if (node.Op == "column" && node.Slot >= sourceOffset && node.Slot < sourceOffset + width * (sourceOffset == 0 ? 1 : 3)) touched.Add((node.Slot - sourceOffset) % width);
            if (node.Subquery is { } subquery) foreach (var slot in subquery.ReferencedSlots) if (slot < width) touched.Add(slot);
            foreach (var child in node.Args) Touch(child, sourceOffset);
        }
        Touch(on); if (view?.Filter is { } viewFilter) Touch(viewFilter);
        foreach (var clause in clauses)
        {
            if (clause.Condition is { } condition) Touch(condition);
            foreach (var assignment in clause.Assignments) if (assignment.Value.Op != "default") Touch(Resolve.Expr(assignment.Value, table.Columns[assignment.Slot].Type.Type));
        }
        foreach (var projection in returning) Touch(projection.Expression, source.Columns.Length);
        var scanTree = database.Tree(table, read, pending.Before);
        var storage = new QueryStorage(database, pending.Before, read, read.Catalog);
        var planning = MergePlanning.Create(table, scanTree, source, on, view?.Filter, clauses.Any(clause => clause.Syntax.Match == "source"), clauses.Length, touched, storage, meter);
        return new(returning.Select(projection => new BoundColumn(projection.Name, projection.Expression.Type, Collation: projection.Expression.Collation)).ToArray(), Run, planning.Root);

        async ValueTask<StatementResult> Run()
        {
            if (source.Initialize is { } initialize) await initialize().ConfigureAwait(false);
            await binder.Initialize().ConfigureAwait(false); foreach (var clause in clauses) await clause.Binder.Initialize().ConfigureAwait(false);
            await returnBinder.Initialize().ConfigureAwait(false);
            var startCost = meter.Accrued; var sourceRows = new List<SqlValue[]>(); long sourceBytes = 0; long matchingCost = 0;
            try
            {
                await foreach (var raw in source.Rows.WithCancellation(token).ConfigureAwait(false))
                {
                    var values = new SqlValue[raw.Length]; for (var i = 0; i < values.Length; i++) values[i] = await raw[i].ResolveAsync(token).ConfigureAwait(false);
                    if (!source.MemoryOutput) meter.Memory.ReserveRow(values);
                    sourceBytes = Meter.Add(sourceBytes, MemorySize.RowBytes(values)); sourceRows.Add(values);
                    meter.Charge(Costs.RowProduced); source.Plan?.AttributeLocal(Costs.RowProduced); meter.Checkpoint(token);
                }
                source.Plan?.SetInclusive(meter.Accrued - startCost);
                matchingCost = meter.Accrued - startCost;
                var actions = new List<MergeAction>(); var matched = new HashSet<string>(StringComparer.Ordinal); var affected = new HashSet<string>(StringComparer.Ordinal);
                var nullTarget = table.Columns.Select(column => SqlValue.Null(column.Type.Type)).ToArray();
                var nullSource = source.Columns.Select(column => SqlValue.Null(column.Type)).ToArray();
                async ValueTask Plan(string match, StorageRecord? record, SqlValue[] targetValues, SqlValue[] sourceValues)
                {
                    var combined = targetValues.Concat(sourceValues).ToArray();
                    foreach (var clause in clauses.Where(item => item.Syntax.Match == match))
                    {
                        if (!await QueryExecution.KeepsAsync(clause.Condition, combined, meter).ConfigureAwait(false)) continue;
                        if (clause.Syntax.Action == "nothing") return;
                        if (record is not null && !affected.Add(Convert.ToHexString(record.Key))) throw new SqlException(SqlStates.CardinalityViolation, "MERGE cannot affect row a second time");
                        SqlValue[]? original = null; SqlValue[]? values = null;
                        if (record is not null) original = table.DecodeRow(await scanTree.MaterializeRecordAsync(record, token).ConfigureAwait(false));
                        if (clause.Syntax.Action != "delete")
                        {
                            values = original?.ToArray() ?? new SqlValue[width];
                            for (var i = 0; i < width; i++)
                            {
                                var assigned = Array.FindIndex(clause.Assignments, item => item.Slot == i);
                                if (assigned < 0 && original is not null) continue;
                                var expression = assigned < 0 ? null : clause.Assignments[assigned].Value;
                                var declaration = table.Columns[i];
                                values[i] = expression is null || expression.Op == "default" || declaration.Identity && clause.Syntax.Overriding == "user"
                                    ? clause.Syntax.Action == "insert" && view is not null && view.Defaults.TryGetValue(i, out var viewDefault) ? Assign(viewDefault, declaration.Type, [], [], meter, pending.Catalog) : DefaultValue(declaration, meter, pending.Catalog)
                                    : await AssignAsync(expression, declaration.Type, clause.Columns, combined, meter, pending.Catalog).ConfigureAwait(false);
                            }
                            Validate(table, values, meter, pending.Catalog);
                            if (view is not null) await ValidateViewChecks(view.Checks.Select(check => new Expression("bound", "", [], Binding: check)).ToArray(), values, meter).ConfigureAwait(false);
                        }
                        actions.Add(new(clause, record, original, values, sourceValues, null)); return;
                    }
                }
                var hashRows = new List<SqlValue[]>(); var hashRecords = new List<StorageRecord>(); long hashBytes = 0;
                HashJoinTable? hashTable = null;
                if (planning.Hash is { } hashPlan)
                {
                    var before = meter.Accrued;
                    await foreach (var record in MergePlanning.Observe(AccessPlan.Primary(table, scanTree, null).Records(table, scanTree, storage, meter, touched, token), planning.ProbeNode, meter, token).ConfigureAwait(false))
                    {
                        var row = scanTree.DecodeDeferredRow(table, record); hashRecords.Add(record); hashRows.Add(row);
                        hashBytes = Meter.Add(hashBytes, meter.Memory.ReserveRow(row, Enumerable.Range(0, width).Select(touched.Contains).ToArray()));
                    }
                    hashTable = await HashJoinTable.Build(hashPlan, hashRows, meter).ConfigureAwait(false);
                    matchingCost = Meter.Add(matchingCost, meter.Accrued - before);
                }
                try
                {
                foreach (var sourceRow in sourceRows)
                {
                    Resolved Substitute(Resolved value) => value.Op == "column" && value.Slot >= width
                        ? new("constant", value.Type, sourceRow[value.Slot - width], [], Collation: value.Collation)
                        : value with { Args = value.Args.Select(Substitute).ToArray() };
                    var access = planning.ForRow(table, scanTree, Substitute(PlanTransform.InitialValues(on)), storage, meter);
                    var found = false;
                    async IAsyncEnumerable<StorageRecord> Candidates()
                    {
                        if (hashTable is not null)
                        {
                            var before = meter.Accrued; var positions = await hashTable.Probe(planning.Hash!, sourceRow, meter).ConfigureAwait(false);
                            matchingCost = Meter.Add(matchingCost, meter.Accrued - before);
                            foreach (var position in positions) yield return hashRecords[position];
                        }
                        else
                        {
                            await using var iterator = MergePlanning.Observe(access.Records(table, scanTree, storage, meter, touched, token), planning.ProbeNode, meter, token).GetAsyncEnumerator(token);
                            while (true)
                            {
                                var before = meter.Accrued; var hasRow = await iterator.MoveNextAsync().ConfigureAwait(false);
                                matchingCost = Meter.Add(matchingCost, meter.Accrued - before);
                                if (!hasRow) yield break; yield return iterator.Current;
                            }
                        }
                    }
                    await foreach (var record in Candidates().ConfigureAwait(false))
                    {
                        var targetRow = scanTree.DecodeDeferredRow(table, record);
                        var beforeOn = meter.Accrued;
                        var keeps = await QueryExecution.KeepsAsync(on, targetRow.Concat(sourceRow).ToArray(), meter).ConfigureAwait(false);
                        matchingCost = Meter.Add(matchingCost, meter.Accrued - beforeOn); if (!keeps) continue;
                        found = true; matched.Add(Convert.ToHexString(record.Key));
                        await Plan("matched", record, targetRow, sourceRow).ConfigureAwait(false);
                    }
                    if (!found) await Plan("target", null, nullTarget, sourceRow).ConfigureAwait(false);
                    meter.Checkpoint(token);
                }
                }
                finally { hashTable?.Dispose(); meter.Memory.Release(hashBytes); }
                planning.Loop?.SetInclusive(matchingCost);
                if (clauses.Any(clause => clause.Syntax.Match == "source"))
                {
                    var access = AccessPlan.Select(table, scanTree, view?.Filter, storage, meter);
                    await foreach (var record in MergePlanning.Observe(access.Records(table, scanTree, storage, meter, touched, token), planning.FinalNode, meter, token).ConfigureAwait(false))
                        if (!matched.Contains(Convert.ToHexString(record.Key)))
                        {
                            var values = scanTree.DecodeDeferredRow(table, record);
                            if (view?.Filter is { } filter)
                            {
                                var beforeFilter = meter.Accrued; var keeps = await QueryExecution.KeepsAsync(filter, values, meter).ConfigureAwait(false);
                                planning.FinalFilter?.AttributeLocal(meter.Accrued - beforeFilter); if (!keeps) continue;
                            }
                            await Plan("source", record, values, nullSource).ConfigureAwait(false);
                        }
                }
                var result = await ApplyMerge(database, pending, table, actions, returning, meter, token, context).ConfigureAwait(false);
                planning.Root?.SetInclusive(meter.Accrued - startCost); return result;
            }
            finally { meter.Memory.Release(sourceBytes); }
        }
    }
}
