package org.jed;

import java.util.ArrayList;
import java.util.HashSet;
import java.util.IdentityHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;

/** Preserve each FROM binding's old label and visible columns across catalog renames. */
final class ViewRenames {
    private ViewRenames() {}
    static Engine.Snapshot apply(Engine.Snapshot snapshot, String relation, String replacement, String oldColumn, String newColumn, List<String> oldColumns) {
        for (Engine.View view : snapshot.views().values()) {
            if (view.dependencies().stream().noneMatch(d -> (d.kind() == 0 || d.kind() == 1) && d.name().equals(relation))) continue;
            view.query();
            SqlParser parser = new SqlParser(view.definition()); SqlParser.Statement query = parser.parseStatement();
            Map<SqlParser.Table, String> edits = new IdentityHashMap<>();
            collect(query, Set.of(), relation, replacement, oldColumn != null, oldColumns, -1, edits);
            List<Engine.ViewDependency> dependencies = new ArrayList<>();
            for (Engine.ViewDependency dependency : view.dependencies()) {
                if ((dependency.kind() == 0 || dependency.kind() == 1) && dependency.name().equals(relation)) {
                    List<String> columns = dependency.columns().stream().map(c -> c.equals(oldColumn) ? newColumn : c).sorted(SqlText::compare).toList();
                    dependency = new Engine.ViewDependency(dependency.kind(), replacement, columns);
                }
                dependencies.add(dependency);
            }
            dependencies.sort((a,b) -> a.kind() != b.kind() ? Integer.compare(a.kind(),b.kind()) : SqlText.compare(a.name(),b.name()));
            snapshot = snapshot.putView(new Engine.View(view.name(), view.columns(), parser.rewrittenSources(edits), dependencies, view.checkOption(), view.flags()));
        }
        return snapshot;
    }
static Engine.Snapshot dropColumn(Engine.Snapshot snapshot, String relation, int removed) {
    for (Engine.View view : snapshot.views().values()) {
        if (view.dependencies().stream().noneMatch(d -> d.kind() == 0 && d.name().equals(relation))) continue;
        view.query();
            SqlParser parser = new SqlParser(view.definition()); SqlParser.Statement query = parser.parseStatement();
        Map<SqlParser.Table, String> edits = new IdentityHashMap<>();
        collect(query, Set.of(), relation, relation, false, List.of(), removed, edits);
        snapshot = snapshot.putView(new Engine.View(view.name(), view.columns(), parser.rewrittenSources(edits), view.dependencies(), view.checkOption(), view.flags()));
    }
    return snapshot;
}
    private static void collect(Object node, Set<String> ctes, String relation, String replacement, boolean columnsChanged, List<String> columns, int removed, Map<SqlParser.Table, String> edits) {
        if (node == null) return;
        if (node instanceof List<?> list) { for (Object item : list) collect(item, ctes, relation, replacement, columnsChanged, columns, removed, edits); return; }
        if (node instanceof SqlParser.With with) {
            Set<String> scope = new HashSet<>(ctes);
            for (SqlParser.Cte cte : with.ctes()) { if (with.recursive()) scope.add(cte.name()); collect(cte.body(), scope, relation, replacement, columnsChanged, columns, removed, edits); scope.add(cte.name()); }
            collect(with.body(), scope, relation, replacement, columnsChanged, columns, removed, edits); return;
        }
        if (node instanceof SqlParser.Table table) {
            if (!table.name().parts().getLast().equals(relation) || table.name().parts().size() == 1 && ctes.contains(relation)) return;
            String label = table.alias() == null ? relation : table.alias();
            String text = (table.name().parts().size() == 2 ? quote(table.name().parts().getFirst()) + " . " : "") + quote(replacement) + " as " + quote(label);
            List<String> aliases = new ArrayList<>(table.columns());
            if (columnsChanged) for (int i = aliases.size(); i < columns.size(); i++) aliases.add(columns.get(i));
            if (removed >= 0 && removed < aliases.size()) aliases.remove(removed);
            if (!aliases.isEmpty()) text += " ( " + String.join(" , ", aliases.stream().map(ViewRenames::quote).toList()) + " )";
            edits.put(table, text); return;
        }
        for (Object child : SqlParser.depthChildren(node)) collect(child, ctes, relation, replacement, columnsChanged, columns, removed, edits);
    }
    private static String quote(String name) { return "\"" + name.replace("\"", "\"\"") + "\""; }
}
