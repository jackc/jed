// Package jed is the Go core of the engine (CLAUDE.md §2): a downstream consumer
// of /spec, the canonical source of truth. Pure Go — no cgo, no FFI. It implements
// the step-1 surface (integer DDL/DML/SELECT) and ships a conformance harness
// (cmd/conformance) that runs the shared corpus.
package jed

import "fmt"

// SqlState is a structured error code (CLAUDE.md §5, §10). Its Code() string is the
// canonical 5-char SQLSTATE from spec/errors/registry.toml (cross-checked in tests).
type SqlState int

const (
	// NumericValueOutOfRange is 22003 — integer overflow (CLAUDE.md §8).
	NumericValueOutOfRange SqlState = iota
	// DivisionByZero is 22012 — division or modulo by zero.
	DivisionByZero
	// InvalidParameterValue is 22023 — a bad numeric typmod (e.g. numeric(0)).
	InvalidParameterValue
	// InvalidTextRepresentation is 22P02 — malformed text input (e.g. bytea hex).
	InvalidTextRepresentation
	// InvalidRowCountInLimitClause is 2201W — a negative LIMIT count.
	InvalidRowCountInLimitClause
	// InvalidRowCountInOffsetClause is 2201X — a negative OFFSET count.
	InvalidRowCountInOffsetClause
	// NotNullViolation is 23502 — not-null constraint violation.
	NotNullViolation
	// UniqueViolation is 23505 — unique (primary key) constraint violation.
	UniqueViolation
	// SyntaxError is 42601.
	SyntaxError
	// UndefinedTable is 42P01.
	UndefinedTable
	// UndefinedColumn is 42703.
	UndefinedColumn
	// AmbiguousColumn is 42702 — a bare column matching more than one relation in scope
	// (spec/design/grammar.md §15).
	AmbiguousColumn
	// UndefinedObject is 42704 (e.g. an unknown type name).
	UndefinedObject
	// InvalidColumnReference is 42P10 — a SELECT DISTINCT ORDER BY key not in the
	// select list.
	InvalidColumnReference
	// DatatypeMismatch is 42804.
	DatatypeMismatch
	// DuplicateTable is 42P07 (CREATE TABLE of an existing name).
	DuplicateTable
	// DuplicateColumn is 42701 (two columns with the same name).
	DuplicateColumn
	// DuplicateAlias is 42712 — two FROM relations share a label (a self-join needs distinct
	// aliases; spec/design/grammar.md §15).
	DuplicateAlias
	// InvalidTableDefinition is 42P16 (e.g. more than one primary key).
	InvalidTableDefinition
	// FeatureNotSupported is 0A000 (not-yet-implemented surface).
	FeatureNotSupported
	// DataCorrupted is XX001 — a malformed on-disk database file (CLAUDE.md §8).
	DataCorrupted
)

// Code returns the canonical SQLSTATE string.
func (s SqlState) Code() string {
	switch s {
	case NumericValueOutOfRange:
		return "22003"
	case DivisionByZero:
		return "22012"
	case InvalidParameterValue:
		return "22023"
	case InvalidTextRepresentation:
		return "22P02"
	case InvalidRowCountInLimitClause:
		return "2201W"
	case InvalidRowCountInOffsetClause:
		return "2201X"
	case NotNullViolation:
		return "23502"
	case UniqueViolation:
		return "23505"
	case SyntaxError:
		return "42601"
	case UndefinedTable:
		return "42P01"
	case UndefinedColumn:
		return "42703"
	case AmbiguousColumn:
		return "42702"
	case UndefinedObject:
		return "42704"
	case InvalidColumnReference:
		return "42P10"
	case DatatypeMismatch:
		return "42804"
	case DuplicateTable:
		return "42P07"
	case DuplicateColumn:
		return "42701"
	case DuplicateAlias:
		return "42712"
	case InvalidTableDefinition:
		return "42P16"
	case FeatureNotSupported:
		return "0A000"
	case DataCorrupted:
		return "XX001"
	default:
		return "XX000"
	}
}

// EngineError is an engine error: a SQLSTATE plus an informational (never-matched)
// message.
type EngineError struct {
	State   SqlState
	Message string
}

// NewError builds an EngineError.
func NewError(state SqlState, message string) *EngineError {
	return &EngineError{State: state, Message: message}
}

// Code returns the error's SQLSTATE string.
func (e *EngineError) Code() string { return e.State.Code() }

// Error renders the error. The SQLSTATE is included in the text so that
// `statement error <code>` also matches as a plain regex under the stock
// sqllogictest runner (spec/design/conformance.md §2).
func (e *EngineError) Error() string {
	return fmt.Sprintf("%s: %s", e.State.Code(), e.Message)
}
