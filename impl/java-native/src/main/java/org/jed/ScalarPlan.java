package org.jed;

import java.util.ArrayList;
import java.util.List;

/** Resolved FROM-less SELECT. Table plans will share these typed expression kernels. */
final class ScalarPlan implements QueryPlan {
    interface Eval { SqlValue run(CostMeter meter); }
    record Expression(SqlType type, Eval eval, boolean constant, CollationDerivation collation) {
        Expression(SqlType type, Eval eval, boolean constant) { this(type, eval, constant, CollationDerivation.NONE); }
        Expression(SqlType type, Eval eval) { this(type, eval, false); }
        SqlValue run(CostMeter meter) { return eval.run(meter); }
    }
    record WholeField(String name, boolean notNull, boolean nullable, boolean allowed) {}
    record Binding(List<String> qualifier, String name, SqlType type, boolean qualifiedOnly, boolean scalarFunction, CollationDerivation collation, WholeField whole) {
        Binding(List<String> qualifier, String name, SqlType type, boolean qualifiedOnly, boolean scalarFunction, CollationDerivation collation) { this(qualifier, name, type, qualifiedOnly, scalarFunction, collation, new WholeField(name, false, false, !qualifiedOnly)); }
        Binding catalog(String fieldName, boolean notNull) { return new Binding(qualifier, name, type, qualifiedOnly, scalarFunction, collation, new WholeField(fieldName, notNull, whole.nullable(), whole.allowed())); }
        Binding extendNulls(boolean nullable) { return new Binding(qualifier, name, type, qualifiedOnly, scalarFunction, collation, new WholeField(whole.name(), whole.notNull(), nullable || whole.nullable(), whole.allowed())); }
        Binding hidden(boolean hidden) { return new Binding(qualifier, name, type, hidden, scalarFunction, collation, whole); }
        Binding wholeAllowed(boolean allowed) { return new Binding(qualifier, name, type, qualifiedOnly, scalarFunction, collation, new WholeField(whole.name(), whole.notNull(), whole.nullable(), allowed)); }
        Binding(List<String> qualifier, String name, SqlType type, boolean qualifiedOnly, boolean scalarFunction) { this(qualifier, name, type, qualifiedOnly, scalarFunction, CollationDerivation.NONE); }
        Binding(List<String> qualifier, String name, SqlType type) { this(qualifier, name, type, false, false); }
        Binding(List<String> qualifier, String name, SqlType type, boolean qualifiedOnly) { this(qualifier, name, type, qualifiedOnly, false); }
    }
    static final class Frame { List<SqlValue> values = List.of(); }
    private final List<String> names;
    private final List<Expression> projections;
    private final Expression where;
    private final Expression limit;
    private final Expression offset;
    private final boolean empty;
    private boolean produced;
    private final PlanMetrics metrics = new PlanMetrics();
    public void requireAnalyze() {}
    public PlanNode actualPlan(boolean verbose) { return metrics.render(explain(verbose)); }
    private SqlParser.Select explainSyntax;
    private Resolver explainResolver;
    private QueryEnvironment explainEnvironment;

    private ScalarPlan(List<String> names, List<Expression> projections, Expression where,
                       Expression limit, Expression offset, boolean empty) {
        this.names = List.copyOf(names); this.projections = List.copyOf(projections);
        this.where = where; this.limit = limit; this.offset = offset; this.empty = empty;
    }
    @Override public List<String> names() { return names; }
    @Override public List<CollationDerivation> collations() { return projections.stream().map(Expression::collation).toList(); }
    @Override public List<SqlType> types() { return projections.stream().map(Expression::type).toList(); }

    static ScalarPlan resolve(SqlParser.Select syntax, SqlValue[] args) {
        return resolve(syntax, args, null);
    }
    static ScalarPlan resolve(SqlParser.Select syntax, SqlValue[] args, QueryEnvironment environment) {
        if (!syntax.from().isEmpty() || !syntax.group().isEmpty() || syntax.having() != null || !syntax.distinctOn().isEmpty()) {
            throw new SqlException("0A000", "relational query execution is not connected yet");
        }
        Resolver resolver = environment == null ? new Resolver(args) : environment.resolver();
        List<String> names = new ArrayList<>();
        List<Expression> projections = new ArrayList<>();
        for (SqlParser.Item item : ProjectionSyntax.expand(syntax.items(), resolver)) {
            names.add(item.alias() != null ? item.alias() : outputName(item.expression()));
            projections.add(resolver.resolve(item.expression(), null));
            if (syntax.distinct()) CollationDerivation.requireSort(projections.getLast(), item.expression());
        }
        Expression where = syntax.where() == null ? null : syntax.where() instanceof SqlParser.Binary binary && binary.op().equals("and")
            ? resolver.binary(binary, true) : resolver.resolve(syntax.where(), SqlType.BOOLEAN);
        if (where != null && where.type != SqlType.BOOLEAN) throw new SqlException("42804", "WHERE requires boolean");
        for (SqlParser.Order order : syntax.order()) {
            if (order.expression() instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.INTEGER) {
                long ordinal = integer(literal.text());
                if (ordinal < 1 || ordinal > projections.size()) throw new SqlException("42P10", "ORDER BY position is out of range");
            } else if (order.expression() instanceof SqlParser.Name name && name.parts().size() == 1 && names.contains(name.parts().getFirst())) {
                // Resolved output reference. One virtual row needs no comparison.
            } else CollationDerivation.requireSort(resolver.resolve(order.expression(), null), order.expression());
        }
        Expression limit = syntax.limit() == null ? null : resolver.resolve(syntax.limit(), SqlType.I64);
        Expression offset = syntax.offset() == null ? null : resolver.resolve(syntax.offset(), SqlType.I64);
        if (limit != null && !limit.type.isInteger() || offset != null && !offset.type.isInteger()) {
            throw new SqlException("42804", "LIMIT and OFFSET require integers");
        }
        ScalarPlan plan = new ScalarPlan(names, projections, where, limit, offset, Contradictions.proven(syntax.where(), args));
        plan.explainSyntax = syntax; plan.explainResolver = resolver; plan.explainEnvironment = environment; return plan;
    }

    @Override public PlanNode explain(boolean verbose) {
        SqlParser.Select syntax = explainSyntax; ExplainExpression printer = new ExplainExpression(List.of(), explainResolver, explainEnvironment.meter());
        long rows = PlanEstimate.rows(syntax.where(), 1, List.of());
        PlanNode node = new PlanNode("Result", "", empty ? 0 : 1, 0, List.of(), "buffered");
        if (syntax.where() != null) node = node.wrap("Filter", (verbose ? "filter=" + printer.print(syntax.where()) : "conjuncts=" + FilterRules.terms(syntax.where()).size()) + (empty ? "; contradiction" : ""), rows, empty ? 0 : PlanEstimate.operations(syntax.where()));
        long offset = TableExplanation.boundEstimate(syntax.offset(), 0, explainResolver, explainEnvironment.meter()), limit = TableExplanation.boundEstimate(syntax.limit(), Long.MAX_VALUE, explainResolver, explainEnvironment.meter());
        if (syntax.limit() != null || syntax.offset() != null) node = node.wrap("Limit", (limit == Long.MAX_VALUE ? "" : "limit=" + limit) + (syntax.offset() == null ? "" : (limit == Long.MAX_VALUE ? "" : " ") + "offset=" + offset), Math.min(limit, Math.max(0, rows - offset)), 0);
        List<SqlParser.Expr> output = syntax.items().stream().map(SqlParser.Item::expression).toList(); long work = 1;
        for (SqlParser.Expr expression : output) work = PlanEstimate.add(work, PlanEstimate.operations(expression));
        return node.finish(verbose ? TableExplanation.append(node.detail(), "output=[" + printer.list(output) + "]") : node.detail(), PlanEstimate.mul(node.rows(), work), "buffered");
    }

