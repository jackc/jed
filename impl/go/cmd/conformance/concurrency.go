package main

// The concurrency schedule runner (spec/design/concurrency-testing.md §4). A `.test` file
// carrying a `# format: concurrency` header is an explicit total order over named read/write
// SESSIONS opened on one SharedDb. Because jed read results depend only on the logical order of
// commits and pin-points — never on timing (concurrency-testing.md §2) — executing the listed
// order on a single thread yields the canonical, deterministic result every core must produce.
// This is the stepped-sequential mode; the stepped-threaded mode (same order under -race) is a
// follow-on. Layers 2 (gate-blocking) and 3 (parallel stress) are specified but not built here.
//
// The result grammar (statement / query, sortmodes, the R float tag) is reused verbatim from the
// sequential runner (main.go) — only the session control + state assertions are new.

import (
	"fmt"
	"sort"
	"strconv"
	"strings"

	"jed"
)

// isConcurrencyFormat reports whether text opts into the schedule format via a
// `# format: concurrency` header line. Any other (or absent) format is the sequential runner.
func isConcurrencyFormat(text string) bool {
	for _, line := range strings.Split(text, "\n") {
		t := strings.TrimSpace(line)
		if !strings.HasPrefix(t, "#") {
			continue
		}
		rest := strings.TrimSpace(strings.TrimPrefix(t, "#"))
		if v, ok := strings.CutPrefix(rest, "format:"); ok {
			return strings.TrimSpace(v) == "concurrency"
		}
	}
	return false
}

// cSession is one open handle in a schedule: exactly one of read/write is set.
type cSession struct {
	read  *jed.ReadHandle
	write *jed.WriteHandle
}

// execute runs sql against the session's handle, returning the outcome. A read session's writes
// are rejected with 25006 by the handle itself (without poisoning it).
func (s *cSession) execute(sql string) (jed.Outcome, error) {
	if s.write != nil {
		return s.write.Execute(sql, nil)
	}
	return s.read.Execute(sql, nil)
}

// runConcurrencyFile runs one `# format: concurrency` file against a fresh SharedDb.
func runConcurrencyFile(text string) error {
	db := jed.NewSharedDB()
	sessions := map[string]*cSession{}
	lines := strings.Split(text, "\n")
	i := 0
	for i < len(lines) {
		line := strings.TrimSpace(lines[i])
		if line == "" || strings.HasPrefix(line, "#") {
			i++
			continue
		}
		fields := strings.Fields(line)
		switch fields[0] {
		case "open":
			if len(fields) < 3 {
				return fmt.Errorf("open needs `<sid> read|write`: %q", line)
			}
			sid, mode := fields[1], fields[2]
			if _, dup := sessions[sid]; dup {
				return fmt.Errorf("session %q already open", sid)
			}
			switch mode {
			case "read":
				sessions[sid] = &cSession{read: db.Read()}
			case "write":
				sessions[sid] = &cSession{write: db.Write()}
			default:
				return fmt.Errorf("unknown session mode %q (want read|write)", mode)
			}
			i++
		case "commit", "rollback", "close":
			if len(fields) < 2 {
				return fmt.Errorf("%s needs a session id: %q", fields[0], line)
			}
			sid := fields[1]
			s, ok := sessions[sid]
			if !ok {
				return fmt.Errorf("%s of unknown session %q", fields[0], sid)
			}
			if err := endSession(fields[0], s); err != nil {
				return fmt.Errorf("%s %s: %w", fields[0], sid, err)
			}
			delete(sessions, sid)
			i++
		case "expect":
			if len(fields) < 3 {
				return fmt.Errorf("expect needs `version|oldest_live <n>`: %q", line)
			}
			want, err := strconv.ParseUint(fields[2], 10, 64)
			if err != nil {
				return fmt.Errorf("expect value not a uint: %q", line)
			}
			var got uint64
			switch fields[1] {
			case "version":
				got = db.Version()
			case "oldest_live":
				got = db.OldestLiveTxid()
			default:
				return fmt.Errorf("unknown expect kind %q (want version|oldest_live)", fields[1])
			}
			if got != want {
				return fmt.Errorf("expect %s %d, got %d", fields[1], want, got)
			}
			i++
		case "on":
			if len(fields) < 3 {
				return fmt.Errorf("on needs `<sid> <record>`: %q", line)
			}
			sid := fields[1]
			s, ok := sessions[sid]
			if !ok {
				return fmt.Errorf("on unknown session %q", sid)
			}
			i++
			if err := runConcurrencyRecord(s, sid, fields[2:], lines, &i); err != nil {
				return err
			}
		default:
			return fmt.Errorf("unknown concurrency directive %q", fields[0])
		}
	}
	if len(sessions) != 0 {
		open := make([]string, 0, len(sessions))
		for sid := range sessions {
			open = append(open, sid)
		}
		sort.Strings(open) // deterministic message; map order must never leak (CLAUDE.md §8)
		return fmt.Errorf("file ended with sessions still open: %s", strings.Join(open, ", "))
	}
	return nil
}

