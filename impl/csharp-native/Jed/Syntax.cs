namespace Jed;

internal sealed record Expression(string Op, string Text, Expression[] Args, int Height = 0, Resolved? Binding = null, Expression? Predicate = null, TypeSpec? Type = null, SelectSyntax? Query = null, TemporalContext? Context = null, ParameterSlot? Parameter = null, OrderSyntax[]? AggregateOrder = null, WindowSyntax? Window = null, JsonSqlSpec? Json = null);
internal sealed record SelectItem(Expression Expression, string? Alias);
internal sealed record OrderSyntax(Expression Expression, bool Descending, bool? NullsFirst)
{
    internal int? OutputIndex { get; init; }
}
internal sealed record SelectSyntax(SelectItem[] Items, Expression? Filter, long? Limit, long Offset,
    string? Table = null, string? TableAlias = null, OrderSyntax[]? Order = null, bool Distinct = false, Expression[]? GroupBy = null, Expression? Having = null, RelationSyntax? Relation = null, bool ValuesQuery = false, bool SetQuery = false, CteSyntax[]? With = null, bool Recursive = false, Dictionary<string, WindowSyntax>? Windows = null, int[][]? GroupingSets = null, bool GroupingExpansion = false) : StatementSyntax
{
    internal Expression[]? DistinctOn { get; init; }
    internal Expression? LimitExpression { get; init; }
    internal Expression? OffsetExpression { get; init; }
    internal int ParseDepth { get; init; } = 1;
    internal bool HasLimit { get; init; }
    internal bool HasOffset { get; init; }
}
internal abstract record StatementSyntax;

internal sealed partial class Parser
{
    private const int MaxDepth = 256; // spec/design/cost.md §7, fixed across cores.
    private readonly Lexer lexer;
    private Token token;
    private readonly List<Token> lookahead = [];
    private List<Token>? capturedTokens = null;
    private int nesting;
    private readonly List<int> queryDepths = [];
    internal Parser(string sql) { lexer = new(sql); token = lexer.Next(); }
    private bool Is(string text) => token.Kind is TokenKind.Word or TokenKind.Symbol && token.Text == text;
    private Token Peek(int distance = 1)
    {
        while (lookahead.Count < distance) lookahead.Add(lexer.Next());
        return lookahead[distance - 1];
    }
    private Token Take()
    {
        var result = token; capturedTokens?.Add(result);
        if (lookahead.Count == 0) token = lexer.Next();
        else { token = lookahead[0]; lookahead.RemoveAt(0); }
        return result;
    }
    private bool Eat(string text) { if (!Is(text)) return false; Take(); return true; }
    private void Expect(string text) { if (!Eat(text)) throw Errors.Syntax($"expected {text}"); }
    private void Depth(int depth)
    {
        for (var i = 0; i < queryDepths.Count; i++) queryDepths[i] = Math.Max(queryDepths[i], depth);
        if (depth > MaxDepth) throw new SqlException(SqlStates.StatementTooComplex, "expression nesting exceeds 256");
    }
    private Expression Node(string op, string text, params Expression[] args)
    {
        var height = args.Length == 0 ? 1 : args.Max(a => a.Height) + 1;
        Depth(nesting + height);
        return new(op, text, args, height);
    }
    private string Name()
    {
        if (token.Kind is not (TokenKind.Word or TokenKind.QuotedName)) throw Errors.Syntax("expected identifier");
        return Take().Text;
    }

