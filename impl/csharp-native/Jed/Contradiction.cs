namespace Jed;

internal static class Contradiction
{
    internal static bool Proves(Resolved expression)
    {
        var bounds = new Dictionary<int, List<(string Op, SqlValue Value)>>();
        foreach (var term in PredicatePlan.Conjuncts(expression))
        {
            if (term.Op == "constant" && !term.ParameterUnbound && (term.Constant.IsNull || term.Type == SqlType.Boolean && !term.Constant.AsBoolean())) return true;
            if (term.Op is not ("=" or "<" or "<=" or ">" or ">=") || term.Args.Length != 2) continue;
            var left = term.Args[0]; var right = term.Args[1]; var op = term.Op;
            if (left.ParameterUnbound || right.ParameterUnbound) continue;
            if (left.Op == "constant" && right.Op == "constant")
            {
                if (left.Constant.IsNull || right.Constant.IsNull) return true;
                if (Comparable(left.Constant, right.Constant, op) && !Accepts(op, Evaluate.Compare(left.Constant, right.Constant))) return true;
                continue;
            }
            if (left.Op == "constant")
            {
                (left, right) = (right, left);
                op = op switch { "<" => ">", "<=" => ">=", ">" => "<", ">=" => "<=", _ => op };
            }
            if (left.Op != "column" || right.Op != "constant") continue;
            if (right.Constant.IsNull) return true;
            if (!Comparable(right.Constant, right.Constant, op)) continue;
            if (!bounds.TryGetValue(left.Slot, out var previous)) bounds[left.Slot] = previous = [];
            foreach (var item in previous)
            {
                if (!Comparable(right.Constant, item.Value, op) || !Comparable(right.Constant, item.Value, item.Op)) continue;
                var order = Evaluate.Compare(right.Constant, item.Value);
                if (op == "=" && !Accepts(item.Op, order) || item.Op == "=" && !Accepts(op, -order)) return true;
                var lower = op is ">" or ">="; var upper = op is "<" or "<=";
                if (lower && item.Op is "<" or "<=" && (order > 0 || order == 0 && (op == ">" || item.Op == "<"))) return true;
                if (upper && item.Op is ">" or ">=" && (order < 0 || order == 0 && (op == "<" || item.Op == ">"))) return true;
            }
            previous.Add((op, right.Constant));
        }
        return false;
    }
    private static bool Comparable(SqlValue left, SqlValue right, string op) => left.Type == right.Type && left.Type.Kind switch
    {
        SqlKind.Text => op == "=",
        SqlKind.Boolean or SqlKind.I16 or SqlKind.I32 or SqlKind.I64 or SqlKind.F32 or SqlKind.F64 or SqlKind.Decimal or SqlKind.Date or SqlKind.Timestamp or SqlKind.Timestamptz or SqlKind.Interval or SqlKind.Uuid or SqlKind.Bytea => true,
        _ => false
    };
    private static bool Accepts(string op, int order) => op switch
    { "=" => order == 0, "<" => order < 0, "<=" => order <= 0, ">" => order > 0, ">=" => order >= 0, _ => false };
}
