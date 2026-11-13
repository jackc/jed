namespace Jed;

public sealed partial class Session
{
    private async ValueTask<Rows> SequenceQuery(SelectSyntax select, Meter meter, CancellationToken token, Parameters parameters, IReadOnlyList<SqlValue> parameterValues)
    {
        if (readTransaction is not null) throw new SqlException(SqlStates.ReadOnlySqlTransaction, "sequence mutation in read-only transaction");
        var db = await GetDatabase(token).ConfigureAwait(false);
        var automatic = transaction is null;
        var pending = automatic ? await db.BeginWriteAsync(token).ConfigureAwait(false) : transaction!.Fork();
        try
        {
            pending.TrackMemory = meter.Memory.Enabled;
            pending.Catalog = pending.Catalog.BindCollations(meter.Unicode, meter.Temporal.Data, meter.Temporal.Environment);
            meter.Sequences.Bind(pending.Catalog, pending);
            if (parameters.Any) parameters.Infer(select, db, pending.Before, pending, pending.Catalog, meter, token);
            parameters.Bind(parameterValues, meter.Temporal);
            if (WritableCteExecution.HasWrites(select))
            {
                var result = await WritableCteExecution.Prepare(db, pending, select.With!, select.Recursive, select with { With = null }, meter, token).Run().ConfigureAwait(false);
                meter.Memory.HoldPending(pending.PendingBytes);
                meter.Checkpoint();
                if (automatic) { await db.CommitAsync(pending, token).ConfigureAwait(false); meter.Memory.HoldPending(0); } else transaction = pending;
                sequenceState = meter.Sequences.State;
                return new(this, result.Columns, QueryExecution.Materialized(result.Rows), meter, affected: result.Affected);
            }
            var query = RelationExecution.Query(select, db, pending.Before, pending, pending.Catalog, meter, token);
            if (query.Initialize is not null) await query.Initialize().ConfigureAwait(false);
            var rows = new List<Row>();
            await foreach (var values in query.Rows.WithCancellation(token).ConfigureAwait(false)) rows.Add(new Row(values).RetainMemory(meter, query.MemoryOutput));
            meter.Memory.HoldPending(pending.PendingBytes);
            meter.Checkpoint();
            if (automatic) { await db.CommitAsync(pending, token).ConfigureAwait(false); meter.Memory.HoldPending(0); }
            else transaction = pending;
            sequenceState = meter.Sequences.State;
            return new(this, query.Columns.Select(column => new Column(column.Name, column.Type)).ToArray(), QueryExecution.Materialized(rows.ToArray()), meter);
        }
        finally { if (automatic) await db.EndWriteAsync(pending).ConfigureAwait(false); }
    }
}
