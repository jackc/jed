namespace Jed;

public sealed partial class Session
{
    private long tempStorageLimit;
    private bool allowDdl;
    private bool allowTempDdl;
    public void SetTempStorageLimit(long maximum)
    {
        ArgumentOutOfRangeException.ThrowIfNegative(maximum); using var operation = Enter(); tempStorageLimit = maximum;
    }
    public void SetAllowDdl(bool allowed) { using var operation = Enter(); allowDdl = allowed; }
    public void SetAllowTempDdl(bool allowed) { using var operation = Enter(); allowTempDdl = allowed; }
    private void CheckDdl(StatementSyntax statement, bool temporary)
    {
        if (statement is SelectSyntax or InsertSyntax or UpdateSyntax or DeleteSyntax or WithStatementSyntax or TransactionSyntax) return;
        if (!(temporary && statement is not (AnalyzeSyntax or ReindexSyntax) ? allowTempDdl : allowDdl)) throw new SqlException(SqlStates.InsufficientPrivilege, "DDL is not allowed");
    }
    private Database? temporaryDatabase;
    private DomainLease? domainTransactionLease;
    private DatabaseDomains? domainTransaction;
    private bool domainReadOnly;
    private bool domainsEnabled;
    private sealed record DomainSavepoint(string Name, DatabaseDomains Scope, SequenceState Sequences);
    private readonly List<DomainSavepoint> domainSavepoints = [];

    private long CurrentPendingBytes() => domainTransaction is { } domains ? DomainPendingBytes(domains) : transaction?.PendingBytes ?? 0;
    internal static long DomainPendingBytes(DatabaseDomains domains) => domains.Items.Where(domain => domain.Name != "temp")
        .Aggregate(0L, (bytes, domain) => Meter.Add(bytes, domain.Pending?.PendingBytes ?? 0));
    private bool NeedsDomains(StatementSyntax statement, string sql) => domainsEnabled || database?.HasAttachments == true ||
        statement is TransactionSyntax || DomainExecution.Temporary(statement) || sql.Contains('.');

    private async ValueTask<Database> GetTemporaryDatabase(CancellationToken token)
    {
        if (temporaryDatabase is not null) return temporaryDatabase;
        var root = await GetDatabase(token).ConfigureAwait(false);
        temporaryDatabase = root.ScratchHost is { } scratch
            ? await Database.CreateStoreAsync(scratch.CreateSpillable(options.TempBuffers == 0 ? long.MaxValue : options.TempBuffers), cancellationToken: token).ConfigureAwait(false)
            : await Database.CreateInMemoryAsync(cancellationToken: token).ConfigureAwait(false);
        return temporaryDatabase;
    }

    private static void BindDomainCollations(DatabaseDomains scope, UnicodeData? unicode, TimeZoneData? timeZones)
    {
        foreach (var domain in scope.Items.ToArray())
        {
            var catalog = scope.BindCatalog(domain).BindCollations(unicode, timeZones);
            if (domain.Pending is { } pending) pending.Catalog = catalog;
            else scope.Replace(domain with { Snapshot = domain.Snapshot with { Catalog = catalog } });
        }
    }

    private static bool PersistentWrite(StatementSyntax statement, DatabaseDomains scope)
    {
        if (statement is WithStatementSyntax with)
            return PersistentWrite(with.Body, scope) || with.Definitions.Any(definition => definition.Mutation is { } mutation && PersistentWrite(mutation, scope));
        if (statement is SelectSyntax select)
            return !WritableCteExecution.HasWrites(select) || select.With!.Any(definition => definition.Mutation is { } mutation && PersistentWrite(mutation, scope));
        var target = DomainExecution.Target(statement);
        if (target is not null && statement is InsertSyntax or UpdateSyntax or DeleteSyntax)
        {
            var seen = new HashSet<string>(StringComparer.Ordinal);
            bool Persistent(string name)
            {
                if (!seen.Add(name)) return false;
                if (scope.Resolve(name).Name != "temp") return true;
                if (scope.View(name) is not { } view) return false;
                return view.View.Dependencies.Any(dependency => dependency.Kind is 0 or 1 && Persistent(dependency.Name));
            }
            if (Persistent(target)) return true;
        }
        return target is null || DomainExecution.ResolveTarget(statement, scope).Name != "temp";
    }

