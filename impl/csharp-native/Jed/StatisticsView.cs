namespace Jed;

// Snapshot-only estimator inputs. Access never reads data pages or mutates facts.
internal sealed class StatisticsView
{
    internal CatalogStatistic Summary { get; }
    internal CatalogStatistic[] Mcv { get; }
    internal CatalogStatistic[] Histogram { get; }
    internal CatalogStatistic[] Terms { get; }
    internal long Population { get; }
    internal long NullRows { get; }
    internal long NonnullRows { get; }
    internal long DistinctValues { get; }
    internal long AverageWidth { get; }
    internal long[] McvRows { get; }
    internal long HistogramRows { get; }
    internal bool FreshComplete => (Summary.Flags & 1) == 0 && Summary.SampleRows == Summary.AnalyzedRows;
    private StatisticsView(CatalogStatistic summary, CatalogStatistic[] entries, long population)
    {
        Summary = summary; Population = population;
        Mcv = entries.Where(item => item.Kind == 1).OrderBy(item => item.ItemOrdinal).ToArray();
        Histogram = entries.Where(item => item.Kind == 2).OrderBy(item => item.ItemOrdinal).ToArray();
        Terms = entries.Where(item => item.Kind == 3).OrderBy(item => item.ItemOrdinal).ToArray();
        NullRows = Math.Min(population, Estimate.Scale(population, summary.NullCount, summary.AnalyzedRows));
        NonnullRows = population - NullRows;
        var analyzedNonnull = summary.AnalyzedRows - summary.NullCount;
        DistinctValues = (Int128)summary.DistinctCount * StatisticsLimits.NdvScaleDenominator > (Int128)analyzedNonnull * StatisticsLimits.NdvScaleNumerator
            ? Estimate.Scale(NonnullRows, summary.DistinctCount, analyzedNonnull) : summary.DistinctCount;
        DistinctValues = Math.Min(NonnullRows, DistinctValues);
        AverageWidth = analyzedNonnull == 0 ? 0 : summary.WidthSum / analyzedNonnull + (summary.WidthSum % analyzedNonnull == 0 ? 0 : 1);
        McvRows = new long[Mcv.Length]; long remaining = NonnullRows;
        for (var i = 0; i < Mcv.Length; i++)
        { McvRows[i] = Math.Min(remaining, Estimate.Scale(population, Mcv[i].SampleFrequency, summary.SampleRows)); remaining -= McvRows[i]; }
        HistogramRows = remaining;
    }
    internal static StatisticsView? Find(Catalog catalog, CatalogTable table, int column)
    {
        if (table.Columns[column].CollationSkew && ColumnStatistics.DistributionEligible(table.Columns[column].Type.Type)) return null;
        var entries = (catalog.Statistics ?? []).Where(item => item.Table == table.Name && item.Column == column).ToArray();
        var summary = entries.FirstOrDefault(item => item.Kind == 0);
        return summary is null || summary.AnalyzedRows == 0 ? null : new(summary, entries, table.RowCount);
    }
    internal static SqlValue Value(CatalogStatistic item, SqlType type)
    { var reader = new WireReader(item.Value!); var value = StoredValues.Decode(ref reader, type); reader.Finish(); return value; }
    internal long TermRows(ReadOnlySpan<byte> term)
    {
        foreach (var item in Terms)
            if (term.SequenceEqual(item.Value)) return Math.Min(NonnullRows, Estimate.Scale(Population, item.RowCount, Summary.AnalyzedRows));
        if ((Summary.Flags & 1) == 0 && Summary.UntrackedTermRows == 0) return 0;
        if (NonnullRows == 0) return 0;
        var half = Summary.UntrackedTermRows / 2 + Summary.UntrackedTermRows % 2;
        return Math.Max(1, Math.Min(NonnullRows, Estimate.Scale(Population, half, Summary.AnalyzedRows)));
    }
    internal long TermEntries => Estimate.Scale(Population, Summary.TermOccurrences, Summary.AnalyzedRows);
}
