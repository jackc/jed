using System.Collections.ObjectModel;

namespace Jed;

/// <summary>An asynchronous session. Concurrent operations are rejected.</summary>
public sealed partial class Session : IAsyncDisposable
{
    private int active;
    private bool disposed;
    private Database? database;
    private bool ownsDatabase;
    private PendingDatabase? transaction;
    private bool transactionFailed;
    private SequenceState sequenceState = new();
    private SequenceState? sequenceBeforeTransaction;
    internal SequenceState SequenceStateSnapshot() => sequenceState.Fork();
    private Database.SnapshotPin? readTransaction;
    private bool TransactionOpen => transaction is not null || readTransaction is not null || domainTransaction is not null;
    private sealed record SavepointFrame(string Name, PendingDatabase? Pending, SequenceState Sequences);
    private readonly List<SavepointFrame> savepoints = [];
    public long MaxQueryMemoryBytes { get; private set; }
    public void SetQueryMemoryLimit(long maximum) { using var operation = Enter(); MaxQueryMemoryBytes = maximum; }
    public long LifetimeCost { get; internal set; }
    public long LifetimeMaxCost { get; private set; }
    private readonly SessionOptions options;
    private TimeZoneData? timeZones;
    private UnicodeData? unicodeData;
    internal TemporalContext NewTemporalContext(QueryOptions? query = null) => new(query?.Clock ?? options.Clock ?? SessionOptions.HostClock, query?.TimeZone ?? options.TimeZone, timeZones, unicodeData);

    public Session(long lifetimeMaxCost = 0) : this(new SessionOptions { LifetimeMaxCost = lifetimeMaxCost }) { }

    public Session(SessionOptions options)
    {
        ArgumentNullException.ThrowIfNull(options);
        if ((options.DefaultPrivileges & ~TablePrivileges.All) != 0) throw new ArgumentOutOfRangeException(nameof(options));
        if (options.LifetimeMaxCost < 0) throw new ArgumentOutOfRangeException(nameof(options));
        _ = TimeZoneData.Resolve(options.TimeZone, options.TimeZones);
        timeZones = options.TimeZones;
        unicodeData = options.UnicodeData;
        this.options = options; defaultPrivileges = options.DefaultPrivileges;
        if (options.MaxTempStorageBytes < 0 || options.TempBuffers < 0) throw new ArgumentOutOfRangeException(nameof(options));
        tempStorageLimit = options.MaxTempStorageBytes; allowDdl = options.AllowDdl; allowTempDdl = options.AllowTempDdl ?? options.AllowDdl;
        LifetimeMaxCost = options.LifetimeMaxCost; MaxQueryMemoryBytes = options.MaxQueryMemoryBytes;
    }

    internal Session(Database database, long lifetimeMaxCost) : this(lifetimeMaxCost) { this.database = database; }
    internal Session(Database database, SessionOptions options) : this(options) { this.database = database; }

    /// <summary>Install an immutable, validated host-provided JTZ bundle for later statements.</summary>
    public void LoadTimeZoneData(ReadOnlySpan<byte> bytes)
    {
        using var operation = Enter(); timeZones = TimeZoneData.Load(bytes);
    }
    /// <summary>Add validated host-provided Unicode collation and casing data.</summary>
    public void LoadUnicodeData(ReadOnlySpan<byte> bytes)
    {
        using var operation = Enter(); var data = UnicodeData.Load(bytes); unicodeData = unicodeData is null ? data : unicodeData.Add(data);
    }

