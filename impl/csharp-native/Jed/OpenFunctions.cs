using System.Text;

namespace Jed;

internal static class OpenFunctions
{
    internal static bool TryCall(string name, SqlValue[] args, SqlType result, Meter meter, out SqlValue value)
    {
        value = default;
        if (RangeCatalog.Facts.Any(f => f.Name == name))
        {
            if (args.Length == 3 && args[2].IsNull) throw new SqlException(SqlStates.DataException, "range flags cannot be NULL");
            var flags = args.Length == 3 ? args[2].AsText() : "[)";
            if (flags.Length != 2 || flags[0] is not ('[' or '(') || flags[1] is not (']' or ')')) throw Errors.Syntax("invalid range bounds flags");
            value = SqlValue.Range(new(result, args[0], args[1], flags[0] == '[', flags[1] == ']')); return true;
        }
        if (args.Length > 0 && args[0].Type.Kind == SqlKind.Range && name is "lower" or "upper" or "isempty" or "lower_inc" or "upper_inc" or "lower_inf" or "upper_inf")
        {
            if (args[0].IsNull) { value = SqlValue.Null(result); return true; }
            var range = args[0].AsRange();
            value = name switch
            {
                "lower" => range.Lower ?? SqlValue.Null(result), "upper" => range.Upper ?? SqlValue.Null(result),
                "isempty" => SqlValue.Boolean(range.IsEmpty),
                "lower_inc" => SqlValue.Boolean(!range.IsEmpty && range.LowerInclusive), "upper_inc" => SqlValue.Boolean(!range.IsEmpty && range.UpperInclusive),
                "lower_inf" => SqlValue.Boolean(!range.IsEmpty && !range.Lower.HasValue), "upper_inf" => SqlValue.Boolean(!range.IsEmpty && !range.Upper.HasValue),
                _ => throw new InvalidOperationException()
            }; return true;
        }
        if (name == "range_merge") { value = RangeSet("merge", args[0], args[1]); return true; }
        if (name == "width_bucket_array") { value = WidthBucket(args, meter); return true; }
        if (name == "string_to_array")
        {
            if (args[0].IsNull) { value = SqlValue.Null(result); return true; }
            var input = args[0].AsText(); var substitute = args.Length == 3 && !args[2].IsNull ? args[2].AsText() : null;
            var pieces = input.Length == 0 ? [] : args[1].IsNull ? input.EnumerateRunes().Select(r => r.ToString()).ToArray() :
                args[1].AsText().Length == 0 ? [input] : input.Split(args[1].AsText(), StringSplitOptions.None);
            value = SqlValue.Array(new(SqlType.Text, pieces.Select(piece => piece == substitute ? SqlValue.Null(SqlType.Text) : SqlValue.Text(piece)))); return true;
        }
        if (name is "array_append" or "array_prepend" or "array_cat")
        {
            value = name == "array_cat" ? Concatenate(args[0], args[1], result) : Append(args[name == "array_append" ? 0 : 1], args[name == "array_append" ? 1 : 0], name == "array_prepend", result);
            return true;
        }
        if (name is not ("array_ndims" or "cardinality" or "array_length" or "array_lower" or "array_upper" or "array_dims" or "array_remove" or "array_replace" or "array_position" or "array_positions" or "array_to_string")) return false;
        if (args[0].IsNull) { value = SqlValue.Null(result); return true; }
        var source = args[0].AsArray();
        if (name is "array_ndims" or "cardinality" or "array_dims")
        {
            value = name == "cardinality" ? SqlValue.Integer(source.Count, SqlType.I32) : source.Rank == 0 ? SqlValue.Null(result) :
                name == "array_ndims" ? SqlValue.Integer(source.Rank, SqlType.I32) :
                SqlValue.Text(string.Concat(source.Dimensions.Select((size, i) => $"[{source.LowerBounds[i]}:{(long)source.LowerBounds[i] + size - 1}]")));
            return true;
        }
        if (name is "array_length" or "array_lower" or "array_upper")
        {
            if (args[1].IsNull) { value = SqlValue.Null(result); return true; }
            var dimension = args[1].AsInteger();
            if (dimension < 1 || dimension > source.Rank) value = SqlValue.Null(result);
            else { var d = (int)dimension - 1; value = SqlValue.Integer(name == "array_length" ? source.Dimensions[d] : name == "array_lower" ? source.LowerBounds[d] : (long)source.LowerBounds[d] + source.Dimensions[d] - 1, SqlType.I32); }
            return true;
        }
        if (name == "array_to_string")
        {
            if (args[1].IsNull) { value = SqlValue.Null(result); return true; }
            var separator = args[1].AsText(); var substitute = args.Length == 3 && !args[2].IsNull ? args[2].AsText() : null;
            meter.Charge(TemporalRender.ZoneCount(args[0]) * Costs.Timezone); meter.Guard();
            var pieces = source.Values.Where(v => !v.IsNull || substitute is not null).Select(v => v.IsNull ? substitute! : v.Type == SqlType.Boolean ? v.AsBoolean() ? "t" : "f" : TemporalRender.Value(v, meter.Temporal, true)).ToArray();
            var bytes = pieces.Sum(piece => (long)TextEncoding.ByteCount(piece)) + Math.Max(0, pieces.Length - 1) * (long)TextEncoding.ByteCount(separator);
            meter.Charge(OpenValue.PayloadBytes(args[0])); meter.Guard(); meter.ScalarOutput(bytes);
            value = SqlValue.Text(string.Join(separator, pieces)); return true;
        }
        if (name != "array_replace" && source.Rank > 1) throw Errors.Unsupported("multidimensional array search or removal");
        if (name == "array_position" && args.Length == 3 && args[2].IsNull) throw new SqlException(SqlStates.NullValueNotAllowed, "array position start is NULL");
        bool Matches(SqlValue element) => OpenValue.CompareElement(element, args[1]) == 0;
        if (name == "array_remove")
        {
            var elements = source.Values.Where(element => !Matches(element)).ToArray();
            value = SqlValue.Array(elements.Length == 0 ? new(source.ElementType, []) : new(source.ElementType, elements, [elements.Length], source.LowerBounds, true)); return true;
        }
        if (name == "array_replace")
        {
            var bytes = 16L * source.Rank + source.Values.Sum(element => 32L + OpenValue.PayloadBytes(Matches(element) ? args[2] : element));
            meter.Charge(OpenValue.PayloadBytes(args[0])); meter.Guard(); meter.ScalarOutput(bytes);
            value = SqlValue.Array(new(source.ElementType, source.Values.Select(element => Matches(element) ? args[2] : element), source.Dimensions, source.LowerBounds, true)); return true;
        }
        var positions = new List<SqlValue>(); var start = args.Length == 3 ? args[2].AsInteger() : long.MinValue;
        for (var i = 0; i < source.Count; i++)
        {
            var position = (long)source.LowerBounds[0] + i;
            if (position < start || !Matches(source.Values[i])) continue;
            if (name == "array_position") { value = SqlValue.Integer(position, SqlType.I32); return true; }
            positions.Add(SqlValue.Integer(position, SqlType.I32));
        }
        value = name == "array_position" ? SqlValue.Null(SqlType.I32) : SqlValue.Array(new(SqlType.I32, positions)); return true;
    }
    private static SqlValue Append(SqlValue arrayValue, SqlValue element, bool prepend, SqlType result)
    {
        var array = arrayValue.IsNull ? new SqlArray(result.Element!, []) : arrayValue.AsArray();
        if (array.Rank > 1) throw new SqlException(SqlStates.DataException, "array must be empty or one-dimensional");
        var lower = array.Rank == 0 ? 1 : array.LowerBounds[0];
        if (array.Rank != 0 && (prepend ? lower == int.MinValue : (long)lower + array.Count > int.MaxValue)) throw Errors.Overflow();
        return SqlValue.Array(new(result.Element!, prepend ? new[] { element }.Concat(array.Values) : array.Values.Append(element), [array.Count + 1], [lower], prepend));
    }
    private static SqlValue Concatenate(SqlValue left, SqlValue right, SqlType result)
    {
        if (left.IsNull) return right; if (right.IsNull) return left;
        var a = left.AsArray(); var b = right.AsArray(); if (a.Count == 0) return right; if (b.Count == 0) return left;
        var large = a.Rank >= b.Rank ? a : b; var small = a.Rank >= b.Rank ? b : a;
        if (large.Rank - small.Rank > 1) throw Shape();
        var offset = large.Rank - small.Rank;
        for (var i = 1; i < large.Rank; i++)
            if (large.Dimensions[i] != small.Dimensions[i - offset] || large.LowerBounds[i] != small.LowerBounds[i - offset]) throw Shape();
        var dimensions = large.Dimensions.ToArray(); dimensions[0] += offset == 0 ? small.Dimensions[0] : 1;
        return SqlValue.Array(new(result.Element!, a.Values.Concat(b.Values), dimensions, large.LowerBounds));
    }
    private static SqlException Shape() => new(SqlStates.ArraySubscriptError, "incompatible array dimensions");

