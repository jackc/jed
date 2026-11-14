package org.jed;

import java.util.List;

/** SQL derivation strength belongs to expressions, independently of their stored value types. */
record CollationDerivation(String name, int strength, boolean conflict, boolean inheritedMissing) {
    CollationDerivation(String name, int strength, boolean conflict) { this(name, strength, conflict, false); }
    static final CollationDerivation NONE = new CollationDerivation(null, 0, false);
    static CollationDerivation implicit(String name) { return name == null ? NONE : new CollationDerivation(name, 1, false); }
    static CollationDerivation explicit(String name) { return new CollationDerivation(name, 2, false); }
    CollationDerivation asColumn() { return conflict ? new CollationDerivation(null, 0, true, true) : strength == 0 ? this : implicit(name); }
    static boolean collatable(SqlType type) { return type.kind() == SqlType.Kind.TEXT || type.kind() == SqlType.Kind.ARRAY && type.element().kind() == SqlType.Kind.TEXT; }
    static CollationDerivation merge(CollationDerivation a, CollationDerivation b) {
        if (a.strength == 2 && b.strength == 2 && !a.name.equals(b.name)) throw new SqlException("42P21", "collation mismatch between explicit collations");
        if (a.strength == 2) return a; if (b.strength == 2) return b;
        if (a.conflict && b.strength != 0) return b; if (b.conflict && a.strength != 0) return a;
        if (a.conflict || b.conflict || a.strength != 0 && b.strength != 0 && !a.name.equals(b.name)) return new CollationDerivation(null, 0, true);
        return a.strength == 0 ? b : a;
    }
    static CollationDerivation comparison(CollationDerivation a, CollationDerivation b) { return merge(a.inheritedMissing ? NONE : a, b.inheritedMissing ? NONE : b); }
    String require() { if (conflict) throw new SqlException("42P22", "could not determine which collation to use"); return name == null ? "C" : name; }
    static CollationDerivation derive(SqlParser.Expr syntax, SqlType type, ScalarPlan.Resolver resolver, CollationDerivation current) {
        if (!collatable(type)) return NONE;
        if (syntax instanceof SqlParser.Name || syntax instanceof SqlParser.ColumnSlot || syntax instanceof SqlParser.Collate || syntax instanceof SqlParser.StoredExpression || syntax instanceof SqlParser.Subquery) return current;
        if (resolver.persistedCollations() && !(syntax instanceof SqlParser.Binary binary && binary.op().equals("||"))) return NONE;
        List<SqlParser.Expr> children;
        if (syntax instanceof SqlParser.Conditional conditional) { var values = new java.util.ArrayList<>(conditional.results()); if (conditional.otherwise() != null) values.add(conditional.otherwise()); children = values; }
        else if (syntax instanceof SqlParser.Cast cast) children = List.of(cast.value());
        else if (syntax instanceof SqlParser.Call call) children = call.args();
        else children = DefaultExpression.children(syntax);
        CollationDerivation result = NONE;
        for (SqlParser.Expr child : children) if (child != null) {
            CollationDerivation input = resolver.resolve(child instanceof SqlParser.NamedArgument named ? named.value() : child, null).collation();
            if (input.inheritedMissing() && !(syntax instanceof SqlParser.Cast && type.kind() == SqlType.Kind.TEXT && type.maxLength() < 0)) continue;
            result = merge(result, input);
        }
        return result;
    }
    static void requireSort(ScalarPlan.Expression expression, SqlParser.Expr syntax) {
        if (!expression.collation().conflict()) return;
        syntax = DefaultExpression.unwrap(syntax);
        boolean inherited = syntax instanceof SqlParser.Name || syntax instanceof SqlParser.ColumnSlot || syntax instanceof SqlParser.Subquery
            || syntax instanceof SqlParser.Cast cast && cast.target().name().equals("text");
        throw new SqlException(inherited ? "42P22" : "42P21", "could not determine sort or grouping collation");
    }
    static SqlType sortType(ScalarPlan.Expression expression) { return !expression.collation().require().equals("C") && collatable(expression.type()) ? SqlType.BYTEA : expression.type(); }
    static SqlValue sortValue(ScalarPlan.Expression expression, SqlValue value, CostMeter meter) {
        if (sortType(expression) != SqlType.BYTEA || !collatable(expression.type())) return value;
        if (value.isNull()) return SqlValue.nullValue(SqlType.BYTEA);
        Collation collation = meter.environment().collation(expression.collation().require());
        return SqlValue.ofBytes(key(value, collation, meter));
    }
    static byte[] key(SqlValue value, Collation collation, CostMeter meter) {
        if (value.type().kind() == SqlType.Kind.TEXT) {
            if (meter != null) { meter.charge(SqlText.length(value.asText()) * Costs.COLLATE); meter.guard(); }
            return collation.sortKey(value.asText());
        }
        Encoding.Sink out = new Encoding.Sink(); SqlArray array = value.asArray();
        for (SqlValue element : array.elements()) { out.u8(element.isNull() ? 2 : 1); if (!element.isNull()) out.raw(key(element, collation, meter)); }
        out.u8(0); out.u8(array.dimensions());
        for (int d = 0; d < array.dimensions(); d++) { out.unsigned(array.length(d), 4); out.signed(array.lowerBound(d), 4); }
        return out.bytes();
    }
    static int compare(SqlValue a, SqlValue b, CostMeter meter, Collation collation) {
        if (collation == Collation.C || !collatable(a.type())) return ScalarPlan.compare(a, b, meter);
        if (a.type().kind() == SqlType.Kind.TEXT) {
            if (meter != null) { meter.charge((SqlText.length(a.asText()) + (long)SqlText.length(b.asText())) * Costs.COLLATE); meter.guard(); }
            return collation.compare(a.asText(), b.asText());
        }
        SqlArray left = a.asArray(), right = b.asArray();
        for (int i = 0; i < Math.min(left.size(), right.size()); i++) {
            SqlValue x = left.element(i), y = right.element(i); int order;
            if (x.isNull() || y.isNull()) order = x.isNull() == y.isNull() ? 0 : x.isNull() ? 1 : -1;
            else order = compare(x, y, meter, collation);
            if (order != 0) return order;
        }
        // Equal elements retain the ordinary shape/lower-bound tie break.
        return ScalarPlan.compare(a, b, null);
    }
}
