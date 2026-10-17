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
	// DatabaseName is the database holding TableName — `main`, `temp`, or a host attachment's name.
	// jed's analog of PostgreSQL's SchemaName: set whenever TableName is (spec/design/error-fields.md
	// §12).
	DatabaseName string
	// ColumnName is the column at fault — set for 23502 (and 22001 truncation).
	ColumnName string
	// DataTypeName is the data type at fault — set for 22003 / 22001.
	DataTypeName string
	// Detail is PostgreSQL's DETAIL sentence naming the offending values (`Key (id)=(1) already
	// exists.`) — set on the class-23 violations, subject to the session's SELECT privilege
	// (spec/design/error-fields.md §9). Never part of the rendered message.
	Detail string
	// Position is the 1-based **character** offset into the statement text of the offending token —
	// set for lexer/parser errors and name-resolution errors (spec/design/error-fields.md §10). 0 means
	// "not set" (pgx's convention).
	Position int32
	// Hint is PostgreSQL's HINT suggestion (`Perhaps you meant to reference the column "t.name".`) —
	// spec/design/error-fields.md §11. Never part of the rendered message.
	Hint string
	// located is set once an expression node's resolution has decided this error's position (stamped
	// or deliberately left absent), so an enclosing node never claims an error raised inside one of its
	// operands (spec/design/error-fields.md §10.2). Internal bookkeeping, not a field.
	located bool
	// hintDecided is set once a raise site has decided this error's Hint (set it, or deliberately left
	// it absent), so an enclosing context never adds its default hint (spec/design/error-fields.md
	// §11.7). Internal bookkeeping, not a field.
	hintDecided bool
	// hasSlot / slotRow / slotValue record the INSERT VALUES slot (row, value) whose stored value
	// raised this error, set where the row is built so the statement can point the error at the slot's
	// literal (spec/design/error-fields.md §10.2). Internal bookkeeping, not a field.
	hasSlot   bool
	slotRow   int
	slotValue int
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

// withDetail attaches a Detail (spec/design/error-fields.md §9). "" (a gated or inapplicable detail)
// leaves the field absent, so a raise site can pass a helper's result straight through.
func (e *EngineError) withDetail(detail string) *EngineError { e.Detail = detail; return e }

// withPosition sets Position unless one is already set: the innermost site that knows a location
// wins (spec/design/error-fields.md §10.2). A 0 position means "unknown" and is ignored.
func (e *EngineError) withPosition(pos int32) *EngineError {
	if e.Position == 0 && pos > 0 {
		e.Position = pos
	}
	return e
}

// withHint sets Hint unless one is already set (spec/design/error-fields.md §11). "" leaves it absent.
func (e *EngineError) withHint(hint string) *EngineError {
	if e.Hint == "" {
		e.Hint = hint
	}
	return e
}

// hinted sets the fixed Hint text and marks the hint decided (spec/design/error-fields.md §11.7).
func (e *EngineError) hinted(text string) *EngineError {
	e.withHint(text)
	e.hintDecided = true
	return e
}

// noHint decides that this error carries no Hint, so no enclosing context adds one (§11.7).
func (e *EngineError) noHint() *EngineError {
	e.hintDecided = true
	return e
}

// defaultHint is an enclosing context's default Hint (§11.7): set text unless the raise site already
// decided the hint.
func (e *EngineError) defaultHint(text string) *EngineError {
	if e.hintDecided || e.Hint != "" {
		return e
	}
	return e.hinted(text)
}

// atSlot records the VALUES slot value (of row 0) a row-building error came from (§10.2), unless one
// is already recorded. A no-op for a non-EngineError.
func atSlot(err error, value int) error {
	var e *EngineError
	if errors.As(err, &e) && !e.hasSlot {
		e.hasSlot, e.slotRow, e.slotValue = true, 0, value
	}
	return err
}

// atRow moves a recorded VALUES slot to row (§10.2). A no-op for an error with no slot.
func atRow(err error, row int) error {
	var e *EngineError
	if errors.As(err, &e) && e.hasSlot {
		e.slotRow = row
	}
	return err
}

// locateValueSlot points an INSERT VALUES store error at the literal of the slot it came from
// (spec/design/error-fields.md §10.2): see locateLit. An error from an expression slot, a default, or
// a non-VALUES source passes through.
func locateValueSlot(rows [][]insertValue, err error) error {
	var e *EngineError
	if !errors.As(err, &e) || !e.hasSlot || e.slotRow >= len(rows) || e.slotValue >= len(rows[e.slotRow]) {
		return err
	}
	return locateLit(rows[e.slotRow][e.slotValue], err)
}

