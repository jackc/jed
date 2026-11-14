package org.jed;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

/** Immutable rectangular, row-major SQL array. Dimensions belong to values, not types. */
public final class SqlArray {
    private final int[] dims;
    private final int[] lower;
    private final List<SqlValue> elements;

    public SqlArray(int[] dims, int[] lower, List<SqlValue> elements) {
        if (dims.length != lower.length || dims.length > 255) throw shape();
        long count = dims.length == 0 ? 0 : 1;
        for (int d = 0; d < dims.length; d++) {
            if (dims[d] < 1 || (long) lower[d] + dims[d] - 1 > Integer.MAX_VALUE) throw shape();
            count *= dims[d];
            if (count > Integer.MAX_VALUE) throw shape();
        }
        if (count != elements.size()) throw shape();
        this.dims = dims.clone(); this.lower = lower.clone(); this.elements = List.copyOf(elements);
    }
    public static SqlArray empty() { return new SqlArray(new int[0], new int[0], List.of()); }
    public static SqlArray vector(List<SqlValue> elements) { return elements.isEmpty() ? empty() : new SqlArray(new int[]{elements.size()}, new int[]{1}, elements); }
    public int[] dims() { return dims.clone(); }
    public int[] lowerBounds() { return lower.clone(); }
    public int dimensions() { return dims.length; }
    public int length(int dimension) { return dims[dimension]; }
    public int lowerBound(int dimension) { return lower[dimension]; }
    public int size() { return elements.size(); }
    public SqlValue element(int index) { return elements.get(index); }
    public List<SqlValue> elements() { return elements; }
    static SqlException shape() { return new SqlException("2202E", "invalid array dimensions"); }

    SqlArray cast(SqlType element, CostMeter meter) {
        List<SqlValue> converted = new ArrayList<>(size());
        for (SqlValue value : elements) converted.add(SqlCasts.cast(value, element, meter));
        return new SqlArray(dims, lower, converted);
    }
    SqlValue subscript(SqlType element, long[] indexes) {
        if (indexes.length != dims.length || dims.length == 0) return SqlValue.nullValue(element);
        int offset = 0;
        for (int d = 0; d < dims.length; d++) {
            long relative = indexes[d] - lower[d];
            if (relative < 0 || relative >= dims[d]) return SqlValue.nullValue(element);
            offset = offset * dims[d] + (int) relative;
        }
        return elements.get(offset);
    }
    SqlArray slice(long[] starts, long[] ends) {
        if (starts.length > dims.length || dims.length == 0) return empty();
        int[] first = new int[dims.length], size = new int[dims.length], bounds = new int[dims.length];
        long count = 1;
        for (int d = 0; d < dims.length; d++) {
            long start = Math.max(lower[d], d < starts.length ? starts[d] : lower[d]);
            long end = Math.min((long) lower[d] + dims[d] - 1, d < ends.length ? ends[d] : (long) lower[d] + dims[d] - 1);
            if (end < start) return empty();
            first[d] = (int) (start - lower[d]); size[d] = (int) (end - start + 1); bounds[d] = 1; count *= size[d];
        }
        List<SqlValue> output = new ArrayList<>((int) count);
        for (int i = 0; i < count; i++) {
            int remainder = i, source = 0, stride = 1;
            for (int d = dims.length - 1; d >= 0; d--) { source += (first[d] + remainder % size[d]) * stride; remainder /= size[d]; stride *= dims[d]; }
            output.add(elements.get(source));
        }
        return new SqlArray(size, bounds, output);
    }
    int compare(SqlArray other, CostMeter meter) {
        for (int i = 0; i < Math.min(size(), other.size()); i++) {
            SqlValue a = element(i), b = other.element(i);
            int order = a.isNull() ? b.isNull() ? 0 : 1 : b.isNull() ? -1 : ScalarPlan.compare(a, b, null);
            if (order != 0) return order;
        }
        int order = Integer.compare(size(), other.size());
        if (order != 0) return order;
        order = Integer.compare(dimensions(), other.dimensions());
        if (order != 0) return order;
        for (int d = 0; d < dims.length; d++) { order = Integer.compare(dims[d], other.dims[d]); if (order != 0) return order; order = Integer.compare(lower[d], other.lower[d]); if (order != 0) return order; }
        return 0;
    }
    String render(TimeZoneData.Zone zone) {
        if (dims.length == 0) return "{}";
        StringBuilder out = new StringBuilder();
        boolean explicit = false; for (int bound : lower) if (bound != 1) explicit = true;
        if (explicit) { for (int d = 0; d < dims.length; d++) out.append('[').append(lower[d]).append(':').append((long) lower[d] + dims[d] - 1).append(']'); out.append('='); }
        render(out, 0, 0, zone); return out.toString();
    }
    private int render(StringBuilder out, int dimension, int offset, TimeZoneData.Zone zone) {
        out.append('{');
        for (int i = 0; i < dims[dimension]; i++) {
            if (i != 0) out.append(',');
            if (dimension + 1 < dims.length) offset = render(out, dimension + 1, offset, zone);
            else {
                SqlValue value = elements.get(offset++);
                if (value.isNull()) { out.append("NULL"); continue; }
                String text = zone == null ? value.toString() : Temporal.renderValue(value, zone);
                if (value.type().kind() == SqlType.Kind.BOOLEAN) text = value.asBoolean() ? "t" : "f";
                boolean quoted = text.isEmpty() || text.equalsIgnoreCase("NULL");
                for (int c = 0; c < text.length(); c++) if (",{}\"\\ \t\n\r\f\u000b".indexOf(text.charAt(c)) >= 0) quoted = true;
                if (quoted) out.append('"');
                for (int c = 0; c < text.length(); c++) { char ch = text.charAt(c); if (ch == '"' || ch == '\\') out.append('\\'); out.append(ch); }
                if (quoted) out.append('"');
            }
        }
        out.append('}'); return offset;
    }
    @Override public String toString() { return render(null); }
    @Override public boolean equals(Object other) { return other instanceof SqlArray array && Arrays.equals(dims, array.dims) && Arrays.equals(lower, array.lower) && elements.equals(array.elements); }
    @Override public int hashCode() { return 31 * (31 * Arrays.hashCode(dims) + Arrays.hashCode(lower)) + elements.hashCode(); }
}
