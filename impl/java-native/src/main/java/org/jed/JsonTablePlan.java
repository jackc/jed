package org.jed;

import java.util.ArrayList;
import java.util.List;

/** Streaming default JSON_TABLE plan: outer parent rows and ordered union of nested siblings. */
final class JsonTablePlan implements RelationPlan {
    private record Column(SqlParser.JsonTableColumn syntax, int index, SqlType type, JsonPath path,
                          JsonSqlFunctions.Behavior empty, JsonSqlFunctions.Behavior error, List<Column> children) {}
    private final SqlParser.JsonTable syntax;
    private final ScalarPlan.Expression context;
    private final List<ScalarPlan.Expression> passing = new ArrayList<>();
    private final List<ScalarPlan.Binding> bindings = new ArrayList<>();
    private final List<Column> columns;
    private final JsonPath root;
    private JsonNode variables;
    private Level cursor;
    private boolean started, closed;

    JsonTablePlan(SqlParser.JsonTable syntax, QueryEnvironment environment) {
        this.syntax = syntax; var resolver = environment.resolver();
        context = resolver.resolve(syntax.context(), SqlType.JSONB);
        if (context.type() != SqlType.JSON && context.type() != SqlType.JSONB && context.type().kind() != SqlType.Kind.TEXT && context.type() != SqlType.UNKNOWN) throw new SqlException("42804", "JSON_TABLE context requires JSON or text");
        for (SqlParser.JsonPassing argument : syntax.passing()) {
            ScalarPlan.Expression value = resolver.resolve(argument.value(), argument.format() ? SqlType.JSONB : null);
            if (!argument.format() && (value.type().kind() == SqlType.Kind.DATE || value.type().kind() == SqlType.Kind.TIMESTAMP || value.type().kind() == SqlType.Kind.TIMESTAMPTZ)) throw new SqlException("0A000", "JSON passing type is not supported");
            if (argument.encoding() != null) { if (value.type() != SqlType.BYTEA) throw new SqlException("42804", "JSON ENCODING requires bytea"); if (!argument.encoding().equalsIgnoreCase("utf8")) throw new SqlException("22023", "JSON encoding conversion is not supported"); }
            passing.add(value);
        }
        root = JsonPath.compile(syntax.path()); columns = resolve(syntax.columns(), resolver);
        if (syntax.aliases().size() > bindings.size()) throw new SqlException("42P10", "too many JSON_TABLE column aliases");
        for (int i = 0; i < syntax.aliases().size(); i++) { var b = bindings.get(i); bindings.set(i, new ScalarPlan.Binding(b.qualifier(), syntax.aliases().get(i), b.type())); }
    }
    private List<Column> resolve(List<SqlParser.JsonTableColumn> syntax, ScalarPlan.Resolver resolver) {
        List<Column> out = new ArrayList<>();
        for (var column : syntax) {
            if (column.kind().equals("nested")) { out.add(new Column(column, -1, null, JsonPath.compile(column.path()), null, null, resolve(column.children(), resolver))); continue; }
            SqlType type = column.kind().equals("ordinality") ? SqlType.I32 : resolver.type(column.type());
            if (type.kind() == SqlType.Kind.ARRAY || type.isComposite()) throw new SqlException("0A000", "structured JSON_TABLE output type is not supported");
            boolean query = type == SqlType.JSON || type == SqlType.JSONB;
            if (column.omitQuotes() || !query && !column.wrapper().equals("without")) throw new SqlException("0A000", "JSON_TABLE column wrapper or quotes mode is not supported");
            if (column.kind().equals("exists") && type != SqlType.BOOLEAN && !type.isInteger()) throw new SqlException("0A000", "EXISTS column requires boolean or integer");
            int index = bindings.size(); bindings.add(new ScalarPlan.Binding(List.of(this.syntax.alias() == null ? "json_table" : this.syntax.alias()), column.name(), type));
            out.add(new Column(column, index, type, column.path() == null ? null : JsonPath.compile(column.path()),
                JsonSqlFunctions.behavior(column.onEmpty(), type, resolver, "null"), JsonSqlFunctions.behavior(column.onError(), type, resolver, column.kind().equals("exists") ? "false" : "null"), List.of()));
        }
        return List.copyOf(out);
    }
    public List<ScalarPlan.Binding> bindings() { return List.copyOf(bindings); }
    public List<String> names() { return bindings.stream().map(ScalarPlan.Binding::name).toList(); }
    public List<SqlType> types() { return bindings.stream().map(ScalarPlan.Binding::type).toList(); }
    public void touch(boolean[] mask) { }
    private List<JsonNode> evaluate(JsonPath path, String source, JsonNode item, boolean exists, CostMeter meter) {
        return JsonPathEval.evaluateTable(path, source, item, variables, exists, meter, passing.size() + 1, syntax.passing().stream().map(SqlParser.JsonPassing::name).toList()).items();
    }
    private List<JsonNode> sequence(JsonPath path, String source, JsonNode item, CostMeter meter) {
        try { return evaluate(path, source, item, false, meter); }
        catch (SqlException failure) { if (!failure.sqlState().startsWith("22")) throw failure; return List.of(); }
    }
    public Row next(CostMeter meter) {
        if (closed) return null;
        if (!started) {
            started = true; List<JsonNode.Member> vars = new ArrayList<>();
            for (int i = 0; i < passing.size(); i++) {
                SqlValue value = passing.get(i).run(meter); var argument = syntax.passing().get(i);
                JsonNode image = JsonSqlFunctions.passing(value, argument.format(), meter);
                if (vars.stream().noneMatch(member -> member.key().equals(argument.name()))) vars.add(new JsonNode.Member(argument.name(), image));
            }
            variables = JsonNode.object(vars).canonical(); SqlValue document = context.run(meter);
            if (document.isNull()) return null;
            cursor = new Level(columns, sequence(root, root.toString(), JsonSqlFunctions.document(document), meter), null, meter);
        }
        if (cursor == null) return null;
        List<SqlValue> row = cursor.next(); if (row == null) { close(); return null; }
        meter.charge(Costs.GENERATED_ROW); meter.guard(); return new Row(names(), row);
    }
    private final class Level {
        final List<Column> columns;
        final List<JsonNode> items;
        final List<SqlValue> parent;
        final CostMeter meter;
        int itemIndex, childIndex;
        List<SqlValue> base;
        JsonNode item;
        Level child;
        boolean emitted;
        Level(List<Column> columns, List<JsonNode> items, List<SqlValue> parent, CostMeter meter) { this.columns = columns; this.items = items; this.parent = parent; this.meter = meter; }
        List<SqlValue> next() {
            while (true) {
                meter.guard();
                if (base == null) {
                    if (itemIndex == items.size()) return null;
                    item = items.get(itemIndex++); base = parent == null ? new ArrayList<>(types().stream().map(SqlValue::nullValue).toList()) : new ArrayList<>(parent);
                    for (Column column : columns) if (!column.syntax.kind().equals("nested")) base.set(column.index, column.syntax.kind().equals("ordinality") ? SqlValue.ofInteger(SqlType.I32, itemIndex) : value(column, item, meter));
                    childIndex = 0; emitted = false;
                }
                if (child != null) { List<SqlValue> row = child.next(); if (row != null) { emitted = true; return row; } child = null; }
                while (childIndex < columns.size() && !columns.get(childIndex).syntax.kind().equals("nested")) childIndex++;
                if (childIndex < columns.size()) {
                    Column column = columns.get(childIndex++);
                    child = new Level(column.children, sequence(column.path, column.path.toString(), item, meter), base, meter); continue;
                }
                List<SqlValue> row = base; base = null;
                if (!emitted) return row;
            }
        }
    }
    private SqlValue value(Column column, JsonNode item, CostMeter meter) {
        List<JsonNode> items;
        boolean exists = column.syntax.kind().equals("exists");
        try { items = evaluate(column.path, column.path.toString(), item, exists, meter); }
        catch (SqlException failure) { if (!failure.sqlState().startsWith("22")) throw failure; return JsonSqlFunctions.executeBehavior(column.error, column.type, meter, failure); }
        if (exists) return column.type == SqlType.BOOLEAN ? SqlValue.ofBoolean(!items.isEmpty()) : SqlValue.ofInteger(column.type, items.isEmpty() ? 0 : 1);
        if (items.isEmpty()) return JsonSqlFunctions.executeBehavior(column.empty, column.type, meter, new SqlException("22035", "JSON path yielded no item"));
        try {
            if (column.type != SqlType.JSON && column.type != SqlType.JSONB) {
                if (items.size() != 1) throw new SqlException("22034", "JSON path yielded multiple items");
                return JsonSqlFunctions.scalar(items.getFirst(), column.type, meter);
            }
            boolean wrap = column.syntax.wrapper().equals("unconditional") || column.syntax.wrapper().equals("conditional") && items.size() != 1;
            if (!wrap && items.size() != 1) throw new SqlException("22034", "JSON path yielded multiple items");
            SqlValue value = JsonSqlFunctions.image(wrap ? JsonNode.array(items) : items.getFirst(), column.type, meter);
            if (wrap) meter.scalarInput(MemoryAccount.payload(value)); return value;
        } catch (SqlException failure) { if (!failure.sqlState().startsWith("22")) throw failure; return JsonSqlFunctions.executeBehavior(column.error, column.type, meter, failure); }
    }
    public void close() { closed = true; cursor = null; variables = null; }
}
