namespace Jed;

internal static partial class StatementExecution
{
    private sealed record IndexedRecord(StorageRecord Record, byte[] Prefix, bool HasNull);
    private static void Keyable(SqlType type)
    {
        if (type.Kind is SqlKind.Unknown or SqlKind.Json or SqlKind.Jsonb or SqlKind.Jsonpath)
            throw Errors.Unsupported($"index key type {type.Name()}");
        if (type.Kind == SqlKind.Array)
        {
            if (type.Element!.Kind == SqlKind.Composite) throw Errors.Unsupported("array-of-composite key");
            Keyable(type.Element!);
        }
        if (type.Kind == SqlKind.Composite) foreach (var field in type.Fields) Keyable(field.Type);
    }
    private static void ValidateIndexDependencies(Resolved expression)
    {
        if (expression.Op == "row" && expression.Text == "whole_row") throw Errors.Unsupported("whole-row reference in stored index expression");
        ValidateTemporalDependencies(expression);
        if (expression.Op == "stable_date") throw new SqlException(SqlStates.InvalidObjectDefinition, "index expression depends on statement clock");
        static bool Zoned(SqlType type) => type == SqlType.Timestamptz || type.Element is { } element && Zoned(element) || type.Fields.Any(field => Zoned(field.Type));
        if (expression.Op == "call" && expression.Text is "to_json" or "to_jsonb" or "array_to_json" or "row_to_json" or "json_scalar" or "json_build_array" or "jsonb_build_array" or "json_build_object" or "jsonb_build_object" && expression.Args.Any(argument => Zoned(argument.Type)))
            throw new SqlException(SqlStates.InvalidObjectDefinition, "JSON conversion depends on session time zone");
        if (expression.Op == "call" && expression.Text.StartsWith("jsonb_path_", StringComparison.Ordinal) && expression.Text.EndsWith("_tz", StringComparison.Ordinal))
            throw new SqlException(SqlStates.InvalidObjectDefinition, "JSON path conversion depends on session time zone");
        if (expression.Op == "call" && expression.Text is "concat" or "concat_ws" or "format" && expression.Args.Any(argument => Zoned(argument.Type)))
            throw new SqlException(SqlStates.InvalidObjectDefinition, "text formatting depends on session time zone");
        if (expression.Op == "call" && (expression.Text == "age" && (expression.Args.Length == 1 || expression.Args.Any(argument => argument.Type == SqlType.Timestamptz)) ||
            expression.Text == "to_char" && expression.Args.Length > 0 && expression.Args[0].Type == SqlType.Timestamptz ||
            expression.Text == "to_timestamp" && expression.Args.Length == 2 && expression.Args[0].Type == SqlType.Text))
            throw new SqlException(SqlStates.InvalidObjectDefinition, "index expression depends on session time");
        if (expression.Op == "cast" && expression.Type == SqlType.Text && expression.Args.Any(argument => Zoned(argument.Type)))
            throw new SqlException(SqlStates.InvalidObjectDefinition, "text conversion depends on session time zone");
        if (expression.Op == "cast" && expression.Args.Length == 1 && expression.Args[0].Type == SqlType.Text && expression.Type is { Kind: SqlKind.Date or SqlKind.Timestamp or SqlKind.Timestamptz or SqlKind.Interval })
            throw new SqlException(SqlStates.InvalidObjectDefinition, "index cast depends on session time");
        if (expression.Op is "+" or "-" && expression.Args.Any(argument => argument.Type == SqlType.Timestamptz) && expression.Args.FirstOrDefault(argument => argument.Type == SqlType.Interval) is { } interval &&
            (interval.Op != "constant" || !interval.Constant.IsNull && (interval.Constant.AsInterval().Months != 0 || interval.Constant.AsInterval().Days != 0)))
            throw new SqlException(SqlStates.InvalidObjectDefinition, "calendar timestamp arithmetic depends on session time zone");
        foreach (var argument in expression.Args) ValidateIndexDependencies(argument);
    }
    private static void PinExpressionCollations(PendingDatabase pending, Resolved expression, Meter meter)
    {
        if (expression.Collation?.Name is { } name) _ = ReferenceCollation(pending, name, meter.Unicode);
        foreach (var argument in expression.Args) PinExpressionCollations(pending, argument, meter);
    }
    private static byte[]? PrefixEnd(byte[] prefix)
    {
        for (var i = prefix.Length - 1; i >= 0; i--)
            if (prefix[i] != 255) { var end = prefix[..(i + 1)]; end[i]++; return end; }
        return null;
    }
    internal static byte[] IndexStorageKey(CatalogTable table, CatalogIndex index, ReadOnlySpan<byte> entry, Catalog catalog)
    {
        var position = 0;
        foreach (var key in index.Keys)
        {
            if (position >= entry.Length) throw StorageFormat.Corrupt("truncated secondary key");
            var descending = (key.Options & 1) != 0;
            var presence = (byte)(entry[position++] ^ (descending ? 255 : 0));
            if (((key.Options & 2) != 0) != descending) presence ^= 1;
            if (presence == 1) continue;
            if (presence != 0) throw StorageFormat.Corrupt("invalid secondary key presence tag");
            var resolved = key.Expression is { } expression ? Resolve.Expr(Resolve.Bind(Resolve.BindTypes(StoredExpression(expression), catalog, new TemporalContext(static () => throw new InvalidOperationException("stored index cannot read clock"), "UTC", data: catalog.BoundTimeZones, unicode: catalog.BoundUnicode)), Columns(table))) : null;
            var type = resolved?.Type ?? table.Columns[key.Column].Type.Type;
            position += KeyEncoding.KeyLength(entry[position..], type, descending: descending, collation: resolved?.Collation?.Table ?? key.CollationData ?? (key.Expression is null ? table.Columns[key.Column].CollationData : null));
        }
        if (position >= entry.Length) throw StorageFormat.Corrupt("missing secondary row key");
        return entry[position..].ToArray();
    }
    private static IndexedRecord? IndexRecord(CatalogTable table, CatalogIndex index, StorageRecord record, SqlValue[] values, Meter meter, Catalog catalog)
    {
        if (index.Kind == 2) return GistRecord(table, index, record, values, meter, catalog);
        if (index.Kind != 0) throw Errors.Unsupported("non-B-tree index maintenance");
        using var unmetered = meter.SuppressCost();
        if (index.Predicate is { } predicate && !QueryExecution.Keeps(Resolve.Expr(Resolve.Bind(Resolve.BindTypes(StoredExpression(predicate), catalog, meter.Temporal), Columns(table))), values, meter)) return null;
        var writer = new WireWriter(); var hasNull = false;
        foreach (var key in index.Keys)
        {
            var resolved = key.Expression is { } expression ? Resolve.Expr(Resolve.Bind(Resolve.BindTypes(StoredExpression(expression), catalog, meter.Temporal), Columns(table))) : null;
            var value = resolved is null ? values[key.Column] : Evaluate.Expr(resolved, meter, row: values);
            var collation = resolved?.Collation?.Table ?? (key.Expression is null ? table.Columns[key.Column].CollationData : key.CollationData);
            var descending = (key.Options & 1) != 0; var nullsFirst = (key.Options & 2) != 0;
            var encoded = KeyEncoding.Encode(value, nullable: true, descending: descending, collation: collation);
            if (nullsFirst != descending) encoded[0] ^= 1;
            writer.Bytes(encoded); hasNull |= value.IsNull;
        }
        var prefix = writer.ToArray(); writer.Bytes(record.Key);
        return new(new(writer.ToArray(), []), prefix, hasNull);
    }
    // Writable WITH reads uniqueness from its pre-statement pin as well as
    // checking accumulated writes. Rows replaced by this same part are excluded
    // so multi-row key swaps still validate against the part's final image.
    private static async ValueTask ValidatePinnedKeys(Database database, StatementContext? context, CatalogTable table,
        IEnumerable<(StorageRecord Record, SqlValue[] Values)> records, ISet<string> replaced, Meter meter, CancellationToken token)
    {
        if (context is null) return;
        var read = context.Read; var tree = database.Tree(table, read, read.Before);
        foreach (var (record, values) in records)
        {
            if (table.PrimaryKey.Length != 0 && !replaced.Contains(Convert.ToHexString(record.Key)) && await tree.FindAsync(record.Key, token).ConfigureAwait(false) is not null)
                throw new SqlException(SqlStates.UniqueViolation, "duplicate primary key in statement snapshot");
            foreach (var index in table.Indexes ?? [])
            {
                if (!index.Unique || IndexRecord(table, index, record, values, meter, read.Catalog) is not { HasNull: false } entry) continue;
                var pinned = database.IndexTree(index, read, read.Before);
                await foreach (var existing in pinned.ScanRangeAsync(entry.Prefix, true, PrefixEnd(entry.Prefix), false, cancellationToken: token).ConfigureAwait(false))
                    if (!replaced.Contains(Convert.ToHexString(IndexStorageKey(table, index, existing.Key, read.Catalog))))
                        throw new SqlException(SqlStates.UniqueViolation, "duplicate unique key in statement snapshot");
            }
        }
    }
    private static async ValueTask<StorageTree> AddIndexRecord(StorageTree tree, CatalogIndex index, IndexedRecord entry, CancellationToken token)
    {
        if (index.Unique && !entry.HasNull)
            await foreach (var existing in tree.ScanRangeAsync(entry.Prefix, true, PrefixEnd(entry.Prefix), false, cancellationToken: token).ConfigureAwait(false))
                if (!existing.Key.AsSpan().SequenceEqual(entry.Record.Key)) throw new SqlException(SqlStates.UniqueViolation, $"duplicate unique key: {index.Name}");
        return await tree.UpsertAsync(entry.Record, token).ConfigureAwait(false);
    }
    private static async ValueTask AddIndexes(Database database, PendingDatabase pending, CatalogTable table, StorageRecord record, SqlValue[] values, Meter meter, CancellationToken token)
    {
        foreach (var index in table.Indexes ?? [])
        {
            if (index.Kind == 1) { await AddGin(database, pending, table, index, record, values, meter, token).ConfigureAwait(false); continue; }
            if (IndexRecord(table, index, record, values, meter, pending.Catalog) is not { } entry) continue;
            var tree = database.IndexTree(index, pending, pending.Before);
            pending.IndexTrees[index.Name] = await AddIndexRecord(tree, index, entry, token).ConfigureAwait(false);
        }
    }
    private static async ValueTask RemoveIndexes(Database database, PendingDatabase pending, CatalogTable table, StorageRecord record, Meter meter, CancellationToken token, SqlValue[]? replacementValues = null, byte[]? replacementKey = null)
    {
        if ((table.Indexes?.Length ?? 0) == 0) return;
        var values = table.DecodeRow(await database.Tree(table, pending, pending.Before).MaterializeRecordAsync(record, token).ConfigureAwait(false));
        foreach (var index in table.Indexes ?? [])
        {
            if (index.Kind == 1) { await RemoveGin(database, pending, table, index, record, values, meter, token, replacementValues, replacementKey).ConfigureAwait(false); continue; }
            if (IndexRecord(table, index, record, values, meter, pending.Catalog) is not { } entry) continue;
            var tree = database.IndexTree(index, pending, pending.Before);
            pending.IndexTrees[index.Name] = await tree.DeleteAsync(entry.Record.Key, token).ConfigureAwait(false);
        }
    }
    private static async ValueTask BuildIndex(Database database, PendingDatabase pending, CatalogTable table, CatalogIndex index, Meter meter, CancellationToken token, bool chargeScan = true)
    {
        if (index.Kind == 1) { await BuildGin(database, pending, table, index, meter, token, chargeScan).ConfigureAwait(false); return; }
        var source = database.Tree(table, pending, pending.Before);
        if (chargeScan) { meter.Charge(await source.CountPagesAsync(token).ConfigureAwait(false)); meter.Checkpoint(); }
        var records = new List<IndexedRecord>();
        await foreach (var record in source.ScanAsync(token).ConfigureAwait(false))
        {
            if (chargeScan) meter.Charge(Costs.StorageRowRead); meter.Checkpoint();
            if (IndexRecord(table, index, record, table.DecodeRow(await source.MaterializeRecordAsync(record, token).ConfigureAwait(false)), meter, pending.Catalog) is { } entry) records.Add(entry);
        }
        records.Sort((left, right) => left.Record.Key.AsSpan().SequenceCompareTo(right.Record.Key));
        var tree = database.IndexTree(index with { Root = 0, RootTag = default, Shape = null, Gist = null }, null, pending.Before);
        foreach (var entry in records) tree = await AddIndexRecord(tree, index, entry, token).ConfigureAwait(false);
        pending.IndexTrees[index.Name] = tree;
    }
    private static async ValueTask<StatementResult> CreateIndex(Database database, PendingDatabase pending, CreateIndexSyntax create, Meter meter, CancellationToken token)
    {
        var table = Table(pending.Catalog, create.Table);
        if (create.Method is not ("btree" or "gin" or "gist")) throw new SqlException(SqlStates.UndefinedObject, $"access method does not exist: {create.Method}");
        WritableCollations(table, pending.Catalog);
        if (create.Method == "gin") return await CreateGin(database, pending, table, create, meter, token).ConfigureAwait(false);
        if (create.Method == "gist") return await CreateGist(database, pending, table, create, meter, token).ConfigureAwait(false);
        var keys = new List<CatalogIndexKey>(); var parts = new List<string>(); var dependencies = new List<Resolved>();
        foreach (var key in create.Keys)
        {
            ValidateStoredExpression(key.Expression, immutable: true);
            var resolved = Resolve.Expr(Resolve.Bind(Resolve.BindTypes(key.Expression, pending.Catalog, meter.Temporal), Columns(table))); ValidateIndexDependencies(resolved); Keyable(resolved.Type); PinExpressionCollations(pending, resolved, meter); dependencies.Add(resolved);
            if (resolved.Op == "column") { keys.Add(new(resolved.Slot, Options: key.Options) { CollationData = resolved.Collation?.Table }); parts.Add(table.Columns[resolved.Slot].Name); }
            else { keys.Add(new(-1, key.Text, key.Options) { CollationData = resolved.Collation?.Table }); parts.Add(key.Expression.Op == "call" ? key.Expression.Text : "expr"); }
        }
        if (create.Predicate is { } predicate)
        {
            ValidateStoredExpression(predicate, immutable: true); var resolved = Resolve.Expr(Resolve.Bind(Resolve.BindTypes(predicate, pending.Catalog, meter.Temporal), Columns(table)));
            if (resolved.Type.Kind is not (SqlKind.Boolean or SqlKind.Unknown)) throw Errors.Type("index predicate requires boolean");
            ValidateIndexDependencies(resolved); PinExpressionCollations(pending, resolved, meter); dependencies.Add(resolved);
        }
        if (create.Method != "btree") throw Errors.Unsupported($"index access method {create.Method}");
        if (create.Name is { } explicitName && RelationExists(pending.Catalog, explicitName))
        {
            if (create.IfNotExists) return new([], []);
            throw new SqlException(SqlStates.DuplicateTable, "relation already exists");
        }
        var used = new HashSet<string>(StringComparer.Ordinal);
        for (var i = 0; i < parts.Count; i++)
        {
            var original = parts[i]; var suffix = 0;
            while (!used.Add(parts[i])) parts[i] = original + (++suffix).ToString(System.Globalization.CultureInfo.InvariantCulture);
        }
        var name = create.Name ?? GeneratedName(table.Name, string.Join('_', parts), "idx", candidate => RelationExists(pending.Catalog, candidate));
        var index = PinIndexDependencies(new CatalogIndex(name, keys.ToArray(), Unique: create.Unique, Predicate: create.PredicateText), dependencies, meter);
        await BuildIndex(database, pending, table, index, meter, token).ConfigureAwait(false);
        Put(pending, table with { Indexes = (table.Indexes ?? []).Append(index).OrderBy(i => i.Name, Utf8NameComparer.Instance).ToArray() });
        return new([], []);
    }
    private static async ValueTask<StatementResult> DropIndex(Database database, PendingDatabase pending, DropIndexSyntax drop, CancellationToken token, bool constraint = false)
    {
        var table = pending.Catalog.Tables.FirstOrDefault(table => (table.Indexes ?? []).Any(index => index.Name == drop.Name));
        if (table is null)
        {
            if (drop.IfExists) return new([], []);
            throw new SqlException(SqlStates.UndefinedObject, "index does not exist");
        }
        var index = table.Indexes!.Single(index => index.Name == drop.Name);
        if ((index.IsConstraint || (table.Exclusions ?? []).Any(exclusion => exclusion.Index == index.Name)) && !constraint) throw new SqlException(SqlStates.DependentObjectsStillExist, "constraint owns this index");
        var remaining = (table.Indexes ?? []).Where(item => item.Name != index.Name).ToArray();
        foreach (var child in pending.Catalog.Tables.ToArray())
        {
            var lost = (child.ForeignKeys ?? []).Where(foreign => foreign.Parent == table.Name && !SameColumns(foreign.ParentColumns, table.PrimaryKey) &&
                !remaining.Any(other => other.Unique && other.Predicate is null && other.Keys.All(key => key.Expression is null) && SameColumns(foreign.ParentColumns, other.Keys.Select(key => key.Column)))).ToArray();
            if (lost.Length == 0) continue;
            if (!drop.Cascade) throw new SqlException(SqlStates.DependentObjectsStillExist, "foreign key requires unique index");
            Put(pending, child with { ForeignKeys = (child.ForeignKeys ?? []).Except(lost).ToArray() });
        }
        var cleared = await database.IndexTree(index, pending, pending.Before).ClearAsync(token).ConfigureAwait(false);
        pending.RetiredPages.UnionWith(cleared.RetiredPages); if (index.Gist is { } gist) pending.RetiredPages.UnionWith(gist.Pages); pending.IndexTrees.Remove(index.Name);
        Put(pending, Table(pending.Catalog, table.Name) with { Indexes = remaining });
        return new([], []);
    }

