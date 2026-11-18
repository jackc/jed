// Package jed is an embedded SQL database written in pure Go, without cgo or a server. It
// follows PostgreSQL's SQL behavior, uses strict static column types, and keeps each durable
// database in a single file.
//
// # Opening a database
//
// Create makes a new database: set CreateOptions.Path for a file, or leave it empty for an
// in-memory database. Open opens an existing file. Close the Database when you are done and check
// the error: closing can flush pending durable writes.
//
// # Queries and results
//
// Database.Exec runs a statement and returns a Result. Database.Query returns Rows: call Next,
// Scan each row, then check Err. Close the Rows when you stop early; a loop that runs to the end
// closes them. QueryRow scans a single row and returns ErrNoRows when there is none. Parameters
// are written $1, $2, … and passed after the SQL text.
//
// Scan converts columns to Go values. Use Null[T] for a column that may be NULL, or implement
// Scanner for your own types. Value holds any SQL value in full; Value.Scan converts one the way
// Rows.Scan does. MapRows maps rows through RowTo or RowToStructByName as an iterator.
// Prepare parses SQL once into a PreparedStatement that any Database, Session or Transaction can
// run with QueryPrepared, QueryRowPrepared or ExecPrepared.
//
// # Sessions and transactions
//
// A Database is safe for concurrent use. Its query methods each run on a new session, so session
// settings and temporary tables do not carry from one call to the next. Database.Session creates
// a Session that keeps that state; a Session and its Rows must not be used concurrently.
//
// BeginFunc runs a function in a transaction that commits when the function returns nil and rolls
// back when it returns an error or panics. Begin returns a Transaction to end with Commit or
// Rollback. TxOptions selects a read-write (the default) or read-only transaction. There is one
// writer at a time; read-only transactions read a consistent snapshot and never wait for it.
//
// # Limits
//
// Every size and count limit follows one rule: a positive value is the limit, 0 selects the
// documented default, Unlimited removes the limit, and any other negative value fails with
// SQLSTATE 22023. Durations use time.Duration; a nil duration option selects its default.
//
// # Cancellation and errors
//
// Methods that run SQL or wait take a context.Context. Pass the same context to the statements
// inside a transaction to cancel them too. Cancellation is cooperative: work stops at the next
// check, and a commit that has started finishes. A canceled operation returns an error with
// SQLSTATE 57014 that also matches context.Canceled or context.DeadlineExceeded with errors.Is.
// Use errors.As to get an *EngineError and its SQLSTATE.
//
// # Interfaces
//
// Database, Session and Transaction share their query methods, so an application can define a
// small interface with just the methods it calls and accept any of the three. Queryer is the full
// method set, for code that needs all of it.
package jed
