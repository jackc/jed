namespace Jed;

public sealed partial class Session
{
    private async ValueTask<Rows> ExplainQuery(ExplainSyntax syntax, Parameters parameters, IReadOnlyList<SqlValue> values,
        Meter outerMeter, long limit, QueryOptions? queryOptions, CancellationToken token)
    {
        if (transactionFailed) throw new SqlException(SqlStates.InFailedSqlTransaction, "transaction is aborted");
        outerMeter.Memory.HoldPending(CurrentPendingBytes());
        var db = await GetDatabase(token).ConfigureAwait(false);
        await using var pin = readTransaction?.Retain() ?? await db.PinAsync(token).ConfigureAwait(false);
        if (transaction is not null) db.CheckPendingRole(transaction);
        readTransaction?.CheckRole();
        var catalog = (transaction?.Catalog ?? readTransaction?.Snapshot.Catalog ?? pin.Snapshot.Catalog).BindCollations(outerMeter.Unicode, outerMeter.Temporal.Data);
        StatementExecution.ValidateCatalogTarget(syntax.Inner);
        CheckPrivileges(syntax.Inner, catalog);
        var writes = syntax.Analyze && ExplainExecution.Writes(syntax.Inner, catalog);
        if (writes && (readTransaction is not null || hostReadSession)) throw new SqlException(SqlStates.ReadOnlySqlTransaction, "EXPLAIN ANALYZE writes in a read-only session");
        if (writes || syntax.Inner is CreateTableAsSyntax) CheckDdl(syntax.Inner, false);
        var automatic = writes && transaction is null;
        var pending = writes ? automatic ? await db.BeginWriteAsync(token).ConfigureAwait(false) : transaction!.Fork() : transaction;
        try
        {
            var snapshot = pending?.Before ?? readTransaction?.Snapshot ?? pin.Snapshot;
            catalog = (pending?.Catalog ?? catalog).BindCollations(outerMeter.Unicode, outerMeter.Temporal.Data);
            if (writes) pending!.Catalog = catalog;
            var innerMeter = new Meter(this, limit, token, queryOptions);
            innerMeter.Sequences.Bind(catalog, pending);
            innerMeter.Memory.HoldPending(CurrentPendingBytes());
            if (syntax.Inner is CreateTableAsSyntax create) StatementExecution.ValidateCreateTableAsName(create, catalog);
            parameters.Infer(syntax.Inner, db, snapshot, pending, catalog, innerMeter, token);
            if (values.Count != 0 || syntax.Analyze) parameters.Bind(values, innerMeter.Temporal);
            innerMeter.Explain = syntax;
            var plan = ExplainExecution.Prepare(syntax.Inner, db, snapshot, pending, catalog, innerMeter, token);
            var affected = syntax.Analyze ? await plan.Run().ConfigureAwait(false) : 0;
            if (syntax.Analyze) plan.Root.SetInclusive(innerMeter.Accrued);
            if (writes)
                outerMeter.Memory.HoldPending(pending!.PendingBytes);
            var result = RenderExplain(plan, syntax, outerMeter, innerMeter, affected);
            try
            {
                if (writes)
                {
                    if (automatic) { await db.CommitAsync(pending!, token).ConfigureAwait(false); outerMeter.Memory.HoldPending(0); }
                    else transaction = pending;
                    sequenceState = innerMeter.Sequences.State;
                }
                return result;
            }
            catch { await result.DisposeAsync().ConfigureAwait(false); throw; }
        }
        finally { if (automatic) await db.EndWriteAsync(pending!).ConfigureAwait(false); }
    }

    private Rows RenderExplain(ExplainedStatement plan, ExplainSyntax syntax, Meter outerMeter, Meter innerMeter, long affected)
    {
        var rows = new List<Row>();
        try
        {
            foreach (var row in plan.Empty ? [] : ExplainRows.Render(plan.Root, syntax, plan.Lane, innerMeter.Accrued, affected))
            {
                outerMeter.Charge(Costs.RowProduced); outerMeter.CheckCancellation();
                rows.Add(row.RetainMemory(outerMeter));
            }
            return new(this, ExplainRows.Columns(syntax), QueryExecution.Materialized(rows.ToArray()), outerMeter, guardCost: false);
        }
        catch { foreach (var row in rows) row.ReleaseMemory(); throw; }
    }
}
