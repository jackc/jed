namespace Jed;

// A WITH statement owns one immutable read view while its parts stage into the
// transaction. Scope is explicit and foreign-key checks settle after the primary.
internal sealed class StatementContext
{
    internal PendingDatabase Read { get; private set; }
    internal QueryScope Scope { get; }
    internal PreparedRelation? PreparedQuery { get; init; }
    internal MutationObservation? MutationObservation { get; init; }
    internal List<Func<ValueTask>> ForeignChecks { get; }
    private string domainName = "main";
    private readonly Dictionary<(string Table, string Key), byte[]?> currentKeys;
    internal StatementContext(PendingDatabase read, QueryScope scope)
    {
        Read = read; Scope = scope; ForeignChecks = []; currentKeys = [];
    }
    private StatementContext(StatementContext parent, QueryScope scope)
    {
        Read = parent.Read; domainName = parent.domainName; Scope = scope; ForeignChecks = parent.ForeignChecks; currentKeys = parent.currentKeys; PreparedQuery = parent.PreparedQuery; MutationObservation = parent.MutationObservation;
    }
    internal StatementContext ForDomain(string name)
    {
        var result = new StatementContext(this, Scope) { domainName = name };
        if (Read.Catalog.Scope is { } domains) result.Read = DomainExecution.ReadPending(domains, name);
        return result;
    }
    internal StatementContext InScope(QueryScope scope) => new(this, scope);
    internal StatementContext ForMutation(PreparedRelation? query, MutationObservation? observation) =>
        new(this, Scope) { PreparedQuery = query, MutationObservation = observation };
    internal byte[]? CurrentKey(string table, byte[] original) => currentKeys.TryGetValue((domainName + "\0" + table, Convert.ToHexString(original)), out var current) ? current : original;
    internal void Move(string table, byte[] original, byte[]? current) => currentKeys[(domainName + "\0" + table, Convert.ToHexString(original))] = current;
    internal async ValueTask Settle()
    {
        foreach (var check in ForeignChecks) await check().ConfigureAwait(false);
        ForeignChecks.Clear();
    }
}
