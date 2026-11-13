namespace Jed;

internal sealed partial record Catalog
{
    internal Catalog? TypeParent { get; init; }
    internal DatabaseDomains? Scope { get; init; }
    internal UnicodeData? BoundUnicode { get; init; }
    internal TimeZoneData? BoundTimeZones { get; init; }
    internal DatabaseEnvironment? BoundEnvironment { get; init; }
    internal Catalog BindCollations(UnicodeData? unicode, TimeZoneData? timeZones = null, DatabaseEnvironment? environment = null)
    {
        var loaded = new Dictionary<string, (Collation Data, bool Skew)>(StringComparer.Ordinal);
        foreach (var reference in Collations ?? [])
        {
            if (reference.Name == "C") continue;
            Collation data;
            try { data = unicode?.Resolve(reference.Name) ?? throw new SqlException(SqlStates.UndefinedObject, "Unicode data is not loaded"); }
            catch (SqlException error) when (error.SqlState == SqlStates.UndefinedObject)
            { throw new SqlException(SqlStates.CollationVersionMismatch, "referenced collation is unavailable: " + reference.Name); }
            loaded.Add(reference.Name, (data, data.UnicodeVersion != reference.UnicodeVersion || data.CldrVersion != reference.CldrVersion));
        }
        if (loaded.Count == 0) return this with { BoundUnicode = unicode, BoundTimeZones = timeZones, BoundEnvironment = environment ?? BoundEnvironment };
        var bound = this with { BoundUnicode = unicode, BoundTimeZones = timeZones, BoundEnvironment = environment ?? BoundEnvironment, Tables = Tables.Select(table => table with { Columns = table.Columns.Select(column =>
            column.Collation is { } name && name != "C" && loaded.TryGetValue(name, out var item)
                ? column with { CollationData = item.Data, CollationSkew = item.Skew } : column).ToArray() }).ToArray() };
        CatalogValidation.Statistics(bound.Statistics ?? [], bound.Tables.ToDictionary(table => table.Name, StringComparer.Ordinal));
        return bound;

    }
}

internal static partial class StatementExecution
{
    private static void WritableCollations(CatalogTable table, Catalog catalog)
    {
        if (table.Columns.Any(column => column.CollationSkew) || (table.Indexes ?? []).Any(index => index.Keys.Any(key => key.CollationSkew)))
            throw new SqlException(SqlStates.CollationVersionMismatch, "table collation version differs from the loaded data");
        var context = new TemporalContext(static () => throw new InvalidOperationException("stored index cannot read clock"), "UTC", data: catalog.BoundTimeZones, unicode: catalog.BoundUnicode, environment: catalog.BoundEnvironment);
        void Check(Resolved expression)
        {
            if (expression.Collation?.Table is { } data && (catalog.Collations ?? []).FirstOrDefault(reference => reference.Name == data.Name) is { } reference &&
                (reference.UnicodeVersion != data.UnicodeVersion || reference.CldrVersion != data.CldrVersion))
                throw new SqlException(SqlStates.CollationVersionMismatch, "index collation version differs from the loaded data");
            foreach (var argument in expression.Args) Check(argument);
        }
        foreach (var index in table.Indexes ?? []) foreach (var key in index.Keys)
            if (key.Expression is { } text) Check(Resolve.Expr(Resolve.Bind(Resolve.BindTypes(StoredExpression(text), catalog, context), Columns(table))));
    }
    private static CatalogColumn ColumnCollation(PendingDatabase pending, CatalogColumn column, string? explicitName, Meter meter)
    {
        if (!CollationRules.Collatable(column.Type.Type))
        {
            if (explicitName is not null) throw Errors.Type("COLLATE requires text or text[]");
            return column;
        }
        var name = explicitName ?? pending.Catalog.Collations?.FirstOrDefault(reference => reference.IsDefault)?.Name ?? "C";
        var data = ReferenceCollation(pending, name, meter.Unicode);
        return column with { Collation = name == "C" ? null : name, CollationData = data };
    }
    internal static Collation? ReferenceCollation(PendingDatabase pending, string name, UnicodeData? unicode)
    {
        if (name == "C") return null;
        var data = unicode?.Resolve(name) ?? throw new SqlException(SqlStates.UndefinedObject, "collation is not loaded: " + name);
        var existing = (pending.Catalog.Collations ?? []).FirstOrDefault(reference => reference.Name == name);
        if (existing is not null)
        {
            if (existing.UnicodeVersion != data.UnicodeVersion || existing.CldrVersion != data.CldrVersion)
                throw new SqlException(SqlStates.CollationVersionMismatch, "collation version differs from the database pin");
        }
        else
        {
            pending.Catalog = pending.Catalog with { Collations = (pending.Catalog.Collations ?? []).Append(new(name, data.UnicodeVersion, data.CldrVersion, data.Description)).ToArray() };
            pending.Changed = true;
        }
        return data;
    }
}
