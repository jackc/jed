using System.Text;

namespace Jed;

internal readonly record struct RegexInstruction(string Op, int A = 0, int B = 0)
{
    internal string Listing => Op switch { "split" => $"split {A} {B}", "jmp" or "char" or "class" or "save" => $"{Op} {A}", _ => Op };
}
internal sealed class RegexProgram
{
    internal const int MaximumProgram = 32768;
    internal RegexInstruction[] Code { get; }
    internal RegexClass[] Classes { get; }
    internal int Groups { get; }
    internal bool IgnoreCase { get; }
    private readonly UnicodeData? unicode;
    private RegexProgram(RegexInstruction[] code, RegexClass[] classes, int groups, bool ignoreCase, UnicodeData? unicode)
    { Code = code; Classes = classes; Groups = groups; IgnoreCase = ignoreCase; this.unicode = unicode; }
    internal string FoldSubject(string input) => IgnoreCase ? UnicodeData.Fold(input, false, unicode, false) : input;
    internal static RegexProgram Compile(string pattern, string flags = "", UnicodeData? unicode = null, bool jsonPath = false, bool gated = false, bool rightmost = false)
    {
        var ignoreCase = flags.Contains('i'); var quote = flags.Contains('q');
        if (ignoreCase)
        {
            if (quote) pattern = UnicodeData.Fold(pattern, false, unicode, false);
            else
            {
                var output = new StringBuilder(); var escaped = false;
                foreach (var rune in pattern.EnumerateRunes())
                {
                    output.Append(escaped || rune.Value == '\\' ? rune.ToString() : UnicodeData.Fold(rune.ToString(), false, unicode, false));
                    if (escaped) escaped = false; else if (rune.Value == '\\') escaped = true;
                }
                pattern = output.ToString();
            }
        }
        RegexNode node; var groups = 0;
        if (quote) node = new("concat", pattern.EnumerateRunes().Select(rune => new RegexNode("char", [], rune.Value)).ToArray());
        else { var parser = new RegexParser(pattern, flags.Contains('m'), flags.Contains('s'), jsonPath && !flags.Contains('s')); node = parser.Parse(); groups = parser.Groups; }
        var emitter = new Emitter(); emitter.Check(node, gated ? 7 : 6); emitter.Add(rightmost ? new("split", 1, 3) : new("split", 3, 1)); emitter.Add(new("anychar")); emitter.Add(new("jmp", 0)); if (gated) emitter.Add(new("gate")); emitter.Add(new("save", 0));
        emitter.Emit(node); emitter.Add(new("save", 1)); emitter.Add(new("match"));
        return new(emitter.Code.ToArray(), emitter.Classes.ToArray(), groups, ignoreCase, unicode);
    }
    private sealed class Emitter
    {
        internal readonly List<RegexInstruction> Code = [];
        internal readonly List<RegexClass> Classes = [];
        internal int Add(RegexInstruction instruction) { if (Code.Count == MaximumProgram) throw RegexParser.Complex(); Code.Add(instruction); return Code.Count - 1; }
        internal void Check(RegexNode node, int wrapper) { if (Size(node) + wrapper > MaximumProgram) throw RegexParser.Complex(); }
        private static long Size(RegexNode node)
        {
            var sizes = node.Children.Select(Size).ToArray();
            var count = node.Op switch
            {
                "concat" => sizes.Sum(), "alt" => sizes.Sum() + 2 * (sizes.Length - 1), "group" => sizes[0] + 2,
                "star" => sizes[0] + 2, "plus" or "quest" => sizes[0] + 1,
                "repeat" => node.Maximum < 0 ? sizes[0] * node.Value + sizes[0] + 2 : sizes[0] * node.Maximum + node.Maximum - node.Value,
                _ => 1
            };
            if (count > MaximumProgram) throw RegexParser.Complex(); return count;
        }
        internal void Emit(RegexNode node)
        {
            switch (node.Op)
            {
                case "concat": foreach (var child in node.Children) Emit(child); break;
                case "alt":
                    var jumps = new List<int>();
                    for (var i = 0; i < node.Children.Length - 1; i++)
                    {
                        var split = Add(default); var first = Code.Count; Emit(node.Children[i]); var jump = Add(default); jumps.Add(jump); Code[split] = new("split", first, Code.Count);
                    }
                    Emit(node.Children[^1]); foreach (var jump in jumps) Code[jump] = new("jmp", Code.Count); break;
                case "group": Add(new("save", node.Value * 2)); Emit(node.Children[0]); Add(new("save", node.Value * 2 + 1)); break;
                case "star":
                    if (node.Children[0].Nullable) { Emit(new("quest", [new("plus", node.Children, Lazy: node.Lazy)], Lazy: node.Lazy)); break; }
                    var start = Add(default); var body = Code.Count; Emit(node.Children[0]); Add(new("jmp", start)); Code[start] = node.Lazy ? new("split", Code.Count, body) : new("split", body, Code.Count); break;
                case "plus":
                    var firstBody = Code.Count; Emit(node.Children[0]); var loop = Code.Count; Add(node.Lazy ? new("split", loop + 1, firstBody) : new("split", firstBody, loop + 1)); break;
                case "quest":
                    var choice = Add(default); var optional = Code.Count; Emit(node.Children[0]); Code[choice] = node.Lazy ? new("split", Code.Count, optional) : new("split", optional, Code.Count); break;
                case "repeat":
                    for (var i = 0; i < node.Value; i++) Emit(node.Children[0]);
                    if (node.Maximum < 0) Emit(new("star", node.Children, Lazy: node.Lazy));
                    else for (var i = node.Value; i < node.Maximum; i++) Emit(new("quest", node.Children, Lazy: node.Lazy)); break;
                case "class": var slot = Classes.Count; Classes.Add(node.Class!); Add(new("class", slot)); break;
                default: Add(new(node.Op, node.Value)); break;
            }
        }
    }
}
