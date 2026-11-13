namespace Jed;

internal sealed record CreateTableAsSyntax(string Name, string[]? Columns, SelectSyntax Query, bool IfNotExists, bool WithData, bool Temporary = false, byte OnCommit = 0) : StatementSyntax;

internal sealed partial class Parser
{
    private CreateTableAsSyntax TableAs(string name, string[]? columns, bool ifNotExists, bool temporary = false)
    {
        var onCommit = OnCommit(temporary);
        Expect("as"); var tokens = new List<Token>();
        while (token.Kind != TokenKind.End && !Is(";")) tokens.Add(Take());
        var withData = true;
        if (tokens.Count >= 3 && tokens[^3].Text == "with" && tokens[^2].Text == "no" && tokens[^1].Text == "data")
        { withData = false; tokens.RemoveRange(tokens.Count - 3, 3); }
        else if (tokens.Count >= 2 && tokens[^2].Text == "with" && tokens[^1].Text == "data") tokens.RemoveRange(tokens.Count - 2, 2);
        var statement = new Parser(string.Join(' ', tokens.Select(RenderToken))).ParseStatement();
        if (statement is not SelectSyntax query) throw Errors.Syntax("CREATE TABLE AS requires a query");
        return new(name, columns, query, ifNotExists, withData, temporary, onCommit);
    }
}

internal static partial class StatementExecution
{
    internal static void ValidateCreateTableAsName(CreateTableAsSyntax create, Catalog catalog)
    {
        var local = DatabaseDomains.Name(create.Name).Local;
        if (catalog.Scope is { } domains) _ = DomainExecution.ResolveTarget(create, domains);
        if (local.StartsWith("jed_", StringComparison.Ordinal)) throw new SqlException(SqlStates.ReservedName, "reserved table name");
    }
    internal static bool CreateTableAsExists(CreateTableAsSyntax create, Catalog catalog)
    {
        var name = DatabaseDomains.Name(create.Name).Local;
        var target = catalog.Scope is { } domains ? DomainExecution.ResolveTarget(create, domains).Catalog : catalog;
        var exists = RelationExists(target, name);
        if (catalog.Scope is { } scope && DomainExecution.ResolveTarget(create, scope).Name is "main" or "temp")
            exists |= scope.Items.Where(domain => domain.Name is "main" or "temp").Any(domain => RelationExists(domain.Catalog, name));
        if (exists && !create.IfNotExists) throw new SqlException(SqlStates.DuplicateTable, "relation already exists");
        return exists;
    }

    private static TypeSpec QueryColumnType(SelectSyntax query, BoundColumn column, int index, Catalog catalog)
    {
        var columns = QueryTypeColumns(query, catalog);
        return index < columns.Length && columns[index].Type is { } declared ? declared : new(column.Type == SqlType.Unknown ? SqlType.Text : column.Type);
    }

    private static async ValueTask<StatementResult> CreateTableAs(Database database, PendingDatabase pending, CreateTableAsSyntax create, Meter meter, CancellationToken token)
    {
        if (create.Name.StartsWith("jed_", StringComparison.Ordinal)) throw new SqlException(SqlStates.ReservedName, "reserved table name");
        PreparedRelation query;
        if (WritableCteExecution.HasWrites(create.Query))
        {
            var plan = WritableCteExecution.Prepare(database, pending, create.Query.With!, create.Query.Recursive, create.Query with { With = null }, meter, token);
            query = new(plan.Columns, WritableCteExecution.Rows(plan));
        }
        else query = RelationExecution.Query(create.Query, database, pending.Before, pending, pending.Catalog, meter, token);
        if (RelationExists(pending.Catalog, create.Name))
        {
            if (create.IfNotExists) return new([], []);
            throw new SqlException(SqlStates.DuplicateTable, "relation already exists");
        }
        if (create.OnCommit >= 128) throw new SqlException(SqlStates.InvalidTableDefinition, "ON COMMIT requires a temporary table");
        if ((create.Columns?.Length ?? 0) > query.Columns.Length) throw Errors.Syntax("too many target column names");
        var columns = query.Columns.Select((column, i) => new CatalogColumn(create.Columns is { } names && i < names.Length ? names[i] : column.Name,
            QueryColumnType(create.Query, column, i, pending.Catalog))).ToArray();
        for (var i = 0; i < columns.Length; i++)
        {
            if (CollationRules.Collatable(columns[i].Type.Type)) CollationRules.Require(query.Columns[i].Collation);
            columns[i] = ColumnCollation(pending, columns[i], query.Columns[i].Collation?.Name, meter);
        }
        if (columns.Select(column => column.Name).Distinct(StringComparer.Ordinal).Count() != columns.Length) throw new SqlException(SqlStates.DuplicateColumn, "duplicate created column");
        foreach (var column in columns)
        {
            var type = column.Type.Type;
            if (type.Identity == "record" || type.Element?.Identity == "record") throw new SqlException(SqlStates.InvalidTableDefinition, "anonymous record cannot be a table column");
            if (type.Kind == SqlKind.Jsonpath || type.Kind == SqlKind.Array && type.Element!.Kind is SqlKind.Array or SqlKind.Range) throw Errors.Unsupported("table column type");
        }
        var table = new CatalogTable(create.Name, columns, []);
        Put(pending, table);
        if (!create.WithData) return new([], []);
        var tree = database.Tree(table, pending, pending.Before); long count = 0;
        if (query.Initialize is not null) await query.Initialize().ConfigureAwait(false);
        await foreach (var sourceRow in query.Rows.WithCancellation(token).ConfigureAwait(false))
        {
            var row = sourceRow.ToArray();
            for (var i = 0; i < row.Length; i++) row[i] = Resolve.ApplyModifier(Resolve.Cast(row[i], columns[i].Type.Type), columns[i].Type);
            tree = await tree.UpsertAsync(tree.PrepareRecord(table.EncodeRow(row, count), meter), token).ConfigureAwait(false); count = checked(count + 1);
        }
        Put(pending, table with { RowCount = count }, tree); return new([], [], count);
    }
}
