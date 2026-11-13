namespace Jed;

internal sealed record FunctionFact(string Name, string[] Families, string Result, string Nulls,
    long Cost, bool Variadic, string[] ArgumentNames, string[] Defaults, string Volatility = "immutable");