    internal SelectSyntax Parse() => ParseStatement() as SelectSyntax ?? throw Errors.Unsupported("expected SELECT");
    internal StatementSyntax ParseStatement()
    {
        if (token.Kind == TokenKind.End) throw Errors.Syntax("empty SQL statement");
        StatementSyntax statement = Is("with") || Is("select") || Is("values") || Is("(") ? QueryStatement() : OtherStatement();
        Eat(";");
        if (token.Kind != TokenKind.End) throw Errors.Syntax($"unexpected token: {token.Text}");
        return statement;
    }
    private SelectSyntax Select()
    {
        queryDepths.Add(nesting + 1);
        try
        {
            var query = Eat("with") ? WithQuery() : QueryTail(SetQueryBody(0));
            return query with { ParseDepth = queryDepths[^1] };
        }
        finally { queryDepths.RemoveAt(queryDepths.Count - 1); }
    }
    private SelectSyntax SetQueryBody(int minimum)
    {
        SelectSyntax left;
        if (Eat("(")) { nesting++; Depth(nesting); left = Select(); Expect(")"); nesting--; }
        else left = SelectCore();
        return SetQueryTail(left, minimum);
    }
    private SelectSyntax SetQueryTail(SelectSyntax left, int minimum)
    {
        while (true)
        {
            var precedence = Is("intersect") ? 2 : Is("union") || Is("except") ? 1 : -1;
            if (precedence < minimum) return left;
            var operation = Take().Text; var all = Eat("all"); if (!all) Eat("distinct");
            var right = SetQueryBody(precedence + 1);
            left = new([new(Node("star", ""), null)], null, null, 0, Relation: new SetRelation(left, right, operation, all), SetQuery: true);
        }
    }
    private SelectSyntax SelectCore()
    {
        var ownsInto = intoAllowed; intoAllowed = false;
        if (Is("values")) return new([new(Node("star", ""), null)], null, null, 0, Relation: ValuesBody(), ValuesQuery: true);
        Expect("select");
        var distinct = Is("distinct") && Peek().Text != "from" && Eat("distinct");
        if (!distinct) Eat("all");
        Expression[]? distinctOn = null;
        if (distinct && Is("on") && Peek().Text == "(")
        {
            Take(); Expect("("); var keys = new List<Expression>();
            do { keys.Add(Expr()); } while (Eat(","));
            Expect(")"); distinctOn = keys.ToArray();
        }
        var items = new List<SelectItem>();
        do
        {
            var expression = Eat("*") ? Node("star", "") : Expr();
            var alias = Eat("as") ? Name() : token.Kind == TokenKind.QuotedName || token.Kind == TokenKind.Word && !Clause(token.Text) ? Name() : null;
            items.Add(new(expression, alias));
        } while (Eat(","));
        IntoClause(ownsInto);
        string? table = null, tableAlias = null;
        RelationSyntax? relation = null;
        if (Eat("from"))
        {
            relation = FromRelation();
            if (relation is TableRelation direct && direct.ColumnAliases is null) { table = direct.Name; tableAlias = direct.Alias; }
        }
        var filter = Eat("where") ? Expr() : null;
        Expression[] group = []; int[][]? groupingSets = null; var groupingExpansion = false;
        if (Eat("group")) { Expect("by"); (group, groupingSets, groupingExpansion) = GroupingClause(); }
        var having = Eat("having") ? Expr() : null;
        Dictionary<string, WindowSyntax>? windows = null;
        if (Eat("window"))
        {
            windows = new(StringComparer.Ordinal);
            do
            {
                var name = Name(); Expect("as"); Expect("("); var definition = WindowDefinition(); Expect(")");
                if (!windows.TryAdd(name, definition)) throw new SqlException(SqlStates.WindowingError, "duplicate window name");
            } while (Eat(","));
        }
        return new(items.ToArray(), filter, null, 0, table, tableAlias, Distinct: distinct, GroupBy: group, Having: having, Relation: relation, Windows: windows, GroupingSets: groupingSets, GroupingExpansion: groupingExpansion) { DistinctOn = distinctOn };

    }
    private SelectSyntax QueryTail(SelectSyntax query)
    {
        var order = new List<OrderSyntax>();
        if (Eat("order"))
        {
            if (query.Order is { Length: > 0 }) throw Errors.Syntax("multiple ORDER BY clauses");
            Expect("by");
            do
            {
                var expression = Expr();
                var descending = Eat("desc"); if (!descending) Eat("asc");
                bool? nulls = null;
                if (Eat("nulls")) { if (Eat("first")) nulls = true; else { Expect("last"); nulls = false; } }
                order.Add(new(expression, descending, nulls));
            } while (Eat(","));
        }
        long? limit = null; long offset = 0;
        var limitExpression = query.LimitExpression; var offsetExpression = query.OffsetExpression;
        var hasLimit = query.HasLimit; var hasOffset = query.HasOffset;
        while (Is("limit") || Is("offset"))
        {
            if (Eat("limit")) { if (hasLimit || query.Limit is not null) throw Errors.Syntax("multiple LIMIT clauses"); hasLimit = true; if (!Eat("all")) { var count = QueryCount(true); limit = count.Value; limitExpression = count.Parameter; } }
            else { Expect("offset"); if (hasOffset) throw Errors.Syntax("multiple OFFSET clauses"); hasOffset = true; var count = QueryCount(false); offset = count.Value ?? 0; offsetExpression = count.Parameter; }
        }
        return query with { Order = order.Count == 0 ? query.Order : order.ToArray(), Limit = limit ?? query.Limit, Offset = offset == 0 ? query.Offset : offset, HasLimit = hasLimit, HasOffset = hasOffset, LimitExpression = limitExpression, OffsetExpression = offsetExpression };
    }
    private string QualifiedName()
    {
        var name = Name();
        if (Eat(".")) name = DatabaseDomains.Qualified(name, Name());
        return name;
    }
    private static bool Clause(string value) => value is "into" or "from" or "where" or "group" or "having" or "window" or "order" or "limit" or "offset" or "union" or "intersect" or "except" or "fetch" or "for" or "returning" or "on" or "join" or "cross" or "inner" or "left" or "right" or "full" or "natural" or "using";
    private (long? Value, Expression? Parameter) QueryCount(bool limit)
    {
        if (token.Kind == TokenKind.Parameter) return (null, Node("parameter", Take().Text));
        var negative = Eat("-"); if (!negative) Eat("+");
        if (token.Kind != TokenKind.Integer) throw Errors.Syntax("expected integer or parameter count");
        var text = Take().Text;
        if (negative && text.Any(character => character != '0')) throw new SqlException(limit ? SqlStates.InvalidRowCountInLimitClause : SqlStates.InvalidRowCountInOffsetClause,
            limit ? "LIMIT must not be negative" : "OFFSET must not be negative");
        if (!long.TryParse(text, System.Globalization.NumberStyles.None, System.Globalization.CultureInfo.InvariantCulture, out var value)) throw Errors.Overflow();
        return (value, null);
    }