    @Override public Row next(CostMeter meter) {
        long before = meter.cost();
        try { return nextMeasured(meter); } finally { metrics.total(before, meter); }
    }
    private Row nextMeasured(CostMeter meter) {
        if (produced) return null;
        produced = true;
        long take = bound(limit, meter, Long.MAX_VALUE, "2201W");
        long skip = bound(offset, meter, 0, "2201X");
        if (take == 0 || empty) return null;
        meter.guard(); meter.memory().reserve(MemorySchedule.ROW);
        try {
            if (where != null) { long before = meter.cost(); boolean accepted; try { accepted = filter(where, meter); } finally { metrics.add("Filter", before, meter); } if (!accepted) return null; }
            if (skip != 0) return null;
            meter.charge(Costs.ROW_PRODUCED); meter.guard();
            List<SqlValue> values = new ArrayList<>(projections.size());
            for (Expression expression : projections) values.add(expression.run(meter));
            return new Row(names, values);
        } finally { meter.memory().release(MemorySchedule.ROW); }
    }
    private static long bound(Expression expression, CostMeter meter, long fallback, String state) {
        if (expression == null) return fallback;
        SqlValue value = expression.run(meter);
        if (value.isNull()) return fallback;
        long number = value.asInteger();
        if (number < 0) throw new SqlException(state, "negative LIMIT or OFFSET");
        return number;
    }
    private static boolean filter(Expression expression, CostMeter meter) {
        SqlValue value = expression.run(meter);
        return !value.isNull() && value.asBoolean();
    }
    static String outputName(SqlParser.Expr expr) {
        if (expr instanceof SqlParser.Name name) return name.parts().getLast();
        if (expr instanceof SqlParser.Call call) return call.name();
        if (expr instanceof SqlParser.Window window) return window.function().name();
        if (expr instanceof SqlParser.Field field && field.name() != null) return field.name();
        if (expr instanceof SqlParser.Cast cast) return outputName(cast.value());
        if (expr instanceof SqlParser.Access access) return outputName(access.base());
        if (expr instanceof SqlParser.Extrema extrema) return extrema.kind();
        return "?column?";
    }
    static Expression constant(SqlValue value) { return new Expression(value.type(), ignored -> value, true); }
    private static boolean unknown(SqlParser.Expr expr) {
        return expr instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.WORD && literal.text().equals("null");
    }
    private static boolean integerLiteral(SqlParser.Expr expr) { return expr instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.INTEGER; }
    static int ordinal(String text) {
        long value = integer(text);
        if (value < Integer.MIN_VALUE || value > Integer.MAX_VALUE) throw new NumberFormatException("ordinal outside i32");
        return (int) value;
    }
    private static boolean numericLiteral(SqlParser.Expr expr) { return expr instanceof SqlParser.Literal literal && (literal.kind() == Lexer.Kind.INTEGER || literal.kind() == Lexer.Kind.DECIMAL); }
    private static boolean adaptableLiteral(SqlParser.Expr expr) { return expr instanceof SqlParser.Parameter || expr instanceof SqlParser.Literal literal && (literal.kind() == Lexer.Kind.STRING || literal.kind() == Lexer.Kind.INTEGER || literal.kind() == Lexer.Kind.DECIMAL); }
    static long integer(String text) {
        boolean negative = text.startsWith("-");
        String magnitude = negative ? text.substring(1) : text;
        int radix = 10;
        if (magnitude.length() > 2 && magnitude.charAt(0) == '0') {
            radix = switch (magnitude.charAt(1)) { case 'x', 'X' -> 16; case 'o', 'O' -> 8; case 'b', 'B' -> 2; default -> 10; };
            if (radix != 10) magnitude = magnitude.substring(2);
        }
        // Negative accumulation represents the i64 minimum without an overflowing positive magnitude.
        long value = 0;
        for (int i = 0; i < magnitude.length(); i++) {
            int digit = Character.digit(magnitude.charAt(i), radix);
            if (digit < 0 || value < (Long.MIN_VALUE + digit) / radix) throw new SqlException("42601", "integer literal magnitude exceeds 2^63");
            value = value * radix - digit;
        }
        if (!negative && value == Long.MIN_VALUE) throw SqlException.numericOverflow();
        return negative ? value : -value;
    }

    static final class Resolver {
        private final SqlValue[] arguments;
        private final List<Binding> bindings;
        private List<Binding> groupedBindings = List.of();
        Resolver grouped(Resolver input) { groupedBindings = input.bindings; return this; }
        private final Frame frame;
        private final QueryEnvironment environment;
        QueryEnvironment queryEnvironment() { return environment; }
        private final Resolver outer;
        private boolean metadataBridge;
        private List<String> excludedLocalNames = List.of();
        private String excludedLocalAlias;
        Resolver excludeLocal(List<String> names, String alias) {
            excludedLocalNames = List.copyOf(names); excludedLocalAlias = alias; return this;
        }

        private final java.util.BitSet touched = new java.util.BitSet();
        private final java.util.BitSet forbidden = new java.util.BitSet();
        void forbidLocal() { forbidden.set(0, bindings.size()); }
        private java.util.function.IntConsumer touchObserver;
        private boolean outerReference;
        private long localReads, outerReads;
        private boolean persistedCollations;
        final java.util.IdentityHashMap<SqlParser.Call, List<HostFunctions.Call>> hostCalls = new java.util.IdentityHashMap<>();
        private List<HostFunction> schemaFunctions;
        Resolver schemaFunctions(List<HostFunction> functions) { schemaFunctions = List.copyOf(functions); return this; }
        List<HostFunction> hostFunctions() { return schemaFunctions != null ? schemaFunctions : meter() == null ? List.of() : meter().environment().functions(); }
        Resolver persisted() { persistedCollations = true; return this; }
        boolean persistedCollations() { return persistedCollations; }
        CostMeter meter() { return environment == null ? null : environment.meter(); }
        private boolean immutableHostCalls(SqlParser.Expr syntax) {
            if (syntax == null || hostFunctions().isEmpty()) return true;
            SqlParser.Expr expression = DefaultExpression.unwrap(syntax);
            if (expression instanceof SqlParser.Call call) {
                Expression resolved = HostFunctions.resolve(call, this);
                if (resolved != null && resolved.eval() instanceof HostFunctions.Call host && host.function.volatility() != HostFunction.Volatility.IMMUTABLE) return false;
            }
            for (SqlParser.Expr child : DefaultExpression.children(expression)) if (!immutableHostCalls(child)) return false;
            return true;
        }
        SqlValue literalValue(String text, SqlType type) { return SqlCasts.literal(text, type, environment == null ? null : environment.meter()); }
        long[] referenceUsage() { return new long[] {localReads, outerReads}; }
        boolean outerOnlySince(long[] usage) { return outerReads > usage[1] && localReads == usage[0]; }
        boolean usedOuterReference() { return outerReference; }
        private void markTouched(int index) { if (forbidden.get(index)) throw new SqlException("42P10", "LATERAL reference requires INNER or LEFT JOIN"); localReads++; touched.set(index); if (touchObserver != null) touchObserver.accept(index); }
        Resolver mergeEarlier(Resolver earlier) {
            if (earlier == null) return this;
            var combined = new ArrayList<Binding>(earlier.bindings); combined.addAll(bindings);
            Frame combinedFrame = new Frame(); int boundary = earlier.bindings.size();
            combinedFrame.values = new java.util.AbstractList<>() {
                public int size() { return combined.size(); }
                public SqlValue get(int index) { return index < boundary ? earlier.frame.values.get(index) : frame.values.get(index - boundary); }
            };
            // Preserve the original query's correlation observer when a lookup leaves the merged sibling namespace.
            Resolver outside = earlier.outer == null ? null : new Resolver(arguments, List.of(), new Frame(), earlier.environment, earlier.outer);
            if (outside != null) outside.metadataBridge = true;
            Resolver result = new Resolver(arguments, combined, combinedFrame, environment, outside);
            for (int i = earlier.forbidden.nextSetBit(0); i >= 0; i = earlier.forbidden.nextSetBit(i + 1)) result.forbidden.set(i);
            for (int i = forbidden.nextSetBit(0); i >= 0; i = forbidden.nextSetBit(i + 1)) result.forbidden.set(boundary + i);
            result.touchObserver = index -> { if (index < boundary) earlier.markTouched(index); else markTouched(index - boundary); };
            return result;
        }
        SqlValue planParameter(int index) { return index > 0 && index <= arguments.length ? arguments[index - 1] : null; }
        boolean provenContradiction(SqlParser.Expr expression) { return Contradictions.proven(expression, arguments); }
        boolean[] touched() {
            boolean[] result = new boolean[bindings.size()];
            for (int i = touched.nextSetBit(0); i >= 0; i = touched.nextSetBit(i + 1)) result[i] = true;
            return result;
        }
        Resolver(SqlValue[] arguments) { this(arguments, List.of(), new Frame()); }
        Resolver(SqlValue[] arguments, List<Binding> bindings, Frame frame) {
            this(arguments, bindings, frame, null, null);
        }
        Resolver(SqlValue[] arguments, List<Binding> bindings, Frame frame, QueryEnvironment environment, Resolver outer) {
            this.arguments = arguments; this.bindings = List.copyOf(bindings); this.frame = frame;
            this.environment = environment; this.outer = outer;
        }

        private record Resolved(Expression expression, boolean[] local, boolean[] inherited) {}
        private final java.util.IdentityHashMap<SqlParser.Expr, java.util.Map<SqlType, Resolved>> resolved = new java.util.IdentityHashMap<>();
        private final java.util.IdentityHashMap<SqlParser.Expr, Boolean> constants = new java.util.IdentityHashMap<>();
        private int resolving;

