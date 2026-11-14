using System.Runtime.CompilerServices;

namespace Jed;

// Only immutable, uncorrelated plans are retained. Execution state, pins, meters,
// iterators and spill hosts are always created afresh. Snapshot identity is a
// deliberately conservative estimator signature: every committed write misses.
internal sealed class PreparedSelectCache
{
    private StorageMeta? meta;
    private StoragePager? pager;
    private Catalog? catalog;
    private string? zone;
    internal SelectPlan? Plan { get; private set; }
    internal BoundColumn[] Columns { get; private set; } = [];
    internal CatalogTable? Table { get; private set; }
    internal bool SimpleScalar { get; private set; }

    internal bool Matches(DatabaseSnapshot current, Catalog currentCatalog, Meter meter) =>
        Plan is not null && ReferenceEquals(meta, current.Meta) && ReferenceEquals(pager, current.Pager) &&
        ReferenceEquals(catalog, currentCatalog) && zone == meter.Temporal.Zone;

    internal void Store(DatabaseSnapshot current, Catalog currentCatalog, Meter meter, SelectPlan plan, BoundColumn[] columns, CatalogTable? table)
    {
        meta = current.Meta; pager = current.Pager; catalog = currentCatalog; zone = meter.Temporal.Zone;
        Plan = plan with { Spill = null }; Columns = columns; Table = table;
        SimpleScalar = table is null && plan.Filter is null && plan.Group is null && plan.Windows is null &&
            !plan.Distinct && plan.DistinctOnCount == 0 && plan.Order is not { Length: > 0 } &&
            !plan.Empty && !plan.ColumnarProjection && plan.Offset == 0 && plan.Limit != 0 &&
            plan.Observation is null && plan.Projections.All(projection => Scalar(projection.Expression));
    }

    private static bool Scalar(Resolved expression) => expression.HostFunction is null && expression.Subquery is null &&
        expression.Outer is null && expression.Args.All(Scalar) && (expression.GrowthOperands?.All(Scalar) ?? true);

    internal async IAsyncEnumerable<Row> ScalarRows(Meter meter)
    {
        // Match ScalarSource's reservation lifetime, including a suspended
        // yielded row, while avoiding the unused general relation pipeline.
        var bytes = meter.Memory.ReserveRow(Array.Empty<SqlValue>());
        try { yield return await QueryExecution.ProjectAsync(Plan!.Projections, [], meter, Plan.ChargeRows).ConfigureAwait(false); }
        finally { meter.Memory.Release(bytes); }
    }
}

public sealed partial class Session
{
    private readonly ConditionalWeakTable<PreparedStatement, PreparedSelectCache> preparedSelects = new();
    private Catalog? readCatalogSource;
    private Catalog? readCatalogBound;

    private Catalog BindReadCatalog(Catalog catalog, Meter meter)
    {
        if (ReferenceEquals(readCatalogSource, catalog) && readCatalogBound is { } prior &&
            ReferenceEquals(prior.BoundUnicode, meter.Unicode) && ReferenceEquals(prior.BoundTimeZones, meter.Temporal.Data) &&
            ReferenceEquals(prior.BoundEnvironment, meter.Temporal.Environment)) return prior;
        var bound = catalog.BindCollations(meter.Unicode, meter.Temporal.Data, meter.Temporal.Environment);
        readCatalogSource = catalog; readCatalogBound = bound;
        return bound;
    }
}
