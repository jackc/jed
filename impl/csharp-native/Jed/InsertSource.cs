namespace Jed;

internal static partial class StatementExecution
{
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
