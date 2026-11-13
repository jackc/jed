namespace Jed;

internal static partial class StatementExecution
{
    private static async ValueTask<StatementResult> ApplyMerge(Database database, PendingDatabase pending, CatalogTable table,
        List<MergeAction> actions, Projection[] returning, Meter meter, CancellationToken token, StatementContext? context)
    {
        var tree = database.Tree(table, pending, pending.Before);
        var vacated = actions.Where(action => action.Old is not null).Select(action => Convert.ToHexString(action.Old!.Key)).ToHashSet(StringComparer.Ordinal);
        long nextRow = -1;
        if (table.PrimaryKey.Length == 0 && actions.Any(action => action.Old is null))
            await foreach (var record in tree.ScanAsync(token).ConfigureAwait(false)) nextRow = Math.Max(nextRow, KeyEncoding.ReadInteger(record.Key));
        for (var i = 0; i < actions.Count; i++)
        {
            var action = actions[i]; if (action.Values is null) continue;
            var rowId = action.Old is null ? checked(++nextRow) : table.PrimaryKey.Length == 0 ? KeyEncoding.ReadInteger(action.Old.Key) : 0;
            actions[i] = action with { Placed = table.EncodeRow(action.Values, rowId) };
        }
        var placed = actions.Where(action => action.Placed is not null).ToArray();
        var keys = new HashSet<string>(StringComparer.Ordinal);
        foreach (var action in placed)
        {
            var key = Convert.ToHexString(action.Placed!.Key);
            if (!keys.Add(key) || !vacated.Contains(key) && await tree.FindAsync(action.Placed.Key, token).ConfigureAwait(false) is not null)
                throw new SqlException(SqlStates.UniqueViolation, "duplicate MERGE primary key");
        }
        await ValidatePinnedKeys(database, context, table, placed.Select(action => (action.Placed!, action.Values!)), vacated, meter, token).ConfigureAwait(false);
        foreach (var index in (table.Indexes ?? []).Where(index => index.Unique).OrderBy(index => index.Name, Utf8NameComparer.Instance))
        {
            var prefixes = new HashSet<string>(StringComparer.Ordinal); var indexTree = database.IndexTree(index, pending, pending.Before);
            foreach (var action in placed)
            {
                if (IndexRecord(table, index, action.Placed!, action.Values!, meter, pending.Catalog) is not { HasNull: false } entry) continue;
                if (!prefixes.Add(Convert.ToHexString(entry.Prefix))) throw new SqlException(SqlStates.UniqueViolation, "duplicate MERGE unique key");
                await foreach (var existing in indexTree.ScanRangeAsync(entry.Prefix, true, PrefixEnd(entry.Prefix), false, cancellationToken: token).ConfigureAwait(false))
                    if (!vacated.Contains(Convert.ToHexString(IndexStorageKey(table, index, existing.Key, pending.Catalog))))
                        throw new SqlException(SqlStates.UniqueViolation, "duplicate MERGE unique key");
            }
        }
        foreach (var exclusion in (table.Exclusions ?? []).OrderBy(item => item.Name, Utf8NameComparer.Instance))
        {
            var index = table.Indexes!.First(item => item.Name == exclusion.Index);
            for (var i = 0; i < placed.Length; i++)
            {
                var action = placed[i];
                if (await ExclusionConflict(database, pending, table, exclusion, action.Placed!, action.Values!, vacated, meter, token).ConfigureAwait(false) is not null)
                    throw ExclusionError(exclusion);
                for (var j = 0; j < i; j++)
                {
                    meter.Charge(Costs.ConstraintCheck); meter.Checkpoint(token);
                    if (ExclusionPair(table, index, exclusion, action.Placed!, action.Values!, placed[j].Placed!, placed[j].Values!, meter, pending.Catalog)) throw ExclusionError(exclusion);
                }
            }
        }
        // Child checks precede disposition and RETURNING. A writable WITH queues
        // them until every part has staged; otherwise self references see placed
        // rows and ignore the keys this statement vacates.
        if (context is null)
            foreach (var action in placed)
                foreach (var foreign in table.ForeignKeys ?? [])
                {
                    if (action.Old is not null && !action.Clause.Assignments.Any(assignment => foreign.Columns.Contains(assignment.Slot))) continue;
                    var nulls = foreign.Columns.Count(column => action.Values![column].IsNull);
                    if (nulls != 0)
                    {
                        if (foreign.MatchFull && nulls != foreign.Columns.Length) throw new SqlException(SqlStates.ForeignKeyViolation, "MATCH FULL mixes NULL and nonnull columns");
                        continue;
                    }
                    var self = foreign.Parent == table.Name;
                    if (self && placed.Any(parent => SameForeignKey(action.Values!, foreign.Columns, parent.Values!, foreign.ParentColumns))) continue;
                    if (!await ParentContains(database, pending, Table(pending.Catalog, foreign.Parent), foreign.ParentColumns, action.Values!, foreign.Columns,
                        token, self ? vacated : null).ConfigureAwait(false)) throw new SqlException(SqlStates.ForeignKeyViolation, "missing foreign key parent");
                }
        for (var i = 0; i < actions.Count; i++)
        {
            var action = actions[i]; if (action.Placed is null) continue;
            var prepared = tree.PrepareRecord(action.Placed, meter);
            if (action.Old is not null)
            {
                var assigned = action.Clause.Assignments.Select(item => item.Slot).ToHashSet();
                var unchanged = Enumerable.Range(0, table.Columns.Length).Where(slot => !table.PrimaryKey.Contains(slot) && !assigned.Contains(slot)).ToHashSet();
                prepared = tree.KeepUnchangedChains(prepared, action.Old, unchanged);
            }
            actions[i] = action with { Placed = prepared };
        }
        var rows = new List<Row>();
        if (returning.Length != 0)
            foreach (var action in actions)
            {
                var values = action.Source.Concat(ReturningRow(table, action.Original, action.Values))
                    .Append(SqlValue.Text(action.Clause.Syntax.Action.ToUpperInvariant())).ToArray();
                rows.Add((await QueryExecution.ProjectAsync(returning, values, meter).ConfigureAwait(false)).RetainMemory(meter));
            }
        meter.Checkpoint(token);
        foreach (var action in actions)
            if (action.Old is not null && (action.Placed is null || !action.Old.Key.AsSpan().SequenceEqual(action.Placed.Key)))
            {
                var current = context?.CurrentKey(table.Name, action.Old.Key) ?? (context is null ? action.Old.Key : null);
                if (current is not null) tree = await tree.DeleteAsync(current, token).ConfigureAwait(false);
            }
        foreach (var action in actions)
        {
            if (action.Placed is not null) tree = await tree.UpsertAsync(action.Placed, token).ConfigureAwait(false);
            if (action.Old is not null) context?.Move(table.Name, action.Old.Key, action.Placed?.Key);
        }
        foreach (var index in table.Indexes ?? [])
        {
            var oneIndex = table with { Indexes = [index] };
            bool Moves(MergeAction action)
            {
                if (action.Old is null || action.Placed is null || index.Kind == 1) return true;
                var before = IndexRecord(table, index, action.Old, action.Original!, meter, pending.Catalog);
                var after = IndexRecord(table, index, action.Placed, action.Values!, meter, pending.Catalog);
                return (before is null) != (after is null) || before is not null && after is not null && !before.Record.Key.AsSpan().SequenceEqual(after.Record.Key);
            }
            foreach (var action in actions)
                if (action.Old is not null && Moves(action))
                    await RemoveIndexes(database, pending, oneIndex, action.Old, meter, token, action.Values, action.Placed?.Key).ConfigureAwait(false);
            foreach (var action in actions)
                if (action.Placed is not null && Moves(action))
                    await AddIndexes(database, pending, oneIndex, action.Placed, action.Values!, meter, token).ConfigureAwait(false);
        }
        if (actions.Count != 0)
        {
            Put(pending, table with { RowCount = table.RowCount + actions.Count(action => action.Old is null) - actions.Count(action => action.Placed is null) }, tree);
            StaleStatistics(pending, table.Name); HoldPendingMemory(pending, meter);
            var deleted = actions.Where(action => action.Placed is null).Select(action => (table.EncodeRow(action.Original!, table.PrimaryKey.Length == 0 ? KeyEncoding.ReadInteger(action.Old!.Key) : 0), action.Original!)).ToList();
            var updated = actions.Where(action => action.Old is not null && action.Placed is not null).Select(action => (table.EncodeRow(action.Original!, table.PrimaryKey.Length == 0 ? KeyEncoding.ReadInteger(action.Old!.Key) : 0), action.Values!)).ToList();
            await ApplyReferentialActions(database, pending, table, deleted, true, meter, token, context: context).ConfigureAwait(false);
            await ApplyReferentialActions(database, pending, table, updated, false, meter, token, context: context).ConfigureAwait(false);
            if (context is not null) await ForeignCheck(context, () => ValidateForeignKeys(database, pending, table, placed.Select(action => action.Values!).ToArray(), token)).ConfigureAwait(false);
        }
        return new(returning.Select(projection => new Column(projection.Name, projection.Expression.Type)).ToArray(), rows.ToArray(), actions.Count);
    }
}
