using System.Runtime.CompilerServices;

namespace Jed;

internal sealed record QueryStorage(Database Database, DatabaseSnapshot Snapshot, PendingDatabase? Pending, Catalog Catalog, bool SingleRelation = true, bool MembershipBounds = true);
internal sealed record SecondaryAccess(CatalogIndex Index, StorageTree Tree, TableScan Scan, long EstimatedRows, long EstimatedPages, long EstimatedCost, Resolved? Predicate)
{
    internal static SecondaryAccess? Select(CatalogTable table, StorageTree primary, Resolved? filter, QueryStorage context)
    {
        return Candidates(table, primary, filter, context).OrderBy(candidate => candidate.EstimatedCost)
            .FirstOrDefault(candidate => candidate.EstimatedCost <= AccessPlan.Primary(table, primary, filter, context.Catalog).EstimatedCost);
    }
    internal static IEnumerable<SecondaryAccess> Candidates(CatalogTable table, StorageTree primary, Resolved? filter, QueryStorage context)
    {
        if (filter is null) yield break;
        foreach (var index in (table.Indexes ?? []).OrderBy(index => index.Name, Utf8NameComparer.Instance))
        {
            if (!StatementExecution.IndexDependenciesUsable(index, context.Catalog)) continue;
            if (index.Kind != 0 || index.Keys.Length == 0 || index.Keys.Any(key => key.CollationSkew)) continue;
            var keys = StatementExecution.ResolveIndexKeys(table, index, context.Catalog);
            if (keys.Any(key => VersionSkew(key, context.Catalog))) continue;
            var partial = StatementExecution.ResolveIndexPredicate(table, index, context.Catalog);
            if (partial is not null && !Implies(filter, partial)) continue;
            var bound = Bounds(filter, keys, index.Keys);
            if (bound is null && context.SingleRelation && partial is not null && keys.All(key => Steppable(key.Type))) bound = (new TableScan(), null, 0);
            if (bound is null) continue;
            var tree = context.Database.IndexTree(index, context.Pending, context.Snapshot);
            var count = bound.Value.Scan.Empty ? 0 : Estimate.Rows(table, context.Catalog, bound.Value.Predicate);
            var accessPredicate = PartialPredicate(bound.Value.Predicate, partial);
            if (partial is not null && !bound.Value.Scan.Empty) count = Estimate.Rows(table, context.Catalog, accessPredicate);
            if (index.Unique && bound.Value.Equalities == keys.Length) count = Math.Min(count, 1);
            var reads = count == 0 ? 0 : bound.Value.Predicate is null ? tree.NodeCount : index.Unique && bound.Value.Equalities == keys.Length ? tree.Height : Math.Clamp(Estimate.Rows(table, context.Catalog, accessPredicate, tree.NodeCount), tree.Height, tree.NodeCount);
            var cost = Estimate.Add(reads, Estimate.Multiply(count, Estimate.Add(primary.Height + 1L, PredicatePlan.Nodes(filter))));
            yield return new(index, tree, bound.Value.Scan, count, reads, cost, bound.Value.Predicate);
        }
    }

    private static bool VersionSkew(Resolved expression, Catalog catalog) =>
        expression.Collation?.Table is { } data && (catalog.Collations ?? []).FirstOrDefault(pin => pin.Name == data.Name) is { } pin &&
        (pin.UnicodeVersion != data.UnicodeVersion || pin.CldrVersion != data.CldrVersion) || expression.Args.Any(arg => VersionSkew(arg, catalog));