        Expression resolve(SqlParser.Expr syntax, SqlType context) {
            // Dispatch may inspect an operand before supplying a contextual type. Keep the
            // already resolved subtree, keyed by syntax identity (record hashing is recursive).
            // Only forms which consume a hint need a distinct entry for that hint.
            if (!(syntax instanceof SqlParser.Literal || syntax instanceof SqlParser.Parameter
                    || syntax instanceof SqlParser.ArrayConstructor || syntax instanceof SqlParser.RowConstructor
                    || syntax instanceof SqlParser.Conditional || syntax instanceof SqlParser.StoredExpression
                    || syntax instanceof SqlParser.Collate)) context = null;
            resolving++;
            try {
                var variants = resolved.get(syntax);
                Resolved cached = variants == null ? null : variants.get(context);
                if (cached != null) {
                    int index = 0;
                    for (Resolver scope = this; scope != null; scope = scope.outer, index++) {
                        if (cached.local()[index]) scope.localReads++;
                        if (cached.inherited()[index]) scope.outerReads++;
                    }
                    return cached.expression();
                }
                List<long[]> before = new ArrayList<>();
                for (Resolver scope = this; scope != null; scope = scope.outer) before.add(scope.referenceUsage());
                Expression result = resolveInner(syntax, context);
                result = new Expression(result.type, result.eval, (result.constant || constantSyntax(syntax, constants)) && immutableHostCalls(syntax), CollationDerivation.derive(syntax, result.type, this, result.collation));
                boolean[] local = new boolean[before.size()], inherited = new boolean[before.size()];
                int index = 0;
                for (Resolver scope = this; scope != null; scope = scope.outer, index++) {
                    local[index] = scope.localReads != before.get(index)[0];
                    inherited[index] = scope.outerReads != before.get(index)[1];
                }
                resolved.computeIfAbsent(syntax, ignored -> new java.util.HashMap<>()).put(context, new Resolved(result, local, inherited));
                return result;
            } finally {
                // Resolution reuse never becomes an execution-result cache or crosses plans.
                if (--resolving == 0) { resolved.clear(); constants.clear(); }
            }
        }
        private Expression resolveInner(SqlParser.Expr syntax, SqlType context) {
            if (syntax instanceof SqlParser.Extrema extrema) return extrema(extrema);
            if (syntax instanceof SqlParser.JsonSql json) return JsonSqlFunctions.resolve(json, this);
            if (syntax instanceof SqlParser.Window) throw new SqlException("42P20", "window function is not allowed here");
            if (syntax instanceof SqlParser.ColumnSlot slot) { markTouched(slot.index()); return new Expression(bindings.get(slot.index()).type(), ignored -> frame.values.get(slot.index()), false, bindings.get(slot.index()).collation()); }
            if (syntax instanceof SqlParser.Collate collate) {
                Expression value = resolve(collate.value(), context == null && unknown(collate.value()) ? SqlType.TEXT : context);
                if (!CollationDerivation.collatable(value.type())) throw SqlException.typeMismatch("COLLATE requires text or text array");
                if (meter() != null) meter().environment().collation(collate.name());
                return new Expression(value.type(), value.eval(), value.constant(), CollationDerivation.explicit(collate.name()));
            }
            if (syntax instanceof SqlParser.UnparsedExpression unparsed) throw StoredDefinitions.error(unparsed.detail());
            if (syntax instanceof SqlParser.StoredExpression stored) return resolve(stored.expression(), context);
            if (syntax instanceof SqlParser.Subquery query && environment != null) return environment.subquery(syntax, query.query(), false, this);
            if (syntax instanceof SqlParser.Exists query && environment != null) return environment.subquery(syntax, query.query(), true, this);
            if (syntax instanceof SqlParser.ArrayConstructor array) return ArrayExpressions.constructor(array, context, this);
            if (syntax instanceof SqlParser.Quantified quantified) return quantified.query() != null && environment != null ? environment.quantified(quantified, this) : ArrayExpressions.quantified(quantified, this);
            if (syntax instanceof SqlParser.RowConstructor row) return CompositeExpressions.constructor(row, context, this);
            if (syntax instanceof SqlParser.Field field) return CompositeExpressions.field(field, this);
            if (syntax instanceof SqlParser.Access access) return ArrayExpressions.access(access, this);
            if (syntax instanceof SqlParser.StoredValue stored) return constant(stored.value());
            if (syntax instanceof SqlParser.Literal literal) {
                return switch (literal.kind()) {
                    case INTEGER -> {
                        long value = integer(literal.text());
                        if (context != null && context.isInteger()) yield constant(SqlValue.ofInteger(context, value));
                        if (context != null && context.kind() == SqlType.Kind.DECIMAL) yield constant(SqlValue.ofDecimal(Decimal.of(value)));
                        if (context != null && context.isFloat()) yield constant(SqlValue.ofFloat(context, context == SqlType.F32 ? (double)(float)value : (double)value));
                        yield constant(SqlValue.ofInteger(value));
                    }
                    case DECIMAL -> constant(context != null && context.isFloat()
                        ? SqlCasts.cast(SqlValue.ofDecimal(Decimal.parse(literal.text())), context)
                        : SqlValue.ofDecimal(Decimal.parse(literal.text())));
                    case STRING -> context == SqlType.DATE && Temporal.clockDate(literal.text()) ? Temporal.dateLiteral(literal.text()) : constant(context != null && switch (context.kind()) {
                        case BYTEA, UUID, DATE, TIMESTAMP, TIMESTAMPTZ, INTERVAL, JSON, JSONB, JSONPATH, ARRAY, RANGE, COMPOSITE -> true;
                        default -> false;
                    } ? SqlCasts.literal(literal.text(), context, environment == null ? null : environment.meter()) : SqlValue.ofText(literal.text()));
                    case WORD -> constant(literal.text().equals("null") ? SqlValue.nullValue(context == null ? SqlType.UNKNOWN : context) : SqlValue.ofBoolean(literal.text().equals("true")));
                    default -> throw new IllegalStateException("unexpected literal kind");
                };
            }
            if (syntax instanceof SqlParser.Name name) {
                Expression expression = lookup(name);
                if (expression == null) throw new SqlException("42703", "column does not exist: " + String.join(".", name.parts()));
                return expression;
            }
            if (syntax instanceof SqlParser.Parameter parameter) {
                if (parameter.index() > arguments.length) {
                    if (environment != null && environment.explainProbe()) return new Expression(context == null ? SqlType.UNKNOWN : context, ignored -> { throw new SqlException("42P02", "missing parameter $" + parameter.index()); }, true);
                    throw new SqlException("42P02", "missing parameter $" + parameter.index());
                }
                SqlValue value = arguments[parameter.index() - 1];
                if (context != null && context.isInteger() && value.type().isInteger()) value = value.isNull() ? SqlValue.nullValue(context) : SqlValue.ofInteger(context, value.asInteger());
                if (context != null && context.kind() == SqlType.Kind.INTERVAL && value.type().kind() == SqlType.Kind.TEXT) value = SqlCasts.cast(value, context);
                return constant(value);
            }
            if (syntax instanceof SqlParser.Cast cast) {
                SqlType target = type(cast.target());
                if (target == SqlType.DATE && cast.value() instanceof SqlParser.Literal text && text.kind() == Lexer.Kind.STRING && Temporal.clockDate(text.text())) return Temporal.dateLiteral(text.text());
                if (target.isComposite() && cast.value() instanceof SqlParser.RowConstructor row) {
                    Expression value = CompositeExpressions.constructor(row, target, this, true);
                    return new Expression(target, meter -> { meter.operation(); return value.run(meter); }, value.constant());
                }
                SqlType inputContext = target.isInteger() || unknown(cast.value()) || target.kind() == SqlType.Kind.ARRAY && cast.value() instanceof SqlParser.ArrayConstructor array && ArrayExpressions.needsContext(array) ? target
                    : target == SqlType.BOOLEAN && integerLiteral(cast.value()) ? SqlType.I32 : null;
                Expression operand = resolve(cast.value(), inputContext);
                if (unknown(cast.value())) {
                    SqlValue value = SqlValue.nullValue(target);
                    if (target.kind() == SqlType.Kind.ARRAY || target.kind() == SqlType.Kind.RANGE || target.isComposite() || target.kind() == SqlType.Kind.TEXT || target == SqlType.JSONPATH) return constant(value);
                    return new Expression(target, meter -> { meter.operation(); return value; }, true);
                }
                if ((target.kind() == SqlType.Kind.RANGE || target.kind() == SqlType.Kind.ARRAY) && operand.type.equals(target)) return operand;
                if (operand.type.kind() == SqlType.Kind.RECORD && target.kind() == SqlType.Kind.COMPOSITE || !SqlCasts.allowed(operand.type, target) && (operand.type.isComposite() || target.isComposite())) throw CompositeValues.cannotCast();
                if (!SqlCasts.allowed(operand.type, target)) throw new SqlException(target == SqlType.JSONPATH ? "42846" : "42804", "cannot cast " + operand.type + " to " + target);
                if (cast.value() instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.STRING) {
                    SqlValue parsed = SqlCasts.literal(literal.text(), target, environment == null ? null : environment.meter());
                    if (target.isComposite()) return new Expression(target, meter -> { meter.operation(); return parsed; }, true);
                    return constant(parsed);
                }
                return new Expression(target, meter -> {
                    meter.operation(); SqlValue source = operand.run(meter);
                    SqlValue result = SqlCasts.cast(source, target, meter);
                    if (!operand.constant && !result.isNull()) {
                        if (target.kind() == SqlType.Kind.TEXT) {
                            if (operand.type == SqlType.JSONB) meter.scalarInput(source.asJsonb().renderedBytes());
                            else if (operand.type == SqlType.BYTEA) meter.scalarInput(2L + 2L * source.asBytes().length);
                            else if (operand.type.kind() == SqlType.Kind.ARRAY || operand.type.isComposite()) meter.scalarInput(SqlText.encodedLength(Temporal.renderValue(source, meter.zone())));
                        } else if (target == SqlType.JSON && operand.type == SqlType.JSONB) meter.scalarInput(source.asJsonb().renderedBytes());
                        else if (target.kind() == SqlType.Kind.ARRAY && target.element().kind() == SqlType.Kind.TEXT && operand.type.kind() == SqlType.Kind.ARRAY && operand.type.element() == SqlType.BYTEA) meter.scalarInput(MemoryAccount.payload(result));
                    }
                    return result;
                });
            }
            if (syntax instanceof SqlParser.Unary unary) return unary(unary);
            if (syntax instanceof SqlParser.Binary binary) return binary(binary, false);
            if (syntax instanceof SqlParser.Between between) {
                Expression result = binary(new SqlParser.Binary("and", new SqlParser.Binary(">=", between.subject(), between.lower()), new SqlParser.Binary("<=", between.subject(), between.upper())), false);
                return between.negated() ? negate(result) : result;
            }
            if (syntax instanceof SqlParser.In in) {
                if (in.query() != null && environment != null) return environment.membership(in, this);
                if (in.query() != null) throw new SqlException("0A000", "IN subquery execution is not connected yet");
                List<Expression> comparisons = new ArrayList<>();
                for (SqlParser.Expr candidate : in.values()) comparisons.add(binary(new SqlParser.Binary("=", in.subject(), candidate), false));
                Expression result = new Expression(SqlType.BOOLEAN, meter -> {
                    for (int i = 1; i < comparisons.size(); i++) meter.operation();
                    boolean unknown = false;
                    for (Expression comparison : comparisons) {
                        SqlValue value = comparison.run(meter);
                        if (value.isNull()) unknown = true;
                        else if (value.asBoolean()) return SqlValue.ofBoolean(true);
                    }
                    return unknown ? SqlValue.nullValue(SqlType.BOOLEAN) : SqlValue.ofBoolean(false);
                });
                return in.negated() ? negate(result) : result;
            }
            if (syntax instanceof SqlParser.Call call) {
                if (call.name().equals("merge_action")) {
                    if (environment == null || environment.mergeAction == null) throw new SqlException("42601", "merge_action() is only valid in MERGE RETURNING");
                    if (!call.args().isEmpty()) throw new SqlException("42883", "merge_action() takes no arguments");
                    return environment.mergeAction;
                }
                if (WindowCatalog.FACTS.stream().anyMatch(fact -> fact.name().equals(call.name()))) throw new SqlException("42809", "window function requires OVER");
                return call(call);
            }
            if (syntax instanceof SqlParser.Conditional conditional) return conditional(conditional, context);
            throw new SqlException("0A000", "expression is not implemented yet");
        }
        List<Binding> metadataBindings() { return bindings; }
        Resolver metadataOuter() {
            Resolver next = outer;
            while (next != null && next.metadataBridge) next = next.outer;
            return next;
        }
        boolean hasColumn(SqlParser.Name name) { return lookup(name) != null; }
        SqlParser.Name canonicalColumn(SqlParser.Name name) {
            List<Binding> matches = bindings.stream().filter(binding -> binding.name().equals(name.parts().getLast()) && (name.parts().size() == 1 && !binding.qualifiedOnly() || name.parts().size() > 1 && binding.qualifier().equals(name.parts().subList(0, name.parts().size() - 1)))).toList();
            if (matches.size() == 1) { var parts = new ArrayList<>(matches.getFirst().qualifier()); parts.add(matches.getFirst().name()); return new SqlParser.Name(parts); }
            return matches.isEmpty() && outer != null ? outer.canonicalColumn(name) : name;
        }
        private Expression lookup(SqlParser.Name name) {
            Expression column = lookup(name, false);
            return column != null || name.parts().size() != 1 ? column : lookup(name, true);
        }
        private Expression lookup(SqlParser.Name name, boolean wholeRow) {
                if (name.parts().size() == 1 && (excludedLocalNames.contains(name.parts().getFirst()) || name.parts().getFirst().equals(excludedLocalAlias))
                    || name.parts().size() > 1 && name.parts().get(name.parts().size() - 2).equals(excludedLocalAlias)) return null;

                if (wholeRow) {
                    var positions = new ArrayList<Integer>(); var fields = new ArrayList<SqlType.Field>();
                    boolean nullable = false; int sentinel = -1;
                    for (int i = 0; i < bindings.size(); i++) {
                        Binding binding = bindings.get(i);
                        if (!binding.whole().allowed() || !binding.qualifier().equals(name.parts())) continue;
                        if (binding.scalarFunction()) { markTouched(i); int column = i; return new Expression(binding.type(), ignored -> frame.values.get(column), false, binding.collation()); }
                        positions.add(i); fields.add(new SqlType.Field(binding.whole().name(), binding.type()));
                        nullable |= binding.whole().nullable(); if (sentinel < 0 && binding.whole().notNull()) sentinel = i;
                    }
                    if (!positions.isEmpty()) {
                        if (persistedCollations) throw new SqlException("0A000", "whole-row references are not supported in persisted indexes");
                        if (nullable && sentinel < 0) throw new SqlException("0A000", "a nullable whole-row reference requires a NOT NULL column");
                        positions.forEach(this::markTouched); SqlType type = SqlType.record(fields); int guard = nullable ? sentinel : -1;
                        return new Expression(type, meter -> {
                            if (guard >= 0) { meter.operation(); meter.operation(); if (frame.values.get(guard).isNull()) return SqlValue.nullValue(type); }
                            meter.operation(); var values = new ArrayList<SqlValue>(positions.size());
                            for (int position : positions) values.add(frame.values.get(position));
                            return SqlValue.ofComposite(type, values);
                        });
                    }
                }
                int found = -1;
                List<String> prefix = name.parts().subList(0, name.parts().size() - 1);
                for (int i = 0; i < bindings.size(); i++) {
                    Binding binding = bindings.get(i);
                    boolean qualifierMatches = prefix.isEmpty() && !binding.qualifiedOnly || !prefix.isEmpty() && prefix.equals(binding.qualifier)
                        || prefix.size() == 1 && !binding.qualifier.isEmpty() && prefix.getFirst().equals(binding.qualifier.getLast());
                    if (wholeRow ? binding.scalarFunction() && binding.qualifier().equals(name.parts()) : binding.name.equals(name.parts().getLast()) && qualifierMatches) {
                        if (found >= 0) throw new SqlException("42702", "ambiguous column: " + name.parts().getLast());
                        if (forbidden.get(i)) throw new SqlException("42P10", "LATERAL reference requires INNER or LEFT JOIN");
                        found = i;
                    }
                }
                if (found < 0 && groupedBindings.stream().anyMatch(binding -> binding.name().equals(name.parts().getLast()) && (prefix.isEmpty() && !binding.qualifiedOnly() || !prefix.isEmpty() && prefix.equals(binding.qualifier()))))
                    throw new SqlException("42803", "subquery uses ungrouped column: " + String.join(".", name.parts()));
                if (found < 0 && outer != null) {
                    if (!prefix.isEmpty() && bindings.stream().anyMatch(binding -> prefix.equals(binding.qualifier)
                        || prefix.size() == 1 && !binding.qualifier.isEmpty() && prefix.getFirst().equals(binding.qualifier.getLast()))) return null;
                    Expression inherited = outer.lookup(name, wholeRow);
                    if (inherited != null) { outerReference = true; outerReads++; if (environment != null) environment.markCorrelated(); return inherited; }
                }
                if (found < 0) {
                    if (!prefix.isEmpty() && bindings.stream().noneMatch(binding -> prefix.equals(binding.qualifier)
                        || prefix.size() == 1 && !binding.qualifier.isEmpty() && prefix.getFirst().equals(binding.qualifier.getLast()))) {
                        throw new SqlException("42P01", "relation does not exist: " + String.join(".", prefix));
                    }
                    return null;
                }
                int index = found;
                markTouched(index);
                return new Expression(bindings.get(index).type, ignored -> frame.values.get(index), false, bindings.get(index).collation());
        }
        private static Expression negate(Expression operand) {
            return new Expression(SqlType.BOOLEAN, meter -> { meter.operation(); SqlValue value = operand.run(meter); return value.isNull() ? value : SqlValue.ofBoolean(!value.asBoolean()); });
        }

