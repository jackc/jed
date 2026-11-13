namespace Jed;

internal sealed partial class Parser
{
    private AlterTableSyntax AddIdentity(string table, string column)
    {
        Expect("generated"); var always = Eat("always"); if (!always) { Expect("by"); Expect("default"); }
        Expect("as"); Expect("identity"); var options = new SequenceOptions();
        if (Eat("(")) { options = ReadSequenceOptions(false, true); Expect(")"); }
        return new(table, "add_identity", column, Name: always ? "always" : "default", Sequence: options);
    }
    private static SequenceOptions MergeIdentityOptions(SequenceOptions before, SequenceOptions next)
    {
        if (before.Increment is not null && next.Increment is not null ||
            (before.Minimum is not null || before.NoMinimum) && (next.Minimum is not null || next.NoMinimum) ||
            (before.Maximum is not null || before.NoMaximum) && (next.Maximum is not null || next.NoMaximum) ||
            before.Start is not null && next.Start is not null || before.Cache is not null && next.Cache is not null ||
            before.Cycle is not null && next.Cycle is not null || before.Restart && next.Restart) throw Errors.Syntax("conflicting identity options");
        return new(Increment: next.Increment ?? before.Increment, Minimum: next.Minimum ?? before.Minimum, Maximum: next.Maximum ?? before.Maximum,
            NoMinimum: before.NoMinimum || next.NoMinimum, NoMaximum: before.NoMaximum || next.NoMaximum, Start: next.Start ?? before.Start,
            Cache: next.Cache ?? before.Cache, Cycle: next.Cycle ?? before.Cycle, Restart: before.Restart || next.Restart,
            RestartValue: next.Restart ? next.RestartValue : before.RestartValue);
    }
    private AlterTableSyntax IdentityChanges(string table, string column, bool consumedSet)
    {
        var options = new SequenceOptions(); string? generation = null;
        while (true)
        {
            if (consumedSet || Eat("set"))
            {
                consumedSet = false;
                if (Eat("generated"))
                {
                    if (generation is not null) throw Errors.Syntax("duplicate generation option");
                    if (Eat("always")) generation = "always";
                    else { Expect("by"); Expect("default"); generation = "default"; }
                }
                else
                {
                    if (Is("as") || Is("restart") || Is("sequence")) throw Errors.Syntax("invalid identity SET option");
                    var next = ReadSequenceOptions(false);
                    if (next == new SequenceOptions()) throw Errors.Syntax("expected identity option");
                    options = MergeIdentityOptions(options, next);
                }
            }
            else if (Is("restart")) options = MergeIdentityOptions(options, ReadSequenceOptions(true, allowSetTail: true));
            else break;
        }
        return new(table, "identity_options", column, generation, Sequence: options);
    }
}
