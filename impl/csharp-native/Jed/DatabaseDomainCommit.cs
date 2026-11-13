namespace Jed;

public sealed partial class Database
{
    // Writer admission is acquired by the domain lease. Every plan is checked
    // before the first store write; only the durable writer may publish early.
    internal static async Task CommitDomainsAsync(DatabaseDomains scope, CancellationToken cancellationToken = default)
    {
        var changed = scope.Items.Where(domain => domain.Pending?.Changed == true)
            .OrderBy(domain => domain.Name, Utf8NameComparer.Instance).ToArray();
        if (changed.Length == 0) return;
        if (changed.Count(domain => !domain.Database.ephemeral) > 1)
            throw new SqlException(SqlStates.FeatureNotSupported, "a transaction may modify at most one durable database");
        if (changed.Any(domain => domain.ReadOnly))
            throw new SqlException(SqlStates.ReadOnlySqlTransaction, "cannot modify a read-only database");
        var plans = new List<(DatabaseDomain Domain, PreparedCommit Prepared)>();
        foreach (var domain in changed)
        {
            if (domain.Name == "temp") domain.Pending!.Catalog = scope.BindCatalog(domain);
            plans.Add((domain, await domain.Database.PrepareCommitAsync(domain.Pending!, domain.Name, cancellationToken, durability: true).ConfigureAwait(false)));
        }
        // These writes target only reusable or newly allocated pages. No root,
        // high-water, free list or storage accounting changes until all succeed.
        foreach (var item in plans.Where(item => item.Domain.Database.ephemeral))
            await item.Domain.Database.committer.StageMemoryAsync(item.Prepared.Plan, cancellationToken).ConfigureAwait(false);
        var publicationGate = scope.Get("main").Database.DomainPublication;
        await publicationGate.WaitAsync(cancellationToken).ConfigureAwait(false);
        IDisposable? publication = null;
        try
        {
            var durable = plans.FirstOrDefault(item => !item.Domain.Database.ephemeral);
            if (durable.Domain is not null)
                publication = await durable.Domain.Database.CommitFileOrMemoryAsync(durable.Prepared.Pending, durable.Prepared.Plan, cancellationToken).ConfigureAwait(false);
            // No cancellation or fallible storage work after the durable point.
            foreach (var item in plans)
            {
                var database = item.Domain.Database;
                if (database.ephemeral) database.committer.AdoptStagedMemory(item.Prepared.Plan);
                if (item.Prepared.Pending.FileWrite?.ResolvedAfterRoleChange != true) database.AdoptPreparedCommit(item.Prepared);
            }
        }
        finally { publication?.Dispose(); publicationGate.Release(); }
    }
}
