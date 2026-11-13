using Jed;

internal sealed partial class SimulationRunner
{
    internal static async Task<string> ReadCommitDigest(SimulationFileSystem fs, bool relaxed, bool service,
        string pending, Func<Task<string>> read)
    {
        var retried = false;
        while (true)
        {
            try { return await read(); }
            catch (SqlException error) when (error.SqlState is SqlStates.IoError or SqlStates.ConnectionFailure)
            {
                // These exceptions defer verification to the mandatory recovery
                // after the faulted step. Corruption and unrelated SQL errors
                // must never be replaced with the transaction's own digest.
                if (relaxed && fs.Fired != "none" || service && fs.IsDead) return pending;
                if (service && !retried && error.SqlState == SqlStates.ConnectionFailure)
                {
                    // A disconnect may land in read-back itself. One fresh
                    // session can re-elect; the interrupted session cannot.
                    retried = true;
                    continue;
                }
                throw;
            }
        }
    }
}
