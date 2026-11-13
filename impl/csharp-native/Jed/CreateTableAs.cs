namespace Jed;

internal sealed record CreateTableAsSyntax(string Name, string[]? Columns, SelectSyntax Query, bool IfNotExists, bool WithData) : StatementSyntax;

internal sealed partial class Parser
{
    private CreateTableAsSyntax TableAs(string name, string[]? columns, bool ifNotExists)
    {
        Expect("as"); var tokens = new List<Token>();
        while (token.Kind != TokenKind.End && !Is(";")) tokens.Add(Take());
        var withData = true;
        if (tokens.Count >= 3 && tokens[^3].Text == "with" && tokens[^2].Text == "no" && tokens[^1].Text == "data")
        { withData = false; tokens.RemoveRange(tokens.Count - 3, 3); }
        else if (tokens.Count >= 2 && tokens[^2].Text == "with" && tokens[^1].Text == "data") tokens.RemoveRange(tokens.Count - 2, 2);
        var statement = new Parser(string.Join(' ', tokens.Select(RenderToken))).ParseStatement();
        if (statement is not SelectSyntax query) throw Errors.Syntax("CREATE TABLE AS requires a query");
        return new(name, columns, query, ifNotExists, withData);
    }
}

internal static partial class StatementExecution
{
    private static TypeSpec QueryColumnType(SelectSyntax query, BoundColumn column, int index, Catalog catalog)
    {
        var source = query.Table is { } sourceName ? catalog.Tables.FirstOrDefault(table => table.Name == sourceName) : null;
        if (query.Items.Length == 1 && query.Items[0].Expression.Op == "star" && source is not null && index < source.Columns.Length)
            return source.Columns[index].Type;
        if (query.Items.Length > index)
        {
            var expression = query.Items[index].Expression;
            if (expression.Op == "cast") return expression.Type ?? SqlTypes.ParseSpec(expression.Text, catalog.NamedType);
            if (expression.Op == "name" && source is not null)
            {
                var name = expression.Text.Split('.')[^1];
                if (source.Columns.FirstOrDefault(item => item.Name == name) is { } declared) return declared.Type;
            }
        }
        return new(column.Type == SqlType.Unknown ? SqlType.Text : column.Type);
    }
    private static async ValueTask<StatementResult> CreateTableAs(Database database, PendingDatabase pending, CreateTableAsSyntax create, Meter meter, CancellationToken token)
    {
        if (create.Name.StartsWith("jed_", StringComparison.Ordinal)) throw new SqlException(SqlStates.ReservedName, "reserved table name");
        var query = RelationExecution.Query(create.Query, database, pending.Before, pending, pending.Catalog, meter, token);
        if (RelationExists(pending.Catalog, create.Name))
        {
            if (create.IfNotExists) return new([], []);
            throw new SqlException(SqlStates.DuplicateTable, "relation already exists");
        }
        if ((create.Columns?.Length ?? 0) > query.Columns.Length) throw Errors.Syntax("too many target column names");
        var columns = query.Columns.Select((column, i) => new CatalogColumn(create.Columns is { } names && i < names.Length ? names[i] : column.Name,
            QueryColumnType(create.Query, column, i, pending.Catalog))).ToArray();
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
