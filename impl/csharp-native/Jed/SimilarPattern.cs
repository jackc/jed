using System.Text;

namespace Jed;

internal sealed record SimilarTranslation(string Text, string[] Parts);
internal sealed class SimilarPattern
{
    internal RegexProgram[] Programs { get; }
    internal int Count => Programs.Sum(program => program.Code.Length);
    private SimilarPattern(RegexProgram[] programs) { Programs = programs; }
    internal static SimilarTranslation Translate(string pattern, string escape = "\\")
    {
        var escapes = escape.EnumerateRunes().ToArray();
        if (escapes.Length > 1) throw new SqlException("22025", "invalid escape string");
        var escapePoint = escapes.Length == 0 ? -1 : escapes[0].Value; var afterEscape = false; var inClass = false; var start = 0;
        var parts = new List<StringBuilder> { new() }; var output = new StringBuilder("^(?:");
        void Emit(string value) { output.Append(value); parts[^1].Append(value); }
        foreach (var rune in pattern.EnumerateRunes())
        {
            var point = rune.Value;
            if (afterEscape)
            {
                afterEscape = false;
                if (point == '"' && !inClass)
                {
                    if (parts.Count == 3) throw new SqlException("2200C", "SQL regular expression may not contain more than two escape-double-quote separators");
                    output.Append(parts.Count == 1 ? "){1,1}?(" : "){1,1}(?:"); parts.Add(new());
                }
                else { Emit("\\" + rune); if (inClass) start = 3; }
            }
            else if (point == escapePoint) afterEscape = true;
            else if (inClass)
            {
                Emit(point == '\\' ? "\\\\" : rune.ToString());
                if (point == ']' && start > 2) inClass = false; else if (point == '^') start++; else start = 3;
            }
            else if (point == '[') { Emit("["); inClass = true; start = 1; }
            else Emit(point switch { '%' => ".*", '_' => ".", '(' => "(?:", '\\' or '.' or '^' or '$' => "\\" + rune, _ => rune.ToString() });
        }
        output.Append(")$"); return new(output.ToString(), parts.Select(part => part.ToString()).ToArray());
    }
    internal static SimilarPattern Compile(SimilarTranslation translation, bool substring)
    {
        if (!substring || translation.Parts.Length == 1) return new([RegexProgram.Compile(translation.Text, "s")]);
        var first = translation.Parts[0]; var middle = translation.Parts[1]; var last = translation.Parts.Length == 3 ? translation.Parts[2] : null;
        var programs = new List<RegexProgram>
        {
            RegexProgram.Compile("(?:" + first + ")", "s", gated: true),
            RegexProgram.Compile("(?:" + middle + ")" + (last is null ? "" : "(?:" + last + ")") + "$", "s", gated: true)
        };
        if (last is not null)
        {
            programs.Add(RegexProgram.Compile("(?:" + middle + ")", "s", gated: true));
            programs.Add(RegexProgram.Compile("(?:" + last + ")$", "s", gated: true, rightmost: true));
        }
        return new(programs.ToArray());
    }
    internal (int Start, int End, long Steps) Run(string source, Meter? meter = null, RegexMachine[]? machines = null)
    {
        var input = source.EnumerateRunes().Select(rune => rune.Value).ToArray(); machines ??= Programs.Select(program => new RegexMachine(program)).ToArray();
        if (Programs.Length == 1)
        {
            var result = machines[0].Search(input, meter: meter); return result.Captures is null ? (-1, -1, result.Steps) : (0, input.Length, result.Steps);
        }
        var ends = new HashSet<int>(); long steps = machines[0].Search(input, meter: meter, gates: [0], ends: ends).Steps;
        var left = machines[1].Search(input, meter: meter, gates: ends); steps += left.Steps;
        if (left.Captures is null) return (-1, -1, steps); var start = left.Captures[0];
        if (Programs.Length == 2) return (start, input.Length, steps);
        ends.Clear(); steps += machines[2].Search(input, meter: meter, gates: [start], ends: ends).Steps;
        var right = machines[3].Search(input, meter: meter, gates: ends); steps += right.Steps;
        return right.Captures is null ? (-1, -1, steps) : (start, right.Captures[0], steps);
    }
}
