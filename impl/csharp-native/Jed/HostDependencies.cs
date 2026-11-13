namespace Jed;

internal static class HostDependencies
{
    internal static CatalogHostDependency[]? Collect(IEnumerable<Resolved> expressions, bool immutable)
    {
        var entries = new List<CatalogHostDependency>();
        void Visit(Resolved expression)
        {
            if (expression.HostFunction is { } function)
            {
                if (string.IsNullOrEmpty(function.ComponentId) || immutable && function.Volatility != FunctionVolatility.Immutable)
                    throw new SqlException(SqlStates.InvalidObjectDefinition, "stored host function requires a component identity and permitted volatility");
                var entry = new CatalogHostDependency(function.Name, function.Arguments.Select(StoredValues.TypeCode).ToArray(),
                    StoredValues.TypeCode(function.Result), function.ComponentId, function.SemanticVersion ?? 0);
                if (!entries.Any(prior => prior.Name == entry.Name && prior.Arguments.SequenceEqual(entry.Arguments))) entries.Add(entry);
            }
            foreach (var argument in expression.Args) Visit(argument);
        }
        foreach (var expression in expressions) Visit(expression);
        return entries.Count == 0 ? null : entries.OrderBy(entry => entry.Name, Utf8NameComparer.Instance)
            .ThenBy(entry => Convert.ToHexString(entry.Arguments), StringComparer.Ordinal).ToArray();
    }
    internal static void Verify(CatalogHostDependency[]? pins, DatabaseEnvironment? environment)
    {
        foreach (var pin in pins ?? [])
        {
            var function = environment?.Functions.FirstOrDefault(function => function.Name == pin.Name && function.Arguments.Select(StoredValues.TypeCode).SequenceEqual(pin.Arguments));
            if (function is null) throw new SqlException(SqlStates.UndefinedFunction, "stored expression host function is missing: " + pin.Name);
            if (StoredValues.TypeCode(function.Result) != pin.Result || function.ComponentId != pin.Component || (function.SemanticVersion ?? 0) != pin.Version)
                throw new SqlException(SqlStates.CollationVersionMismatch, "stored host function component or version changed: " + pin.Name);
        }
    }
    internal static void VerifyResolved(CatalogHostDependency[]? pins, Resolved expression) => VerifyResolved(pins, [expression]);
    internal static void VerifyResolved(CatalogHostDependency[]? pins, IEnumerable<Resolved> expressions)
    {
        var seen = new List<HostFunction>();
        void Visit(Resolved node)
        {
            if (node.HostFunction is { } function)
            {
                seen.Add(function);
                if (!(pins ?? []).Any(pin => pin.Name == function.Name && pin.Arguments.SequenceEqual(function.Arguments.Select(StoredValues.TypeCode)) &&
                    pin.Result == StoredValues.TypeCode(function.Result) && pin.Component == function.ComponentId && pin.Version == (function.SemanticVersion ?? 0)))
                    throw new SqlException(SqlStates.CollationVersionMismatch, "stored expression resolves to a different host function");
            }
            foreach (var argument in node.Args) Visit(argument);
        }
        foreach (var expression in expressions) Visit(expression);
        if ((pins ?? []).Any(pin => !seen.Any(function => function.Name == pin.Name && pin.Arguments.SequenceEqual(function.Arguments.Select(StoredValues.TypeCode)))))
            throw new SqlException(SqlStates.CollationVersionMismatch, "stored host function was captured by a different overload");
    }

}

internal static partial class StatementExecution
{
    private static void WritableCheckDependencies(CatalogTable table, Catalog catalog, Meter meter)
    {
        foreach (var check in table.Checks ?? [])
        {
            var expression = Resolve.Expr(Resolve.Bind(Resolve.BindTypes(StoredExpression(check.Expression), catalog, meter.Temporal), Columns(table)));
            HostDependencies.Verify(check.HostDependencies, meter.Temporal.Environment);
            HostDependencies.VerifyResolved(check.HostDependencies, expression);
        }
    }
}
