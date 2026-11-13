namespace Jed;

internal static partial class StatementExecution
{
    private static bool ForeignKeyTypes(SqlType local, SqlType parent) => CatalogValidation.ForeignKeyTypes(local, parent);
    private static bool SameColumns(IEnumerable<int> left, IEnumerable<int> right) => left.Order().SequenceEqual(right.Order());
    private static SqlValue? EqualCast(SqlValue value, SqlType target)
    {
        try
        {
            var converted = Resolve.Cast(value, target);
            return value.Type == target || Evaluate.Compare(value, converted) == 0 ? converted : null;
        }
        catch (SqlException error) when (error.SqlState is SqlStates.NumericValueOutOfRange or SqlStates.InvalidTextRepresentation) { return null; }
    }
    private static bool SameForeignKey(SqlValue[] left, int[] leftColumns, SqlValue[] right, int[] rightColumns)
    {
        for (var i = 0; i < leftColumns.Length; i++)
        {
            var a = left[leftColumns[i]]; var b = right[rightColumns[i]];
            if (a.IsNull || b.IsNull || (a.Type.IsNumeric() && b.Type.IsNumeric() ? Evaluate.Compare(a, b) != 0 : !KeyEncoding.Encode(a).AsSpan().SequenceEqual(KeyEncoding.Encode(b)))) return false;
        }
        return true;
    }
    private sealed record DeferredForeignCheck(CatalogTable Parent, string Child, CatalogForeignKey Foreign, SqlValue[][] Rows, byte Action);
    private static async ValueTask ApplyReferentialActions(Database database, PendingDatabase pending, CatalogTable parent,
        IReadOnlyList<(StorageRecord Old, SqlValue[] Values)> changes, bool deleting, Meter meter, CancellationToken token, int depth = 0,
        List<DeferredForeignCheck>? deferred = null)
    {
        if (depth > 128) throw new SqlException(SqlStates.ProgramLimitExceeded, "referential action nesting limit");
        deferred ??= [];
        foreach (var originalChild in pending.Catalog.Tables.OrderBy(table => table.Name, Utf8NameComparer.Instance).ToArray())
        {
            foreach (var foreign in (originalChild.ForeignKeys ?? []).Where(key => key.Parent == parent.Name).OrderBy(key => key.Name, Utf8NameComparer.Instance))
            {
                var action = deleting ? foreign.OnDelete : foreign.OnUpdate;
                var child = Table(pending.Catalog, originalChild.Name);
                var source = database.Tree(child, pending, pending.Before);
                var relevant = changes.Where(change => deleting || !SameForeignKey(parent.DecodeRow(change.Old), foreign.ParentColumns, change.Values, foreign.ParentColumns)).ToArray();
                if (relevant.Length == 0) continue;
                var oldRows = relevant.Select(change => parent.DecodeRow(change.Old)).ToArray();
                if (action <= 1)
                {
                    deferred.Add(new(parent, child.Name, foreign, oldRows, action));
                    continue;
                }
                var replacements = new List<(StorageRecord Old, SqlValue[] Values)>();
                var mapping = new Dictionary<string, SqlValue[]>(StringComparer.Ordinal);
                for (var i = 0; i < relevant.Length; i++)
                    if (ForeignTuple(oldRows[i], foreign.ParentColumns, parent, foreign.ParentColumns) is { } tuple) mapping.TryAdd(tuple, relevant[i].Values);
                await foreach (var record in ForeignTargets(database, pending, child, parent, foreign, oldRows, meter, token).ConfigureAwait(false))
                {
                    meter.Charge(Costs.StorageRowRead); meter.Checkpoint();
                    var materialized = await source.MaterializeRecordAsync(record, token).ConfigureAwait(false);
                    var values = child.DecodeRow(materialized);
                    var tuple = ForeignTuple(values, foreign.Columns, parent, foreign.ParentColumns);
                    if (tuple is null || !mapping.TryGetValue(tuple, out var newParent)) continue;
                    var replacement = values.ToArray();
                    if (!(deleting && action == 2))
                    {
                        foreach (var column in foreign.Columns.Order())
                        {
                            var i = Array.IndexOf(foreign.Columns, column); var declaration = child.Columns[column];
                            var value = action switch
                            {
                                2 => Resolve.Cast(newParent[foreign.ParentColumns[i]], declaration.Type.Type),
                                3 => SqlValue.Null(declaration.Type.Type),
                                4 => DefaultValue(declaration, meter, pending.Catalog),
                                _ => throw StorageFormat.Corrupt("invalid referential action")
                            };
                            if (!value.IsNull && declaration.Type.Type == SqlType.Text && declaration.Type.Length is { } length && value.AsText().EnumerateRunes().Count() > length)
                                throw new SqlException(SqlStates.StringDataRightTruncation, "referential value exceeds varchar length");
                            replacement[column] = Resolve.ApplyModifier(value, declaration.Type);
                        }
                        Validate(child, replacement, meter, pending.Catalog);
                    }
                    replacements.Add((materialized, replacement));
                }
                if (replacements.Count == 0) continue;
                var tree = source; var cascadeDelete = deleting && action == 2;
                foreach (var replacement in replacements)
                {
                    tree = await tree.DeleteAsync(replacement.Old.Key, token).ConfigureAwait(false);
                    await RemoveIndexes(database, pending, child, replacement.Old, meter, token).ConfigureAwait(false);
                }
                if (!cascadeDelete)
                    foreach (var replacement in replacements)
                    {
                        var encoded = tree.PrepareRecord(child.EncodeRow(replacement.Values, child.PrimaryKey.Length == 0 ? KeyEncoding.ReadInteger(replacement.Old.Key) : 0), meter);
                        if (await tree.FindAsync(encoded.Key, token).ConfigureAwait(false) is not null) throw new SqlException(SqlStates.UniqueViolation, "referential action duplicates primary key");
                        tree = await tree.UpsertAsync(encoded, token).ConfigureAwait(false);
                        await AddIndexes(database, pending, child, encoded, replacement.Values, meter, token).ConfigureAwait(false);
                    }
                Put(pending, child with { RowCount = cascadeDelete ? child.RowCount - replacements.Count : child.RowCount }, tree);
                await ApplyReferentialActions(database, pending, child, replacements, cascadeDelete, meter, token, depth + 1, deferred).ConfigureAwait(false);
                if (!cascadeDelete) await ValidateForeignKeys(database, pending, child, replacements.Select(change => change.Values).ToArray(), token).ConfigureAwait(false);
            }
        }
        if (depth != 0) return;
        foreach (var check in deferred.OrderBy(check => check.Child, Utf8NameComparer.Instance).ThenBy(check => check.Foreign.Name, Utf8NameComparer.Instance))
        {
            var seen = new HashSet<string>(StringComparer.Ordinal);
            var child = Table(pending.Catalog, check.Child);
            foreach (var row in check.Rows)
            {
                var tuple = ForeignTuple(row, check.Foreign.ParentColumns, check.Parent, check.Foreign.ParentColumns);
                if (tuple is null || !seen.Add(tuple)) continue;
                if (check.Action == 0 && await ParentContains(database, pending, Table(pending.Catalog, check.Parent.Name), check.Foreign.ParentColumns, row, check.Foreign.ParentColumns, token).ConfigureAwait(false)) continue;
                if (await ChildContains(database, pending, child, check.Parent, check.Foreign, row, meter, token).ConfigureAwait(false))
                    throw new SqlException(SqlStates.ForeignKeyViolation, "referencing row prevents parent change");
            }
        }
    }
}
