namespace Jed;

public sealed partial class Session
{
    private async ValueTask TransactionControl(TransactionSyntax control, CancellationToken token)
    {
        if (transactionFailed && control.Operation is not ("commit" or "rollback" or "rollback_to"))
            throw new SqlException(SqlStates.InFailedSqlTransaction, "transaction is aborted");
        if (control.Operation == "begin")
        {
            if (TransactionOpen) throw new SqlException(SqlStates.ActiveSqlTransaction, "transaction is already active");
            var db = await GetDatabase(token).ConfigureAwait(false);
            if (control.ReadOnly) readTransaction = await db.PinAsync(token).ConfigureAwait(false);
            else transaction = await db.BeginWriteAsync(token).ConfigureAwait(false);
            sequenceBeforeTransaction = sequenceState.Fork();
            return;
        }
        if (control.Operation is "commit" or "rollback")
        {
            var pending = transaction; var read = readTransaction;
            var rollback = control.Operation == "rollback" || transactionFailed;
            transaction = null; readTransaction = null;
            try
            {
                if (pending is not null && control.Operation == "commit" && !transactionFailed)
                    await database!.CommitAsync(pending, token).ConfigureAwait(false);
            }
            catch { rollback = true; throw; }
            finally
            {
                if (rollback && sequenceBeforeTransaction is not null) sequenceState = sequenceBeforeTransaction;
                sequenceBeforeTransaction = null;
                transactionFailed = false; savepoints.Clear();
                if (pending is not null) await database!.EndWriteAsync(pending).ConfigureAwait(false);
                if (read is not null) await read.DisposeAsync().ConfigureAwait(false);
            }
            return;
        }
        if (!TransactionOpen) throw new SqlException(SqlStates.NoActiveSqlTransaction, "savepoint requires a transaction");
        if (control.Operation == "savepoint")
        {
            savepoints.Add(new(control.Name!, transaction?.Fork(), sequenceState.Fork())); return;
        }
        var index = savepoints.FindLastIndex(frame => frame.Name == control.Name);
        if (index < 0) throw new SqlException(SqlStates.InvalidSavepointSpecification, "savepoint does not exist");
        if (control.Operation == "rollback_to")
        {
            transaction = savepoints[index].Pending?.Fork(); sequenceState = savepoints[index].Sequences.Fork(); transactionFailed = false;
            savepoints.RemoveRange(index + 1, savepoints.Count - index - 1);
        }
        else savepoints.RemoveRange(index, savepoints.Count - index);
    }
}
