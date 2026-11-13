namespace Jed;

internal static partial class Resolve
{
    private static Expression? WholeRow(Expression expression, BoundColumn[] columns)
    {
        var members = columns.Select((column, slot) => (column, slot)).Where(item => item.column.Qualifier == expression.Text && item.column.WholeRow).ToArray();
        if (members.Length == 0) return null;
        var level = members.Min(item => item.column.Level); members = members.Where(item => item.column.Level == level).ToArray();
        var fields = new List<Expression>();
        foreach (var (column, slot) in members)
        {
            var bound = column.Projection ?? (column.Outer is { } outer ? new Resolved("outer", column.Type, default, [], Outer: outer, Collation: column.Collation) : new("column", column.Type, default, [], Slot: slot, Collation: column.Collation));
            CaptureOuter(bound); fields.Add(new("bound", column.Name, [], Binding: bound));
        }
        if (members.Length == 1 && members[0].column.ScalarRelation) return fields[0] with { Text = expression.Text };
        var type = SqlType.Record(members.Select(item => new SqlField(item.column.RowName ?? item.column.Name, item.column.Type)));
        var row = new Expression("whole_row", expression.Text, fields.ToArray(), Binding: new("row", type, default, []));
        if (!members.Any(item => item.column.NullExtended)) return row;
        var sentinel = Array.FindIndex(members, item => item.column.NotNull);
        if (sentinel < 0) throw Errors.Unsupported("whole-row reference to nullable relation without a NOT NULL column");
        return new("case", expression.Text, [new("is_null", "", [fields[sentinel]]), new("null", "", []), row]);
    }
}
