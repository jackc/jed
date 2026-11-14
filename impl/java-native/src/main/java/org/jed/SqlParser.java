package org.jed;

import java.util.ArrayList;
import java.util.List;
import java.util.ArrayDeque;
import java.util.Collections;
import java.util.IdentityHashMap;
import java.util.Set;
import java.util.Map;
import java.util.HashMap;

/** Syntax only: no value evaluation or host-dependent coercion during parsing. */
final class SqlParser {
    sealed interface Statement permits Analyze, Merge, Reindex, CreateSequence, AlterSequence, DropSequence, Select, Values, CreateTable, DropTable, CreateView, DropView, CreateType, DropType, Insert, Update, Delete, Transaction, CreateIndex, DropIndex, AlterTable, Explain, SetOperation, QueryTail, With {}
    record Analyze(Name table, List<String> columns) implements Statement { Analyze { columns = List.copyOf(columns); } }
    sealed interface Expr permits Extrema, JsonSql, Window, ColumnSlot, Literal, Name, Parameter, Unary, Binary, Cast, Call, Conditional, Star, Default, StoredValue, StoredExpression, UnparsedExpression, Subquery, Exists, In, Between, Quantified, ArrayConstructor, RowConstructor, Access, Field, Collate, NamedArgument {}
    record SetOperation(Statement left, Statement right, String kind, boolean all) implements Statement {}
    record QueryTail(Statement query, List<Order> order, Expr limit, Expr offset) implements Statement { QueryTail { order = List.copyOf(order); } }
    record With(boolean recursive, List<Cte> ctes, Statement body) implements Statement { With { ctes = List.copyOf(ctes); } }
    record Cte(String name, List<String> columns, String materialization, Statement body, Search search, Cycle cycle) { Cte { columns = List.copyOf(columns); } }
    record Search(boolean breadth, List<String> columns, String output) { Search { columns = List.copyOf(columns); } }
    record Cycle(List<String> columns, String mark, Expr value, Expr otherwise, String path) { Cycle { columns = List.copyOf(columns); } }
    record Reindex(String target, Name name, boolean concurrently) implements Statement {}
    record Explain(Statement inner, boolean analyze, boolean verbose, boolean costs, boolean lane) implements Statement {}
    record Subquery(Statement query) implements Expr {}
    record Exists(Statement query) implements Expr {}
    record In(Expr subject, List<Expr> values, Statement query, boolean negated) implements Expr { In { values = List.copyOf(values); } }
    record Between(Expr subject, Expr lower, Expr upper, boolean negated) implements Expr {}
    record Quantified(String operator, Expr subject, String quantifier, Expr array, Statement query) implements Expr {}
    record ArrayConstructor(List<Expr> values, Statement query) implements Expr { ArrayConstructor { values = List.copyOf(values); } }
    record RowConstructor(List<Expr> values) implements Expr { RowConstructor { values = List.copyOf(values); } }
    record Subscript(Expr lower, Expr upper, boolean slice) {}
    record Access(Expr base, List<Subscript> subscripts) implements Expr { Access { subscripts = List.copyOf(subscripts); } }
    record Field(Expr base, String name) implements Expr {} // name null means .*
    record Collate(Expr value, String name) implements Expr {}
    record StoredValue(SqlValue value) implements Expr {}
    record UnparsedExpression(String text, String detail) implements Expr {
        UnparsedExpression(String text) { this(text, "stored expression"); }
    }
    record StoredExpression(String text, Expr expression, int position) implements Expr {
        StoredExpression(String text, Expr expression) { this(text, expression, -1); }
    }
    record ColumnSlot(int index) implements Expr {}
    record Star(List<String> qualifier) implements Expr { Star { qualifier = List.copyOf(qualifier); } }
    record Default() implements Expr {}
    record Literal(Lexer.Kind kind, String text) implements Expr {}
    record Name(List<String> parts) implements Expr { Name { parts = List.copyOf(parts); } }
    record Parameter(int index) implements Expr {}
    record NamedArgument(String name, Expr value) implements Expr {}
    record Unary(String op, Expr value) implements Expr {}
    record Binary(String op, Expr left, Expr right) implements Expr {}
    record Cast(Expr value, TypeName target) implements Expr {}
    record Call(String name, List<Expr> args, boolean distinct, boolean star, List<Order> order, Expr filter, boolean withinGroup) implements Expr {
        Call(String name, List<Expr> args, boolean distinct, boolean star, List<Order> order, Expr filter) { this(name, args, distinct, star, order, filter, false); }
        Call { args = List.copyOf(args); order = List.copyOf(order); }
        Call(String name, List<Expr> args) { this(name, args, false, false, List.of(), null); }
    }
    record JsonPassing(String name, Expr value, boolean format, String encoding) { JsonPassing(String name, Expr value, boolean format) { this(name, value, format, null); } }
    record JsonBehavior(String kind, Expr value) {}
    record JsonSql(String kind, Expr context, String path, List<JsonPassing> passing, TypeName returning,
                   String wrapper, boolean omitQuotes, JsonBehavior onEmpty, JsonBehavior onError,
                   boolean unique, boolean contextFormat) implements Expr {
        JsonSql { passing = List.copyOf(passing); }
        List<Expr> children() {
            ArrayList<Expr> result = new ArrayList<>(); result.add(context);
            for (JsonPassing arg : passing) result.add(arg.value());
            if (onEmpty != null && onEmpty.value() != null) result.add(onEmpty.value());
            if (onError != null && onError.value() != null) result.add(onError.value());
            return List.copyOf(result);
        }
        JsonSql map(java.util.function.Function<Expr, Expr> rewrite) {
            return new JsonSql(kind, rewrite.apply(context), path, passing.stream().map(arg -> new JsonPassing(arg.name(), rewrite.apply(arg.value()), arg.format(), arg.encoding())).toList(), returning, wrapper, omitQuotes,
                onEmpty == null ? null : new JsonBehavior(onEmpty.kind(), onEmpty.value() == null ? null : rewrite.apply(onEmpty.value())),
                onError == null ? null : new JsonBehavior(onError.kind(), onError.value() == null ? null : rewrite.apply(onError.value())), unique, contextFormat);
        }
    }
    record Window(Call function, WindowSpec spec, String reference) implements Expr {}
    record WindowSpec(String base, List<Expr> partition, List<Order> order, WindowFrame frame) {
        WindowSpec { partition = List.copyOf(partition); order = List.copyOf(order); }
    }
    record WindowFrame(String mode, FrameBound start, FrameBound end, String exclude) {}
    record FrameBound(String kind, Expr offset) {}
    record NamedWindow(String name, WindowSpec spec) {}
    record Conditional(Expr input, List<Expr> conditions, List<Expr> results, Expr otherwise) implements Expr {
        Conditional { conditions = List.copyOf(conditions); results = List.copyOf(results); }
    }
    record Extrema(String kind, List<Expr> args) implements Expr {
        Extrema { args = List.copyOf(args); }
        Extrema map(java.util.function.Function<Expr, Expr> rewrite) { return new Extrema(kind, args.stream().map(rewrite).toList()); }
    }
    record TypeName(String name, int precision, int scale, int dimensions, int intervalFields) {
        TypeName(String name, int precision, int scale, int dimensions) { this(name, precision, scale, dimensions, 0); }
    }
    record Item(Expr expression, String alias) {}
    record Order(Expr expression, boolean descending, Boolean nullsFirst) {}
    sealed interface From permits Table, Join, Derived, TableFunction, JsonTable, Grouped {}
    record Grouped(From source, String alias, List<String> columns) implements From { Grouped { columns = List.copyOf(columns); } }
    record JsonTable(Expr context, String path, String pathName, List<JsonPassing> passing, List<JsonTableColumn> columns, String alias, List<String> aliases) implements From {
        JsonTable { passing = List.copyOf(passing); columns = List.copyOf(columns); aliases = List.copyOf(aliases); }
    }
    record JsonTableColumn(String name, TypeName type, String path, String pathName, String kind, boolean format,
                           String wrapper, boolean omitQuotes, JsonBehavior onEmpty, JsonBehavior onError, List<JsonTableColumn> children) {
        JsonTableColumn { children = List.copyOf(children); }
    }
    record TableFunction(Call call, String alias, List<String> columns, List<Column> definitions, boolean lateral) implements From {
        TableFunction { columns = List.copyOf(columns); definitions = List.copyOf(definitions); }
    }
    record Table(Name name, String alias, List<String> columns) implements From { Table { columns = List.copyOf(columns); } }
    record Join(From left, From right, String kind, Expr on, List<String> using, boolean natural) implements From {
        Join { using = List.copyOf(using); }
    }
    record Derived(Statement query, String alias, List<String> columns, boolean lateral) implements From {
        Derived { columns = List.copyOf(columns); }
        Derived(Statement query, String alias, List<String> columns) { this(query, alias, columns, false); }
    }
    record Select(List<Item> items, Expr where, List<Order> order, Expr limit, Expr offset, boolean distinct,
                  List<From> from, List<Expr> group, Expr having, List<Expr> distinctOn, List<NamedWindow> windows) implements Statement {
        Select { items = List.copyOf(items); order = List.copyOf(order); from = List.copyOf(from); group = List.copyOf(group); distinctOn = List.copyOf(distinctOn); windows = List.copyOf(windows); }
        Select(List<Item> items, Expr where, List<Order> order, Expr limit, Expr offset, boolean distinct, List<From> from, List<Expr> group, Expr having, List<Expr> distinctOn) {
            this(items, where, order, limit, offset, distinct, from, group, having, distinctOn, List.of());
        }
        Select(List<Item> items, Expr where, List<Order> order, Expr limit, Expr offset, boolean distinct) {
            this(items, where, order, limit, offset, distinct, List.of(), List.of(), null, List.of());
        }
    }
    record Values(List<List<Expr>> rows, List<Order> order, Expr limit, Expr offset) implements Statement {
        Values { rows = rows.stream().map(List::copyOf).toList(); order = List.copyOf(order); }
    }
    record ExcludeElement(Expr expression, String operator) {}
    record Constraint(String kind, String name, List<String> columns, Expr expression,
                      Name reference, List<String> referenceColumns, String onDelete, String onUpdate, boolean matchFull, String method, List<ExcludeElement> excludeElements) {
        Constraint(String kind, String name, List<String> columns, Expr expression, Name reference, List<String> referenceColumns, String onDelete, String onUpdate, boolean matchFull) { this(kind, name, columns, expression, reference, referenceColumns, onDelete, onUpdate, matchFull, null, List.of()); }
        Constraint { columns = List.copyOf(columns); referenceColumns = List.copyOf(referenceColumns); excludeElements = List.copyOf(excludeElements); }
        Constraint(String kind, String name, List<String> columns, Expr expression, Name reference, List<String> referenceColumns, String onDelete, String onUpdate) { this(kind, name, columns, expression, reference, referenceColumns, onDelete, onUpdate, false); }
    }
    record Identity(boolean always, String sequenceName, List<SequenceOption> options) { Identity { options = List.copyOf(options); } }
    record Column(String name, TypeName type, List<Constraint> constraints, Identity identity, String collation) {
        Column(String name, TypeName type, List<Constraint> constraints, Identity identity) { this(name, type, constraints, identity, null); }
        Column { constraints = List.copyOf(constraints); }
        Column(String name, TypeName type, List<Constraint> constraints) { this(name, type, constraints, null); }
    }
    record CreateTable(Name name, boolean temporary, boolean ifNotExists, List<Column> columns,
                       List<Constraint> constraints, Statement asQuery, List<String> columnNames,
                       String onCommit, boolean withData) implements Statement {
        CreateTable { columns = List.copyOf(columns); constraints = List.copyOf(constraints); columnNames = List.copyOf(columnNames); }
    }
    record CreateView(Name name, boolean temporary, boolean replace, boolean ifNotExists, List<String> columns,
                      Statement query, String definition, int checkOption, List<ViewOption> options) implements Statement {
        CreateView { columns = List.copyOf(columns); options = List.copyOf(options); }
    }
    record ViewOption(String name, String value) {}
    record DropView(List<Name> names, boolean ifExists, boolean cascade) implements Statement { DropView { names = List.copyOf(names); } }
    record SequenceOption(String name, Long value, TypeName type) {}
    record CreateSequence(Name name, boolean ifNotExists, List<SequenceOption> options) implements Statement { CreateSequence { options = List.copyOf(options); } }
    record AlterSequence(Name name, boolean ifExists, String newName, List<SequenceOption> options) implements Statement { AlterSequence { options = List.copyOf(options); } }
    record DropSequence(List<Name> names, boolean ifExists, boolean cascade) implements Statement { DropSequence { names = List.copyOf(names); } }
    record CreateType(Name name, List<Column> fields) implements Statement { CreateType { fields = List.copyOf(fields); } }
    record DropType(List<Name> names, boolean ifExists, boolean cascade) implements Statement { DropType { names = List.copyOf(names); } }
    record DropTable(List<Name> names, boolean ifExists, boolean cascade) implements Statement { DropTable { names = List.copyOf(names); } }
    record Insert(Name table, List<String> columns, List<List<Expr>> values, Statement query,
                  boolean defaultValues, List<Item> returning, String oldAlias, String newAlias, String overriding, String alias, Conflict conflict) implements Statement {
        Insert { columns = List.copyOf(columns); values = values.stream().map(List::copyOf).toList(); returning = List.copyOf(returning); }
        Insert(Name table, List<String> columns, List<List<Expr>> values, Statement query, boolean defaultValues, List<Item> returning, String oldAlias, String newAlias, String overriding) { this(table, columns, values, query, defaultValues, returning, oldAlias, newAlias, overriding, null, null); }
        Insert(Name table, List<String> columns, List<List<Expr>> values, Statement query, boolean defaultValues, List<Item> returning, String oldAlias, String newAlias) { this(table, columns, values, query, defaultValues, returning, oldAlias, newAlias, null); }
        Insert(Name table, List<String> columns, List<List<Expr>> values, Statement query, boolean defaultValues, List<Item> returning) { this(table, columns, values, query, defaultValues, returning, "old", "new"); }
    }
    record ConflictElement(String column, String collation, String opclass, boolean ordered) {}
    record Conflict(List<ConflictElement> columns, String constraint, Expr predicate, boolean nothing, List<Assignment> assignments, Expr where) {
        Conflict { columns = List.copyOf(columns); assignments = List.copyOf(assignments); }
    }
    record Assignment(String column, Expr expression) {}
    record Update(Name table, String alias, List<Assignment> assignments, List<From> from,
                  Expr where, List<Item> returning, String oldAlias, String newAlias) implements Statement {
        Update { assignments = List.copyOf(assignments); from = List.copyOf(from); returning = List.copyOf(returning); }
        Update(Name table, String alias, List<Assignment> assignments, List<From> from, Expr where, List<Item> returning) { this(table, alias, assignments, from, where, returning, "old", "new"); }
    }
    record Delete(Name table, String alias, List<From> using, Expr where, List<Item> returning, String oldAlias, String newAlias) implements Statement {
        Delete { using = List.copyOf(using); returning = List.copyOf(returning); }
        Delete(Name table, String alias, List<From> using, Expr where, List<Item> returning) { this(table, alias, using, where, returning, "old", "new"); }
    }
    record Merge(Name table, String alias, From source, Expr on, List<MergeWhen> clauses,
                 List<Item> returning, String oldAlias, String newAlias, Expr targetWhere) implements Statement {
        Merge(Name table, String alias, From source, Expr on, List<MergeWhen> clauses, List<Item> returning, String oldAlias, String newAlias) { this(table, alias, source, on, clauses, returning, oldAlias, newAlias, null); }
        Merge { clauses = List.copyOf(clauses); returning = List.copyOf(returning); }
    }
    record MergeWhen(String kind, Expr condition, String action, List<Assignment> assignments,
                     List<String> columns, List<Expr> values, boolean defaultValues, String overriding) {
        MergeWhen { assignments = List.copyOf(assignments); columns = List.copyOf(columns); values = List.copyOf(values); }
    }
    record Transaction(String action, String name, boolean readOnly) implements Statement {}
    record IndexKey(Expr expression, boolean descending, Boolean nullsFirst, String opclass, boolean explicitDirection) {
        IndexKey(Expr expression, boolean descending, Boolean nullsFirst, String opclass) { this(expression, descending, nullsFirst, opclass, descending); }
    }
    record CreateIndex(Name name, Name table, boolean unique, boolean ifNotExists, String method,
                       List<IndexKey> keys, Expr where) implements Statement { CreateIndex { keys = List.copyOf(keys); } }
    record DropIndex(List<Name> names, boolean ifExists, boolean cascade) implements Statement { DropIndex { names = List.copyOf(names); } }
    record AlterAction(String action, String name, String newName, Column column, Expr expression,
                       TypeName type, Constraint constraint, boolean ifExists, Identity identity, List<SequenceOption> sequenceOptions, Boolean generated) {
        AlterAction(String action, String name, String newName, Column column, Expr expression, TypeName type, Constraint constraint, boolean ifExists) { this(action, name, newName, column, expression, type, constraint, ifExists, null, List.of(), null); }
    }
    record AlterTable(Name table, boolean ifExists, List<AlterAction> actions, boolean viewOnly) implements Statement {
        AlterTable(Name table, boolean ifExists, List<AlterAction> actions) { this(table, ifExists, actions, false); }
        AlterTable { actions = List.copyOf(actions); }
    }

