package main

// The concurrency schedule runner (spec/design/concurrency-testing.md §4). A `.test` file carrying
// a `# format: concurrency` header is an explicit total order over named read/write SESSIONS opened
// on one SharedDb. Because jed read results depend only on the logical order of commits and
// pin-points — never on timing (concurrency-testing.md §2) — executing the listed order yields the
// canonical, deterministic result every core must produce. Two execution modes share ONE parse:
//
//   - stepped-SEQUENTIAL (the binary's default): walk the steps on one goroutine. This DEFINES the
//     canonical output every core (including single-threaded TS) must reproduce.
//   - stepped-THREADED (opt-in, `go test -race`): one goroutine per session, the listed order
//     enforced by a turn token (the driver sends a command and waits for the worker's reply — and,
//     for an end step, joins the goroutine — before advancing). Same schedule, same deterministic
//     result, but every operation runs on a real goroutine against the shared handle, so the race
//     detector exercises the actual concurrency implementation (concurrency-testing.md §4.3). Driven
//     by concurrency_threaded_test.go.
//
// Layers 2 (gate-blocking) and 3 (parallel stress) are specified but not built here.
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

// cRecord is one sqllogictest record body run via `on <sid>` (a statement or a query).
type cRecord struct {
	kind     string // "statement" | "query"
	expect   string // statement: "ok" | "error"
	code     string // statement error: the expected SQLSTATE
	coltypes string // query: the per-column render tags
	sortmode string // query: nosort | rowsort | valuesort
	sql      string
	expected []string // query: the expected rendered rows
}

// cStep is one step of a schedule — the parsed form both execution modes consume.
type cStep struct {
	kind       string // "open" | "on" | "commit" | "rollback" | "close" | "expect"
	sid        string
	mode       string  // open: read | write
	rec        cRecord // on
	expectKind string  // expect: version | oldest_live
	expectVal  uint64
}

// cSession is one open handle in a schedule: exactly one of read/write is set.
type cSession struct {
	read  *jed.ReadHandle
	write *jed.WriteHandle
}

// execute runs sql against the session's handle, returning the outcome. A read session's writes are
// rejected with 25006 by the handle itself (without poisoning it).
func (s *cSession) execute(sql string) (jed.Outcome, error) {
	if s.write != nil {
		return s.write.Execute(sql, nil)
	}
	return s.read.Execute(sql, nil)
}

// --- parsing (shared by both modes) ----------------------------------------------------------

// concurrencyDirectives are the line-leading keywords that bound a record body. Unlike the
// sequential format, a schedule does not separate records with blank lines, so an `on` record's SQL
// (and a query's expected rows) runs until the next directive, a blank line, or a comment.
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

// parseRecord parses one `on <sid> <record>` body (the record kind + its SQL/expected rows),
// advancing *i past it.
func parseRecord(rec, lines []string, i *int) (cRecord, error) {
	switch rec[0] {
	case "statement":
		expect := ""
		if len(rec) > 1 {
			expect = rec[1]
		}
		code := ""
		if len(rec) > 2 {
			code = rec[2]
		}
		return cRecord{kind: "statement", expect: expect, code: code, sql: takeConcurrencySQL(lines, i)}, nil
	case "query":
		coltypes := ""
		if len(rec) > 1 {
			coltypes = rec[1]
		}
		sortmode := "nosort"
		if len(rec) > 2 {
			sortmode = rec[2]
		}
		sql, expected := takeConcurrencyQuery(lines, i)
		return cRecord{kind: "query", coltypes: coltypes, sortmode: sortmode, sql: sql, expected: expected}, nil
	default:
		return cRecord{}, fmt.Errorf("unknown record kind %q", rec[0])
	}
}

