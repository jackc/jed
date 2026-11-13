using System.Runtime.CompilerServices;

namespace Jed;

internal static class SetExecution
{
    internal static async IAsyncEnumerable<SqlValue[]> Run(IAsyncEnumerable<SqlValue[]> left, IAsyncEnumerable<SqlValue[]> right,
        SqlType[] types, string operation, bool all, Meter meter, [EnumeratorCancellation] CancellationToken cancellationToken)
    {
        async ValueTask<List<SqlValue[]>> Read(IAsyncEnumerable<SqlValue[]> source)
        {
            var rows = new List<SqlValue[]>();
            await foreach (var row in source.WithCancellation(cancellationToken).ConfigureAwait(false))
                rows.Add(row.Select((value, index) => Resolve.Cast(value, types[index])).ToArray());
            return rows;
        }
        var a = await Read(left).ConfigureAwait(false); var b = await Read(right).ConfigureAwait(false);
        if (operation == "union" && all)
        {
            foreach (var row in a.Concat(b)) { meter.Checkpoint(cancellationToken); yield return row; }
            yield break;
        }
        var aKeys = new string[a.Count]; var bKeys = new string[b.Count];
        for (var i = 0; i < a.Count; i++) { foreach (var value in a[i]) GroupExecution.ChargeKey(value, meter); aKeys[i] = GroupExecution.Identity(a[i]); }
        for (var i = 0; i < b.Count; i++) { foreach (var value in b[i]) GroupExecution.ChargeKey(value, meter); bKeys[i] = GroupExecution.Identity(b[i]); }
        var emitted = new HashSet<string>(StringComparer.Ordinal);
        if (operation == "union")
        {
            for (var i = 0; i < a.Count; i++) if (emitted.Add(aKeys[i])) yield return a[i];
            for (var i = 0; i < b.Count; i++) if (emitted.Add(bKeys[i])) yield return b[i];
            yield break;
        }
        var counts = new Dictionary<string, int>(StringComparer.Ordinal);
        foreach (var key in bKeys) counts[key] = counts.GetValueOrDefault(key) + 1;
        for (var i = 0; i < a.Count; i++)
        {
            meter.Checkpoint(cancellationToken); var key = aKeys[i]; var count = counts.GetValueOrDefault(key);
            if (all && count > 0) counts[key] = count - 1;
            var keep = operation == "intersect" ? count > 0 : count == 0;
            if (keep && (all || emitted.Add(key))) yield return a[i];
        }
    }
}