    private final List<Lexer.Token> tokens;
    private int at;
    private int depth;
    private final Map<Expr, int[]> itemSpans = new IdentityHashMap<>();
    private final Map<Name, Integer> nameTokens = new IdentityHashMap<>();
    private final Map<Table, int[]> sourceSpans = new IdentityHashMap<>();
    private final Set<Expr> fieldAccessible = Collections.newSetFromMap(new IdentityHashMap<>());

    SqlParser(String sql) { tokens = new Lexer(sql).scan(); }

    Select parse() {
        Statement statement = parseStatement();
        if (!(statement instanceof Select select)) throw new SqlException("0A000", "statement execution is not implemented yet");
        return select;
    }

    Statement parseStatement() {
        Statement statement = statement();
        take(";");
        validateDepth(statement);
        if (peek().kind() != Lexer.Kind.END) throw syntax("unexpected token: " + peek().text());
        return statement;
    }

    String expanded(java.util.Map<Expr, String> replacements) {
        Map<Integer, int[]> spans = new java.util.HashMap<>(); Map<Integer, String> values = new java.util.HashMap<>();
        itemSpans.forEach((expression, span) -> { String value = replacements.get(expression); if (value != null) { spans.put(span[0], span); values.put(span[0], value); } });
        List<String> pieces = new ArrayList<>();
        for (int i = 0; i < tokens.size() && tokens.get(i).kind() != Lexer.Kind.END; ) {
            if (spans.containsKey(i)) { pieces.add(values.get(i)); i = spans.get(i)[1]; }
            else { pieces.add(SqlTokens.render(List.of(tokens.get(i)))); i++; }
        }
        return String.join(" ", pieces);
    }
    String rewrittenSources(Map<Table, String> replacements) {
        Map<Integer, int[]> spans = new HashMap<>(); Map<Integer, String> values = new HashMap<>();
        sourceSpans.forEach((source, span) -> {
            String replacement = replacements.get(source);
            if (replacement != null) {
                String originalName = "\"" + source.name().parts().getLast().replace("\"", "\"\"") + "\"";
                int nameOffset = source.name().parts().size() == 2 ? ("\"" + source.name().parts().getFirst().replace("\"", "\"\"") + "\" . ").length() : 0;
                if (replacement.startsWith(originalName, nameOffset)) {
                    int nameToken = span[0] + (source.name().parts().size() == 2 ? 2 : 0);
                    replacement = replacement.substring(0, nameOffset) + SqlTokens.render(List.of(tokens.get(nameToken))) + replacement.substring(nameOffset + originalName.length());
                }
                if (source.alias() != null) {
                    String generated = " as \"" + source.alias().replace("\"", "\"\"") + "\"";
                    int position = replacement.indexOf(generated);
                    int aliasStart = span[0] + (source.name().parts().size() == 2 ? 3 : 1);
                    int aliasEnd = aliasStart + (tokens.get(aliasStart).text().equals("as") ? 2 : 1);
                    if (position >= 0) replacement = replacement.substring(0, position) + " " + SqlTokens.render(tokens.subList(aliasStart, aliasEnd)) + replacement.substring(position + generated.length());
                }
                if (source.name().parts().size() == 2) {
                    String prefix = "\"" + source.name().parts().getFirst().replace("\"", "\"\"") + "\" . ";
                    if (replacement.startsWith(prefix)) replacement = SqlTokens.render(tokens.subList(span[0], span[0] + 2)) + " " + replacement.substring(prefix.length());
                }
                spans.put(span[0], span); values.put(span[0], replacement);
            }
        });
        List<String> pieces = new ArrayList<>();
        for (int i = 0; i < tokens.size() && tokens.get(i).kind() != Lexer.Kind.END; ) {
            if (spans.containsKey(i)) { pieces.add(values.get(i)); i = spans.get(i)[1]; }
            else { pieces.add(SqlTokens.render(List.of(tokens.get(i)))); i++; }
        }
        return String.join(" ", pieces);
    }

    private Statement statement() {
        if (take("analyze")) {
            Name table = name(); List<String> columns = new ArrayList<>();
            if (take("(")) { do { columns.add(identifier()); } while (take(",")); expect(")"); }
            return new Analyze(table, columns);
        }
        if (take("explain")) return explain();
        if (take("reindex")) return reindex();
        if (queryStart()) return query();
        if (take("create")) return create();
        if (take("drop")) return drop();
        if (take("alter")) return alter();
        if (take("insert")) return insert();
        if (take("update")) return update();
        if (take("delete")) return delete();
        if (take("merge")) return merge();
        if (take("begin")) { take("work"); take("transaction"); return new Transaction("begin", null, readOnly()); }
        if (take("start")) { expect("transaction"); return new Transaction("begin", null, readOnly()); }
        if (take("commit") || take("end")) { take("work"); take("transaction"); return new Transaction("commit", null, false); }
        if (take("rollback") || take("abort")) {
            take("work"); take("transaction");
            if (take("to")) { savepointKeyword(); return new Transaction("rollback_to", identifier(), false); }
            return new Transaction("rollback", null, false);
        }
        if (take("savepoint")) return new Transaction("savepoint", identifier(), false);
        if (take("release")) { savepointKeyword(); return new Transaction("release", identifier(), false); }
        throw syntax("unknown statement");
    }
    private void savepointKeyword() {
        if (is("savepoint") && at + 1 < tokens.size() && (tokens.get(at + 1).kind() == Lexer.Kind.WORD || tokens.get(at + 1).kind() == Lexer.Kind.NAME)) at++;
    }

