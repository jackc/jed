namespace Jed;

internal static class QueryCounts
{
    internal static SelectSyntax Resolve(SelectSyntax query)
    {
        long? Count(Expression expression, bool limit)
        {
            if (expression.Parameter is null) throw new SqlException(SqlStates.UndefinedParameter, "unbound query count parameter");
            var resolved = expression.Parameter.Resolve(SqlType.I64);
            if (resolved.Constant.IsNull) return null;
            var value = resolved.Constant.AsInteger();
            if (value < 0) throw new SqlException(limit ? SqlStates.InvalidRowCountInLimitClause : SqlStates.InvalidRowCountInOffsetClause,
                limit ? "LIMIT must not be negative" : "OFFSET must not be negative");
            return value;
        }
        return query with
        {
            Limit = query.LimitExpression is { } limit ? Count(limit, true) : query.Limit,
            Offset = query.OffsetExpression is { } offset ? Count(offset, false) ?? 0 : query.Offset
        };
    }
}