        SqlType type(SqlParser.TypeName type) {
            SqlType result;
            try { result = SqlType.named(type.name()); }
            catch (SqlException failure) { result = environment == null ? null : environment.namedType(type.name()); if (result == null) throw failure; }
            if (result.kind() == SqlType.Kind.INTERVAL) {
                if (type.intervalFields() != 0 || type.precision() >= 0) result = SqlType.interval(type.intervalFields(), type.precision());
                if (type.dimensions() != 0 && type.intervalFields() != 0) throw new SqlException("0A000", "qualified interval arrays are not supported");
                return type.dimensions() == 0 ? result : SqlType.array(result);
            }
            if (type.precision() != -1) {
                if (result.kind() == SqlType.Kind.DECIMAL) result = SqlType.decimal(type.precision(), type.scale());
                else if (result.kind() == SqlType.Kind.TEXT) result = SqlType.varchar(type.precision());
                else if (result.kind() == SqlType.Kind.TIMESTAMP || result.kind() == SqlType.Kind.TIMESTAMPTZ) result = SqlType.timestamp(result.kind() == SqlType.Kind.TIMESTAMPTZ, type.precision());
                else throw new SqlException("0A000", "type modifier is not implemented yet");
            }
            if (type.dimensions() != 0) result = SqlType.array(result);
            return result;
        }