    private Statement query() {
        enter();
        try {
            if (take("with")) return with();
            Statement result = intersect();
            while (is("union") || is("except")) {
                String kind = tokens.get(at++).text(); boolean all = take("all"); if (!all) take("distinct");
                Statement right = intersect(); if (right instanceof CreateTable) throw syntax("INTO is allowed only in the first SELECT"); result = result instanceof CreateTable into ? intoQuery(into, new SetOperation(into.asQuery(), right, kind, all)) : new SetOperation(result, right, kind, all);
            }
            Suffix suffix = suffix();
            if (result instanceof CreateTable into && (!suffix.order.isEmpty() || suffix.limit != null || suffix.offset != null)) return intoQuery(into, new QueryTail(into.asQuery(), suffix.order, suffix.limit, suffix.offset));
            if (suffix.order.isEmpty() && suffix.limit == null && suffix.offset == null) return result;
            Suffix prior = result instanceof Select x ? new Suffix(x.order(), x.limit(), x.offset()) : result instanceof Values x ? new Suffix(x.order(), x.limit(), x.offset()) : result instanceof QueryTail x ? new Suffix(x.order(), x.limit(), x.offset()) : new Suffix(List.of(), null, null);
            if (!prior.order().isEmpty() && !suffix.order().isEmpty() || prior.limit() != null && suffix.limit() != null || prior.offset() != null && suffix.offset() != null) throw syntax("repeated query clause");
            if (result instanceof Select x && x.order.isEmpty() && x.limit == null && x.offset == null)
                return new Select(x.items, x.where, suffix.order, suffix.limit, suffix.offset, x.distinct, x.from, x.group, x.having, x.distinctOn, x.windows);
            if (result instanceof Values x && x.order.isEmpty() && x.limit == null && x.offset == null)
                return new Values(x.rows, suffix.order, suffix.limit, suffix.offset);
            return new QueryTail(result, suffix.order, suffix.limit, suffix.offset);
        } finally { depth--; }
    }
    private Statement intersect() {
        Statement result = queryOperand();
        while (take("intersect")) {
            boolean all = take("all"); if (!all) take("distinct");
            Statement right = queryOperand(); if (right instanceof CreateTable) throw syntax("INTO is allowed only in the first SELECT"); result = result instanceof CreateTable into ? intoQuery(into, new SetOperation(into.asQuery(), right, "intersect", all)) : new SetOperation(result, right, "intersect", all);
        }
        return result;
    }
    private static CreateTable intoQuery(CreateTable into, Statement query) { return new CreateTable(into.name(), into.temporary(), into.ifNotExists(), into.columns(), into.constraints(), query, into.columnNames(), into.onCommit(), into.withData()); }
    private Statement queryOperand() {
        if (take("(")) { Statement query = query(); expect(")"); return query; }
        if (is("select")) return select();
        if (is("values")) return values();
        throw syntax("expected query");
    }
    private Statement readQuery() {
        Statement result = query();
        if (result instanceof CreateTable) throw syntax("INTO is not allowed in a query operand");
        return result;
    }
    private Statement with() {
        boolean recursive = take("recursive"); List<Cte> ctes = new ArrayList<>();
        do {
            String name = identifier(); List<String> columns = optionalNames(); expect("as");
            String materialization = null;
            if (take("materialized")) materialization = "materialized";
            else if (take("not")) { expect("materialized"); materialization = "not_materialized"; }
            expect("("); Statement body = statement(); expect(")");
            if (!queryOrDml(body)) throw syntax("CTE requires query or data-modifying statement");
            Search search = null; Cycle cycle = null;
            if (take("search")) {
                boolean breadth = take("breadth"); if (!breadth) expect("depth");
                expect("first"); expect("by"); List<String> by = names(); expect("set"); search = new Search(breadth, by, identifier());
            }
            if (take("cycle")) {
                List<String> by = names(); expect("set"); String mark = identifier();
                Expr value = new Literal(Lexer.Kind.WORD, "true"), otherwise = new Literal(Lexer.Kind.WORD, "false");
                if (take("to")) { value = cycleConstant(); expect("default"); otherwise = cycleConstant(); }
                expect("using"); cycle = new Cycle(by, mark, value, otherwise, identifier());
            }
            ctes.add(new Cte(name, columns, materialization, body, search, cycle));
        } while (take(","));
        Statement body = statement();
        if (body instanceof With) throw syntax("multiple WITH clauses are not allowed");
        if (body instanceof CreateTable into && into.asQuery() != null) return intoQuery(into, new With(recursive, ctes, into.asQuery()));
        if (!queryOrDml(body)) throw syntax("WITH requires query or data-modifying statement");
        return new With(recursive, ctes, body);
    }
    private Expr cycleConstant() {
        if (is("-") || is("+")) throw syntax("CYCLE mark requires an unsigned constant");
        Expr value = primary();
        if (!(value instanceof Literal || value instanceof Cast cast && cast.value instanceof Literal)) throw syntax("CYCLE mark requires a constant");
        return value;
    }
    private static boolean queryOrDml(Statement value) {
        return value instanceof Select || value instanceof Values || value instanceof With || value instanceof SetOperation || value instanceof QueryTail
            || value instanceof Insert || value instanceof Update || value instanceof Delete || value instanceof Merge;
    }
    private boolean queryStart() {
        int next = at;
        while (next < tokens.size() - 1 && tokens.get(next).is("(")) next++;
        return tokens.get(next).is("with") || tokens.get(next).is("select") || tokens.get(next).is("values") && next + 1 < tokens.size() && tokens.get(next + 1).is("(");
    }
    private Explain explain() {
        boolean analyze = false, verbose = false, costs = true, lane = false;
        if (take("analyze")) analyze = true;
        else if (is("(") && !queryStart()) {
            at++;
            Set<String> seen = new java.util.HashSet<>();
            do {
                String option = identifier();
                if (!seen.add(option)) throw syntax("duplicate EXPLAIN option");
                boolean value = true;
                if (take("true") || take("on")) value = true;
                else if (take("false") || take("off")) value = false;
                switch (option) {
                    case "analyze" -> analyze = value;
                    case "verbose" -> verbose = value;
                    case "costs" -> costs = value;
                    case "lane" -> lane = value;
                    default -> throw syntax("unknown EXPLAIN option");
                }
            } while (take(","));
            expect(")");
        }
        if (is("explain") || is("analyze")) throw syntax("invalid EXPLAIN statement prefix");
        Statement inner = statement();
        if (!(queryOrDml(inner) || inner instanceof CreateTable create && create.asQuery != null)) throw syntax("statement cannot be explained");
        return new Explain(inner, analyze, verbose, costs, lane);
    }

    private boolean readOnly() {
        if (!take("read")) return false;
        if (take("only")) return true;
        expect("write"); return false;
    }

    private Statement select() {
        expect("select");
        boolean distinct = !ahead(1, "from") && take("distinct");
        List<Expr> distinctOn = List.of();
        if (distinct && take("on")) { expect("("); distinctOn = expressionList(); expect(")"); }
        if (!distinct) take("all");
        List<Item> items = items();
        Name into = null; boolean temporaryInto = false;
        if (take("into")) { if (!ahead(1, ".")) temporaryInto = take("temporary") || take("temp"); if (take("unlogged")) throw new SqlException("0A000", "unlogged SELECT INTO is unsupported"); take("table"); into = name(); }
        List<From> from = take("from") ? fromList() : List.of();
        if (from.isEmpty() && items.stream().anyMatch(item -> item.expression() instanceof Star)) throw syntax("SELECT * requires a FROM clause");
        Expr where = take("where") ? expression(0) : null;
        List<Expr> group = List.of();
        if (take("group")) { expect("by"); group = new ArrayList<>(); do { group.add(groupingTerm()); } while (take(",")); }
        Expr having = take("having") ? expression(0) : null;
        List<NamedWindow> windows = new ArrayList<>();
        if (take("window")) do { String name = identifier(); expect("as"); expect("("); windows.add(new NamedWindow(name, windowSpec())); expect(")"); } while (take(","));
        Select selected = new Select(items, where, List.of(), null, null, distinct, from, group, having, distinctOn, windows);
        return into == null ? selected : new CreateTable(into, temporaryInto, false, List.of(), List.of(), selected, List.of(), null, true);
    }

    private WindowSpec windowSpec() {
        String base = null;
        if (!is(")") && !is("partition") && !is("order") && !is("rows") && !is("range") && !is("groups")) base = identifier();
        List<Expr> partition = new ArrayList<>(); List<Order> order = new ArrayList<>();
        if (take("partition")) { expect("by"); do { partition.add(expression(0)); } while (take(",")); }
        if (take("order")) {
            expect("by");
            do {
                Expr expression = expression(0); boolean descending = take("desc"); if (!descending) take("asc");
                Boolean nullsFirst = null;
                if (take("nulls")) { if (take("first")) nullsFirst = true; else { expect("last"); nullsFirst = false; } }
                order.add(new Order(expression, descending, nullsFirst));
            } while (take(","));
        }
        WindowFrame frame = null;
        if (is("rows") || is("range") || is("groups")) {
            String mode = tokens.get(at++).text(); FrameBound start, end;
            if (take("between")) { start = frameBound(); expect("and"); end = frameBound(); }
            else { start = frameBound(); end = new FrameBound("current", null); }
            String exclude = "none";
            if (take("exclude")) {
                if (take("current")) { expect("row"); exclude = "current"; }
                else if (take("group")) exclude = "group";
                else if (take("ties")) exclude = "ties";
                else { expect("no"); expect("others"); }
            }
            frame = new WindowFrame(mode, start, end, exclude);
        }
        return new WindowSpec(base, partition, order, frame);
    }
    private FrameBound frameBound() {
        if (take("current")) { expect("row"); return new FrameBound("current", null); }
        if (take("unbounded")) { if (take("preceding")) return new FrameBound("unbounded_preceding", null); expect("following"); return new FrameBound("unbounded_following", null); }
        Expr value = expression(31);
        if (take("preceding")) return new FrameBound("preceding", value);
        expect("following"); return new FrameBound("following", value);
    }

    private Expr groupingTerm() {
        enter();
        try {
            if (is("grouping") && ahead(1, "sets")) {
                at += 2; expect("("); List<Expr> sets = new ArrayList<>();
                if (!is(")")) do { sets.add(groupingTerm()); } while (take(","));
                expect(")"); return new Call("$grouping_sets", sets);
            }
            if ((is("rollup") || is("cube")) && ahead(1, "(")) {
                String kind = tokens.get(at++).text(); expect("("); List<Expr> terms = new ArrayList<>();
                if (!is(")")) do { terms.add(groupingTerm()); } while (take(","));
                expect(")"); return new Call("$" + kind, terms);
            }
            if (is("(") && ahead(1, ")")) { at += 2; return new Call("$grouping_tuple", List.of()); }
            boolean parenthesized = is("(");
            Expr expression = expression(0);
            if (parenthesized && expression instanceof RowConstructor row) return new Call("$grouping_tuple", row.values());
            return expression;
        } finally { depth--; }
    }

    private List<Item> items() {
        List<Item> items = new ArrayList<>();
        do {
            int start = at;
            Expr expression = take("*") ? new Star(List.of()) : expression(0);
            if (expression instanceof Star) itemSpans.put(expression, new int[] {start, at});
            String alias = null;
            if (take("as")) alias = identifier();
            else if (peek().kind() == Lexer.Kind.NAME || peek().kind() == Lexer.Kind.WORD && !noBareLabel(peek().text())) alias = identifier();
            if (expression instanceof Star && alias != null) throw syntax("star projection cannot have an alias");
            items.add(new Item(expression, alias));
        } while (take(","));
        return items;
    }

    private record Suffix(List<Order> order, Expr limit, Expr offset) {}
    private Suffix suffix() {
        List<Order> orders = new ArrayList<>();
        if (take("order")) {
            expect("by");
            do {
                Expr expr = expression(0);
                boolean descending = take("desc");
                if (!descending) take("asc");
                Boolean nullsFirst = null;
                if (take("nulls")) {
                    if (take("first")) nullsFirst = true;
                    else { expect("last"); nullsFirst = false; }
                }
                orders.add(new Order(expr, descending, nullsFirst));
            } while (take(","));
        }
        Expr limit = null, offset = null;
        boolean hasLimit = false, hasOffset = false;
        while (is("limit") || is("offset")) {
            if (take("limit")) {
                if (hasLimit) throw syntax("duplicate LIMIT");
                hasLimit = true;
                if (!take("all")) limit = expression(0);
            } else {
                expect("offset");
                if (hasOffset) throw syntax("duplicate OFFSET");
                hasOffset = true;
                offset = expression(0);
            }
        }
        return new Suffix(orders, limit, offset);
    }

