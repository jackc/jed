namespace Jed;

internal static class PlanTransform
{
    internal static Resolved WithoutMembershipBounds(Resolved expression) => expression.Op is "in_init" or "quantified_init"
        ? expression with { Op = "membership_barrier" }
        : expression with { Args = expression.Args.Select(WithoutMembershipBounds).ToArray() };
    internal static bool CorrelatedAccess(Resolved? expression) => expression is not null &&
        (expression.Op == "outer" || expression.Args.Any(CorrelatedAccess));
    internal static Resolved GenericAccess(Resolved expression) => expression.Op == "outer"
        ? new("constant", expression.Type, SqlValue.Null(expression.Type), [], Collation: expression.Collation, ParameterUnbound: true, BoundSource: true)
        : expression with { Args = expression.Args.Select(GenericAccess).ToArray() };
    internal static bool DynamicAccess(Resolved? expression) => expression is not null &&
        (expression.Op is "outer" or "in_init" or "quantified_init" || expression.Args.Any(DynamicAccess));
    internal static async ValueTask<Resolved> AccessValues(Resolved expression, CancellationToken token)
    {
        if (expression.Op == "outer")
            return new("constant", expression.Type, await expression.Outer!.Value.ResolveAsync(token).ConfigureAwait(false), [], Collation: expression.Collation, BoundSource: true);
        if (expression.Op == "initplan" && expression.Subquery!.Initialized)
            return new("constant", expression.Type, await expression.Subquery.Value.ResolveAsync(token).ConfigureAwait(false), [], Collation: expression.Collation);
        if (expression.Args.Length == 0) return expression;
        var args = new Resolved[expression.Args.Length];
        for (var i = 0; i < args.Length; i++) args[i] = await AccessValues(expression.Args[i], token).ConfigureAwait(false);
        return expression with { Args = args };
    }
    internal static Resolved InitialValues(Resolved expression)
    {
        if (expression.Op == "outer") return new("constant", expression.Type, expression.Outer!.Value, [], Collation: expression.Collation, BoundSource: true);
        if (expression.Op == "initplan" && expression.Subquery!.Initialized)
            return new("constant", expression.Type, expression.Subquery.Value, [], Collation: expression.Collation);
        Resolved[]? changed = null;
        for (var i = 0; i < expression.Args.Length; i++)
        {
            var value = InitialValues(expression.Args[i]);
            if (!ReferenceEquals(value, expression.Args[i]))
            {
                changed ??= (Resolved[])expression.Args.Clone();
                changed[i] = value;
            }
        }
        return changed is null ? expression : expression with { Args = changed };

    }
    private static bool HasInitial(Resolved? expression)
    {
        if (expression is null) return false;
        if (expression.Op == "outer" || expression.Op == "initplan" && expression.Subquery!.Initialized) return true;
        foreach (var argument in expression.Args) if (HasInitial(argument)) return true;
        return false;
    }
    private static bool HasInitial(AggregatePlan aggregate) => HasInitial(aggregate.Argument) || HasInitial(aggregate.Filter) ||
        (aggregate.Extra?.Any(HasInitial) ?? false) || (aggregate.Order?.Any(key => HasInitial(key.Expression)) ?? false) ||
        (aggregate.WithinOrder?.Any(key => HasInitial(key.Expression)) ?? false) || (aggregate.Direct?.Any(HasInitial) ?? false);
    internal static SelectPlan InitialValues(SelectPlan plan)
    {
        // Ordinary plans are immutable and have no execution-bound cells. Retain
        // them instead of copying every expression, projection, and order key.
        var changed = HasInitial(plan.Filter) || plan.Projections.Any(value => HasInitial(value.Expression)) ||
            (plan.Order?.Any(key => HasInitial(key.Expression)) ?? false);
        if (!changed && plan.Group is { } group)
            changed = group.Keys.Any(HasInitial) || HasInitial(group.Having) || group.Aggregates.Any(HasInitial);
        if (!changed && plan.Windows is { } windows)
            changed = windows.Plans.Any(window => window.Args.Any(HasInitial) || window.Partition.Any(HasInitial) ||
                window.Order.Any(key => HasInitial(key.Expression)) || window.Aggregate is { } aggregate && HasInitial(aggregate));
        return changed ? Map(plan, InitialValues) : plan;
    }

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
                Order = aggregate.Order?.Select(key => key with { Expression = map(key.Expression) }).ToArray(),
                WithinOrder = aggregate.WithinOrder?.Select(key => key with { Expression = map(key.Expression) }).ToArray(),
                Direct = aggregate.Direct?.Select(map).ToArray()
            }).ToArray()
        }
    };
}
