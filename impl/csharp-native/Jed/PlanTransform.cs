namespace Jed;

internal static class PlanTransform
{
    internal static Resolved InitialValues(Resolved expression)
    {
        if (expression.Op == "outer") return new("constant", expression.Type, expression.Outer!.Value, []);
        if (expression.Op == "initplan" && expression.Subquery!.Initialized)
            return new("constant", expression.Type, expression.Subquery.Value, []);
        return expression with { Args = expression.Args.Select(InitialValues).ToArray() };
    }
    internal static SelectPlan InitialValues(SelectPlan plan) => Map(plan, InitialValues);
    internal static SelectPlan Map(SelectPlan plan, Func<Resolved, Resolved> map) => plan with
    {
        Projections = plan.Projections.Select(projection => projection with { Expression = map(projection.Expression) }).ToArray(),
        Filter = plan.Filter is null ? null : map(plan.Filter),
        Order = plan.Order?.Select(order => order with { Expression = map(order.Expression) }).ToArray(),
        Windows = plan.Windows is null ? null : plan.Windows with
        {
            Plans = plan.Windows.Plans.Select(window => window with
            {
                Args = window.Args.Select(map).ToArray(), Partition = window.Partition.Select(map).ToArray(),
                Order = window.Order.Select(key => key with { Expression = map(key.Expression) }).ToArray(),
                Aggregate = window.Aggregate is null ? null : window.Aggregate with
                {
                    Argument = window.Aggregate.Argument is null ? null : map(window.Aggregate.Argument),
                    Filter = window.Aggregate.Filter is null ? null : map(window.Aggregate.Filter),
                Extra = window.Aggregate.Extra?.Select(map).ToArray(),
                Order = window.Aggregate.Order?.Select(key => key with { Expression = map(key.Expression) }).ToArray()
                }
            }).ToArray()
        },
        Group = plan.Group is null ? null : plan.Group with
        {
            Keys = plan.Group.Keys.Select(map).ToArray(), Having = plan.Group.Having is null ? null : map(plan.Group.Having),
            Aggregates = plan.Group.Aggregates.Select(aggregate => aggregate with
            {
                Argument = aggregate.Argument is null ? null : map(aggregate.Argument), Filter = aggregate.Filter is null ? null : map(aggregate.Filter),
                Extra = aggregate.Extra?.Select(map).ToArray(),
                Order = aggregate.Order?.Select(key => key with { Expression = map(key.Expression) }).ToArray()
            }).ToArray()
        }
    };
}
