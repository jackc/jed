namespace Jed;

// Bounded deterministic full-population inspection with a hash-priority row sample.
internal sealed class ColumnStatistics(CatalogTable table, int column, Meter meter)
{
    private sealed record Sample(ulong Priority, long Ordinal, bool Null, byte[]? Key, byte[]? Value);
    private sealed class SampleOrder : IComparer<Sample>
    {
        public int Compare(Sample? left, Sample? right)
        { var order = left!.Priority.CompareTo(right!.Priority); return order != 0 ? order : left.Ordinal.CompareTo(right.Ordinal); }
    }
    private sealed record Frequency(byte[] Key, byte[] Value, uint Count);
    private sealed record TermCounter(long Count, long Delta);
    private readonly SortedSet<Sample> sample = new(new SampleOrder());
    private readonly SortedSet<ulong> hashes = [];
    private readonly SortedDictionary<byte[], TermCounter> terms = new(StatisticsBytes.Instance);
    private readonly bool distribution = DistributionEligible(table.Columns[column].Type.Type);
    private readonly bool termEligible = StatisticsTerms.Eligible(table.Columns[column].Type.Type);
    private long rows, nulls, width, termOccurrences;
    private bool hashOverflow, termsPruned, oversizedTerm;
    internal static bool DistributionEligible(SqlType type) => type.Kind is not (SqlKind.Array or SqlKind.Composite or SqlKind.Json or SqlKind.Jsonb or SqlKind.Jsonpath);
    internal static ulong Hash(ReadOnlySpan<byte> bytes)
    {
        var hash = 14695981039346656037UL;
        foreach (var value in bytes) hash = unchecked((hash ^ value) * 1099511628211UL);
        return hash;
    }
    internal void Add(byte[] storageKey, SqlValue value)
    {
        byte[]? key = null, body = null; long valueWidth = 0;
        if (!value.IsNull)
        {
            if (distribution) { key = KeyEncoding.Encode(value, collation: table.Columns[column].CollationData); valueWidth = key.Length; }
            else valueWidth = BodyWidth(value);
        }
        meter.Charge(Estimate.Multiply(Costs.StatisticsValue, Math.Max(1, valueWidth))); meter.Checkpoint();
        if (value.IsNull) nulls++;
        else
        {
            width = Estimate.Add(width, valueWidth);
            if (distribution)
            {
                var hash = Hash(key!);
                if (!hashes.Contains(hash))
                {
                    if (hashes.Count == StatisticsLimits.KmvHashes) hashOverflow = true;
                    if (hashes.Count < StatisticsLimits.KmvHashes || hash < hashes.Max)
                    { hashes.Add(hash); if (hashes.Count > StatisticsLimits.KmvHashes) hashes.Remove(hashes.Max); }
                }
                if (key!.Length <= StatisticsLimits.MaxValueBytes && BodyWidth(value) + 1 <= StatisticsLimits.MaxValueBytes) body = StoredValues.Encode(value);
            }
        }
        var priority = Hash(storageKey);
        if (sample.Count < StatisticsLimits.SampleRows || priority < sample.Max!.Priority || priority == sample.Max.Priority && rows < sample.Max.Ordinal)
        {
            sample.Add(new(priority, rows, value.IsNull, body is null ? null : key, body));
            if (sample.Count > StatisticsLimits.SampleRows) sample.Remove(sample.Max!);
        }
        if (termEligible && !value.IsNull)
        {
            foreach (var term in StatisticsTerms.Extract(value))
            {
                meter.Charge(Estimate.Multiply(Costs.StatisticsValue, Math.Max(1, term.Length))); meter.Checkpoint();
                termOccurrences = Estimate.Add(termOccurrences, 1);
                var bucket = termOccurrences / StatisticsLimits.TermBucketWidth + (termOccurrences % StatisticsLimits.TermBucketWidth == 0 ? 0 : 1);
                if (term.Length > StatisticsLimits.MaxValueBytes) oversizedTerm = true;
                else if (terms.TryGetValue(term, out var old)) terms[term] = old with { Count = Estimate.Add(old.Count, 1) };
                else terms.Add(term, new(1, bucket - 1));
                if (termOccurrences % StatisticsLimits.TermBucketWidth == 0)
                    foreach (var entry in terms.Where(entry => Estimate.Add(entry.Value.Count, entry.Value.Delta) <= bucket).ToArray())
                    { terms.Remove(entry.Key); termsPruned = true; }
            }
        }
        rows++;
    }
    internal CatalogStatistic[] Finish()
    {
        var ndv = Distinct();
        var nonnull = (uint)sample.Count(item => !item.Null);
        var oversized = sample.Any(item => !item.Null && item.Value is null);
        var groups = new List<Frequency>();
        foreach (var item in sample.Where(item => item.Value is not null).OrderBy(item => item.Key!, StatisticsBytes.Instance).ThenBy(item => item.Ordinal))
        {
            if (groups.Count != 0 && groups[^1].Key.AsSpan().SequenceEqual(item.Key)) groups[^1] = groups[^1] with { Count = groups[^1].Count + 1 };
            else groups.Add(new(item.Key!, item.Value!, 1));
        }
        var complete = sample.Count == rows && !oversized && groups.Count <= StatisticsLimits.McvEntries;
        var mcv = groups.Where(group => complete || group.Count >= 2 && (Int128)group.Count * ndv > nonnull)
            .OrderByDescending(group => group.Count).ThenBy(group => group.Key, StatisticsBytes.Instance).Take(StatisticsLimits.McvEntries).ToArray();
        var selected = new SortedSet<byte[]>(mcv.Select(group => group.Key), StatisticsBytes.Instance);
        var remainder = sample.Where(item => item.Key is not null && !selected.Contains(item.Key)).OrderBy(item => item.Key!, StatisticsBytes.Instance).ThenBy(item => item.Ordinal).ToArray();
        var histogram = new List<byte[]>();
        if (!oversized && remainder.Length >= 2)
        {
            var count = Math.Min(StatisticsLimits.HistogramBounds, remainder.Length);
            for (var i = 0; i < count; i++) histogram.Add(remainder[(int)((long)i * (remainder.Length - 1) / (count - 1))].Value!);
        }
        var termList = terms.OrderByDescending(entry => entry.Value.Count).ThenBy(entry => entry.Key, StatisticsBytes.Instance).ToArray();
        var untracked = termList.Length > StatisticsLimits.TermEntries ? termList[StatisticsLimits.TermEntries].Value.Count : 0;
        if (termsPruned) untracked = Math.Max(untracked, termOccurrences / StatisticsLimits.TermBucketWidth + (termOccurrences % StatisticsLimits.TermBucketWidth == 0 ? 0 : 1));
        if (oversizedTerm) untracked = Math.Max(untracked, 1);
        untracked = Math.Min(untracked, rows - nulls);
        var retainedTerms = termList.Take(StatisticsLimits.TermEntries).ToArray();
        var result = new List<CatalogStatistic> { new(0, table.Name, column, (byte)((distribution ? 2 : 0) | (termEligible ? 4 : 0)),
            rows, nulls, width, ndv, (uint)sample.Count, nonnull, mcv.Length, histogram.Count, termOccurrences, untracked, retainedTerms.Length) };
        for (var i = 0; i < mcv.Length; i++) result.Add(new(1, table.Name, column, ItemOrdinal: i, SampleFrequency: mcv[i].Count, Value: mcv[i].Value));
        for (var i = 0; i < histogram.Count; i++) result.Add(new(2, table.Name, column, ItemOrdinal: i, Value: histogram[i]));
        for (var i = 0; i < retainedTerms.Length; i++) result.Add(new(3, table.Name, column, ItemOrdinal: i, RowCount: retainedTerms[i].Value.Count, Value: retainedTerms[i].Key));
        return result.ToArray();
    }
    private long Distinct()
    {
        if (!distribution) return 0;
        if (!hashOverflow) return hashes.Count;
        var numerator = (UInt128)(StatisticsLimits.KmvHashes - 1) << 64;
        var denominator = (UInt128)hashes.Max + 1;
        var count = (numerator + denominator - 1) / denominator;
        return (long)UInt128.Min((UInt128)(rows - nulls), UInt128.Max((UInt128)(StatisticsLimits.KmvHashes + 1), count));
    }
    // Canonical ordinary value body, without its nullable tag or fixed-column compression.
    internal static long BodyWidth(SqlValue value)
    {
        if (value.IsNull) return 0;
        switch (value.Type.Kind)
        {
            case SqlKind.Text: return 2L + TextEncoding.Utf8.GetByteCount(value.AsText());
            case SqlKind.Json: return 2L + TextEncoding.Utf8.GetByteCount(value.AsJsonText());
            case SqlKind.Jsonpath: return 2L + TextEncoding.Utf8.GetByteCount(value.AsJsonpath().Text);
            case SqlKind.Bytea: return 2L + value.ByteSpan.Length;
            case SqlKind.Decimal: return 5L + 2L * value.AsDecimal().ExportBase10000().Length;
            case SqlKind.Jsonb: return JsonWidth(value.AsJsonb());
            case SqlKind.Array:
                var array = value.AsArray(); long bytes = 2L + 8L * array.Rank + (array.Values.Any(item => item.IsNull) ? (array.Count + 7L) / 8 : 0);
                foreach (var item in array.Values) bytes = Estimate.Add(bytes, BodyWidth(item)); return bytes;
            case SqlKind.Composite:
                var fields = value.AsComposite().Fields; long size = (fields.Count + 7L) / 8;
                foreach (var field in fields) size = Estimate.Add(size, BodyWidth(field)); return size;
            case SqlKind.Range:
                var range = value.AsRange(); return 1L + (range.Lower is { } lo ? BodyWidth(lo) : 0) + (range.Upper is { } hi ? BodyWidth(hi) : 0);
            default: return StoredValues.Width(new(value.Type));
        }
    }
    private static long VarintWidth(int value) { long width = 1; for (; value >= 128; value >>= 7) width++; return width; }
    private static long JsonWidth(SqlJsonNode node)
    {
        switch (node.Kind)
        {
            case SqlJsonKind.Number: return 1 + BodyWidth(SqlValue.StoredDecimal(node.Number!));
            case SqlJsonKind.String: var bytes = TextEncoding.Utf8.GetByteCount(node.Text!); return 1 + VarintWidth(bytes) + bytes;
            case SqlJsonKind.Array: return 1 + VarintWidth(node.Elements.Count) + node.Elements.Sum(JsonWidth);
            case SqlJsonKind.Object: return 1 + VarintWidth(node.Members.Count) + node.Members.Sum(member => 1 + VarintWidth(TextEncoding.Utf8.GetByteCount(member.Key)) + TextEncoding.Utf8.GetByteCount(member.Key) + JsonWidth(member.Value));
            default: return 1;
        }
    }
}
