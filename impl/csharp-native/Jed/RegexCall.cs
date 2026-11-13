using System.Text;

namespace Jed;

internal sealed class RegexCall(string name, int flagsIndex, string flags, RegexProgram? program)
{
    internal string Name { get; } = name;
    internal int FlagsIndex { get; } = flagsIndex;
    internal string Flags { get; } = flags;
    internal RegexProgram? Program { get; } = program;
}
internal static class RegexResolve
{
    internal static Resolved Prepare(Resolved call, UnicodeData? unicode)
    {
        var name = call.Text;
        if (!name.StartsWith("regexp_", StringComparison.Ordinal) && !(name == "substring" && call.Args.Length == 2 && call.Args[1].Type == SqlType.Text)) return call;
        var flagsIndex = name switch { "regexp_replace" => 3, "regexp_instr" => 5, "regexp_substr" => 4, "regexp_count" => 3, "substring" => -1, _ => 2 };
        return call with { Regex = Descriptor(name, call.Args, flagsIndex, "", unicode) };
    }
    internal static Resolved Binary(Expression expression)
    {
        var args = expression.Args.Select(arg => Resolve.Expr(arg, SqlType.Text)).ToArray();
        if (args.Any(arg => arg.Type != SqlType.Text)) throw Errors.Type("regular expression operands require text");
        var flags = expression.Op.EndsWith('*') ? "i" : "";
        return new("regex", SqlType.Boolean, default, args, expression.Op, Regex: Descriptor(expression.Op, args, -1, flags, expression.Context?.Unicode));
    }
    private static RegexCall Descriptor(string name, Resolved[] args, int flagsIndex, string flags, UnicodeData? unicode)
    {
        if (flagsIndex >= args.Length) flagsIndex = -1;
        var constantFlags = flagsIndex < 0 || args[flagsIndex].Op == "constant" && !args[flagsIndex].Constant.IsNull;
        if (constantFlags && flagsIndex >= 0) flags = args[flagsIndex].Constant.AsText();
        RegexProgram? program = null;
        if (constantFlags)
        {
            ValidateFlags(name, flags);
            if (args[1].Op == "constant" && !args[1].Constant.IsNull) program = RegexProgram.Compile(args[1].Constant.AsText(), flags, unicode);
        }
        return new(name, flagsIndex, flags, program);
    }
    internal static void ValidateFlags(string name, string flags)
    {
        foreach (var rune in flags.EnumerateRunes()) if (rune.Value is not ('i' or 'g')) throw RegexParser.Bad("invalid regular expression option: " + rune);
        if (flags.Contains('g') && name is not ("regexp_replace" or "regexp_matches")) throw new SqlException("22023", name + "() does not support the \"global\" option");
    }
}
