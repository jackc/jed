// Package abide is the Go core of the engine (CLAUDE.md §2): a downstream consumer
// of /spec, the canonical source of truth. Pure Go — no cgo, no FFI. It implements
// the step-1 surface (integer DDL/DML/SELECT) and ships a conformance harness
// (cmd/conformance) that runs the shared corpus.
package abide

import "fmt"

// SqlState is a structured error code (CLAUDE.md §5, §10). Its Code() string is the
// canonical 5-char SQLSTATE from spec/errors/registry.toml (cross-checked in tests).
type SqlState int

const (
	// NumericValueOutOfRange is 22003 — integer overflow (CLAUDE.md §8).
	NumericValueOutOfRange SqlState = iota
	// SyntaxError is 42601.
	SyntaxError
	// UndefinedTable is 42P01.
	UndefinedTable
	// UndefinedColumn is 42703.
	UndefinedColumn
	// DatatypeMismatch is 42804.
	DatatypeMismatch
	// FeatureNotSupported is 0A000 (not-yet-implemented surface).
	FeatureNotSupported
)

// Code returns the canonical SQLSTATE string.
func (s SqlState) Code() string {
	switch s {
	case NumericValueOutOfRange:
		return "22003"
	case SyntaxError:
		return "42601"
	case UndefinedTable:
		return "42P01"
	case UndefinedColumn:
		return "42703"
	case DatatypeMismatch:
		return "42804"
	case FeatureNotSupported:
		return "0A000"
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
