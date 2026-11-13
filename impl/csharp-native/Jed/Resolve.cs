using System.Globalization;

namespace Jed;

internal sealed record Resolved(string Op, SqlType Type, SqlValue Constant, Resolved[] Args);
internal sealed record Projection(Resolved Expression, string Name);
internal sealed record SelectPlan(Projection[] Projections, Resolved? Filter, long? Limit, long Offset, bool Empty);

internal static class Resolve
{
    private static Resolved Constant(SqlValue value) => new("constant", value.Type, value, []);
    private static Resolved Node(string op, SqlType type, params Resolved[] args) => new(op, type, default, args);
    private static bool Adaptable(Expression expression) => expression.Op is "integer" or "null";

    internal static SelectPlan Select(SelectSyntax syntax)
    {
        var projections = syntax.Items.Select(item => new Projection(Expr(item.Expression), item.Alias ?? "?column?")).ToArray();
        var filter = syntax.Filter is null ? null : Expr(syntax.Filter);
        if (filter is not null && filter.Type is not (SqlType.Boolean or SqlType.Unknown))
            throw Errors.Type("WHERE requires boolean");
        return new(projections, filter, syntax.Limit, syntax.Offset, filter is not null && EmptyFilter(filter));
    }

    // planner.md §3: a literal FALSE/NULL conjunct proves the relation empty.
    // Resolve every operand before this rewrite so it cannot hide type errors.
    private static bool EmptyFilter(Resolved expression) =>
        expression.Op == "constant" && (expression.Constant.IsNull || expression.Type == SqlType.Boolean && !expression.Constant.AsBoolean()) ||
        expression.Op == "and" && expression.Args.Any(EmptyFilter);

    internal static Resolved Expr(Expression expression, SqlType hint = SqlType.Unknown)
    {
        switch (expression.Op)
        {
            case "integer":
                if (!long.TryParse(expression.Text, NumberStyles.AllowLeadingSign, CultureInfo.InvariantCulture, out var integer))
                    throw Errors.Overflow();
                return Constant(SqlValue.Integer(integer, hint.IsInteger() ? hint : SqlType.I64));
            case "boolean": return Constant(SqlValue.Boolean(expression.Text == "true"));
            case "text": return Constant(SqlValue.Text(expression.Text));
            case "null": return Constant(SqlValue.Null());
            case "name": throw new SqlException(SqlStates.UndefinedColumn, $"column does not exist: {expression.Text}");
            case "cast":
            {
                var target = SqlTypes.Parse(expression.Text);
                var source = expression.Args[0];
                if (source.Op == "text") return Constant(ParseText(source.Text, target));
                var inner = Expr(source, target == SqlType.Boolean ? SqlType.I32 : SqlType.Unknown);
                if (!CanCast(inner.Type, target)) throw Errors.Type($"cannot cast {inner.Type.Name()} to {target.Name()}");
                // Text identity is folded; numeric casts remain runtime nodes even
                // for constants, preserving overflow timing and exact costs.
                if (target == SqlType.Text && inner.Type == SqlType.Text) return inner;
                return Node("cast", target, inner);
            }
            case "neg": case "pos":
            {
                var inner = Expr(expression.Args[0], hint);
                if (!inner.Type.IsInteger() && inner.Type != SqlType.Unknown) throw Errors.Type("unary arithmetic requires numeric input");
                return expression.Op == "pos" ? inner : Node("neg", inner.Type, inner);
            }
            case "not":
            {
                var inner = Expr(expression.Args[0]);
                BooleanOperand(inner);
                return Node("not", SqlType.Boolean, inner);
            }
            case "is_null": case "is_not_null":
                return Node(expression.Op, SqlType.Boolean, Expr(expression.Args[0]));
            case "and": case "or":
            {
                var left = Expr(expression.Args[0]);
                var right = Expr(expression.Args[1]);
                BooleanOperand(left); BooleanOperand(right);
                return Node(expression.Op, SqlType.Boolean, left, right);
            }
            default:
            {
                var (left, right) = Pair(expression.Args[0], expression.Args[1]);
                var arithmetic = expression.Op is "+" or "-" or "*" or "/" or "%";
                var common = Common(left.Type, right.Type);
                if (arithmetic && !common.IsInteger() && common != SqlType.Unknown)
                    throw Errors.Type("arithmetic requires numeric operands");
                return Node(expression.Op, arithmetic ? common : SqlType.Boolean, left, right);
            }
        }
    }

