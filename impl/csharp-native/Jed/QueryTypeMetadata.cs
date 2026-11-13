namespace Jed;

internal static partial class StatementExecution
{
    internal static void ValidateRecursiveModifiers(CteSyntax definition, SetRelation body, Catalog catalog)
    {
        var anchor = QueryTypeColumns(body.Left, catalog);
        var self = anchor.Select((column, index) => column with { Qualifier = definition.Name,
            Name = definition.Columns is { } names && index < names.Length ? names[index] : column.Name }).ToArray();
        var step = QueryTypeColumns(body.Right, catalog, new(StringComparer.Ordinal) { [definition.Name] = self });
        static (int?, int?, int?, byte) Modifier(TypeSpec? type) => (type?.Precision, type?.Scale, type?.Length, type?.IntervalFields ?? 0);
        for (var i = 0; i < Math.Min(anchor.Length, step.Length); i++)
            if (Modifier(anchor[i].Type) != (null, null, null, (byte)0) && Modifier(anchor[i].Type) != Modifier(step[i].Type))
                throw Errors.Type("recursive term changes its anchor type modifier");
    }
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
            var body = cte.Mutation is { } mutation ? ReturningQuery(mutation) : cte.Query;
            ctes[cte.Name] = Alias(Nested(body), cte.Name, cte.Columns);
        }
        static SelectSyntax ReturningQuery(StatementSyntax mutation) => mutation switch
        {
            InsertSyntax value => new(value.Returning, null, null, 0) { Relation = new TableRelation(value.Table, value.Alias) },
            UpdateSyntax value => new(value.Returning, null, null, 0) { Relation = new TableRelation(value.Table) },
            DeleteSyntax value => new(value.Returning, null, null, 0) { Relation = new TableRelation(value.Table) },
            _ => new([], null, null, 0)
        };
        TypeSpec? Common(IEnumerable<TypeSpec?> types)
        {
            var items = types.ToArray();
            return items.Length != 0 && items[0] is { } first && items.All(item => item == first) ? first : null;
        }
        TypeSpec? ExpressionType(Expression expression, TypeColumn[] columns)
        {
            if (expression.Op == "cast") return expression.Type ?? SqlTypes.ParseSpec(expression.Text, catalog.NamedType);
            if (expression.Op == "name")
            {
                var dot = expression.Text.LastIndexOf('.'); var name = expression.Text[(dot + 1)..]; var label = dot < 0 ? null : expression.Text[..dot];
                var matches = columns.Where(column => column.Name == name && (label is null || column.Qualifier == label)).ToArray();
                return matches.Length == 1 ? matches[0].Type : null;
            }
            if (expression.Op == "collate") return ExpressionType(expression.Args[0], columns);
            if (expression.Op == "case") return Common(expression.Args.Where((_, index) => index % 2 == 1 || index == expression.Args.Length - 1).Select(item => ExpressionType(item, columns)));
            if (expression.Op == "call" && expression.Text is "coalesce" or "greatest" or "least")
                return Common(expression.Args.Select(item => ExpressionType(item, columns)));
            if (expression.Op == "call" && expression.Text == "nullif")
            {
                var first = ExpressionType(expression.Args[0], columns); var second = ExpressionType(expression.Args[1], columns);
                return first?.Type.IsInteger() == true && second?.Type == SqlType.Decimal ? null : first;
            }
            if (expression.Op == "subquery" && expression.Query is { } scalar) return Nested(scalar).FirstOrDefault()?.Type;
            if (expression.Op == "field" && ExpressionType(expression.Args[0], columns)?.Type.Fields.FirstOrDefault(field => field.Name == expression.Text) is { } field)
                return new(field.Type, field.Precision, field.Scale, field.Length, field.IntervalFields);
            return null;
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
                case ValuesRelation values:
                    return values.Values.Length == 0 ? [] : values.Values[0].Select((_, index) => new TypeColumn($"column{index + 1}", null,
                        Common(values.Values.Select(row => index < row.Length ? ExpressionType(row[index], []) : null)))).ToArray();
                case FunctionRelation { Definitions: { } definitions } function:
                    return definitions.Select(column => new TypeColumn(column.Name, function.Alias, SqlTypes.ParseSpec(column.TypeName, catalog.NamedType))).ToArray();
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
            "name" => expression.Text.Split('.')[^1], "call" or "field" => expression.Text,
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
            if (expression.Op == "field_star" && ExpressionType(expression.Args[0], input)?.Type.Fields is { } fields)
            {
                result.AddRange(fields.Select(field => new TypeColumn(field.Name, null, new(field.Type, field.Precision, field.Scale, field.Length, field.IntervalFields))));
                continue;
            }
            var type = ExpressionType(expression, input);
            result.Add(new(item.Alias ?? Name(expression), null, type));
        }
        return result.ToArray();
    }
}
