namespace Jed;

internal sealed record SearchSyntax(bool Breadth, string[] Columns, string Name);
internal sealed record CycleSyntax(string[] Columns, string Mark, Expression Value, Expression Default, string Path);

// SEARCH/CYCLE expressions operate on the produced row followed by its parent's
// decoration cells. They use the ordinary evaluator and its allocation/cost gates.
internal sealed class RecursiveDecoration
{
    internal BoundColumn[] Columns { get; }
    internal Resolved[] Anchor { get; }
    internal Resolved[] Step { get; }
    internal Resolved? Keep { get; }
    internal int OwnCount { get; }
    private RecursiveDecoration(BoundColumn[] columns, int ownCount, Resolved[] anchor, Resolved[] step, Resolved? keep)
    { Columns = columns; OwnCount = ownCount; Anchor = anchor; Step = step; Keep = keep; }

    internal static CycleSyntax? BindMarks(CycleSyntax? cycle, Catalog catalog, Meter meter)
    {
        if (cycle is null) return null;
        var marked = Resolve.Expr(Resolve.BindTypes(cycle.Value, catalog, meter.Temporal));
        var normal = Resolve.Expr(Resolve.BindTypes(cycle.Default, catalog, meter.Temporal));
        if (cycle.Value.Op == "text" && normal.Type != SqlType.Unknown && normal.Type != SqlType.Text)
            marked = new("constant", normal.Type, Resolve.ParseText(cycle.Value.Text, normal.Type), []);
        if (cycle.Default.Op == "text" && marked.Type != SqlType.Unknown && marked.Type != SqlType.Text)
            normal = new("constant", marked.Type, Resolve.ParseText(cycle.Default.Text, marked.Type), []);
        var type = Resolve.Common(marked.Type, normal.Type); if (type == SqlType.Unknown) type = SqlType.Text;
        Expression Bound(SqlValue value) => new("bound", "", [], Binding: new("constant", type, Resolve.Cast(value, type), []));
        using (meter.SuppressCost())
        {
            var value = Bound(Evaluate.Expr(marked, meter)); var fallback = Bound(Evaluate.Expr(normal, meter));
            _ = Resolve.Expr(new("=", "", [value, fallback]));
            return cycle with { Value = value, Default = fallback };
        }
    }
    internal static RecursiveDecoration Create(BoundColumn[] own, SearchSyntax? search, CycleSyntax? cycle)
    {
        var columns = own.ToList(); var anchor = new List<Resolved>(); var step = new List<Resolved>(); Resolved? keep = null;
        var names = own.Select(column => column.Name).ToHashSet(StringComparer.Ordinal);
        void Add(string name, Resolved initial, Resolved next)
        {
            if (!names.Add(name)) throw Errors.Syntax("recursive decoration column name collides");
            columns.Add(new(name, initial.Type)); anchor.Add(initial); step.Add(next);
        }
        static Expression Bound(Resolved value) => new("bound", "", [], Binding: value);
        static Resolved Column(int slot, SqlType type) => new("column", type, default, [], Slot: slot);
        static Resolved Constant(SqlValue value) => new("constant", value.Type, value, []);
        static Resolved ResolveNode(string op, string text, params Resolved[] args) => Resolve.Expr(new(op, text, args.Select(Bound).ToArray()));
        Resolved Row(string[] selected, Resolved? first = null)
        {
            if (selected.Distinct(StringComparer.Ordinal).Count() != selected.Length) throw new SqlException(SqlStates.DuplicateColumn, "duplicate recursive decoration column");
            var members = new List<Resolved>(); if (first is not null) members.Add(first);
            foreach (var name in selected)
            {
                var slot = Array.FindIndex(own, column => column.Name == name);
                if (slot < 0) throw Errors.Syntax("recursive decoration column does not exist");
                members.Add(Column(slot, own[slot].Type));
            }
            return ResolveNode("row", "", members.ToArray());
        }
        if (search is not null)
        {
            if (search.Breadth)
            {
                var initial = Row(search.Columns, Constant(SqlValue.Integer(0, SqlType.I64)));
                var parent = Column(columns.Count, initial.Type);
                var depth = ResolveNode("field", "f1", parent);
                Add(search.Name, initial, Row(search.Columns, ResolveNode("+", "", depth, Constant(SqlValue.Integer(1, SqlType.I64)))));
            }
            else
            {
                var path = ResolveNode("array", "", Row(search.Columns));
                Add(search.Name, path, ResolveNode("||", "", Column(columns.Count, path.Type), path));
            }
        }
        if (cycle is not null)
        {
            var marked = Resolve.Expr(cycle.Value); var normal = Resolve.Expr(cycle.Default);
            if (cycle.Value.Op == "text" && normal.Type != SqlType.Unknown && normal.Type != SqlType.Text)
                marked = Constant(Resolve.ParseText(cycle.Value.Text, normal.Type));
            if (cycle.Default.Op == "text" && marked.Type != SqlType.Unknown && marked.Type != SqlType.Text)
                normal = Constant(Resolve.ParseText(cycle.Default.Text, marked.Type));
            var type = Resolve.Common(marked.Type, normal.Type); if (type == SqlType.Unknown) type = SqlType.Text;
            marked = Constant(Resolve.Cast(marked.Constant, type)); normal = Constant(Resolve.Cast(normal.Constant, type));
            // Resolve equality even when no rows are produced.
            _ = ResolveNode("=", "", marked, normal);
            var parentMark = Column(columns.Count, type);
            var row = Row(cycle.Columns); var path = ResolveNode("array", "", row);
            var parentPath = Column(columns.Count + 1, path.Type);
            var repeated = ResolveNode("quantified", "= any", row, parentPath);
            var nextMark = new Resolved("case", type, default, [repeated, marked, normal]);
            keep = ResolveNode("<>", "", parentMark, marked);
            Add(cycle.Mark, normal, nextMark);
            Add(cycle.Path, path, ResolveNode("||", "", parentPath, path));
        }
        return new(columns.ToArray(), own.Length, anchor.ToArray(), step.ToArray(), keep);
    }
}
