namespace Jed;

internal sealed record GinQueryNode(bool All, byte[][] Terms, GinQueryNode[] Children, bool Unbound = false, bool Fallback = false)
{
    internal static GinQueryNode Empty { get; } = new(false, [], []);
    internal static GinQueryNode Make(bool all, IEnumerable<byte[]> terms, IEnumerable<GinQueryNode>? children = null)
    {
        var leaves = new SortedSet<byte[]>(terms, StatisticsBytes.Instance); var nested = new List<GinQueryNode>();
        foreach (var child in children ?? [])
            if (child.All == all) { leaves.UnionWith(child.Terms); nested.AddRange(child.Children); } else nested.Add(child);
        if (leaves.Count == 0 && nested.Count == 1) return nested[0];
        return new(all, leaves.ToArray(), nested.ToArray());
    }
    internal GinQueryNode Tagged(CatalogIndex index, int slot) => this with
    { Terms = Terms.Select(term => GinIndex.Tag(index, slot, term)).ToArray(), Children = Children.Select(child => child.Tagged(index, slot)).ToArray() };
}

internal static class GinQuery
{
    internal static GinQueryNode[]? Match(Resolved filter, int column, SqlType type, Meter? meter = null)
    {
        var conjuncts = PredicatePlan.Conjuncts(filter).ToArray();
        foreach (var term in conjuncts) if (Single(term, column, type, meter, out var matched)) return matched is null ? null : [matched];
        foreach (var term in conjuncts)
        {
            var arms = Disjunction(term); if (arms is null) continue;
            var matches = new List<GinQueryNode>(); var valid = true;
            foreach (var arm in arms)
            {
                var found = false;
                foreach (var part in PredicatePlan.Conjuncts(arm))
                    if (Single(part, column, type, meter, out var match))
                    { found = true; if (match is null) valid = false; else matches.Add(match); break; }
                if (!found) valid = false;
                if (!valid) break;
            }
            if (valid) return matches.ToArray();
        }
        return null;
    }
    private static IEnumerable<Resolved>? Disjunction(Resolved expression)
    {
        if (expression.Op == "in_list") return expression.Args.Skip(1).Select(value => new Resolved("=", SqlType.Boolean, default, [expression.Args[0], value]));
        if (expression.Op != "or") return null;
        IEnumerable<Resolved> Flatten(Resolved node)
        {
            if (node.Op == "or") foreach (var child in node.Args) foreach (var arm in Flatten(child)) yield return arm;
            else yield return node;
        }
        return Flatten(expression);
    }
    private static bool Single(Resolved expression, int column, SqlType type, Meter? meter, out GinQueryNode? result)
    {
        result = null;
        if (expression.Args.Length != 2) return false;
        var operation = expression.Op is "json_binary" or "json_path_binary" or "open_predicate" ? expression.Text : expression.Op;
        var operand = expression.Args[0]; var query = expression.Args[1];
        static Resolved Column(Resolved value) => value.Op == "promote" && value.Type.Kind == SqlKind.Array && value.Type.Element!.IsInteger() ? Column(value.Args[0]) : value;
        if (expression.Op == "quantified" && expression.Text == "= any") { operation = "member"; (operand, query) = (query, operand); }
        else if (operation == "=" && Column(query).Op == "column" && Column(query).Slot == column || type == SqlType.Jsonb && operation == "<@")
        { (operand, query) = (query, operand); if (operation == "<@") operation = "@>"; }
        operand = Column(operand);
        if (operand.Op != "column" || operand.Slot != column) return false;
        if (query.ParameterUnbound)
        {
            if (type.Kind == SqlKind.Array && operation is "@>" or "&&" or "=" or "member" || type == SqlType.Jsonb && operation is "@>" or "?" or "?|" or "?&" or "@?" or "@@")
            { result = new(operation is not ("&&" or "?|"), [], [], Unbound: true); return true; }
            return false;
        }
        if (!Value(query, out var value, meter)) return false;
        if (type.Kind == SqlKind.Array)
        {
            if (operation is not ("@>" or "&&" or "=" or "member")) return false;
            if (value.IsNull) { result = GinQueryNode.Empty; return true; }
            var values = operation == "member" ? [value] : value.AsArray().Values.ToArray();
            if (operation == "@>" && values.Any(item => item.IsNull)) { result = GinQueryNode.Empty; return true; }
            var converted = new List<SqlValue>();
            foreach (var item in values)
            {
                if (item.IsNull) continue;
                SqlValue cast;
                try { cast = Resolve.Cast(item, type.Element!); if (Evaluate.Compare(cast, item) != 0) throw Errors.Type("GIN term loses value"); }
                catch (SqlException)
                { if (operation == "&&") continue; result = GinQueryNode.Empty; return true; }
                converted.Add(cast);
            }
            var terms = StatisticsTerms.Extract(SqlValue.Array(new(type.Element!, converted))).ToArray();
            if (terms.Length == 0 && operation is "@>" or "=") { result = new(false, [], [], Fallback: true); return true; }
            result = GinQueryNode.Make(operation != "&&", terms); return true;
        }
        if (type != SqlType.Jsonb || operation is not ("@>" or "?" or "?|" or "?&" or "@?" or "@@")) return false;
        if (value.IsNull) { result = GinQueryNode.Empty; return true; }
        if (operation is "@?" or "@@") { result = GinJsonPath.Extract(value.AsJsonpath(), operation == "@@") ?? new(false, [], [], Fallback: true); return true; }
        var extracted = operation == "@>" ? StatisticsTerms.Extract(value) : operation == "?"
            ? StatisticsTerms.Extract(SqlValue.Jsonb(SqlJsonNode.String(value.AsText())))
            : value.AsArray().Values.Where(item => !item.IsNull).SelectMany(item => StatisticsTerms.Extract(SqlValue.Jsonb(SqlJsonNode.String(item.AsText()))));
        var list = extracted.ToArray();
        if (list.Length == 0 && operation is "@>" or "?&") { result = new(false, [], [], Fallback: true); return true; }
        result = GinQueryNode.Make(operation != "?|", list); return true;
    }
    internal static bool Value(Resolved expression, out SqlValue value, Meter? meter = null)
    {
        value = default;
        if (expression.ParameterUnbound) return false;
        if (expression.Op == "constant") { value = expression.Constant; return true; }
        if (expression.Op is "cast" or "promote" && Value(expression.Args[0], out var original, meter))
        {
            try { value = Resolve.ApplyModifier(Resolve.Cast(original, expression.Type), expression.Target ?? new(expression.Type)); return true; }
            catch (SqlException) { return false; }
        }
        if (expression.Op != "array")
        {
            if (meter is null || !Evaluate.Constant(expression)) return false;
            try { using var unmetered = meter.SuppressCost(); value = Evaluate.Expr(expression, meter); return true; }
            catch (SqlException) { return false; }
        }
        var values = new SqlValue[expression.Args.Length];
        for (var i = 0; i < values.Length; i++) if (!Value(expression.Args[i], out values[i], meter)) return false;
        if (expression.Text != "nested") { value = SqlValue.Array(new(expression.Type.Element!, values)); return true; }
        if (values.Any(item => item.IsNull)) return false;
        var arrays = values.Select(item => item.AsArray()).ToArray();
        if (arrays.Length == 0 || arrays[0].Count == 0) { value = SqlValue.Array(new(expression.Type.Element!, [])); return true; }
        if (arrays.Any(array => !array.Dimensions.SequenceEqual(arrays[0].Dimensions) || !array.LowerBounds.SequenceEqual(arrays[0].LowerBounds))) return false;
        value = SqlValue.Array(new(expression.Type.Element!, arrays.SelectMany(array => array.Values), new[] { arrays.Length }.Concat(arrays[0].Dimensions), new[] { 1 }.Concat(arrays[0].LowerBounds))); return true;
    }
}
