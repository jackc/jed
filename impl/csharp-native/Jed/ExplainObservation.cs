using System.Runtime.CompilerServices;

namespace Jed;

internal static class ExplainObservation
{
    // Measure only work performed while this iterator owns control. A consumer's
    // projection or sibling evaluation between yields is never charged here.
    internal static async IAsyncEnumerable<T> Rows<T>(IAsyncEnumerable<T> source, ExplainNode node, Meter meter,
        [EnumeratorCancellation] CancellationToken token = default, bool inclusive = false)
    {
        var baseline = node.ActualCost; long accrued = 0;
        await using var iterator = source.GetAsyncEnumerator(token);
        while (true)
        {
            var before = meter.Accrued;
            bool available;
            try { available = await iterator.MoveNextAsync().ConfigureAwait(false); }
            finally
            {
                accrued = Estimate.Add(accrued, meter.Accrued - before);
                if (inclusive) node.SetInclusive(Estimate.Add(baseline, accrued));
                else node.Attribute(meter.Accrued - before);
            }
            if (!available) yield break;
            yield return iterator.Current;
        }
    }
}
