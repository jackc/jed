package org.jed;

import java.util.List;

/** Streaming cursor. Scalar projection is evaluated when the caller advances it. */
public final class Rows implements AutoCloseable {
    private final Session session;
    private final QueryPlan plan;
    private final CostMeter meter;
    private final boolean frozenExplain;
    private final Object gate = new Object();
    private boolean ownsSession;
    Rows ownSession() { ownsSession = true; return this; }
    /** Affected DML/CTAS rows, or -1 when the statement carries no count. */
    public long affectedRows() { return plan.affectedRows(); }
    private boolean advancing;
    private boolean closed;
    private boolean released;
    Rows(Session session, QueryPlan plan, CostMeter meter) { this(session, plan, meter, false); }
    Rows(Session session, QueryPlan plan, CostMeter meter, boolean frozenExplain) { this.session = session; this.plan = plan; this.meter = meter; this.frozenExplain = frozenExplain; }
    public List<String> names() { return plan.names(); }
    public List<SqlType> types() { return plan.types(); }
    public long cost() { return meter.cost(); }
    public Row next() {
        synchronized (gate) {
            if (advancing) throw new SqlException("55000", "cursor has an active operation");
            if (closed) return null;
            advancing = true;
        }
        try {
            if (session.closed()) throw new SqlException("55000", "session is closed");
            meter.cancellationGuard();
            Row row = plan.next(meter);
            if (row == null) close();
            synchronized (gate) { return closed ? null : row; }
        } catch (SqlException error) { session.failed(error); close(); throw error.withCost(meter.cost()); }
        catch (RuntimeException | Error error) { close(); throw error; }
        finally {
            boolean release;
            synchronized (gate) {
                advancing = false;
                release = closed;
            }
            if (release) release();
        }
    }
    @Override public void close() {
        boolean release;
        synchronized (gate) {
            closed = true;
            // Keep the session reserved until an in-flight evaluator has returned.
            release = !advancing;
        }
        if (release) release();
    }
    private void release() {
        synchronized (gate) { if (released) return; released = true; }
        try { plan.close(); } finally { session.release(this); if (ownsSession) session.close(); }
    }
}
