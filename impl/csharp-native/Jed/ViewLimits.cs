namespace Jed;

// Structural expansion is independent of optimizer revisits and execution cost.
internal static class ViewLimits
{
    internal static void Validate(SelectSyntax query, Catalog catalog, QueryScope? scope = null, int baseDepth = 0)
    {
        var parsed = new Dictionary<CatalogView, SelectSyntax>();
        var active = new HashSet<CatalogView>();
        int Query(SelectSyntax body, Catalog home, HashSet<string> inherited, int depth, bool insideView)
        {
            if (depth + body.ParseDepth > 256) throw new SqlException(SqlStates.StatementTooComplex, "expanded view nesting exceeds 256");
            var ctes = new HashSet<string>(inherited, StringComparer.Ordinal); var count = 0;
            foreach (var cte in body.With ?? [])
            {
                if (body.Recursive) ctes.Add(cte.Name);
                count += Query(cte.Query, home, ctes, depth, insideView); ctes.Add(cte.Name);
            }
            void Expression(Expression expression)
            {
                if (expression.Query is { } nested) count += Query(nested, home, ctes, depth, insideView);
                foreach (var child in expression.Args) Expression(child);
                if (expression.Predicate is { } predicate) Expression(predicate);
                foreach (var order in expression.AggregateOrder ?? []) Expression(order.Expression);
                if (expression.Window is { } window) Window(window);
            }
            void Window(WindowSyntax window)
            {
                foreach (var item in window.Partition) Expression(item);
                foreach (var item in window.Order) Expression(item.Expression);
                if (window.Frame?.Start.Offset is { } start) Expression(start);
                if (window.Frame?.End.Offset is { } end) Expression(end);
            }
            void Relation(RelationSyntax? relation)
            {
                switch (relation)
                {
                    case TableRelation table:
                    {
                        if (ctes.Contains(table.Name) || !insideView && scope?.TryGet(table.Name, out _) == true) break;
                        var domain = home.Scope?.Resolve(table.Name);
                        var nextHome = domain?.Catalog ?? home;
                        var name = DatabaseDomains.Name(table.Name).Local;
                        var view = (nextHome.Views ?? []).FirstOrDefault(view => view.Name == name);
                        if (view is null) break;
                        if (!active.Add(view)) throw new SqlException(SqlStates.InvalidObjectDefinition, "recursive view expansion");
                        try
                        {
                            if (!parsed.TryGetValue(view, out var definition)) parsed.Add(view, definition = StatementExecution.ViewQuery(view));
                            var expanded = 1 + Query(definition, nextHome, [], depth + table.ParseDepth, true);
                            if (expanded > 1024) throw new SqlException(SqlStates.StatementTooComplex, "view expansion exceeds 1024");
                            if (insideView) count += expanded;
                        }
                        finally { active.Remove(view); }
                        break;
                    }
                    case DerivedRelation derived: count += Query(derived.Query, home, ctes, depth, insideView); break;
                    case SetRelation set: count += Query(set.Left, home, ctes, depth, insideView); count += Query(set.Right, home, ctes, depth, insideView); break;
                    case JoinedRelation joined: Relation(joined.Left); Relation(joined.Right); if (joined.On is { } on) Expression(on); break;
                    case GroupedRelation grouped: Relation(grouped.Source); break;
                    case AliasedRelation aliased: Relation(aliased.Source); break;
                    case LateralRelation lateral: Relation(lateral.Source); break;
                    case ValuesRelation values: foreach (var row in values.Values) foreach (var value in row) Expression(value); break;
                    case FunctionRelation function: Expression(function.Function); break;
                }
            }
            Relation(body.Relation ?? (body.Table is { } table ? new TableRelation(table, body.TableAlias) : null));
            foreach (var item in body.Items) Expression(item.Expression);
            if (body.Filter is { } filter) Expression(filter);
            if (body.Having is { } having) Expression(having);
            if (body.LimitExpression is { } limit) Expression(limit);
            if (body.OffsetExpression is { } offset) Expression(offset);
            foreach (var item in body.GroupBy ?? []) Expression(item);
            foreach (var item in body.DistinctOn ?? []) Expression(item);
            foreach (var item in body.Order ?? []) Expression(item.Expression);
            foreach (var window in body.Windows?.Values.AsEnumerable() ?? []) Window(window);
            if (count > 1024) throw new SqlException(SqlStates.StatementTooComplex, "view expansion exceeds 1024");
            return count;
        }
        _ = Query(query, catalog, [], baseDepth, false);
    }
}
