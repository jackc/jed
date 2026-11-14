package org.jed;

/** Count lexical CTE references for the canonical default materialization choice. */
final class CteReferences {
    private CteReferences() {}
    static int count(SqlParser.Statement statement, String name) {
        if (statement instanceof SqlParser.Select s) return s.from().stream().mapToInt(f -> from(f, name)).sum()
            + s.items().stream().mapToInt(i -> expr(i.expression(), name)).sum() + expr(s.where(), name) + expr(s.having(), name)
            + s.group().stream().mapToInt(e -> expr(e, name)).sum() + s.order().stream().mapToInt(o -> expr(o.expression(), name)).sum() + expr(s.limit(), name) + expr(s.offset(), name) + s.windows().stream().mapToInt(w -> window(w.spec(), name)).sum();
        if (statement instanceof SqlParser.Values v) return v.rows().stream().flatMap(java.util.Collection::stream).mapToInt(e -> expr(e, name)).sum()
            + v.order().stream().mapToInt(o -> expr(o.expression(), name)).sum() + expr(v.limit(), name) + expr(v.offset(), name);
        if (statement instanceof SqlParser.SetOperation s) return count(s.left(), name) + count(s.right(), name);
        if (statement instanceof SqlParser.QueryTail t) return count(t.query(), name) + t.order().stream().mapToInt(o -> expr(o.expression(), name)).sum() + expr(t.limit(), name) + expr(t.offset(), name);
        if (statement instanceof SqlParser.Insert i) return (i.query() == null ? 0 : count(i.query(), name)) + i.values().stream().flatMap(java.util.Collection::stream).mapToInt(e -> expr(e, name)).sum() + i.returning().stream().mapToInt(r -> expr(r.expression(), name)).sum() + (i.conflict() == null ? 0 : expr(i.conflict().predicate(), name) + expr(i.conflict().where(), name) + i.conflict().assignments().stream().mapToInt(a -> expr(a.expression(), name)).sum());
        if (statement instanceof SqlParser.Update u) return u.assignments().stream().mapToInt(a -> expr(a.expression(), name)).sum() + u.from().stream().mapToInt(f -> from(f, name)).sum() + expr(u.where(), name) + u.returning().stream().mapToInt(r -> expr(r.expression(), name)).sum();
        if (statement instanceof SqlParser.Delete d) return d.using().stream().mapToInt(f -> from(f, name)).sum() + expr(d.where(), name) + d.returning().stream().mapToInt(r -> expr(r.expression(), name)).sum();
        if (statement instanceof SqlParser.Merge m) return from(m.source(), name) + expr(m.on(), name)
            + m.clauses().stream().mapToInt(c -> expr(c.condition(), name) + c.assignments().stream().mapToInt(a -> expr(a.expression(), name)).sum() + c.values().stream().mapToInt(e -> expr(e, name)).sum()).sum()
            + m.returning().stream().mapToInt(i -> expr(i.expression(), name)).sum();
        if (statement instanceof SqlParser.With w) {
            int result = 0;
            for (SqlParser.Cte c : w.ctes()) { result += count(c.body(), name); if (c.name().equals(name)) return result; }
            return result + count(w.body(), name);
        }
        return 0;
    }
    static int from(SqlParser.From source, String name) {
        if (source instanceof SqlParser.Table t) return t.name().parts().equals(java.util.List.of(name)) ? 1 : 0;
        if (source instanceof SqlParser.Grouped g) return from(g.source(), name);
        if (source instanceof SqlParser.JsonTable table) return expr(table.context(), name) + table.passing().stream().mapToInt(value -> expr(value.value(), name)).sum() + table.columns().stream().mapToInt(column -> jsonColumn(column, name)).sum();
        if (source instanceof SqlParser.TableFunction f) return expr(f.call(), name);
        if (source instanceof SqlParser.Derived d) return count(d.query(), name);
        if (source instanceof SqlParser.Join j) return from(j.left(), name) + from(j.right(), name) + expr(j.on(), name);
        return 0;
    }
    private static int jsonColumn(SqlParser.JsonTableColumn column, String name) {
        return (column.onEmpty() == null ? 0 : expr(column.onEmpty().value(), name)) + (column.onError() == null ? 0 : expr(column.onError().value(), name))
            + column.children().stream().mapToInt(child -> jsonColumn(child, name)).sum();
    }
    private static int window(SqlParser.WindowSpec spec, String name) {
        if (spec == null) return 0;
        return spec.partition().stream().mapToInt(e -> expr(e, name)).sum() + spec.order().stream().mapToInt(o -> expr(o.expression(), name)).sum()
            + (spec.frame() == null ? 0 : expr(spec.frame().start().offset(), name) + expr(spec.frame().end().offset(), name));
    }
    private static int expr(SqlParser.Expr expression, String name) {
        if (expression instanceof SqlParser.Window w) return expr(w.function(), name) + window(w.spec(), name);
        if (expression instanceof SqlParser.Subquery q) return count(q.query(), name);
        if (expression instanceof SqlParser.Exists q) return count(q.query(), name);
        if (expression instanceof SqlParser.In q) return expr(q.subject(), name) + (q.query() == null ? q.values().stream().mapToInt(v -> expr(v, name)).sum() : count(q.query(), name));
        if (expression instanceof SqlParser.Binary b) return expr(b.left(), name) + expr(b.right(), name);
        if (expression instanceof SqlParser.Unary u) return expr(u.value(), name);
        if (expression instanceof SqlParser.Cast c) return expr(c.value(), name);
        if (expression instanceof SqlParser.JsonSql c) return c.children().stream().mapToInt(e -> expr(e, name)).sum();
        if (expression instanceof SqlParser.Extrema c) return c.args().stream().mapToInt(e -> expr(e, name)).sum();
        if (expression instanceof SqlParser.Call c) return c.args().stream().mapToInt(e -> expr(e, name)).sum() + expr(c.filter(), name) + c.order().stream().mapToInt(o -> expr(o.expression(), name)).sum();
        if (expression instanceof SqlParser.Conditional c) return expr(c.input(), name) + expr(c.otherwise(), name) + c.conditions().stream().mapToInt(e -> expr(e, name)).sum() + c.results().stream().mapToInt(e -> expr(e, name)).sum();
        if (expression instanceof SqlParser.Between b) return expr(b.subject(), name) + expr(b.lower(), name) + expr(b.upper(), name);
        if (expression instanceof SqlParser.ArrayConstructor a) return a.query() == null ? a.values().stream().mapToInt(e -> expr(e, name)).sum() : count(a.query(), name);
        if (expression instanceof SqlParser.RowConstructor r) return r.values().stream().mapToInt(e -> expr(e, name)).sum();
        if (expression instanceof SqlParser.Quantified q) return expr(q.subject(), name) + (q.query() == null ? expr(q.array(), name) : count(q.query(), name));
        if (expression instanceof SqlParser.Field f) return expr(f.base(), name);
        if (expression instanceof SqlParser.Collate c) return expr(c.value(), name);
        if (expression instanceof SqlParser.Access a) return expr(a.base(), name) + a.subscripts().stream().mapToInt(s -> expr(s.lower(), name) + expr(s.upper(), name)).sum();
        if (expression instanceof SqlParser.NamedArgument a) return expr(a.value(), name);
        return 0;
    }
}
