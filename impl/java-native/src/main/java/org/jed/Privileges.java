package org.jed;

import java.util.ArrayList;
import java.util.EnumSet;
import java.util.HashMap;
import java.util.HashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;

/** Session envelope and exhaustive, side-effect-free authorization preflight. */
final class Privileges {
    private Set<Privilege> defaults = EnumSet.of(Privilege.SELECT, Privilege.INSERT, Privilege.UPDATE, Privilege.DELETE);
    private final Map<String, Set<Privilege>> grants = new HashMap<>(), revokes = new HashMap<>();
    boolean allowDdl = true;
    Boolean allowTempDdl;
    void defaults(Set<Privilege> value) { if (value.contains(Privilege.EXECUTE)) throw new SqlException("22023", "default privileges are table privileges"); defaults = Set.copyOf(value); }
    Set<Privilege> defaults() { return defaults; }
    void change(String name, boolean deny, Privilege... values) {
        SqlText.encode(name); (deny ? revokes : grants).computeIfAbsent(name, ignored -> EnumSet.noneOf(Privilege.class)).addAll(List.of(values));
    }
    void clear() { grants.clear(); revokes.clear(); }
    void require(String object, Privilege operation) {
        if (revokes.getOrDefault(object, Set.of()).contains(operation)
                || operation != Privilege.EXECUTE && !defaults.contains(operation) && !grants.getOrDefault(object, Set.of()).contains(operation))
            throw new SqlException("42501", "permission denied for " + (operation == Privilege.EXECUTE ? "function " : "table ") + object);
    }
    void check(SqlParser.Statement statement, Engine.Snapshot snapshot, QueryEnvironment environment) {
        if (defaults.size() == 4 && revokes.isEmpty() && allowDdl && !Boolean.FALSE.equals(allowTempDdl)) return;
        new Preflight(snapshot, environment).walk(statement, Set.of(), true);
    }
    private final class Preflight {
        final Engine.Snapshot snapshot;
        final QueryEnvironment environment;
        final Set<String> views = new HashSet<>();
        Preflight(Engine.Snapshot snapshot, QueryEnvironment environment) { this.snapshot = snapshot; this.environment = environment; }
        void relation(SqlParser.Name name, Privilege operation, boolean tables) {
            String key = Engine.name(name);
            Engine.Snapshot domain = DomainRouting.readDomain(snapshot, name);
            if (tables && (CatalogRelation.recognized(name) || domain.tables().containsKey(key) || domain.views().containsKey(key))) require(key, operation);
            Engine.View view = domain.views().get(key);
            if (tables && view != null && (view.flags() & 2) != 0 && operation != Privilege.SELECT && ViewWrites.shape(view.query(), false)) {
                SqlParser.Select select = (SqlParser.Select)view.query(); SqlParser.Name base = ((SqlParser.Table)select.from().getFirst()).name();
                relation(base, operation, true); if (operation != Privilege.INSERT && reads(select.where())) relation(base, Privilege.SELECT, true);
            }
            if (view != null && operation == Privilege.SELECT && views.add(key)) { walk(view.query(), Set.of(), (view.flags() & 2) != 0); views.remove(key); }
        }
        void ddl(boolean temporary) { if (!(temporary ? allowTempDdl == null ? allowDdl : allowTempDdl : allowDdl)) throw new SqlException("42501", "DDL is not permitted in this session"); }
        boolean temp(SqlParser.Name name) { return DomainRouting.scope(snapshot, name, false).equals("temp"); }
        void walk(Object node, Set<String> ctes, boolean tables) {
            if (node == null) return;
            if (node instanceof List<?> list) { for (Object item : list) walk(item, ctes, tables); return; }
            if (node instanceof SqlParser.With with) {
                Set<String> local = new HashSet<>(ctes);
                for (SqlParser.Cte cte : with.ctes()) { if (with.recursive()) local.add(cte.name()); walk(cte.body(), local, tables); local.add(cte.name()); }
                walk(with.body(), local, tables); return;
            }
            if (node instanceof SqlParser.Table source) { if (!(source.name().parts().size() == 1 && ctes.contains(source.name().parts().getFirst()))) relation(source.name(), Privilege.SELECT, tables); return; }
            if (node instanceof SqlParser.Call call && !Set.of("coalesce", "nullif").contains(call.name())) require(call.name(), Privilege.EXECUTE);
            if (node instanceof SqlParser.Analyze analyze) { ddl(false); relation(analyze.table(), Privilege.SELECT, tables); return; }
            if (node instanceof SqlParser.Insert insert) {
                relation(insert.table(), Privilege.INSERT, tables);
                boolean read = reads(insert.returning());
                if (insert.conflict() != null) {
                    SqlParser.Conflict conflict = insert.conflict(); read |= !conflict.columns().isEmpty() || conflict.constraint() != null || reads(conflict.assignments()) || reads(conflict.where());
                    if (!conflict.nothing()) relation(insert.table(), Privilege.UPDATE, tables);
                }
                if (read) relation(insert.table(), Privilege.SELECT, tables);
                catalog(insert, ctes); 
            } else if (node instanceof SqlParser.Update update) {
                relation(update.table(), Privilege.UPDATE, tables);
                if (reads(update.assignments()) || reads(update.where()) || reads(update.returning())) relation(update.table(), Privilege.SELECT, tables);
                catalog(update, ctes);
            } else if (node instanceof SqlParser.Delete delete) {
                relation(delete.table(), Privilege.DELETE, tables);
                if (reads(delete.where()) || reads(delete.returning())) relation(delete.table(), Privilege.SELECT, tables);
            } else if (node instanceof SqlParser.Merge merge) {
                relation(merge.table(), Privilege.SELECT, tables);
                for (SqlParser.MergeWhen clause : merge.clauses()) if (!clause.action().equals("nothing")) relation(merge.table(), Privilege.valueOf(clause.action().toUpperCase(java.util.Locale.ROOT)), tables);
                catalog(merge, ctes);
            } else if (node instanceof SqlParser.CreateTable create) {
                ddl(create.temporary() || temp(create.name()));
                if (create.asQuery() != null && create.withData()) walk(create.asQuery(), ctes, tables);
                return;
            } else if (node instanceof SqlParser.CreateView create) { ddl(create.temporary() || temp(create.name())); }
            else if (node instanceof SqlParser.CreateIndex create) ddl(temp(create.table()));
            else if (node instanceof SqlParser.AlterTable alter) ddl(temp(alter.table()));
            else if (node instanceof SqlParser.DropTable drop) { for (SqlParser.Name name : drop.names()) ddl(temp(name)); }
            else if (node instanceof SqlParser.DropView drop) { for (SqlParser.Name name : drop.names()) ddl(temp(name)); }
            else if (node instanceof SqlParser.Reindex reindex) {
                ddl(false);
                if (reindex.target().equals("table")) relation(reindex.name(), Privilege.SELECT, tables);
                if (reindex.target().equals("index")) for (Engine.Table table : snapshot.tables().values()) if (table.indexes().stream().anyMatch(index -> index.name().equals(Engine.name(reindex.name())))) relation(new SqlParser.Name(List.of(table.name())), Privilege.SELECT, tables);
            } else if (node instanceof SqlParser.DropIndex drop) { for (SqlParser.Name name : drop.names()) ddl(temp(name)); }
            else if (node instanceof SqlParser.AlterSequence alter) ddl(temp(alter.name()));
            else if (node instanceof SqlParser.DropSequence drop) { for (SqlParser.Name name : drop.names()) ddl(temp(name)); }
            else if (node instanceof SqlParser.CreateType || node instanceof SqlParser.DropType || node instanceof SqlParser.CreateSequence) ddl(false);
            for (Object child : SqlParser.depthChildren(node)) walk(child, ctes, tables);
        }
        void catalog(SqlParser.Statement statement, Set<String> ctes) {
            DomainRouting.Route route = DomainRouting.route(statement, snapshot);
            Engine.Snapshot local = snapshot.domain(route.domain());
            ViewWrites.Expanded expanded = ViewWrites.expand(statement, snapshot, environment); statement = expanded.statement();
            for (Object child : SqlParser.depthChildren(statement)) walk(child, ctes, true);
            SqlParser.Name target = statement instanceof SqlParser.Insert i ? i.table() : statement instanceof SqlParser.Update u ? u.table() : ((SqlParser.Merge)statement).table();
            Engine.Table table = DomainRouting.readDomain(snapshot, target).tables().get(Engine.name(target)); if (table == null) return;
            for (Engine.Check check : table.checks()) walk(check.expression(), ctes, true);
            if (statement instanceof SqlParser.Insert insert) {
                defaults(table, insert.columns(), insert.values(), insert.defaultValues(), insert.overriding(), ctes);
                if (insert.conflict() != null) assignedDefaults(table, insert.conflict().assignments(), ctes);
            } else if (statement instanceof SqlParser.Update update) assignedDefaults(table, update.assignments(), ctes);
            else if (statement instanceof SqlParser.Merge merge) for (SqlParser.MergeWhen clause : merge.clauses()) {
                if (clause.action().equals("insert")) defaults(table, clause.columns(), List.of(clause.values()), clause.defaultValues(), clause.overriding(), ctes);
                else if (clause.action().equals("update")) assignedDefaults(table, clause.assignments(), ctes);
            }
            for (ViewWrites.Check check : expanded.checks()) walk(check.expression(), ctes, true);
        }
        void assignedDefaults(Engine.Table table, List<SqlParser.Assignment> assignments, Set<String> ctes) {
            for (SqlParser.Assignment assignment : assignments) if (assignment.expression() instanceof SqlParser.Default) for (Engine.Column column : table.columns()) if (column.name().equals(assignment.column())) walk(column.defaultValue(), ctes, true);
        }
        void defaults(Engine.Table table, List<String> columns, List<List<SqlParser.Expr>> values, boolean all, String overriding, Set<String> ctes) {
            List<String> targets = columns.isEmpty() ? table.columns().stream().map(Engine.Column::name).toList() : columns;
            for (Engine.Column column : table.columns()) {
                int ordinal = targets.indexOf(column.name()); boolean used = all || ordinal < 0 || column.identity() != 0 && "user".equals(overriding);
                if (!used) for (List<SqlParser.Expr> row : values) if (ordinal >= row.size() || row.get(ordinal) instanceof SqlParser.Default) { used = true; break; }
                if (used) walk(column.defaultValue(), ctes, true);
            }
        }
    }
    private static boolean reads(Object node) {
        if (node == null) return false;
        if (node instanceof List<?> list) return list.stream().anyMatch(Privileges::reads);
        if (node instanceof SqlParser.Name || node instanceof SqlParser.Star || node instanceof SqlParser.Subquery || node instanceof SqlParser.Exists || node instanceof SqlParser.Statement) return true;
        for (Object child : SqlParser.depthChildren(node)) if (reads(child)) return true;
        return false;
    }
}
