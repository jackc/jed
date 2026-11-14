package org.jed;

import java.util.ArrayList;
import java.util.Comparator;
import java.util.Iterator;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.stream.Stream;

/** Read-only catalog rows generated from a pinned database domain, without storage reads. */
final class CatalogRelation implements RelationPlan {
    private static final Set<String> NAMES = Set.of("jed_tables", "jed_columns", "jed_indexes", "jed_constraints", "jed_sequences", "jed_types", "jed_type_fields", "jed_views", "jed_statistics");
    private static final SqlType TEXTS = SqlType.array(SqlType.TEXT), BOOLS = SqlType.array(SqlType.BOOLEAN);
    private record Field(String name, SqlType type, boolean required) {}
    private final String name, domain;
    private final Engine.Snapshot snapshot;
    private final QueryEnvironment environment;
    private final List<ScalarPlan.Binding> bindings;
    private Iterator<List<SqlValue>> rows;
    private boolean closed;
    static boolean recognized(SqlParser.Name name) { return NAMES.contains(name.parts().getLast()); }
    CatalogRelation(SqlParser.Table syntax, QueryEnvironment environment) {
        this.environment = environment; name = syntax.name().parts().getLast();
        if (syntax.name().parts().size() > 2) throw new SqlException("42P01", "invalid catalog database qualifier");
        domain = syntax.name().parts().size() == 1 ? "main" : syntax.name().parts().getFirst();
        snapshot = environment.readRoot.domain(domain);
        List<Field> fields = fields(name);
        if (syntax.columns().size() > fields.size()) throw new SqlException("42P10", "too many catalog column aliases");
        String alias = syntax.alias() == null ? name : syntax.alias(); List<ScalarPlan.Binding> bound = new ArrayList<>();
        for (int i = 0; i < fields.size(); i++) { Field f = fields.get(i); bound.add(new ScalarPlan.Binding(List.of(alias), i < syntax.columns().size() ? syntax.columns().get(i) : f.name(), f.type()).catalog(f.name(), f.required())); }
        bindings = List.copyOf(bound);
    }
    public List<ScalarPlan.Binding> bindings() { return bindings; }
    public List<String> names() { return bindings.stream().map(ScalarPlan.Binding::name).toList(); }
    public List<SqlType> types() { return bindings.stream().map(ScalarPlan.Binding::type).toList(); }
    public void touch(boolean[] mask) { }
    public Row next(CostMeter meter) {
        if (closed) return null;
        if (rows == null) rows = rows().iterator();
        if (!rows.hasNext()) return null;
        meter.charge(Costs.GENERATED_ROW); meter.guard(); return new Row(names(), rows.next());
    }
    public PlanNode explain(boolean verbose) {
        long count = switch (name) {
            case "jed_tables" -> snapshot.tables().size();
            case "jed_columns" -> tables().mapToLong(t -> t.columns().size()).sum() + (viewsVisible() ? views().mapToLong(v -> v.columns().size()).sum() : 0);
            case "jed_indexes" -> tables().mapToLong(t -> t.indexes().size()).sum();
            case "jed_constraints" -> tables().mapToLong(t -> t.checks().size() + t.indexes().stream().filter(i -> i.unique() && i.constraint()).count() + t.foreignKeys().size() + t.exclusions().size()).sum();
            case "jed_sequences" -> snapshot.sequences().size();
            case "jed_types" -> snapshot.types().size();
            case "jed_type_fields" -> typesByName().mapToLong(t -> t.fields().size()).sum();
            case "jed_views" -> snapshot.views().size();
            case "jed_statistics" -> tables().mapToLong(table -> table.statistics().size()).sum();
            default -> throw new IllegalStateException(name);
        };
        return new PlanNode("Catalog Scan " + name, "db=" + domain, count, PlanEstimate.mul(count, Costs.GENERATED_ROW), List.of(), "buffered");
    }
    public void close() { closed = true; rows = null; }
    private Stream<Engine.Table> tables() { return sorted(snapshot.tables()); }
    private Stream<Engine.View> views() { return sorted(snapshot.views()); }
    private Stream<SqlType> typesByName() { return sorted(snapshot.types()); }
    private static <T> Stream<T> sorted(Map<String,T> map) { return map.entrySet().stream().sorted((a,b) -> SqlText.compare(a.getKey(), b.getKey())).map(Map.Entry::getValue); }
    private boolean viewsVisible() { return domain.equals("main") || domain.equals("temp"); }
    private Stream<List<SqlValue>> rows() {
        return switch (name) {
            case "jed_tables" -> tables().map(table -> List.of(text(table.name())));
            case "jed_columns" -> Stream.concat(tables().flatMap(this::columns), viewsVisible() ? views().flatMap(this::viewColumns) : Stream.empty());
            case "jed_indexes" -> tables().flatMap(table -> table.indexes().stream().sorted(Comparator.comparing(Engine.Index::name, SqlText::compare)).map(index -> List.of(text(index.name()), text(table.name()), strings(index.keys().stream().map(key -> key.column() < 0 ? keyText(key.expression().text()) : table.columns().get(key.column()).name()).toList()), bool(index.unique()), text(switch(index.kind()) { case 0 -> "btree"; case 1 -> "gin"; case 2 -> "gist"; default -> throw new IllegalStateException("index method"); }), text(index.predicate() == null ? null : index.predicate().text()), booleans(index.keys().stream().map(Engine.IndexKey::descending).toList()), booleans(index.keys().stream().map(Engine.IndexKey::nullsFirst).toList()))));
            case "jed_constraints" -> tables().flatMap(this::constraints);
            case "jed_sequences" -> sorted(snapshot.sequences()).map(sequence -> List.of(text(sequence.name()), integer(sequence.start()), integer(sequence.minimum()), integer(sequence.maximum()), integer(sequence.increment()), bool(sequence.cycle()), integer(sequence.cache()), sequence.called() ? integer(sequence.last()) : SqlValue.nullValue(SqlType.I64), text(sequence.owner()), text(sequence.owner() == null ? null : snapshot.tables().get(sequence.owner()).columns().get(sequence.ownerColumn()).name())));
            case "jed_types" -> typesByName().map(type -> List.of(text(type.name()), text("composite")));
            case "jed_type_fields" -> typesByName().flatMap(type -> java.util.stream.IntStream.range(0, type.fields().size()).mapToObj(i -> { SqlType.Field field = type.fields().get(i); return List.of(text(type.name()), text(field.name()), ordinal(i + 1), text(field.type().toString()), bool(field.notNull())); }));
            case "jed_views" -> views().map(view -> List.of(text(view.name()), text(view.definition()), strings(view.columns().stream().map(Engine.ViewColumn::name).toList()), text(switch(view.checkOption()) { case 0 -> "NONE"; case 1 -> "LOCAL"; case 2 -> "CASCADED"; default -> throw new IllegalStateException("view check option"); }), bool((view.flags() & 1) != 0), bool((view.flags() & 2) != 0)));
            case "jed_statistics" -> tables().flatMap(table -> table.statistics().entrySet().stream().map(entry -> {
                ColumnStatistics value = entry.getValue(); return List.of(text(table.name()), text(table.columns().get(entry.getKey()).name()), integer(value.analyzedRows()), bool(value.stale()), integer(value.nullCount()), value.distinctCount() == null ? SqlValue.nullValue(SqlType.I64) : integer(value.distinctCount()), integer(value.sampleRows()), value.averageWidth() == null ? SqlValue.nullValue(SqlType.I64) : integer(value.averageWidth()), ordinal(value.mcv().size()), ordinal(value.histogram().size()), value.terms() == null ? SqlValue.nullValue(SqlType.I32) : ordinal(value.terms().entries().size()));
            }));
            default -> throw new IllegalStateException(name);
        };
    }
    private Stream<List<SqlValue>> columns(Engine.Table table) {
        return java.util.stream.IntStream.range(0, table.columns().size()).mapToObj(i -> {
            Engine.Column column = table.columns().get(i); int key = -1;
            for (int k = 0; k < table.primaryKey().length; k++) if (table.primaryKey()[k] == i) key = k;
            return List.of(text(table.name()), text(column.name()), ordinal(i + 1), text(column.type().toString()), bool(column.notNull()), key < 0 ? SqlValue.nullValue(SqlType.I32) : ordinal(key + 1), text(defaultText(column)), text(column.identity() == 2 ? "ALWAYS" : column.identity() == 1 ? "BY DEFAULT" : null));
        });
    }
    private Stream<List<SqlValue>> viewColumns(Engine.View view) {
        List<SqlType> types;
        try (QueryPlan plan = environment.viewScope(view.name()).plan(view.query())) { types = plan.types(); }
        return java.util.stream.IntStream.range(0, view.columns().size()).mapToObj(i -> { Engine.ViewColumn column = view.columns().get(i); return List.of(text(view.name()), text(column.name()), ordinal(i + 1), text(types.get(i) == SqlType.UNKNOWN ? "text" : types.get(i).toString()), bool(false), SqlValue.nullValue(SqlType.I32), text(column.defaultText()), text(null)); });
    }
    private Stream<List<SqlValue>> constraints(Engine.Table table) {
        Stream<List<SqlValue>> checks = table.checks().stream().sorted(Comparator.comparing(Engine.Check::name, SqlText::compare)).map(check -> constraint(check.name(), table.name(), "check", null, check.expression().text(), null, null, null, null));
        Stream<List<SqlValue>> unique = table.indexes().stream().filter(i -> i.unique() && i.constraint()).sorted(Comparator.comparing(Engine.Index::name, SqlText::compare)).map(index -> constraint(index.name(), table.name(), "unique", index.keys().stream().map(key -> table.columns().get(key.column()).name()).toList(), null, null, null, null, null));
        Stream<List<SqlValue>> foreign = table.foreignKeys().stream().sorted(Comparator.comparing(Engine.ForeignKey::name, SqlText::compare)).map(key -> constraint(key.name(), table.name(), "foreign_key", key.local().stream().map(i -> table.columns().get(i).name()).toList(), null, key.parent(), key.referenced().stream().map(i -> snapshot.tables().get(key.parent()).columns().get(i).name()).toList(), null, key.matchFull() ? "full" : "simple"));
        Stream<List<SqlValue>> exclude = table.exclusions().stream().sorted(Comparator.comparing(Engine.Exclusion::name, SqlText::compare)).map(key -> { Engine.Index index = table.indexes().stream().filter(i -> i.name().equals(key.index())).findFirst().orElseThrow(); return constraint(key.name(), table.name(), "exclude", key.columns().stream().map(i -> table.columns().get(i).name()).toList(), index.predicate() == null ? null : index.predicate().text(), null, null, key.operators(), null); });
        return Stream.concat(Stream.concat(checks, unique), Stream.concat(foreign, exclude));
    }
    private static List<SqlValue> constraint(String name, String table, String kind, List<String> columns, String expression, String parent, List<String> referenced, List<String> operators, String match) {
        return List.of(text(name), text(table), text(kind), strings(columns), text(expression), text(parent), strings(referenced), strings(operators), text(match));
    }
    private String defaultText(Engine.Column column) {
        SqlParser.Expr expression = column.defaultValue(); if (expression == null) return null;
        if (expression instanceof SqlParser.StoredExpression stored) return stored.text();
        SqlValue value = environment.meter().unmetered(() -> expression instanceof SqlParser.StoredValue stored ? stored.value() : environment.resolver().resolve(expression, column.type()).run(environment.meter()));
        if (value.isNull()) return null;
        value = SqlCasts.cast(value, column.type()); String text = Temporal.renderValue(value, Environment.empty().zone("UTC"));
        return value.type().isInteger() || value.type().kind() == SqlType.Kind.DECIMAL || value.type() == SqlType.BOOLEAN ? text : "'" + text.replace("'", "''") + "'";
    }
    private static String keyText(String text) {
        List<Lexer.Token> tokens = new Lexer(text).scan();
        while (tokens.size() > 2 && tokens.getFirst().text().equals("(") && tokens.get(tokens.size() - 2).text().equals(")")) {
            int depth = 0; boolean wraps = true;
            for (int i = 0; i < tokens.size() - 2; i++) { String token = tokens.get(i).text(); if (token.equals("(")) depth++; else if (token.equals(")")) depth--; if (depth == 0) { wraps = false; break; } }
            if (!wraps) break;
            text = SqlTokens.render(tokens.subList(1, tokens.size() - 2)); tokens = new Lexer(text).scan();
        }
        return text;
    }
    private static SqlValue text(String value) { return value == null ? SqlValue.nullValue(SqlType.TEXT) : SqlValue.ofText(value); }
    private static SqlValue bool(boolean value) { return SqlValue.ofBoolean(value); }
    private static SqlValue ordinal(int value) { return SqlValue.ofInteger(SqlType.I32, value); }
    private static SqlValue integer(long value) { return SqlValue.ofInteger(SqlType.I64, value); }
    private static SqlValue strings(List<String> values) { return values == null ? SqlValue.nullValue(TEXTS) : SqlValue.ofArray(TEXTS, SqlArray.vector(values.stream().map(CatalogRelation::text).toList())); }
    private static SqlValue booleans(List<Boolean> values) { return SqlValue.ofArray(BOOLS, SqlArray.vector(values.stream().map(CatalogRelation::bool).toList())); }
    private static List<Field> fields(String name) {
        String[] names; SqlType[] types; Set<String> nullable;
        switch (name) {
            case "jed_tables" -> { names = new String[]{"name"}; types = new SqlType[]{SqlType.TEXT}; nullable = Set.of(); }
            case "jed_columns" -> { names = new String[]{"table_name","name","ordinal","type","not_null","pk_ordinal","column_default","identity_generation"}; types = new SqlType[]{SqlType.TEXT,SqlType.TEXT,SqlType.I32,SqlType.TEXT,SqlType.BOOLEAN,SqlType.I32,SqlType.TEXT,SqlType.TEXT}; nullable = Set.of("pk_ordinal","column_default","identity_generation"); }
            case "jed_indexes" -> { names = new String[]{"name","table_name","columns","is_unique","method","predicate","descending","nulls_first"}; types = new SqlType[]{SqlType.TEXT,SqlType.TEXT,TEXTS,SqlType.BOOLEAN,SqlType.TEXT,SqlType.TEXT,BOOLS,BOOLS}; nullable = Set.of("predicate"); }
            case "jed_constraints" -> { names = new String[]{"name","table_name","type","columns","expression","ref_table","ref_columns","operators","match_type"}; types = new SqlType[]{SqlType.TEXT,SqlType.TEXT,SqlType.TEXT,TEXTS,SqlType.TEXT,SqlType.TEXT,TEXTS,TEXTS,SqlType.TEXT}; nullable = Set.of("columns","expression","ref_table","ref_columns","operators","match_type"); }
            case "jed_sequences" -> { names = new String[]{"name","start_value","min_value","max_value","increment_by","cycle","cache_size","last_value","owned_by_table","owned_by_column"}; types = new SqlType[]{SqlType.TEXT,SqlType.I64,SqlType.I64,SqlType.I64,SqlType.I64,SqlType.BOOLEAN,SqlType.I64,SqlType.I64,SqlType.TEXT,SqlType.TEXT}; nullable = Set.of("last_value","owned_by_table","owned_by_column"); }
            case "jed_types" -> { names = new String[]{"name","kind"}; types = new SqlType[]{SqlType.TEXT,SqlType.TEXT}; nullable = Set.of(); }
            case "jed_type_fields" -> { names = new String[]{"type_name","name","ordinal","type","not_null"}; types = new SqlType[]{SqlType.TEXT,SqlType.TEXT,SqlType.I32,SqlType.TEXT,SqlType.BOOLEAN}; nullable = Set.of(); }
            case "jed_views" -> { names = new String[]{"name","definition","columns","check_option","security_barrier","security_invoker"}; types = new SqlType[]{SqlType.TEXT,SqlType.TEXT,TEXTS,SqlType.TEXT,SqlType.BOOLEAN,SqlType.BOOLEAN}; nullable = Set.of(); }
            case "jed_statistics" -> { names = new String[]{"table_name","column_name","analyzed_rows","is_stale","null_count","distinct_count","sample_rows","average_width","mcv_count","histogram_count","term_count"}; types = new SqlType[]{SqlType.TEXT,SqlType.TEXT,SqlType.I64,SqlType.BOOLEAN,SqlType.I64,SqlType.I64,SqlType.I64,SqlType.I64,SqlType.I32,SqlType.I32,SqlType.I32}; nullable = Set.of("distinct_count","average_width","term_count"); }
            default -> throw new IllegalStateException(name);
        }
        List<Field> result = new ArrayList<>(); for (int i = 0; i < names.length; i++) result.add(new Field(names[i], types[i], !nullable.contains(names[i]))); return List.copyOf(result);
    }
}
