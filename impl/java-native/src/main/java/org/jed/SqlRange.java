package org.jed;

import java.util.Objects;

/** Canonical range bounds; Java null denotes an unbounded endpoint. */
public final class SqlRange {
    private final SqlValue lower, upper;
    private final boolean empty, lowerInclusive, upperInclusive;
    private SqlRange(boolean empty, SqlValue lower, SqlValue upper, boolean li, boolean ui) {
        this.empty = empty; this.lower = lower; this.upper = upper;
        this.lowerInclusive = lower != null && li; this.upperInclusive = upper != null && ui;
    }
    public static SqlRange empty() { return new SqlRange(true, null, null, false, false); }
    public static SqlRange create(SqlType element, SqlValue lower, SqlValue upper, boolean li, boolean ui) {
        if (lower != null && lower.isNull()) lower = null;
        if (upper != null && upper.isNull()) upper = null;
        if (lower != null && !lower.type().equals(element) || upper != null && !upper.type().equals(element)) throw SqlException.typeMismatch("range endpoint type mismatch");
        if (lower != null && upper != null) {
            int c = ScalarPlan.compare(lower, upper, null);
            if (c > 0) throw new SqlException("22000", "range lower bound exceeds upper bound");
            if (c == 0 && !(li && ui)) return empty();
        }
        boolean discrete = false;
        for (RangeCatalog.Fact fact : RangeCatalog.FACTS) if (fact.element().equals(element.toString())) { discrete = fact.discrete(); break; }
        if (discrete) {
            if (lower != null && !li && finiteDate(lower)) { lower = step(lower); li = true; }
            if (upper != null && ui && finiteDate(upper)) { upper = step(upper); ui = false; }
        }
        if (lower != null && upper != null) {
            int c = ScalarPlan.compare(lower, upper, null);
            if (c > 0 || c == 0 && !(li && ui)) return empty();
        }
        return new SqlRange(false, lower, upper, li, ui);
    }
    private static boolean finiteDate(SqlValue value) { return value.type() != SqlType.DATE || value.asDate() != Integer.MIN_VALUE && value.asDate() != Integer.MAX_VALUE; }
    private static SqlValue step(SqlValue value) {
        if (value.type() == SqlType.DATE) { int day = value.asDate(); if (day >= Integer.MAX_VALUE - 1) throw SqlException.numericOverflow(); return SqlValue.ofDate(day + 1); }
        try { return SqlValue.ofInteger(value.type(), Math.addExact(value.asInteger(), 1)); }
        catch (ArithmeticException failure) { throw SqlException.numericOverflow(); }
    }
    public boolean isEmpty() { return empty; }
    public SqlValue lower() { return lower; }
    public SqlValue upper() { return upper; }
    public boolean lowerInclusive() { return lowerInclusive; }
    public boolean upperInclusive() { return upperInclusive; }
    static int bound(SqlValue a, boolean ai, boolean al, SqlValue b, boolean bi, boolean bl) {
        if (a == null || b == null) {
            int x = a == null ? al ? -1 : 1 : 0, y = b == null ? bl ? -1 : 1 : 0;
            return Integer.compare(x, y);
        }
        int c = ScalarPlan.compare(a, b, null); if (c != 0) return c;
        int x = ai ? 0 : al ? 1 : -1, y = bi ? 0 : bl ? 1 : -1;
        return Integer.compare(x, y);
    }
    int lowerCompare(SqlRange other) { return bound(lower, lowerInclusive, true, other.lower, other.lowerInclusive, true); }
    int upperCompare(SqlRange other) { return bound(upper, upperInclusive, false, other.upper, other.upperInclusive, false); }
    public int compare(SqlRange other) {
        if (empty || other.empty) return empty == other.empty ? 0 : empty ? -1 : 1;
        int c = lowerCompare(other); return c != 0 ? c : upperCompare(other);
    }
    boolean contains(SqlValue point) { return !empty && bound(lower, lowerInclusive, true, point, true, true) <= 0 && bound(upper, upperInclusive, false, point, true, false) >= 0; }
    boolean contains(SqlRange other) { return other.empty || !empty && lowerCompare(other) <= 0 && upperCompare(other) >= 0; }
    boolean before(SqlRange other) { return !empty && !other.empty && bound(upper, upperInclusive, false, other.lower, other.lowerInclusive, true) < 0; }
    boolean overlaps(SqlRange other) { return !empty && !other.empty && !before(other) && !other.before(this); }
    private static boolean touches(SqlValue upper, boolean ui, SqlValue lower, boolean li) { return upper != null && lower != null && ui != li && ScalarPlan.compare(upper, lower, null) == 0; }
    boolean adjacent(SqlRange other) { return !empty && !other.empty && (touches(upper, upperInclusive, other.lower, other.lowerInclusive) || touches(other.upper, other.upperInclusive, lower, lowerInclusive)); }
    SqlRange union(SqlRange other, boolean strict) {
        if (empty) return other; if (other.empty) return this;
        if (strict && !overlaps(other) && !adjacent(other)) throw new SqlException("22000", "range union is not contiguous");
        SqlRange lo = lowerCompare(other) <= 0 ? this : other, hi = upperCompare(other) >= 0 ? this : other;
        return new SqlRange(false, lo.lower, hi.upper, lo.lowerInclusive, hi.upperInclusive);
    }
    SqlRange intersection(SqlRange other) {
        if (!overlaps(other)) return empty();
        SqlRange lo = lowerCompare(other) >= 0 ? this : other, hi = upperCompare(other) <= 0 ? this : other;
        return new SqlRange(false, lo.lower, hi.upper, lo.lowerInclusive, hi.upperInclusive);
    }
    SqlRange subtract(SqlRange other) {
        if (!overlaps(other)) return this;
        int lo = lowerCompare(other), hi = upperCompare(other);
        if (lo >= 0 && hi <= 0) return empty();
        if (lo < 0 && hi > 0) throw new SqlException("22000", "range difference is not contiguous");
        return lo < 0 ? new SqlRange(false, lower, other.lower, lowerInclusive, !other.lowerInclusive)
            : new SqlRange(false, other.upper, upper, !other.upperInclusive, upperInclusive);
    }
    String render(TimeZoneData.Zone zone) {
        if (empty) return "empty";
        return (lowerInclusive ? "[" : "(") + endpoint(lower, zone) + "," + endpoint(upper, zone) + (upperInclusive ? "]" : ")");
    }
    private static String endpoint(SqlValue value, TimeZoneData.Zone zone) {
        if (value == null) return "";
        String text = Temporal.renderValue(value, zone); boolean quote = text.isEmpty();
        for (int i = 0; i < text.length(); i++) if (",[]()\"\\".indexOf(text.charAt(i)) >= 0 || Character.isWhitespace(text.charAt(i))) quote = true;
        return quote ? '"' + text.replace("\\", "\\\\").replace("\"", "\"\"") + '"' : text;
    }
    @Override public String toString() { return render(null); }
    @Override public boolean equals(Object other) { return other instanceof SqlRange r && compare(r) == 0; }
    @Override public int hashCode() { return Objects.hash(empty, lower, upper, lowerInclusive, upperInclusive); }
}
