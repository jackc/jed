namespace Jed;

internal static partial class StatementExecution
{
    internal static SelectSyntax InsertQuery(InsertSyntax insert, CatalogTable table, Catalog catalog, Meter meter)
    {
        var slots = insert.Columns is null ? Enumerable.Range(0, table.Columns.Length).ToArray() : insert.Columns.Select(name => ColumnIndex(table, name)).ToArray();
        var query = Resolve.BindTypes(insert.Source ?? throw new InvalidOperationException("INSERT has no query source"), catalog, meter.Temporal);
        return AdaptInsertSource(query, slots.Select(slot => table.Columns[slot].Type).ToArray(), catalog, meter);
    }

    private static SelectSyntax AdaptInsertSource(SelectSyntax query, TypeSpec[] targets, Catalog catalog, Meter meter)
    {
        if (query.Distinct || query.ValuesQuery || query.SetQuery || query.Items.Length != targets.Length || query.Items.Any(item => item.Expression.Op == "star")) return query;
        bool UsedAsKey(Expression key, int index)
        {
            if (key.Op == "integer" && long.TryParse(key.Text, out var ordinal) && ordinal == index + 1) return true;
            if (key.Op == "name" && query.Items[index].Alias == key.Text) return true;
            return key.Op == "text" && key.Text == query.Items[index].Expression.Text;
        }
        var items = query.Items.ToArray();
        for (var i = 0; i < items.Length; i++)
        {
            var item = items[i];
            if (item.Expression.Op != "text" || targets[i].Type.Kind is not (SqlKind.Uuid or SqlKind.Bytea or SqlKind.Date or SqlKind.Timestamp or SqlKind.Timestamptz or SqlKind.Interval or SqlKind.Composite or SqlKind.Array or SqlKind.Range or SqlKind.Json or SqlKind.Jsonb)) continue;
            if ((query.Order ?? []).Any(order => UsedAsKey(order.Expression, i)) || (query.GroupBy ?? []).Any(key => UsedAsKey(key, i))) continue;
            if (targets[i].Type == SqlType.Date && TemporalText.IsClockDate(item.Expression.Text))
                _ = TemporalInput.ParseDate(item.Expression.Text);
            var bound = Resolve.Expr(Resolve.BindTypes(item.Expression, catalog, meter.Temporal), targets[i].Type);
            items[i] = item with { Expression = new("bound", "", [], Binding: bound) };
        }
        return query with { Items = items };
    }
}
