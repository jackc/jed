package org.jed;

/** A database failure carrying the language-neutral SQLSTATE. */
public final class SqlException extends RuntimeException {
    private static final long serialVersionUID = 1L;
    private final String sqlState;
    private long cost;
    private boolean costAttached;

    public SqlException(String sqlState, String message) {
        super(message);
        if (sqlState == null || sqlState.length() != 5) {
            throw new IllegalArgumentException("SQLSTATE must contain five characters");
        }
        this.sqlState = sqlState;
    }

    public String sqlState() { return sqlState; }
    public long cost() { return cost; }
    SqlException withCost(long cost) {
        if (!costAttached) {
            this.cost = cost;
            costAttached = true;
        }
        return this;
    }

    static SqlException numericOverflow() {
        return new SqlException("22003", "numeric value out of range");
    }

    static SqlException typeMismatch(String message) {
        return new SqlException("42804", message);
    }
}
