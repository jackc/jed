package migrate

import (
	"errors"
	"fmt"

	jed "github.com/jackc/jed/impl/go"
)

// errMoreThanOneSeparator is the raw split-time signal that a file contains two or more
// separator lines; parseMigration wraps it in a LoadError with the file's name.
var errMoreThanOneSeparator = errors.New("more than one separator line (" + Separator + ")")

// MigrationError wraps an engine error raised while running a migration statement
// (design.md §8 — the tern MigrationPgError analogue). It records the migration Name,
// the Direction ("up"/"down"), and the failing Statement text, and unwraps to the
// underlying *jed.EngineError, so a caller can still branch on the SQLSTATE.
type MigrationError struct {
	Name      string
	Direction string
	Statement string
	Err       error
}

func (e *MigrationError) Error() string {
	msg := fmt.Sprintf("migration %q (%s) failed", e.Name, e.Direction)
	if e.Err != nil {
		msg += ": " + e.Err.Error()
	}
	if e.Statement != "" {
		msg += "\n  in statement: " + e.Statement
	}
	return msg
}

func (e *MigrationError) Unwrap() error { return e.Err }

// SqlState returns the underlying engine SQLSTATE when the wrapped error is a
// *jed.EngineError, or "" otherwise — a convenience so callers need not unwrap by hand.
func (e *MigrationError) SqlState() string {
	var ee *jed.EngineError
	if errors.As(e.Err, &ee) {
		return ee.Code()
	}
	return ""
}

// IrreversibleMigration is returned when a down-migration is requested through a migration
// that has no down half (design.md §8).
type IrreversibleMigration struct {
	Sequence int
	Name     string
}

func (e *IrreversibleMigration) Error() string {
	return fmt.Sprintf("migration %d (%q) is irreversible: it has no down migration", e.Sequence, e.Name)
}

// BadVersion is returned when a target version, or the version read from the version table,
// is outside the known range 0 … N (design.md §6/§8) — the migrations directory and the
// database disagree.
type BadVersion struct {
	Version int    // the offending version
	N       int    // the highest known sequence
	Whence  string // "target" or "database" — where the bad value came from
}

func (e *BadVersion) Error() string {
	return fmt.Sprintf("%s version %d is out of range 0 … %d", e.Whence, e.Version, e.N)
}

// LoadError is a load-time failure: a malformed file, a gap or duplicate in the sequence
// numbers, an empty forward half, or an unreadable source (design.md §7/§8). It is raised
// before any statement runs.
type LoadError struct {
	Name string // the file/migration name, when the error is file-specific ("" for set-level errors)
	Msg  string
}

func (e *LoadError) Error() string {
	if e.Name != "" {
		return fmt.Sprintf("migration %q: %s", e.Name, e.Msg)
	}
	return e.Msg
}