    internal static bool Implies(Resolved filter, Resolved predicate) => IndexImplication.Proves(filter, predicate);
    internal static bool Steppable(SqlType type) => type.Kind is not (SqlKind.Array or SqlKind.Range or SqlKind.Composite or SqlKind.Json or SqlKind.Jsonb or SqlKind.Jsonpath or SqlKind.Unknown);
    internal static Resolved? PartialPredicate(Resolved? access, Resolved? partial)
    {
        if (partial is null) return access;
        var used = access is null ? [] : PredicatePlan.Slots(access);
        return PredicatePlan.Combine((access is null ? Enumerable.Empty<Resolved>() : [access]).Concat(
            PredicatePlan.Conjuncts(partial).Where(term => !PredicatePlan.Slots(term).Overlaps(used))));
    }
    internal static (TableScan Scan, Resolved? Predicate, int Equalities)? Bounds(Resolved filter, Resolved[] keys, CatalogIndexKey[] definitions)
    {
        var prefix = Array.Empty<byte>(); var consumed = new List<Resolved>(); var equalities = 0;
        foreach (var (key, definition) in keys.Zip(definitions))
        {
            byte[]? equal = null, lower = null, upper = null;
            var matched = false; var unknownEquality = false;
            foreach (var term in PredicatePlan.Conjuncts(filter))
            {
                var op = term.Op;
                Resolved operand, value;
                if (op is "is_null" or "is_not_null") { operand = term.Args[0]; value = new("constant", key.Type, SqlValue.Null(key.Type), []); }
                else if (op is "=" or "<" or "<=" or ">" or ">=" && term.Args.Length == 2)
                {
                    operand = term.Args[0]; value = term.Args[1];
                    if (operand.Op == "constant") { (operand, value) = (value, operand); op = Reverse(op); }
                }
                else continue;
                if (value.Op != "constant" || !Resolve.Equivalent(operand, key)) continue;
                if (!AccessConstant.Compatible(value, key.Type)) continue;
                if (CollationRules.Collatable(key.Type) && (term.Collation?.Name ?? "C") != (key.Collation?.Name ?? "C") && op is not ("is_null" or "is_not_null")) continue;
                if (value.ParameterUnbound)
                { matched = true; consumed.Add(term); unknownEquality |= op == "="; continue; }
                if (value.Constant.IsNull && op is not ("is_null" or "is_not_null")) return (new(Empty: true), term, equalities);
                SqlValue converted;
                try
                {
                    if (op is "is_null" or "is_not_null") converted = Resolve.Cast(value.Constant, key.Type);
                    else
                    {
                        if (!AccessConstant.Point(value.Constant, key.Type, out var point) || point is null)
                        { if (op == "=") return (new(Empty: true), term, equalities); continue; }
                        converted = point.Value;
                    }
                }
                catch (SqlException) { continue; }
                var encoded = Slot(converted, definition, key.Collation?.Table);
                matched = true; consumed.Add(term);
                if (op is "=" or "is_null")
                {
                    if (equal is not null && !equal.AsSpan().SequenceEqual(encoded)) return (new(Empty: true), term, equalities);
                    equal = encoded; continue;
                }
                // A comparison's range never includes the NULL slot.
                var present = (byte)(Slot(SqlValue.Null(key.Type), definition, key.Collation?.Table)[0] ^ 1);
                lower = Max(lower, [present]); upper = Min(upper, Successor([present]));
                if (op == "is_not_null") continue;
                if ((definition.Options & 1) != 0) op = Reverse(op);
                if (op == ">") lower = Max(lower, Successor(encoded));
                else if (op == ">=") lower = Max(lower, encoded);
                else if (op == "<") upper = Min(upper, encoded);
                else if (op == "<=") upper = Min(upper, Successor(encoded));
            }
            if (unknownEquality) { equalities++; continue; }
            if (equal is not null)
            {
                if (lower is not null && equal.AsSpan().SequenceCompareTo(lower) < 0 || upper is not null && equal.AsSpan().SequenceCompareTo(upper) >= 0)
                    return (new(Empty: true), PredicatePlan.Combine(consumed), equalities);
                prefix = prefix.Concat(equal).ToArray(); equalities++; continue;
            }
            if (!matched && prefix.Length == 0) return null;
            var low = lower is null ? prefix.Length == 0 ? null : prefix : prefix.Concat(lower).ToArray();
            var high = upper is null ? Successor(prefix) : prefix.Concat(upper).ToArray();
            return (new(low, true, high, false, Empty: low is not null && high is not null && low.AsSpan().SequenceCompareTo(high) >= 0), PredicatePlan.Combine(consumed), equalities);
        }
        return (new(prefix, true, Successor(prefix), false), PredicatePlan.Combine(consumed), equalities);
    }
    private static string Reverse(string op) => op switch { "<" => ">", "<=" => ">=", ">" => "<", ">=" => "<=", _ => op };
    private static byte[] Slot(SqlValue value, CatalogIndexKey definition, Collation? collation)
    {
        var descending = (definition.Options & 1) != 0;
        var bytes = KeyEncoding.Encode(value, true, descending, collation: collation);
        if (((definition.Options & 2) != 0) != descending) bytes[0] ^= 1;
        return bytes;
    }
    private static byte[]? Max(byte[]? a, byte[]? b) => a is null ? b : b is null ? a : a.AsSpan().SequenceCompareTo(b) >= 0 ? a : b;
    private static byte[]? Min(byte[]? a, byte[]? b) => a is null ? b : b is null ? a : a.AsSpan().SequenceCompareTo(b) <= 0 ? a : b;
    internal static byte[]? Successor(byte[] prefix)
    {
        for (var i = prefix.Length - 1; i >= 0; i--)
            if (prefix[i] != 255) { var next = prefix[..(i + 1)]; next[i]++; return next; }
        return null;
    }
    internal async IAsyncEnumerable<StorageRecord> OrderedRecords(CatalogTable table, StorageTree primary, Catalog catalog, Meter meter,
        IReadOnlySet<int>? touched, [EnumeratorCancellation] CancellationToken token)
    {
        if (Scan.Empty) yield break;
        meter.Charge(Tree.CountRangeNodes(Scan.Lower, Scan.LowerInclusive, Scan.Upper, Scan.UpperInclusive)); meter.Checkpoint(token);
        await foreach (var entry in Tree.ScanRangeAsync(Scan.Lower, Scan.LowerInclusive, Scan.Upper, Scan.UpperInclusive, cancellationToken: token).ConfigureAwait(false))
        {
            var key = StatementExecution.IndexStorageKey(table, Index, entry.Key, catalog);
            meter.Charge(primary.Height); meter.Checkpoint(token);
            var record = await primary.FindAsync(key, token).ConfigureAwait(false) ?? throw StorageFormat.Corrupt("secondary index names a missing row");
            meter.Charge(primary.ValueReadUnits(record, touched)); meter.Charge(Costs.StorageRowRead); meter.Checkpoint(token);
            yield return record;
        }
    }
    internal async IAsyncEnumerable<SqlValue[]> Rows(CatalogTable table, StorageTree primary, Catalog catalog, Meter meter,
        IReadOnlySet<int>? touched, [EnumeratorCancellation] CancellationToken token, bool reverse = false)
    {
        await foreach (var record in Records(table, primary, catalog, meter, touched, token, reverse).ConfigureAwait(false))
            yield return primary.DecodeDeferredRow(table, record);
    }
    internal async IAsyncEnumerable<StorageRecord> Records(CatalogTable table, StorageTree primary, Catalog catalog, Meter meter,
        IReadOnlySet<int>? touched, [EnumeratorCancellation] CancellationToken token, bool reverse = false)
    {
        if (Scan.Empty) yield break;
        meter.Charge(Tree.CountRangeNodes(Scan.Lower, true, Scan.Upper, false)); meter.Checkpoint(token);
        var records = new SortedDictionary<byte[], StorageRecord>(Comparer<byte[]>.Create((a, b) => a.AsSpan().SequenceCompareTo(b)));
        await foreach (var entry in Tree.ScanRangeAsync(Scan.Lower, true, Scan.Upper, false, cancellationToken: token).ConfigureAwait(false))
        {
            var key = StatementExecution.IndexStorageKey(table, Index, entry.Key, catalog);
            meter.Charge(primary.Height); meter.Checkpoint(token);
            var record = await primary.FindAsync(key, token).ConfigureAwait(false) ?? throw StorageFormat.Corrupt("secondary index names a missing row");
            meter.Charge(primary.ValueReadUnits(record, touched)); records.TryAdd(key, record);
        }
        foreach (var record in reverse ? records.Values.Reverse() : records.Values)
        {
            meter.Charge(Costs.StorageRowRead); meter.Checkpoint(token);
            yield return record;
        }
    }
}