        private Expression unary(SqlParser.Unary syntax) {
            Expression jsonPredicate = JsonPredicates.resolve(syntax, this); if (jsonPredicate != null) return jsonPredicate;
            String op = syntax.op();
            Expression operand = resolve(syntax.value(), op.equals("not") ? SqlType.BOOLEAN : null);
            if (op.equals("not") && operand.type != SqlType.BOOLEAN) throw new SqlException("42804", "NOT requires boolean");
            if (op.equals("+") || op.equals("-")) {
                if (op.equals("-") && operand.type.kind() == SqlType.Kind.INTERVAL) return TemporalOps.negateInterval(operand);
                if (!operand.type.isNumeric()) throw new SqlException("42883", "numeric unary operator requires numeric input");
                return new Expression(operand.type, meter -> {
                    meter.operation(); SqlValue value = operand.run(meter);
                    if (value.isNull() || op.equals("+")) return value;
                    if (value.type().isInteger()) {
                        try { return SqlValue.ofInteger(value.type(), Math.negateExact(value.asInteger())); }
                        catch (ArithmeticException error) { throw SqlException.numericOverflow(); }
                    }
                    if (value.type().isFloat()) return SqlValue.ofFloat(value.type(), -value.asFloat());
                    return SqlValue.ofDecimal(value.asDecimal().negate());
                });
            }
            return new Expression(SqlType.BOOLEAN, meter -> {
                meter.operation(); SqlValue value = operand.run(meter);
                return switch (op) {
                    case "not" -> value.isNull() ? value : SqlValue.ofBoolean(!value.asBoolean());
                    case "is null", "is unknown" -> SqlValue.ofBoolean(value.type().isComposite() ? CompositeValues.allNull(value) : value.isNull());
                    case "is not null", "is not unknown" -> SqlValue.ofBoolean(value.type().isComposite() ? CompositeValues.allPresent(value) : !value.isNull());
                    case "is true" -> SqlValue.ofBoolean(!value.isNull() && value.asBoolean());
                    case "is false" -> SqlValue.ofBoolean(!value.isNull() && !value.asBoolean());
                    case "is not true" -> SqlValue.ofBoolean(value.isNull() || !value.asBoolean());
                    case "is not false" -> SqlValue.ofBoolean(value.isNull() || value.asBoolean());
                    default -> throw new SqlException("0A000", "unary expression not implemented");
                };
            });
        }

        Expression binary(SqlParser.Binary syntax, boolean filterContext) {
            if (filterContext && syntax.op().equals("and")) {
                List<SqlParser.Expr> terms = FilterRules.terms(syntax);
                List<Expression> expressions = new ArrayList<>();
                for (SqlParser.Expr term : terms) {
                    Expression expression = predicate(term);
                    if (expression.type() != SqlType.BOOLEAN) throw SqlException.typeMismatch("logical operator requires boolean");
                    expressions.add(expression);
                }
                List<Integer> order = new ArrayList<>();
                for (int i = 0; i < terms.size(); i++) order.add(i);
                order.sort(java.util.Comparator.<Integer>comparingInt(i -> FilterRules.correlated(terms.get(i), environment) ? 1 : 0)
                    .thenComparingLong(i -> FilterRules.nodes(terms.get(i), environment)));
                boolean changed = false;
                for (int i = 0; i < order.size(); i++) changed |= order.get(i) != i;
                if (changed) return new Expression(SqlType.BOOLEAN, meter -> {
                    meter.charge((expressions.size() - 1L) * Costs.OPERATOR_EVAL); meter.guard();
                    for (int index : order) {
                        SqlValue value = expressions.get(index).run(meter);
                        if (value.isNull() || !value.asBoolean()) return value;
                    }
                    return SqlValue.ofBoolean(true);
                });
                return filterWritten(syntax, expressions.iterator());
            }
            return binaryWritten(syntax, filterContext);
        }
        Expression predicate(SqlParser.Expr syntax) {
            if (syntax instanceof SqlParser.Between between && !between.negated()) return binary(new SqlParser.Binary("and", new SqlParser.Binary(">=", between.subject(), between.lower()), new SqlParser.Binary("<=", between.subject(), between.upper())), true);
            return syntax instanceof SqlParser.Binary binary && binary.op().equals("and") ? binary(binary, true) : resolve(syntax, SqlType.BOOLEAN);
        }
        Expression writtenPredicate(SqlParser.Expr syntax) {
            if (syntax instanceof SqlParser.Binary binary && binary.op().equals("and")) {
                Expression left = writtenPredicate(binary.left()), right = writtenPredicate(binary.right());
                if (left.type() != SqlType.BOOLEAN || right.type() != SqlType.BOOLEAN) throw SqlException.typeMismatch("logical operator requires boolean");
                return new Expression(SqlType.BOOLEAN, meter -> { meter.operation(); SqlValue value = left.run(meter); return value.isNull() || !value.asBoolean() ? value : right.run(meter); });
            }
            return resolve(syntax, SqlType.BOOLEAN);
        }
        private Expression filterWritten(SqlParser.Expr syntax, java.util.Iterator<Expression> terms) {
            if (!(syntax instanceof SqlParser.Binary binary) || !binary.op().equals("and")) return terms.next();
            Expression left = filterWritten(binary.left(), terms), right = filterWritten(binary.right(), terms);
            return new Expression(SqlType.BOOLEAN, meter -> {
                meter.operation(); SqlValue first = left.run(meter);
                return first.isNull() || !first.asBoolean() ? first : right.run(meter);
            });
        }
        private Expression binaryWritten(SqlParser.Binary syntax, boolean filterContext) {
            String op = syntax.op();
            if (environment != null && syntax.left() instanceof SqlParser.RowConstructor && syntax.right() instanceof SqlParser.Subquery
                    && List.of("=", "<>", "!=", "<", ">", "<=", ">=").contains(op)) return environment.rowComparison(syntax, this);
            Expression textConcat = TextConcat.resolve(syntax, this); if (textConcat != null) return textConcat;
            Expression regex = RegexFunctions.binary(syntax, this); if (regex != null) return regex;
            Expression like = LikePatterns.binary(syntax, this); if (like != null) return like;
            Expression jsonBinary = JsonOperators.binary(syntax, this);
            if (jsonBinary != null) return jsonBinary;
            Expression rangeBinary = RangeFunctions.binary(syntax, this);
            if (rangeBinary != null) return rangeBinary;
            Expression arrayBinary = ArrayFunctions.binary(syntax, this);
            if (arrayBinary != null) return arrayBinary;
            if (op.equals("and") || op.equals("or")) {
                Expression left = filterContext && syntax.left() instanceof SqlParser.Binary b && b.op().equals("and") ? binary(b, true) : resolve(syntax.left(), SqlType.BOOLEAN);
                Expression right = filterContext && syntax.right() instanceof SqlParser.Binary b && b.op().equals("and") ? binary(b, true) : resolve(syntax.right(), SqlType.BOOLEAN);
                if (left.type != SqlType.BOOLEAN || right.type != SqlType.BOOLEAN) throw new SqlException("42804", "logical operator requires boolean");
                return new Expression(SqlType.BOOLEAN, meter -> {
                    meter.operation(); SqlValue a = left.run(meter);
                    if (op.equals("and") && (filterContext && a.isNull() || !a.isNull() && !a.asBoolean())) return a;
                    if (op.equals("or") && !a.isNull() && a.asBoolean()) return a;
                    SqlValue b = right.run(meter);
                    if (!b.isNull() && b.asBoolean() == op.equals("or")) return b;
                    if (a.isNull() || b.isNull()) return SqlValue.nullValue(SqlType.BOOLEAN);
                    return b;
                });
            }
            boolean compare = switch (op) { case "=", "<>", "!=", "<", ">", "<=", ">=", "is distinct from", "is not distinct from" -> true; default -> false; };
            Expression a, b;
            if (compare && (syntax.left() instanceof SqlParser.Parameter || syntax.right() instanceof SqlParser.Parameter)) {
                a = resolve(syntax.left(), null); b = resolve(syntax.right(), null);
                if (unknown(syntax.left())) a = resolve(syntax.left(), b.type);
                if (unknown(syntax.right())) b = resolve(syntax.right(), a.type);
            } else if (unknown(syntax.left()) || adaptableLiteral(syntax.left()) && !adaptableLiteral(syntax.right()) && !unknown(syntax.right())) {
                b = resolve(syntax.right(), null); a = resolve(syntax.left(), temporalStringContext(op, syntax.left(), b.type) ? SqlType.INTERVAL : unknown(syntax.left()) && b.type.kind() == SqlType.Kind.INTERVAL && op.equals("*") ? SqlType.DECIMAL : b.type);
            } else {
                a = resolve(syntax.left(), null);
                b = resolve(syntax.right(), temporalStringContext(op, syntax.right(), a.type) ? SqlType.INTERVAL : unknown(syntax.right()) && a.type.kind() == SqlType.Kind.INTERVAL && (op.equals("*") || op.equals("/")) ? SqlType.DECIMAL : unknown(syntax.right()) || adaptableLiteral(syntax.right()) ? a.type : null);
            }
            if (a.type.isComposite() && b.type.isComposite()) {
                a = resolve(CompositeExpressions.comparisonNulls(syntax.left(), b.type), null);
                b = resolve(CompositeExpressions.comparisonNulls(syntax.right(), a.type), null);
            }
            if (a.type.isFloat() && b.type.isFloat() && a.type != b.type) {
                if (a.type == SqlType.F32) a = widenFloat(a);
                if (b.type == SqlType.F32) b = widenFloat(b);
            }
            Expression left = a, right = b;
            if (compare) {
                compatible(left.type, right.type);
                String collationName = CollationDerivation.comparison(left.collation(), right.collation()).require();
                Collation collation = meter() == null ? Collation.C : meter().environment().collation(collationName);
                return new Expression(SqlType.BOOLEAN, meter -> {
                    meter.operation(); SqlValue x = left.run(meter), y = right.run(meter);
                    if (op.equals("is distinct from") || op.equals("is not distinct from")) {
                        boolean equal = x.isNull() && y.isNull() || !x.isNull() && !y.isNull() && compare(x, y, meter) == 0;
                        return SqlValue.ofBoolean(op.equals("is not distinct from") == equal);
                    }
                    if (x.isNull() || y.isNull()) return SqlValue.nullValue(SqlType.BOOLEAN);
                    if (x.type().isComposite()) return CompositeValues.sqlCompare(op, x, y, meter);
                    int order = List.of("=", "!=", "<>").contains(op) ? compare(x, y, meter) : CollationDerivation.compare(x, y, meter, collation);
                    return SqlValue.ofBoolean(switch (op) { case "=" -> order == 0; case "!=", "<>" -> order != 0; case "<" -> order < 0; case ">" -> order > 0; case "<=" -> order <= 0; case ">=" -> order >= 0; default -> false; });
                });
            }
            if (op.equals("||")) {
                boolean bytes = left.type.kind() == SqlType.Kind.BYTEA && right.type.kind() == SqlType.Kind.BYTEA;
                if (!bytes && (left.type.kind() != SqlType.Kind.TEXT || right.type.kind() != SqlType.Kind.TEXT)) throw new SqlException("42883", "concatenation requires matching text or bytea operands");
                SqlType result = bytes ? SqlType.BYTEA : SqlType.TEXT;
                return new Expression(result, meter -> {
                    meter.operation(); SqlValue x = left.run(meter); if (x.isNull()) return SqlValue.nullValue(result);
                    SqlValue y = right.run(meter); if (y.isNull()) return SqlValue.nullValue(result);
                    SqlValue value;
                    if (bytes) {
                        byte[] first = x.asBytes(), second = y.asBytes();
                        if (first.length > Integer.MAX_VALUE - second.length) throw new SqlException("54000", "concatenation result too large");
                        byte[] output = java.util.Arrays.copyOf(first, first.length + second.length); System.arraycopy(second, 0, output, first.length, second.length); value = SqlValue.ofBytes(output);
                    } else value = SqlValue.ofText(x.asText() + y.asText());
                    if (!left.constant || !right.constant) meter.scalarInput(MemoryAccount.payload(value));
                    return value;
                });
            }
            Expression temporal = TemporalOps.resolve(op, left, right);
            if (temporal != null) return temporal;
            if (List.of(SqlType.Kind.DATE, SqlType.Kind.TIMESTAMP, SqlType.Kind.TIMESTAMPTZ, SqlType.Kind.INTERVAL).contains(left.type.kind())
                || List.of(SqlType.Kind.DATE, SqlType.Kind.TIMESTAMP, SqlType.Kind.TIMESTAMPTZ, SqlType.Kind.INTERVAL).contains(right.type.kind())) throw SqlException.typeMismatch("unsupported temporal operands");
            if (!left.type.isNumeric() || !right.type.isNumeric()) throw SqlException.typeMismatch("arithmetic requires numeric operands");
            SqlType result = numeric(left.type, right.type);
            return new Expression(result, meter -> {
                meter.operation(); SqlValue x = left.run(meter), y = right.run(meter);
                if (x.isNull() || y.isNull()) return SqlValue.nullValue(result);
                if (result.isInteger()) return integerOperation(op, x.asInteger(), y.asInteger(), result);
                if (result.isFloat()) return floatOperation(op, x.asFloat(), y.asFloat(), result);
                Decimal dx = decimal(x), dy = decimal(y);
                if ((op.equals("/") || op.equals("%")) && dy.isZero()) throw new SqlException("22012", "division by zero");
                decimalWork(op, dx, dy, meter);
                return SqlValue.ofDecimal(switch (op) {
                    case "+" -> dx.add(dy); case "-" -> dx.subtract(dy); case "*" -> dx.multiply(dy);
                    case "/" -> dx.divide(dy); case "%" -> dx.modulo(dy);
                    default -> throw new SqlException("0A000", "numeric operator not implemented: " + op);
                });
            });
        }

