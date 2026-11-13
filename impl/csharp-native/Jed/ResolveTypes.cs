namespace Jed;

internal static partial class Resolve
{
    internal static Expression BindTypes(Expression expression, Catalog catalog, TemporalContext? context = null) => expression with
    {
        Context = context ?? expression.Context ?? (catalog.BoundEnvironment is { } environment ? new TemporalContext(environment.ClockSource.NowMicroseconds, "UTC", environment.TimeZones, environment.UnicodeData, environment) : null),
        AggregateOrder = expression.AggregateOrder?.Select(key => key with { Expression = BindTypes(key.Expression, catalog, context) }).ToArray(),
        Window = expression.Window is null ? null : BindTypes(expression.Window, catalog, context),

        Query = expression.Query is null ? null : BindTypes(expression.Query, catalog, context),
        Type = expression.Op == "cast" ? SqlTypes.ParseSpec(expression.Text, catalog.NamedType) : expression.Type,
        Args = expression.Args.Select(arg => BindTypes(arg, catalog, context)).ToArray(),
        Predicate = expression.Predicate is null ? null : BindTypes(expression.Predicate, catalog, context)
    };
    internal static SelectSyntax BindTypes(SelectSyntax query, Catalog catalog, TemporalContext? context = null) => query with
    {
        Windows = query.Windows?.ToDictionary(pair => pair.Key, pair => BindTypes(pair.Value, catalog, context), StringComparer.Ordinal),
        With = query.With?.Select(definition => definition with { Query = BindTypes(definition.Query, catalog, context) }).ToArray(),
        DistinctOn = query.DistinctOn?.Select(expression => BindTypes(expression, catalog, context)).ToArray(),
        Items = query.Items.Select(item => item with { Expression = BindTypes(item.Expression, catalog, context) }).ToArray(),
        Filter = query.Filter is null ? null : BindTypes(query.Filter, catalog, context),
        Order = query.Order?.Select(order => order with { Expression = BindTypes(order.Expression, catalog, context) }).ToArray(),
        GroupBy = query.GroupBy?.Select(expression => BindTypes(expression, catalog, context)).ToArray(),
        Having = query.Having is null ? null : BindTypes(query.Having, catalog, context),
        Relation = query.Relation is null ? null : BindTypes(query.Relation, catalog, context)
    };
    private static WindowSyntax BindTypes(WindowSyntax window, Catalog catalog, TemporalContext? context) => window with
    {
        Partition = window.Partition.Select(key => BindTypes(key, catalog, context)).ToArray(),
        Order = window.Order.Select(key => key with { Expression = BindTypes(key.Expression, catalog, context) }).ToArray(),
        Frame = window.Frame is null ? null : window.Frame with
        {
            Start = window.Frame.Start with { Offset = window.Frame.Start.Offset is null ? null : BindTypes(window.Frame.Start.Offset, catalog, context) },
            End = window.Frame.End with { Offset = window.Frame.End.Offset is null ? null : BindTypes(window.Frame.End.Offset, catalog, context) }
        }
    };
    private static RelationSyntax BindTypes(RelationSyntax source, Catalog catalog, TemporalContext? context = null) => source switch
    {
        LateralRelation lateral => lateral with { Source = BindTypes(lateral.Source, catalog, context) },
        AliasedRelation aliased => aliased with { Source = BindTypes(aliased.Source, catalog, context) },
        SetRelation set => set with { Left = BindTypes(set.Left, catalog, context), Right = BindTypes(set.Right, catalog, context) },
        ValuesRelation values => values with { Values = values.Values.Select(row => row.Select(expression => BindTypes(expression, catalog, context)).ToArray()).ToArray() },
        DerivedRelation derived => derived with { Query = BindTypes(derived.Query, catalog, context) },
        FunctionRelation function => function with { Function = BindTypes(function.Function, catalog, context) },
        GroupedRelation group => group with { Source = BindTypes(group.Source, catalog, context) },
        JoinedRelation join => join with { Left = BindTypes(join.Left, catalog, context), Right = BindTypes(join.Right, catalog, context), On = join.On is null ? null : BindTypes(join.On, catalog, context) },
        _ => source
    };
}
