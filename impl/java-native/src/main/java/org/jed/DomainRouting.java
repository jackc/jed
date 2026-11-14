package org.jed;

import java.util.List;

/** Database qualifiers select storage domains; column labels remain unqualified. */
final class DomainRouting {
    private DomainRouting() {}
    record Route(String domain, SqlParser.Statement statement) {}
    static Engine.Snapshot readDomain(Engine.Snapshot roots, SqlParser.Name name) {
        return roots.domain(scope(roots, name, false));
    }
    static String scope(Engine.Snapshot roots, SqlParser.Name name, boolean create) {
        if (name.parts().size() > 2) throw new SqlException("42P01", "invalid database-qualified name");
        if (name.parts().size() == 2) { String domain = name.parts().getFirst(); roots.domain(domain); return domain; }
        Engine.Snapshot temp = roots.domains().get("temp");
        if (!create && temp != null && Engine.relationTaken(temp, name.parts().getLast())) return "temp";
        return "main";
    }
    static boolean hostAttachment(SqlParser.Name name) { return name.parts().size() == 2 && !List.of("main", "temp").contains(name.parts().getFirst()); }
    static SqlParser.Name local(SqlParser.Name name) { return name == null || name.parts().isEmpty() ? name : new SqlParser.Name(List.of(name.parts().getLast())); }
    static Route route(SqlParser.Statement statement, Engine.Snapshot roots) {
        SqlParser.Name target = target(statement);
        if (target == null) return new Route("main", statement);
        boolean create = statement instanceof SqlParser.CreateTable || statement instanceof SqlParser.CreateView || statement instanceof SqlParser.CreateType || statement instanceof SqlParser.CreateSequence;
        boolean temporary = statement instanceof SqlParser.CreateTable table && table.temporary() || statement instanceof SqlParser.CreateView view && view.temporary();
        if (temporary && target.parts().size() == 2 && !target.parts().getFirst().equals("temp")) throw new SqlException("42601", "temporary relation must use temp database");
        String domain = temporary ? "temp" : scope(roots, target, create); roots.domain(domain);
        if (statement instanceof SqlParser.CreateView view && domain.equals("main") && (view.replace() && roots.domain("temp").views().containsKey(target.parts().getLast()) || viewUsesTemp(view.query(), roots, java.util.Set.of()))) domain = "temp";
        SqlParser.Name name = local(target);
        SqlParser.Statement rewritten = switch (statement) {
            case SqlParser.Analyze x -> new SqlParser.Analyze(name, x.columns());
            case SqlParser.Insert x -> new SqlParser.Insert(name, x.columns(), x.values(), x.query(), x.defaultValues(), x.returning(), x.oldAlias(), x.newAlias(), x.overriding(), x.alias(), x.conflict());
            case SqlParser.Update x -> new SqlParser.Update(name, x.alias(), x.assignments(), x.from(), x.where(), x.returning(), x.oldAlias(), x.newAlias());
            case SqlParser.Delete x -> new SqlParser.Delete(name, x.alias(), x.using(), x.where(), x.returning(), x.oldAlias(), x.newAlias());
            case SqlParser.Merge x -> new SqlParser.Merge(name, x.alias(), x.source(), x.on(), x.clauses(), x.returning(), x.oldAlias(), x.newAlias(), x.targetWhere());
            case SqlParser.CreateTable x -> new SqlParser.CreateTable(name, domain.equals("temp"), x.ifNotExists(), x.columns(), x.constraints(), x.asQuery(), x.columnNames(), x.onCommit(), x.withData());
            case SqlParser.CreateView x -> new SqlParser.CreateView(name, domain.equals("temp"), x.replace(), x.ifNotExists(), x.columns(), x.query(), x.definition(), x.checkOption(), x.options());
            case SqlParser.AlterTable x -> new SqlParser.AlterTable(name, x.ifExists(), x.actions(), x.viewOnly());
            case SqlParser.CreateIndex x -> {
                if (x.name() != null && x.name().parts().size() == 2 && !x.name().parts().getFirst().equals(domain)) throw new SqlException("0A000", "index and table must belong to the same database");
                yield new SqlParser.CreateIndex(local(x.name()), name, x.unique(), x.ifNotExists(), x.method(), x.keys(), x.where());
            }
            case SqlParser.CreateSequence x -> new SqlParser.CreateSequence(name, x.ifNotExists(), x.options());
            case SqlParser.AlterSequence x -> new SqlParser.AlterSequence(name, x.ifExists(), x.newName(), x.options());
            case SqlParser.CreateType x -> new SqlParser.CreateType(name, x.fields());
            case SqlParser.Reindex x -> new SqlParser.Reindex(x.target(), name, x.concurrently());
            case SqlParser.DropTable x -> new SqlParser.DropTable(localNames(x.names(), roots, domain), x.ifExists(), x.cascade());
            case SqlParser.DropView x -> new SqlParser.DropView(localNames(x.names(), roots, domain), x.ifExists(), x.cascade());
            case SqlParser.DropIndex x -> new SqlParser.DropIndex(localNames(x.names(), roots, domain), x.ifExists(), x.cascade());
            case SqlParser.DropSequence x -> new SqlParser.DropSequence(localNames(x.names(), roots, domain), x.ifExists(), x.cascade());
            case SqlParser.DropType x -> new SqlParser.DropType(localNames(x.names(), roots, domain), x.ifExists(), x.cascade());
            default -> statement;
        };
        return new Route(domain, rewritten);
    }
    static List<SqlParser.Statement> splitDrop(SqlParser.Statement statement, Engine.Snapshot roots) {
        List<SqlParser.Name> names = switch (statement) { case SqlParser.DropTable x -> x.names(); case SqlParser.DropView x -> x.names(); case SqlParser.DropIndex x -> x.names(); case SqlParser.DropSequence x -> x.names(); case SqlParser.DropType x -> x.names(); default -> List.of(); };
        var groups = new java.util.TreeMap<String, java.util.ArrayList<SqlParser.Name>>(SqlText::compare);
        for (SqlParser.Name name : names) groups.computeIfAbsent(scope(roots, name, false), ignored -> new java.util.ArrayList<>()).add(name);
        if (groups.size() < 2) return List.of(statement);
        return groups.values().stream().map(group -> switch (statement) {
            case SqlParser.DropTable x -> (SqlParser.Statement)new SqlParser.DropTable(group, x.ifExists(), x.cascade());
            case SqlParser.DropView x -> new SqlParser.DropView(group, x.ifExists(), x.cascade());
            case SqlParser.DropIndex x -> new SqlParser.DropIndex(group, x.ifExists(), x.cascade());
            case SqlParser.DropSequence x -> new SqlParser.DropSequence(group, x.ifExists(), x.cascade());
            case SqlParser.DropType x -> new SqlParser.DropType(group, x.ifExists(), x.cascade());
            default -> throw new IllegalStateException();
        }).toList();
    }
    static boolean tempOnly(SqlParser.Statement statement, Engine.Snapshot roots) {
        List<SqlParser.Statement> drops = splitDrop(statement, roots); if (drops.size() > 1) return drops.stream().allMatch(drop -> tempOnly(drop, roots));
        if (persistentSequence(statement, roots)) return false;
        if (statement instanceof SqlParser.With with) return tempOnly(with.body(), roots) && with.ctes().stream().allMatch(cte -> tempOnly(cte.body(), roots));
        if (statement instanceof SqlParser.Select || statement instanceof SqlParser.Values || statement instanceof SqlParser.SetOperation || statement instanceof SqlParser.QueryTail) return true;
        SqlParser.Name target = target(statement);
        if (target == null || !route(statement, roots).domain().equals("temp")) return false;
        Engine.Table table = roots.domain("temp").tables().get(target.parts().getLast());
        if (table != null && table.columns().stream().anyMatch(column -> persistentSequence(column.defaultValue(), roots))) return false;
        if (statement instanceof SqlParser.Insert || statement instanceof SqlParser.Update || statement instanceof SqlParser.Delete || statement instanceof SqlParser.Merge) {
            java.util.Set<String> seen = new java.util.HashSet<>();
            while (true) {
                if (!scope(roots, target, false).equals("temp")) return false;
                Engine.View view = roots.domain("temp").views().get(target.parts().getLast());
                if (view == null || !seen.add(view.name()) || !ViewWrites.shape(view.query(), false)) return true;
                target = ((SqlParser.Table)((SqlParser.Select)view.query()).from().getFirst()).name();
            }
        }
        return true;
    }
    private static boolean persistentSequence(Object node, Engine.Snapshot roots) {
        if (node == null) return false;
        if (node instanceof List<?> list) return list.stream().anyMatch(item -> persistentSequence(item, roots));
        if (node instanceof SqlParser.Call call && (call.name().equals("nextval") || call.name().equals("setval")))
            return call.args().isEmpty() || !(call.args().getFirst() instanceof SqlParser.Literal literal) || literal.kind() != Lexer.Kind.STRING || !roots.domain("temp").sequences().containsKey(Sequences.name(literal.text()));
        for (Object child : SqlParser.depthChildren(node)) if (persistentSequence(child, roots)) return true;
        return false;
    }
    static boolean viewUsesTemp(Object node, Engine.Snapshot roots, java.util.Set<String> ctes) {
        if (node == null) return false;
        if (node instanceof List<?> list) return list.stream().anyMatch(item -> viewUsesTemp(item, roots, ctes));
        if (node instanceof SqlParser.With with) {
            java.util.Set<String> local = new java.util.HashSet<>(ctes); boolean temporary = false;
            for (SqlParser.Cte cte : with.ctes()) { if (with.recursive()) local.add(cte.name()); temporary |= viewUsesTemp(cte.body(), roots, local); local.add(cte.name()); }
            return viewUsesTemp(with.body(), roots, local) || temporary;
        }
        if (node instanceof SqlParser.Table table && !(table.name().parts().size() == 1 && ctes.contains(table.name().parts().getFirst()))) {
            String domain = scope(roots, table.name(), false);
            if (!domain.equals("main") && !domain.equals("temp")) throw new SqlException("0A000", "views cannot reference attachments");
            return domain.equals("temp");
        }
        if (node instanceof SqlParser.Call call && java.util.Set.of("nextval", "currval", "setval").contains(call.name()) && !call.args().isEmpty() && call.args().getFirst() instanceof SqlParser.Literal literal && literal.kind() == Lexer.Kind.STRING && roots.domain("temp").sequences().containsKey(Sequences.name(literal.text()))) return true;
        boolean temporary = false; for (Object child : SqlParser.depthChildren(node)) temporary |= viewUsesTemp(child, roots, ctes); return temporary;
    }
    static void validate(SqlParser.Statement statement, Engine.Snapshot roots, String domain) {
        SqlParser.Name target = target(statement);
        boolean create = statement instanceof SqlParser.CreateTable || statement instanceof SqlParser.CreateView || statement instanceof SqlParser.CreateSequence || statement instanceof SqlParser.CreateType;
        if (target != null) {
            if (create && target.parts().getLast().startsWith("jed_")) throw new SqlException("42939", "reserved relation name");
            if (!create && CatalogRelation.recognized(target)) throw new SqlException("42809", "cannot modify system relation");
        }
        boolean attached = !domain.equals("main") && !domain.equals("temp");
        if (attached && (statement instanceof SqlParser.CreateView || statement instanceof SqlParser.CreateSequence || statement instanceof SqlParser.CreateType)) throw new SqlException("0A000", "catalog object in attachment is unsupported");
        if (attached && statement instanceof SqlParser.Insert insert && insert.conflict() != null) throw new SqlException("0A000", "ON CONFLICT in attachment is unsupported");
        if (attached && statement instanceof SqlParser.CreateIndex index && index.method() != null && !index.method().equals("btree")) throw new SqlException("0A000", "attachment index method is unsupported");
        if (statement instanceof SqlParser.CreateTable table && table.ifNotExists() && (Engine.relationTaken(roots.domain(domain), table.name().parts().getLast()) || (domain.equals("main") || domain.equals("temp")) && Engine.relationTaken(roots.domain(domain.equals("main") ? "temp" : "main"), table.name().parts().getLast()))) return;
        if (statement instanceof SqlParser.CreateTable || statement instanceof SqlParser.AlterTable) validateDefinition(statement, roots, domain, target.parts().getLast(), attached);
    }
    private static void validateDefinition(Object node, Engine.Snapshot roots, String domain, String target, boolean attached) {
        if (node == null) return;
        if (node instanceof List<?> list) { for (Object item : list) validateDefinition(item, roots, domain, target, attached); return; }
        if (node instanceof SqlParser.Constraint constraint && constraint.name() != null && constraint.name().startsWith("jed_") && (constraint.kind().equals("unique") || constraint.kind().equals("exclude"))) throw new SqlException("42939", "reserved constraint index name");
        if (node instanceof SqlParser.Constraint constraint && constraint.reference() != null) {
            SqlParser.Name reference = constraint.reference();
            String parentDomain = reference.parts().size() == 1 && reference.parts().getLast().equals(target) ? domain : scope(roots, reference, false);
            if (attached || !parentDomain.equals("main") && !parentDomain.equals("temp")) throw new SqlException("0A000", "attachment foreign key is unsupported");
            boolean self = parentDomain.equals(domain) && reference.parts().getLast().equals(target);
            if (!self && !Engine.relationTaken(roots.domain(parentDomain), reference.parts().getLast())) throw new SqlException("42P01", "referenced relation does not exist");
            if (!parentDomain.equals(domain)) throw new SqlException("42P16", "foreign key cannot cross temporary and persistent domains");
        }
        if (node instanceof SqlParser.Column column) {
            if (!domain.equals("main") && column.collation() != null) throw new SqlException("0A000", "collated columns in this database domain are unsupported");
            if (attached && (column.identity() != null || Sequences.serialType(column.type()) != null || roots.types().containsKey(column.type().name()))) throw new SqlException("0A000", "attachment column catalog dependency is unsupported");
        }
        if (!domain.equals("main") && node instanceof SqlParser.Constraint constraint && constraint.kind().equals("exclude")) throw new SqlException("0A000", "attachment exclusion constraint is unsupported");
        for (Object child : SqlParser.depthChildren(node)) validateDefinition(child, roots, domain, target, attached);
    }
    static SqlParser.Name target(SqlParser.Statement statement) {
        return switch (statement) {
            case SqlParser.Analyze x -> x.table(); case SqlParser.Insert x -> x.table(); case SqlParser.Update x -> x.table(); case SqlParser.Delete x -> x.table(); case SqlParser.Merge x -> x.table();
            case SqlParser.CreateTable x -> x.name(); case SqlParser.CreateView x -> x.name(); case SqlParser.CreateIndex x -> x.table(); case SqlParser.AlterTable x -> x.table();
            case SqlParser.CreateSequence x -> x.name(); case SqlParser.AlterSequence x -> x.name(); case SqlParser.CreateType x -> x.name(); case SqlParser.Reindex x -> x.name();
            case SqlParser.DropTable x -> x.names().getFirst(); case SqlParser.DropView x -> x.names().getFirst(); case SqlParser.DropIndex x -> x.names().getFirst(); case SqlParser.DropSequence x -> x.names().getFirst(); case SqlParser.DropType x -> x.names().getFirst();
            default -> null;
        };
    }
    private static List<SqlParser.Name> localNames(List<SqlParser.Name> names, Engine.Snapshot roots, String domain) {
        for (SqlParser.Name name : names) if (!scope(roots, name, false).equals(domain)) throw new SqlException("0A000", "multi-domain DROP must be split into statements");
        return names.stream().map(DomainRouting::local).toList();
    }
}
