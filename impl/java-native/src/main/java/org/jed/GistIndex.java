package org.jed;

import java.util.ArrayList;
import java.util.List;
import org.jed.storage.PersistentTree;

/** GiST schema validation and fixed-width/range operator-class dispatch. */
final class GistIndex {
    private GistIndex() {}
    static void validateType(SqlType type) {
        switch (type.kind()) {
            case RANGE, I16, I32, I64, BOOLEAN, UUID, DATE, TIMESTAMP, TIMESTAMPTZ -> { }
            case TEXT, BYTEA, DECIMAL, INTERVAL -> throw new SqlException("0A000", "GiST operator class is not supported for " + type);
            default -> throw new SqlException("42704", "type has no default GiST operator class: " + type);
        }
    }
    static Engine.Index definition(SqlParser.CreateIndex syntax, Engine.Table table, Engine.Snapshot snapshot, QueryEnvironment environment, Database database) {
        if (syntax.unique()) throw new SqlException("0A000", "GiST does not support unique indexes");
        if (syntax.keys().size() != 1) throw new SqlException("0A000", "multi-column GiST requires an exclusion constraint");
        if (syntax.where() != null) throw new SqlException("0A000", "partial GiST requires an exclusion constraint");
        List<Engine.IndexKey> keys = new ArrayList<>(); List<String> parts = new ArrayList<>();
        for (var key : syntax.keys()) {
            var expression = DefaultExpression.unwrap(key.expression());
            if (!(expression instanceof SqlParser.Name name)) throw new SqlException("0A000", "GiST expression key is not supported");
            if (name.parts().size() != 1) throw new SqlException("42601", "qualified index column");
            int column = table.column(name.parts().getLast()); SqlType type = table.columns().get(column).type(); validateType(type);
            if (key.opclass() != null && !(type.kind() == SqlType.Kind.RANGE && key.opclass().equals("range_ops"))) throw new SqlException("42704", "unknown GiST operator class: " + key.opclass());
            if (key.explicitDirection() || key.nullsFirst() != null) throw new SqlException("0A000", "GiST does not support key ordering options");
            keys.add(new Engine.IndexKey(column, null, type, false, false)); parts.add(table.columns().get(column).name());
        }
        String name = syntax.name() == null ? Sequences.autoName(snapshot, table.name(), String.join("_", parts), "idx") : syntax.name().parts().getLast();
        if (syntax.name() != null && name.startsWith("jed_")) throw new SqlException("42939", "reserved index name");
        return new Engine.Index(name, keys, false, false, null, 2, PersistentTree.empty(database.pageSize(), TableLeafCodec.index(), database.pageCodec()));
    }
}
