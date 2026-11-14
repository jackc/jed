package org.jed;

import java.io.IOException;
import java.util.ArrayList;
import java.util.List;

/** Re-admit persisted definitions and stage all replacement trees before a single publication. */
final class Reindex {
    private Reindex() {}
    static Engine.Snapshot run(SqlParser.Reindex command, Engine.Snapshot snapshot, QueryEnvironment environment, Database database, long workMem) throws IOException {
        if (command.target().equals("schema")) throw new SqlException("0A000", "schemas are not supported");
        if (command.target().equals("system") && command.concurrently()) throw new SqlException("0A000", "cannot reindex system catalogs concurrently");
        List<Engine.Table> targets = new ArrayList<>(); String only = null;
        if (command.target().equals("database") || command.target().equals("system")) {
            if (command.name() != null && !command.name().parts().equals(List.of("main"))) throw new SqlException("0A000", "can only reindex the currently open database");
            if (command.target().equals("system")) return snapshot;
            targets.addAll(snapshot.tables().values());
            targets.sort((a, b) -> java.util.Arrays.compareUnsigned(SqlText.encode(a.name()), SqlText.encode(b.name())));
        } else {
            List<String> parts = command.name().parts();
            if (parts.size() > 2 || parts.size() == 2 && !parts.getFirst().equals("main")) throw new SqlException("0A000", "database is not attached");
            String name = parts.getLast();
            Engine.Table target = command.target().equals("index") ? Engine.indexOwner(snapshot, name) : snapshot.tables().get(name);
            if (target == null) {
                if (Engine.relationTaken(snapshot, name) || name.startsWith("jed_")) throw new SqlException("42809", "relation is not a " + command.target());
                throw new SqlException("42P01", "relation does not exist: " + name);
            }
            if (command.target().equals("index")) only = name;
            targets.add(target);
        }
        Engine.Snapshot next = snapshot;
        for (Engine.Table table : targets) {
            List<Engine.Index> indexes = new ArrayList<>(); boolean changed = false;
            for (Engine.Index index : table.indexes()) {
                if (only != null && !index.name().equals(only)) { indexes.add(index); continue; }
                StoredDefinitions.require(index);
                CollationCompatibility.require(table, environment.meter());
                ScalarPlan.Resolver resolver = environment.resolver(table.bindings(null), new ScalarPlan.Frame()).persisted();
                List<Engine.IndexKey> keys = new ArrayList<>();
                for (Engine.IndexKey key : index.keys()) {
                    SqlParser.Expr syntax = key.column() >= 0 ? new SqlParser.Name(List.of(table.columns().get(key.column()).name())) : key.expression();
                    Indexes.validateExpression(syntax, resolver);
                    ScalarPlan.Expression value = resolver.resolve(syntax, null);
                    if (index.kind() == 1) GinIndex.validateType(value.type());
                    else if (index.kind() == 2) GistIndex.validateType(value.type());
                    else if (!Indexes.keyable(value.type())) throw new SqlException("0A000", "type is not indexable");
                    String collation = CollationDerivation.collatable(value.type()) ? IndexCollations.derive(syntax, table.bindings(null)) : "C";
                    keys.add(new Engine.IndexKey(key.column(), key.expression(), value.type(), key.descending(), key.nullsFirst(), collation));
                }
                if (index.predicate() != null) {
                    Indexes.validateExpression(index.predicate(), resolver);
                    if (resolver.resolve(index.predicate(), SqlType.BOOLEAN).type() != SqlType.BOOLEAN) throw SqlException.typeMismatch("index predicate must be boolean");
                }
                Engine.Index admitted = new Engine.Index(index.name(), keys, index.unique(), index.constraint(), index.predicate(), index.kind(), index.entries(), IndexDependencies.forIndex(index, table, environment), index.gist(), HostDependencies.forIndex(index, table, environment));
                if (index.kind() != 0 && admitted.timezoneDependencies().present()) throw new SqlException("42P17", "index method cannot persist timezone dependencies");
                indexes.add(Indexes.build(admitted, table, new SqlValue[0], environment, database, workMem)); changed = true;
            }
            if (changed) next = next.put(table.withIndexes(indexes));
        }
        return next;
    }
}