    private static (Resolved, Resolved) Pair(Expression left, Expression right)
    {
        // Resolve a concrete sibling once, then adapt only literal operands.
        if (Adaptable(left) && !Adaptable(right))
        {
            var resolvedRight = Expr(right);
            return (Expr(left, resolvedRight.Type), resolvedRight);
        }
        var resolvedLeft = Expr(left);
        return (resolvedLeft, Expr(right, resolvedLeft.Type));
    }
    private static SqlType Common(SqlType left, SqlType right)
    {
        if (left == SqlType.Unknown) return right;
        if (right == SqlType.Unknown) return left;
        if (left == right) return left;
        if (left.IsInteger() && right.IsInteger()) return SqlTypes.Fact(left).Rank >= SqlTypes.Fact(right).Rank ? left : right;
        throw Errors.Type($"incompatible operands: {left.Name()} and {right.Name()}");
    }
    private static void BooleanOperand(Resolved value)
    {
        if (value.Type is not (SqlType.Boolean or SqlType.Unknown)) throw Errors.Type("logical operation requires boolean operands");
    }
    private static bool CanCast(SqlType from, SqlType to) => from == SqlType.Unknown || from == to || CastCatalog.Pairs.Contains((from, to));

    internal static SqlValue ParseText(string text, SqlType target)
    {
        if (target == SqlType.Text) return SqlValue.Text(text);
        var input = text.Trim(' ', '\t', '\r', '\n', '\f', '\v');
        if (target.IsInteger())
        {
            var digits = input.AsSpan();
            if (!digits.IsEmpty && digits[0] is '+' or '-') digits = digits[1..];
            if (digits.IsEmpty || digits.ContainsAnyExceptInRange('0', '9'))
                throw new SqlException(SqlStates.InvalidTextRepresentation, "invalid integer input");
            if (!long.TryParse(input, NumberStyles.AllowLeadingSign, CultureInfo.InvariantCulture, out var value)) throw Errors.Overflow();
            return SqlValue.Integer(value, target);
        }
        if (target == SqlType.Boolean)
        {
            var lower = TextEncoding.FoldName(input);
            if (lower == "1") return SqlValue.Boolean(true);
            if (lower == "0") return SqlValue.Boolean(false);
            if (lower.Length > 0)
            {
                var yes = "true".StartsWith(lower, StringComparison.Ordinal) || "yes".StartsWith(lower, StringComparison.Ordinal) || "on".StartsWith(lower, StringComparison.Ordinal);
                var no = "false".StartsWith(lower, StringComparison.Ordinal) || "no".StartsWith(lower, StringComparison.Ordinal) || "off".StartsWith(lower, StringComparison.Ordinal);
                if (yes != no) return SqlValue.Boolean(yes);
            }
            throw new SqlException(SqlStates.InvalidTextRepresentation, "invalid boolean input");
        }
        throw Errors.Unsupported($"text input for {target.Name()}");
    }

    internal static SqlValue Cast(SqlValue value, SqlType target)
    {
        if (value.IsNull) return SqlValue.Null(target);
        if (value.Type == target) return value;
        if (target == SqlType.Text) return SqlValue.Text(value.ToString());
        if (value.Type == SqlType.Text) return ParseText(value.AsText(), target);
        if (target == SqlType.Boolean) return SqlValue.Boolean(value.AsInteger() != 0);
        if (value.Type == SqlType.Boolean) return SqlValue.Integer(value.AsBoolean() ? 1 : 0, target);
        return SqlValue.Integer(value.AsInteger(), target);
    }
}
