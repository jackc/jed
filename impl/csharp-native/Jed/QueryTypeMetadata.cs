namespace Jed;

internal static partial class StatementExecution
{
    private sealed record TypeColumn(string Name, string? Qualifier, TypeSpec? Type);
    // Typmods belong to a projected expression, not its scalar type identity.
    // Preserve them only for references/casts and unanimous set-operation arms.
    private static TypeColumn[] QueryTypeColumns(SelectSyntax query, Catalog catalog,
        Dictionary<string, TypeColumn[]>? inherited = null, int depth = 0)
    {
        if (depth > 128) throw new SqlException(SqlStates.ProgramLimitExceeded, "query type metadata nesting exceeds limit");
        var ctes = inherited is null ? new Dictionary<string, TypeColumn[]>(StringComparer.Ordinal) : new(inherited, StringComparer.Ordinal);
        TypeColumn[] Alias(TypeColumn[] columns, string? alias, string[]? names = null) => columns.Select((column, index) =>
            column with { Qualifier = alias, Name = names is not null && index < names.Length ? names[index] : column.Name }).ToArray();
        TypeColumn[] Nested(SelectSyntax body) => QueryTypeColumns(body, catalog, ctes, depth + 1);
        foreach (var cte in query.With ?? [])
        {
            if (query.Recursive && cte.Query.Relation is SetRelation recursive) ctes[cte.Name] = Alias(Nested(recursive.Left), cte.Name, cte.Columns);
            ctes[cte.Name] = Alias(Nested(cte.Query), cte.Name, cte.Columns);
        }
        TypeColumn[] Source(RelationSyntax? relation)
        {
            switch (relation)
            {
                case TableRelation named:
                {
                    if (ctes.TryGetValue(named.Name, out var cte)) return Alias(cte, named.Alias ?? named.Name, named.ColumnAliases);
                    var local = DatabaseDomains.Name(named.Name).Local;
                    var home = catalog.Scope?.Resolve(named.Name).Catalog ?? catalog;
                    if (home.Tables.FirstOrDefault(table => table.Name == local) is { } table)
                        return Alias(table.Columns.Select(column => new TypeColumn(column.Name, null, column.Type)).ToArray(), named.Alias ?? local, named.ColumnAliases);
                    if ((home.Views ?? []).FirstOrDefault(view => view.Name == local) is { } view)
                    {
                        var columns = QueryTypeColumns(ViewQuery(view), home, depth: depth + 1);
                        columns = Alias(columns, local, view.Columns.Select(column => column.Name).ToArray());
                        return Alias(columns, named.Alias ?? local, named.ColumnAliases);
                    }
                    return [];
                }
                case DerivedRelation derived: return Alias(Nested(derived.Query), derived.Alias, derived.ColumnAliases);
                case AliasedRelation aliased: return Alias(Source(aliased.Source), aliased.Alias, aliased.ColumnAliases);
                case LateralRelation lateral: return Source(lateral.Source);
                case GroupedRelation grouped: return Source(grouped.Source);
                case JoinedRelation join:
                {
                    var left = Source(join.Left); var right = Source(join.Right);
                    var names = join.Using ?? (join.Natural ? left.Select(column => column.Name).Intersect(right.Select(column => column.Name), StringComparer.Ordinal).ToArray() : []);
                    if (names.Length == 0) return [.. left, .. right];
                    var merged = names.Select(name => new TypeColumn(name, null,
                        left.FirstOrDefault(column => column.Name == name)?.Type is { } type && right.FirstOrDefault(column => column.Name == name)?.Type == type ? type : null));
                    return [.. merged, .. left.Where(column => !names.Contains(column.Name)), .. right.Where(column => !names.Contains(column.Name))];
                }
                case SetRelation set:
                {
                    var left = Nested(set.Left); var right = Nested(set.Right);
                    return left.Select((column, index) => column with { Qualifier = null, Type = index < right.Length && column.Type == right[index].Type ? column.Type : null }).ToArray();
                }
                default: return [];
            }
        }
        var input = Source(query.Relation ?? (query.Table is null ? null : new TableRelation(query.Table, query.TableAlias)));
        if (query.SetQuery) return input;
        static string Name(Expression expression) => expression.Op switch
        {
            "name" => expression.Text.Split('.')[^1], "call" => expression.Text,
            "cast" or "subscript" => Name(expression.Args[0]), _ => "?column?"
        };
        var result = new List<TypeColumn>();
        foreach (var item in query.Items)
        {
            var expression = item.Expression;
            if (expression.Op == "star")
            {
                result.AddRange(input.Where(column => expression.Text.Length == 0 || column.Qualifier == expression.Text)); continue;
            }
            TypeSpec? type = null;
            if (expression.Op == "cast") type = expression.Type ?? SqlTypes.ParseSpec(expression.Text, catalog.NamedType);
            else if (expression.Op == "name")
            {
                var dot = expression.Text.LastIndexOf('.'); var name = expression.Text[(dot + 1)..]; var label = dot < 0 ? null : expression.Text[..dot];
                var matches = input.Where(column => column.Name == name && (label is null || column.Qualifier == label)).ToArray();
                if (matches.Length == 1) type = matches[0].Type;
            }
            result.Add(new(item.Alias ?? Name(expression), null, type));
        }
        return result.ToArray();
    }
}
