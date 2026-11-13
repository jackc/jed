using System.Globalization;
using System.Text;

namespace Jed;

internal static class JsonOps
{
    internal static int Compare(SqlJsonNode left, SqlJsonNode right, bool root = true)
    {
        if (root)
        {
            if (left.Kind == SqlJsonKind.Array && left.Elements.Count == 0 && Scalar(right)) return -1;
            if (right.Kind == SqlJsonKind.Array && right.Elements.Count == 0 && Scalar(left)) return 1;
        }
        var rank = Rank(left).CompareTo(Rank(right)); if (rank != 0) return rank;
        switch (left.Kind)
        {
            case SqlJsonKind.Null: return 0;
            case SqlJsonKind.False: case SqlJsonKind.True: return left.Kind.CompareTo(right.Kind);
            case SqlJsonKind.Number: return left.Number!.CompareTo(right.Number);
            case SqlJsonKind.String: return TextCompare(left.Text!, right.Text!);
            case SqlJsonKind.Array:
                var count = left.Elements.Count.CompareTo(right.Elements.Count); if (count != 0) return count;
                for (var i = 0; i < left.Elements.Count; i++) { var compared = Compare(left.Elements[i], right.Elements[i], false); if (compared != 0) return compared; } return 0;
            case SqlJsonKind.Object:
                var memberCount = left.Members.Count.CompareTo(right.Members.Count); if (memberCount != 0) return memberCount;
                for (var i = 0; i < left.Members.Count; i++)
                {
                    var compared = TextCompare(left.Members[i].Key, right.Members[i].Key); if (compared == 0) compared = Compare(left.Members[i].Value, right.Members[i].Value, false); if (compared != 0) return compared;
                }
                return 0;
            default: throw new InvalidOperationException();
        }
    }
    private static int Rank(SqlJsonNode value) => value.Kind switch { SqlJsonKind.Null => 0, SqlJsonKind.String => 1, SqlJsonKind.Number => 2, SqlJsonKind.False or SqlJsonKind.True => 3, SqlJsonKind.Array => 4, _ => 5 };
    internal static bool Scalar(SqlJsonNode value) => value.Kind is not (SqlJsonKind.Array or SqlJsonKind.Object);
    private static int TextCompare(string left, string right) => TextEncoding.Utf8.GetBytes(left).AsSpan().SequenceCompareTo(TextEncoding.Utf8.GetBytes(right));
    internal static bool Contains(SqlJsonNode left, SqlJsonNode right, bool root = true)
    {
        if (left.Kind == SqlJsonKind.Array && Scalar(right) && root) return left.Elements.Any(element => Scalar(element) && Compare(element, right, false) == 0);
        if (Rank(left) != Rank(right)) return false;
        if (left.Kind == SqlJsonKind.Object)
            return right.Members.All(wanted => left.Members.Any(member => member.Key == wanted.Key && Contains(member.Value, wanted.Value, false)));
        if (left.Kind == SqlJsonKind.Array)
            return right.Elements.All(wanted => left.Elements.Any(element => Contains(element, wanted, false)));
        return Compare(left, right, false) == 0;
    }
    internal static bool Exists(SqlJsonNode value, string key) => value.Kind switch
    {
        SqlJsonKind.Object => value.Members.Any(member => member.Key == key),
        SqlJsonKind.Array => value.Elements.Any(element => element.Kind == SqlJsonKind.String && element.Text == key),
        SqlJsonKind.String => value.Text == key,
        _ => false
    };
    internal static SqlJsonNode Node(SqlValue value, bool decode = true) => value.Type == SqlType.Jsonb ? value.AsJsonb() : JsonInput.Parse(value.AsJsonText(), decode, canonical: false);
    internal static SqlValue Extract(SqlValue value, SqlValue selector, bool text, bool path)
    {
        var outputType = text ? SqlType.Text : value.Type;
        if (value.IsNull || selector.IsNull) return SqlValue.Null(outputType);
        var root = Node(value); SqlJsonNode? found = null;
        if (!path)
        {
            if (selector.Type == SqlType.Text)
                found = root.Kind == SqlJsonKind.Object ? root.Members.LastOrDefault(member => member.Key == selector.AsText()).Value : null;
            else
            {
                var index = selector.AsInteger();
                if (root.Kind == SqlJsonKind.Array) { if (index < 0) index += root.Elements.Count; if (index >= 0 && index < root.Elements.Count) found = root.Elements[(int)index]; }
                else if (value.Type == SqlType.Jsonb && Scalar(root) && index is 0 or -1) found = root;
            }
        }
        else
        {
            var steps = selector.AsArray().Values; if (steps.Any(step => step.IsNull)) return SqlValue.Null(outputType);
            var firstArrays = new Dictionary<int, int>();
            if (value.Type == SqlType.Json) RecordArrays(root, 0);
            void RecordArrays(SqlJsonNode node, int depth)
            {
                if (node.Kind == SqlJsonKind.Array) { firstArrays.TryAdd(depth, node.Elements.Count); foreach (var child in node.Elements) RecordArrays(child, depth + 1); }
                else if (node.Kind == SqlJsonKind.Object) foreach (var member in node.Members) RecordArrays(member.Value, depth + 1);
            }
            void Walk(SqlJsonNode node, int depth)
            {
                if (depth == steps.Count) { found = node; return; }
                var step = steps[depth].AsText();
                if (node.Kind == SqlJsonKind.Object) { foreach (var member in node.Members) if (member.Key == step) Walk(member.Value, depth + 1); }
                else if (node.Kind == SqlJsonKind.Array && Index(step) is { } index)
                {
                    if (index < 0) index += value.Type == SqlType.Json ? firstArrays.GetValueOrDefault(depth, node.Elements.Count) : node.Elements.Count;
                    if (index >= 0 && index < node.Elements.Count) Walk(node.Elements[index], depth + 1);
                }
            }
            Walk(root, 0);
            if (steps.Count == 0 && value.Type == SqlType.Json && Scalar(root) && (!text || root.Kind != SqlJsonKind.String && root.Kind != SqlJsonKind.Null))
                return text ? SqlValue.Text(value.AsJsonText()[..(root.Start + root.Length)]) : SqlValue.StoredJson(value.AsJsonText()[..(root.Start + root.Length)]);
        }
        if (found is null || text && found.Kind == SqlJsonKind.Null) return SqlValue.Null(outputType);
        if (text) return SqlValue.Text(found.Kind == SqlJsonKind.String ? found.Text! : value.Type == SqlType.Json ? found.Verbatim : found.ToString());
        return value.Type == SqlType.Jsonb ? SqlValue.Jsonb(found) : SqlValue.StoredJson(found.Verbatim);
    }
    internal static int? Index(string input)
    {
        var value = input.AsSpan().TrimStart(" \t\r\n\f\v");
        if (value.IsEmpty) return null; var begin = value[0] is '+' or '-' ? 1 : 0;
        if (begin == value.Length) return null; for (var i = begin; i < value.Length; i++) if (!char.IsAsciiDigit(value[i])) return null;
        return int.TryParse(value, NumberStyles.AllowLeadingSign, CultureInfo.InvariantCulture, out var parsed) ? parsed : null;
    }
    internal static SqlValue Binary(string operation, SqlValue left, SqlValue right)
    {
        if (operation is "->" or "->>" or "#>" or "#>>") return Extract(left, right, operation.EndsWith(">>", StringComparison.Ordinal), operation[0] == '#');
        var resultType = operation is "||" or "-" or "#-" ? SqlType.Jsonb : SqlType.Boolean;
        if (left.IsNull || right.IsNull) return SqlValue.Null(resultType);
        var node = left.AsJsonb();
        switch (operation)
        {
            case "@>": return SqlValue.Boolean(Contains(node, right.AsJsonb()));
            case "<@": return SqlValue.Boolean(Contains(right.AsJsonb(), node));
            case "?": return SqlValue.Boolean(Exists(node, right.AsText()));
            case "?|": return SqlValue.Boolean(right.AsArray().Values.Any(key => !key.IsNull && Exists(node, key.AsText())));
            case "?&": return SqlValue.Boolean(right.AsArray().Values.All(key => key.IsNull || Exists(node, key.AsText())));
            case "||":
                var other = right.AsJsonb();
                return SqlValue.Jsonb(node.Kind == SqlJsonKind.Object && other.Kind == SqlJsonKind.Object ? SqlJsonNode.Object(node.Members.Concat(other.Members)) : SqlJsonNode.Array((node.Kind == SqlJsonKind.Array ? node.Elements : [node]).Concat(other.Kind == SqlJsonKind.Array ? other.Elements : [other])));
            case "-":
                if (Scalar(node)) throw new SqlException("22023", "cannot delete from scalar");
                if (right.Type.IsInteger())
                {
                    if (node.Kind == SqlJsonKind.Object) throw new SqlException("22023", "cannot delete from object using integer index");
                    var index = right.AsInteger(); if (index < 0) index += node.Elements.Count;
                    return SqlValue.Jsonb(SqlJsonNode.Array(node.Elements.Where((_, i) => i != index)));
                }
                var keys = right.Type.Kind == SqlKind.Array ? right.AsArray().Values.Where(v => !v.IsNull).Select(v => v.AsText()).ToHashSet(StringComparer.Ordinal) : new HashSet<string>(StringComparer.Ordinal) { right.AsText() };
                if (right.Type.Kind == SqlKind.Array && right.AsArray().Dimensions.Count > 1) throw new SqlException("2202E", "wrong number of array subscripts");
                return SqlValue.Jsonb(node.Kind == SqlJsonKind.Object ? SqlJsonNode.Object(node.Members.Where(member => !keys.Contains(member.Key))) : SqlJsonNode.Array(node.Elements.Where(element => element.Kind != SqlJsonKind.String || !keys.Contains(element.Text!))));
            case "#-": return SqlValue.Jsonb(Mutate(node, right.AsArray(), null, "delete", false));
            default: throw Errors.Unsupported($"JSON operator {operation}");
        }
    }
    internal static SqlJsonNode Mutate(SqlJsonNode root, SqlArray path, SqlJsonNode? replacement, string operation, bool flag)
    {
        if (path.Dimensions.Count > 1) throw new SqlException("2202E", "wrong number of array subscripts");
        if (Scalar(root)) throw new SqlException("22023", operation == "delete" ? "cannot delete path in scalar" : "cannot set path in scalar");
        if (path.Values.Count == 0) return root;
        if (root.Elements.Count == 0 && root.Members.Count == 0 && (operation == "delete" || operation == "set" && !flag)) return root;
        SqlJsonNode Walk(SqlJsonNode node, int depth)
        {
            if (path.Values[depth].IsNull) throw new SqlException("22004", $"path element at position {depth + 1} is null");
            if (Scalar(node)) return node;
            var key = path.Values[depth].AsText(); var last = depth == path.Values.Count - 1;
            if (node.Kind == SqlJsonKind.Object)
            {
                var members = node.Members.ToList(); var index = members.FindIndex(member => member.Key == key);
                if (index < 0) { if (last && (operation == "insert" || operation == "set" && flag)) members.Add(new(key, replacement!)); }
                else if (!last) members[index] = new(key, Walk(members[index].Value, depth + 1));
                else if (operation == "delete") members.RemoveAt(index);
                else if (operation == "insert") throw new SqlException("22023", "cannot replace existing key");
                else members[index] = new(key, replacement!);
                return SqlJsonNode.Object(members);
            }
            var parsed = Index(key) ?? throw new SqlException("22P02", $"path element at position {depth + 1} is not an integer: \"{key}\"");
            long position = parsed; if (position < 0) position += node.Elements.Count;
            var elements = node.Elements.ToList();
            if (position < 0 || position >= elements.Count)
            {
                if (last && (operation == "insert" || operation == "set" && flag)) elements.Insert(position < 0 ? 0 : elements.Count, replacement!);
            }
            else if (!last) elements[(int)position] = Walk(elements[(int)position], depth + 1);
            else if (operation == "delete") elements.RemoveAt((int)position);
            else if (operation == "insert") elements.Insert((int)position + (flag ? 1 : 0), replacement!);
            else elements[(int)position] = replacement!;
            return SqlJsonNode.Array(elements);
        }
        return Walk(root, 0);
    }
    internal static byte[] IdentityBytes(SqlJsonNode node)
    {
        SqlJsonNode Canonical(SqlJsonNode value) => value.Kind switch
        {
            SqlJsonKind.Number => SqlJsonNode.Numeric(value.Number!.TrimScale()),
            SqlJsonKind.Array => SqlJsonNode.Array(value.Elements.Select(Canonical)),
            SqlJsonKind.Object => SqlJsonNode.Object(value.Members.Select(member => new SqlJsonMember(member.Key, Canonical(member.Value)))),
            _ => value
        };
        return TextEncoding.Utf8.GetBytes(Canonical(node).ToString());
    }
}