    private Expression Expr(int minimum = 0)
    {
        Depth(nesting);
        Expression left; var fieldBase = false;
        if (Eat("("))
        {
            nesting++; Depth(nesting);
            if (Is("select") || Is("values") || Is("with")) left = Node("subquery", "") with { Query = Select() };
            else left = Expr();
            if (left.Op == "subquery" && (Is("union") || Is("except") || Is("intersect")))
                left = left with { Query = QueryTail(SetQueryTail(left.Query!, 0)) };
            fieldBase = true;
            if (Eat(","))
            {
                fieldBase = false;
                var fields = new List<Expression> { left }; do { fields.Add(Expr()); } while (Eat(","));
                left = Node("row", "", fields.ToArray());
            }
            Expect(")"); nesting--;
        }
        else if (token.Kind is TokenKind.Word or TokenKind.QuotedName && Peek().Text == ".")
        {
            var qualifier = Name(); Expect(".");
            left = Eat("*") ? Node("star", qualifier) : Node("name", qualifier + "." + Name());
        }
        else if (Is("-") || Is("+") || Is("not"))
        {
            var op = Take().Text;
            nesting++; Depth(nesting);
            if (op is "-" or "+" && token.Kind is TokenKind.Integer or TokenKind.Decimal && Peek().Text != "::")
            {
                var kind = token.Kind;
                var text = (op == "-" ? "-" : "") + Take().Text;
                nesting--;
                left = Node(kind == TokenKind.Integer ? "integer" : "decimal", text);
            }
            else
            {
                var operand = Expr(op == "not" ? 30 : 70);
                nesting--;
                left = Node(op == "-" ? "neg" : op == "+" ? "pos" : "not", "", operand);
            }
        }
        else if (Eat("exists"))
        {
            Expect("("); nesting++; Depth(nesting); left = Node("exists", "") with { Query = Select() }; Expect(")"); nesting--;
        }
        else if (Eat("array")) left = ArrayExpression();
        else if (Is("[")) left = ArrayExpression();
        else if (Eat("cast"))
        {
            Expect("("); nesting++; Depth(nesting);
            var inner = Expr(); Expect("as");
            var target = TypeName(); Expect(")"); nesting--;
            left = Node("cast", target, inner);
        }
        else if (token.Kind == TokenKind.Integer) left = Node("integer", Take().Text);
        else if (token.Kind == TokenKind.Decimal) left = Node("decimal", Take().Text);
        else if (token.Kind == TokenKind.String) left = Node("text", Take().Text);
        else if (Eat("null")) left = Node("null", "");
        else if (Is("true") || Is("false")) left = Node("boolean", Take().Text);
        else if (Eat("case")) left = Case();
        else if (Eat("extract"))
        {
            Expect("("); var field = token.Kind == TokenKind.String ? Take().Text : Name(); Expect("from");
            var source = Expr(); Expect(")"); left = Node("extract", field, source);
        }
        else if (Eat("current_date")) { if (Eat("(")) Expect(")"); left = Node("call", "current_date"); }
        else if (Eat("current_timestamp")) left = Node("call", "now");
        else if (token.Kind == TokenKind.Parameter) left = Node("parameter", Take().Text);
        else if (token.Kind is TokenKind.Word or TokenKind.QuotedName)
        {
            var quoted = token.Kind == TokenKind.QuotedName;
            var name = Name();
            if (!quoted && name == "double" && Eat("precision")) name += " precision";
            if (!quoted && name == "character" && Eat("varying")) name += " varying";
            if (!quoted && name == "timestamp" && (Is("with") || Is("without")))
            {
                var withZone = Eat("with"); if (!withZone) Expect("without"); Expect("time"); Expect("zone");
                if (withZone) name = "timestamptz";
            }
            if (token.Kind == TokenKind.String)
            {
                var literal = Node("text", Take().Text);
                if (name == "interval") name = IntervalQualifier(name);
                left = Node("cast", name, literal);
            }
            else if (Is("(") && name is "json" or "json_value" or "json_query" or "json_exists" or "json_serialize")
            {
                Take(); left = JsonSpecial(name);
            }
            else if (name == "substring" && Eat("(")) left = SubstringSyntax();
            else if (!quoted && name == "overlay" && Eat("(")) left = OverlaySyntax();
            else if (!quoted && name == "position" && Eat("(")) left = PositionSyntax();
            else if (Eat("("))
            {
                nesting++; Depth(nesting);
                var args = new List<Expression>();
                var distinctArgument = Eat("distinct"); OrderSyntax[]? aggregateOrder = null;
                if (!Eat(")"))
                {
                    do
                    {
                        if (token.Kind is TokenKind.Word or TokenKind.QuotedName && Peek().Text == "=>")
                        {
                            var label = Name(); Expect("=>"); args.Add(Node("named", label, Expr()));
                        }
                        else args.Add(Eat("variadic") ? Node("variadic", "", Expr()) : Eat("*") ? Node("star", "") : Expr());
                        if (args[^1].Op == "variadic" && Is(",")) throw Errors.Syntax("VARIADIC argument must be last");
                    } while (Eat(","));
                    if (Eat("order")) { Expect("by"); aggregateOrder = OrderItems(); }
                    Expect(")");
                }
                nesting--;
                if (name is "coalesce" or "greatest" or "least" && (args.Count == 0 || distinctArgument || aggregateOrder is not null || args.Any(argument => argument.Op is "named" or "variadic") || Is("over") || Is("filter") || Is("within"))) throw Errors.Syntax("invalid conditional expression syntax");
                if (distinctArgument && (args.Count == 0 || args.Any(a => a.Op == "star"))) throw Errors.Syntax("DISTINCT requires an expression");
                left = Node(name == "row" ? "row" : distinctArgument ? "call_distinct" : "call", name, args.ToArray()) with { AggregateOrder = aggregateOrder };
                if (Eat("within")) { if (distinctArgument || aggregateOrder is not null) throw Errors.Syntax("invalid ordered-set aggregate decoration"); Expect("group"); Expect("("); Expect("order"); Expect("by"); left = left with { Op = "ordered_aggregate", AggregateOrder = OrderItems() }; Expect(")"); }
                if (Eat("filter")) { Expect("("); Expect("where"); var predicate = Expr(); Expect(")"); left = left with { Predicate = predicate }; }
                if (Eat("over")) left = left with { Window = Over() };
            }
            else
            {
                if (Eat(".")) { if (Eat("*")) { left = Node("star", name); return left; } name += "." + Name(); }
                left = Node("name", name);
            }
        }
        else throw Errors.Syntax($"expected expression, found {token.Text}");

        var seenIs = false; var seenComparison = false;
        while (true)
        {
            if (Is("collate") && minimum <= 80)
            {
                Take(); if (token.Kind != TokenKind.QuotedName) throw Errors.Syntax("collation name must be quoted"); left = Node("collate", Name(), left); continue;
            }
            if (Is("[") && minimum <= 80)
            {
                var dimensions = new List<Expression> { left };
                do
                {
                    Expect("["); nesting++; Depth(nesting);
                    var lower = Is(":") ? Node("omitted", "") : Expr();
                    if (Eat(":")) dimensions.Add(Node("slice", "", lower, Is("]") ? Node("omitted", "") : Expr()));
                    else dimensions.Add(Node("index", "", lower));
                    Expect("]"); nesting--;
                } while (Is("["));
                left = Node("subscript", "", dimensions.ToArray()); continue;
            }
            if (Is(".") && minimum <= 80)
            {
                if (!fieldBase) throw Errors.Syntax("field access requires a parenthesized base");
                Take(); left = Eat("*") ? Node("field_star", "", left) : Node("field", Name(), left); continue;
            }
            if (Is("::") && minimum <= 80)
            {
                Take(); left = Node("cast", TypeName(), left); continue;
            }
            if (Is("at") && minimum <= 70)
            {
                Take(); Expect("time"); Expect("zone"); left = Node("at_time_zone", "", left, Expr(71)); continue;
            }
            if (Is("is") && minimum <= 40)
            {
                if (seenIs) throw Errors.Syntax("IS predicates do not associate"); seenIs = true;
                Take(); var negate = Eat("not");
                if (Eat("null")) left = Node(negate ? "is_not_null" : "is_null", "", left);
                else if (Eat("json")) left = JsonPredicate(left, negate);
                else if (Is("true") || Is("false") || Is("unknown"))
                    left = Node("is_boolean", (negate ? "not_" : "") + Take().Text, left);
                else if (Eat("distinct"))
                {
                    Expect("from"); nesting++; Depth(nesting);
                    var right = Expr(41); nesting--;
                    left = Node(negate ? "not_distinct" : "distinct", "", left, right);
                }
                else throw Errors.Unsupported("IS predicates other than NULL/DISTINCT FROM");
                continue;
            }
            if (minimum <= 35 && (Is("between") || Is("in") || Is("like") || Is("ilike") || Is("similar") || Is("not") && Peek().Text is "between" or "in" or "like" or "ilike" or "similar"))
            {
                var negate = Eat("not");
                if (Eat("between"))
                {
                    var lower = Expr(41); Expect("and"); var upper = Expr(41);
                    left = Node("and", "", Node(">=", "", left, lower), Node("<=", "", left, upper));
                }
                else if (Eat("in"))
                {
                    Expect("("); nesting++; Depth(nesting);
                    if (Is("select") || Is("values") || Is("with"))
                        left = Node("in_subquery", "", left, Node("subquery_set", "") with { Query = Select() });
                    else
                    {
                        var operands = new List<Expression> { left };
                        do
                        {
                            if (operands.Count > 1000) throw new SqlException(SqlStates.StatementTooComplex, "IN list exceeds 1000 elements");
                            operands.Add(Expr());
                        } while (Eat(","));
                        left = operands.Count == 2 && operands[1] is { Op: "subquery", Query: { } nestedQuery }
                            ? Node("in_subquery", "", operands[0], Node("subquery_set", "") with { Query = QueryTail(SetQueryTail(nestedQuery, 0)) })
                            : Node("in", "", operands.ToArray());
                    }
                    Expect(")"); nesting--;
                }
                else if (Eat("similar"))
                {
                    Expect("to"); var pattern = Expr(41);
                    left = Eat("escape") ? Node("similar", "", left, pattern, Expr(41)) : Node("similar", "", left, pattern);
                }
                else
                {
                    var matchOperation = Eat("ilike") ? "ilike" : "like"; if (matchOperation == "like") Expect("like"); var pattern = Expr(41);
                    if (Eat("escape")) pattern = Node("call", "like_escape", pattern, Expr(41));
                    left = Node(matchOperation, "", left, pattern);
                }
                if (negate) left = left.Op == "in_subquery" ? left with { Text = "not" } : Node("not", "", left);
                continue;
            }
            var precedence = token.Kind is TokenKind.Word or TokenKind.Symbol ? token.Text switch
            {
                "or" => 10, "and" => 20,
                "~" or "~*" or "!~" or "!~*" => 40,
                "=" or "<>" or "!=" or "<" or "<=" or ">" or ">=" or "@>" or "<@" or "&&" or "-|-" or "<<" or ">>" or "&<" or "&>" or "?" or "?|" or "?&" or "@?" or "@@" => 40,
                "->" or "->>" or "#>" or "#>>" or "#-" => 45,
                "||" => 45, "+" or "-" => 50, "*" or "/" or "%" => 60, "^" => 65,
                _ => -1
            } : -1;
            if (precedence < minimum) break;
            var operation = Take().Text;
            if (operation is "=" or "<>" or "!=" or "<" or "<=" or ">" or ">=")
            {
                if (seenComparison) throw Errors.Syntax("comparison predicates do not associate");
                seenComparison = true;
            }
            nesting++; Depth(nesting);
            if (precedence == 40 && (Is("any") || Is("all") || Is("some")))
            {
                var quantifier = Take().Text; Expect("(");
                var subquery = Is("select") || Is("values") || Is("with");
                var array = subquery ? Node("subquery_set", "") with { Query = Select() } : Expr();
                if (array.Op == "subquery" && (Is("union") || Is("intersect") || Is("except")))
                    array = array with { Query = QueryTail(SetQueryTail(array.Query!, 0)) };
                Expect(")"); nesting--;
                if (array.Op == "subquery") { array = array with { Op = "subquery_set" }; subquery = true; }
                left = Node(subquery ? "quantified_subquery" : "quantified", operation + " " + (quantifier == "all" ? "all" : "any"), left, array);
            }
            else
            {
                var operand = Expr(precedence + 1); nesting--;
                left = Node(operation, "", left, operand);
            }
        }
        return left;
    }
    private Expression ArrayExpression()
    {
        Expect("["); nesting++; Depth(nesting); var values = new List<Expression>();
        if (!Eat("]")) { do { values.Add(Expr()); } while (Eat(",")); Expect("]"); }
        nesting--; return Node("array", "", values.ToArray());
    }
    private Expression Case()
    {
        nesting++; Depth(nesting);
        var subject = Is("when") ? null : Expr();
        var args = new List<Expression>();
        if (!Is("when")) throw Errors.Syntax("CASE requires WHEN");
        while (Eat("when"))
        {
            var condition = Expr();
            if (subject is not null) condition = Node("=", "", subject, condition);
            Expect("then"); args.Add(condition); args.Add(Expr());
        }
        args.Add(Eat("else") ? Expr() : Node("null", ""));
        Expect("end"); nesting--;
        return Node("case", "", args.ToArray());
    }
    private string IntervalQualifier(string name)
    {
        if (!(Is("year") || Is("month") || Is("day") || Is("hour") || Is("minute") || Is("second"))) return name;
        name += " " + Take().Text;
        if (Eat("to")) name += " to " + Name();
        if (Eat("("))
        {
            var sign = Eat("-") ? "-" : Eat("+") ? "+" : "";
            if (token.Kind != TokenKind.Integer) throw Errors.Syntax("expected interval precision");
            name += "(" + sign + Take().Text + ")"; Expect(")");
        }
        return name;
    }
    private string TypeName()
    {
        var quoted = token.Kind == TokenKind.QuotedName;
        var name = Name();
        if (!quoted && name == "character" && Eat("varying")) name += " varying";
        if (!quoted && name == "double" && Eat("precision")) name += " precision";
        if (Eat("("))
        {
            var modifiers = new List<string>();
            do
            {
                var sign = Eat("-") ? "-" : Eat("+") ? "+" : "";
                if (token.Kind != TokenKind.Integer) throw Errors.Syntax("expected type modifier integer");
                modifiers.Add(sign + Take().Text);
            } while (Eat(","));
            Expect(")"); name += "(" + string.Join(',', modifiers) + ")";
        }
        if (name.StartsWith("timestamp", StringComparison.Ordinal) && (Is("with") || Is("without")))
        {
            var withZone = Eat("with"); if (!withZone) Expect("without"); Expect("time"); Expect("zone");
            if (withZone) name = "timestamptz" + name[9..];
        }
        if (name.StartsWith("interval", StringComparison.Ordinal)) name = IntervalQualifier(name);
        while (Eat("[")) { if (token.Kind == TokenKind.Integer) Take(); Expect("]"); if (!name.EndsWith("[]", StringComparison.Ordinal)) name += "[]"; }
        return name;
    }
}
