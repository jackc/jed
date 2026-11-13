using System.Text;

namespace Jed;

internal readonly record struct RegexRange(int Low, int High);
internal sealed record RegexClass(bool Negated, RegexRange[] Ranges)
{
    internal bool Contains(int point)
    {
        var low = 0; var high = Ranges.Length;
        while (low < high) { var middle = low + (high - low) / 2; if (Ranges[middle].High < point) low = middle + 1; else high = middle; }
        return (low < Ranges.Length && Ranges[low].Low <= point) != Negated;
    }
    internal string Listing => (Negated ? "^" : "") + string.Join(",", Ranges.Select(range => $"{range.Low}-{range.High}"));
    internal static RegexClass Make(bool negated, IEnumerable<RegexRange> ranges)
    {
        var result = new List<RegexRange>();
        foreach (var range in ranges.OrderBy(range => range.Low).ThenBy(range => range.High))
        {
            if (result.Count != 0 && range.Low <= result[^1].High + 1) result[^1] = new(result[^1].Low, Math.Max(result[^1].High, range.High));
            else result.Add(range);
        }
        return new(negated, result.ToArray());
    }
    internal RegexRange[] PositiveRanges()
    {
        if (!Negated) return Ranges;
        var result = new List<RegexRange>(); var start = 0;
        foreach (var range in Ranges) { if (range.Low > start) result.Add(new(start, range.Low - 1)); start = range.High + 1; }
        if (start <= 0x10ffff) result.Add(new(start, 0x10ffff)); return result.ToArray();
    }
}
internal sealed record RegexNode(string Op, RegexNode[] Children, int Value = 0, int Maximum = 0, bool Lazy = false, RegexClass? Class = null)
{
    internal bool Nullable => Op switch { "char" or "any" or "class" => false, "concat" => Children.All(child => child.Nullable), "alt" => Children.Any(child => child.Nullable), "group" or "plus" => Children[0].Nullable, "repeat" => Value == 0 || Children[0].Nullable, _ => true };
}
internal sealed class RegexParser
{
    private readonly int[] points; private readonly bool multiline, dotAll, newlineSensitive;
    private int position, depth;
    internal int Groups { get; private set; }
    internal RegexParser(string pattern, bool multiline = false, bool dotAll = false, bool newlineSensitive = false)
    {
        TextEncoding.Validate(pattern); points = pattern.EnumerateRunes().Select(rune => rune.Value).ToArray();
        this.multiline = multiline; this.dotAll = dotAll; this.newlineSensitive = newlineSensitive;
    }
    internal RegexNode Parse() { var result = Alternation(); if (position != points.Length) throw Bad("unbalanced parenthesis"); return result; }
    internal static SqlException Bad(string detail) => new("2201B", "invalid regular expression: " + detail);
    internal static SqlException Complex() => new("54001", "regular expression is too complex");
    private bool Take(int point) { if (position == points.Length || points[position] != point) return false; position++; return true; }
    private RegexNode Alternation()
    {
        if (++depth > 256) throw Complex();
        var branches = new List<RegexNode> { Concatenation() }; while (Take('|')) branches.Add(Concatenation()); depth--;
        return branches.Count == 1 ? branches[0] : new("alt", branches.ToArray());
    }
    private RegexNode Concatenation()
    {
        var nodes = new List<RegexNode>();
        while (position < points.Length && points[position] is not ('|' or ')'))
        {
            var point = points[position];
            if (point is '*' or '+' or '?' || point == '{' && Interval(position, out _, out _, out _)) throw Bad("quantifier operand missing");
            var node = Atom();
            if (position < points.Length)
            {
                point = points[position];
                if (point is '*' or '+' or '?') { position++; node = new(point == '*' ? "star" : point == '+' ? "plus" : "quest", [node], Lazy: Take('?')); }
                else if (point == '{' && Interval(position, out var minimum, out var maximum, out var end)) { position = end; if (maximum >= 0 && maximum < minimum) throw Bad("invalid repetition count"); node = new("repeat", [node], minimum, maximum, Take('?')); }
            }
            nodes.Add(node);
        }
        return nodes.Count == 1 ? nodes[0] : new("concat", nodes.ToArray());
    }
    private bool Interval(int start, out int minimum, out int maximum, out int end)
    {
        var at = start + 1; minimum = maximum = 0; end = start;
        int Number()
        {
            var value = 0;
            while (at < points.Length && points[at] is >= '0' and <= '9') { value = Math.Min(32769, value * 10 + points[at++] - '0'); }
            return value;
        }
        if (at == points.Length || points[at] is not (>= '0' and <= '9')) return false;
        minimum = Number(); maximum = minimum;
        if (at < points.Length && points[at] == ',')
        {
            at++; maximum = -1;
            if (at < points.Length && points[at] is >= '0' and <= '9') maximum = Number();
        }
        if (at == points.Length || points[at] != '}') return false; end = at + 1; return true;
    }
    private RegexNode Atom()
    {
        var point = points[position++];
        switch (point)
        {
            case '(':
                var group = 0;
                if (Take('?')) { if (!Take(':')) throw Bad("unbalanced parenthesis"); } else group = ++Groups;
                var body = Alternation(); if (!Take(')')) throw Bad("unbalanced parenthesis");
                return group == 0 ? body : new("group", [body], group);
            case '[': return Bracket();
            case '\\': return Escape(false);
            case '.': return dotAll ? new("class", [], Class: new(true, [])) : new("any", []);
            case '^': return new(multiline ? "assertlinestart" : "assertstart", []);
            case '$': return new(multiline ? "assertlineend" : "assertend", []);
            default: return new("char", [], point);
        }
    }
    private RegexNode Escape(bool bracket)
    {
        if (position == points.Length) throw Bad("trailing backslash"); var point = points[position++];
        if (!bracket && point is 'A' or 'z') return new(point == 'A' ? "assertstart" : "assertend", []);
        var lower = point is >= 'A' and <= 'Z' ? point + 32 : point;
        if (lower is 'd' or 'w' or 's')
        {
            RegexRange[] ranges = lower switch { 'd' => [new('0', '9')], 'w' => [new('0', '9'), new('A', 'Z'), new('_', '_'), new('a', 'z')], _ => [new(9, 13), new(32, 32)] };
            return new("class", [], Class: new(point != lower, ranges));
        }
        var control = point switch { 'n' => 10, 't' => 9, 'r' => 13, 'f' => 12, 'v' => 11, _ => -1 };
        if (control >= 0) return new("char", [], control);
        if (point is >= 'A' and <= 'Z' or >= 'a' and <= 'z' or >= '0' and <= '9') throw Bad("invalid escape \\" + new Rune(point));
        return new("char", [], point);
    }
    private RegexNode Bracket()
    {
        var negated = Take('^'); var ranges = new List<RegexRange>(); var first = true;
        RegexNode Member()
        {
            if (position == points.Length) throw Bad("unbalanced bracket expression");
            var point = points[position++];
            if (point == '[' && position < points.Length && points[position] is ':' or '.' or '=') throw Bad("unbalanced bracket expression");
            return point == '\\' ? Escape(true) : new("char", [], point);
        }
        while (position < points.Length && (first || points[position] != ']'))
        {
            var member = Member();
            if (member.Op == "char" && !(first && member.Value == '-') && position + 1 < points.Length && points[position] == '-' && points[position + 1] != ']')
            {
                position++; var last = Member(); if (last.Op != "char" || last.Value < member.Value) throw Bad("unbalanced bracket expression");
                ranges.Add(new(member.Value, last.Value));
            }
            else if (member.Op == "char") ranges.Add(new(member.Value, member.Value)); else ranges.AddRange(member.Class!.PositiveRanges());
            first = false;
        }
        if (!Take(']')) throw Bad("unbalanced bracket expression");
        if (negated && newlineSensitive) ranges.Add(new(10, 10));
        return new("class", [], Class: RegexClass.Make(negated, ranges));
    }
}
