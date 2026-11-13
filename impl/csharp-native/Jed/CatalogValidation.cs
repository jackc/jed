namespace Jed;

internal static class CatalogValidation
{
    internal static bool ForeignKeyTypes(SqlType local, SqlType parent) => local == parent || local.IsInteger() && (parent.IsInteger() || parent == SqlType.Decimal) || local.IsFloat() && parent.IsFloat();
    internal static void Validate(Catalog catalog)
    {
        var tables = Unique(catalog.Tables, item => item.Name);
        var types = Unique(catalog.Types ?? [], item => item.Name);
        var sequences = Unique(catalog.Sequences ?? [], item => item.Name);
        var collations = Unique(catalog.Collations ?? [], item => item.Name);
        var views = Unique(catalog.Views ?? [], item => item.Name);
        if (collations.Values.Count(item => item.IsDefault) > 1) throw StorageFormat.Corrupt("multiple default collations");
        var visiting = new HashSet<string>(StringComparer.Ordinal); var visited = new HashSet<string>(StringComparer.Ordinal);
        void Type(SqlType type, int depth)
        {
            if (depth > 128) throw StorageFormat.Corrupt("composite dependency nesting limit");
            if (type.Element is { } element) Type(element, depth + 1);
            if (type.Kind != SqlKind.Composite) return;
            var name = type.Identity!;
            if (!types.TryGetValue(name, out var composite)) throw StorageFormat.Corrupt("missing composite type");
            if (visited.Contains(name)) return;
            if (!visiting.Add(name)) throw StorageFormat.Corrupt("cyclic composite dependency");
            foreach (var field in composite.Fields) Type(field.Type.Type, depth + 1);
            visiting.Remove(name); visited.Add(name);
        }
        foreach (var composite in types.Values) Type(SqlType.Composite(composite.Name), 0);
        foreach (var table in tables.Values)
        {
            if (table.RowCount < 0 || (table.Root == 0) != (table.RowCount == 0))
                throw StorageFormat.Corrupt("table root and row count disagree");
            foreach (var column in table.Columns)
            {
                Type(column.Type.Type, 0);
                if (column.Collation is { } name && name != "C" && !collations.ContainsKey(name))
                    throw StorageFormat.Corrupt("missing column collation");
            }
            _ = Unique(table.Indexes ?? [], item => item.Name);
            _ = Unique(table.Checks ?? [], item => item.Name);
            _ = Unique(table.ForeignKeys ?? [], item => item.Name);
            _ = Unique(table.Exclusions ?? [], item => item.Name);
        }
        foreach (var sequence in sequences.Values)
            if (sequence.OwnerTable is { } name && (!tables.TryGetValue(name, out var owner) ||
                sequence.OwnerColumn < 0 || sequence.OwnerColumn >= owner.Columns.Length || !owner.Columns[sequence.OwnerColumn].Type.Type.IsInteger()))
                throw StorageFormat.Corrupt("invalid sequence owner");
        visiting.Clear(); visited.Clear();
        void View(string name, int depth)
        {
            if (depth > 128) throw StorageFormat.Corrupt("view dependency nesting limit");
            if (visited.Contains(name)) return;
            if (!visiting.Add(name)) throw StorageFormat.Corrupt("cyclic view dependency");
            if (tables.ContainsKey(name)) throw StorageFormat.Corrupt("table and view share name");
            foreach (var dependency in views[name].Dependencies)
            {
                switch (dependency.Kind)
                {
                    case 0:
                        if (!tables.TryGetValue(dependency.Name, out var table) || dependency.Columns.Any(column => !table.Columns.Any(c => c.Name == column)))
                            throw StorageFormat.Corrupt("missing view table or column dependency");
                        break;
                    case 1:
                        if (!views.ContainsKey(dependency.Name)) throw StorageFormat.Corrupt("missing view dependency");
                        View(dependency.Name, depth + 1); break;
                    case 2:
                        if (!sequences.ContainsKey(dependency.Name)) throw StorageFormat.Corrupt("missing view sequence dependency"); break;
                    case 3:
                        if (!types.ContainsKey(dependency.Name)) throw StorageFormat.Corrupt("missing view type dependency"); break;
                    default: throw StorageFormat.Corrupt("invalid view dependency kind");
                }
            }
            visiting.Remove(name); visited.Add(name);
        }
        foreach (var name in views.Keys) View(name, 0);
        Statistics(catalog.Statistics ?? [], tables);
    }