    private static async ValueTask<bool> ParentContains(Database database, PendingDatabase pending, CatalogTable parent, int[] columns, SqlValue[] values, int[] valueColumns, CancellationToken token)
    {
        var converted = new Dictionary<int, SqlValue>();
        for (var i = 0; i < columns.Length; i++)
        {
            if (EqualCast(values[valueColumns[i]], parent.Columns[columns[i]].Type.Type) is not { } value) return false;
            converted.Add(columns[i], value);
        }
        if (SameColumns(columns, parent.PrimaryKey))
        {
            var writer = new WireWriter(); foreach (var column in parent.PrimaryKey) writer.Bytes(KeyEncoding.Encode(converted[column]));
            return await database.Tree(parent, pending, pending.Before).FindAsync(writer.ToArray(), token).ConfigureAwait(false) is not null;
        }
        var index = (parent.Indexes ?? []).FirstOrDefault(index => index.Unique && index.Predicate is null && index.Keys.All(key => key.Expression is null) && SameColumns(columns, index.Keys.Select(key => key.Column)));
        if (index is null) throw StorageFormat.Corrupt("foreign key has no unique covering key");
        var prefix = new WireWriter();
        foreach (var key in index.Keys)
        {
            var descending = (key.Options & 1) != 0; var encoded = KeyEncoding.Encode(converted[key.Column], true, descending);
            if (((key.Options & 2) != 0) != descending) encoded[0] ^= 1;
            prefix.Bytes(encoded);
        }
        var start = prefix.ToArray();
        await foreach (var record in database.IndexTree(index, pending, pending.Before).ScanRangeAsync(start, true, PrefixEnd(start), false, cancellationToken: token).ConfigureAwait(false)) return true;
        return false;
    }
    private static async ValueTask ValidateForeignKeys(Database database, PendingDatabase pending, CatalogTable table, IReadOnlyList<SqlValue[]> rows, CancellationToken token)
    {
        foreach (var values in rows)
            foreach (var foreign in table.ForeignKeys ?? [])
            {
                var nulls = foreign.Columns.Count(column => values[column].IsNull);
                if (nulls != 0)
                {
                    if (foreign.MatchFull && nulls != foreign.Columns.Length) throw new SqlException(SqlStates.ForeignKeyViolation, "MATCH FULL mixes NULL and nonnull columns");
                    continue;
                }
                var parent = Table(pending.Catalog, foreign.Parent);
                if (!await ParentContains(database, pending, parent, foreign.ParentColumns, values, foreign.Columns, token).ConfigureAwait(false))
                    throw new SqlException(SqlStates.ForeignKeyViolation, "missing foreign key parent");
            }
    }
}