    private Values values() {
        expect("values");
        List<List<Expr>> rows = valueRows();
        return new Values(rows, List.of(), null, null);
    }
    private List<List<Expr>> valueRows() {
        List<List<Expr>> rows = new ArrayList<>();
        do { expect("("); rows.add(expressionList()); expect(")"); } while (take(","));
        return rows;
    }
    private List<Expr> expressionList() {
        List<Expr> values = new ArrayList<>();
        do { values.add(expression(0)); } while (take(","));
        return values;
    }
    private List<String> names() {
        List<String> names = new ArrayList<>();
        do { names.add(identifier()); } while (take(","));
        return names;
    }
    private List<String> optionalNames() {
        if (!take("(")) return List.of();
        List<String> names = names(); expect(")"); return names;
    }
    private Name name() {
        List<String> parts = new ArrayList<>();
        parts.add(identifier());
        while (take(".")) parts.add(identifier());
        return new Name(parts);
    }
    private List<From> fromList() {
        List<From> sources = new ArrayList<>();
        do { sources.add(joined()); } while (take(","));
        return sources;
    }
    private From joined() {
        From left = source();
        while (is("join") || is("inner") || is("left") || is("right") || is("full") || is("cross") || is("natural")) {
            boolean natural = take("natural");
            String kind = "inner";
            if (take("left")) { kind = "left"; take("outer"); }
            else if (take("right")) { kind = "right"; take("outer"); }
            else if (take("full")) { kind = "full"; take("outer"); }
            else if (take("cross")) { if (natural) throw syntax("NATURAL CROSS JOIN is not allowed"); kind = "cross"; }
            else take("inner");
            expect("join");
            From right = source();
            Expr on = null;
            List<String> using = List.of();
            if (!natural && !kind.equals("cross")) {
                if (take("on")) on = expression(0);
                else { expect("using"); expect("("); using = names(); expect(")"); }
            }
            left = new Join(left, right, kind, on, using, natural);
        }
        return left;
    }
    private From source() {
        enter();
        try {
            boolean lateral = take("lateral");
            if (take("(")) {
                if (sourceQueryStart()) {
                    Statement query = readQuery();
                    expect(")");
                    String alias = sourceAlias();
                    return new Derived(query, alias, optionalNames(), lateral);
                }
                if (lateral) throw syntax("LATERAL requires a query or table function");
                From source = joined(); expect(")");
                if (!(source instanceof Join) && !(source instanceof Grouped)) throw syntax("parenthesized relation requires a join");
                String alias = sourceAlias();
                return new Grouped(source, alias, alias == null ? List.of() : optionalNames());
            }
            if (is("json_table") && ahead(1, "(")) return jsonTable();
            int sourceStart = at; Name table = name();
            if (take("(")) {
                if (table.parts().size() != 1) throw syntax("table-function name must be unqualified");
                List<Expr> args = new ArrayList<>();
                if (!take(")")) {
                    do {
                        if (is("*") || is("distinct") || is("all")) throw syntax("invalid table-function argument");
                        if ((peek().kind() == Lexer.Kind.WORD || peek().kind() == Lexer.Kind.NAME) && ahead(1, "=>")) {
                            String parameter = identifier(); expect("=>");
                            for (Expr argument : args) if (argument instanceof NamedArgument named && named.name().equals(parameter)) throw syntax("duplicate argument name");
                            args.add(new NamedArgument(parameter, expression(0)));
                        } else {
                            if (args.stream().anyMatch(argument -> argument instanceof NamedArgument)) throw syntax("positional argument follows named argument");
                            args.add(expression(0));
                        }
                    } while (take(","));
                    expect(")");
                }
                String alias = null;
                boolean as = take("as");
                if (!as || !is("(")) alias = as ? identifier() : sourceAlias();
                List<String> columns = new ArrayList<>(); List<Column> definitions = new ArrayList<>();
                if (take("(")) {
                    String column = identifier(); boolean typed = !is(",") && !is(")");
                    if (!typed && alias == null) throw syntax("column aliases require a relation alias");
                    while (true) {
                        if (typed) definitions.add(new Column(column, typeName(), List.of())); else columns.add(column);
                        if (!take(",")) break; column = identifier();
                    }
                    expect(")");
                }
                return new TableFunction(new Call(table.parts().getFirst(), args), alias, columns, definitions, lateral);
            }
            if (lateral) throw syntax("LATERAL requires a query or table function");
            String alias = sourceAlias();
            List<String> columns = alias == null ? List.of() : optionalNames();
            Table result = new Table(table, alias, columns); sourceSpans.put(result, new int[] {sourceStart, at}); return result;
        } finally { depth--; }
    }
    private boolean expressionQueryStart() {
        if (!queryStart()) return false;
        if (!is("(")) return true;
        int nesting = 0;
        for (int i = at; i < tokens.size(); i++) {
            if (tokens.get(i).is("(")) nesting++;
            else if (tokens.get(i).is(")") && --nesting == 0) {
                Lexer.Token next = tokens.get(Math.min(i + 1, tokens.size() - 1));
                return next.is(")") || next.is("union") || next.is("intersect") || next.is("except") || next.is("order") || next.is("limit") || next.is("offset");
            }
        }
        return true;
    }
    private boolean sourceQueryStart() {
        if (!queryStart()) return false;
        if (!is("(")) return true;
        int nesting = 0;
        for (int i = at; i < tokens.size(); i++) {
            if (tokens.get(i).is("(")) nesting++;
            else if (tokens.get(i).is(")") && --nesting == 0) {
                return i + 1 == tokens.size() || !(tokens.get(i + 1).is("as") || tokens.get(i + 1).is("join") || tokens.get(i + 1).is("inner") || tokens.get(i + 1).is("left") || tokens.get(i + 1).is("right") || tokens.get(i + 1).is("full") || tokens.get(i + 1).is("cross") || tokens.get(i + 1).is("natural"));
            }
        }
        return true;
    }
    private String sourceAlias() {
        if (take("as")) return identifier();
        if (peek().kind() == Lexer.Kind.NAME || peek().kind() == Lexer.Kind.WORD && !clause(peek().text())) return identifier();
        return null;
    }

    private static boolean noBareLabel(String word) {
        return switch (word) {
            case "array", "as", "char", "character", "create", "day", "except", "fetch", "filter", "for", "from", "grant", "group", "having", "hour", "intersect", "into", "isnull", "limit", "minute", "month", "notnull", "offset", "on", "order", "over", "overlaps", "precision", "returning", "second", "to", "union", "varying", "where", "window", "with", "within", "without", "year" -> true;
            default -> false;
        };
    }

    private static boolean clause(String word) {
        return switch (word) {
            case "from", "where", "group", "having", "window", "order", "limit", "offset", "union", "except", "intersect", "fetch",
                 "join", "inner", "left", "right", "full", "cross", "natural", "on", "using", "returning", "set", "with", "for" -> true;
            default -> false;
        };
    }