// parseSchedule parses a `# format: concurrency` file into its schedule (the steps both modes run).
func parseSchedule(text string) ([]cStep, error) {
	lines := strings.Split(text, "\n")
	var steps []cStep
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
				return nil, fmt.Errorf("open needs `<sid> read|write`: %q", line)
			}
			steps = append(steps, cStep{kind: "open", sid: fields[1], mode: fields[2]})
			i++
		case "commit", "rollback", "close":
			if len(fields) < 2 {
				return nil, fmt.Errorf("%s needs a session id: %q", fields[0], line)
			}
			steps = append(steps, cStep{kind: fields[0], sid: fields[1]})
			i++
		case "expect":
			if len(fields) < 3 {
				return nil, fmt.Errorf("expect needs `version|oldest_live <n>`: %q", line)
			}
			want, err := strconv.ParseUint(fields[2], 10, 64)
			if err != nil {
				return nil, fmt.Errorf("expect value not a uint: %q", line)
			}
			if fields[1] != "version" && fields[1] != "oldest_live" {
				return nil, fmt.Errorf("unknown expect kind %q (want version|oldest_live)", fields[1])
			}
			steps = append(steps, cStep{kind: "expect", expectKind: fields[1], expectVal: want})
			i++
		case "on":
			if len(fields) < 3 {
				return nil, fmt.Errorf("on needs `<sid> <record>`: %q", line)
			}
			sid := fields[1]
			i++
			rec, err := parseRecord(fields[2:], lines, &i)
			if err != nil {
				return nil, err
			}
			steps = append(steps, cStep{kind: "on", sid: sid, rec: rec})
		default:
			return nil, fmt.Errorf("unknown concurrency directive %q", fields[0])
		}
	}
	return steps, nil
}

