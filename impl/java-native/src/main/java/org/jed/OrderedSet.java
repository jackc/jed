package org.jed;

import java.util.ArrayList;
import java.util.List;
import java.util.Set;

/** Per-group direct arguments and streaming finalization over a bounded external sort. */
final class OrderedSet {
    static final Set<String> NAMES = Set.of("mode", "percentile_cont", "percentile_disc", "rank", "dense_rank", "percent_rank", "cume_dist");
    final String name;
    final List<ScalarPlan.Expression> direct;
    final List<SqlParser.Order> order;
    final SqlType result;
    final boolean hypothetical;
    final List<ScalarPlan.Expression> keyExpressions;
    final boolean decorated;
    private OrderedSet(String name, List<ScalarPlan.Expression> direct, List<SqlParser.Order> order, SqlType result, List<ScalarPlan.Expression> keys) {
        this.name = name; this.direct = direct; this.order = order; this.result = result;
        keyExpressions = List.copyOf(keys); decorated = keys.stream().anyMatch(key -> !CollationDerivation.sortType(key).equals(key.type()));
        hypothetical = !name.equals("mode") && !name.startsWith("percentile_");
    }
    static void validate(SqlParser.Call call) {
        if (!call.withinGroup() && Set.of("rank", "dense_rank", "percent_rank", "cume_dist").contains(call.name())) throw new SqlException("42809", "window function requires OVER");
        if (!call.withinGroup() || !NAMES.contains(call.name()) || call.star()) throw overload();
        if (call.distinct()) throw new SqlException("42601", "DISTINCT is not allowed with WITHIN GROUP");
        boolean hypothetical = !call.name().equals("mode") && !call.name().startsWith("percentile_");
        if (hypothetical ? call.args().size() != call.order().size() : call.order().size() != 1 || call.args().size() != (call.name().equals("mode") ? 0 : 1)) throw overload();
    }
    static AggregatePlan.Fold resolve(SqlParser.Call call, ScalarPlan.Resolver input, ScalarPlan.Resolver grouped, List<SqlParser.Expr> directSyntax) {
        validate(call);
        boolean hypothetical = !call.name().equals("mode") && !call.name().startsWith("percentile_");
        List<ScalarPlan.Expression> keys = new ArrayList<>(), direct = new ArrayList<>();
        for (SqlParser.Order key : call.order()) {
            if (AggregatePlan.contains(key.expression()) || WindowSyntax.contains(key.expression())) throw new SqlException("42803", "aggregate or window in WITHIN GROUP");
            ScalarPlan.Expression value = input.resolve(key.expression(), null); ScalarPlan.requireComparable(value.type()); CollationDerivation.requireSort(value, key.expression());
            if (call.name().equals("percentile_cont")) {
                if (!value.type().isNumeric() && value.type().kind() != SqlType.Kind.INTERVAL) throw overload();
                if (value.type().isNumeric()) { ScalarPlan.Expression original = value; value = new ScalarPlan.Expression(SqlType.F64, meter -> SqlCasts.cast(original.run(meter), SqlType.F64), original.constant()); }
            }
            keys.add(value);
        }
        for (int i = 0; i < directSyntax.size(); i++) {
            SqlType context = hypothetical ? keys.get(i).type() : null;
            ScalarPlan.Expression value = grouped.resolve(directSyntax.get(i), context);
            if (hypothetical) {
                if (!value.type().equals(context) && value.type() != SqlType.UNKNOWN && !(value.type().isNumeric() && context.isNumeric())) throw overload();
                ScalarPlan.Expression original = value; value = new ScalarPlan.Expression(context, meter -> SqlCasts.cast(original.run(meter), context), original.constant());
            } else {
                SqlType type = value.type().kind() == SqlType.Kind.ARRAY ? value.type().element() : value.type();
                if (!type.isNumeric() && type != SqlType.UNKNOWN) throw overload();
            }
            direct.add(value);
        }
        ScalarPlan.Expression filter = call.filter() == null ? null : input.resolve(call.filter(), SqlType.BOOLEAN);
        if (filter != null && filter.type() != SqlType.BOOLEAN) throw SqlException.typeMismatch("aggregate filter requires boolean");
        SqlType type = hypothetical ? call.name().equals("rank") || call.name().equals("dense_rank") ? SqlType.I64 : SqlType.F64 : keys.getFirst().type();
        if (!hypothetical && !direct.isEmpty() && direct.getFirst().type().kind() == SqlType.Kind.ARRAY) type = SqlType.array(type);
        OrderedSet specification = new OrderedSet(call.name(), direct, call.order(), type, keys);
        SqlType tuple = SqlType.record(java.util.stream.IntStream.range(0, keys.size()).mapToObj(i -> new SqlType.Field("f" + i, keys.get(i).type())).toList());
        ScalarPlan.Expression operand = keys.size() == 1 ? keys.getFirst() : new ScalarPlan.Expression(tuple, meter -> SqlValue.ofComposite(tuple, keys.stream().map(key -> key.run(meter)).toList()), false);
        return new AggregatePlan.Fold(call.name(), operand, filter, type, false, call.order(), keys, specification);
    }
    List<SqlValue> keys(SqlValue value) { return order.size() == 1 ? List.of(value) : value.asComposite(); }
    List<SqlValue> sortedRow(SqlValue value, CostMeter meter) {
        List<SqlValue> originals = keys(value), stored = new ArrayList<>();
        for (int i = 0; i < originals.size(); i++) stored.add(AggregatePlan.sortKey(keyExpressions.get(i), originals.get(i), meter));
        if (decorated) stored.addAll(originals);
        return stored;
    }
    List<SqlType> storedTypes() {
        List<SqlType> types = new ArrayList<>();
        for (ScalarPlan.Expression key : keyExpressions) types.add(CollationDerivation.sortType(key));
        if (decorated) for (ScalarPlan.Expression key : keyExpressions) types.add(key.type());
        return types;
    }
    private List<SqlValue> originals(List<SqlValue> row) { return row == null || !decorated ? row : row.subList(order.size(), row.size()); }
    SqlValue finish(RowSpool.Cursor cursor, long count, CostMeter meter) {
        List<SqlValue> values = meter.unmetered(() -> direct.stream().map(value -> value.run(meter)).toList());
        if (hypothetical) return rank(cursor, count, values, meter);
        SqlValue fraction = values.isEmpty() ? null : values.getFirst();
        if (fraction != null && fraction.isNull()) return SqlValue.nullValue(result);
        List<SqlValue> fractions = fraction == null ? List.of() : fraction.type().kind() == SqlType.Kind.ARRAY ? fraction.asArray().elements() : List.of(fraction);
        double[] positions = new double[fractions.size()]; long[] lower = new long[positions.length], upper = new long[positions.length];
        for (int i = 0; i < positions.length; i++) if (!fractions.get(i).isNull()) {
            double p = SqlCasts.cast(fractions.get(i), SqlType.F64).asFloat();
            if (!(p >= 0 && p <= 1)) throw new SqlException("22003", "percentile fraction must be between zero and one");
            positions[i] = name.equals("percentile_disc") ? Math.max(0, Math.ceil(p * count) - 1) : p * (count - 1);
            lower[i] = (long)Math.floor(positions[i]); upper[i] = (long)Math.ceil(positions[i]);
        }
        if (count == 0) return SqlValue.nullValue(result);
        if (name.equals("mode")) {
            SqlValue previous = null, best = null; long run = 0, maximum = 0; List<SqlValue> row;
            while ((row = originals(cursor.next())) != null) {
                SqlValue value = row.getFirst();
                if (previous != null && CollationDerivation.compare(previous, value, null, meter.environment().collation(keyExpressions.getFirst().collation().require())) == 0) run++; else { previous = value; run = 1; }
                if (run > maximum) { maximum = run; best = previous; }
            }
            return best;
        }
        SqlValue[] lows = new SqlValue[positions.length], highs = new SqlValue[positions.length]; long index = 0; List<SqlValue> row;
        while ((row = originals(cursor.next())) != null) {
            for (int i = 0; i < positions.length; i++) { if (lower[i] == index) lows[i] = row.getFirst(); if (upper[i] == index) highs[i] = row.getFirst(); }
            index++;
        }
        SqlType element = result.kind() == SqlType.Kind.ARRAY ? result.element() : result;
        List<SqlValue> output = new ArrayList<>();
        for (int i = 0; i < positions.length; i++) {
            if (fractions.get(i).isNull()) output.add(SqlValue.nullValue(element));
            else if (lower[i] == upper[i]) output.add(lows[i]);
            else if (element.kind() == SqlType.Kind.INTERVAL) output.add(SqlValue.ofInterval(lerp(lows[i].asInterval(), highs[i].asInterval(), positions[i] - lower[i])));
            else output.add(SqlValue.ofFloat(SqlType.F64, lows[i].asFloat() + ((positions[i] - lower[i]) * (highs[i].asFloat() - lows[i].asFloat()))));
        }
        return result.kind() == SqlType.Kind.ARRAY ? SqlValue.ofArray(result, new SqlArray(fraction.asArray().dims(), fraction.asArray().lowerBounds(), output)) : output.getFirst();
    }
    private SqlValue rank(RowSpool.Cursor cursor, long count, List<SqlValue> hypotheticalRow, CostMeter meter) {
        long before = 0, through = 0, distinct = 0; List<SqlValue> row, previous = null;
        while ((row = originals(cursor.next())) != null) {
            int compared = compare(row, hypotheticalRow, meter);
            if (compared < 0) { before++; if (previous == null || compare(row, previous, meter) != 0) distinct++; }
            if (compared <= 0) through++; previous = row;
        }
        return switch (name) {
            case "rank" -> SqlValue.ofInteger(SqlType.I64, before + 1);
            case "dense_rank" -> SqlValue.ofInteger(SqlType.I64, distinct + 1);
            case "percent_rank" -> SqlValue.ofFloat(SqlType.F64, count == 0 ? 0 : (double)before / count);
            default -> SqlValue.ofFloat(SqlType.F64, (double)(through + 1) / (count + 1));
        };
    }
    private int compare(List<SqlValue> a, List<SqlValue> b, CostMeter meter) {
        for (int i = 0; i < order.size(); i++) {
            var key = order.get(i); SqlValue x = a.get(i), y = b.get(i); boolean first = key.nullsFirst() == null ? key.descending() : key.nullsFirst();
            int compared = x.isNull() || y.isNull() ? x.isNull() == y.isNull() ? 0 : x.isNull() == first ? -1 : 1 : CollationDerivation.compare(x, y, null, meter.environment().collation(keyExpressions.get(i).collation().require())) * (key.descending() ? -1 : 1);
            if (compared != 0) return compared;
        }
        return 0;
    }
    private static SqlInterval lerp(SqlInterval low, SqlInterval high, double fraction) {
        SqlInterval delta = high.subtract(low);
        double months = delta.months() * fraction, days = delta.days() * fraction;
        int wholeMonths = (int)months, wholeDays = (int)days;
        double monthDays = Math.rint((months - wholeMonths) * 30 * 1_000_000) / 1_000_000;
        double seconds = Math.rint(((days - wholeDays) + monthDays - (int)monthDays) * 86400 * 1_000_000) / 1_000_000;
        try {
            long carry = (long)(seconds / 86400); seconds -= carry * 86400;
            wholeDays = Math.toIntExact(Math.addExact(Math.addExact(wholeDays, (int)monthDays), carry));
            double micros = Math.rint(delta.micros() * fraction + seconds * 1_000_000);
            if (!Double.isFinite(micros) || micros < Long.MIN_VALUE || micros >= 0x1.0p63) throw Temporal.overflow();
            return low.add(new SqlInterval(wholeMonths, wholeDays, (long)micros));
        } catch (ArithmeticException error) { throw Temporal.overflow(); }
    }
    private static SqlException overload() { return new SqlException("42883", "ordered-set aggregate arguments do not match"); }
}