        private static boolean temporalStringContext(String op, SqlParser.Expr syntax, SqlType sibling) {
            return op.equals("+") && (sibling.kind() == SqlType.Kind.TIMESTAMP || sibling.kind() == SqlType.Kind.TIMESTAMPTZ)
                && syntax instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.STRING;
        }

        private Expression nullIf(SqlParser.Extrema syntax) {
            SqlParser.Expr first = syntax.args().getFirst(), second = syntax.args().get(1);
            Expression a, b;
            if (unknown(first) || adaptableLiteral(first)) { b = resolve(second, null); a = resolve(first, b.type == SqlType.UNKNOWN ? SqlType.TEXT : b.type); }
            else { a = resolve(first, null); b = resolve(second, unknown(second) || adaptableLiteral(second) ? a.type : null); }
            compatible(a.type, b.type); requireComparable(a.type); requireComparable(b.type);
            CollationDerivation.comparison(a.collation(), b.collation()).require();
            SqlType result = a.type.isInteger() && b.type.kind() == SqlType.Kind.DECIMAL ? SqlType.DECIMAL : a.type;
            Expression left = a, right = b;
            return new Expression(result, meter -> {
                meter.operation(); SqlValue x = left.run(meter), y = right.run(meter);
                return !x.isNull() && !y.isNull() && compare(x, y, meter) == 0 ? SqlValue.nullValue(result) : SqlCasts.cast(x, result);
            });
        }
        private Expression extrema(SqlParser.Extrema syntax) {
            if (syntax.kind().equals("nullif")) return nullIf(syntax);
            List<Expression> args = new ArrayList<>(); SqlType type = SqlType.UNKNOWN;
            for (SqlParser.Expr arg : syntax.args()) {
                Expression value = resolve(arg, null); args.add(value); type = common(type, value.type());
            }
            SqlType result = type == SqlType.UNKNOWN ? SqlType.TEXT : type;
            requireComparable(result);
            CollationDerivation derivation = CollationDerivation.NONE; for (Expression arg : args) derivation = CollationDerivation.merge(derivation, arg.collation());
            String collationName = derivation.require(); Collation collation = meter() == null ? Collation.C : meter().environment().collation(collationName);
            return new Expression(result, meter -> {
                meter.operation(); SqlValue winner = SqlValue.nullValue(result);
                for (Expression arg : args) {
                    SqlValue value = arg.run(meter); if (value.isNull()) continue;
                    value = SqlCasts.cast(value, result);
                    if (winner.isNull()) winner = value;
                    else { int order = CollationDerivation.compare(value, winner, meter, collation); if (syntax.kind().equals("greatest") ? order > 0 : order < 0) winner = value; }
                }
                return winner;
            });
        }
        private Expression conditional(SqlParser.Conditional syntax, SqlType context) {
            Expression input = syntax.input() == null ? null : resolve(syntax.input(), null);
            List<Expression> conditions = new ArrayList<>(), results = new ArrayList<>();
            SqlType resultType = context;
            for (int i = 0; i < syntax.conditions().size(); i++) {
                Expression condition = resolve(syntax.conditions().get(i), input == null ? SqlType.BOOLEAN : input.type);
                if (input == null && condition.type != SqlType.BOOLEAN) throw new SqlException("42804", "CASE WHEN requires boolean");
                if (input != null) CollationDerivation.comparison(input.collation(), condition.collation()).require();
                if (input != null && input.type == SqlType.F64 && condition.type == SqlType.F32) condition = widenFloat(condition);
                conditions.add(condition);
                Expression result = resolve(syntax.results().get(i), resultType);
                if (!unknown(syntax.results().get(i))) resultType = resultType == null ? result.type : common(resultType, result.type);
                results.add(result);
            }
            Expression otherwise = resolve(syntax.otherwise(), resultType);
            if (!unknown(syntax.otherwise())) resultType = resultType == null ? otherwise.type : common(resultType, otherwise.type);
            SqlType selected = resultType == null ? SqlType.TEXT : resultType;
            if (unknown(syntax.otherwise())) selected = switch (selected.kind()) {
                case TEXT -> SqlType.TEXT; case DECIMAL -> SqlType.DECIMAL; case TIMESTAMP -> SqlType.TIMESTAMP; case TIMESTAMPTZ -> SqlType.TIMESTAMPTZ; case INTERVAL -> SqlType.INTERVAL; default -> selected;
            };
            SqlType type = selected;
            return new Expression(type, meter -> {
                meter.operation();
                SqlValue subject = input == null ? null : input.run(meter);
                for (int i = 0; i < conditions.size(); i++) {
                    SqlValue condition = conditions.get(i).run(meter);
                    if (input != null) { meter.operation(); if (input.type == SqlType.F32 && conditions.get(i).type == SqlType.F64) meter.operation(); }
                    boolean matches = input == null ? !condition.isNull() && condition.asBoolean() : !condition.isNull() && !subject.isNull() && compare(subject, condition, meter) == 0;
                    if (matches) return SqlCasts.cast(results.get(i).run(meter), type);
                }
                return SqlCasts.cast(otherwise.run(meter), type);
            });
        }

