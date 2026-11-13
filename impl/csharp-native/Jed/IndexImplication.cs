namespace Jed;

// The bounded syntactic prover in indexes.md §9.1. It never evaluates SQL or
// reads parameter values; each recursive step consumes an authored expression.
internal static class IndexImplication
{
    internal static bool Proves(Resolved premise, Resolved required)
    {
        var terms = PredicatePlan.Conjuncts(premise).ToArray();
        bool Conjunction(Resolved target) => target.Op == "and"
            ? Conjunction(target.Args[0]) && Conjunction(target.Args[1])
            : terms.Any(term => Clause(term, target)) || target.Op == "or" && (Conjunction(target.Args[0]) || Conjunction(target.Args[1]));
        return Conjunction(required);
    }
    private static bool Clause(Resolved premise, Resolved required)
    {
        if (Resolve.Equivalent(premise, required)) return true;
        if (required.Op == "and") return Clause(premise, required.Args[0]) && Clause(premise, required.Args[1]);
        if (premise.Op == "or") return Clause(premise.Args[0], required) && Clause(premise.Args[1], required);
        if (premise.Op == "and") return Clause(premise.Args[0], required) || Clause(premise.Args[1], required);
        if (Members(premise) is { } members)
            return members.Length != 0 && members.All(member => Atom(member, required));
        if (required.Op == "or" && (Clause(premise, required.Args[0]) || Clause(premise, required.Args[1]))) return true;
        return Atom(premise, required);
    }
    private static Resolved[]? Members(Resolved value)
    {
        IEnumerable<Resolved>? elements = null;
        if (value.Op == "in_list" && value.Text != "not") elements = value.Args.Skip(1);
        if (value.Op == "quantified" && value.Text == "= any")
        {
            if (value.Args[1].Op == "array") elements = value.Args[1].Args;
            else if (value.Args[1] is { Op: "constant", ParameterIndex: 0, ParameterUnbound: false } array)
                elements = array.Constant.IsNull ? [] : array.Constant.AsArray().Values.Select(item => new Resolved("constant", item.Type, item, []));
        }
        if (elements is null) return null;
        var items = elements.Take(101).ToArray();
        if (items.Length > 100 || items.Any(item => item.Op != "constant" || item.ParameterIndex != 0 || item.ParameterUnbound || !item.Constant.IsNull && !ProverConstant(item))) return null;
        return items.Where(item => !item.Constant.IsNull).Select(item => new Resolved("=", SqlType.Boolean, default, [value.Args[0], item], Collation: value.Collation)).ToArray();
    }
    private static bool ProverConstant(Resolved value) => value.Op == "constant" && value.ParameterIndex == 0 && !value.ParameterUnbound && !value.Constant.IsNull &&
        value.Type.Kind is SqlKind.I16 or SqlKind.I32 or SqlKind.I64 or SqlKind.Text or SqlKind.Decimal or SqlKind.Date or SqlKind.Timestamp or SqlKind.Timestamptz or SqlKind.Boolean or SqlKind.Uuid;
    private static (Resolved Operand, Resolved Constant, string Op)? Comparison(Resolved term)
    {
        if (term.Op is not ("=" or "<>" or "!=" or "<" or "<=" or ">" or ">=") || term.Args.Length != 2) return null;
        if (ProverConstant(term.Args[1])) return (term.Args[0], term.Args[1], term.Op);
        if (!ProverConstant(term.Args[0])) return null;
        return (term.Args[1], term.Args[0], term.Op switch { "<" => ">", "<=" => ">=", ">" => "<", ">=" => "<=", _ => term.Op });
    }
    private static bool Atom(Resolved premise, Resolved required)
    {
        if (required.Op == "and") return Atom(premise, required.Args[0]) && Atom(premise, required.Args[1]);
        if (required.Op == "or") return Atom(premise, required.Args[0]) || Atom(premise, required.Args[1]);
        if (Comparison(premise) is not { } first) return false;
        if (required.Op == "is_not_null" && Resolve.Equivalent(first.Operand, required.Args[0])) return true;
        if (Comparison(required) is not { } second || !Resolve.Equivalent(first.Operand, second.Operand)) return false;
        if (first.Constant.Type != second.Constant.Type && !(first.Constant.Type.IsInteger() && second.Constant.Type.IsInteger())) return false;
        if ((first.Op is "<" or "<=" or ">" or ">=" || second.Op is "<" or "<=" or ">" or ">=") &&
            (premise.Collation?.Table is not null || required.Collation?.Table is not null)) return false;
        var order = Evaluate.Compare(first.Constant.Constant, second.Constant.Constant);
        return first.Op switch
        {
            "=" => second.Op switch { "=" => order == 0, "<>" or "!=" => order != 0, "<" => order < 0, "<=" => order <= 0, ">" => order > 0, ">=" => order >= 0, _ => false },
            "<" => second.Op is "<" or "<=" or "<>" or "!=" && order <= 0,
            "<=" => second.Op == "<=" && order <= 0 || second.Op is "<" or "<>" or "!=" && order < 0,
            ">" => second.Op is ">" or ">=" or "<>" or "!=" && order >= 0,
            ">=" => second.Op == ">=" && order >= 0 || second.Op is ">" or "<>" or "!=" && order > 0,
            "<>" or "!=" => second.Op is "<>" or "!=" && order == 0,
            _ => false
        };
    }
}