internal static partial class StatementExecution
{
    internal static Resolved[] ResolveIndexKeys(CatalogTable table, CatalogIndex index, Catalog catalog) => index.Keys.Select(key => key.Expression is { } sql
        ? Resolve.Expr(Resolve.Bind(Resolve.BindTypes(StoredExpression(sql), catalog, new TemporalContext(static () => throw new InvalidOperationException("index key reads clock"), "UTC", data: catalog.BoundTimeZones, unicode: catalog.BoundUnicode)), Columns(table)))
        : new Resolved("column", table.Columns[key.Column].Type.Type, default, [], Slot: key.Column, Collation: CollationRules.Collatable(table.Columns[key.Column].Type.Type) ? new(table.Columns[key.Column].Collation ?? "C", Table: table.Columns[key.Column].CollationData) : null)).ToArray();
    internal static Resolved? ResolveIndexPredicate(CatalogTable table, CatalogIndex index, Catalog catalog) => index.Predicate is { } sql
        ? Resolve.Expr(Resolve.Bind(Resolve.BindTypes(StoredExpression(sql), catalog, new TemporalContext(static () => throw new InvalidOperationException("index predicate reads clock"), "UTC", data: catalog.BoundTimeZones, unicode: catalog.BoundUnicode)), Columns(table))) : null;
}