        private Expression call(SqlParser.Call syntax) {
            Expression host = HostFunctions.resolve(syntax, this); if (host != null) { HostFunctions.Call node = (HostFunctions.Call)host.eval(); node.frame = frame; hostCalls.computeIfAbsent(syntax, ignored -> new ArrayList<>()).add(node); return host; }
            Expression setting = SessionVariables.resolve(syntax, this); if (setting != null) return setting;
            Expression builder = TextBuilders.resolve(syntax, this); if (builder != null) return builder;
            if (environment != null && environment.sequences() != null) {
                Expression sequence = environment.sequences().resolve(syntax, this, environment);
                if (sequence != null) return sequence;
            }
            return normalizedCall(NamedFunctions.normalize(syntax));
        }
        private Expression normalizedCall(SqlParser.Call syntax) {
            Expression formatting = Formatting.resolve(syntax, this); if (formatting != null) return formatting;
            Expression regex = RegexFunctions.resolve(syntax, this); if (regex != null) return regex;
            Expression like = LikePatterns.resolve(syntax, this); if (like != null) return like;
            Expression jsonFunction = JsonFunctions.resolve(syntax, this);
            if (jsonFunction != null) return jsonFunction;
            Expression rangeFunction = RangeFunctions.resolve(syntax, this);
            if (rangeFunction != null) return rangeFunction;
            Expression arrayFunction = ArrayFunctions.resolve(syntax, this);
            if (arrayFunction != null) return arrayFunction;
            if (syntax.name().equals("extract") && syntax.args().size() == 2 && syntax.args().getFirst() instanceof SqlParser.Literal field && field.kind() == Lexer.Kind.STRING) {
                Expression source = resolve(syntax.args().get(1), null);
                TemporalFunctions.validateExtract(field.text(), source.type);
                return new Expression(SqlType.DECIMAL, meter -> {
                    meter.operation(); SqlValue value = source.run(meter);
                    return value.isNull() ? SqlValue.nullValue(SqlType.DECIMAL) : TemporalFunctions.extract(field.text(), value, false, meter);
                });
            }
            if (AggregatePlan.aggregateName(syntax.name())) throw new SqlException("42803", "aggregate is not allowed in this context");
            if (syntax.distinct() || syntax.star() || !syntax.order().isEmpty() || syntax.filter() != null) {
                throw new SqlException("42809", "aggregate decoration on scalar function");
            }
            if (syntax.name().equals("coalesce")) {
                if (syntax.args().isEmpty()) throw new SqlException("42601", "COALESCE requires arguments");
                List<Expression> args = new ArrayList<>();
                SqlType type = null;
                for (SqlParser.Expr arg : syntax.args()) {
                    Expression resolved = resolve(arg, type); args.add(resolved);
                    if (!unknown(arg)) type = type == null ? resolved.type : common(type, resolved.type);
                }
                SqlType resultType = type == null ? SqlType.TEXT : type;
                return new Expression(resultType, meter -> {
                    meter.operation();
                    for (Expression arg : args) { SqlValue value = arg.run(meter); if (!value.isNull()) return SqlCasts.cast(value, resultType); }
                    return SqlValue.nullValue(resultType);
                });
            }
            List<Expression> arguments = new ArrayList<>();
            for (SqlParser.Expr argument : syntax.args()) arguments.add(resolve(argument, null));
            List<String> origins = syntax.args().stream().map(argument -> {
                if (argument instanceof SqlParser.Literal literal) return switch (literal.kind()) {
                    case INTEGER -> "integer_literal"; case DECIMAL -> "decimal_literal"; case STRING -> "string_literal";
                    case WORD -> literal.text().equals("null") ? "unknown" : "value"; default -> "value";
                };
                return "value";
            }).toList();
            Expression builtin = ScalarFunctions.resolve(syntax.name(), arguments, origins);
            if (builtin != null) return builtin;
            throw new SqlException("42883", "function is not implemented: " + syntax.name());
        }
    }

