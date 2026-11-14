package org.jed;

import java.util.ArrayList;
import java.util.List;

/** Bounded speculative host work; all SQL work and result validation remain at scalar replay. */
final class HostBatch {
    private HostBatch() {}
    interface Source { List<SqlValue> next(); }
    static final class Window {
        private final List<HostFunctions.Call> calls;
        private final ScalarPlan.Frame frame;
        private final Source source;
        private final List<List<SqlValue>> rows = new ArrayList<>();
        private RuntimeException deferred;
        private int at;
        Window(List<SqlParser.Expr> roots, ScalarPlan.Resolver resolver, ScalarPlan.Frame frame, Source source) {
            calls = new ArrayList<>(); for (SqlParser.Expr root : roots) eligible(root, resolver, calls);
            this.frame = frame; this.source = source;
        }
        boolean active() { return !calls.isEmpty(); }
        void close() { rows.clear(); deferred = null; for (HostFunctions.Call call : calls) call.prefetched.clear(); }
        List<SqlValue> next(CostMeter meter, long remaining, long firstCost) {
            if (calls.isEmpty()) return source.next();
            if (at == rows.size()) {
                if (deferred != null) { RuntimeException failure = deferred; deferred = null; throw failure; }
                rows.clear(); at = 0; for (HostFunctions.Call call : calls) call.prefetched.clear();
                int width = (int)Math.min(Math.min(64, remaining), meter.speculationRows(firstCost));
                for (int i = 0; i < width; i++) {
                    try { List<SqlValue> row = source.next(); if (row == null) break; rows.add(row); }
                    catch (RuntimeException failure) { deferred = failure; break; }
                }
                if (rows.isEmpty()) { if (deferred != null) { RuntimeException failure = deferred; deferred = null; throw failure; } return null; }
                List<SqlValue> saved = frame.values;
                try { for (HostFunctions.Call call : calls) prefetch(call, rows, frame, meter); }
                finally { frame.values = saved; }
            }
            return rows.get(at++);
        }
    }
    private static void eligible(SqlParser.Expr syntax, ScalarPlan.Resolver resolver, List<HostFunctions.Call> result) {
        if (syntax == null) return;
        SqlParser.Expr expression = DefaultExpression.unwrap(syntax);
        if (expression instanceof SqlParser.Call call) {
            for (HostFunctions.Call node : resolver.hostCalls.getOrDefault(call, List.of())) {
                if (!node.function.batched() || node.function.volatility() == HostFunction.Volatility.VOLATILE || call.args().isEmpty()) continue;
                boolean trivial = true;
                for (SqlParser.Expr arg : call.args()) {
                    SqlParser.Expr value = DefaultExpression.unwrap(arg);
                    if (!(value instanceof SqlParser.Name || value instanceof SqlParser.ColumnSlot || value instanceof SqlParser.Parameter || value instanceof SqlParser.Literal || value instanceof SqlParser.StoredValue)) trivial = false;
                }
                if (trivial && !result.contains(node)) result.add(node);
            }
            return;
        }
        if (expression instanceof SqlParser.Cast cast) eligible(cast.value(), resolver, result);
        else if (expression instanceof SqlParser.Unary unary && List.of("-", "not", "is null", "is not null").contains(unary.op())) eligible(unary.value(), resolver, result);
        else if (expression instanceof SqlParser.Binary binary && List.of("+", "-", "*", "/", "%", "=", "<>", "!=", "<", ">", "<=", ">=").contains(binary.op())) {
            eligible(binary.left(), resolver, result); eligible(binary.right(), resolver, result);
        }
    }
    private static void prefetch(HostFunctions.Call call, List<List<SqlValue>> rows, ScalarPlan.Frame frame, CostMeter meter) {
        int count = (int)Math.min(rows.size(), meter.speculationRows(call.function.cost()));
        var columns = new ArrayList<List<SqlValue>>(); for (int a = 0; a < call.arguments.size(); a++) columns.add(new ArrayList<>());
        var accepted = new ArrayList<List<SqlValue>>();
        for (int i = 0; i < count; i++) {
            List<SqlValue> row = rows.get(i);
            // Unfetched stored cells are not opened speculatively. A safe peek-only view
            // makes a lazy argument opt out without moving its I/O or error boundary.
            frame.values = row instanceof StoredRow.Values values ? values.residentView() : row;
            var values = new ArrayList<SqlValue>(); boolean skip = false;
            try {
                for (int a = 0; a < call.arguments.size(); a++) {
                    SqlValue value = call.arguments.get(a).run(meter);
                    if (value.isNull()) { skip = true; break; }
                    values.add(SqlCasts.cast(value, call.function.arguments().get(a), null));
                }
            } catch (Unfetched ignored) { skip = true; }
            catch (SqlException failure) { skip = true; }
            if (skip) continue;
            accepted.add(row); for (int a = 0; a < values.size(); a++) columns.get(a).add(values.get(a));
        }
        if (accepted.isEmpty()) return;
        HostFunctions.Batch batch = HostFunctions.invoke(call.function, columns.stream().map(List::copyOf).toList(), accepted.size());
        for (int i = 0; i < batch.values().size(); i++) call.prefetched.put(accepted.get(i), new Outcome(batch.values().get(i), null));
        if (batch.error() != null) call.prefetched.put(accepted.get(batch.values().size()), new Outcome(null, batch.error()));
    }
    record Outcome(SqlValue value, SqlException error) {}
    static final class Unfetched extends RuntimeException {
        private static final long serialVersionUID = 1L;
        Unfetched() { super(null, null, false, false); }
    }
}
