package org.jed;

import java.util.Map;

/** A file pin is checked against loaded data before any key-based access or maintenance. */
final class CollationCompatibility {
    private CollationCompatibility() {}
    static void validate(Engine.Snapshot snapshot, Environment environment) {
        for (Engine.CollationReference reference : snapshot.collations().values()) {
            try { environment.collation(reference.name()); }
            catch (SqlException missing) {
                if (!missing.sqlState().equals("42704")) throw missing;
                throw new SqlException("XX002", "missing collation " + reference.name() + " pinned to Unicode " + reference.unicodeVersion() + ", CLDR " + reference.cldrVersion());
            }
        }
    }
    static boolean matches(String name, Map<String, Engine.CollationReference> references, Environment environment) {
        if (name.equals("C")) return true;
        Engine.CollationReference reference = references.get(name);
        if (reference == null) return true; // A newly declared object uses the currently loaded data.
        Collation loaded = environment.collation(name);
        return reference.unicodeVersion().equals(loaded.unicodeVersion()) && reference.cldrVersion().equals(loaded.cldrVersion());
    }
    static boolean table(Engine.Table table, CostMeter meter) {
        for (Engine.Column column : table.columns()) if (!matches(column.collation(), meter.collations(), meter.environment())) return false;
        for (Engine.Index index : table.indexes()) if (!index(index, meter)) return false;
        return true;
    }
    static boolean index(Engine.Index index, CostMeter meter) {
        for (Engine.IndexKey key : index.keys()) if (!matches(key.collation(), meter.collations(), meter.environment())) return false;
        return true;
    }
    static void require(Engine.Table table, CostMeter meter) {
        if (!table(table, meter)) throw new SqlException("XX002", "table " + table.name() + " references a collation whose loaded Unicode/CLDR version differs from its file pin");
    }
    static String defaultName(Engine.Snapshot snapshot) {
        for (Engine.CollationReference reference : snapshot.collations().values()) if (reference.isDefault()) return reference.name();
        return "C";
    }
}