    private static Dictionary<string, T> Unique<T>(IEnumerable<T> items, Func<T, string> name)
    {
        var map = new Dictionary<string, T>(StringComparer.Ordinal);
        foreach (var item in items) if (!map.TryAdd(name(item), item)) throw StorageFormat.Corrupt("duplicate catalog name");
        return map;
    }

    private static void Statistics(CatalogStatistic[] items, Dictionary<string, CatalogTable> tables)
    {
        foreach (var group in items.GroupBy(item => (item.Table, item.Column)))
        {
            if (!tables.TryGetValue(group.Key.Table, out var table) || group.Key.Column < 0 || group.Key.Column >= table.Columns.Length)
                throw StorageFormat.Corrupt("invalid statistic owner");
            var summaries = group.Where(item => item.Kind == 0).ToArray();
            if (summaries.Length != 1) throw StorageFormat.Corrupt("missing or duplicate statistic summary");
            var summary = summaries[0]; var type = table.Columns[group.Key.Column].Type.Type;
            var termEligible = type.Kind == SqlKind.Jsonb || type.Kind == SqlKind.Array && type.Element!.Kind is
                SqlKind.Boolean or SqlKind.Date or SqlKind.Timestamp or SqlKind.Timestamptz or SqlKind.I16 or SqlKind.I32 or SqlKind.I64 or SqlKind.F32 or SqlKind.F64 or SqlKind.Decimal or SqlKind.Text or SqlKind.Bytea or SqlKind.Uuid or SqlKind.Interval;
            var distributionEligible = type.Kind is not (SqlKind.Composite or SqlKind.Array or SqlKind.Json or SqlKind.Jsonb or SqlKind.Jsonpath);
            if (((summary.Flags & 2) != 0) != distributionEligible) throw StorageFormat.Corrupt("statistic distribution eligibility mismatch");
            if (((summary.Flags & 4) != 0) != termEligible) throw StorageFormat.Corrupt("statistic term eligibility mismatch");
            for (byte kind = 1; kind <= 3; kind++)
            {
                var entries = group.Where(item => item.Kind == kind).ToArray();
                var count = kind == 1 ? summary.McvCount : kind == 2 ? summary.HistogramCount : summary.TermCount;
                if (entries.Length != count) throw StorageFormat.Corrupt("statistic item count mismatch");
                var keys = new HashSet<string>(StringComparer.Ordinal); byte[]? prior = null; long priorCount = long.MaxValue;
                for (var index = 0; index < entries.Length; index++)
                {
                    var item = entries[index];
                    if (item.ItemOrdinal != index || item.Value is not { Length: > 0 and <= 128 }) throw StorageFormat.Corrupt("invalid statistic item");
                    byte[] key;
                    if (kind == 3)
                    {
                        key = item.Value;
                        if (item.RowCount < 1 || item.RowCount > summary.AnalyzedRows - summary.NullCount) throw StorageFormat.Corrupt("invalid statistic term count");
                    }
                    else
                    {
                        var reader = new WireReader(item.Value); var value = StoredValues.Decode(ref reader, type); reader.Finish();
                        if (value.IsNull || !StoredValues.Encode(value).AsSpan().SequenceEqual(item.Value)) throw StorageFormat.Corrupt("noncanonical statistic value");
                        key = KeyEncoding.Encode(value);
                        if (key.Length > 128 || kind == 1 && (item.SampleFrequency == 0 || item.SampleFrequency > summary.SampleNonnullRows))
                            throw StorageFormat.Corrupt("invalid statistic distribution value");
                    }
                    var frequency = kind == 1 ? item.SampleFrequency : item.RowCount;
                    if (kind != 2 && !keys.Add(Convert.ToHexString(key))) throw StorageFormat.Corrupt("duplicate statistic value");
                    if (prior is not null && (kind == 2 ? prior.AsSpan().SequenceCompareTo(key) > 0 :
                        frequency > priorCount || frequency == priorCount && prior.AsSpan().SequenceCompareTo(key) >= 0))
                        throw StorageFormat.Corrupt("unordered statistic values");
                    prior = key; priorCount = frequency;
                }
            }
        }
    }
}
