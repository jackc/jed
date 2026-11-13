using System.Text;

namespace Jed;

internal sealed class SequenceState
{
    internal Dictionary<string, long> Current = new(StringComparer.Ordinal);
    internal string? Last;
    internal SequenceState Fork() => new() { Current = new(Current, StringComparer.Ordinal), Last = Last };
}

internal sealed class SequenceContext(SequenceState state, Meter meter)
{
    private Catalog snapshot = new();
    private PendingDatabase? pending;
    internal SequenceState State { get; } = state;
    internal void Bind(Catalog catalog, PendingDatabase? write = null) { snapshot = catalog; pending = write; }
    internal IDisposable PushBinding(Catalog catalog, PendingDatabase? write)
    {
        var restore = new Binding(this, snapshot, pending); Bind(catalog, write); return restore;
    }
    private sealed class Binding(SequenceContext context, Catalog catalog, PendingDatabase? write) : IDisposable
    {
        public void Dispose() => context.Bind(catalog, write);
    }
    internal static string Name(string text)
    {
        text = text.Trim();
        var quoted = false;
        for (var index = 0; index < text.Length; index++)
        {
            if (text[index] == '"')
            {
                if (quoted && index + 1 < text.Length && text[index + 1] == '"') { index++; continue; }
                quoted = !quoted;
            }
            else if (!quoted && text[index] == '.')
                return DatabaseDomains.Qualified(Name(text[..index]), Name(text[(index + 1)..]));
        }
        if (text.Length == 0) throw new SqlException(SqlStates.InvalidName, "invalid sequence name");
        if (text[0] == '"')
        {
            var value = new StringBuilder();
            for (var i = 1; i < text.Length; i++)
            {
                if (text[i] != '"') { value.Append(text[i]); continue; }
                if (i + 1 < text.Length && text[i + 1] == '"') { value.Append('"'); i++; continue; }
                if (i == text.Length - 1) return value.ToString();
                break;
            }
            throw new SqlException(SqlStates.InvalidName, "invalid sequence name");
        }
        if (!(text[0] is >= 'a' and <= 'z' or >= 'A' and <= 'Z' or '_') ||
            text.Any(character => character is not (>= 'a' and <= 'z' or >= 'A' and <= 'Z' or >= '0' and <= '9' or '_' or '$')))
            throw new SqlException(SqlStates.InvalidName, "invalid sequence name");
        return TextEncoding.FoldName(text);
    }
    internal SqlValue Call(string operation, SqlValue[] arguments)
    {
        if (arguments.Any(value => value.IsNull)) return SqlValue.Null(SqlType.I64);
        if (operation == "lastval") return SqlValue.Integer(State.Last is { } last && State.Current.TryGetValue(last, out var lastValue) ? lastValue :
            throw new SqlException(SqlStates.ObjectNotInPrerequisiteState, "lastval is not defined"), SqlType.I64);
        var name = Name(arguments[0].AsText()); var catalog = pending?.Catalog ?? snapshot; var write = pending; var stateName = name;
        if (snapshot.Scope is { } domains)
        {
            var domain = domains.Resolve(name); name = DatabaseDomains.Name(name).Local;
            catalog = domain.Catalog; write = domain.Pending;
            stateName = domain.Name == "main" ? name : DatabaseDomains.Qualified(domain.Name, name);
        }
        var sequence = (catalog.Sequences ?? []).FirstOrDefault(sequence => sequence.Name == name)
            ?? throw new SqlException(SqlStates.UndefinedTable, "sequence does not exist");
        if (operation == "currval") return SqlValue.Integer(State.Current.TryGetValue(stateName, out var current) ? current :
            throw new SqlException(SqlStates.ObjectNotInPrerequisiteState, "currval is not defined"), SqlType.I64);
        if (write is null) throw new SqlException(SqlStates.ReadOnlySqlTransaction, "sequence mutation requires a writable statement");
        meter.Charge(Costs.SequenceAdvance); meter.Checkpoint();
        long value; var called = true;
        if (operation == "setval")
        {
            value = arguments[1].AsInteger(); called = arguments.Length < 3 || arguments[2].AsBoolean();
            if (value < sequence.Minimum || value > sequence.Maximum) throw Errors.Overflow();
        }
        else
        {
            var next = sequence.IsCalled ? (Int128)sequence.LastValue + sequence.Increment : sequence.LastValue;
            if (next < sequence.Minimum || next > sequence.Maximum)
            {
                if (!sequence.Cycle) throw new SqlException(SqlStates.SequenceGeneratorLimitExceeded, "sequence limit reached");
                next = sequence.Increment > 0 ? sequence.Minimum : sequence.Maximum;
            }
            value = (long)next;
        }
        var updated = sequence with { LastValue = value, IsCalled = called };
        write.Catalog = write.Catalog with { Sequences = (write.Catalog.Sequences ?? []).Select(sequence => sequence.Name == name ? updated : sequence).ToArray() };
        write.Changed = true;
        if (called) State.Current[stateName] = value;
        if (operation == "nextval") State.Last = stateName;
        return SqlValue.Integer(value, SqlType.I64);
    }
}
