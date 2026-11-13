namespace Jed;

public sealed partial class Session
{
    private readonly Dictionary<string, string> variables = new(StringComparer.Ordinal);
    public void SetVar(string name, string value)
    {
        ArgumentNullException.ThrowIfNull(name); ArgumentNullException.ThrowIfNull(value);
        using var operation = Enter(); CheckVariableName(name); variables[VariableName(name)] = value;
    }
    public void ResetVar(string name) { ArgumentNullException.ThrowIfNull(name); using var operation = Enter(); CheckVariableName(name); variables.Remove(VariableName(name)); }
    public string? Var(string name) { ArgumentNullException.ThrowIfNull(name); using var operation = Enter(); return variables.GetValueOrDefault(VariableName(name)); }
    public void ResetVariables() { using var operation = Enter(); variables.Clear(); }
    internal static string VariableName(string name) => string.Create(name.Length, name, static (destination, source) =>
    {
        for (var i = 0; i < source.Length; i++) destination[i] = source[i] is >= 'A' and <= 'Z' ? (char)(source[i] + 32) : source[i];
    });
    private static void CheckVariableName(string name)
    {
        if (!name.Contains('.', StringComparison.Ordinal)) throw new SqlException("42704", "unrecognized configuration parameter: " + name);
    }
    internal Dictionary<string, string> VariableSnapshot() => new(variables, StringComparer.Ordinal);
}