    public void SetLifetimeCostLimit(long maximum)
    {
        ArgumentOutOfRangeException.ThrowIfNegative(maximum);
        using var operation = Enter(); LifetimeMaxCost = maximum;
    }
    /// <summary>Resolve now; evaluate SELECT lazily as the returned cursor is drained.</summary>
    public Task<Rows> QueryAsync(string sql, long maxCost = 0, CancellationToken cancellationToken = default) =>
        QueryCore(sql, maxCost, cancellationToken, null);
    public Task<Rows> QueryAsync(string sql, QueryOptions options, CancellationToken cancellationToken = default) =>
        QueryCore(sql, (options ?? throw new ArgumentNullException(nameof(options))).MaxCost, cancellationToken, options);
    private async Task<Rows> QueryCore(string sql, long maxCost, CancellationToken cancellationToken, QueryOptions? queryOptions, StatementSyntax? prepared = null)
    {
        using var operation = Enter();
        ArgumentNullException.ThrowIfNull(sql);
        if (maxCost < 0) throw new ArgumentOutOfRangeException(nameof(maxCost));
        if (cancellationToken.IsCancellationRequested) throw new SqlCanceledException(cancellationToken);
        if (queryOptions?.MaxScalarBytes < 0 || queryOptions?.MaxSqlLength < 0) throw new ArgumentOutOfRangeException(nameof(queryOptions));
        var meter = new Meter(this, maxCost, cancellationToken, queryOptions);
        try
        {
            var maxSqlLength = queryOptions?.MaxSqlLength ?? options.MaxSqlLength;
            if (maxSqlLength > 0 && TextEncoding.ByteCount(sql) > maxSqlLength)
                throw new SqlException(SqlStates.ProgramLimitExceeded, "SQL input exceeds length limit");
            meter.Checkpoint();
            var parameterContext = new Parameters();
            var statement = parameterContext.Statement(prepared ?? new Parser(sql).ParseStatement());
            StatementExecution.ValidateCatalogTarget(statement);
            CheckReindexTransaction(statement);
            if (statement is ExplainSyntax explain)
                return NeedsDomains(explain.Inner, sql)
                    ? await DomainExplain(explain, parameterContext, queryOptions?.Parameters ?? [], meter, maxCost, queryOptions, cancellationToken).ConfigureAwait(false)
                    : await ExplainQuery(explain, parameterContext, queryOptions?.Parameters ?? [], meter, maxCost, queryOptions, cancellationToken).ConfigureAwait(false);
            if (hostReadSession && statement is not SelectSyntax) throw new SqlException(SqlStates.ReadOnlySqlTransaction, "read handle cannot write");
            if (statement is not TransactionSyntax) meter.Memory.HoldPending(CurrentPendingBytes());
            if (NeedsDomains(statement, sql)) return await DomainQuery(statement, parameterContext, queryOptions?.Parameters ?? [], meter, cancellationToken).ConfigureAwait(false);
            if (transaction is not null && statement is not TransactionSyntax { Operation: "rollback" }) database!.CheckPendingRole(transaction);
            if (statement is not TransactionSyntax { Operation: "rollback" }) readTransaction?.CheckRole();
            if (statement is TransactionSyntax control)
            {
                parameterContext.Bind(queryOptions?.Parameters ?? [], meter.Temporal);
                await TransactionControl(control, cancellationToken).ConfigureAwait(false);
                return new(this, [], QueryExecution.Materialized([]), meter);
            }
            if (transactionFailed) throw new SqlException(SqlStates.InFailedSqlTransaction, "transaction is aborted");
            if (statement is SelectSyntax select)
            {
                var db = await GetDatabase(cancellationToken).ConfigureAwait(false);
                var pin = readTransaction?.Retain() ?? await db.PinAsync(cancellationToken).ConfigureAwait(false);
                try
                {
                    var catalog = (transaction?.Catalog ?? readTransaction?.Snapshot.Catalog ?? pin.Snapshot.Catalog).BindCollations(meter.Unicode, meter.Temporal.Data);
                    CheckPrivileges(statement, catalog);
                    if (SequenceExecution.Writes(select, catalog))
                    {
                        await pin.DisposeAsync().ConfigureAwait(false);
                        return await SequenceQuery(select, meter, cancellationToken, parameterContext, queryOptions?.Parameters ?? []).ConfigureAwait(false);
                    }
                    meter.Sequences.Bind(catalog, transaction);
                    if (parameterContext.Any) parameterContext.Infer(statement, db, transaction?.Before ?? readTransaction?.Snapshot ?? pin.Snapshot, transaction, catalog, meter, cancellationToken);
                    parameterContext.Bind(queryOptions?.Parameters ?? [], meter.Temporal);
                    var query = RelationExecution.Query(select, db, transaction?.Before ?? readTransaction?.Snapshot ?? pin.Snapshot, transaction, catalog, meter, cancellationToken);
                    if (query.Initialize is { } initialize) await initialize().ConfigureAwait(false);
                    return new(this, query.Columns.Select(column => new Column(column.Name, column.Type)).ToArray(), RelationExecution.PublicRows(query.Rows), meter, pin);
                }
                catch { await pin.DisposeAsync().ConfigureAwait(false); throw; }
            }
            CheckDdl(statement, false);
            if (readTransaction is not null) throw new SqlException(SqlStates.ReadOnlySqlTransaction, "write in read-only transaction");
            var handle = await GetDatabase(cancellationToken).ConfigureAwait(false);
            var automatic = transaction is null;
            var pendingWrite = automatic ? await handle.BeginWriteAsync(cancellationToken).ConfigureAwait(false) : transaction!.Fork();
            try
            {
                pendingWrite.TrackMemory = meter.Memory.Enabled;
                pendingWrite.Catalog = pendingWrite.Catalog.BindCollations(meter.Unicode, meter.Temporal.Data);
                meter.Sequences.Bind(pendingWrite.Catalog, pendingWrite);
                if (parameterContext.Any) parameterContext.Infer(statement, handle, pendingWrite.Before, pendingWrite, pendingWrite.Catalog, meter, cancellationToken);
                parameterContext.Bind(queryOptions?.Parameters ?? [], meter.Temporal);
                CheckPrivileges(statement, pendingWrite.Catalog);
                var result = await StatementExecution.Execute(handle, pendingWrite, statement, meter, cancellationToken).ConfigureAwait(false);
                meter.Memory.HoldPending(pendingWrite.PendingBytes);
                if (automatic) { await handle.CommitAsync(pendingWrite, cancellationToken).ConfigureAwait(false); meter.Memory.HoldPending(0); }
                else transaction = pendingWrite;
                sequenceState = meter.Sequences.State;
                return new(this, result.Columns, QueryExecution.Materialized(result.Rows), meter, affected: result.Affected);
            }
            finally { if (automatic) await handle.EndWriteAsync(pendingWrite).ConfigureAwait(false); }
        }
        catch (Exception error)
        {
            if (error is not SqlException { SqlState: SqlStates.ActiveSqlTransaction }) FailStatement();
            if (error is SqlException sqlError) sqlError.Cost = meter.Accrued;
            if (error is SqlCanceledException canceled) canceled.Cost = meter.Accrued;
            if (error is OperationCanceledException && error is not SqlCanceledException)
                throw new SqlCanceledException(cancellationToken) { Cost = meter.Accrued };
            throw;
        }
    }
    internal async ValueTask SetWorkMemAsync(long bytes)
    {
        var handle = await GetDatabase(CancellationToken.None).ConfigureAwait(false);
        handle.SetWorkMem(bytes);
    }

