namespace Jed;

// Runtime domains never enter catalog serialization. Each owns its own page
// namespace and pending COW trees; a statement scope only binds names to them.
internal sealed record DatabaseDomain(string Name, Database Database, DatabaseSnapshot Snapshot,
    PendingDatabase? Pending = null, bool ReadOnly = false)
{
    internal Catalog Catalog => Pending?.Catalog ?? Snapshot.Catalog;
}

internal sealed class DatabaseDomains
{
    private readonly Dictionary<string, DatabaseDomain> domains;
    private string? storedDomain;
    internal DatabaseDomains(IEnumerable<DatabaseDomain> domains) => this.domains = domains.ToDictionary(domain => domain.Name, StringComparer.Ordinal);
    internal IReadOnlyCollection<DatabaseDomain> Items => domains.Values;
    internal bool TryGet(string name, out DatabaseDomain domain) => domains.TryGetValue(name, out domain!);
    internal DatabaseDomain Get(string name) => domains.TryGetValue(name, out var domain) ? domain :
        throw new SqlException(SqlStates.UndefinedTable, $"database is not attached: {name}");
    internal void Replace(DatabaseDomain domain) => domains[domain.Name] = domain;
    internal Catalog BindCatalog(DatabaseDomain domain) => domain.Catalog with { Scope = this, TypeParent = domain.Name == "temp" ? Get("main").Catalog : null };
    internal DatabaseDomains Fork()
    {
        var fork = new DatabaseDomains(domains.Values.Select(domain => domain with { Pending = domain.Pending?.Fork() })) { storedDomain = storedDomain };
        foreach (var domain in fork.Items) if (domain.Pending is { } pending) pending.Catalog = pending.Catalog with { Scope = fork };
        return fork;
    }
    internal DatabaseDomains Stored(string domain) => new(domains.Values) { storedDomain = domain == "temp" ? null : domain };
    internal static (string? Domain, string Local) Name(string name)
    {
        var separator = name.IndexOf('\0');
        return separator < 0 ? (null, name) : (name[..separator], name[(separator + 1)..]);
    }
    internal static string Qualified(string domain, string local) => domain + "\0" + local;
    internal DatabaseDomain Resolve(string name, bool creation = false, bool temporary = false)
    {
        var parts = Name(name);
        if (parts.Domain is { } qualifier)
        {
            if (temporary && qualifier != "temp") throw new SqlException(SqlStates.InvalidTableDefinition, "temporary table requires the temp domain");
            return Get(qualifier);
        }
        if (temporary) return Get("temp");
        if (storedDomain is { } home) return Get(home);
        if (!creation && domains.TryGetValue("temp", out var temp) && HasRelation(temp.Catalog, parts.Local)) return temp;
        return Get("main");
    }
    private static bool HasRelation(Catalog catalog, string name) => catalog.Tables.Any(table => table.Name == name || (table.Indexes ?? []).Any(index => index.Name == name)) ||
        (catalog.Sequences ?? []).Any(sequence => sequence.Name == name) || (catalog.Views ?? []).Any(view => view.Name == name);
    internal CatalogTable Table(string name)
    {
        var domain = Resolve(name); var local = Name(name).Local;
        var table = domain.Catalog.Tables.FirstOrDefault(table => table.Name == local) ?? throw new SqlException(SqlStates.UndefinedTable, $"table does not exist: {local}");
        return table with { Domain = domain, Indexes = table.Indexes?.Select(index => index with { Domain = domain }).ToArray() };
    }
    internal (CatalogView View, DatabaseDomain Domain)? View(string name)
    {
        var domain = Resolve(name); var local = Name(name).Local;
        return (domain.Catalog.Views ?? []).FirstOrDefault(view => view.Name == local) is { } view ? (view, domain) : null;
    }
}
