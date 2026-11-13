namespace Jed;

internal static class GinGather
{
    private sealed record Term(byte[] Key, long Span, List<StorageRecord>? Peek)
    {
        internal int Runs => Peek?.Count ?? int.MaxValue;
        internal long Bytes => Peek?.Sum(record => (long)record.Key.Length) ?? long.MaxValue;
    }
    internal static async ValueTask<SortedSet<byte[]>> Run(StorageTree tree, GinQueryNode node, Meter meter, CancellationToken token)
    {
        var result = new SortedSet<byte[]>(StatisticsBytes.Instance); var initialized = false;
        if (node.Terms.Length != 0)
        {
            if (node.All && node.Terms.Length > 1) result = await Intersect(tree, node.Terms, meter, token).ConfigureAwait(false);
            else foreach (var term in node.Terms) result.UnionWith(await Whole(tree, term, meter, token).ConfigureAwait(false));
            initialized = true;
        }
        foreach (var child in node.Children)
        {
            if (node.All && initialized && result.Count == 0) break;
            var keys = await Run(tree, child, meter, token).ConfigureAwait(false);
            if (!initialized || !node.All) result.UnionWith(keys); else result.IntersectWith(keys);
            initialized = true;
        }
        return result;
    }
    private static async ValueTask<List<StorageRecord>> Records(StorageTree tree, byte[] term, CancellationToken token)
    {
        var records = new List<StorageRecord>();
        await foreach (var record in tree.ScanRangeAsync(term, true, SecondaryAccess.Successor(term), false, cancellationToken: token).ConfigureAwait(false)) records.Add(record);
        return records;
    }
    private static SortedSet<byte[]> Decode(IEnumerable<StorageRecord> records, int termLength, Meter meter)
    {
        var keys = new SortedSet<byte[]>(StatisticsBytes.Instance);
        foreach (var record in records)
            foreach (var key in GinRuns.Decode(record.Key, termLength))
            { meter.Charge(Costs.GinEntry); meter.Checkpoint(); if (!keys.Add(key)) throw StorageFormat.Corrupt("duplicate GIN posting"); }
        return keys;
    }
    private static async ValueTask<SortedSet<byte[]>> Whole(StorageTree tree, byte[] term, Meter meter, CancellationToken token)
    {
        meter.Charge(tree.CountRangeNodes(term, true, SecondaryAccess.Successor(term), false)); meter.Checkpoint(token);
        return Decode(await Records(tree, term, token).ConfigureAwait(false), term.Length, meter);
    }
    private static async ValueTask<SortedSet<byte[]>> Intersect(StorageTree tree, byte[][] input, Meter meter, CancellationToken token)
    {
        var terms = new List<Term>();
        foreach (var key in input)
        {
            var span = tree.CountRangeNodes(key, true, SecondaryAccess.Successor(key), false);
            List<StorageRecord>? peek = null;
            if (span == tree.Height)
            { meter.Charge(span); meter.Checkpoint(token); peek = await Records(tree, key, token).ConfigureAwait(false); }
            terms.Add(new(key, span, peek));
        }
        terms.Sort((a, b) =>
        {
            var order = a.Span.CompareTo(b.Span); if (order != 0) return order;
            order = a.Runs.CompareTo(b.Runs); if (order != 0) return order;
            order = a.Bytes.CompareTo(b.Bytes); return order != 0 ? order : a.Key.AsSpan().SequenceCompareTo(b.Key);
        });
        var first = terms[0];
        var candidates = first.Peek is null ? await Whole(tree, first.Key, meter, token).ConfigureAwait(false) : Decode(first.Peek, first.Key.Length, meter);
        foreach (var term in terms.Skip(1))
        {
            if (candidates.Count == 0) break;
            candidates = await Probe(tree, term.Key, candidates.ToArray(), meter, token).ConfigureAwait(false);
        }
        return candidates;
    }
    private static async ValueTask<SortedSet<byte[]>> Probe(StorageTree tree, byte[] term, byte[][] candidates, Meter meter, CancellationToken token)
    {
        var result = new SortedSet<byte[]>(StatisticsBytes.Instance); var end = SecondaryAccess.Successor(term);
        for (var start = 0; start < candidates.Length;)
        {
            var low = term.Concat(candidates[start]).ToArray(); var finish = start;
            while (finish + 1 < candidates.Length && tree.CountRangeNodes(low, true, term.Concat(candidates[finish + 1]).ToArray(), true) == tree.Height) finish++;
            var high = term.Concat(candidates[finish]).ToArray(); var readsThrough = high;
            var postings = new SortedSet<byte[]>(StatisticsBytes.Instance);
            await foreach (var record in tree.ScanRangeAsync(low, true, end, false, cancellationToken: token).ConfigureAwait(false))
            {
                postings.UnionWith(GinRuns.Decode(record.Key, term.Length)); readsThrough = record.Key;
                if (record.Key.AsSpan().SequenceCompareTo(high) >= 0) break;
            }
            // If the term ended before the group's last probe, include that miss's
            // search path, as prescribed by the logical leaf-group read interval.
            if (readsThrough.AsSpan().SequenceCompareTo(high) < 0) readsThrough = high;
            meter.Charge(tree.CountRangeNodes(low, true, readsThrough, true)); meter.Checkpoint(token);
            for (var i = start; i <= finish; i++)
            { meter.Charge(Costs.GinEntry); meter.Checkpoint(token); if (postings.Contains(candidates[i])) result.Add(candidates[i]); }
            start = finish + 1;
        }
        return result;
    }
}