    private async ValueTask<Database> GetDatabase(CancellationToken cancellationToken)
    {
        if (database is not null) return database;
        database = await Database.CreateInMemoryAsync(cancellationToken: cancellationToken).ConfigureAwait(false);
        ownsDatabase = true; return database;
    }
    internal void FailStatement() { if (TransactionOpen && !hostReadSession) transactionFailed = true; }

    internal Operation Enter()
    {
        if (Interlocked.CompareExchange(ref active, 1, 0) != 0)
            throw new SqlException(SqlStates.ObjectInUse, "session operation already in progress");
        if (disposed) { Volatile.Write(ref active, 0); throw new ObjectDisposedException(nameof(Session)); }
        return new Operation(this);
    }
    internal readonly struct Operation(Session session) : IDisposable
    {
        public void Dispose() => Volatile.Write(ref session.active, 0);
    }
    public async ValueTask DisposeAsync()
    {
        if (disposed) return;
        using var operation = Enter();
        if (transaction is not null) { var pending = transaction; transaction = null; await database!.EndWriteAsync(pending).ConfigureAwait(false); }
        if (readTransaction is not null) { await readTransaction.DisposeAsync().ConfigureAwait(false); readTransaction = null; }
        savepoints.Clear();
        if (domainTransactionLease is not null) { await domainTransactionLease.ReleaseWriters().ConfigureAwait(false); await domainTransactionLease.DisposeAsync().ConfigureAwait(false); domainTransactionLease = null; domainTransaction = null; }
        if (temporaryDatabase is not null) { await temporaryDatabase.DisposeAsync().ConfigureAwait(false); temporaryDatabase = null; }
        if (ownsDatabase) await database!.DisposeAsync().ConfigureAwait(false);
        disposed = true;
    }
}

public sealed record Column(string Name, SqlType Type);

/// <summary>An owned row; values remain valid after advancing or closing the cursor.</summary>
public sealed class Row
{
    private readonly SqlValue[] values;
    internal Row(SqlValue[] values) { this.values = values; }
    private QueryMemoryAccount? memoryOwner;
    private long memoryBytes;
    internal Row RetainMemory(Meter meter, bool alreadyReserved = false)
    {
        if (memoryOwner is null && meter.Memory.Enabled)
        { memoryBytes = alreadyReserved ? MemorySize.RowBytes(values) : meter.Memory.ReserveRow(values); memoryOwner = meter.Memory; }
        return this;
    }
    internal void ReleaseMemory() { memoryOwner?.Release(memoryBytes); memoryOwner = null; memoryBytes = 0; }
    public int Count => values.Length;
    public SqlValue this[int index] => index >= 0 && index < values.Length ? values[index] : throw new SqlException(SqlStates.UndefinedColumn, "column index out of range");
}