    private async ValueTask<Rows> DomainQuery(StatementSyntax statement, Parameters parameters, IReadOnlyList<SqlValue> values, Meter meter, CancellationToken token)
    {
        domainsEnabled = true;
        var db = await GetDatabase(token).ConfigureAwait(false);
        var temp = await GetTemporaryDatabase(token).ConfigureAwait(false);
        if (statement is TransactionSyntax control)
        {
            parameters.Bind(values, meter.Temporal);
            await DomainControl(control, db, temp, meter, token).ConfigureAwait(false);
            return new(this, [], QueryExecution.Materialized([]), meter);
        }
        if (transactionFailed) throw new SqlException(SqlStates.InFailedSqlTransaction, "transaction is aborted");
        domainTransactionLease?.CheckRoles();
        var lease = domainTransactionLease?.Retain() ?? await DomainLease.Acquire(db, temp, false, false, token).ConfigureAwait(false);
        var transfer = false;
        try
        {
            var scope = (domainTransaction ?? lease.Scope).Fork();
            BindDomainCollations(scope, meter.Unicode, meter.Temporal.Data);
            var main = scope.Get("main"); var catalog = scope.BindCatalog(main);
            var writes = statement is not SelectSyntax selected || SequenceExecution.Writes(selected, catalog);
            CheckDdl(statement, DomainExecution.Target(statement) is { } target && scope.Resolve(target,
                DomainExecution.Creation(statement) && statement is not CreateViewSyntax { Replace: true }, DomainExecution.Temporary(statement)).Name == "temp");
            if (writes && domainReadOnly) throw new SqlException(SqlStates.ReadOnlySqlTransaction, "write in read-only transaction");
            if (writes && domainTransaction is null)
            {
                var persistent = PersistentWrite(statement, scope);
                await lease.DisposeAsync().ConfigureAwait(false);
                lease = await DomainLease.Acquire(db, temp, persistent, true, token).ConfigureAwait(false);
                scope = lease.Scope.Fork(); BindDomainCollations(scope, meter.Unicode, meter.Temporal.Data);
                main = scope.Get("main"); catalog = scope.BindCatalog(main);
            }
            foreach (var domain in scope.Items) if (domain.Pending is { } pending) pending.TrackMemory = domain.Name != "temp" && meter.Memory.Enabled;
            meter.Sequences.Bind(catalog, main.Pending);
            if (parameters.Any) parameters.Infer(statement, db, main.Snapshot, main.Pending, catalog, meter, token);
            parameters.Bind(values, meter.Temporal);
            CheckPrivileges(statement, catalog);
            if (!writes)
            {
                var query = RelationExecution.Query((SelectSyntax)statement, db, main.Snapshot, main.Pending, catalog, meter, token);
                if (query.Initialize is { } initialize) await initialize().ConfigureAwait(false);
                transfer = true;
                return new(this, query.Columns.Select(column => new Column(column.Name, column.Type)).ToArray(), RelationExecution.PublicRows(query.Rows), meter, lease);
            }
            var execution = main.Pending ?? DomainExecution.ReadPending(scope, "main");
            StatementResult result;
            if (statement is SelectSyntax select)
            {
                if (WritableCteExecution.HasWrites(select))
                    result = await WritableCteExecution.Prepare(db, execution, select.With!, select.Recursive, select with { With = null }, meter, token).Run().ConfigureAwait(false);
                else
                {
                    var query = RelationExecution.Query(select, db, main.Snapshot, main.Pending, catalog, meter, token);
                    if (query.Initialize is { } initialize) await initialize().ConfigureAwait(false);
                    var rows = new List<Row>();
                    await foreach (var row in query.Rows.WithCancellation(token).ConfigureAwait(false)) rows.Add(new Row(row).RetainMemory(meter, query.MemoryOutput));
                    result = new(query.Columns.Select(column => new Column(column.Name, column.Type)).ToArray(), rows.ToArray());
                }
            }
            else result = await StatementExecution.Execute(db, execution, statement, meter, token).ConfigureAwait(false);
            meter.Memory.HoldPending(DomainPendingBytes(scope));
            CheckTemporaryStorage(scope);
            meter.Checkpoint();
            if (domainTransaction is null)
            {
                await StatementExecution.TemporaryCommit(scope, meter, token).ConfigureAwait(false);
                await Database.CommitDomainsAsync(scope, token).ConfigureAwait(false);
                meter.Memory.HoldPending(0);
            }
            else domainTransaction = scope;
            sequenceState = meter.Sequences.State;
            return new(this, result.Columns, QueryExecution.Materialized(result.Rows), meter, affected: result.Affected);
        }
        finally { if (!transfer) await lease.DisposeAsync().ConfigureAwait(false); }
    }

    private void CheckTemporaryStorage(DatabaseDomains scope)
    {
        var temp = scope.Get("temp");
        if (temp.Pending?.Changed == true && tempStorageLimit > 0 &&
            (long)temp.Snapshot.Meta.PageCount * temp.Snapshot.Meta.PageSize > tempStorageLimit)
            throw new SqlException(SqlStates.TempStorageLimitExceeded, "temporary storage limit exceeded");
    }

