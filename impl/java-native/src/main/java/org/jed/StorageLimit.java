package org.jed;

/** Database-scoped committed page admission, after statement resource checks. */
final class StorageLimit {
    private StorageLimit() {}
    static SqlException exceeded(long limit) { return new SqlException("54P06", "storage of database \"main\" exceeded the limit of " + limit + " bytes"); }
}