    static SqlType common(SqlType a, SqlType b) {
        if (a == SqlType.UNKNOWN) return b;
        if (b == SqlType.UNKNOWN) return a;
        if (a.equals(b)) return a;
        if (a.kind() == SqlType.Kind.RANGE || b.kind() == SqlType.Kind.RANGE) throw SqlException.typeMismatch("different range types");
        if (a.isComposite() || b.isComposite()) return CompositeExpressions.common(a, b);
        if (a.kind() == SqlType.Kind.ARRAY && b.kind() == SqlType.Kind.ARRAY) { compatible(a, b); return SqlType.array(common(a.element(), b.element())); }
        if (a.kind() == b.kind()) return switch (a.kind()) {
            case TEXT -> SqlType.TEXT; case DECIMAL -> SqlType.DECIMAL;
            case TIMESTAMP -> SqlType.TIMESTAMP; case TIMESTAMPTZ -> SqlType.TIMESTAMPTZ;
            case INTERVAL -> SqlType.INTERVAL; default -> a;
        };
        if (!(a.isNumeric() && b.isNumeric()) || a.isFloat() != b.isFloat()) throw SqlException.typeMismatch("types must be compatible");
        return numeric(a, b);
    }
    static void compatible(SqlType a, SqlType b) {
        if (a == SqlType.JSON || b == SqlType.JSON || a == SqlType.JSONPATH || b == SqlType.JSONPATH) throw new SqlException("42883", "type has no comparison operator");
        if (a == SqlType.UNKNOWN || b == SqlType.UNKNOWN) return;
        if (a.kind() == SqlType.Kind.RANGE || b.kind() == SqlType.Kind.RANGE) { if (!a.equals(b)) throw SqlException.typeMismatch("different range types"); return; }
        if (a.isComposite() || b.isComposite()) { CompositeExpressions.compatible(a, b); return; }
        if (a.kind() == SqlType.Kind.ARRAY && b.kind() == SqlType.Kind.ARRAY) {
            if (!a.element().equals(b.element()) && !(a.element().isInteger() && b.element().isInteger())) throw SqlException.typeMismatch("incompatible array element types");
            return;
        }
        if (a.kind() == b.kind()) return;
        if (a.isInteger() && b.isInteger() || (a.isInteger() || a.kind() == SqlType.Kind.DECIMAL) && (b.isInteger() || b.kind() == SqlType.Kind.DECIMAL) || a.isFloat() && b.isFloat()) return;
        throw SqlException.typeMismatch("cannot compare " + a + " and " + b);
    }
    private static SqlType numeric(SqlType a, SqlType b) {
        if (a.isInteger() && b.isInteger()) return a == SqlType.I64 || b == SqlType.I64 ? SqlType.I64 : a == SqlType.I32 || b == SqlType.I32 ? SqlType.I32 : SqlType.I16;
        if ((a.isInteger() || a.kind() == SqlType.Kind.DECIMAL) && (b.isInteger() || b.kind() == SqlType.Kind.DECIMAL)) return SqlType.DECIMAL;
        if (a.isFloat() && b.isFloat()) return a == SqlType.F64 || b == SqlType.F64 ? SqlType.F64 : SqlType.F32;
        throw SqlException.typeMismatch("arithmetic types must be compatible: " + a + " and " + b);
    }
    private static SqlValue integerOperation(String op, long a, long b, SqlType type) {
        try {
            long value = switch (op) {
                case "+" -> Math.addExact(a, b);
                case "-" -> Math.subtractExact(a, b);
                case "*" -> Math.multiplyExact(a, b);
                case "/" -> {
                    if (b == 0) throw new SqlException("22012", "division by zero");
                    if (a == Long.MIN_VALUE && b == -1) throw SqlException.numericOverflow();
                    yield a / b;
                }
                case "%" -> { if (b == 0) throw new SqlException("22012", "division by zero"); yield a % b; }
                default -> throw new SqlException("0A000", "integer operator not implemented: " + op);
            };
            return SqlValue.ofInteger(type, value);
        } catch (ArithmeticException error) { throw SqlException.numericOverflow(); }
    }
    private static Decimal decimal(SqlValue value) { return value.type().isInteger() ? Decimal.of(value.asInteger()) : value.asDecimal(); }
    private static SqlValue floatOperation(String op, double a, double b, SqlType type) {
        if ((op.equals("/") || op.equals("%")) && b == 0 && !Double.isNaN(a)) throw new SqlException("22012", "division by zero");
        double result = switch (op) {
            case "+" -> a + b; case "-" -> a - b; case "*" -> a * b; case "/" -> a / b; case "%" -> a % b;
            default -> throw new SqlException("42883", "float operator does not exist: " + op);
        };
        if (type == SqlType.F32) result = (float) result;
        if (Double.isInfinite(result) && Double.isFinite(a) && Double.isFinite(b)) throw SqlException.numericOverflow();
        if (result == 0 && a != 0 && b != 0 && (op.equals("*") || op.equals("/") && Double.isFinite(b))) throw SqlException.numericOverflow();
        return SqlValue.ofFloat(type, result);
    }
    static void decimalWork(String op, Decimal a, Decimal b, CostMeter meter) {
        int scale = Math.max(a.scale(), b.scale());
        long groups;
        if (op.equals("/")) {
            int shift = a.divisionScale(b) + b.scale() - a.scale();
            groups = Math.max(1, (a.precision() + Math.max(0, shift) + 3L) / 4)
                * Math.max(1, (b.precision() + Math.max(0, -shift) + 3L) / 4);
        } else if (op.equals("*")) groups = Math.max(1, (a.precision() + 3L) / 4) * Math.max(1, (b.precision() + 3L) / 4);
        else if (op.equals("%")) groups = a.groupsAtScale(scale) * b.groupsAtScale(scale);
        else groups = Math.max(a.groupsAtScale(scale), b.groupsAtScale(scale));
        meter.charge(Math.max(0, groups - 1) * Costs.DECIMAL_WORK); meter.guard();
    }
    static void requireComparable(SqlType type) {
        if (type == SqlType.JSON || type == SqlType.JSONPATH) throw new SqlException("42883", "type has no comparison operator");
        if (type.kind() == SqlType.Kind.ARRAY) requireComparable(type.element());
        if (type.isComposite()) for (SqlType.Field field : type.fields()) requireComparable(field.type());
    }
    private static Expression widenFloat(Expression source) {
        return new Expression(SqlType.F64, meter -> { meter.operation(); return SqlCasts.cast(source.run(meter), SqlType.F64); }, source.constant());
    }
    static int compare(SqlValue a, SqlValue b, CostMeter meter) {
        if (a.type() == SqlType.JSON || a.type() == SqlType.JSONPATH) throw new SqlException("42883", "type has no comparison operator");
        if (a.type() == SqlType.JSONB) return a.asJsonb().compare(b.asJsonb());
        if (a.type().kind() == SqlType.Kind.RANGE) return a.asRange().compare(b.asRange());
        if (a.type().isComposite()) return CompositeValues.compare(a, b, meter);
        if (a.type().kind() == SqlType.Kind.ARRAY) return a.asArray().compare(b.asArray(), meter);
        if (a.type().isInteger() && b.type().isInteger()) return Long.compare(a.asInteger(), b.asInteger());
        if (a.type().kind() == SqlType.Kind.DECIMAL || b.type().kind() == SqlType.Kind.DECIMAL) {
            Decimal x = decimal(a), y = decimal(b); if (meter != null) decimalWork("compare", x, y, meter); return x.compareTo(y);
        }
        if (a.type().isFloat() && b.type().isFloat()) {
            double x = a.asFloat(), y = b.asFloat();
            return x == y ? 0 : Double.compare(x, y);
        }
        if (a.type() == SqlType.BOOLEAN) return Boolean.compare(a.asBoolean(), b.asBoolean());
        if (a.type() == SqlType.UUID) return a.asUuid().compareTo(b.asUuid());
        if (a.type() == SqlType.DATE) return Integer.compare(a.asDate(), b.asDate());
        if (a.type() == SqlType.INTERVAL) return a.asInterval().compareTo(b.asInterval());
        if (a.type().kind() == SqlType.Kind.TIMESTAMP || a.type().kind() == SqlType.Kind.TIMESTAMPTZ) return Long.compare(a.asTimestamp(), b.asTimestamp());
        if (a.type().kind() == SqlType.Kind.TEXT) {
            String x = a.asText(), y = b.asText();
            if (meter != null) { meter.charge(Math.max(0, Math.min(SqlText.length(x), SqlText.length(y)) - 1L) * Costs.VARLEN_COMPARE); meter.guard(); }
            return SqlText.compare(x, y);
        }
        if (a.type().kind() == SqlType.Kind.BYTEA) {
            byte[] x = a.asBytes(), y = b.asBytes();
            if (meter != null) { meter.charge(Math.max(0, Math.min(x.length, y.length) - 1L) * Costs.VARLEN_COMPARE); meter.guard(); }
            return java.util.Arrays.compareUnsigned(x, y);
        }
        throw new SqlException("0A000", "comparison is not implemented for " + a.type());
    }
    private static boolean constantSyntax(SqlParser.Expr syntax, java.util.IdentityHashMap<SqlParser.Expr, Boolean> cache) {
        Boolean known = cache.get(syntax);
        if (known != null) return known;
        boolean result = constantSyntaxInner(syntax, cache);
        cache.put(syntax, result);
        return result;
    }
    private static boolean constantSyntaxInner(SqlParser.Expr syntax, java.util.IdentityHashMap<SqlParser.Expr, Boolean> cache) {
        if (syntax instanceof SqlParser.Extrema extrema) return extrema.args().stream().allMatch(item -> constantSyntax(item, cache));
        if (syntax instanceof SqlParser.JsonSql json) return json.children().stream().allMatch(item -> constantSyntax(item, cache));
        if (syntax instanceof SqlParser.StoredExpression stored) return constantSyntax(stored.expression(), cache);
        if (syntax instanceof SqlParser.Literal || syntax instanceof SqlParser.Parameter || syntax instanceof SqlParser.StoredValue) return true;
        if (syntax instanceof SqlParser.Cast cast) return !(cast.target().name().equals("date") && cast.value() instanceof SqlParser.Literal value && value.kind() == Lexer.Kind.STRING && Temporal.clockDate(value.text())) && constantSyntax(cast.value(), cache);
        if (syntax instanceof SqlParser.RowConstructor row) return row.values().stream().allMatch(item -> constantSyntax(item, cache));
        if (syntax instanceof SqlParser.Field field) return constantSyntax(field.base(), cache);
        if (syntax instanceof SqlParser.ArrayConstructor array) return array.query() == null && array.values().stream().allMatch(item -> constantSyntax(item, cache));
        if (syntax instanceof SqlParser.Access access) return constantSyntax(access.base(), cache) && access.subscripts().stream().allMatch(subscript -> (subscript.lower() == null || constantSyntax(subscript.lower(), cache)) && (subscript.upper() == null || constantSyntax(subscript.upper(), cache)));
        if (syntax instanceof SqlParser.Quantified quantified) return quantified.query() == null && constantSyntax(quantified.subject(), cache) && constantSyntax(quantified.array(), cache);
        if (syntax instanceof SqlParser.Unary unary) return constantSyntax(unary.value(), cache);
        if (syntax instanceof SqlParser.Binary binary) return constantSyntax(binary.left(), cache) && constantSyntax(binary.right(), cache);
        if (syntax instanceof SqlParser.Between between) return constantSyntax(between.subject(), cache) && constantSyntax(between.lower(), cache) && constantSyntax(between.upper(), cache);
        if (syntax instanceof SqlParser.In in) return in.query() == null && constantSyntax(in.subject(), cache) && in.values().stream().allMatch(item -> constantSyntax(item, cache));
        if (syntax instanceof SqlParser.Call call) return call.args().stream().allMatch(item -> constantSyntax(item, cache))
            && !List.of("now", "clock_timestamp", "current_timestamp", "current_date", "localtimestamp", "uuidv4", "gen_random_uuid", "uuidv7", "nextval", "setval", "currval", "lastval").contains(call.name());
        if (syntax instanceof SqlParser.Conditional branch) return (branch.input() == null || constantSyntax(branch.input(), cache))
            && branch.conditions().stream().allMatch(item -> constantSyntax(item, cache))
            && branch.results().stream().allMatch(item -> constantSyntax(item, cache)) && constantSyntax(branch.otherwise(), cache);
        return false;
    }
}
