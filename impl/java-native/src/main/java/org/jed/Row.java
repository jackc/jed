package org.jed;

import java.util.List;

/** An owned row whose lifetime is independent of its source cursor. */
public final class Row {
    private final List<String> names;
    private final List<SqlValue> values;
    Row(List<String> names, List<SqlValue> values) { this.names = names; this.values = List.copyOf(values); }
    public List<SqlValue> values() { return values; }
    public SqlValue get(int index) {
        if (index < 0 || index >= values.size()) throw new SqlException("42703", "column index does not exist");
        return values.get(index);
    }
    public SqlValue get(String name) {
        int index = names.indexOf(name);
        if (index < 0) throw new SqlException("42703", "column does not exist: " + name);
        return values.get(index);
    }
}