/// <summary>A single-pass asynchronous stream with exact cost available after drain or failure.</summary>
public sealed class Rows : IAsyncEnumerable<Row>, IAsyncDisposable
{
    private readonly Session session;
    private readonly IAsyncEnumerator<Row> stream;
    private readonly IDisposable? retained;
    private readonly Meter meter;
    private readonly bool guardCost;
    private bool finished;
    private bool disposed;
    private int enumerated;
    private int active;
    public ReadOnlyCollection<Column> Columns { get; }
    public long Cost => meter.Accrued;

    /// <summary>DML/CTAS command count, or null when the statement has no count.</summary>
    public long? AffectedRows { get; }
    internal Rows(Session session, SelectPlan plan, IAsyncEnumerable<SqlValue[]> source, Meter meter,
        CancellationToken cancellationToken = default, IDisposable? retained = null)
        : this(session, plan.Projections.Select(p => new Column(p.Name, p.Expression.Type)).ToArray(),
            QueryExecution.Select(plan, source, meter, cancellationToken), meter, retained) { }

    internal Rows(Session session, Column[] columns, IAsyncEnumerable<Row> source, Meter meter, IDisposable? retained = null, long? affected = null, bool guardCost = true)
    {
        this.session = session; this.meter = meter; this.retained = retained; this.guardCost = guardCost;
        stream = source.GetAsyncEnumerator(); Columns = Array.AsReadOnly(columns); AffectedRows = affected;
    }

    public async ValueTask<Row?> ReadAsync(CancellationToken cancellationToken = default)
    {
        await ValueTask.CompletedTask.ConfigureAwait(false);
        // Admission failures must not modify a cursor another operation owns.
        using var operation = session.Enter();
        using var cursorOperation = Enter();
        ObjectDisposedException.ThrowIf(disposed, this);
        try
        {
            if (finished) return null;
            CursorCheckpoint(cancellationToken);
            if (!await stream.MoveNextAsync().ConfigureAwait(false))
            {
                finished = true; await stream.DisposeAsync().ConfigureAwait(false); await ReleaseRetainedAsync().ConfigureAwait(false);
                CursorCheckpoint(cancellationToken); return null;
            }
            CursorCheckpoint(cancellationToken);
            var result = stream.Current; result.ReleaseMemory(); return result;
        }
        catch (Exception error)
        {
            finished = true; session.FailStatement();
            try { await stream.DisposeAsync().ConfigureAwait(false); }
            finally { await ReleaseRetainedAsync().ConfigureAwait(false); }
            if (error is SqlException sql) sql.Cost = Cost;
            if (error is SqlCanceledException canceled) canceled.Cost = Cost;
            if (error is OperationCanceledException && error is not SqlCanceledException)
                throw new SqlCanceledException(cancellationToken) { Cost = Cost };
            throw;
        }
    }

    public async IAsyncEnumerator<Row> GetAsyncEnumerator(CancellationToken cancellationToken = default)
    {
        if (Interlocked.Exchange(ref enumerated, 1) != 0) throw new InvalidOperationException("cursor can only be enumerated once");
        try
        {
            while (await ReadAsync(cancellationToken).ConfigureAwait(false) is { } row) yield return row;
        }
        finally { await DisposeAsync().ConfigureAwait(false); }
    }

    private void CursorCheckpoint(CancellationToken cancellationToken)
    {
        if (guardCost) meter.Checkpoint(cancellationToken);
        else meter.CheckCancellation(cancellationToken);
    }

    public async ValueTask DisposeAsync()
    {
        if (Volatile.Read(ref disposed)) return;
        using var operation = Enter();
        disposed = true;
        try { await stream.DisposeAsync().ConfigureAwait(false); }
        finally { await ReleaseRetainedAsync().ConfigureAwait(false); }
    }

    private ValueTask ReleaseRetainedAsync()
    {
        if (retained is IAsyncDisposable asynchronous) return asynchronous.DisposeAsync();
        retained?.Dispose(); return ValueTask.CompletedTask;
    }

    private CursorOperation Enter()
    {
        if (Interlocked.CompareExchange(ref active, 1, 0) != 0)
            throw new SqlException(SqlStates.ObjectInUse, "cursor operation already in progress");
        return new CursorOperation(this);
    }
    private readonly struct CursorOperation(Rows rows) : IDisposable
    {
        public void Dispose() => Volatile.Write(ref rows.active, 0);
    }
}
