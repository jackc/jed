// Package jed is the Go core of the engine (CLAUDE.md §2): a downstream consumer
// of /spec, the canonical source of truth. Pure Go — no cgo, no FFI. It implements
// the step-1 surface (integer DDL/DML/SELECT) and ships a conformance harness
// (cmd/conformance) that runs the shared corpus.
package jed

import (
	"errors"
	"fmt"
	"strings"
)

// The SqlState type + its Code() mapping are generated from spec/errors/registry.toml
// (the codegen "middle path", CLAUDE.md §5 — see sqlstate.go / spec/design/codegen.md).
// The hand-written EngineError scaffolding below consumes it.

// EngineError is an engine error: a SQLSTATE plus an informational (never-matched)
// message, and optional structured diagnostic fields (spec/design/error-fields.md §3)
// modeled on pgx's pgconn.PgError — the constraint / table / column / data-type name a
// host would otherwise scrape from the message text. An empty string means "not set"
// (pgx's own convention).
type EngineError struct {
	State   SqlState
	Message string
	// ConstraintName is the violated constraint — set for 23505/23514/23503/23P01.
	ConstraintName string
	// TableName is the relation the failing write targeted.
	TableName string
	// ColumnName is the column at fault — set for 23502 (and 22001 truncation).
	ColumnName string
	// DataTypeName is the data type at fault — set for 22003 / 22001.
	DataTypeName string
}

// newError builds an EngineError.
func newError(state SqlState, message string) *EngineError {
	return &EngineError{State: state, Message: message}
}

// Code returns the error's SQLSTATE string.
func (e *EngineError) Code() string { return e.State.Code() }

// Error renders the error. The SQLSTATE is included in the text so that
// `statement error <code>` also matches as a plain regex under the stock
// sqllogictest runner (spec/design/conformance.md §2). The structured fields are
// metadata, not part of the rendered line (spec/design/error-fields.md §3).
func (e *EngineError) Error() string {
	return fmt.Sprintf("%s: %s", e.State.Code(), e.Message)
}

// --- structured-field builders (spec/design/error-fields.md §5) --------------------
// Chainable setters. newError leaves every field "", so existing call sites are
// unaffected; only the raise sites that know an identifier opt in.

func (e *EngineError) withConstraint(name string) *EngineError { e.ConstraintName = name; return e }
func (e *EngineError) withTable(name string) *EngineError      { e.TableName = name; return e }
func (e *EngineError) withColumn(name string) *EngineError     { e.ColumnName = name; return e }
func (e *EngineError) withDataType(name string) *EngineError   { e.DataTypeName = name; return e }

// stampTable annotates a column-store failure (23502 / 22003 / 22001) with the target
// relation: the relation name is in scope at the DML boundary, not inside the coercion
// (spec/design/error-fields.md §4). A no-op for a non-EngineError.
func stampTable(err error, table string) error {
	var e *EngineError
	if errors.As(err, &e) {
		e.TableName = table
	}
	return err
}

// --- typed integrity-violation constructors (spec/design/error-fields.md §5) --------
// One source per message template (mirrors spec/errors/registry.toml), so the prose and
// the structured field can never drift apart. Message text is byte-identical to the prior
// inline concatenations — the conformance corpus matches on it.

// newUniqueViolation — 23505 for a duplicate key under `constraint` (a unique index's name,
// or the derived <table>_pkey) on `table`.
func newUniqueViolation(table, constraint string) *EngineError {
	return newError(UniqueViolation,
		"duplicate key value violates unique constraint: "+constraint).
		withTable(table).withConstraint(constraint)
}

// newCheckViolation — 23514, a row fails CHECK `constraint` on `table`.
func newCheckViolation(table, constraint string) *EngineError {
	return newError(CheckViolation,
		"new row for relation "+table+" violates check constraint "+constraint).
		withTable(table).withConstraint(constraint)
}

// newFKViolationInsert — 23503 child side, an INSERT/UPDATE on `table` references a parent
// key absent under FK `constraint`.
func newFKViolationInsert(table, constraint string) *EngineError {
	return newError(ForeignKeyViolation,
		"insert or update on table "+table+" violates foreign key constraint "+constraint).
		withTable(table).withConstraint(constraint)
}

// newFKViolationDelete — 23503 parent side, a DELETE/UPDATE on `parent` strands a child of
// `child` still referencing it under FK `constraint`.
func newFKViolationDelete(parent, constraint, child string) *EngineError {
	return newError(ForeignKeyViolation,
		"update or delete on table "+parent+" violates foreign key constraint "+constraint+" on table "+child).
		withTable(parent).withConstraint(constraint)
}

// newExclusionViolation — 23P01, a row conflicts with an existing one under EXCLUDE
// `constraint` (the backing GiST index's name) on `table`.
func newExclusionViolation(table, constraint string) *EngineError {
	return newError(ExclusionViolation,
		"conflicting key value violates exclusion constraint: "+constraint).
		withTable(table).withConstraint(constraint)
}

// newNotNullViolation — 23502, a NULL into NOT NULL `column`. The table is stamped at the
// DML boundary (stampTable), where the relation name is in scope.
func newNotNullViolation(column string) *EngineError {
	return newError(NotNullViolation,
		"null value in column "+column+" violates not-null constraint").
		withColumn(column)
}

// pkeyName is the derived <table>_pkey constraint name PostgreSQL auto-assigns a primary
// key (jed persists no such relation — constraints.md §5.4).
func pkeyName(table string) string { return strings.ToLower(table) + "_pkey" }