    private async ValueTask DomainControl(TransactionSyntax control, Database db, Database temp, Meter meter, CancellationToken token)
    {
        if (transactionFailed && control.Operation is not ("commit" or "rollback" or "rollback_to"))
            throw new SqlException(SqlStates.InFailedSqlTransaction, "transaction is aborted");
        if (control.Operation == "begin")
        {
            if (TransactionOpen) throw new SqlException(SqlStates.ActiveSqlTransaction, "transaction is already active");
            domainTransactionLease = await DomainLease.Acquire(db, temp, !control.ReadOnly, !control.ReadOnly, token).ConfigureAwait(false);
            domainTransaction = domainTransactionLease.Scope.Fork(); domainReadOnly = control.ReadOnly;
            sequenceBeforeTransaction = sequenceState.Fork(); return;
        }
        if (control.Operation is "commit" or "rollback")
        {
            var scope = domainTransaction; var lease = domainTransactionLease;
            var rollback = control.Operation == "rollback" || transactionFailed;
            domainTransaction = null; domainTransactionLease = null;
            try
            {
                if (!rollback && scope is not null)
                {
                    lease!.CheckRoles();
                    if (!domainReadOnly)
                    {
                        BindDomainCollations(scope, meter.Unicode, meter.Temporal.Data); meter.Sequences.Bind(scope.BindCatalog(scope.Get("main")), scope.Get("main").Pending);
                        await StatementExecution.TemporaryCommit(scope, meter, token).ConfigureAwait(false);
                        await Database.CommitDomainsAsync(scope, token).ConfigureAwait(false);
                    }
                }
            }
            catch { rollback = true; throw; }
            finally
            {
                if (rollback && sequenceBeforeTransaction is not null) sequenceState = sequenceBeforeTransaction;
                sequenceBeforeTransaction = null; transactionFailed = false; domainReadOnly = false; domainSavepoints.Clear();
                if (lease is not null) { await lease.ReleaseWriters().ConfigureAwait(false); await lease.DisposeAsync().ConfigureAwait(false); }
            }
            return;
        }
        if (domainTransaction is null) throw new SqlException(SqlStates.NoActiveSqlTransaction, "savepoint requires a transaction");
        domainTransactionLease!.CheckRoles();
        if (control.Operation == "savepoint") { foreach (var domain in domainTransaction.Items) domain.Pending?.ForgetPendingKeys(); domainSavepoints.Add(new(control.Name!, domainTransaction.Fork(), sequenceState.Fork())); return; }
        var index = domainSavepoints.FindLastIndex(frame => frame.Name == control.Name);
        if (index < 0) throw new SqlException(SqlStates.InvalidSavepointSpecification, "savepoint does not exist");
        if (control.Operation == "rollback_to")
        {
            domainTransaction = domainSavepoints[index].Scope.Fork(); foreach (var domain in domainTransaction.Items) domain.Pending?.ForgetPendingKeys(); sequenceState = domainSavepoints[index].Sequences.Fork(); transactionFailed = false;
            domainSavepoints.RemoveRange(index + 1, domainSavepoints.Count - index - 1);
        }
        else domainSavepoints.RemoveRange(index, domainSavepoints.Count - index);
    }
}

internal static partial class StatementExecution
{
    internal static async ValueTask TemporaryCommit(DatabaseDomains scope, Meter meter, CancellationToken token)
    {
        var temp = scope.Get("temp"); if (temp.Pending is not { } pending) return;
        var truncate = pending.Catalog.Tables.Where(table => table.OnCommit == 1).OrderBy(table => table.Name, Utf8NameComparer.Instance).ToArray();
        foreach (var parent in truncate)
            if (pending.Catalog.Tables.Any(child => child.OnCommit != 1 && (child.ForeignKeys ?? []).Any(key => key.Parent == parent.Name)))
                throw Errors.Unsupported("unsupported ON COMMIT and foreign key combination");
        foreach (var table in truncate)
        {
            var tree = await temp.Database.Tree(table, pending, pending.Before).ClearAsync(token).ConfigureAwait(false);
            foreach (var index in table.Indexes ?? [])
                pending.IndexTrees[index.Name] = await temp.Database.IndexTree(index, pending, pending.Before).ClearAsync(token).ConfigureAwait(false);
            if (table.RowCount != 0) Put(pending, table with { RowCount = 0 }, tree);
        }
        foreach (var table in pending.Catalog.Tables.Where(table => table.OnCommit == 2).OrderBy(table => table.Name, Utf8NameComparer.Instance).ToArray())
            await ExecuteLocal(temp.Database, pending, new DropTableSyntax(table.Name, false, true), meter, token).ConfigureAwait(false);
    }
}
