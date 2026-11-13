namespace Jed;

internal static partial class StatementExecution
{
    private sealed record ConflictArbiter(bool Primary, CatalogIndex? Index, CatalogExclusion? Exclusion = null);
    private sealed record UpsertRow(StorageRecord? Old, StorageRecord Record, SqlValue[] Values, byte[]? ArbiterKey);
    private static string BtreeClass(SqlType type) => type.Kind switch
    {
        SqlKind.I16 => "int2_ops", SqlKind.I32 => "int4_ops", SqlKind.I64 => "int8_ops", SqlKind.Boolean => "bool_ops",
        SqlKind.F32 => "float4_ops", SqlKind.F64 => "float8_ops", SqlKind.Text => "text_ops", SqlKind.Decimal => "numeric_ops",
        SqlKind.Bytea => "bytea_ops", SqlKind.Uuid => "uuid_ops", SqlKind.Date => "date_ops", SqlKind.Timestamp => "timestamp_ops",
        SqlKind.Timestamptz => "timestamptz_ops", SqlKind.Interval => "interval_ops", SqlKind.Array => "array_ops",
        SqlKind.Composite => "record_ops", SqlKind.Range => "range_ops", SqlKind.Jsonb => "jsonb_ops", _ => ""
    };
    private static IEnumerable<Resolved> Conjuncts(Resolved value) => value.Op == "and" ? value.Args.SelectMany(Conjuncts) : [value];
    private static bool PredicateImplies(Resolved premise, Resolved required)
    {
        var premises = Conjuncts(premise).ToArray();
        return Conjuncts(required).All(target => premises.Any(source => ImpliesAtom(source, target)));
    }
    private static bool ImpliesAtom(Resolved source, Resolved target)
    {
        if (Resolve.Equivalent(source, target)) return true;
        if (source.Args.Length != 2 || target.Args.Length != 2 ||
            !Resolve.Equivalent(source.Args[0], target.Args[0]) || source.Args[1].Op != "constant" || target.Args[1].Op != "constant" ||
            source.Args[1].Constant.IsNull || target.Args[1].Constant.IsNull) return false;
        var comparison = Evaluate.Compare(source.Args[1].Constant, target.Args[1].Constant);
        return target.Op switch
        {
            ">" => source.Op is ">" or ">=" or "=" && comparison > 0 || source.Op == ">" && comparison == 0,
            ">=" => source.Op is ">" or ">=" or "=" && comparison >= 0,
            "<" => source.Op is "<" or "<=" or "=" && comparison < 0 || source.Op == "<" && comparison == 0,
            "<=" => source.Op is "<" or "<=" or "=" && comparison <= 0,
            _ => false
        };
    }
    private static bool KnownBtreeClass(string name) => name is "int2_ops" or "int4_ops" or "int8_ops" or "bool_ops" or "text_ops" or "varchar_ops" or "numeric_ops" or "bytea_ops" or "uuid_ops" or "timestamp_ops" or "timestamptz_ops" or "interval_ops" or "float4_ops" or "float8_ops" or "date_ops" or "jsonb_ops" or "array_ops" or "range_ops" or "record_ops";
    private static ConflictArbiter[] ConflictArbiters(CatalogTable table, ConflictSyntax syntax, Catalog catalog, Meter meter)
    {
        var candidates = new List<ConflictArbiter>();
        if (table.PrimaryKey.Length != 0) candidates.Add(new(true, null));
        candidates.AddRange((table.Indexes ?? []).Where(index => index.Unique).OrderBy(index => index.Predicate is null ? 0 : 1).ThenBy(index => index.Name, Utf8NameComparer.Instance).Select(index => new ConflictArbiter(false, index)));
        if (syntax.Constraint is { } constraint)
        {
            if ((table.Exclusions ?? []).FirstOrDefault(exclusion => exclusion.Name == constraint) is { } exclusion)
            {
                if (syntax.Assignments is not null) throw new SqlException(SqlStates.WrongObjectType, "ON CONFLICT DO UPDATE does not support exclusion constraints");
                return [new(false, table.Indexes!.First(index => index.Name == exclusion.Index), exclusion)];
            }
            var match = candidates.FirstOrDefault(candidate => candidate.Primary ? constraint == GeneratedName(table.Name, "", "pkey", static _ => false) :
                candidate.Index!.IsConstraint && candidate.Index.Name == constraint);
            return match is null ? throw new SqlException(SqlStates.UndefinedObject, "constraint does not exist") : [match];
        }
        if (syntax.Elements is null)
        {
            if (syntax.Assignments is not null) throw Errors.Syntax("DO UPDATE requires a conflict target");
            return candidates.Concat((table.Exclusions ?? []).OrderBy(exclusion => exclusion.Name, Utf8NameComparer.Instance)
                .Select(exclusion => new ConflictArbiter(false, table.Indexes!.First(index => index.Name == exclusion.Index), exclusion))).ToArray();
        }
        var columns = new List<int>(); var decorations = true;
        foreach (var element in syntax.Elements)
        {
            if (element.Ordered) throw new SqlException(SqlStates.InvalidColumnReference, "ordering is not allowed in a conflict target");
            var column = ColumnIndex(table, element.Column); columns.Add(column);
            if (element.Collation is { } collation)
            {
                if (collation != "C") _ = meter.Unicode?.Resolve(collation) ?? throw new SqlException(SqlStates.UndefinedObject, "collation does not exist");
                decorations &= (table.Columns[column].Type.Type == SqlType.Text || table.Columns[column].Type.Type.Element == SqlType.Text) && (table.Columns[column].Collation ?? "C") == collation;
            }
            if (element.Opclass is { } opclass)
            {
                if (!KnownBtreeClass(opclass)) throw new SqlException(SqlStates.UndefinedObject, "operator class does not exist");
                decorations &= (opclass == "varchar_ops" ? "text_ops" : opclass) == BtreeClass(table.Columns[column].Type.Type);
            }
        }
        Resolved? predicate = null;
        if (syntax.Predicate is { } expression)
        {
            ValidateStoredExpression(expression);
            predicate = Resolve.Expr(Resolve.Bind(Resolve.BindTypes(expression, catalog, meter.Temporal), Columns(table)));
        }
        foreach (var candidate in candidates)
        {
            if (!decorations) break;
            var members = candidate.Primary ? table.PrimaryKey : candidate.Index!.Keys.Where(key => key.Expression is null).Select(key => key.Column).ToArray();
            if (!candidate.Primary && candidate.Index!.Keys.Any(key => key.Expression is not null) || !SameColumns(columns.Distinct(), members)) continue;
            if (candidate.Index?.Predicate is { } text)
            {
                if (predicate is null || !PredicateImplies(predicate, Resolve.Expr(Resolve.Bind(Resolve.BindTypes(StoredExpression(text), catalog, meter.Temporal), Columns(table))))) continue;
            }
            return [candidate];
        }
        throw new SqlException(SqlStates.InvalidColumnReference, "no unique key matches the conflict target");
    }
    private static byte[]? ConflictKey(CatalogTable table, ConflictArbiter arbiter, StorageRecord record, SqlValue[] values, Meter meter, Catalog catalog)
    {
        if (arbiter.Primary) return record.Key;
        var indexed = IndexRecord(table, arbiter.Index!, record, values, meter, catalog);
        return indexed is null || indexed.HasNull ? null : indexed.Prefix;
    }
    private static async ValueTask<StorageRecord?> ExistingConflict(Database database, PendingDatabase pending, CatalogTable table, StorageTree source,
        ConflictArbiter arbiter, byte[] prefix, HashSet<string> updated, CancellationToken token)
    {
        if (arbiter.Primary)
        {
            if (updated.Contains(Convert.ToHexString(prefix))) return null;
            return await source.FindAsync(prefix, token).ConfigureAwait(false);
        }
        await foreach (var entry in database.IndexTree(arbiter.Index!, pending, pending.Before).ScanRangeAsync(prefix, true, PrefixEnd(prefix), false, cancellationToken: token).ConfigureAwait(false))
        {
            var key = IndexStorageKey(table, arbiter.Index!, entry.Key, pending.Catalog);
            if (updated.Contains(Convert.ToHexString(key))) continue;
            return await source.FindAsync(key, token).ConfigureAwait(false) ?? throw StorageFormat.Corrupt("conflict index points to missing row");
        }
        return null;
    }
    private static async ValueTask<StatementResult> Upsert(Database database, PendingDatabase pending, CatalogTable table, StorageTree source,
        InsertSyntax insert, int[] slots, Expression[][] input, Meter meter, CancellationToken token, SubqueryBinder inputBinder, StatementContext? context = null)
    {
        var conflict = insert.Conflict!;
        if ((insert.Alias ?? table.Name) == "excluded" && conflict.Assignments is not null &&
            conflict.Assignments.Select(item => item.Value).Concat(conflict.Filter is null ? [] : [conflict.Filter])
                .SelectMany(ExpressionNodes).Any(node => node.Op == "name" && node.Text.StartsWith("excluded.", StringComparison.Ordinal)))
            throw new SqlException(SqlStates.AmbiguousAlias, "excluded target alias is ambiguous");
        var arbiters = ConflictArbiters(table, conflict, pending.Catalog, meter);
        var columns = insert.ViewBinding?.Conflict ?? Columns(table, insert.Alias).Concat(table.Columns.Select(column => new BoundColumn(column.Name, column.Type.Type, "excluded", Hidden: true, Collation: CollationRules.Collatable(column.Type.Type) ? new(Name: column.Collation ?? "C", Table: column.CollationData) : null, WholeRow: false))).ToArray();
        var readState = context?.Read ?? pending.Fork();
        var readTree = database.Tree(table, readState, pending.Before);
        var binder = new SubqueryBinder(columns, database, pending.Before, readState, readState.Catalog, meter, token, context?.Scope);
        var assignments = conflict.Assignments?.Select(assignment => (Slot: ColumnIndex(table, assignment.Column), Value: assignment.Value.Op == "default" ? assignment.Value : binder.Bind(assignment.Value))).ToArray();
        if (assignments is not null)
        {
            if (assignments.Select(assignment => assignment.Slot).Distinct().Count() != assignments.Length) throw new SqlException(SqlStates.DuplicateColumn, "column assigned twice");
            foreach (var assignment in assignments)
            {
                if (assignment.Value.Op == "default") continue;
                if (table.Columns[assignment.Slot].IdentityAlways) throw new SqlException(SqlStates.GeneratedAlways, "explicit identity update");
                Assignable(Resolve.Expr(AssignmentLiteral(Resolve.Bind(assignment.Value, columns), table.Columns[assignment.Slot].Type), table.Columns[assignment.Slot].Type.Type).Type, table.Columns[assignment.Slot].Type.Type);
            }
        }
        var filter = conflict.Filter is null ? null : Resolve.Expr(binder.Bind(conflict.Filter));
        if (filter is not null && filter.Type.Kind is not (SqlKind.Boolean or SqlKind.Unknown)) throw Errors.Type("conflict WHERE requires boolean");
        var returnTable = insert.Alias is { } alias ? table with { Name = alias } : table;
        var returnBinder = new SubqueryBinder(insert.ViewBinding?.Returning ?? ReturningColumns(returnTable, insert.ReturningAliases), database, pending.Before, readState, readState.Catalog, meter, token, context?.Scope);
        var returning = Returning(returnTable, insert.Returning, pending.Catalog, meter, returnBinder, insert.ReturningAliases);
        var checkBinder = new SubqueryBinder(Columns(table), database, pending.Before, readState, readState.Catalog, meter, token, context?.Scope);
        foreach (var check in insert.ViewChecks ?? []) _ = checkBinder.Bind(check);
        await inputBinder.Initialize().ConfigureAwait(false); await binder.Initialize().ConfigureAwait(false);
        await checkBinder.Initialize().ConfigureAwait(false);
        var planned = new List<UpsertRow>(); var updated = new HashSet<string>(StringComparer.Ordinal);
        long rowid = -1;
        if (table.PrimaryKey.Length == 0) await foreach (var record in source.ScanAsync(token).ConfigureAwait(false)) rowid = Math.Max(rowid, KeyEncoding.ReadInteger(record.Key));
        foreach (var expressions in input)
        {
            if (expressions.Length != 0 && expressions.Length != slots.Length) throw Errors.Syntax("INSERT value count differs from target");
            var values = new SqlValue[table.Columns.Length];
            for (var i = 0; i < values.Length; i++)
            {
                var supplied = Array.IndexOf(slots, i); var column = table.Columns[i];
                var useDefault = supplied < 0 || supplied >= expressions.Length || expressions[supplied].Op == "default" || column.Identity && insert.Overriding == "user";
                if (!useDefault && column.IdentityAlways && insert.Overriding != "system") throw new SqlException(SqlStates.GeneratedAlways, "explicit identity value");
                values[i] = useDefault ? InsertDefault(insert, i, column, meter, pending.Catalog) : await InsertValueAsync(expressions[supplied], column.Type, meter, pending.Catalog).ConfigureAwait(false);
            }
            Validate(table, values, meter, pending.Catalog);
            var record = table.EncodeRow(values, checked(rowid + 1));
            StorageRecord? existing = null; var conflictsWithPlanned = false;
            foreach (var arbiter in arbiters)
            {
                if (arbiter.Exclusion is { } exclusion)
                {
                    var conflictKey = await ExclusionConflict(database, readState, table, exclusion, record, values, updated, meter, token).ConfigureAwait(false);
                    if (conflictKey is not null) existing = await readTree.FindAsync(conflictKey, token).ConfigureAwait(false) ?? throw StorageFormat.Corrupt("exclusion index points to missing row");
                    if (existing is not null) break;
                    conflictsWithPlanned = planned.Any(row => ExclusionPair(table, arbiter.Index!, exclusion, record, values, row.Record, row.Values, meter, pending.Catalog));
                    if (conflictsWithPlanned) break;
                    continue;
                }
                var key = ConflictKey(table, arbiter, record, values, meter, pending.Catalog); if (key is null) continue;
                existing = await ExistingConflict(database, readState, table, readTree, arbiter, key, updated, token).ConfigureAwait(false);
                if (existing is not null) break;
                conflictsWithPlanned = planned.Any(row => ConflictKey(table, arbiter, row.Record, row.Values, meter, pending.Catalog) is { } other && key.AsSpan().SequenceEqual(other));
                if (conflictsWithPlanned) break;
            }
            if (existing is null && !conflictsWithPlanned)
            {
                await ValidateViewChecks(insert.ViewChecks, values, meter).ConfigureAwait(false);
                if (table.PrimaryKey.Length == 0) rowid++;
                planned.Add(new(null, record, values, null)); continue;
            }
            if (assignments is null) continue;
            if (conflictsWithPlanned) throw new SqlException(SqlStates.CardinalityViolation, "ON CONFLICT cannot affect the same row twice");
            existing = await readTree.MaterializeRecordAsync(existing!, token).ConfigureAwait(false);
            var before = table.DecodeRow(existing); var scope = before.Concat(values).ToArray();
            if (!await QueryExecution.KeepsAsync(filter, scope, meter).ConfigureAwait(false)) continue;
            var after = before.ToArray();
            foreach (var assignment in assignments.OrderBy(assignment => assignment.Slot))
                after[assignment.Slot] = assignment.Value.Op == "default" ? DefaultValue(table.Columns[assignment.Slot], meter, pending.Catalog) :
                    await AssignAsync(assignment.Value, table.Columns[assignment.Slot].Type, columns, scope, meter, pending.Catalog).ConfigureAwait(false);
            Validate(table, after, meter, pending.Catalog); await ValidateViewChecks(insert.ViewChecks, after, meter).ConfigureAwait(false);
            var encoded = table.EncodeRow(after, table.PrimaryKey.Length == 0 ? KeyEncoding.ReadInteger(existing!.Key) : 0);
            updated.Add(Convert.ToHexString(existing!.Key)); planned.Add(new(existing, encoded, after, null));
        }
        // Phase two preserves the specified physical history: all inserts, then
        // replacements, each retaining its phase-one source order.
        planned = planned.OrderBy(row => row.Old is null ? 0 : 1).ToList();
        await ValidatePinnedKeys(database, context, table, planned.Select(row => (row.Record, row.Values)),
            planned.Where(row => row.Old is not null).Select(row => Convert.ToHexString(row.Old!.Key)).ToHashSet(StringComparer.Ordinal), meter, token).ConfigureAwait(false);
        await ValidateFinalIndexKeys(database, pending, table, planned.Select(row => (row.Record, row.Values)),
            planned.Where(row => row.Old is not null).Select(row => Convert.ToHexString(row.Old!.Key)).ToHashSet(StringComparer.Ordinal), meter, token).ConfigureAwait(false);
        var rekeys = planned.Any(row => row.Old is not null && !row.Old.Key.AsSpan().SequenceEqual(row.Record.Key));
        var priorRecords = new Dictionary<string, StorageRecord>(StringComparer.Ordinal);
        var tree = source;
        var removed = 0;
        var inPlace = new HashSet<string>(StringComparer.Ordinal);
        var newKeys = new HashSet<string>(StringComparer.Ordinal);
        var changedIndexes = new Dictionary<string, CatalogTable>(StringComparer.Ordinal);
        foreach (var row in planned)
            if (!newKeys.Add(Convert.ToHexString(row.Record.Key))) throw new SqlException(SqlStates.UniqueViolation, "duplicate primary key");
        foreach (var row in planned.Where(row => row.Old is not null))
        {
            var currentKey = context?.CurrentKey(table.Name, row.Old!.Key) ?? (context is null ? row.Old!.Key : null);
            var current = currentKey is null ? null : await tree.FindAsync(currentKey, token).ConfigureAwait(false);
            if (current is not null)
            {
                var materialized = await tree.MaterializeRecordAsync(current, token).ConfigureAwait(false);
                if (current.Key.AsSpan().SequenceEqual(row.Record.Key)) inPlace.Add(Convert.ToHexString(row.Old!.Key));
                else tree = await tree.DeleteAsync(current.Key, token).ConfigureAwait(false);
                var oldValues = table.DecodeRow(materialized);
                var indexTable = table with { Indexes = (table.Indexes ?? []).Where(index =>
                {
                    if (index.Kind == 1) return true;
                    var oldEntry = IndexRecord(table, index, materialized, oldValues, meter, pending.Catalog);
                    var newEntry = IndexRecord(table, index, row.Record, row.Values, meter, pending.Catalog);
                    return (oldEntry is null) != (newEntry is null) || oldEntry is not null && newEntry is not null &&
                        !oldEntry.Record.Key.AsSpan().SequenceEqual(newEntry.Record.Key);
                }).ToArray() };
                changedIndexes[Convert.ToHexString(row.Old!.Key)] = indexTable;
                priorRecords[Convert.ToHexString(row.Old!.Key)] = materialized;
                if (rekeys) await RemoveIndexes(database, pending, indexTable, materialized, meter, token, row.Values, row.Record.Key).ConfigureAwait(false);
                removed++;
            }
            context?.Move(table.Name, row.Old!.Key, null);
        }
        foreach (var row in planned)
        {
            if (await tree.FindAsync(row.Record.Key, token).ConfigureAwait(false) is not null &&
                (row.Old is null || !inPlace.Contains(Convert.ToHexString(row.Old.Key)))) throw new SqlException(SqlStates.UniqueViolation, "duplicate primary key");
            tree = await tree.UpsertAsync(tree.PrepareRecord(row.Record, meter), token).ConfigureAwait(false);
            var indexTable = row.Old is null ? table : changedIndexes.GetValueOrDefault(Convert.ToHexString(row.Old.Key), table);
            if (!rekeys && row.Old is not null && priorRecords.TryGetValue(Convert.ToHexString(row.Old.Key), out var prior))
                await RemoveIndexes(database, pending, indexTable, prior, meter, token, row.Values, row.Record.Key).ConfigureAwait(false);
            await AddIndexes(database, pending, indexTable, row.Record, row.Values, meter, token, uniqueValidated: true).ConfigureAwait(false);
            if (row.Old is not null) context?.Move(table.Name, row.Old.Key, row.Record.Key);
        }
        if (planned.Count != 0) { Put(pending, table with { RowCount = checked(table.RowCount - removed + planned.Count) }, tree); StaleStatistics(pending, table.Name); }
        HoldPendingMemory(pending, meter);
        await ValidateExclusions(database, pending, table, planned.Select(row => (row.Record, row.Values)), meter, token, context,
            planned.Where(row => row.Old is not null).Select(row => Convert.ToHexString(row.Old!.Key)).ToHashSet(StringComparer.Ordinal)).ConfigureAwait(false);
        var changes = planned.Where(row => row.Old is not null).Select(row => (row.Old!, row.Values)).ToArray();
        if (changes.Length != 0) await ApplyReferentialActions(database, pending, table, changes, false, meter, token, context: context).ConfigureAwait(false);
        await ForeignCheck(context, () => ValidateForeignKeys(database, pending, table, planned.Select(row => row.Values).ToArray(), token)).ConfigureAwait(false);
        await returnBinder.Initialize().ConfigureAwait(false);
        var rows = new List<Row>();
        if (returning.Length != 0) foreach (var row in planned) rows.Add((await QueryExecution.ProjectAsync(returning, ReturningRow(table, row.Old is null ? null : table.DecodeRow(row.Old), row.Values), meter).ConfigureAwait(false)).RetainMemory(meter));
        return new(returning.Select(projection => new Column(projection.Name, projection.Expression.Type)).ToArray(), rows.ToArray(), planned.Count);
    }
}