    private Statement create() {
        boolean replace = take("or"); if (replace) expect("replace");
        boolean temporary = take("temporary") || take("temp");
        if (take("unlogged")) throw new SqlException("0A000", "unlogged tables are not supported");
        if (take("materialized")) { expect("view"); throw new SqlException("0A000", "materialized views are not supported"); }
        boolean recursive = take("recursive");
        if (take("view")) return createView(temporary, replace, recursive);
        if (recursive) throw syntax("expected VIEW after RECURSIVE");
        if (replace) throw syntax("expected VIEW after OR REPLACE");
        if (take("table")) return createTable(temporary);
        if (temporary) throw syntax("expected TABLE after TEMP");
        if (take("sequence")) { boolean exists = ifNotExists(); Name name = name(); return new CreateSequence(name, exists, sequenceOptions(false)); }
        if (take("type")) {
            Name name = name(); expect("as"); expect("("); List<Column> fields = new ArrayList<>();
            if (!take(")")) { do { fields.add(column()); } while (take(",")); expect(")"); }
            return new CreateType(name, fields);
        }
        boolean unique = take("unique");
        if (take("index")) return createIndex(unique);
        throw syntax("unknown CREATE object kind");
    }
    private CreateView createView(boolean temporary, boolean replace, boolean recursive) {
        boolean ifNotExists = ifNotExists();
        if (replace && ifNotExists) throw syntax("OR REPLACE and IF NOT EXISTS cannot be combined");
        Name name = name(); List<String> columns = optionalNames(); List<ViewOption> options = new ArrayList<>();
        if (take("with")) {
            expect("(");
            do {
                String option = identifier(); String value = "true";
                if (take("=")) { Lexer.Token token = tokens.get(at++); if (token.kind() == Lexer.Kind.END || token.kind() == Lexer.Kind.SYMBOL) throw syntax("expected view option value"); value = token.text(); }
                options.add(new ViewOption(option, value));
            } while (take(","));
            expect(")");
        }
        expect("as"); int start = at; Statement query = query();
        String definition = SqlTokens.render(tokens.subList(start, at)); int checkOption = 0;
        if (take("with")) {
            checkOption = take("local") ? 1 : 2; if (checkOption == 2) take("cascaded");
            expect("check"); expect("option");
        }
        if (recursive) {
            if (columns.isEmpty()) throw syntax("recursive view requires a column list");
            if (checkOption != 0 || options.stream().anyMatch(option -> option.name().equals("check_option"))) throw new SqlException("0A000", "CHECK OPTION is unsupported for recursive views");
            String quotedName = "\"" + name.parts().getLast().replace("\"", "\"\"") + "\"";
            String quotedColumns = columns.stream().map(column -> "\"" + column.replace("\"", "\"\"") + "\"").collect(java.util.stream.Collectors.joining(" , "));
            definition = "with recursive " + quotedName + " ( " + quotedColumns + " ) as ( " + definition + " ) select " + quotedColumns + " from " + quotedName;
            query = new SqlParser(definition).parseStatement();
        }
        return new CreateView(name, temporary, replace, ifNotExists, columns, query, definition, checkOption, options);
    }
    private List<SequenceOption> sequenceOptions(boolean alter) {
        List<SequenceOption> options = new ArrayList<>(); Set<String> seen = new java.util.HashSet<>();
        while (true) {
            String option; Long value = null; TypeName type = null;
            if (take("as")) { if (alter) throw new SqlException("0A000", "ALTER SEQUENCE AS is not supported"); option = "as"; type = typeName(); }
            else if (take("increment")) { option = "increment"; take("by"); value = sequenceInteger(); }
            else if (take("start")) { option = "start"; take("with"); value = sequenceInteger(); }
            else if (take("restart")) { if (!alter) throw syntax("RESTART requires ALTER SEQUENCE"); option = "restart"; if (take("with") || is("-") || is("+") || peek().kind() == Lexer.Kind.INTEGER) value = sequenceInteger(); }
            else if (take("minvalue")) { option = "minimum"; value = sequenceInteger(); }
            else if (take("maxvalue")) { option = "maximum"; value = sequenceInteger(); }
            else if (take("cache")) { option = "cache"; value = sequenceInteger(); }
            else if (take("cycle")) { option = "cycle"; value = 1L; }
            else if (take("no")) {
                if (take("cycle")) { option = "cycle"; value = 0L; }
                else if (take("minvalue")) option = "minimum";
                else { expect("maxvalue"); option = "maximum"; }
            } else if (is("owned")) throw new SqlException("0A000", "manual sequence ownership is not supported");
            else break;
            if (!seen.add(option)) throw syntax("conflicting or redundant sequence options");
            options.add(new SequenceOption(option, value, type));
        }
        return options;
    }
    private long sequenceInteger() {
        boolean negative = take("-"); if (!negative) take("+"); Lexer.Token token = tokens.get(at++);
        if (token.kind() != Lexer.Kind.INTEGER) throw syntax("expected integer sequence option");
        try { return Long.parseLong((negative ? "-" : "") + Decimal.parse(token.text()).toString()); }
        catch (NumberFormatException failure) { throw SqlException.numericOverflow(); }
    }
    private boolean ifNotExists() {
        if (!is("if") || !ahead(1, "not")) return false;
        at += 2; expect("exists"); return true;
    }
    private boolean ifExists() {
        if (!is("if") || !ahead(1, "exists")) return false;
        at += 2; return true;
    }
    private CreateTable createTable(boolean temporary) {
        boolean ifNotExists = ifNotExists();
        Name table = name();
        List<Column> columns = new ArrayList<>();
        List<Constraint> constraints = new ArrayList<>();
        List<String> columnNames = List.of();
        Statement asQuery = null;
        // A parenthesized name-only list is the optional CTAS output renaming list.
        boolean nameList = false;
        if (is("(")) {
            int saved = at;
            try { at++; names(); expect(")"); nameList = is("as") || is("on"); }
            catch (SqlException ignored) { /* typed column list */ }
            finally { at = saved; }
        }
        if (nameList) columnNames = optionalNames();
        else if (take("(")) {
            do {
                if (tableConstraintStart()) constraints.add(constraint(null));
                else columns.add(column());
            } while (take(","));
            expect(")");
        }
        String onCommit = onCommit();
        boolean withData = true;
        if (take("as")) {
            if (!queryStart()) throw syntax("expected query after AS");
            asQuery = readQuery();
            if (take("with")) { withData = !take("no"); expect("data"); }
        } else if (columns.isEmpty()) throw syntax("CREATE TABLE requires columns or AS query");
        return new CreateTable(table, temporary, ifNotExists, columns, constraints, asQuery, columnNames, onCommit, withData);
    }
    private String onCommit() {
        if (!take("on")) return null;
        expect("commit");
        if (take("drop")) return "drop";
        if (take("delete")) { expect("rows"); return "delete"; }
        expect("preserve"); expect("rows"); return "preserve";
    }
    private boolean tableConstraintStart() {
        return is("exclude") || is("constraint") && (ahead(2, "check") || ahead(2, "unique") || ahead(2, "foreign") || ahead(2, "primary") || ahead(2, "exclude")) || is("primary") && ahead(1, "key") ||
            (is("check") || is("unique")) && ahead(1, "(") || is("foreign") && ahead(1, "key");
    }
    private Column column() {
        String name = identifier(); TypeName type = typeName(); String collation = take("collate") ? identifier() : null;
        List<Constraint> constraints = new ArrayList<>(); Identity identity = null;
        while (is("generated") || is("constraint") || is("primary") || is("not") || is("null") || is("default") || is("check") || is("unique") || is("references")) {
            if (take("generated")) {
                if (identity != null) throw syntax("multiple identity specifications");
                identity = identity();
            } else constraints.add(constraint(name));
        }
        return new Column(name, type, constraints, identity, collation);
    }
    private Identity identity() {
        boolean always = take("always"); if (!always) { expect("by"); expect("default"); }
        expect("as"); expect("identity"); String sequenceName = null; List<SequenceOption> options = new ArrayList<>();
        if (take("(")) {
            while (!is(")")) {
                if (take("sequence")) { expect("name"); if (sequenceName != null) throw syntax("duplicate SEQUENCE NAME"); sequenceName = identifier(); }
                else { List<SequenceOption> next = sequenceOptions(false); if (next.isEmpty()) throw syntax("expected identity sequence option"); options.addAll(next); }
            }
            expect(")");
        }
        Set<String> seen = new java.util.HashSet<>();
        for (SequenceOption option : options) if (option.name().equals("as") || !seen.add(option.name())) throw syntax("conflicting or redundant identity options");
        return new Identity(always, sequenceName, options);
    }
    private Constraint constraint(String column) {
        String name = take("constraint") ? identifier() : null;
        if (take("exclude")) {
            if (column != null) throw syntax("EXCLUDE must be a table constraint");
            String method = take("using") ? identifier() : "btree";
            List<ExcludeElement> elements = new ArrayList<>(); expect("(");
            do { Expr expression = storedExpression(); expect("with"); String operator = peek().text(); at++;
                if (!List.of("&&", "=", "<>", "<", ">", "<=", ">=", "@>", "<@", "<<", ">>", "&<", "&>", "-|-").contains(operator)) throw syntax("expected exclusion operator");
                elements.add(new ExcludeElement(expression, operator));
            } while (take(",")); expect(")");
            Expr predicate = null; if (take("where")) { expect("("); predicate = storedExpression(); expect(")"); }
            return new Constraint("exclude", name, List.of(), predicate, null, List.of(), null, null, false, method, elements);
        }
        String kind;
        Expr expression = null;
        List<String> columns = column == null ? List.of() : List.of(column);
        Name reference = null;
        List<String> referenceColumns = List.of();
        String onDelete = "no_action", onUpdate = "no_action";
        if (take("primary")) { expect("key"); kind = "primary_key"; if (column == null) { expect("("); columns = names(); expect(")"); } }
        else if (take("unique")) { kind = "unique"; if (column == null) { expect("("); columns = names(); expect(")"); } }
        else if (take("not")) { expect("null"); kind = "not_null"; }
        else if (take("null")) kind = "null";
        else if (take("default")) { kind = "default"; expression = storedExpression(); }
        else if (take("check")) { kind = "check"; expect("("); expression = storedExpression(); expect(")"); }
        else if (take("foreign")) { expect("key"); expect("("); columns = names(); expect(")"); kind = "foreign_key"; }
        else if (is("references") && column != null) kind = "foreign_key";
        else throw syntax("expected constraint");
        boolean matchFull = false;
        if (kind.equals("foreign_key")) {
            expect("references"); reference = name(); referenceColumns = optionalNames();
            if (take("match")) { if (take("full")) matchFull = true; else if (!take("simple")) { expect("partial"); throw new SqlException("0A000", "MATCH PARTIAL is not implemented"); } }
            boolean deleteSeen = false, updateSeen = false;
            while (take("on")) {
                if (take("delete")) { if (deleteSeen) throw syntax("duplicate ON DELETE"); deleteSeen = true; onDelete = referenceAction(); }
                else { expect("update"); if (updateSeen) throw syntax("duplicate ON UPDATE"); updateSeen = true; onUpdate = referenceAction(); }
            }
        }
        if (column == null && (kind.equals("default") || kind.equals("null") || kind.equals("not_null"))) throw syntax("column constraint outside column");
        return new Constraint(kind, name, columns, expression, reference, referenceColumns, onDelete, onUpdate, matchFull);
    }
    private String referenceAction() {
        if (take("cascade")) return "cascade";
        if (take("restrict")) return "restrict";
        if (take("no")) { expect("action"); return "no_action"; }
        expect("set");
        if (take("null")) return "set_null";
        expect("default"); return "set_default";
    }
    private Statement drop() {
        boolean sequence = take("sequence"), type = !sequence && take("type"), table = !sequence && !type && take("table"), view = !sequence && !type && !table && take("view");
        if (!sequence && !table && !type && !view && !take("index")) throw new SqlException("0A000", "DROP object kind is not implemented yet");
        boolean ifExists = ifExists();
        List<Name> names = new ArrayList<>();
        do { names.add(name()); } while (take(","));
        boolean cascade = take("cascade");
        if (!cascade) take("restrict");
        return sequence ? new DropSequence(names, ifExists, cascade) : view ? new DropView(names, ifExists, cascade) : type ? new DropType(names, ifExists, cascade) : table ? new DropTable(names, ifExists, cascade) : new DropIndex(names, ifExists, cascade);
    }
    private Insert insert() {
        expect("into"); Name table = name();
        String alias = take("as") ? identifier() : null;
        List<String> columns;
        if (queryStart() && is("(")) {
            int saved = at;
            try { columns = optionalNames(); }
            catch (SqlException failure) { at = saved; columns = List.of(); }
        } else columns = optionalNames();
        String overriding = null;
        if (take("overriding")) { overriding = take("system") ? "system" : "user"; if (overriding.equals("user")) expect("user"); expect("value"); }
        List<List<Expr>> rows = List.of();
        Statement query = null;
        boolean defaults = false;
        if (take("default")) { expect("values"); defaults = true; }
        else if (queryStart()) {
            query = readQuery();
            if (query instanceof Values values && values.order().isEmpty() && values.limit() == null && values.offset() == null) { rows = values.rows(); query = null; }
        }
        else throw syntax("expected VALUES, DEFAULT VALUES, or SELECT");
        Conflict conflict = null;
        if (take("on")) {
            expect("conflict"); List<ConflictElement> elements = new ArrayList<>(); String constraint = null; Expr predicate = null;
            if (take("(")) {
                do {
                    String column = identifier(), collation = take("collate") ? identifier() : null, opclass = null;
                    if ((peek().kind() == Lexer.Kind.WORD || peek().kind() == Lexer.Kind.NAME) && !is("asc") && !is("desc") && !is("nulls")) opclass = identifier();
                    boolean ordered = take("asc") || take("desc");
                    if (take("nulls")) { ordered = true; if (!take("first")) expect("last"); }
                    elements.add(new ConflictElement(column, collation, opclass, ordered));
                } while (take(","));
                expect(")"); if (take("where")) predicate = expression(0);
            } else if (take("on")) { expect("constraint"); constraint = identifier(); }
            expect("do"); boolean nothing = take("nothing"); List<Assignment> assignments = new ArrayList<>(); Expr where = null;
            if (!nothing) {
                expect("update"); expect("set");
                do { String column = identifier(); expect("="); assignments.add(new Assignment(column, expression(0))); } while (take(","));
                if (take("where")) where = expression(0);
            }
            conflict = new Conflict(elements, constraint, predicate, nothing, assignments, where);
        }
        Returning returning = returning();
        return new Insert(table, columns, rows, query, defaults, returning.items, returning.oldAlias, returning.newAlias, overriding, alias, conflict);
    }
    private record Returning(List<Item> items, String oldAlias, String newAlias) {}
    private Returning returning() {
        String oldAlias = "old", newAlias = "new";
        if (!take("returning")) return new Returning(List.of(), oldAlias, newAlias);
        if (is("with") && ahead(1, "(") && (ahead(2, "old") || ahead(2, "new")) && ahead(3, "as")) {
            at++; expect("("); boolean oldSeen = false, newSeen = false;
            do {
                boolean old = take("old"); if (!old) expect("new"); expect("as"); String alias = identifier();
                if (old) { if (oldSeen) throw syntax("duplicate OLD alias"); oldSeen = true; oldAlias = alias; }
                else { if (newSeen) throw syntax("duplicate NEW alias"); newSeen = true; newAlias = alias; }
            } while (take(","));
            expect(")");
        }
        return new Returning(items(), oldAlias, newAlias);
    }
    private Update update() {
        Name table = name(); String alias = sourceAlias(); expect("set");
        List<Assignment> assignments = new ArrayList<>();
        do { String column = identifier(); expect("="); assignments.add(new Assignment(column, expression(0))); } while (take(","));
        List<From> from = take("from") ? fromList() : List.of();
        Expr where = take("where") ? expression(0) : null;
        Returning returning = returning();
        return new Update(table, alias, assignments, from, where, returning.items, returning.oldAlias, returning.newAlias);
    }
    private Delete delete() {
        expect("from"); Name table = name(); String alias = sourceAlias();
        List<From> using = take("using") ? fromList() : List.of();
        Expr where = take("where") ? expression(0) : null;
        Returning returning = returning();
        return new Delete(table, alias, using, where, returning.items, returning.oldAlias, returning.newAlias);
    }
    private Merge merge() {
        expect("into"); Name table = name(); String alias = sourceAlias(); expect("using");
        From source = joined(); expect("on"); Expr on = expression(0);
        List<MergeWhen> clauses = new ArrayList<>(); Set<String> closed = new java.util.HashSet<>();
        while (take("when")) {
            boolean unmatched = take("not"); expect("matched"); String kind = unmatched ? "target" : "matched";
            if (unmatched && take("by")) { if (take("source")) kind = "source"; else expect("target"); }
            if (closed.contains(kind)) throw syntax("unreachable WHEN clause after unconditional clause");
            Expr condition = take("and") ? expression(0) : null; expect("then");
            if (condition == null) closed.add(kind);
            String action; List<Assignment> assignments = new ArrayList<>(); List<String> columns = List.of();
            List<Expr> values = new ArrayList<>(); boolean defaults = false; String overriding = null;
            if (take("do")) { expect("nothing"); action = "nothing"; }
            else if (!kind.equals("target") && take("delete")) action = "delete";
            else if (!kind.equals("target") && take("update")) {
                action = "update"; expect("set");
                do { String column = identifier(); expect("="); assignments.add(new Assignment(column, expression(0))); } while (take(","));
            } else if (kind.equals("target") && take("insert")) {
                action = "insert";
                if (take("(")) { columns = new ArrayList<>(); do { columns.add(identifier()); } while (take(",")); expect(")"); }
                if (take("overriding")) { if (take("system")) overriding = "system"; else { expect("user"); overriding = "user"; } expect("value"); }
                if (take("default")) { expect("values"); defaults = true; }
                else { expect("values"); expect("("); if (!is(")")) do { values.add(expression(0)); } while (take(",")); expect(")"); }
            } else throw syntax("invalid MERGE action");
            clauses.add(new MergeWhen(kind, condition, action, assignments, columns, values, defaults, overriding));
        }
        if (clauses.isEmpty()) throw syntax("MERGE requires WHEN clause");
        Returning output = returning();
        return new Merge(table, alias, source, on, clauses, output.items, output.oldAlias, output.newAlias);
    }
    private Reindex reindex() {
        boolean concurrently = false;
        if (take("(")) {
            do {
                String option = identifier();
                if (option.equals("tablespace")) { identifier(); throw new SqlException("0A000", "tablespaces are not supported"); }
                if (!option.equals("verbose") && !option.equals("concurrently")) throw syntax("unrecognized REINDEX option");
                boolean value = true;
                if (!is(",") && !is(")")) {
                    String word = tokens.get(at++).text();
                    value = switch (word.toLowerCase(java.util.Locale.ROOT)) {
                        case "true", "on", "1" -> true;
                        case "false", "off", "0" -> false;
                        default -> throw syntax("REINDEX option requires a Boolean value");
                    };
                }
                if (option.equals("concurrently")) concurrently = value;
            } while (take(","));
            expect(")");
        }
        String target = identifier();
        if (!List.of("index", "table", "database", "system", "schema").contains(target)) throw syntax("expected REINDEX target");
        boolean relation = target.equals("index") || target.equals("table");
        if (is("concurrently") && (!relation || at + 1 < tokens.size() && tokens.get(at + 1).kind() != Lexer.Kind.END && !ahead(1, ";"))) { at++; concurrently = true; }
        Name name = relation || target.equals("schema") ? name() : peek().kind() == Lexer.Kind.END || is(";") ? null : name();
        return new Reindex(target, name, concurrently);
    }
    private CreateIndex createIndex(boolean unique) {
        boolean ifNotExists = ifNotExists();
        Name name = is("on") && (!ahead(1, "on") || ahead(2, "(")) ? null : name(); if (ifNotExists && name == null) throw syntax("IF NOT EXISTS requires an index name"); expect("on"); Name table = name();
        String method = take("using") ? identifier() : "btree";
        expect("(");
        List<IndexKey> keys = new ArrayList<>();
        do {
            int start = at; Expr expression = expression(0);
            if (!(expression instanceof Name) && !(expression instanceof Call) && !tokens.get(start).is("(")) throw syntax("index expression must be parenthesized");
            expression = new StoredExpression(SqlTokens.render(tokens.subList(start, at)), expression, start);
            String opclass = null;
            if ((peek().kind() == Lexer.Kind.WORD || peek().kind() == Lexer.Kind.NAME) && !is("asc") && !is("desc") && !is("nulls")) opclass = identifier();
            boolean explicitDirection = is("desc") || is("asc"); boolean descending = take("desc"); if (!descending) take("asc");
            Boolean nullsFirst = null;
            if (take("nulls")) { if (take("first")) nullsFirst = true; else { expect("last"); nullsFirst = false; } }
            keys.add(new IndexKey(expression, descending, nullsFirst, opclass, explicitDirection));
        } while (take(","));
        expect(")"); Expr where = take("where") ? storedExpression() : null;
        return new CreateIndex(name, table, unique, ifNotExists, method, keys, where);
    }
    private Statement alter() {
        if (take("sequence")) {
            boolean exists = ifExists(); Name name = name();
            if (take("rename")) { expect("to"); return new AlterSequence(name, exists, identifier(), List.of()); }
            List<SequenceOption> options = sequenceOptions(true); if (options.isEmpty()) throw syntax("expected sequence options");
            return new AlterSequence(name, exists, null, options);
        }
        boolean view = take("view"); if (!view) expect("table"); boolean ifExists = ifExists(); Name table = name();
        if (view && take("owner")) { expect("to"); identifier(); throw new SqlException("0A000", "view ownership is unsupported"); }
        List<AlterAction> actions = new ArrayList<>();
        do {
            if (view && take("owner")) { expect("to"); identifier(); throw new SqlException("0A000", "view ownership is unsupported"); }
            if (view && (is("set") || is("reset"))) {
                boolean reset = take("reset"); if (!reset) expect("set"); if (!reset && take("schema")) { identifier(); throw new SqlException("0A000", "view schema moves are unsupported"); } expect("("); Set<String> seen = new java.util.HashSet<>();
                do {
                    String option = identifier(), value = "true";
                    if (!seen.add(option)) throw new SqlException("22023", "view option specified more than once");
                    if (!reset && take("=")) { String sign = take("-") ? "-" : take("+") ? "+" : ""; Lexer.Token token = tokens.get(at++); if (token.kind() == Lexer.Kind.END || token.kind() == Lexer.Kind.SYMBOL) throw syntax("expected option value"); value = sign + token.text(); }
                    actions.add(new AlterAction(reset ? "reset_option" : "set_option", option, value, null, null, null, null, false));
                } while (take(",")); expect(")");
            } else if (take("add")) {
                take("column");
                if (tableConstraintStart()) actions.add(new AlterAction("add_constraint", null, null, null, null, null, constraint(null), false));
                else { boolean exists = ifNotExists(); actions.add(new AlterAction("add_column", null, null, column(), null, null, null, exists)); }
            } else if (take("drop")) {
                if (take("primary")) { expect("key"); String action = take("cascade") ? "drop_primary_key_cascade" : "drop_primary_key"; take("restrict"); actions.add(new AlterAction(action, null, null, null, null, null, null, false)); continue; }
                boolean constraint = take("constraint"); if (!constraint) take("column");
                boolean exists = ifExists(); String name = identifier();
                String action = constraint ? "drop_constraint" : "drop_column";
                if (take("cascade")) action += "_cascade"; else take("restrict");
                actions.add(new AlterAction(action, name, null, null, null, null, null, exists));
            } else if (take("rename")) {
                if (take("constraint")) { String name = identifier(); expect("to"); actions.add(new AlterAction("rename_constraint", name, identifier(), null, null, null, null, false)); }
                else if (take("to")) actions.add(new AlterAction("rename_table", null, identifier(), null, null, null, null, false));
                else { take("column"); String name = identifier(); expect("to"); actions.add(new AlterAction("rename_column", name, identifier(), null, null, null, null, false)); }
            } else if (take("alter")) {
                take("column"); String name = identifier(); String action; Expr expression = null; TypeName type = null;
                if (take("add")) {
                    expect("generated"); actions.add(new AlterAction("add_identity", name, null, null, null, null, null, false, identity(), List.of(), null)); continue;
                }
                if (is("restart") || is("set") && !ahead(1, "default") && !ahead(1, "not") && !ahead(1, "data")) {
                    List<SequenceOption> options = new ArrayList<>(); Boolean generated = null;
                    while (is("set") || is("restart")) {
                        if (take("set")) {
                            if (take("generated")) {
                                if (generated != null) throw syntax("duplicate GENERATED option");
                                generated = take("always"); if (!generated) { expect("by"); expect("default"); }
                            } else {
                                if (is("as") || is("restart") || is("sequence")) throw syntax("invalid identity option");
                                List<SequenceOption> next = sequenceOptions(true); if (next.isEmpty()) throw syntax("expected identity option"); options.addAll(next);
                            }
                        } else options.addAll(sequenceOptions(true));
                    }
                    Set<String> seen = new java.util.HashSet<>(); for (SequenceOption option : options) if (!seen.add(option.name())) throw syntax("duplicate identity option");
                    actions.add(new AlterAction("set_identity", name, null, null, null, null, null, false, null, options, generated)); continue;
                }
                if (take("set")) {
                    if (take("default")) { action = "set_default"; expression = storedExpression(); }
                    else if (take("data")) { expect("type"); action = "type"; type = typeName(); if (take("using")) expression = expression(0); }
                    else { expect("not"); expect("null"); action = "set_not_null"; }
                } else if (take("drop")) {
                    if (take("identity")) { actions.add(new AlterAction("drop_identity", name, null, null, null, null, null, ifExists())); continue; }
                    if (take("default")) action = "drop_default";
                    else { expect("not"); expect("null"); action = "drop_not_null"; }
                } else {
                    expect("type"); action = "type"; type = typeName();
                    if (take("using")) expression = expression(0);
                }
                actions.add(new AlterAction(action, name, null, null, expression, type, null, false));
            } else throw syntax("expected ALTER TABLE action");
        } while (take(","));
        return new AlterTable(table, ifExists, actions, view);
    }
    private boolean ahead(int distance, String spelling) { return tokens.get(Math.min(at + distance, tokens.size() - 1)).is(spelling); }

