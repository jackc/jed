package jed

import (
	"reflect"
)

// exprEqual reports whether two parsed expression trees are STRUCTURALLY equal (spec/design/grammar.md
// §10) — the Go equivalent of the Rust core's derived PartialEq on Expr. Used by the SELECT DISTINCT
// ORDER BY restriction to decide whether an expression sort key matches a select-list expression. The
// AST carries no source positions, so textually-identical fragments (`a + b` here and there) compare
// equal; following the node pointers is exactly the recursive tree comparison.
func exprEqual(a, b exprNode) bool { return reflect.DeepEqual(a, b) }

// Statement executor (CLAUDE.md §10).
//
// SCAFFOLD (step-5 Phase A): dispatches a parsed statement; each arm is filled in
// feature-by-feature (Phases B–E).

// outcomeKind distinguishes a bare statement result from a query result set.
type outcomeKind int

const (
	// outcomeStatement is a statement producing no result set (CREATE, INSERT).
	outcomeStatement outcomeKind = iota
	// outcomeQuery is a query result set.
	outcomeQuery
)

// outcome is the result of executing one statement. Cost is the deterministic execution
// cost accrued while running it (CLAUDE.md §13) — a DML statement accrues its scan +
// filter cost even though it returns no rows.
type outcome struct {
	Kind outcomeKind
	// ColumnNames are the output column names of a query result (nil for a non-query
	// statement); the column count is len(ColumnNames) (spec/design/grammar.md §8).
	ColumnNames []string
	// ColumnTypes is the canonical name of each output column's resolved type (parallel to
	// ColumnNames; nil for a non-query statement) — i16/i32/i64/text/boolean/decimal/…,
	// or "unknown" for an untyped NULL column. It is the resolved SCALAR type — for decimal the
	// unconstrained "decimal", not the numeric(p,s) typmod (spec/design/conformance.md §7).
	ColumnTypes []string
	Rows        [][]Value
	Cost        int64
	// RowsAffected is how many rows a DML statement (INSERT/UPDATE/DELETE without
	// RETURNING) touched — PostgreSQL's command-tag count (spec/design/api.md §4).
	// HasRowsAffected distinguishes a DML statement that matched nothing (0, true)
	// from DDL and transaction control, which have no row count (0, false).
	RowsAffected    int64
	HasRowsAffected bool
}

// DefaultPageSize is the default serialization page size (8 KiB — spec/design/storage.md §3),
// used for a fresh in-memory or newly-created database when no explicit size is given.
const DefaultPageSize uint32 = 8192

// DefaultMaxSQLLength is the default per-handle input-SQL byte limit (1 MiB — CLAUDE.md §13;
// spec/design/api.md §8, cost.md §7). The §13 input-size gate's default ceiling: generous for
// hand-written / ORM SQL, yet bounds the parse tree to a few MB so unbounded untrusted input
// cannot exhaust memory. A caller raises it (trusted bulk loads) or sets 0 for unlimited via
// SetMaxSQLLength. Identical across cores (§8).
const DefaultMaxSQLLength = 1 << 20

// DefaultTempBuffers is the default per-session storage budget for SESSION-LOCAL temporary tables, in
// BYTES (spec/design/temp-tables.md §7). Temp tables RETAIN bytes across statements, which neither the
// per-statement cost ceiling (maxCost) nor the cumulative budget (lifetimeMaxCost) bounds, so
// tempBuffers is the §13 gate that does: the instant a session's resident temp storage (byte-identical
// on-disk record bytes) would exceed it, the write aborts 54P03. 0 ⇒ unlimited (a trusted handle).
// Identical across cores (§8); the abort point is part of the cross-core contract.
const defaultTempBuffers = 32 << 20

// maxCompositeDepth is the maximum composite-type nesting depth (CLAUDE.md §13; spec/design/cost.md
// §7b). A composite type's depth is the length of its deepest chain of nested composites, counting
// itself: a row of scalars is depth 1, `CREATE TYPE b AS (x a)` is `1 + depth(a)`, and an array
// field counts as its element (array levels are not composite levels — CompositeRefOf looks through
// one array level the same way). A CREATE TYPE whose result would exceed this is rejected 54001, and
// a loaded catalog that exceeds it is treated as corrupt XX001 — bounding the native recursion of
// every derived walk (value codec, comparator, record_out/record_in, ResolveColType) at the two
// producers (DDL + load) so all downstream walks are transitively stack-safe. A fixed, cross-core
// constant like maxExprDepth (§8). The chain is built across many cheap statements, so neither the
// per-statement input-size cap nor the parser nesting counter sees it (cost.md §7).
const maxCompositeDepth = 32
