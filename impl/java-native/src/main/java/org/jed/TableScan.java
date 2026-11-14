package org.jed;

import java.io.IOException;
import java.util.List;
import java.util.ArrayList;
import java.util.Arrays;
import org.jed.storage.PersistentTree;

/** Shared mutation scan, retaining residual predicates after the physical key bound. */
final class TableScan implements AutoCloseable {
    private final Engine.Table table;
    private final boolean reverse;
    private final CostMeter meter;
    private boolean prepass;
    private PersistentTree.Cursor cursor;
    private byte[] point;
    private PersistentTree.Cursor indexCursor;
    private int indexPrefixLength;
    private IndexAccess.Bound indexBound;
    private int indexAt;
    private GinAccess.Candidates ginCandidates;
    private List<byte[]> ginKeys;
    private int ginAt;
    private boolean[] ginTouched;
    private boolean done;
    private List<KeyIntervals.Interval> intervals;
    private int intervalAt;
    private boolean intervalPointRead;
    TableScan(Engine.Table table, String alias, SqlParser.Expr predicate, SqlValue[] args, CostMeter meter) throws IOException {
        this(table, alias, table.columns().stream().map(Engine.Column::name).toList(), predicate, args, meter);
    }
    TableScan(Engine.Table table, String alias, List<String> names, SqlParser.Expr predicate, SqlValue[] args, CostMeter meter) throws IOException {
        this(table, alias, names, predicate, args, meter, null, null);
    }
    TableScan(Engine.Table table, String alias, List<String> names, SqlParser.Expr predicate, SqlValue[] args, CostMeter meter, boolean[] touched, QueryEnvironment environment) throws IOException {
        this(table, alias, names, predicate, args, meter, touched, environment, false);
    }
    TableScan(Engine.Table table, String alias, List<String> names, SqlParser.Expr predicate, SqlValue[] args, CostMeter meter, boolean[] touched, QueryEnvironment environment, boolean reverse) throws IOException {
        this(table, alias, names, predicate, args, meter, touched, environment, reverse, null);
    }
    TableScan(Engine.Table table, String alias, List<String> names, SqlParser.Expr predicate, SqlValue[] args, CostMeter meter, boolean[] touched, QueryEnvironment environment, boolean reverse, Engine.Index orderedIndex) throws IOException {
        this(table, alias, names, predicate, args, meter, touched, environment, reverse, orderedIndex, false);
    }
    TableScan(Engine.Table table, String alias, List<String> names, SqlParser.Expr predicate, SqlValue[] args, CostMeter meter, boolean[] touched, QueryEnvironment environment, boolean reverse, Engine.Index orderedIndex, boolean storageOrder) throws IOException {
        this(table, alias, names, predicate, args, meter, touched, environment, reverse, orderedIndex, storageOrder, true);
    }
    TableScan(Engine.Table table, String alias, List<String> names, SqlParser.Expr predicate, SqlValue[] args, CostMeter meter, boolean[] touched, QueryEnvironment environment, boolean reverse, Engine.Index orderedIndex, boolean storageOrder, boolean allowPartialFull) throws IOException {
        this(table, alias, names, predicate, args, meter, touched, environment, reverse, orderedIndex, storageOrder, allowPartialFull, null);
    }
    TableScan(Engine.Table table, String alias, List<String> names, SqlParser.Expr predicate, SqlValue[] args, CostMeter meter, boolean[] touched, QueryEnvironment environment, boolean reverse, Engine.Index orderedIndex, boolean storageOrder, boolean allowPartialFull, AccessCandidate forced) throws IOException {
        this.table = table; this.reverse = reverse; this.meter = meter;
        if (Contradictions.proven(predicate, args)) { done = true; return; }
        if (!CollationCompatibility.table(table, meter)) {
            meter.charge(table.rows().rangePageCount(null, true, null, false) * Costs.PAGE_READ); meter.guard();
            cursor = open(table.rows(), null, null);
            if (touched != null) {
                try { PersistentTree.Entry entry; while ((entry = cursor.next()) != null) charge(entry, touched, meter); }
                finally { cursor.close(); }
                cursor = open(table.rows(), null, null);
            }
            return;
        }
        ScalarPlan.Resolver constants = (environment == null ? new ScalarPlan.Resolver(args) : environment.resolver()).excludeLocal(names, alias == null ? table.name() : alias);
        if (forced != null) {
            if (forced.kind() == AccessCandidate.BTREE || forced.kind() == AccessCandidate.INDEX_SET) {
                indexBound = IndexAccess.find(table, names, alias == null ? table.name() : alias, predicate, constants, meter,
                    new IndexAccess.Selection(forced.index(), forced.predicate(), forced.rows(), forced.pages(), forced.scanCost()));
                if (indexBound == null) throw new IllegalStateException("selected B-tree access is no longer available");
                if (touched != null) {
                    prepass = true;
                    try { PersistentTree.Entry entry; while ((entry = next()) != null) charge(entry, touched, meter); }
                    finally { prepass = false; if (indexCursor != null) indexCursor.close(); indexCursor = null; indexAt = 0; done = false; }
                }
                return;
            }
            if (forced.kind() == AccessCandidate.GIN || forced.kind() == AccessCandidate.GIST) {
                Engine.Table selected = table.withIndexes(List.of(forced.index()));
                if (forced.kind() == AccessCandidate.GIN) {
                    GinAccess.Bound bound = GinAccess.find(selected, names, alias == null ? table.name() : alias, forced.predicate(), constants, meter);
                    if (bound != null && !bound.fullScan()) ginCandidates = GinAccess.gather(bound, meter);
                } else {
                    GistAccess.Bound bound = GistAccess.find(selected, names, alias == null ? table.name() : alias, forced.predicate(), constants, meter);
                    if (bound != null) ginCandidates = GistAccess.gather(bound, meter);
                }
                if (ginCandidates != null) { ginKeys = ginCandidates.keys(); if (reverse) ginKeys = ginKeys.reversed(); ginTouched = touched; return; }
                predicate = null;
            } else predicate = forced.predicate();
            storageOrder = true;
        }
        PrimarySelection primary = primary(table, names, alias == null ? table.name() : alias, predicate, constants, meter);
        if (orderedIndex != null) {
            indexBound = IndexAccess.find(table, names, alias == null ? table.name() : alias, predicate, constants, meter, orderedIndex, allowPartialFull);
            if (indexBound == null) indexBound = new IndexAccess.Bound(orderedIndex, List.of(new IndexAccess.Range(new byte[0], null)), 0, table.rows().size());
            // Ordered LIMIT walks account external values only for rows actually fetched.
            ginTouched = touched;
            return;
        }
        List<ScalarPlan.Expression> key = TableQueryPlan.point(predicate, table, names, alias == null ? table.name() : alias, constants);
        if (key == null && table.primaryKey().length > 1) {
            KeyIntervals.Tuple tuple = KeyIntervals.tuple(predicate, table, names, alias == null ? table.name() : alias, constants, meter, environment);
            if (tuple != null) {
                StatisticsEstimate statistics = new StatisticsEstimate(table, names, alias == null ? table.name() : alias, constants, meter);
                long rows = tuple.intervals().isEmpty() ? 0 : statistics.rows(tuple.predicate(), table.rows().size(), List.of());
                long pages = PlanEstimate.mul(tuple.intervals().size(), table.rows().structuralFacts().height());
                long cost = PlanEstimate.add(pages, PlanEstimate.mul(rows, 1 + PlanEstimate.operations(predicate, constants, meter)));
                if (primary == null || cost < primary.cost()) {
                    intervals = reverse ? new ArrayList<>(tuple.intervals().reversed()) : tuple.intervals();
                    if (touched != null) {
                        prepass = true;
                        try { PersistentTree.Entry entry; while ((entry = next()) != null) charge(entry, touched, meter); }
                        finally { prepass = false; if (cursor != null) cursor.close(); cursor = null; intervalAt = 0; intervalPointRead = false; done = false; }
                    }
                    return;
                }
            }
        }
        if (key == null && table.primaryKey().length == 1) {
            int ordinal = table.primaryKey()[0];
            intervals = KeyIntervals.find(predicate, names.get(ordinal), alias == null ? table.name() : alias, table.columns().get(ordinal).type(), constants, meter, environment, table.columns().get(ordinal).collation());
            if (intervals != null) {
                if (reverse) intervals = new ArrayList<>(intervals.reversed());
                if (touched != null) {
                    prepass = true;
                    try { PersistentTree.Entry entry; while ((entry = next()) != null) charge(entry, touched, meter); }
                    finally { prepass = false; if (cursor != null) cursor.close(); cursor = null; intervalAt = 0; intervalPointRead = false; done = false; }
                }
                return;
            }
        }
        if (key == null && !storageOrder && !table.indexes().isEmpty()) {
            IndexAccess.Bound bound = IndexAccess.find(table, names, alias == null ? table.name() : alias, predicate, constants, meter, null, allowPartialFull);
            long incumbent = bound == null ? PlanEstimate.add(table.rows().structuralFacts().nodeCount(), PlanEstimate.mul(table.rows().size(), 1 + PlanEstimate.operations(predicate, constants, meter))) : PlanEstimate.add(bound.pages(), PlanEstimate.mul(bound.rows(), 1 + PlanEstimate.operations(predicate, constants, meter)));
            if (primary != null && primary.cost() <= incumbent) { bound = null; incumbent = primary.cost(); }
            GistEstimate.Selection gist = GistEstimate.select(table, names, alias == null ? table.name() : alias, predicate, constants, meter, incumbent, bound == null && primary == null);
            if (gist != null) incumbent = gist.cost();
            GinEstimate.Selection gin = GinEstimate.select(table, names, alias == null ? table.name() : alias, predicate, constants, meter, incumbent, true, bound == null && primary == null && gist == null);
            if (gin != null && gin.bound().fullScan()) {
                meter.charge(table.rows().rangePageCount(null, true, null, false) * Costs.PAGE_READ); meter.guard();
                cursor = open(table.rows(), null, null);
                if (touched != null) { try { PersistentTree.Entry entry; while ((entry = cursor.next()) != null) charge(entry, touched, meter); } finally { cursor.close(); } cursor = open(table.rows(), null, null); }
                return;
            }
            if (gin != null) {
                ginCandidates = GinAccess.gather(gin.bound(), meter); ginKeys = ginCandidates.keys(); if (reverse) ginKeys = ginKeys.reversed(); ginTouched = touched; return;
            }
            if (gist != null) {
                ginCandidates = GistAccess.gather(gist.bound(), meter); ginKeys = ginCandidates.keys();
                if (reverse) ginKeys = ginKeys.reversed(); ginTouched = touched; return;
            }
            if (bound != null) {
                indexBound = bound;
                if (touched != null) {
                    prepass = true;
                    try { PersistentTree.Entry entry; while ((entry = next()) != null) charge(entry, touched, meter); }
                    finally { prepass = false; if (indexCursor != null) indexCursor.close(); indexCursor = null; indexAt = 0; done = false; }
                }
                return;
            }
        }
        if (key == null) {
            Bounds bounds = bounds(table, names, alias == null ? table.name() : alias, predicate, constants, meter);
            if (bounds.empty) { done = true; return; }
            meter.charge(table.rows().rangePageCount(bounds.lower, true, bounds.upper, false) * Costs.PAGE_READ); meter.guard();
            cursor = open(table.rows(), bounds.lower, bounds.upper);
            if (touched != null) {
                try { PersistentTree.Entry entry; while ((entry = cursor.next()) != null) charge(entry, touched, meter); }
                finally { cursor.close(); }
                cursor = open(table.rows(), bounds.lower, bounds.upper);
            }
        } else {
            Encoding.Sink bytes = new Encoding.Sink();
            for (int keyAt = 0; keyAt < key.size(); keyAt++) {
                ScalarPlan.Expression expression = key.get(keyAt);
                SqlValue value = expression.run(meter);
                if (value.isNull()) { done = true; return; }
                SqlType target = table.columns().get(table.primaryKey()[keyAt]).type();
                if (target.isInteger() && value.type().isInteger() && !target.equals(value.type())) {
                    if (!target.contains(value.asInteger())) { done = true; return; }
                    value = SqlValue.ofInteger(target, value.asInteger());
                }
                value = keyValue(value, target);
                if (value == null) { done = true; return; }
                bytes.raw(IndexCollations.key(value, table.columns().get(table.primaryKey()[keyAt]).collation(), meter.environment()));
            }
            point = bytes.bytes();
            Bounds clipped = bounds(table, names, alias == null ? table.name() : alias, predicate, constants, meter);
            if (clipped.empty || clipped.lower != null && Arrays.compareUnsigned(point, clipped.lower) < 0 || clipped.upper != null && Arrays.compareUnsigned(point, clipped.upper) >= 0) { point = null; done = true; return; }
            meter.charge(table.rows().pointPageCount(point) * Costs.PAGE_READ); meter.guard();
            if (touched != null) { PersistentTree.Entry entry = table.rows().getEntry(point); if (entry != null) charge(entry, touched, meter); }
        }
    }
    private PersistentTree.Cursor open(PersistentTree tree, byte[] lower, byte[] upper) throws IOException {
        return reverse ? tree.reverseCursor(lower, true, upper, false) : tree.cursor(lower, true, upper, false);
    }
    private void charge(PersistentTree.Entry entry, boolean[] touched, CostMeter meter) {
        StoredRow.ReadCost cost = Encoding.readCost(entry, table.types(), touched, table.rows().layout());
        meter.charge(cost.pages() * Costs.PAGE_READ + cost.decompressions() * Costs.VALUE_DECOMPRESS); meter.guard();
    }
    private record Bounds(byte[] lower, byte[] upper, boolean empty) {}
    record PrimarySelection(SqlParser.Expr predicate, long rows, long pages, long cost) {}
    static PrimarySelection primary(Engine.Table table, List<String> names, String alias, SqlParser.Expr predicate, ScalarPlan.Resolver constants, CostMeter meter) throws IOException {
        if (table.primaryKey().length == 0 || !CollationCompatibility.table(table, meter)) return null;
        List<SqlParser.Expr> terms = new ArrayList<>(); conjuncts(predicate, terms);
        SqlParser.Expr access = null;
        for (int ordinal : table.primaryKey()) {
            String name = names.get(ordinal); Engine.Column definition = table.columns().get(ordinal);
            SqlParser.Expr equal = null, ranges = null;
            for (SqlParser.Expr term : terms) {
                if (!(term instanceof SqlParser.Binary binary) || !List.of("=", "<", "<=", ">", ">=").contains(binary.op())) continue;
                String op = binary.op(); SqlParser.Expr value;
                if (column(binary.left(), name, alias)) value = binary.right();
                else if (column(binary.right(), name, alias)) { value = binary.left(); op = switch (op) { case "<" -> ">"; case "<=" -> ">="; case ">" -> "<"; case ">=" -> "<="; default -> op; }; }
                else continue;
                ScalarPlan.Expression resolved = constant(constants, value, definition.type());
                if (resolved == null || !keyCompatible(resolved.type(), definition.type())) continue;
                if (!op.equals("=") && !CollationDerivation.merge(CollationDerivation.implicit(definition.collation()), resolved.collation()).require().equals(definition.collation())) continue;
                SqlParser.Expr admitted = new SqlParser.Binary(op, new SqlParser.Name(List.of(alias, name)), value);
                if (op.equals("=")) equal = admitted; else ranges = FilterRules.and(ranges, admitted);
            }
            access = FilterRules.and(access, equal == null ? ranges : equal);
            if (equal == null) break;
        }
        if (access == null) return null;
        StatisticsEstimate statistics = new StatisticsEstimate(table, names, alias, constants, meter);
        List<String> unique = table.primaryKey().length == 1 ? List.of(names.get(table.primaryKey()[0])) : List.of();
        var facts = table.rows().structuralFacts(); long rows = statistics.rows(access, table.rows().size(), unique);
        long pages = rows == 0 ? 0 : Math.min(facts.nodeCount(), Math.max(facts.height(), statistics.rows(access, facts.nodeCount(), unique)));
        return new PrimarySelection(access, rows, pages, PlanEstimate.add(pages, PlanEstimate.mul(rows, 1 + PlanEstimate.operations(predicate, constants, meter))));
    }
    private static Bounds bounds(Engine.Table table, List<String> names, String alias, SqlParser.Expr predicate, ScalarPlan.Resolver constants, CostMeter meter) {
        if (table.primaryKey().length == 0) return new Bounds(null, null, false);
        List<SqlParser.Expr> terms = new ArrayList<>(); conjuncts(predicate, terms);
        byte[] prefix = new byte[0], lower = null, upper = null;
        for (int column : table.primaryKey()) {
            byte[] equal = null, columnLower = null, columnUpper = null;
            String name = names.get(column);
            for (SqlParser.Expr term : terms) {
                if (!(term instanceof SqlParser.Binary binary)) continue;
                String op = binary.op();
                if (!List.of("=", "<", "<=", ">", ">=").contains(op)) continue;
                SqlParser.Expr value;
                if (column(binary.left(), name, alias)) value = binary.right();
                else if (column(binary.right(), name, alias)) {
                    value = binary.left(); op = switch (op) { case "<" -> ">"; case "<=" -> ">="; case ">" -> "<"; case ">=" -> "<="; default -> op; };
                } else continue;
                ScalarPlan.Expression expression = constant(constants, value, table.columns().get(column).type());
                if (expression == null) continue;
                SqlType target = table.columns().get(column).type();
                if (!keyCompatible(expression.type(), target)) continue;
                String collation = table.columns().get(column).collation();
                if (!op.equals("=") && !CollationDerivation.merge(CollationDerivation.implicit(collation), expression.collation()).require().equals(collation)) continue;
                SqlValue resolved = expression.run(meter);
                if (resolved.isNull()) return new Bounds(null, null, true);
                if (target.isInteger() && resolved.type().isInteger()) {
                    if (!target.contains(resolved.asInteger())) {
                        boolean below = resolved.asInteger() < 0;
                        if (op.equals("=") || op.startsWith(">") && !below || op.startsWith("<") && below) return new Bounds(null, null, true);
                        continue;
                    }
                    resolved = SqlValue.ofInteger(target, resolved.asInteger());
                }
                resolved = keyValue(resolved, target);
                if (resolved == null) { if (op.equals("=")) return new Bounds(null, null, true); continue; }
                byte[] encoded = concat(prefix, IndexCollations.key(resolved, collation, meter.environment()));
                if (op.equals("=")) {
                    if (equal != null && !Arrays.equals(equal, encoded)) return new Bounds(null, null, true);
                    equal = encoded;
                } else if (op.equals(">") || op.equals(">=")) {
                    byte[] bound = op.equals(">") ? successor(encoded) : encoded;
                    if (bound == null) return new Bounds(null, null, true);
                    if (columnLower == null || Arrays.compareUnsigned(bound, columnLower) > 0) columnLower = bound;
                } else {
                    byte[] bound = op.equals("<=") ? successor(encoded) : encoded;
                    if (bound != null && (columnUpper == null || Arrays.compareUnsigned(bound, columnUpper) < 0)) columnUpper = bound;
                }
            }
            if (equal != null) {
                if (columnLower != null && Arrays.compareUnsigned(equal, columnLower) < 0 || columnUpper != null && Arrays.compareUnsigned(equal, columnUpper) >= 0) return new Bounds(null, null, true);
                prefix = equal; lower = equal; upper = successor(equal);
            } else {
                if (columnLower != null) lower = columnLower;
                if (columnUpper != null) upper = columnUpper;
                break;
            }
        }
        return new Bounds(lower, upper, lower != null && upper != null && Arrays.compareUnsigned(lower, upper) >= 0);
    }
    private static boolean column(SqlParser.Expr expression, String column, String alias) {
        return expression instanceof SqlParser.Name name && name.parts().getLast().equals(column)
            && (name.parts().size() == 1 || name.parts().size() == 2 && name.parts().getFirst().equals(alias));
    }
    private static void conjuncts(SqlParser.Expr expression, List<SqlParser.Expr> terms) {
        if (expression instanceof SqlParser.Binary binary && binary.op().equals("and")) { conjuncts(binary.left(), terms); conjuncts(binary.right(), terms); }
        else if (expression instanceof SqlParser.Between between && !between.negated()) {
            terms.add(new SqlParser.Binary(">=", between.subject(), between.lower())); terms.add(new SqlParser.Binary("<=", between.subject(), between.upper()));
        } else if (expression != null) terms.add(expression);
    }
    private static byte[] concat(byte[] a, byte[] b) { byte[] bytes = Arrays.copyOf(a, a.length + b.length); System.arraycopy(b, 0, bytes, a.length, b.length); return bytes; }
    private static byte[] successor(byte[] bytes) {
        byte[] next = bytes.clone();
        for (int i = next.length - 1; i >= 0; i--) if ((next[i] & 255) != 255) { next[i]++; return Arrays.copyOf(next, i + 1); }
        return null;
    }
    boolean canReadAhead() { return ginKeys == null && intervals == null && indexBound == null; }
    PersistentTree.Entry next() throws IOException {
        if (done) return null;
        if (ginKeys != null) {
            if (ginAt == ginKeys.size()) { done = true; return null; }
            byte[] key = ginKeys.get(ginAt++);
            meter.charge(table.rows().pointPageCount(key) * Costs.PAGE_READ); meter.guard();
            PersistentTree.Entry entry = table.rows().getEntry(key);
            if (entry == null) throw new org.jed.storage.CorruptStorageException("GIN posting names a missing row");
            if (ginTouched != null) charge(entry, ginTouched, meter);
            return entry;
        }
        if (intervals != null) {
            while (intervalAt < intervals.size()) {
                KeyIntervals.Interval interval = intervals.get(intervalAt);
                if (interval.point() != null) {
                    if (!intervalPointRead) {
                        intervalPointRead = true;
                        if (!prepass) { meter.charge(table.rows().pointPageCount(interval.point()) * Costs.PAGE_READ); meter.guard(); }
                        PersistentTree.Entry entry = table.rows().getEntry(interval.point());
                        if (entry != null) return entry;
                    }
                } else {
                    if (cursor == null) {
                        if (!prepass) { meter.charge(table.rows().rangePageCount(interval.lower(), true, interval.upper(), false) * Costs.PAGE_READ); meter.guard(); }
                        cursor = open(table.rows(), interval.lower(), interval.upper());
                    }
                    PersistentTree.Entry entry = cursor.next();
                    if (entry != null) return entry;
                    cursor.close(); cursor = null;
                }
                intervalAt++; intervalPointRead = false;
            }
            done = true; return null;
        }
        if (indexBound != null) {
            while (indexAt < indexBound.ranges().size()) {
                IndexAccess.Range range = indexBound.ranges().get(reverse ? indexBound.ranges().size() - indexAt - 1 : indexAt);
                byte[] prefix = range.lower();
                if (indexCursor == null) {
                    indexPrefixLength = prefix.length;
                    if (!prepass) { meter.charge(indexBound.index().entries().rangePageCount(range.lower(), true, range.upper(), false) * Costs.PAGE_READ); meter.guard(); }
                    indexCursor = open(indexBound.index().entries(), range.lower(), range.upper());
                }
                PersistentTree.Entry entry = indexCursor.next();
                if (entry != null) {
                    byte[] rowKey = IndexKeyReader.primaryKey(entry.key(), indexBound.index().keys());
                    if (!prepass) { meter.charge(table.rows().pointPageCount(rowKey) * Costs.PAGE_READ); meter.guard(); }
                    PersistentTree.Entry row = table.rows().getEntry(rowKey);
                    if (row != null && ginTouched != null) charge(row, ginTouched, meter);
                    return row;
                }
                indexCursor.close(); indexCursor = null; indexAt++;
            }
            done = true; return null;
        }
        if (cursor != null) return cursor.next();
        done = true;
        return table.rows().getEntry(point);
    }
    static boolean keyCompatible(SqlType source, SqlType target) {
        return source.kind() == target.kind() || (source.isInteger() || source.kind() == SqlType.Kind.DECIMAL) && (target.isInteger() || target.kind() == SqlType.Kind.DECIMAL);
    }
    static SqlValue keyValue(SqlValue value, SqlType target) {
        if (value.type().equals(target)) return value;
        if (!keyCompatible(value.type(), target)) return null;
        try {
            SqlValue converted = SqlCasts.cast(value, target);
            return ScalarPlan.compare(value, converted, null) == 0 ? converted : null;
        } catch (SqlException failure) { if (failure.sqlState().equals("22003")) return null; throw failure; }
    }
    static ScalarPlan.Expression constant(ScalarPlan.Resolver resolver, SqlParser.Expr value, SqlType type) {
        SqlParser.Literal literal = DefaultExpression.literal(value); if (literal != null) value = literal;
        if (!(value instanceof SqlParser.Literal || value instanceof SqlParser.Parameter || value instanceof SqlParser.StoredValue || value instanceof SqlParser.Name || value instanceof SqlParser.Cast cast && cast.value() instanceof SqlParser.Literal castLiteral && (castLiteral.kind() == Lexer.Kind.STRING || castLiteral.kind() == Lexer.Kind.WORD))) return null;
        try { ScalarPlan.Expression result = resolver.resolve(value, value instanceof SqlParser.Parameter ? null : type); return value instanceof SqlParser.Parameter && result.type() == SqlType.UNKNOWN ? resolver.resolve(value, type) : result; }
        catch (SqlException error) { if (value instanceof SqlParser.Name && (error.sqlState().equals("42703") || error.sqlState().equals("42P01"))) return null; throw error; }
    }
    @Override public void close() { done = true; if (cursor != null) cursor.close(); if (indexCursor != null) indexCursor.close(); if (ginCandidates != null) { ginCandidates.close(); ginCandidates = null; ginKeys = null; } }
}
