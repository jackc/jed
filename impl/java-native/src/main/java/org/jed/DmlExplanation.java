package org.jed;

import java.util.ArrayList;
import java.util.HashSet;
import java.util.List;
import java.util.function.Function;

/** Resolve write expressions and expose the target/source plan without staging writes. */
final class DmlExplanation implements QueryPlan {
    private final Engine.Table table;
    private final SqlParser.Statement statement;
    private final QueryEnvironment environment;
    private final ScalarPlan.Resolver resolver;
    private final QueryPlan source, returning;
    private final String alias;
    private final SqlParser.Expr predicate;
    DmlExplanation(SqlParser.Statement statement, Engine.Table table, SqlValue[] args, QueryEnvironment environment, Function<SqlParser.Statement,QueryPlan> reader) {
        this.table = table; this.statement = statement; this.environment = environment;
        String oldAlias, newAlias; List<SqlParser.Item> items; boolean old;
        if (statement instanceof SqlParser.Insert insert) { alias = insert.alias(); predicate = null; items = insert.returning(); oldAlias = insert.oldAlias(); newAlias = insert.newAlias(); old = false; }
        else if (statement instanceof SqlParser.Update update) { alias = update.alias(); predicate = update.where(); items = update.returning(); oldAlias = update.oldAlias(); newAlias = update.newAlias(); old = false; if (!update.from().isEmpty()) throw new SqlException("0A000", "UPDATE FROM is unsupported"); }
        else { SqlParser.Delete delete = (SqlParser.Delete)statement; alias = delete.alias(); predicate = delete.where(); items = delete.returning(); oldAlias = delete.oldAlias(); newAlias = delete.newAlias(); old = true; if (!delete.using().isEmpty()) throw new SqlException("0A000", "DELETE USING is unsupported"); }
        resolver = environment.resolver(table.bindings(alias), new ScalarPlan.Frame());
        if (predicate != null) resolver.predicate(predicate);
        QueryPlan read = null, output = null;
        try {
            if (statement instanceof SqlParser.Update update) {
                HashSet<String> seen = new HashSet<>();
                for (SqlParser.Assignment assignment : update.assignments()) {
                    if (!seen.add(assignment.column())) throw new SqlException("42701", "column assigned more than once");
                    Engine.Column column = column(assignment.column());
                    if (!(assignment.expression() instanceof SqlParser.Default)) Engine.assignment(resolver, assignment.expression(), column.type());
                }
            }
            if (statement instanceof SqlParser.Insert insert) {
                List<Engine.Column> columns = insert.columns().isEmpty() ? table.columns() : insert.columns().stream().map(this::column).toList();
                if (new HashSet<>(insert.columns()).size() != insert.columns().size()) throw new SqlException("42701", "duplicate insert column");
                if (insert.query() != null) {
                    read = reader.apply(insert.query()); if (read.types().size() != columns.size()) throw new SqlException("42601", "insert source column count mismatch");
                } else for (List<SqlParser.Expr> row : insert.values()) {
                    if (!row.isEmpty() && row.size() != columns.size()) throw new SqlException("42601", "insert values column count mismatch");
                    for (int i = 0; i < row.size(); i++) if (!(row.get(i) instanceof SqlParser.Default)) Engine.assignment(environment.resolver(), row.get(i), columns.get(i).type());
                }
            }
            try (Returning projection = new Returning(table, alias, items, oldAlias, newAlias, old, args, environment)) {
                boolean[] assigned = null;
                if (statement instanceof SqlParser.Update update) {
                    assigned = new boolean[table.columns().size()];
                    for (SqlParser.Assignment assignment : update.assignments()) for (int c = 0; c < assigned.length; c++) if (table.columns().get(c).name().equals(assignment.column())) assigned[c] = true;
                }
                boolean[] readTouched = projection.readTouched(assigned);
                for (int c = 0; c < readTouched.length; c++) if (readTouched[c]) resolver.resolve(new SqlParser.ColumnSlot(c), null);
                output = projection.finish(environment.meter());
            }
            source = read; returning = output;
        } catch (RuntimeException | Error failure) { if (read != null) read.close(); if (output != null) output.close(); throw failure; }
    }
    private Engine.Column column(String name) { for (Engine.Column column : table.columns()) if (column.name().equals(name)) return column; throw new SqlException("42703", "column does not exist: " + name); }
    public List<String> names() { return returning.names(); }
    public List<SqlType> types() { return returning.types(); }
    public Row next(CostMeter meter) { throw new IllegalStateException("metadata-only write cannot execute"); }
    public PlanNode explain(boolean verbose) {
        PlanNode child; String kind, detail = "";
        if (statement instanceof SqlParser.Insert insert) {
            kind = "Insert"; if (insert.conflict() != null) detail = "on conflict do " + (insert.conflict().nothing() ? "nothing" : "update");
            if (source != null) child = source.explain(verbose);
            else { long rows = insert.values().size(); child = new PlanNode("Values", "rows=" + rows, rows, 0, List.of(), "buffered"); }
        } else {
            kind = statement instanceof SqlParser.Update ? "Update" : "Delete";
            if (statement instanceof SqlParser.Update update) detail = "sets=" + update.assignments().size();
            var query = new SqlParser.Select(List.of(), predicate, List.of(), null, null, false);
            child = TableExplanation.describe(query, table, table.columns().stream().map(Engine.Column::name).toList(), alias == null ? table.name() : alias, List.of(), table.bindings(alias), resolver, environment.meter(), false, false, false, verbose, false, true, null, null, DomainRouting.hostAttachment(DomainRouting.target(statement)));
        }
        return new PlanNode(kind + " " + table.name(), detail, child.rows(), child.cost(), List.of(child), "buffered");
    }
    public void requireAnalyze() { requireAnalyzedWrite(); }
    void requireAnalyzedWrite() { if (source != null) source.requireAnalyze(); }
    PlanNode actualWrite(boolean verbose, PlanMetrics metrics, PlanNode sourceActual) {
        PlanNode node = explain(verbose);
        if (sourceActual == null) return metrics.render(node);
        return new PlanNode(node.label(), node.detail(), node.rows(), metrics.total(), List.of(sourceActual), node.lane());
    }
    public void close() { if (source != null) source.close(); returning.close(); }
}