// locateLit: PostgreSQL coerces a VALUES literal to its column at parse time, so a type mismatch and
// a string's input error point at the literal; a length check (22001) and a number's range check run
// later and carry no position (spec/design/error-fields.md §10.2). A non-literal slot passes through.
func locateLit(iv insertValue, err error) error {
	var e *EngineError
	if !errors.As(err, &e) || iv.IsDefault || iv.IsParam || iv.IsRow || iv.IsArray || iv.IsExpr {
		return err
	}
	positioned := e.State == DatatypeMismatch ||
		(iv.Lit.Kind == literalText && strings.HasPrefix(e.Code(), "22") && e.State != StringDataRightTruncation)
	if positioned {
		e.withPosition(int32(iv.LitPos))
	}
	return err
}

// locateError stamps an error leaving an expression node's resolution with pos unless an inner node
// already decided its position, then marks it decided (spec/design/error-fields.md §10.2). A no-op for
// a non-EngineError.
func locateError(err error, pos srcPos) error {
	var e *EngineError
	if errors.As(err, &e) && !e.located {
		e.withPosition(int32(pos))
		e.located = true
	}
	return err
}

// assignmentHint gives a store-time 42804 (a value of the wrong type for its column or default)
// PostgreSQL's assignment hint, unless its raise site decided otherwise (spec/design/error-fields.md
// §11.7). Any other error passes through.
func assignmentHint(err error) error {
	var e *EngineError
	if errors.As(err, &e) && e.State == DatatypeMismatch {
		e.defaultHint(hintAssignmentMismatch)
	}
	return err
}

// positionError sets an error's Position at pos unless one is already set — a raise site that knows
// the offending expression (spec/design/error-fields.md §10.2). A no-op for a non-EngineError.
func positionError(err error, pos srcPos) error {
	var e *EngineError
	if errors.As(err, &e) {
		e.withPosition(int32(pos))
	}
	return err
}

// atTarget positions a DML target's 42P01 at the target's name (spec/design/error-fields.md §10.2);
// any other error passes through.
func atTarget(err error, pos srcPos) error {
	var e *EngineError
	if errors.As(err, &e) && e.State == UndefinedTable {
		e.withPosition(int32(pos))
	}
	return err
}

// shiftPosition moves a statement error's Position by chars characters — into the coordinates of the
// script that contains the statement (spec/design/error-fields.md §10.1). A no-op for an error with no
// position, or one that is not an EngineError.
func shiftPosition(err error, chars int32) error {
	var e *EngineError
	if errors.As(err, &e) && e.Position > 0 {
		e.Position += chars
	}
	return err
}

// srcPosOf is the position at index i of an index-aligned position list, or 0 when the list does not
// reach it (no positions were recorded).
func srcPosOf(positions []srcPos, i int) int32 {
	if i < len(positions) {
		return int32(positions[i])
	}
	return 0
}

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

// newFKViolationMatchFull — 23503 child side under MATCH FULL, an INSERT/UPDATE on `table` writes
// a referencing tuple that mixes NULL and non-NULL values under FK `constraint` (constraints.md
// §6.3). PostgreSQL's DETAIL names no values, so it is never privilege-gated.
func newFKViolationMatchFull(table, constraint string) *EngineError {
	return newFKViolationInsert(table, constraint).
		withDetail("MATCH FULL does not allow mixing of null and nonnull key values.")
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

// newGeneratedAlwaysInsert is 428C9 — an INSERT supplies a non-DEFAULT value to GENERATED ALWAYS
// identity column without OVERRIDING SYSTEM VALUE, with PostgreSQL's Detail and Hint
// (spec/design/error-fields.md §11.9).
func newGeneratedAlwaysInsert(column string) *EngineError {
	return newError(GeneratedAlways, "cannot insert a non-DEFAULT value into column "+column).
		withDetail(identityAlwaysDetail(column)).
		hinted("Use OVERRIDING SYSTEM VALUE to override.")
}

// newGeneratedAlwaysUpdate is 428C9 — an UPDATE assigns GENERATED ALWAYS identity column a value
// other than DEFAULT, with PostgreSQL's Detail (spec/design/error-fields.md §11.9).
func newGeneratedAlwaysUpdate(column string) *EngineError {
	return newError(GeneratedAlways, "column "+column+" can only be updated to DEFAULT").
		withDetail(identityAlwaysDetail(column)).
		noHint()
}

// identityAlwaysDetail is PostgreSQL's 428C9 Detail for a GENERATED ALWAYS identity column
// (rewriteHandler.c).
func identityAlwaysDetail(column string) string {
	return "Column \"" + column + "\" is an identity column defined as GENERATED ALWAYS."
}

// newNotNullViolation — 23502, a NULL into NOT NULL `column`. The table is stamped at the
// DML boundary (stampTable), where the relation name is in scope.
func newNotNullViolation(column string) *EngineError {
	return newError(NotNullViolation,
		"null value in column "+column+" violates not-null constraint").
		withColumn(column)
}

// pkeyName is the derived <table>_pkey constraint name PostgreSQL auto-assigns a primary
// key, truncated to fit 63 bytes (grammar.md §3.1). jed persists no such relation
// (constraints.md §5.4), so it takes no collision suffix.
func pkeyName(table string) string { return makeObjectName(table, "", false, "pkey") }
