package jed

import (
	"reflect"
)

// exprEqual reports whether two parsed expression trees are STRUCTURALLY equal (spec/design/grammar.md
// §10) — the Go equivalent of the Rust core's derived PartialEq on Expr. Used by the SELECT DISTINCT
// ORDER BY restriction (and GROUP BY key matching) to decide whether an expression matches another.
// Token positions (tokPos fields — recorded for view-definition rewrites, spec/design/views.md §5/§7)
// are ignored, so textually-identical fragments (`a + b` here and there) compare equal; everything
// else is reflect.DeepEqual's recursive tree comparison.
func exprEqual(a, b exprNode) bool { return astEqual(reflect.ValueOf(a), reflect.ValueOf(b)) }

var tokPosType = reflect.TypeOf(tokPos(0))

// astEqual is reflect.DeepEqual over an acyclic AST, except that a tokPos value always compares
// equal.
func astEqual(a, b reflect.Value) bool {
	if a.Type() != b.Type() {
		return false
	}
	if a.Type() == tokPosType {
		return true
	}
	switch a.Kind() {
	case reflect.Struct:
		for i := 0; i < a.NumField(); i++ {
			if !astEqual(a.Field(i), b.Field(i)) {
				return false
			}
		}
		return true
	case reflect.Pointer:
		if a.IsNil() || b.IsNil() {
			return a.IsNil() == b.IsNil()
		}
		if a.Pointer() == b.Pointer() {
			return true
		}
		return astEqual(a.Elem(), b.Elem())
	case reflect.Interface:
		if a.IsNil() || b.IsNil() {
			return a.IsNil() == b.IsNil()
		}
		return astEqual(a.Elem(), b.Elem())
	case reflect.Slice:
		if a.IsNil() != b.IsNil() || a.Len() != b.Len() {
			return false
		}
		fallthrough
	case reflect.Array:
		for i := 0; i < a.Len(); i++ {
			if !astEqual(a.Index(i), b.Index(i)) {
				return false
			}
		}
		return true
	case reflect.Map:
		if a.IsNil() != b.IsNil() || a.Len() != b.Len() {
			return false
		}
		iter := a.MapRange()
		for iter.Next() {
			other := b.MapIndex(iter.Key())
			if !other.IsValid() || !astEqual(iter.Value(), other) {
				return false
			}
		}
		return true
	case reflect.Func:
		return a.IsNil() && b.IsNil()
	case reflect.Bool:
		return a.Bool() == b.Bool()
	case reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64:
		return a.Int() == b.Int()
	case reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64, reflect.Uintptr:
		return a.Uint() == b.Uint()
	case reflect.Float32, reflect.Float64:
		return a.Float() == b.Float()
	case reflect.Complex64, reflect.Complex128:
		return a.Complex() == b.Complex()
	case reflect.String:
		return a.String() == b.String()
	default:
		return a.Pointer() == b.Pointer()
	}
}

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

// DefaultTempBuffers is the default per-session RESIDENT budget for SESSION-LOCAL temporary tables, in
// BYTES (spec/design/temp-tables.md §6/§7) — PostgreSQL's temp_buffers. Once a session's temp domain
// outgrows it, a host with a spill directory migrates the domain to a scratch file and bounds its page
// cache by it. Residency only: results, cost, and the 54P03 point never depend on it. 0 ⇒ never spill
// (unbounded residency).
const DefaultTempBuffers = 32 << 20

// DefaultMaxTempStorageBytes is the default per-session storage ceiling for SESSION-LOCAL temporary
// tables, in BYTES (spec/design/temp-tables.md §7). Temp tables RETAIN bytes across statements, which
// neither the per-statement cost ceiling (maxCost) nor the cumulative budget (lifetimeMaxCost) bounds,
// so maxTempStorageBytes is the §13 gate that does: once the temp domain's committed page bytes exceed
// it — resident or spilled alike — the next temp write aborts 54P03. 0 ⇒ unlimited (a trusted handle).
// Identical across cores (§8); the abort point is part of the cross-core contract.
const DefaultMaxTempStorageBytes = 1 << 30

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