    private Expr expression(int minimum) { return expression(minimum, false); }
    private Expr expression(int minimum, boolean stopSimilar) {
        enter();
        try {
            Expr left = primary();
            boolean compared = false;
            while (true) {
                if (80 >= minimum) {
                    if (take("::")) { left = new Cast(left, typeName()); continue; }
                    if (take("collate")) { left = new Collate(left, identifier()); continue; }
                    if (is("[")) {
                        List<Subscript> specs = new ArrayList<>();
                        while (take("[")) {
                            Expr lower = is(":") ? null : expression(0);
                            boolean slice = take(":");
                            Expr upper = slice && !is("]") ? expression(0) : null;
                            expect("]"); specs.add(new Subscript(lower, upper, slice));
                        }
                        left = new Access(left, specs); continue;
                    }
                    if (take(".")) {
                        if (!fieldAccessible.contains(left) && !(left instanceof Field)) throw syntax("field selection requires a parenthesized base");
                        left = new Field(left, take("*") ? null : identifier()); continue;
                    }
                }
                if (55 >= minimum && is("at") && ahead(1, "time") && ahead(2, "zone")) {
                    at += 3; left = new Call("timezone", List.of(expression(56), left)); continue;
                }
                boolean negated = is("not") && (ahead(1, "in") || ahead(1, "between") || ahead(1, "like") || ahead(1, "ilike") || ahead(1, "similar"));
                Lexer.Token operator = tokens.get(at + (negated ? 1 : 0));
                String op = operator.text();
                if (op.equals("similar") && (stopSimilar || !ahead(negated ? 2 : 1, "to"))) break;
                int precedence = precedence(operator);
                if (precedence < minimum) break;
                if (precedence == 35 && compared) throw syntax("comparison operators do not associate");
                if (precedence == 35) compared = true;
                at += negated ? 2 : 1;
                if (op.equals("is")) {
                    boolean not = take("not");
                    if (take("null")) left = new Unary(not ? "is not null" : "is null", left);
                    else if (take("json")) {
                        String kind = "value";
                        if (is("value") || is("scalar") || is("array") || is("object")) kind = tokens.get(at++).text();
                        boolean unique = false;
                        if (is("with") || is("without")) { unique = take("with"); if (!unique) expect("without"); expect("unique"); take("keys"); }
                        left = new Unary("is json:" + kind + ":" + unique + ":" + not, left);
                    }
                    else if (take("distinct")) {
                        expect("from");
                        left = new Binary(not ? "is not distinct from" : "is distinct from", left, expression(precedence + 1));
                    } else if (is("true") || is("false") || is("unknown")) {
                        String truth = tokens.get(at++).text();
                        left = new Unary("is " + (not ? "not " : "") + truth, left);
                    } else throw syntax("expected NULL, truth value, or DISTINCT FROM");
                } else if (op.equals("between")) {
                    Expr lower = expression(precedence + 1); expect("and");
                    left = new Between(left, lower, expression(precedence + 1), negated);
                } else if (op.equals("in")) {
                    expect("(");
                    Statement query = null; List<Expr> values = new ArrayList<>();
                    if (queryStart()) query = readQuery();
                    else do {
                        if (values.size() == 1000) throw new SqlException("54001", "IN list exceeds maximum length");
                        values.add(expression(precedence + 1));
                    } while (take(","));
                    expect(")"); left = new In(left, values, query, negated);
                } else if (op.equals("like") || op.equals("ilike") || op.equals("similar")) {
                    if (op.equals("similar")) expect("to");
                    Expr pattern = expression(precedence + 1);
                    Expr escape = take("escape") ? expression(precedence + 1) : null;
                    if (op.equals("similar")) {
                        pattern = new Call("similar_to_escape", escape == null ? List.of(pattern) : List.of(pattern, escape));
                        left = new Binary(negated ? "not similar" : "similar", left, pattern);
                    } else {
                        if (escape != null) pattern = new Call("like_escape", List.of(pattern, escape));
                        left = new Binary((negated ? "not " : "") + op, left, pattern);
                    }
                } else if (isComparison(op) && (is("any") || is("some") || is("all"))) {
                    String quantifier = tokens.get(at++).text(); expect("(");
                    Expr array = null; Statement query = null;
                    if (queryStart()) query = readQuery(); else array = expression(0);
                    expect(")"); left = new Quantified(op, left, quantifier.equals("some") ? "any" : quantifier, array, query);
                } else left = new Binary(op, left, expression(precedence + 1));
            }
            return left;
        } finally { depth--; }
    }
    private static boolean isComparison(String op) { return switch (op) { case "=", "<>", "!=", "<", ">", "<=", ">=" -> true; default -> false; }; }

