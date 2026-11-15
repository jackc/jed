// Package jed provides an embedded SQL database implemented in pure Go, without
// cgo or an external database server. It uses strict static SQL types and stores
// each durable database in a single file.
//
// # Opening a database
//
// CreateDatabase creates a new database. Set CreateOptions.Path for a durable
// file, or leave it empty for an in-memory database. OpenDatabase opens an
// existing file; OpenDatabaseWithOptions configures its cache, durability,
// locking, and environment. Close the Database when it is no longer needed,
// checking the returned error: closing can flush pending durable writes.
//
// # Queries and results
//
// Database.Exec executes a statement and returns a Result. Database.Query
// returns Rows; iterate with Rows.Next, scan with Rows.Scan, and check Rows.Err.
// Close the rows when finished, including when abandoning a query early.
// Database.QueryRow combines a query with a single Scan and returns ErrNoRows
// when no row exists. SQL parameters use $1, $2, and so on; pass arguments
// separately from SQL text.
//
// Scan supports Go values, Null[T], and custom Scanner implementations. Value
// preserves SQL values that need the full database representation. MapRows
// lazily maps rows through RowTo or RowToStructByName and yields (value, error);
// once iteration begins, it closes the rows on completion or early exit.
// PreparedStatement holds parsed SQL independently of a session; execute it
// through a handle's QueryPrepared, QueryRowPrepared, or ExecPrepared methods.
//
// # Sessions and transactions
//
// Database can be shared across goroutines. Its query and script methods each
// use a fresh session, so session variables and temporary tables do not carry
// between those calls. Database.Session creates an independent Session for
// persistent connection state. Check its error for invalid settings and close
// the session when done. A Session and its Rows must not be used concurrently.
// Timeouts use time.Duration; a nil optional duration selects its default.
//
// Database.View and Database.Update run callbacks inside read-only and writable
// transactions. They commit on success and roll back on an error or panic.
// Session offers the same helpers, or Begin, Commit, and Rollback for a block
// spanning multiple calls. Only one writer is active at a time; concurrent
// readers observe committed snapshots.
//
// # Cancellation and errors
//
// Query, Exec, transaction admission, and ExecuteScript take a context.Context.
// Pass the same context to queries inside a transaction callback to cancel their
// execution too. Cancellation is cooperative and stops before commit work;
// a completed commit is not reported as canceled. Context cancellation errors
// retain SQLSTATE 57014 and support errors.Is with context.Canceled or
// context.DeadlineExceeded. Use errors.As to inspect an EngineError and its Code.
//
// # Application interfaces
//
// Applications can define small interfaces containing only the methods they
// consume. Database, Session, and Transaction satisfy them implicitly. Queryer
// is available when an application needs the complete query-method set.
package jed
