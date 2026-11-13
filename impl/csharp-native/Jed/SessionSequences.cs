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
            meter.Sequences.Bind(pending.Catalog, pending);
            if (parameters.Any) parameters.Infer(select, db, pending.Before, pending, pending.Catalog, meter, token);
            parameters.Bind(parameterValues, meter.Temporal);
            var query = RelationExecution.Query(select, db, pending.Before, pending, pending.Catalog, meter, token);
            if (query.Initialize is not null) await query.Initialize().ConfigureAwait(false);
            var rows = new List<Row>();
            await foreach (var values in query.Rows.WithCancellation(token).ConfigureAwait(false)) rows.Add(new(values));
            meter.Checkpoint();
            if (automatic) await db.CommitAsync(pending, token).ConfigureAwait(false);
            else transaction = pending;
            sequenceState = meter.Sequences.State;
            return new(this, query.Columns.Select(column => new Column(column.Name, column.Type)).ToArray(), QueryExecution.Materialized(rows.ToArray()), meter);
        }
        finally { if (automatic) await db.EndWriteAsync(pending).ConfigureAwait(false); }
    }
}