    private Expr primary() {
        Lexer.Token token = peek();
        // Qualified names win over keyword interpretations.
        if ((token.kind() == Lexer.Kind.WORD || token.kind() == Lexer.Kind.NAME) && tokens.get(Math.min(at + 1, tokens.size() - 1)).is(".")) {
            List<String> parts = new ArrayList<>();
            parts.add(identifier());
            while (take(".")) {
                if (take("*")) return new Star(parts);
                parts.add(identifier());
            }
            return expressionName(parts);
        }
        if (take("(")) {
            if (expressionQueryStart()) { Expr nested = new Subquery(readQuery()); expect(")"); fieldAccessible.add(nested); return nested; }
            Expr nested = expression(0);
            if (take(",")) {
                List<Expr> values = new ArrayList<>(); values.add(nested);
                do { values.add(expression(0)); } while (take(","));
                expect(")"); return new RowConstructor(values);
            }
            expect(")"); fieldAccessible.add(nested); return nested;
        }
        if (is("exists") && ahead(1, "(")) { at += 2; Statement query = readQuery(); expect(")"); return new Exists(query); }
        if (is("array") && ahead(1, "[")) { at++; return array(); }
        if (is("array") && ahead(1, "(")) { at += 2; Statement query = readQuery(); expect(")"); return new ArrayConstructor(List.of(), query); }
        if (is("row") && ahead(1, "(")) {
            at += 2; List<Expr> values = new ArrayList<>();
            if (!take(")")) { do { values.add(expression(0)); } while (take(",")); expect(")"); }
            return new RowConstructor(values);
        }
        if (take("-") || take("+")) {
            String op = tokens.get(at - 1).text();
            Expr operand = expression(60);
            if (operand instanceof Literal literal && (literal.kind == Lexer.Kind.INTEGER || literal.kind == Lexer.Kind.DECIMAL)) {
                String text = literal.text;
                return new Literal(literal.kind, op.equals("-") ? text.startsWith("-") ? text.substring(1) : "-" + text : text);
            }
            return new Unary(op, operand);
        }
        if (take("not")) return new Unary("not", expression(30));
        if (take("cast")) {
            expect("("); Expr value = expression(0); expect("as"); TypeName type = typeName(); expect(")");
            return new Cast(value, type);
        }
        if (take("case")) {
            Expr input = is("when") ? null : expression(0);
            List<Expr> conditions = new ArrayList<>(), results = new ArrayList<>();
            while (take("when")) { conditions.add(expression(0)); expect("then"); results.add(expression(0)); }
            if (conditions.isEmpty()) throw syntax("CASE requires WHEN");
            Expr otherwise = take("else") ? expression(0) : new Literal(Lexer.Kind.WORD, "null");
            expect("end");
            return new Conditional(input, conditions, results, otherwise);
        }
        if (peek().kind() == Lexer.Kind.WORD && (is("greatest") || is("least") || is("nullif")) && ahead(1, "(")) {
            String kind = tokens.get(at++).text(); expect("("); List<Expr> args = new ArrayList<>();
            do {
                if (is(")") || is("*") || is("distinct") || is("variadic") || ahead(1, "=>")) throw syntax("invalid " + kind + " argument");
                args.add(expression(0));
            } while (take(","));
            expect(")");
            if (is("filter") || is("over")) throw syntax("conditional expression cannot have function modifiers");
            if (kind.equals("nullif") && args.size() != 2) throw syntax("NULLIF requires two arguments");
            return new Extrema(kind, args);
        }
        if (peek().kind() == Lexer.Kind.WORD && List.of("json", "json_scalar", "json_serialize", "json_exists", "json_value", "json_query").contains(peek().text()) && ahead(1, "(")) return jsonSql();
        if (is("position") && ahead(1, "(")) {
            at += 2; Expr needle = expression(36); expect("in"); Expr source = expression(36); expect(")");
            return new Call("position", List.of(source, needle));
        }
        if (is("extract") && ahead(1, "(")) {
            at += 2;
            String unit = peek().kind() == Lexer.Kind.STRING ? tokens.get(at++).text() : identifier();
            expect("from"); Expr source = expression(0); expect(")");
            return new Call("extract", List.of(new Literal(Lexer.Kind.STRING, unit), source));
        }
        if (is("current_timestamp") && !ahead(1, "(")) { at++; return new Call("now", List.of()); }
        if (is("current_date") && !ahead(1, "(")) { at++; return new Call("current_date", List.of()); }
        if (take("default")) return new Default();
        if (take("null") || take("true") || take("false")) return new Literal(Lexer.Kind.WORD, tokens.get(at - 1).text());
        if (token.kind() == Lexer.Kind.INTEGER || token.kind() == Lexer.Kind.DECIMAL || token.kind() == Lexer.Kind.STRING) {
            at++; return new Literal(token.kind(), token.text());
        }
        if (token.kind() == Lexer.Kind.PARAMETER) { at++; return new Parameter(Integer.parseInt(token.text())); }
        if (token.kind() == Lexer.Kind.WORD || token.kind() == Lexer.Kind.NAME) {
            String name = identifier();
            if (token.kind() == Lexer.Kind.WORD && name.equals("timestamp") && (is("with") || is("without"))) {
                boolean with = take("with"); if (!with) expect("without"); expect("time"); expect("zone");
                if (peek().kind() != Lexer.Kind.STRING) throw syntax("timestamp typed literal requires string");
                name = with ? "timestamptz" : "timestamp";
            }
            if (token.kind() == Lexer.Kind.WORD && name.equals("double") && is("precision") && tokens.get(Math.min(at + 1, tokens.size() - 1)).kind() == Lexer.Kind.STRING) { at++; name = "double precision"; }
            else if (token.kind() == Lexer.Kind.WORD && name.equals("character") && is("varying") && tokens.get(Math.min(at + 1, tokens.size() - 1)).kind() == Lexer.Kind.STRING) { at++; name = "character varying"; }
            if (take("(")) {
                List<Expr> args = new ArrayList<>();
                boolean distinct = take("distinct");
                if (distinct && (is(")") || is(","))) throw syntax("DISTINCT requires an aggregate argument");
                if (!distinct && is("all") && !ahead(1, ")") && !ahead(1, ",")) take("all");
                boolean star = take("*");
                if (distinct && star) throw syntax("DISTINCT requires an aggregate argument");
                List<Order> order = new ArrayList<>();
                if (star) expect(")");
                else if (!take(")")) {
                    do {
                        if (args.stream().anyMatch(argument -> argument instanceof Unary unary && unary.op().equals("variadic"))) throw syntax("VARIADIC argument must be last");
                        if (take("variadic")) args.add(new Unary("variadic", expression(0)));
                        else if ((peek().kind() == Lexer.Kind.WORD || peek().kind() == Lexer.Kind.NAME) && ahead(1, "=>")) {
                            String parameter = identifier(); expect("=>");
                            for (Expr argument : args) if (argument instanceof NamedArgument named && named.name().equals(parameter)) throw syntax("duplicate argument name");
                            args.add(new NamedArgument(parameter, expression(0)));
                        } else {
                            if (args.stream().anyMatch(argument -> argument instanceof NamedArgument)) throw syntax("positional argument follows named argument");
                            args.add(expression(0, token.kind() == Lexer.Kind.WORD && name.equals("substring") && args.isEmpty()));
                        }
                        if (token.kind() == Lexer.Kind.WORD && !distinct && args.size() == 1) {
                            if (name.equals("overlay") && take("placing")) {
                                args.add(expression(0)); expect("from"); args.add(expression(0));
                                if (take("for")) args.add(expression(0)); expect(")"); return new Call(name, args);
                            }
                            if (name.equals("substring") && (is("from") || is("for") || is("similar"))) {
                                Expr start, count = null;
                                if (take("from")) { start = expression(0); if (take("for")) count = expression(0); }
                                else if (take("for")) { count = expression(0); start = take("from") ? expression(0) : new Literal(Lexer.Kind.INTEGER, "1"); }
                                else { expect("similar"); start = expression(0); expect("escape"); count = expression(0); }
                                args.add(start); if (count != null) args.add(count); expect(")"); return new Call(name, args);
                            }
                        }
                    } while (take(","));
                    if (take("order")) {
                        expect("by");
                        do {
                            Expr key = expression(0);
                            boolean descending = take("desc"); if (!descending) take("asc");
                            Boolean nullsFirst = null;
                            if (take("nulls")) { if (take("first")) nullsFirst = true; else { expect("last"); nullsFirst = false; } }
                            order.add(new Order(key, descending, nullsFirst));
                        } while (take(","));
                    }
                    expect(")");
                }
                boolean withinGroup = take("within");
                if (withinGroup) {
                    if (distinct || star || !order.isEmpty()) throw syntax("invalid WITHIN GROUP aggregate syntax");
                    expect("group"); expect("("); expect("order"); expect("by");
                    do {
                        Expr key = expression(0); boolean descending = take("desc"); if (!descending) take("asc");
                        Boolean first = null; if (take("nulls")) { if (take("first")) first = true; else { expect("last"); first = false; } }
                        order.add(new Order(key, descending, first));
                    } while (take(","));
                    expect(")");
                }
                Expr filter = null;
                if (take("filter")) { expect("("); expect("where"); filter = expression(0); expect(")"); }
                Call call = new Call(name, args, distinct, star, order, filter, withinGroup);
                if (take("over")) {
                    if (name.equals("grouping")) throw syntax("GROUPING cannot have OVER");
                    if (take("(")) { WindowSpec spec = windowSpec(); expect(")"); return new Window(call, spec, null); }
                    return new Window(call, null, identifier());
                }
                return call;
            }
            if (peek().kind() == Lexer.Kind.STRING) {
                Lexer.Token value = tokens.get(at++);
                TypeName target = new TypeName(name, -1, -1, 0);
                if (name.equals("interval")) target = intervalQualifier(target);
                return new Cast(new Literal(value.kind(), value.text()), target);
            }
            return expressionName(List.of(name));
        }
        throw syntax("expected expression, found " + token.text());
    }

    private Name expressionName(List<String> parts) {
        Name name = new Name(parts); nameTokens.put(name, at - 1); return name;
    }
    static StoredExpression renameColumn(StoredExpression stored, String table, String oldName, String newName, boolean renameTable) {
        StoredDefinitions.require(stored, "stored expression of table \"" + table + "\"");
        SqlParser parser = new SqlParser("SELECT " + stored.text());
        parser.parse();
        List<Lexer.Token> tokens = new ArrayList<>(parser.tokens);
        for (Map.Entry<Name, Integer> entry : parser.nameTokens.entrySet()) {
            List<String> parts = entry.getKey().parts();
            int index = entry.getValue();
            boolean replace = renameTable ? parts.size() == 2 && parts.getFirst().equals(oldName)
                : parts.getLast().equals(oldName) && (parts.size() == 1 || parts.size() == 2 && parts.getFirst().equals(table));
            if (!replace) continue;
            if (renameTable) index -= 2;
            Lexer.Token old = tokens.get(index);
            tokens.set(index, new Lexer.Token(Lexer.Kind.NAME, newName, old.start()));
        }
        String text = SqlTokens.render(tokens.subList(1, tokens.size() - 1));
        return new StoredExpression(text, new SqlParser("SELECT " + text).parse().items().getFirst().expression());
    }

    private ArrayConstructor array() {
        enter();
        try {
            expect("["); List<Expr> values = new ArrayList<>();
            if (!take("]")) {
                do { values.add(is("[") ? array() : expression(0)); } while (take(","));
                expect("]");
            }
            return new ArrayConstructor(values, null);
        } finally { depth--; }
    }

    private TypeName typeName() {
        boolean bare = peek().kind() == Lexer.Kind.WORD;
        String name = identifier();
        if (bare && name.equals("double") && take("precision")) name = "double precision";
        else if (bare && name.equals("character") && take("varying")) name = "character varying";
        int precision = -1, scale = -1;
        if (take("(")) {
            precision = typeNumber();
            if (precision < 0 && List.of("timestamp", "timestamptz", "interval").contains(name)) throw syntax("temporal precision must be non-negative");
            boolean second = take(",");
            if (second && (name.equals("timestamp") || name.equals("timestamptz") || name.equals("interval"))) throw syntax("temporal type accepts only one precision modifier");
            scale = second ? typeNumber() : 0;
            expect(")");
        }
        if (bare && name.equals("timestamp") && (is("with") || is("without"))) {
            boolean with = take("with");
            if (!with) expect("without");
            expect("time"); expect("zone");
            name = with ? "timestamptz" : "timestamp";
        }
        TypeName result = new TypeName(name, precision, scale, 0);
        if (name.equals("interval")) result = intervalQualifier(result);
        int dimensions = 0;
        while (take("[")) { if (!is("]")) typeNumber(); expect("]"); dimensions++; }
        return new TypeName(name, result.precision(), result.scale(), dimensions, result.intervalFields());
    }
    private JsonTable jsonTable() {
        expect("json_table"); expect("("); Expr context = expression(0); expect(","); String path = jsonPathLiteral();
        String pathName = take("as") ? identifier() : null;
        List<JsonPassing> passing = new ArrayList<>();
        if (take("passing")) do {
            Expr value = expression(0); boolean format = false; String encoding = null;
            if (take("format")) { expect("json"); format = true; if (take("encoding")) { encoding = identifier(); if (!List.of("utf8", "utf16", "utf32").contains(encoding)) throw new SqlException("22023", "unknown JSON encoding"); } }
            expect("as"); passing.add(new JsonPassing(identifier(), value, format, encoding));
        } while (take(","));
        java.util.Set<String> names = new java.util.HashSet<>(); if (pathName != null) names.add(pathName);
        List<JsonTableColumn> columns = jsonTableColumns(names);
        if (take("plan")) throw new SqlException("0A000", "explicit JSON_TABLE plan is not supported");
        expect(")"); String alias = sourceAlias(); return new JsonTable(context, path, pathName, passing, columns, alias, alias == null ? List.of() : optionalNames());
    }
    private String jsonPathLiteral() { if (peek().kind() != Lexer.Kind.STRING) throw syntax("JSON path must be a string literal"); return tokens.get(at++).text(); }
    private List<JsonTableColumn> jsonTableColumns(java.util.Set<String> names) {
        enter();
        try {
            expect("columns"); expect("("); List<JsonTableColumn> columns = new ArrayList<>(); boolean ordinal = false;
            do {
                if (take("nested")) {
                    take("path"); String path = jsonPathLiteral(), name = take("as") ? identifier() : null;
                    if (name != null && !names.add(name)) throw new SqlException("42712", "duplicate JSON_TABLE column or path name");
                    columns.add(new JsonTableColumn(null, null, path, name, "nested", false, "without", false, null, null, jsonTableColumns(names))); continue;
                }
                String name = identifier(); if (!names.add(name)) throw new SqlException("42712", "duplicate JSON_TABLE column or path name");
                if (take("for")) {
                    expect("ordinality"); if (ordinal) throw syntax("only one FOR ORDINALITY column is allowed"); ordinal = true;
                    columns.add(new JsonTableColumn(name, new TypeName("i64", -1, 0, 0), null, null, "ordinality", false, "without", false, null, null, List.of())); continue;
                }
                TypeName type = typeName(); boolean format = false;
                if (take("format")) { expect("json"); format = true; }
                String kind = take("exists") ? "exists" : "regular";
                String path = take("path") ? jsonPathLiteral() : "$." + JsonNode.string(name).render();
                String wrapper = "without"; boolean omit = false;
                if (take("with")) { wrapper = take("conditional") ? "conditional" : "unconditional"; if (wrapper.equals("unconditional")) take("unconditional"); take("array"); expect("wrapper"); }
                else if (take("without")) { take("array"); expect("wrapper"); }
                if (is("omit") || is("keep")) { omit = take("omit"); if (!omit) expect("keep"); expect("quotes"); if (take("on")) { expect("scalar"); expect("string"); } }
                JsonBehavior empty = null, error = null;
                while (!is(",") && !is(")")) {
                    String behavior; Expr value = null;
                    if (take("default")) { if (kind.equals("exists")) throw syntax("EXISTS does not accept DEFAULT"); behavior = "default"; value = expression(0); }
                    else if (take("empty")) behavior = take("object") ? "object" : take("array") ? "array" : "array";
                    else if (is("error") || is("null") || is("true") || is("false") || is("unknown")) behavior = tokens.get(at++).text();
                    else throw syntax("expected JSON_TABLE column behavior");
                    expect("on"); if (take("empty")) { if (kind.equals("exists") || empty != null) throw syntax("invalid ON EMPTY clause"); empty = new JsonBehavior(behavior, value); }
                    else { expect("error"); if (error != null) throw syntax("duplicate ON ERROR clause"); error = new JsonBehavior(behavior, value); }
                }
                columns.add(new JsonTableColumn(name, type, path, null, kind, format, wrapper, omit, empty, error, List.of()));
            } while (take(","));
            expect(")"); return columns;
        } finally { depth--; }
    }