// runRecord runs one `on <sid>` record against exec (a session's Execute), returning the first
// mismatch as an error. exec is a function so the same logic drives both the sequential map and a
// worker goroutine's handle.
func runRecord(exec func(string) (jed.Outcome, error), sid string, rec cRecord) error {
	switch rec.kind {
	case "statement":
		_, err := exec(rec.sql)
		switch rec.expect {
		case "ok":
			if err != nil {
				return fmt.Errorf("[%s] statement expected ok, got error %s\n  SQL: %s", sid, err.Error(), rec.sql)
			}
		case "error":
			if err == nil {
				return fmt.Errorf("[%s] statement expected error %s, but it succeeded\n  SQL: %s", sid, rec.code, rec.sql)
			}
			if got := codeOf(err); got != rec.code {
				return fmt.Errorf("[%s] statement expected error %s, got %s\n  SQL: %s", sid, rec.code, got, rec.sql)
			}
		default:
			return fmt.Errorf("[%s] unknown statement kind %q", sid, rec.expect)
		}
	case "query":
		outcome, err := exec(rec.sql)
		if err != nil {
			return fmt.Errorf("[%s] query failed with %s\n  SQL: %s", sid, err.Error(), rec.sql)
		}
		cols := len(rec.coltypes)
		if cols == 0 {
			cols = 1
		}
		actual := renderOutcome(outcome, cols, rec.sortmode)
		expected := applySort(rec.expected, cols, rec.sortmode)
		if !equalColtyped(actual, expected, rec.coltypes, cols) {
			return fmt.Errorf("[%s] query result mismatch\n  SQL: %s\n  expected: %v\n  actual:   %v", sid, rec.sql, expected, actual)
		}
	default:
		return fmt.Errorf("[%s] unknown record kind %q", sid, rec.kind)
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

// --- stepped-sequential mode (the binary's default; defines canonical output) ----------------

// runConcurrencyFile runs one `# format: concurrency` file in the canonical stepped-sequential mode.
func runConcurrencyFile(text string) error {
	steps, err := parseSchedule(text)
	if err != nil {
		return err
	}
	return runScheduleSequential(steps)
}

// runScheduleSequential executes a schedule on a single goroutine: the canonical transcript.
func runScheduleSequential(steps []cStep) error {
	db := jed.NewSharedDB()
	sessions := map[string]*cSession{}
	for _, st := range steps {
		switch st.kind {
		case "open":
			if _, dup := sessions[st.sid]; dup {
				return fmt.Errorf("session %q already open", st.sid)
			}
			switch st.mode {
			case "read":
				sessions[st.sid] = &cSession{read: db.Read()}
			case "write":
				sessions[st.sid] = &cSession{write: db.Write()}
			default:
				return fmt.Errorf("unknown session mode %q (want read|write)", st.mode)
			}
		case "commit", "rollback", "close":
			s, ok := sessions[st.sid]
			if !ok {
				return fmt.Errorf("%s of unknown session %q", st.kind, st.sid)
			}
			if err := endSession(st.kind, s); err != nil {
				return fmt.Errorf("%s %s: %w", st.kind, st.sid, err)
			}
			delete(sessions, st.sid)
		case "expect":
			got := expectValue(db, st.expectKind)
			if got != st.expectVal {
				return fmt.Errorf("expect %s %d, got %d", st.expectKind, st.expectVal, got)
			}
		case "on":
			s, ok := sessions[st.sid]
			if !ok {
				return fmt.Errorf("on unknown session %q", st.sid)
			}
			if err := runRecord(s.execute, st.sid, st.rec); err != nil {
				return err
			}
		}
	}
	if len(sessions) != 0 {
		return fmt.Errorf("file ended with sessions still open: %s", sortedKeys(sessions))
	}
	return nil
}

// expectValue reads the SharedDb state a `expect <kind>` directive asserts.
func expectValue(db *jed.SharedDB, kind string) uint64 {
	if kind == "oldest_live" {
		return db.OldestLiveTxid()
	}
	return db.Version() // "version"
}

// sortedKeys joins a session map's keys in sorted order — a deterministic message; map iteration
// order must never leak (CLAUDE.md §8).
func sortedKeys(sessions map[string]*cSession) string {
	open := make([]string, 0, len(sessions))
	for sid := range sessions {
		open = append(open, sid)
	}
	sort.Strings(open)
	return strings.Join(open, ", ")
}

// --- stepped-threaded mode (opt-in, `go test -race`; real concurrent-path coverage) ----------

// cCmd is a command sent from the driver to a session's worker goroutine.
type cCmd struct {
	end  bool    // true → end the session (kind tells how); false → run rec
	kind string  // end: commit | rollback | close
	rec  cRecord // run
}

// cWorker is a spawned per-session worker: its command channel, its reply channel, and a done
// channel closed when the goroutine exits (the join point).
type cWorker struct {
	cmd   chan cCmd
	reply chan error
	done  chan struct{}
}

// readWorker is a read session's goroutine: it pins a snapshot, runs records against it, and on
// `close` (or a closed command channel during teardown) deregisters by closing the handle.
func readWorker(db *jed.SharedDB, sid string, cmd chan cCmd, reply chan error, done chan struct{}) {
	defer close(done)
	s := &cSession{read: db.Read()}
	reply <- nil // ack the open: the snapshot is pinned + registered
	for c := range cmd {
		if c.end {
			var err error
			if c.kind == "close" {
				s.read.Close() // deregister BEFORE the reply, so the watermark is advanced on return
			} else {
				err = fmt.Errorf("%s of a read session (use close)", c.kind)
			}
			reply <- err
			return
		}
		reply <- runRecord(s.execute, sid, c.rec)
	}
	s.read.Close() // teardown: command channel closed without an explicit end
}

// writeWorker is a write session's goroutine: it acquires the writer gate, runs records against the
// working set, and on `commit`/`rollback` (or teardown) ends the transaction, releasing the gate.
func writeWorker(db *jed.SharedDB, sid string, cmd chan cCmd, reply chan error, done chan struct{}) {
	defer close(done)
	s := &cSession{write: db.Write()}
	reply <- nil // ack the open: the writer gate is held, the working set captured
	for c := range cmd {
		if c.end {
			var err error
			switch c.kind {
			case "commit":
				err = s.write.Commit()
			case "rollback":
				err = s.write.Rollback()
			default:
				err = fmt.Errorf("%s of a write session (use commit/rollback)", c.kind)
			}
			reply <- err
			return
		}
		reply <- runRecord(s.execute, sid, c.rec)
	}
	_ = s.write.Rollback() // teardown: command channel closed without an explicit end (release gate)
}

// runScheduleThreaded executes a schedule with one goroutine per session, the listed order enforced
// by a turn token: the driver sends a command and waits for the worker's reply (and, for an end
// step, joins the goroutine) before advancing — so exactly one session runs at a time, in order, yet
// every operation runs on a real goroutine against the shared handle (race-detector coverage). The
// canonical result is identical to the sequential mode (concurrency-testing.md §2/§4.3).
func runScheduleThreaded(steps []cStep) error {
	db := jed.NewSharedDB()
	workers := map[string]*cWorker{}
	var result error
	for _, st := range steps {
		if err := threadedStep(db, workers, st); err != nil {
			result = err
			break
		}
	}
	// Tear down any still-open workers: closing the command channel ends the worker loop (a read
	// handle deregisters; a write handle rolls back + releases the gate), then we join it.
	open := make([]string, 0, len(workers))
	for sid := range workers {
		open = append(open, sid)
	}
	sort.Strings(open) // deterministic order; map iteration order must never leak (CLAUDE.md §8)
	for _, sid := range open {
		w := workers[sid]
		close(w.cmd)
		<-w.done
	}
	if result == nil && len(open) != 0 {
		return fmt.Errorf("file ended with sessions still open: %s", strings.Join(open, ", "))
	}
	return result
}

// threadedStep runs one schedule step in threaded mode (spawn/dispatch to the session's goroutine,
// or read the SharedDb state for an `expect`).
func threadedStep(db *jed.SharedDB, workers map[string]*cWorker, st cStep) error {
	switch st.kind {
	case "open":
		if _, dup := workers[st.sid]; dup {
			return fmt.Errorf("session %q already open", st.sid)
		}
		w := &cWorker{cmd: make(chan cCmd), reply: make(chan error), done: make(chan struct{})}
		switch st.mode {
		case "read":
			go readWorker(db, st.sid, w.cmd, w.reply, w.done)
		case "write":
			go writeWorker(db, st.sid, w.cmd, w.reply, w.done)
		default:
			return fmt.Errorf("unknown session mode %q (want read|write)", st.mode)
		}
		if err := <-w.reply; err != nil { // the turn token: wait for the open ack before advancing
			<-w.done
			return fmt.Errorf("open %s: %w", st.sid, err)
		}
		workers[st.sid] = w
		return nil
	case "on":
		w, ok := workers[st.sid]
		if !ok {
			return fmt.Errorf("on unknown session %q", st.sid)
		}
		w.cmd <- cCmd{rec: st.rec}
		return <-w.reply
	case "commit", "rollback", "close":
		w, ok := workers[st.sid]
		if !ok {
			return fmt.Errorf("%s of unknown session %q", st.kind, st.sid)
		}
		w.cmd <- cCmd{end: true, kind: st.kind}
		err := <-w.reply
		<-w.done // join AFTER the reply, so the handle's deregister/gate-release has happened
		delete(workers, st.sid)
		if err != nil {
			return fmt.Errorf("%s %s: %w", st.kind, st.sid, err)
		}
		return nil
	case "expect":
		got := expectValue(db, st.expectKind)
		if got != st.expectVal {
			return fmt.Errorf("expect %s %d, got %d", st.expectKind, st.expectVal, got)
		}
		return nil
	}
	return fmt.Errorf("unknown concurrency directive %q", st.kind)
}
