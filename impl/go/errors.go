// Package jed is the Go core of the engine (CLAUDE.md §2): a downstream consumer
// of /spec, the canonical source of truth. Pure Go — no cgo, no FFI. It implements
// the step-1 surface (integer DDL/DML/SELECT) and ships a conformance harness
// (cmd/conformance) that runs the shared corpus.
package jed

import "fmt"

// The SqlState type + its Code() mapping are generated from spec/errors/registry.toml
// (the codegen "middle path", CLAUDE.md §5 — see sqlstate.go / spec/design/codegen.md).
// The hand-written EngineError scaffolding below consumes it.

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