    private JsonSql jsonSql() {
        String kind = tokens.get(at++).text(); expect("("); Expr context = expression(0);
        boolean format = false;
        if (take("format")) { expect("json"); format = true; }
        String path = null; ArrayList<JsonPassing> passing = new ArrayList<>();
        boolean query = List.of("json_exists", "json_value", "json_query").contains(kind);
        if (query) {
            expect(","); if (peek().kind() != Lexer.Kind.STRING) throw syntax("JSON path must be a string literal"); path = tokens.get(at++).text();
            if (take("passing")) do {
                Expr value = expression(0); boolean encoded = false; String encoding = null;
                if (take("format")) { expect("json"); encoded = true; if (take("encoding")) { encoding = identifier(); if (!List.of("utf8", "utf16", "utf32").contains(encoding.toLowerCase(java.util.Locale.ROOT))) throw new SqlException("22023", "unknown JSON encoding"); } }
                expect("as"); String name = identifier();
                passing.add(new JsonPassing(name, value, encoded, encoding));
            } while (take(","));
        }
        TypeName returning = null;
        if (take("returning")) {
            if (kind.equals("json_scalar") || kind.equals("json_exists")) throw syntax("RETURNING is not allowed here");
            returning = typeName(); if (take("format")) expect("json");
        }
        boolean unique = false, omit = false; String wrapper = "without";
        if (kind.equals("json")) { if (is("with") || is("without")) { unique = take("with"); if (!unique) expect("without"); expect("unique"); take("keys"); } }
        else if (kind.equals("json_query")) {
            if (take("with")) { wrapper = take("conditional") ? "conditional" : "unconditional"; if (wrapper.equals("unconditional")) take("unconditional"); take("array"); expect("wrapper"); }
            else if (take("without")) { take("array"); expect("wrapper"); }
            if (is("keep") || is("omit")) { omit = take("omit"); if (!omit) expect("keep"); expect("quotes"); if (take("on")) { expect("scalar"); expect("string"); } }
        }
        JsonBehavior empty = null, error = null;
        if (query) while (!is(")")) {
            String behavior; Expr value = null;
            if (take("default")) { if (kind.equals("json_exists")) throw syntax("JSON_EXISTS does not accept DEFAULT"); behavior = "default"; value = expression(0); }
            else if (take("empty")) behavior = take("object") ? "object" : take("array") ? "array" : "array";
            else if (is("error") || is("null") || is("true") || is("false") || is("unknown")) behavior = tokens.get(at++).text();
            else throw syntax("expected JSON behavior");
            expect("on");
            if (take("empty")) { if (kind.equals("json_exists") || empty != null) throw syntax("invalid ON EMPTY clause"); empty = new JsonBehavior(behavior, value); }
            else { expect("error"); if (error != null) throw syntax("duplicate ON ERROR clause"); error = new JsonBehavior(behavior, value); }
        }
        expect(")"); return new JsonSql(kind, context, path, passing, returning, wrapper, omit, empty, error, unique, format);
    }

    private TypeName intervalQualifier(TypeName type) {
        if (!List.of("year", "month", "day", "hour", "minute", "second").stream().anyMatch(this::is)) return type;
        if (type.precision() != -1) throw syntax("interval precision cannot precede a qualifier");
        String name = tokens.get(at++).text();
        if (take("to")) name += " to " + identifier();
        DatetimeUnits.Qualifier qualifier = null;
        for (DatetimeUnits.Qualifier candidate : DatetimeUnits.QUALIFIERS) if (candidate.name().equals(name)) qualifier = candidate;
        if (qualifier == null) throw syntax("invalid interval field qualifier");
        int precision = -1;
        if (take("(")) {
            if (!qualifier.least().equals("second")) throw syntax("precision requires SECOND");
            precision = typeNumber();
            if (precision < 0) throw syntax("interval precision must be non-negative");
            expect(")");
        }
        return new TypeName(type.name(), precision, type.scale(), type.dimensions(), qualifier.code());
    }

    private int typeNumber() {
        boolean negative = take("-");
        if (peek().kind() != Lexer.Kind.INTEGER) throw syntax("expected type modifier");
        try { return Integer.parseInt((negative ? "-" : "") + tokens.get(at++).text()); }
        catch (NumberFormatException e) { throw new SqlException("22003", "type modifier out of range"); }
    }

    private static final Map<String, Integer> PRECEDENCE = operatorPrecedence();
    private static Map<String, Integer> operatorPrecedence() {
        Map<String, Integer> values = new HashMap<>();
        for (OperatorCatalog.Fact fact : OperatorCatalog.FACTS) {
            if (fact.arity() == 2) values.put(fact.symbol().isEmpty() ? fact.name() : fact.symbol(), fact.precedence());
        }
        for (String name : List.of("->", "->>", "#>", "#>>", "?", "?|", "?&", "#-", "@?", "@@")) values.put(name, values.get("||"));
        int compare = values.get("=");
        for (String name : List.of("!=", "is", "in", "between", "similar", "!~", "!~*")) values.put(name, compare);
        return Map.copyOf(values);
    }
    private static int precedence(Lexer.Token token) {
        if (token.kind() != Lexer.Kind.WORD && token.kind() != Lexer.Kind.SYMBOL) return -1;
        return PRECEDENCE.getOrDefault(token.text(), -1);
    }

    private StoredExpression storedExpression() {
        int start = at;
        Expr expression = expression(0);
        return new StoredExpression(SqlTokens.render(tokens.subList(start, at)), expression, start);
    }

    private record DepthNode(Object value, int depth) {}
    private static void validateDepth(Statement statement) {
        ArrayDeque<DepthNode> pending = new ArrayDeque<>();
        pending.add(new DepthNode(statement, 0));
        while (!pending.isEmpty()) {
            DepthNode node = pending.removeLast(); Object value = node.value;
            int depth = node.depth + (value instanceof Expr || value instanceof SetOperation ? 1 : 0);
            if (depth > 256) throw new SqlException("54001", "statement too complex: expression depth exceeds 256");
            if (value instanceof List<?> list) { for (Object child : list) addDepth(pending, child, depth); continue; }
            Object[] children = depthChildren(value);
            if (value instanceof Expr) for (Object child : children) if (child instanceof Star star && !star.qualifier().isEmpty()) throw syntax("qualified star is only allowed as a projection");
            for (Object child : children) addDepth(pending, child, depth);
        }
    }
    static Object[] depthChildren(Object value) {
        return switch (value) {
                case StoredExpression x -> new Object[] {x.expression}; case NamedArgument x -> new Object[] {x.value};
                case Unary x -> new Object[] {x.value}; case Binary x -> new Object[] {x.left, x.right};
                case Cast x -> new Object[] {x.value}; case Collate x -> new Object[] {x.value};
                case Call x -> new Object[] {x.args, x.order, x.filter};
                case Extrema x -> new Object[] {x.args};
                case JsonSql x -> new Object[] {x.context, x.passing, x.onEmpty, x.onError};
                case JsonPassing x -> new Object[] {x.value}; case JsonBehavior x -> new Object[] {x.value};
                case JsonTable x -> new Object[] {x.context, x.passing, x.columns};
                case JsonTableColumn x -> new Object[] {x.onEmpty, x.onError, x.children};
                case Window x -> new Object[] {x.function, x.spec};
                case NamedWindow x -> new Object[] {x.spec};
                case WindowSpec x -> new Object[] {x.partition, x.order, x.frame};
                case WindowFrame x -> new Object[] {x.start, x.end}; case FrameBound x -> new Object[] {x.offset};
                case Conditional x -> new Object[] {x.input, x.conditions, x.results, x.otherwise};
                case Subquery x -> new Object[] {x.query}; case Exists x -> new Object[] {x.query};
                case In x -> new Object[] {x.subject, x.values, x.query};
                case Between x -> new Object[] {x.subject, x.lower, x.upper};
                case Quantified x -> new Object[] {x.subject, x.array, x.query};
                case RowConstructor x -> new Object[] {x.values}; case ArrayConstructor x -> new Object[] {x.values, x.query};
                case Access x -> new Object[] {x.base, x.subscripts}; case Subscript x -> new Object[] {x.lower, x.upper};
                case Field x -> new Object[] {x.base}; case Item x -> new Object[] {x.expression}; case Order x -> new Object[] {x.expression};
                case Select x -> new Object[] {x.items, x.where, x.order, x.limit, x.offset, x.from, x.group, x.having, x.distinctOn, x.windows};
                case Values x -> new Object[] {x.rows, x.order, x.limit, x.offset};
                case Join x -> new Object[] {x.left, x.right, x.on}; case Derived x -> new Object[] {x.query};
                case TableFunction x -> new Object[] {x.call}; case Grouped x -> new Object[] {x.source};
                case Constraint x -> new Object[] {x.expression}; case Column x -> new Object[] {x.constraints};
                case CreateSequence x -> new Object[0];
                case AlterSequence x -> new Object[0];
                case DropSequence x -> new Object[0];
                case CreateView x -> new Object[] {x.query};
                case DropView x -> new Object[0];
                case CreateTable x -> new Object[] {x.columns, x.constraints, x.asQuery};
                case Insert x -> new Object[] {x.values, x.query, x.returning, x.conflict};
                case Conflict x -> new Object[] {x.predicate, x.assignments, x.where}; case Assignment x -> new Object[] {x.expression};
                case Update x -> new Object[] {x.assignments, x.from, x.where, x.returning};
                case Delete x -> new Object[] {x.using, x.where, x.returning};
                case Merge x -> new Object[] {x.source, x.on, x.clauses, x.returning, x.targetWhere};
                case MergeWhen x -> new Object[] {x.condition, x.assignments, x.values};
                case IndexKey x -> new Object[] {x.expression}; case CreateIndex x -> new Object[] {x.keys, x.where};
                case AlterAction x -> new Object[] {x.column, x.expression, x.constraint}; case AlterTable x -> new Object[] {x.actions};
                case Explain x -> new Object[] {x.inner};
                case SetOperation x -> new Object[] {x.left, x.right};
                case QueryTail x -> new Object[] {x.query, x.order, x.limit, x.offset};
                case With x -> new Object[] {x.ctes, x.body};
                case Cte x -> new Object[] {x.body, x.cycle};
                case Cycle x -> new Object[] {x.value, x.otherwise};
                default -> new Object[0];
            };
    }
    private static void addDepth(ArrayDeque<DepthNode> pending, Object child, int depth) { if (child != null) pending.add(new DepthNode(child, depth)); }

    private void enter() {
        if (++depth > 256) throw new SqlException("54001", "statement too complex: nesting depth exceeds the maximum of 256");
    }
    private Lexer.Token peek() { return tokens.get(at); }
    private boolean is(String word) { return peek().is(word); }
    private boolean take(String word) { if (is(word)) { at++; return true; } return false; }
    private void expect(String word) { if (!take(word)) throw syntax("expected " + word + ", found " + peek().text()); }
    private String identifier() {
        Lexer.Token token = peek();
        if (token.kind() != Lexer.Kind.WORD && token.kind() != Lexer.Kind.NAME) throw syntax("expected identifier");
        at++; return token.text();
    }
    private static SqlException syntax(String message) { return new SqlException("42601", message); }
}