    internal static SqlValue WidthBucket(SqlValue[] args, Meter meter, CollationDerivation? collation = null)
    {
        var array = args[1].AsArray();
        if (array.Rank > 1) throw new SqlException(SqlStates.ArraySubscriptError, "threshold array must be one-dimensional");
        if (array.Values.Any(value => value.IsNull)) throw new SqlException(SqlStates.NullValueNotAllowed, "threshold array contains NULL");
        var low = 0; var high = array.Count;
        while (low < high)
        {
            var middle = low + (high - low) / 2; meter.Charge(Costs.OperatorEval); meter.Guard();
            if (CollationRules.Compare(args[0], array.Values[middle], collation, meter) < 0) high = middle; else low = middle + 1;
        }
        return SqlValue.Integer(low, SqlType.I32);
    }

    internal static SqlValue RangeSet(string operation, SqlValue left, SqlValue right)
    {
        if (left.IsNull || right.IsNull) return SqlValue.Null(left.Type);
        var a = left.AsRange(); var b = right.AsRange();
        if (a.IsEmpty) return operation is "*" or "-" ? left : right;
        if (b.IsEmpty) return operation == "*" ? right : left;
        var lower = OpenValue.Bound(a.Lower, a.LowerInclusive, b.Lower, b.LowerInclusive, true);
        var upper = OpenValue.Bound(a.Upper, a.UpperInclusive, b.Upper, b.UpperInclusive, false);
        var overlaps = Overlaps(a, b);
        if (operation == "*")
        {
            if (!overlaps) return SqlValue.Range(SqlRange.Empty(left.Type));
            return SqlValue.Range(new(left.Type, lower >= 0 ? a.Lower : b.Lower, upper <= 0 ? a.Upper : b.Upper, lower >= 0 ? a.LowerInclusive : b.LowerInclusive, upper <= 0 ? a.UpperInclusive : b.UpperInclusive));
        }
        if (operation == "-")
        {
            if (!overlaps) return left;
            if (lower >= 0 && upper <= 0) return SqlValue.Range(SqlRange.Empty(left.Type));
            if (lower < 0 && upper > 0) throw new SqlException(SqlStates.DataException, "range difference is not contiguous");
            return lower < 0 ? SqlValue.Range(new(left.Type, a.Lower, b.Lower, a.LowerInclusive, !b.LowerInclusive)) :
                SqlValue.Range(new(left.Type, b.Upper, a.Upper, !b.UpperInclusive, a.UpperInclusive));
        }
        if (operation != "merge" && !overlaps && !Adjacent(a, b)) throw new SqlException(SqlStates.DataException, "range union is not contiguous");
        return SqlValue.Range(new(left.Type, lower <= 0 ? a.Lower : b.Lower, upper >= 0 ? a.Upper : b.Upper, lower <= 0 ? a.LowerInclusive : b.LowerInclusive, upper >= 0 ? a.UpperInclusive : b.UpperInclusive));
    }
    private static bool Before(SqlRange a, SqlRange b) => a.Upper is { } upper && b.Lower is { } lower &&
        (Evaluate.Compare(upper, lower) is var compare && (compare < 0 || compare == 0 && !(a.UpperInclusive && b.LowerInclusive)));
    private static bool Overlaps(SqlRange a, SqlRange b) => !a.IsEmpty && !b.IsEmpty && !Before(a, b) && !Before(b, a);
    private static bool Adjacent(SqlRange a, SqlRange b) => !a.IsEmpty && !b.IsEmpty &&
        (a.Upper is { } au && b.Lower is { } bl && Evaluate.Compare(au, bl) == 0 && a.UpperInclusive != b.LowerInclusive ||
         b.Upper is { } bu && a.Lower is { } al && Evaluate.Compare(bu, al) == 0 && b.UpperInclusive != a.LowerInclusive);
    internal static bool ContainsElement(SqlRange range, SqlValue value) => !range.IsEmpty && !value.IsNull &&
        (!range.Lower.HasValue || Evaluate.Compare(value, range.Lower.Value) is var lo && (lo > 0 || lo == 0 && range.LowerInclusive)) &&
        (!range.Upper.HasValue || Evaluate.Compare(value, range.Upper.Value) is var hi && (hi < 0 || hi == 0 && range.UpperInclusive));
    internal static SqlValue Predicate(string operation, SqlValue left, SqlValue right)
    {
        if (left.IsNull || right.IsNull) return SqlValue.Null(SqlType.Boolean);
        if (operation == "<@") return Predicate("@>", right, left);
        if (left.Type.Kind == SqlKind.Array)
        {
            var a = left.AsArray(); var b = right.AsArray();
            bool Found(SqlValue element) => !element.IsNull && a.Values.Any(candidate => !candidate.IsNull && OpenValue.CompareElement(candidate, element) == 0);
            return SqlValue.Boolean(operation == "&&" ? b.Values.Any(Found) : b.Values.All(Found));
        }
        var first = left.AsRange();
        if (right.Type.Kind != SqlKind.Range) return SqlValue.Boolean(ContainsElement(first, right));
        var second = right.AsRange();
        var result = operation switch
        {
            "@>" => second.IsEmpty || !first.IsEmpty && OpenValue.Bound(first.Lower, first.LowerInclusive, second.Lower, second.LowerInclusive, true) <= 0 && OpenValue.Bound(first.Upper, first.UpperInclusive, second.Upper, second.UpperInclusive, false) >= 0,
            "&&" => Overlaps(first, second), "-|-" => Adjacent(first, second),
            "<<" => !first.IsEmpty && !second.IsEmpty && Before(first, second), ">>" => !first.IsEmpty && !second.IsEmpty && Before(second, first),
            "&<" => !first.IsEmpty && !second.IsEmpty && OpenValue.Bound(first.Upper, first.UpperInclusive, second.Upper, second.UpperInclusive, false) <= 0,
            "&>" => !first.IsEmpty && !second.IsEmpty && OpenValue.Bound(first.Lower, first.LowerInclusive, second.Lower, second.LowerInclusive, true) >= 0,
            _ => throw Errors.Type("invalid range predicate")
        };
        return SqlValue.Boolean(result);
    }
}
