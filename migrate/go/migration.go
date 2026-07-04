package migrate

import (
	"strings"

	jed "github.com/jackc/jed/impl/go"
)

// Separator is the magic line that splits a migration file's up half from its down
// half (design.md §4). It is kept verbatim from tern for muscle memory; it is itself a
// valid jed "--" line comment, so a file is inert if ever fed straight to the engine.
const Separator = "---- create above / drop below ----"

// Migration is one loaded migration (design.md §4/§6). Sequence is 1-based; Name is the
// free-form label from the filename. Up is the forward SQL (never empty). Down is the
// reverse SQL; it is empty exactly when the migration is Irreversible (the file had no
// separator).
type Migration struct {
	Sequence     int
	Name         string
	Up           string
	Down         string
	Irreversible bool
}

// parseMigration splits a file's raw contents into a Migration (design.md §4). The file
// is split on the separator line; text before it is the up half, text after it is the
// down half. A file with no separator is up-only (irreversible). The up half must be
// non-empty (only whitespace/comments is a load-time error).
func parseMigration(sequence int, name, contents string) (Migration, error) {
	up, down, hasDown, err := splitHalves(contents)
	if err != nil {
		return Migration{}, &LoadError{Name: name, Msg: err.Error()}
	}
	if !hasSQL(up) {
		return Migration{}, &LoadError{Name: name, Msg: "no SQL in forward migration step"}
	}
	m := Migration{Sequence: sequence, Name: name, Up: up}
	if hasDown {
		m.Down = down
	} else {
		m.Irreversible = true
	}
	return m, nil
}

// splitHalves splits contents on the Separator line into the up half and (optionally) the
// down half. A line is a separator iff its trimmed content equals Separator, so trailing
// whitespace / a Windows "\r" is tolerated. At most one separator is allowed (design.md
// §7): a second separator line is a load-time error rather than silently folding into the
// down half. hasDown is false when the file has no separator (irreversible).
func splitHalves(contents string) (up, down string, hasDown bool, err error) {
	lines := strings.Split(contents, "\n")
	sep := -1
	for i, line := range lines {
		if strings.TrimSpace(line) == Separator {
			if sep != -1 {
				return "", "", false, errMoreThanOneSeparator
			}
			sep = i
		}
	}
	if sep == -1 {
		return contents, "", false, nil
	}
	up = strings.Join(lines[:sep], "\n")
	down = strings.Join(lines[sep+1:], "\n")
	return up, down, true, nil
}

// hasSQL reports whether text contains any SQL beyond whitespace and comments — the check
// that a migration half is non-empty. It reuses the engine's lexer-aware statement splitter
// (jed.SplitStatements), which skips comment-only / blank spans, so a half of only comments
// yields zero statements and is therefore empty.
func hasSQL(text string) bool {
	for range jed.SplitStatements(text) {
		return true
	}
	return false
}