// endSession ends a session: commit/rollback a write session, close a read session.
func endSession(kind string, s *cSession) error {
	switch kind {
	case "close":
		if s.read == nil {
			return fmt.Errorf("close of a write session (use commit/rollback)")
		}
		s.read.Close()
	case "commit":
		if s.write == nil {
			return fmt.Errorf("commit of a read session (use close)")
		}
		return s.write.Commit()
	case "rollback":
		if s.write == nil {
			return fmt.Errorf("rollback of a read session (use close)")
		}
		return s.write.Rollback()
	}
	return nil
}

// concurrencyDirectives are the line-leading keywords that bound a record body. Unlike the
// sequential format, a schedule does not separate records with blank lines, so an `on` record's
// SQL (and a query's expected rows) runs until the next directive, a blank line, or a comment.
var concurrencyDirectives = map[string]bool{
	"open": true, "on": true, "commit": true, "rollback": true, "close": true, "expect": true,
}

// isBoundary reports whether line ends the current record body: blank, a comment, or the start of
// the next schedule directive.
func isBoundary(line string) bool {
	t := strings.TrimSpace(line)
	if t == "" || strings.HasPrefix(t, "#") {
		return true
	}
	fields := strings.Fields(t)
	return len(fields) > 0 && concurrencyDirectives[fields[0]]
}

// takeConcurrencySQL reads a statement's SQL body: lines from *i up to the next record boundary.
func takeConcurrencySQL(lines []string, i *int) string {
	var sql []string
	for *i < len(lines) && !isBoundary(lines[*i]) {
		sql = append(sql, lines[*i])
		*i++
	}
	return strings.Join(sql, "\n")
}

// takeConcurrencyQuery reads a query body: SQL up to the `----` separator, then expected rows up to
// the next record boundary.
func takeConcurrencyQuery(lines []string, i *int) (sql string, expected []string) {
	var body []string
	for *i < len(lines) {
		if strings.TrimSpace(lines[*i]) == "----" {
			*i++
			break
		}
		body = append(body, lines[*i])
		*i++
	}
	for *i < len(lines) && !isBoundary(lines[*i]) {
		expected = append(expected, strings.TrimSpace(lines[*i]))
		*i++
	}
	return strings.Join(body, "\n"), expected
}

// runConcurrencyRecord runs one `on <sid> <record>` body (a sqllogictest statement/query) against
// session s, advancing i past the record's SQL and any expected rows.
func runConcurrencyRecord(s *cSession, sid string, rec, lines []string, i *int) error {
	switch rec[0] {
	case "statement":
		expect := ""
		if len(rec) > 1 {
			expect = rec[1]
		}
		sql := takeConcurrencySQL(lines, i)
		_, err := s.execute(sql)
		switch expect {
		case "ok":
			if err != nil {
				return fmt.Errorf("[%s] statement expected ok, got error %s\n  SQL: %s", sid, err.Error(), sql)
			}
		case "error":
			want := ""
			if len(rec) > 2 {
				want = rec[2]
			}
			if err == nil {
				return fmt.Errorf("[%s] statement expected error %s, but it succeeded\n  SQL: %s", sid, want, sql)
			}
			if got := codeOf(err); got != want {
				return fmt.Errorf("[%s] statement expected error %s, got %s\n  SQL: %s", sid, want, got, sql)
			}
		default:
			return fmt.Errorf("[%s] unknown statement kind %q", sid, expect)
		}
	case "query":
		coltypes := ""
		sortmode := "nosort"
		if len(rec) > 1 {
			coltypes = rec[1]
		}
		if len(rec) > 2 {
			sortmode = rec[2]
		}
		sql, expected := takeConcurrencyQuery(lines, i)
		outcome, err := s.execute(sql)
		if err != nil {
			return fmt.Errorf("[%s] query failed with %s\n  SQL: %s", sid, err.Error(), sql)
		}
		cols := len(coltypes)
		if cols == 0 {
			cols = 1
		}
		actual := renderOutcome(outcome, cols, sortmode)
		expected = applySort(expected, cols, sortmode)
		if !equalColtyped(actual, expected, coltypes, cols) {
			return fmt.Errorf("[%s] query result mismatch\n  SQL: %s\n  expected: %v\n  actual:   %v", sid, sql, expected, actual)
		}
	default:
		return fmt.Errorf("[%s] unknown record kind %q", sid, rec[0])
	}
	return nil
}
