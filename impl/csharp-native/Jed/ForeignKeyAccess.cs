using System.Runtime.CompilerServices;

namespace Jed;

internal static partial class StatementExecution
{
    private sealed record ForeignAccess(StorageTree Tree, CatalogIndex? Index, int[] Columns);
    private static ForeignAccess? ChildAccess(Database database, PendingDatabase pending, CatalogTable child, CatalogTable parent, CatalogForeignKey foreign)
    {
        foreach (var ordinal in foreign.Columns)
        {
            var paired = foreign.ParentColumns[Array.IndexOf(foreign.Columns, ordinal)];
            if (child.Columns[ordinal].Type.Type == SqlType.Text &&
                (child.Columns[ordinal].Collation ?? "C") != (parent.Columns[paired].Collation ?? "C")) return null;
        }
        var width = foreign.Columns.Length;
        if (child.PrimaryKey.Length >= width && SameColumns(child.PrimaryKey.Take(width), foreign.Columns))
            return new(database.Tree(child, pending, pending.Before), null, child.PrimaryKey[..width]);
        foreach (var index in (child.Indexes ?? []).OrderBy(index => index.Name, Utf8NameComparer.Instance))
        {
            if (index.Kind != 0 || index.Predicate is not null || index.Keys.Length < width ||
                index.Keys.Any(key => key.Expression is not null) || !SameColumns(index.Keys.Take(width).Select(key => key.Column), foreign.Columns) ||
                index.Keys.Skip(width).Any(key => StoredValues.Width(new(child.Columns[key.Column].Type.Type)) == 0)) continue;
            return new(database.IndexTree(index, pending, pending.Before), index, index.Keys.Take(width).Select(key => key.Column).ToArray());
        }
        return null;
    }
    private static byte[]? ForeignPrefix(ForeignAccess access, CatalogTable child, CatalogForeignKey foreign, SqlValue[] parentRow)
    {
        var writer = new WireWriter();
        for (var i = 0; i < access.Columns.Length; i++)
        {
            var column = access.Columns[i]; var value = parentRow[foreign.ParentColumns[Array.IndexOf(foreign.Columns, column)]];
            if (value.IsNull || EqualCast(value, child.Columns[column].Type.Type) is not { } converted) return null;
            if (access.Index is null) writer.Bytes(KeyEncoding.Encode(converted));
            else
            {
                var options = access.Index.Keys[i].Options; var descending = (options & 1) != 0;
                var encoded = KeyEncoding.Encode(converted, true, descending);
                if (((options & 2) != 0) != descending) encoded[0] ^= 1;
                writer.Bytes(encoded);
            }
        }
        return writer.ToArray();
    }
    private static string? ForeignTuple(SqlValue[] values, int[] columns, CatalogTable parent, int[] parentColumns)
    {
        var writer = new WireWriter();
        for (var i = 0; i < columns.Length; i++)
        {
            if (values[columns[i]].IsNull || EqualCast(values[columns[i]], parent.Columns[parentColumns[i]].Type.Type) is not { } value) return null;
            writer.Bytes(KeyEncoding.Encode(value));
        }
        return Convert.ToHexString(writer.ToArray());
    }
    private static async ValueTask<bool> ChildContains(Database database, PendingDatabase pending, CatalogTable child, CatalogTable parent,
        CatalogForeignKey foreign, SqlValue[] parentRow, Meter meter, CancellationToken token)
    {
        var access = ChildAccess(database, pending, child, parent, foreign);
        if (access is not null)
        {
            var prefix = ForeignPrefix(access, child, foreign, parentRow); if (prefix is null) return false;
            var end = PrefixEnd(prefix);
            meter.Charge(await access.Tree.CountRangePagesAsync(prefix, true, end, false, token).ConfigureAwait(false)); meter.Checkpoint();
            await foreach (var record in access.Tree.ScanRangeAsync(prefix, true, end, false, cancellationToken: token).ConfigureAwait(false)) return true;
            return false;
        }
        var tree = database.Tree(child, pending, pending.Before);
        meter.Charge(await tree.CountPagesAsync(token).ConfigureAwait(false)); meter.Checkpoint();
        await foreach (var record in tree.ScanAsync(token).ConfigureAwait(false))
        {
            meter.Charge(Costs.StorageRowRead); meter.Checkpoint();
            if (SameForeignKey(child.DecodeRow(await tree.MaterializeRecordAsync(record, token).ConfigureAwait(false)), foreign.Columns, parentRow, foreign.ParentColumns)) return true;
        }
        return false;
    }
    private static async IAsyncEnumerable<StorageRecord> ForeignTargets(Database database, PendingDatabase pending, CatalogTable child,
        CatalogTable parent, CatalogForeignKey foreign, IEnumerable<SqlValue[]> parentRows, Meter meter, [EnumeratorCancellation] CancellationToken token)
    {
        var source = database.Tree(child, pending, pending.Before);
        var access = ChildAccess(database, pending, child, parent, foreign);
        if (access is null)
        {
            meter.Charge(await source.CountPagesAsync(token).ConfigureAwait(false)); meter.Checkpoint();
            await foreach (var record in source.ScanAsync(token).ConfigureAwait(false)) yield return record;
            yield break;
        }
        var records = new SortedDictionary<byte[], StorageRecord>(Comparer<byte[]>.Create((left, right) => left.AsSpan().SequenceCompareTo(right)));
        var prefixes = new HashSet<string>(StringComparer.Ordinal);
        foreach (var row in parentRows)
        {
            var prefix = ForeignPrefix(access, child, foreign, row);
            if (prefix is null || !prefixes.Add(Convert.ToHexString(prefix))) continue;
            var end = PrefixEnd(prefix);
            meter.Charge(await access.Tree.CountRangePagesAsync(prefix, true, end, false, token).ConfigureAwait(false)); meter.Checkpoint();
            await foreach (var entry in access.Tree.ScanRangeAsync(prefix, true, end, false, cancellationToken: token).ConfigureAwait(false))
            {
                var record = entry;
                if (access.Index is not null)
                {
                    var key = IndexStorageKey(child, access.Index, entry.Key, pending.Catalog);
                    meter.Charge(await source.CountRangePagesAsync(key, true, key, true, token).ConfigureAwait(false)); meter.Checkpoint();
                    record = await source.FindAsync(key, token).ConfigureAwait(false) ?? throw StorageFormat.Corrupt("secondary index row is absent");
                }
                records.TryAdd(record.Key, record);
            }
        }
        foreach (var record in records.Values) yield return record;
    }
}
