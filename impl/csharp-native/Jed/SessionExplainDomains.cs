namespace Jed;

public sealed partial class Session
{
    private async ValueTask<Rows> DomainExplain(ExplainSyntax syntax, Parameters parameters, IReadOnlyList<SqlValue> values,
        Meter outerMeter, long limit, QueryOptions? queryOptions, CancellationToken token)
    {
        domainsEnabled = true;
        if (transactionFailed) throw new SqlException(SqlStates.InFailedSqlTransaction, "transaction is aborted");
        domainTransactionLease?.CheckRoles();
        outerMeter.Memory.HoldPending(CurrentPendingBytes());
        var db = await GetDatabase(token).ConfigureAwait(false);
        var temp = await GetTemporaryDatabase(token).ConfigureAwait(false);
        var lease = domainTransactionLease?.Retain() ?? await DomainLease.Acquire(db, temp, false, false, token).ConfigureAwait(false);
        try
        {
            var scope = (domainTransaction ?? lease.Scope).Fork();
            BindDomainCollations(scope, outerMeter.Unicode, outerMeter.Temporal.Data, outerMeter.Temporal.Environment);
            var main = scope.Get("main"); var catalog = scope.BindCatalog(main);
            StatementExecution.ValidateCatalogTarget(syntax.Inner);
            CheckPrivileges(syntax.Inner, catalog);
            var writes = syntax.Analyze && ExplainExecution.Writes(syntax.Inner, catalog);
            if (writes && (domainReadOnly || hostReadSession))
                throw new SqlException(SqlStates.ReadOnlySqlTransaction, "EXPLAIN ANALYZE writes in a read-only session");
            if (writes || syntax.Inner is CreateTableAsSyntax)
                CheckDdl(syntax.Inner, DomainExecution.Target(syntax.Inner) is { } target && scope.Resolve(target,
                    DomainExecution.Creation(syntax.Inner), DomainExecution.Temporary(syntax.Inner)).Name == "temp");
            if (writes)
            {
                if (domainTransaction is null)
                {
                    var persistent = PersistentWrite(syntax.Inner, scope);
                    await lease.DisposeAsync().ConfigureAwait(false);
                    lease = await DomainLease.Acquire(db, temp, persistent, true, token).ConfigureAwait(false);
                    scope = lease.Scope.Fork(); BindDomainCollations(scope, outerMeter.Unicode, outerMeter.Temporal.Data, outerMeter.Temporal.Environment);
                    main = scope.Get("main"); catalog = scope.BindCatalog(main);
                }
            }
            var innerMeter = new Meter(this, limit, token, queryOptions);
            foreach (var domain in scope.Items)
                if (domain.Pending is { } pending) pending.TrackMemory = domain.Name != "temp" && innerMeter.Memory.Enabled;
            innerMeter.Memory.HoldPending(CurrentPendingBytes());
            innerMeter.Sequences.Bind(catalog, main.Pending);
            var execution = main.Pending ?? (writes ? DomainExecution.ReadPending(scope, "main") : null);
            if (syntax.Inner is CreateTableAsSyntax create) StatementExecution.ValidateCreateTableAsName(create, catalog);
            parameters.Infer(syntax.Inner, db, main.Snapshot, execution, catalog, innerMeter, token);
            if (values.Count != 0 || syntax.Analyze) parameters.Bind(values, innerMeter.Temporal);
            innerMeter.Explain = syntax;
            var plan = ExplainExecution.Prepare(syntax.Inner, db, main.Snapshot, execution, catalog, innerMeter, token);
            var affected = syntax.Analyze ? await plan.Run().ConfigureAwait(false) : 0;
            if (syntax.Analyze) plan.Root.SetInclusive(innerMeter.Accrued);
            if (writes)
            {
                outerMeter.Memory.HoldPending(DomainPendingBytes(scope));
                CheckTemporaryStorage(scope); outerMeter.Checkpoint();
            }
            var result = RenderExplain(plan, syntax, outerMeter, innerMeter, affected);
            try
            {
                if (writes)
                {
                    if (domainTransaction is null)
                    {
                        await StatementExecution.TemporaryCommit(scope, innerMeter, token).ConfigureAwait(false);
                        await lease!.ReleaseWriterPinsForCommitAsync().ConfigureAwait(false);
                        await Database.CommitDomainsAsync(scope, token).ConfigureAwait(false);
                        outerMeter.Memory.HoldPending(0);
                    }
                    else domainTransaction = scope;
                    sequenceState = innerMeter.Sequences.State;
                }
                return result;
            }
            catch { await result.DisposeAsync().ConfigureAwait(false); throw; }
        }
        finally { await lease.DisposeAsync().ConfigureAwait(false); }
    }
}
