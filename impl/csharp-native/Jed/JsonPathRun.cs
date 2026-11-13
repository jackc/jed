using System.Globalization;
using System.Text;

namespace Jed;

/// <summary>Depth-first SQL/JSON sequence evaluator. Accessor chains use an explicit stack.</summary>
internal readonly record struct JsonPathBinding(string Name, SqlJsonNode Value, int Base);

internal sealed class JsonPathRun(SqlJsonPath path, SqlJsonNode document, SqlJsonNode variables, Meter meter, int variableBases = 1, JsonPathBinding[]? passing = null)
{
    private readonly record struct Item(SqlJsonNode Node, int Last = -1, bool Suppress = false, long Base = 0, long Offset = 0);
    private long nextBase = variableBases + 1;
    private readonly Dictionary<(long Base, SqlJsonNode Node), long> offsets = new();
    private readonly Dictionary<PathExpr, RegexMachine> regex = new(ReferenceEqualityComparer.Instance);
    private readonly JsonPathBinding[]? bindings = passing?.OrderBy(binding => binding.Name, StringComparer.Ordinal).ToArray();
    private void Charge(long amount = 1) { meter.Charge(amount); meter.Guard(); }
    private static SqlException Error(string state, string message = "invalid SQL/JSON path operation") => new(state, message);
    internal bool Failed { get; private set; }
    internal (List<SqlJsonNode> Items, bool Failed) Run(bool silent = false, bool exists = false, bool chargeItems = true)
    {
        var result = new List<SqlJsonNode>();
        foreach (var item in Sequence(silent, exists)) { if (chargeItems) Charge(); result.Add(item); }
        return (result, Failed);
    }
    internal IEnumerable<SqlJsonNode> Sequence(bool silent = false, bool exists = false)
    {
        if (variables.Kind != SqlJsonKind.Object) throw Error("22023", "vars argument is not an object");
        Charge(TextEncoding.ByteCount(path.Text) * Costs.JsonpathCompile);
        var program = SqlJsonPath.Parse(path.Text);
        var pending = new Stack<PathExpr>(); pending.Push(program.Root);
        while (pending.TryPop(out var expression))
        {
            if (expression.Op == "regex")
            {
                var compiled = RegexProgram.Compile(expression.Args[1].Value!.Text!, expression.Text, meter.Unicode, jsonPath: true);
                Charge(compiled.Code.Length * Costs.RegexCompile); regex.Add(expression, new(compiled));
            }
            for (var i = expression.Args.Length - 1; i >= 0; i--) pending.Push(expression.Args[i]);
        }
        Layout(document, 0, 0);
        if (bindings is null) Layout(variables, 0, 1);
        else foreach (var binding in bindings) Layout(binding.Value, 0, binding.Base);
        using var iterator = Eval(program.Root, new(document), exists && !path.Strict).GetEnumerator();
        while (true)
        {
            bool more;
            try { more = iterator.MoveNext(); }
            catch (SqlException error) when (silent && Suppressible(error)) { Failed = true; more = false; }
            if (!more) yield break;
            yield return iterator.Current.Node; if (exists && !path.Strict) yield break;
        }
    }
    private static bool Suppressible(SqlException error) => error.SqlState.StartsWith("22", StringComparison.Ordinal);
    private static bool Accessor(PathExpr expression) => expression.Op is "member" or "members" or "elements" or "index" or "filter" or "method" or "descendants";
    private IEnumerable<Item> Eval(PathExpr expression, Item current, bool exists = false)
    {
        if (!Accessor(expression)) return Basic(expression, current, exists);
        var steps = new List<PathExpr>(); var seed = expression;
        while (Accessor(seed)) { steps.Add(seed); seed = seed.Args[0]; }
        steps.Reverse(); return Chain(seed, steps, current, exists);
    }
    private IEnumerable<Item> Chain(PathExpr seed, List<PathExpr> steps, Item current, bool exists)
    {
        var stack = new Stack<(IEnumerator<Item> Items, int Step)>(); stack.Push((Basic(seed, current, false).GetEnumerator(), 0));
        try
        {
            while (stack.Count != 0)
            {
                var frame = stack.Peek(); if (!frame.Items.MoveNext()) { stack.Pop().Items.Dispose(); continue; }
                var item = frame.Items.Current;
                if (frame.Step == steps.Count) { yield return item; continue; }
                stack.Push((Apply(steps[frame.Step], item, current, exists).GetEnumerator(), frame.Step + 1));
            }
        }
        finally { while (stack.Count != 0) stack.Pop().Items.Dispose(); }
    }
    private IEnumerable<Item> Basic(PathExpr expression, Item current, bool exists)
    {
        if (expression.Predicate) { yield return new(TruthNode(Truth(expression, current))); yield break; }
        switch (expression.Op)
        {
            case "root": yield return new(document, Suppress: current.Suppress); yield break;
            case "current": yield return current; yield break;
            case "last": yield return new(SqlJsonNode.Numeric(ExactDecimal.FromInt64(current.Last))); yield break;
            case "literal": yield return new(expression.Value!); yield break;
            case "variable":
                if (bindings is not null)
                {
                    var low = 0; var high = bindings.Length;
                    while (low < high) { var middle = low + (high - low) / 2; if (StringComparer.Ordinal.Compare(bindings[middle].Name, expression.Text) < 0) low = middle + 1; else high = middle; }
                    if (low == bindings.Length || bindings[low].Name != expression.Text) throw Error("42704", "could not find jsonpath variable " + expression.Text);
                    var binding = bindings[low]; yield return new(binding.Value, Suppress: current.Suppress, Base: binding.Base); yield break;
                }
                var member = variables.Members.FirstOrDefault(member => member.Key == expression.Text);
                if (member.Value is null) throw Error("42704", "could not find jsonpath variable " + expression.Text);
                yield return new(member.Value, Suppress: current.Suppress, Base: 1, Offset: offsets.GetValueOrDefault((1, member.Value))); yield break;
            case "+": case "-": case "*": case "/": case "%":
                if (expression.Args.Length == 1)
                {
                    foreach (var item in Unwrap(Eval(expression.Args[0], current), !path.Strict))
                    {
                        Charge(); if (item.Node.Kind != SqlJsonKind.Number) { if (exists && !path.Strict) continue; throw Error("2203B"); }
                        Charge(); yield return item with { Node = SqlJsonNode.Numeric(expression.Op == "-" ? item.Node.Number!.Negate() : item.Node.Number!) };
                    }
                    yield break;
                }
                var left = Unwrap(Eval(expression.Args[0], current), !path.Strict).ToArray(); var right = Unwrap(Eval(expression.Args[1], current), !path.Strict).ToArray();
                if (left.Length != 1 || left[0].Node.Kind != SqlJsonKind.Number || right.Length != 1 || right[0].Node.Kind != SqlJsonKind.Number) throw Error("22038");
                var a = left[0].Node.Number!; var b = right[0].Node.Number!; Charge();
                if (!(b.IsZero && expression.Op is "/" or "%")) Charge(a.DecimalWork(b, expression.Op));
                var value = expression.Op switch { "+" => a.Add(b), "-" => a.Subtract(b), "*" => a.Multiply(b), "/" => a.Divide(b), _ => a.Remainder(b) };
                Charge(); yield return new(SqlJsonNode.Numeric(value)); yield break;
            default: throw Error("42601");
        }
    }
    private IEnumerable<Item> Unwrap(IEnumerable<Item> source, bool unwrap)
    {
        foreach (var item in source)
            if (unwrap && item.Node.Kind == SqlJsonKind.Array) { foreach (var child in item.Node.Elements) yield return Child(item, child); }
            else yield return item;
    }
    private Item Child(Item parent, SqlJsonNode node) => parent with { Node = node, Offset = offsets.GetValueOrDefault((parent.Base, node)) };
    private IEnumerable<Item> Apply(PathExpr step, Item input, Item current, bool exists)
    {
        var unwrap = !path.Strict && (step.Op is "member" or "members" or "filter" || step.Op == "method" && step.Text is not ("type" or "size"));
        foreach (var item in Unwrap([input], unwrap))
        {
            Charge(); var node = item.Node; var suppress = !path.Strict || item.Suppress;
            switch (step.Op)
            {
                case "member":
                    if (node.Kind != SqlJsonKind.Object) { if (!suppress) throw Error("2203A"); break; }
                    var found = false;
                    foreach (var member in node.Members) { Charge(); if (member.Key != step.Text) continue; found = true; Charge(); yield return Child(item, member.Value); break; }
                    if (!found && !suppress) throw Error("2203A"); break;
                case "members":
                    if (node.Kind != SqlJsonKind.Object) { if (!suppress) throw Error("2203C"); break; }
                    foreach (var member in node.Members) { Charge(); yield return Child(item, member.Value); } break;
                case "elements": case "index":
                    if (node.Kind != SqlJsonKind.Array && path.Strict) { if (!suppress) throw Error("22039"); break; }
                    var elements = node.Kind == SqlJsonKind.Array ? node.Elements : new[] { node };
                    var indexed = item with { Last = elements.Count - 1 };
                    if (step.Op == "elements") { foreach (var element in elements) { Charge(); yield return Child(indexed, element); } break; }
                    foreach (var subscript in step.Args.Skip(1))
                    {
                        var first = Index(subscript.Op == "range" ? subscript.Args[0] : subscript, current with { Last = elements.Count - 1 });
                        var last = subscript.Op == "range" ? Index(subscript.Args[1], current with { Last = elements.Count - 1 }) : first;
                        if (!suppress && (first < 0 || last >= elements.Count || first > last)) throw Error("22033");
                        first = Math.Max(0, first); last = Math.Min(elements.Count - 1, last);
                        for (var index = first; index <= last; index++) { Charge(); yield return Child(indexed, elements[index]); }
                    }
                    break;
                case "filter": if (Truth(step.Args[1], item) == true) { Charge(); yield return item; } break;
                case "method":
                    if (step.Text == "keyvalue")
                    {
                        if (node.Kind != SqlJsonKind.Object) throw Error("2203C");
                        var id = SqlJsonNode.Numeric(ExactDecimal.FromInt64(checked(item.Base * 10_000_000_000 + item.Offset)));
                        foreach (var member in node.Members)
                        {
                            var pair = SqlJsonNode.Object([new("id", id), new("key", SqlJsonNode.String(member.Key)), new("value", member.Value)]);
                            var baseNumber = nextBase++; Layout(pair, 0, baseNumber); Charge(); yield return new(pair, item.Last, item.Suppress, baseNumber);
                        }
                    }
                    else
                    {
                        if (step.Text == "size" && node.Kind != SqlJsonKind.Array && path.Strict && item.Suppress) break;
                        var result = Method(step, node); Charge(); yield return item with { Node = result };
                    }
                    break;
                case "descendants":
                    var bounds = step.Text.Trim('{', '}').Split(" to ", StringSplitOptions.None);
                    var lower = bounds[0] == "" ? 0 : bounds[0] == "last" ? int.MaxValue : int.Parse(bounds[0], CultureInfo.InvariantCulture);
                    var upper = bounds[0] == "" ? int.MaxValue : bounds[^1] == "last" ? int.MaxValue : int.Parse(bounds[^1], CultureInfo.InvariantCulture);
                    if (lower == 0) { Charge(); yield return item with { Suppress = true }; }
                    var pending = new Stack<(SqlJsonNode Node, int Level)>();
                    void Push(SqlJsonNode value, int level) { var children = value.Kind == SqlJsonKind.Array ? value.Elements : value.Members.Select(member => member.Value).ToArray(); for (var i = children.Count - 1; i >= 0; i--) pending.Push((children[i], level)); }
                    if (upper > 0) Push(node, 1);
                    while (pending.Count != 0)
                    {
                        var child = pending.Pop(); Charge();
                        if (child.Level >= lower || lower == int.MaxValue && upper == int.MaxValue && JsonOps.Scalar(child.Node)) { Charge(); yield return Child(item, child.Node) with { Suppress = true }; }
                        if (child.Level < upper) Push(child.Node, child.Level + 1);
                    }
                    break;
            }
        }
    }
    private int Index(PathExpr expression, Item current)
    {
        var items = Eval(expression, current).ToArray();
        if (items.Length != 1 || items[0].Node.Kind != SqlJsonKind.Number) throw Error("22033");
        try { var value = items[0].Node.Number!.Truncate().ToInt64(); if (value < int.MinValue || value > int.MaxValue) throw Error("22033"); return (int)value; }
        catch (SqlException) { throw Error("22033"); }
    }
    private static SqlJsonNode TruthNode(bool? truth) => truth.HasValue ? SqlJsonNode.Boolean(truth.Value) : SqlJsonNode.Null;
    private bool? Truth(PathExpr expression, Item current)
    {
        if (expression.Op == "!") { var value = Truth(expression.Args[0], current); return value.HasValue ? !value.Value : null; }
        if (expression.Op == "unknown") return !Truth(expression.Args[0], current).HasValue;
        if (expression.Op is "&&" or "||")
        {
            var left = Truth(expression.Args[0], current); var right = Truth(expression.Args[1], current);
            return expression.Op == "&&" ? left == false || right == false ? false : left == true && right == true ? true : null : left == true || right == true ? true : left == false && right == false ? false : null;
        }
        try
        {
            if (expression.Op == "exists") { var sequence = Eval(expression.Args[0], current, !path.Strict); return path.Strict ? sequence.Count() > 0 : sequence.Any(); }
            var left = Unwrap(Eval(expression.Args[0], current), !path.Strict).ToArray();
            var right = expression.Op == "regex" ? [] : Unwrap(Eval(expression.Args[1], current), !path.Strict && expression.Op != "starts").ToArray();
            var unknown = false; var any = false;
            foreach (var a in left)
            {
                if (expression.Op == "regex")
                {
                    Charge();
                    if (a.Node.Kind != SqlJsonKind.String) { if (path.Strict) return null; unknown = true; }
                    else if (regex[expression].Search(a.Node.Text!, meter: meter).Captures is not null) { if (!path.Strict) return true; any = true; }
                    continue;
                }
                foreach (var b in right)
                {
                    Charge(); bool? pair;
                    if (a.Node.Kind == SqlJsonKind.Number && b.Node.Kind == SqlJsonKind.Number) Charge(a.Node.Number!.DecimalWork(b.Node.Number!, "+"));
                    if (a.Node.Kind == SqlJsonKind.String && b.Node.Kind == SqlJsonKind.String) Charge(Math.Max(0, Math.Min(a.Node.Text!.EnumerateRunes().Count(), b.Node.Text!.EnumerateRunes().Count()) - 1));
                    if (expression.Op == "starts") pair = a.Node.Kind == SqlJsonKind.String && b.Node.Kind == SqlJsonKind.String ? a.Node.Text!.StartsWith(b.Node.Text!, StringComparison.Ordinal) : null;
                    else if (a.Node.Kind == SqlJsonKind.Null || b.Node.Kind == SqlJsonKind.Null)
                        pair = a.Node.Kind == b.Node.Kind ? CompareResult(expression.Op, 0) : expression.Op is "!=" or "<>";
                    else if (!JsonOps.Scalar(a.Node) || !JsonOps.Scalar(b.Node) || a.Node.Kind != b.Node.Kind && !(a.Node.Kind is SqlJsonKind.False or SqlJsonKind.True && b.Node.Kind is SqlJsonKind.False or SqlJsonKind.True)) pair = null;
                    else pair = CompareResult(expression.Op, JsonOps.Compare(a.Node, b.Node));
                    if (pair == true) { if (!path.Strict) return true; any = true; }
                    else if (!pair.HasValue) { if (path.Strict) return null; unknown = true; }
                }
            }
            return any ? true : unknown ? null : false;
        }
        catch (SqlException error) when (Suppressible(error)) { return null; }
    }
    private static bool CompareResult(string operation, int value) => operation switch { "==" => value == 0, "!=" or "<>" => value != 0, "<" => value < 0, "<=" => value <= 0, ">" => value > 0, ">=" => value >= 0, _ => false };
    private SqlJsonNode Method(PathExpr method, SqlJsonNode node)
    {
        if (method.Text == "type") return SqlJsonNode.String(node.Kind switch { SqlJsonKind.Null => "null", SqlJsonKind.False or SqlJsonKind.True => "boolean", SqlJsonKind.Number => "number", SqlJsonKind.String => "string", SqlJsonKind.Array => "array", _ => "object" });
        if (method.Text == "size") { if (node.Kind != SqlJsonKind.Array && path.Strict) throw Error("22039"); return SqlJsonNode.Numeric(ExactDecimal.FromInt64(node.Kind == SqlJsonKind.Array ? node.Elements.Count : 1)); }
        if (method.Text is "date" or "datetime" or "time" or "time_tz" or "timestamp" or "timestamp_tz") throw Errors.Unsupported("JSON path datetime methods");
        try
        {
            if (method.Text is "abs" or "floor" or "ceiling")
            {
                if (node.Kind != SqlJsonKind.Number) throw Error("22036");
                return SqlJsonNode.Numeric(method.Text switch { "abs" => node.Number!.Abs(), "floor" => node.Number!.Floor(), _ => node.Number!.Ceiling() });
            }
            if (method.Text == "boolean")
            {
                if (node.Kind is SqlJsonKind.True or SqlJsonKind.False) return node;
                if (node.Kind == SqlJsonKind.Number) { var integer = IntegerInput.ParseText(node.Number!.ToString(), SqlType.I32); return SqlJsonNode.Boolean(integer != 0); }
                if (node.Kind != SqlJsonKind.String) throw Error("22036"); var text = TextEncoding.FoldName(node.Text!);
                if (text is "1" or "on" || text.Length > 0 && ("true".StartsWith(text, StringComparison.Ordinal) || "yes".StartsWith(text, StringComparison.Ordinal))) return SqlJsonNode.Boolean(true);
                if (text is "0" or "of" or "off" || text.Length > 0 && ("false".StartsWith(text, StringComparison.Ordinal) || "no".StartsWith(text, StringComparison.Ordinal))) return SqlJsonNode.Boolean(false);
                throw Error("22036");
            }
            if (method.Text == "string") { if (node.Kind is not (SqlJsonKind.String or SqlJsonKind.Number or SqlJsonKind.True or SqlJsonKind.False)) throw Error("22036"); return node.Kind == SqlJsonKind.String ? node : SqlJsonNode.String(node.ToString()); }
            if (node.Kind is not (SqlJsonKind.Number or SqlJsonKind.String)) throw Error("22036");
            if (method.Text is "integer" or "bigint")
            {
                var type = method.Text == "integer" ? SqlType.I32 : SqlType.I64;
                var integer = node.Kind == SqlJsonKind.String ? IntegerInput.ParseText(node.Text!, type) : SqlTypes.Check(node.Number!.Round().ToInt64(), type);
                return SqlJsonNode.Numeric(ExactDecimal.FromInt64(integer));
            }
            if (method.Text == "double")
            {
                var floating = FloatValue.Parse(node.Kind == SqlJsonKind.String ? node.Text! : node.Number!.ToString()); if (!double.IsFinite(floating)) throw Error("22036");
                if (node.Kind == SqlJsonKind.Number) return node;
                var exact = FloatValue.ToDecimal(floating); var exponent = exact.IsZero ? 0 : exact.Digits - exact.Scale;
                return SqlJsonNode.Numeric(RoundSignificant(exact, 15 - exponent).TrimScale());
            }
            var number = node.Kind == SqlJsonKind.Number ? node.Number! : ExactDecimal.Parse(node.Text!);
            if (method.Text == "decimal" && method.Args.Length > 1)
            {
                var precision = method.Args[1].Value!.Number!.Truncate().ToInt64(); var scale = method.Args.Length > 2 ? method.Args[2].Value!.Number!.Truncate().ToInt64() : 0;
                if (precision < int.MinValue || precision > int.MaxValue || scale < int.MinValue || scale > int.MaxValue) throw Error("22036");
                if (precision < 1 || precision > 1000 || scale < -1000 || scale > 1000) throw Error("22023"); number = number.Round((int)scale);
                if (!number.IsZero && number.Digits - number.Scale > precision - scale) throw Error("22036");
            }
            return SqlJsonNode.Numeric(number);
        }
        catch (SqlException error) when (error.SqlState != "22023") { throw Error("22036", "invalid argument for jsonpath item method " + method.Text); }
    }
    internal static ExactDecimal RoundSignificant(ExactDecimal value, int precision)
    {
        var absolute = value.Abs(); var shifted = ExactDecimal.Parse(absolute.ToString() + "e" + precision.ToString(CultureInfo.InvariantCulture));
        var rounded = NumberFormat.HalfEven(shifted, 0);
        var result = ExactDecimal.Parse(rounded.ToString() + "e" + (-precision).ToString(CultureInfo.InvariantCulture));
        return value.Negative ? result.Negate() : result;
    }
    private long Layout(SqlJsonNode node, long start, long baseNumber)
    {
        if (node.Kind == SqlJsonKind.String) return start + TextEncoding.ByteCount(node.Text!);
        if (node.Kind == SqlJsonKind.Number)
        {
            var digits = node.Number!.CoefficientDigits() + new string('0', (4 - node.Number.Scale % 4) % 4);
            var groups = (digits.Length + 3) / 4; var tail = digits.Length;
            while (tail >= 4 && digits.AsSpan(tail - 4, 4).SequenceEqual("0000")) { groups--; tail -= 4; }
            var nonzero = node.Number.IsZero ? 0 : groups;
            var weight = TemporalValue.FloorDiv(node.Number.Digits - node.Number.Scale - 1, 4);
            return (start + 3) / 4 * 4 + 4 + (node.Number.Scale <= 63 && weight is >= -64 and <= 63 ? 2 : 4) + nonzero * 2;
        }
        if (JsonOps.Scalar(node)) return start;
        start = (start + 3) / 4 * 4; offsets[(baseNumber, node)] = start;
        var at = start + 4 + (node.Kind == SqlJsonKind.Array ? node.Elements.Count * 4 : node.Members.Count * 8);
        if (node.Kind == SqlJsonKind.Array) foreach (var child in node.Elements) at = Layout(child, at, baseNumber);
        else { at += node.Members.Sum(member => TextEncoding.ByteCount(member.Key)); foreach (var member in node.Members) at = Layout(member.Value, at, baseNumber); }
        return at;
    }
}
