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
	// CardinalityViolation is 21000 — a scalar subquery used as an expression returned more than
	// one row (spec/design/grammar.md §26).
	CardinalityViolation SqlState = iota
	// DataException is 22000 — the bare class-22 data exception. PostgreSQL raises it for
	// "argument must be empty or one-dimensional array" — array_append/array_prepend on a
	// multidimensional array (spec/design/array-functions.md §3.2).
	DataException
	// NumericValueOutOfRange is 22003 — integer overflow (CLAUDE.md §8).
	NumericValueOutOfRange
	// NullValueNotAllowed is 22004 — PostgreSQL "initial position must not be null" for
	// array_position's optional start subscript (spec/design/array-functions.md §8).
	NullValueNotAllowed
	// InvalidDatetimeFormat is 22007 — malformed timestamp/timestamptz input.
	InvalidDatetimeFormat
	// DatetimeFieldOverflow is 22008 — an out-of-range datetime field or a value beyond the
	// representable int64-microsecond range (spec/design/timestamp.md).
	DatetimeFieldOverflow
	// DivisionByZero is 22012 — division or modulo by zero.
	DivisionByZero
	// InvalidParameterValue is 22023 — a bad numeric typmod (e.g. numeric(0)).
	InvalidParameterValue
	// ArraySubscriptError is 2202E — a multidimensional array built/parsed with non-matching
	// sub-array dimensions, or an array literal with inverted [l:u] bounds (spec/design/array.md §11).
	ArraySubscriptError
	// InvalidTextRepresentation is 22P02 — malformed text input (e.g. bytea hex).
	InvalidTextRepresentation
	// InvalidEscapeSequence is 22025 — a LIKE pattern ending in a lone escape character.
	InvalidEscapeSequence
	// InvalidRowCountInLimitClause is 2201W — a negative LIMIT count.
	InvalidRowCountInLimitClause
	// InvalidRowCountInOffsetClause is 2201X — a negative OFFSET count.
	InvalidRowCountInOffsetClause
	// NotNullViolation is 23502 — not-null constraint violation.
	NotNullViolation
	// UniqueViolation is 23505 — unique (primary key) constraint violation.
	UniqueViolation
	// CheckViolation is 23514 — a candidate row falsified a CHECK expression at
	// INSERT/UPDATE (spec/design/constraints.md §4).
	CheckViolation
	// ActiveSqlTransaction is 25001 — a BEGIN issued while a transaction is already open (no
	// nesting — there is no SAVEPOINT this slice; spec/design/transactions.md §4.2).
	ActiveSqlTransaction
	// ReadOnlySqlTransaction is 25006 — a write statement in a READ ONLY transaction
	// (spec/design/transactions.md §4.3).
	ReadOnlySqlTransaction
	// InFailedSqlTransaction is 25P02 — a statement (other than ROLLBACK/COMMIT) in a failed
	// transaction block; it stays poisoned until the block ends (transactions.md §6).
	InFailedSqlTransaction
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
	// GroupingError is 42803 — a non-aggregated column not in GROUP BY, or an aggregate in a
	// context that disallows one (WHERE / ON / nested; spec/design/aggregates.md §6).
	GroupingError
	// UndefinedFunction is 42883 — an unknown function name in a call (aggregates.md §5).
	UndefinedFunction
	// IndeterminateDatatype is 42P18 — a bind parameter $N whose type cannot be inferred from
	// context (spec/design/api.md §5).
	IndeterminateDatatype
	// UndefinedParameter is 42P02 — a bind parameter $N where none can exist (a CHECK
	// expression; spec/design/constraints.md §4.1).
	UndefinedParameter
	// DuplicateObject is 42710 — a constraint name already taken on this table
	// (spec/design/constraints.md §4.3).
	DuplicateObject
	// WrongObjectType is 42809 — DROP TABLE of an index name, DROP INDEX of a table name
	// (spec/design/indexes.md §2).
	WrongObjectType
	// DependentObjectsStillExist is 2BP01 — DROP TYPE ... RESTRICT of a composite type a
	// table column or another composite field still references (spec/design/composite.md §7).
	DependentObjectsStillExist
	// FeatureNotSupported is 0A000 (not-yet-implemented surface).
	FeatureNotSupported
	// StatementTooComplex is 54001 — a statement's expression / subquery / set-operation nesting
	// depth exceeds the engine's fixed maximum (maxExprDepth). The native-stack-safety gate for
	// untrusted input — deeply-nested SQL would otherwise overflow the recursive-descent parser /
	// resolve / eval walks BEFORE the cost meter runs, so 54P01 cannot catch it (CLAUDE.md §13;
	// spec/design/cost.md §7). Borrows PG's 54001 statement_too_complex name/code, but the trigger
	// is a fixed depth (deterministic, cross-core) not PG's runtime stack probe.
	StatementTooComplex
	// CostLimitExceeded is 54P01 — a query's accrued execution cost reached the caller-set
	// max_cost ceiling and execution was aborted (CLAUDE.md §13; spec/design/cost.md §6).
	// jed-specific (PostgreSQL has no execution-cost ceiling); class 54 program_limit_exceeded.
	CostLimitExceeded
	// IoError is 58030 — an I/O error from the host file layer (spec/design/api.md §2).
	IoError
	// UndefinedFile is 58P01 — open of a database path that does not exist.
	UndefinedFile
	// DuplicateFile is 58P02 — create of a database path that already exists.
	DuplicateFile
	// DataCorrupted is XX001 — a malformed on-disk database file (CLAUDE.md §8).
	DataCorrupted
)

// Code returns the canonical SQLSTATE string.
func (s SqlState) Code() string {
	switch s {
	case CardinalityViolation:
		return "21000"
	case DataException:
		return "22000"
	case NumericValueOutOfRange:
		return "22003"
	case NullValueNotAllowed:
		return "22004"
	case InvalidDatetimeFormat:
		return "22007"
	case DatetimeFieldOverflow:
		return "22008"
	case DivisionByZero:
		return "22012"
	case InvalidParameterValue:
		return "22023"
	case ArraySubscriptError:
		return "2202E"
	case InvalidTextRepresentation:
		return "22P02"
	case InvalidEscapeSequence:
		return "22025"
	case InvalidRowCountInLimitClause:
		return "2201W"
	case InvalidRowCountInOffsetClause:
		return "2201X"
	case NotNullViolation:
		return "23502"
	case UniqueViolation:
		return "23505"
	case CheckViolation:
		return "23514"
	case ActiveSqlTransaction:
		return "25001"
	case ReadOnlySqlTransaction:
		return "25006"
	case InFailedSqlTransaction:
		return "25P02"
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
	case GroupingError:
		return "42803"
	case UndefinedFunction:
		return "42883"
	case IndeterminateDatatype:
		return "42P18"
	case UndefinedParameter:
		return "42P02"
	case DuplicateObject:
		return "42710"
	case WrongObjectType:
		return "42809"
	case DependentObjectsStillExist:
		return "2BP01"
	case FeatureNotSupported:
		return "0A000"
	case StatementTooComplex:
		return "54001"
	case CostLimitExceeded:
		return "54P01"
	case IoError:
		return "58030"
	case UndefinedFile:
		return "58P01"
	case DuplicateFile:
		return "58P02"
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
