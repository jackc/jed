// Command conformance is the Go core's conformance harness (CLAUDE.md §7).
//
// Walks spec/conformance/suites, and for each .test file whose `# requires:`
// capabilities are all in this core's SupportedCapabilities, runs the
// sqllogictest-style records against a fresh Database and compares output. Files
// needing a capability the core does not declare are SKIPPED (not failed), so an
// incomplete engine reads as "fewer tests run" (spec/design/conformance.md §3).
//
// Needs no TOML: the per-impl gate is the file's `# requires:` header vs this core's
// declared capability set; the manifest/profile data is validated by `rake verify`.
package main

import (
	"fmt"
	"math"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"

	"jed"
)

func main() {
	os.Exit(run())
}

func run() int {
	suites := suitesDir()
	var files []string
	_ = filepath.WalkDir(suites, func(path string, d os.DirEntry, err error) error {
		if err == nil && !d.IsDir() && strings.HasSuffix(path, ".test") {
			files = append(files, path)
		}
		return nil
	})
	sort.Strings(files)

	supported := map[string]bool{}
	for _, c := range jed.SupportedCapabilities {
		supported[c] = true
	}

	passed, failed, skipped := 0, 0, 0
	for _, file := range files {
		data, err := os.ReadFile(file)
		if err != nil {
			fmt.Printf("FAIL %s: %v\n", file, err)
			failed++
			continue
		}
		text := string(data)
		rel, _ := filepath.Rel(suites, file)

		var missing []string
		for _, c := range parseRequires(text) {
			if !supported[c] {
				missing = append(missing, c)
			}
		}
		if len(missing) > 0 {
			fmt.Printf("SKIP %s  (missing: %s)\n", rel, strings.Join(missing, ", "))
			skipped++
			continue
		}

		// A `# format: concurrency` file is an explicit multi-session schedule run against a
		// SharedDb (spec/design/concurrency-testing.md §4); everything else is the sequential
		// single-handle runner. Both share the result grammar; only the driver differs.
		runErr := runFile
		if isConcurrencyFormat(text) {
			runErr = runConcurrencyFile
		}
		if err := runErr(text); err != nil {
			fmt.Printf("FAIL %s: %v\n", rel, err)
			failed++
		} else {
			fmt.Printf("PASS %s\n", rel)
			passed++
		}
	}

	fmt.Printf("\n%d passed, %d failed, %d skipped\n", passed, failed, skipped)
	if failed != 0 {
		return 1
	}
	return 0
}

func suitesDir() string {
	wd, _ := os.Getwd()
	// Walk up to the repo root (the dir containing spec/) so the harness works from
	// anywhere under impl/go.
	dir := wd
	for {
		if _, err := os.Stat(filepath.Join(dir, "spec", "conformance", "suites")); err == nil {
			return filepath.Join(dir, "spec", "conformance", "suites")
		}
		parent := filepath.Dir(dir)
		if parent == dir {
			return filepath.Join(wd, "spec", "conformance", "suites")
		}
		dir = parent
	}
}

// repoRoot walks up from the working dir to the repo root (the dir containing spec/), so the harness
// can locate jed's pinned collation bundle from anywhere under impl/go.
func repoRoot() string {
	wd, _ := os.Getwd()
	dir := wd
	for {
		if _, err := os.Stat(filepath.Join(dir, "spec", "conformance", "suites")); err == nil {
			return dir
		}
		parent := filepath.Dir(dir)
		if parent == dir {
			return wd
		}
		dir = parent
	}
}

// parseLoadCollationDirective parses a `# load-collation: <name> = <fixture>[, <fixture>…]` line —
// the corpus's deterministic, host-free way to make a collation available before the records that
// use it (spec/design/collation.md §10). In the reference-only model the named collation is normally
// VENDORED (so the fixtures are an unused-but-documented fallback for a not-yet-vendored name,
// loadCollation). Returns the name and paths, or false if not this directive.
func parseLoadCollationDirective(line string) (string, []string, bool) {
	body, ok := strings.CutPrefix(strings.TrimSpace(strings.TrimPrefix(line, "#")), "load-collation:")
	if !ok {
		return "", nil, false
	}
	name, files, ok := strings.Cut(body, "=")
	if !ok {
		return "", nil, false
	}
	var paths []string
	for _, f := range strings.Split(files, ",") {
		if f = strings.TrimSpace(f); f != "" {
			paths = append(paths, f)
		}
	}
	if len(paths) == 0 {
		return "", nil, false
	}
	return strings.TrimSpace(name), paths, true
}

// loadCollation makes a collation named `name` available to the records that follow
// (spec/design/collation.md §2/§9/§10). The harness acts as the HOST: it loads jed's own pinned
// production JUCD bundle (spec/collation/fixtures/unicode.jucd) into the engine-global set via
// db.LoadUnicodeData (idempotent — the set is global), exactly as a production host would, then
// asserts the named collation now resolves. A name no loaded bundle provides fails the test, naming
// it (the directive's fixture paths are now a documentary provenance note, not loaded).
func loadCollation(name string) error {
	path := filepath.Join(repoRoot(), "spec", "collation", "fixtures", "unicode.jucd")
	data, err := os.ReadFile(path)
	if err != nil {
		return fmt.Errorf("load-collation: read %s: %w", path, err)
	}
	if err := jed.LoadUnicodeData(data); err != nil {
		return fmt.Errorf("load-collation: load unicode.jucd: %w", err)
	}
	if jed.LoadedCollation(name) == nil {
		return fmt.Errorf("load-collation: collation %q is not provided by the loaded bundle", name)
	}
	return nil
}

// parseFixtureDirective parses a file-level `# fixture: <spec-relative-path>` line — the corpus's way
// to run a file against a PRE-BUILT database image instead of a fresh database, so a test can exercise
// on-disk state SQL cannot construct (a version-skewed collation pin + a wrong-for-loaded index — the
// skew read-safety regression, spec/design/collation.md §12/§14). The path is relative to spec/.
// Gated by the harness.fixture_open capability. Returns the path, or false if not this directive.
func parseFixtureDirective(line string) (string, bool) {
	body, ok := strings.CutPrefix(strings.TrimSpace(strings.TrimPrefix(line, "#")), "fixture:")
	if !ok {
		return "", false
	}
	body = strings.TrimSpace(body)
	if body == "" {
		return "", false
	}
	return body, true
}

// openFixture opens the pre-built database image named by a `# fixture:` directive (path relative to
// spec/). The harness acts as the host: it first loads jed's pinned production bundle so any
// referenced collation resolves on open (a skewed pin still resolves — to a DIFFERENT version, which
// is the point), then reconstructs the database in memory via LoadDatabase. The handle is read-WRITE
// so a write against a skewed table exercises the real XX002 guard (collation.md §12), not a
// read-only-handle error.
func openFixture(rel string) (*jed.Database, error) {
	bundle := filepath.Join(repoRoot(), "spec", "collation", "fixtures", "unicode.jucd")
	if data, err := os.ReadFile(bundle); err == nil {
		_ = jed.LoadUnicodeData(data) // idempotent: the loaded set is engine-global
	}
	path := filepath.Join(repoRoot(), "spec", rel)
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("fixture: read %s: %w", path, err)
	}
	db, err := jed.LoadDatabase(data)
	if err != nil {
		return nil, fmt.Errorf("fixture: open %s: %w", rel, err)
	}
	return db, nil
}

func parseRequires(text string) []string {
	for _, line := range strings.Split(text, "\n") {
		t := strings.TrimSpace(line)
		if strings.HasPrefix(t, "#") {
			rest := strings.TrimSpace(strings.TrimPrefix(t, "#"))
			if strings.HasPrefix(rest, "requires:") {
				var out []string
				for _, c := range strings.Split(strings.TrimPrefix(rest, "requires:"), ",") {
					if c = strings.TrimSpace(c); c != "" {
						out = append(out, c)
					}
				}
				return out
			}
		}
	}
	return nil
}

// parseCostDirective parses a `# cost: N` directive line (CLAUDE.md §13). Returns the
// asserted cost and true, or (0, false) if the comment is not a cost directive.
func parseCostDirective(line string) (int64, bool) {
	rest, ok := strings.CutPrefix(strings.TrimSpace(strings.TrimPrefix(line, "#")), "cost:")
	if !ok {
		return 0, false
	}
	n, err := strconv.ParseInt(strings.TrimSpace(rest), 10, 64)
	if err != nil {
		return 0, false
	}
	return n, true
}

// parseMaxCostDirective parses a `# max_cost: N` directive line. Returns the caller-set cost
// ceiling to run the next record under and true, or (0, false) if not a max_cost directive.
// Mirrors `# cost:`, but instead of asserting the accrued cost it bounds it: the record is
// expected to abort with 54P01 once accrued cost reaches N (CLAUDE.md §13; cost.md §6).
func parseMaxCostDirective(line string) (int64, bool) {
	rest, ok := strings.CutPrefix(strings.TrimSpace(strings.TrimPrefix(line, "#")), "max_cost:")
	if !ok {
		return 0, false
	}
	n, err := strconv.ParseInt(strings.TrimSpace(rest), 10, 64)
	if err != nil {
		return 0, false
	}
	return n, true
}

// parseLifetimeMaxCostDirective parses a `# lifetime_max_cost: N` directive line. Returns the
// per-SESSION cumulative cost budget and true, or (0, false) if not one. Unlike `# max_cost:`
// (per-record, reset after each record), this is STICKY: it sets the session budget for the rest of
// the file (the cumulative cost builds across records on the one Database the file runs against), so
// an ordered statement sequence can drive the session to its budget and assert the 54P02 abort —
// what the per-record `# cost:` directive cannot express (spec/design/session.md §5.4).
func parseLifetimeMaxCostDirective(line string) (int64, bool) {
	rest, ok := strings.CutPrefix(strings.TrimSpace(strings.TrimPrefix(line, "#")), "lifetime_max_cost:")
	if !ok {
		return 0, false
	}
	n, err := strconv.ParseInt(strings.TrimSpace(rest), 10, 64)
	if err != nil {
		return 0, false
	}
	return n, true
}

// parseMaxSQLLengthDirective parses a `# max_sql_length: N` directive line. Returns the per-handle
// input-size limit (bytes) to run the next record under and true, or (0, false) if not one.
// Mirrors `# max_cost:`: it lets a record set a small cap and assert that an over-long statement
// aborts with 54000 (CLAUDE.md §13; cost.md §7, api.md §8). 0 is unlimited; absent ⇒ the engine
// default (1 MiB) for every other record.
func parseMaxSQLLengthDirective(line string) (int, bool) {
	rest, ok := strings.CutPrefix(strings.TrimSpace(strings.TrimPrefix(line, "#")), "max_sql_length:")
	if !ok {
		return 0, false
	}
	n, err := strconv.Atoi(strings.TrimSpace(rest))
	if err != nil {
		return 0, false
	}
	return n, true
}

// parsePrivSet parses a comma/whitespace-separated privilege list (SELECT, INSERT; EXECUTE; the
// keyword ALL = the four table privileges; NONE = the empty set) into a jed.PrivilegeSet. Used by the
// # default_privileges: / # grant: / # revoke: directives (spec/design/session.md §5.3).
func parsePrivSet(list string) (jed.PrivilegeSet, bool) {
	body := strings.TrimSpace(list)
	if strings.EqualFold(body, "none") {
		return jed.PrivSetEmpty, true
	}
	if strings.EqualFold(body, "all") {
		return jed.PrivSetAllTable, true
	}
	set := jed.PrivSetEmpty
	for _, tok := range strings.Split(body, ",") {
		name := strings.TrimSpace(tok)
		if name == "" {
			continue
		}
		p, ok := jed.PrivilegeFromName(name)
		if !ok {
			return 0, false
		}
		set = set.With(p)
	}
	return set, true
}

// parseDefaultPrivilegesDirective parses a `# default_privileges: SELECT, INSERT` directive line
// (spec/design/session.md §5.3): the table-privilege set granted to every table for the next record
// (NONE / ALL accepted).
func parseDefaultPrivilegesDirective(line string) (jed.PrivilegeSet, bool) {
	rest, ok := strings.CutPrefix(strings.TrimSpace(strings.TrimPrefix(line, "#")), "default_privileges:")
	if !ok {
		return 0, false
	}
	return parsePrivSet(rest)
}

// privDelta is a parsed `# grant:` / `# revoke:` directive: a privilege set and the lowercased
// object it applies to.
type privDelta struct {
	privs  jed.PrivilegeSet
	object string
}

// parsePrivDelta parses a `PRIVS ON object` body (after the grant:/revoke: prefix is stripped): the
// privilege set and the single-word object name after the ON keyword (spec/design/session.md §5.3).
func parsePrivDelta(body string) (privDelta, bool) {
	lower := strings.ToLower(body)
	idx := strings.Index(lower, " on ")
	if idx < 0 {
		return privDelta{}, false
	}
	privs, ok := parsePrivSet(body[:idx])
	if !ok {
		return privDelta{}, false
	}
	object := strings.TrimSpace(body[idx+4:])
	if object == "" || len(strings.Fields(object)) != 1 {
		return privDelta{}, false
	}
	return privDelta{privs: privs, object: object}, true
}

// parseGrantDirective parses a `# grant: PRIVS ON object` directive line (spec/design/session.md §5.3).
func parseGrantDirective(line string) (privDelta, bool) {
	rest, ok := strings.CutPrefix(strings.TrimSpace(strings.TrimPrefix(line, "#")), "grant:")
	if !ok {
		return privDelta{}, false
	}
	return parsePrivDelta(rest)
}

// parseRevokeDirective parses a `# revoke: PRIVS ON object` directive line (spec/design/session.md §5.3).
func parseRevokeDirective(line string) (privDelta, bool) {
	rest, ok := strings.CutPrefix(strings.TrimSpace(strings.TrimPrefix(line, "#")), "revoke:")
	if !ok {
		return privDelta{}, false
	}
	return parsePrivDelta(rest)
}

// parseAllowDDLDirective parses a `# allow_ddl: on|off` directive line (spec/design/session.md §5.3):
// whether DDL is permitted on the session for the next record.
func parseAllowDDLDirective(line string) (bool, bool) {
	rest, ok := strings.CutPrefix(strings.TrimSpace(strings.TrimPrefix(line, "#")), "allow_ddl:")
	if !ok {
		return false, false
	}
	switch strings.ToLower(strings.TrimSpace(rest)) {
	case "on", "true", "yes":
		return true, true
	case "off", "false", "no":
		return false, true
	default:
		return false, false
	}
}

// parseAllowTempDDLDirective parses a `# allow_temp_ddl: on|off` directive line (spec/design/
// temp-tables.md §5): whether session-local temporary-table DDL is permitted for the next record.
func parseAllowTempDDLDirective(line string) (bool, bool) {
	rest, ok := strings.CutPrefix(strings.TrimSpace(strings.TrimPrefix(line, "#")), "allow_temp_ddl:")
	if !ok {
		return false, false
	}
	switch strings.ToLower(strings.TrimSpace(rest)) {
	case "on", "true", "yes":
		return true, true
	case "off", "false", "no":
		return false, true
	default:
		return false, false
	}
}

// parseTempBuffersDirective parses a `# temp_buffers: N` directive line (spec/design/temp-tables.md
// §7): the per-session temp-table storage budget (bytes) to run the next record under (0 ⇒ unlimited).
// Mirrors `# max_cost:` — per-record, reset after — so a record can set a small budget and assert that
// an over-budget temp write traps 54P03.
func parseTempBuffersDirective(line string) (int, bool) {
	rest, ok := strings.CutPrefix(strings.TrimSpace(strings.TrimPrefix(line, "#")), "temp_buffers:")
	if !ok {
		return 0, false
	}
	n, err := strconv.Atoi(strings.TrimSpace(rest))
	if err != nil {
		return 0, false
	}
	return n, true
}

// parseAllowSharedTempDDLDirective parses a `# allow_shared_temp_ddl: on|off` directive line
// (spec/design/temp-tables.md §5): whether DATABASE-WIDE shared temporary-table DDL is permitted for
// the next record.
func parseAllowSharedTempDDLDirective(line string) (bool, bool) {
	rest, ok := strings.CutPrefix(strings.TrimSpace(strings.TrimPrefix(line, "#")), "allow_shared_temp_ddl:")
	if !ok {
		return false, false
	}
	switch strings.ToLower(strings.TrimSpace(rest)) {
	case "on", "true", "yes":
		return true, true
	case "off", "false", "no":
		return false, true
	default:
		return false, false
	}
}

// parseSharedTempMemDirective parses a `# shared_temp_mem: N` directive line (spec/design/temp-tables.md
// §7): the GLOBAL shared-temp storage budget (bytes) to run the next record under (0 ⇒ unlimited).
// Mirrors `# temp_buffers:` — per-record, reset after.
func parseSharedTempMemDirective(line string) (int, bool) {
	rest, ok := strings.CutPrefix(strings.TrimSpace(strings.TrimPrefix(line, "#")), "shared_temp_mem:")
	if !ok {
		return 0, false
	}
	n, err := strconv.Atoi(strings.TrimSpace(rest))
	if err != nil {
		return 0, false
	}
	return n, true
}

// varPair is a session-variable (name, value) parsed from a # set: directive.
type varPair struct{ name, value string }

// parseSetDirective parses a `# set: name=value, name2=value2` directive line (spec/design/session.md
// §6.1): the session variables to set for the next record (reset after, like # seed: / # grant:).
// Each pair splits on the first `=`; names are dotted custom variables.
func parseSetDirective(line string) ([]varPair, bool) {
	rest, ok := strings.CutPrefix(strings.TrimSpace(strings.TrimPrefix(line, "#")), "set:")
	if !ok {
		return nil, false
	}
	var pairs []varPair
	for _, part := range strings.Split(rest, ",") {
		part = strings.TrimSpace(part)
		if part == "" {
			continue
		}
		name, value, found := strings.Cut(part, "=")
		if !found {
			return nil, false
		}
		pairs = append(pairs, varPair{strings.TrimSpace(name), strings.TrimSpace(value)})
	}
	return pairs, true
}

// parseSeedDirective parses a `# seed: N` directive line (spec/design/entropy.md §6): the fixed
// PRNG seed (uint64) to run the next record under, making the uuid generators cross-core identical.
func parseSeedDirective(line string) (uint64, bool) {
	rest, ok := strings.CutPrefix(strings.TrimSpace(strings.TrimPrefix(line, "#")), "seed:")
	if !ok {
		return 0, false
	}
	n, err := strconv.ParseUint(strings.TrimSpace(rest), 10, 64)
	if err != nil {
		return 0, false
	}
	return n, true
}

// parseClockDirective parses a `# clock: N` directive line (entropy.md §6): the fixed statement
// clock (i64 micros since the Unix epoch) to run the next record under, fixing uuidv7's instant.
func parseClockDirective(line string) (int64, bool) {
	rest, ok := strings.CutPrefix(strings.TrimSpace(strings.TrimPrefix(line, "#")), "clock:")
	if !ok {
		return 0, false
	}
	n, err := strconv.ParseInt(strings.TrimSpace(rest), 10, 64)
	if err != nil {
		return 0, false
	}
	return n, true
}

// clockAdvance is a parsed `# clock_advance: start,step` directive (entropy.md §6): an advancing
// clock that returns start, start+step, … one increment per read.
type clockAdvance struct{ start, step int64 }

// parseClockAdvanceDirective parses a `# clock_advance: start,step` directive line: an advancing
// clock making clock_timestamp()'s per-call reads deterministic and distinguishable from now().
func parseClockAdvanceDirective(line string) (clockAdvance, bool) {
	rest, ok := strings.CutPrefix(strings.TrimSpace(strings.TrimPrefix(line, "#")), "clock_advance:")
	if !ok {
		return clockAdvance{}, false
	}
	parts := strings.SplitN(strings.TrimSpace(rest), ",", 2)
	if len(parts) != 2 {
		return clockAdvance{}, false
	}
	start, err1 := strconv.ParseInt(strings.TrimSpace(parts[0]), 10, 64)
	step, err2 := strconv.ParseInt(strings.TrimSpace(parts[1]), 10, 64)
	if err1 != nil || err2 != nil {
		return clockAdvance{}, false
	}
	return clockAdvance{start: start, step: step}, true
}

// assertCost checks the accrued execution cost matches a pending `# cost:` directive.
func assertCost(expected *int64, actual int64, sql string) error {
	if expected != nil && *expected != actual {
		return fmt.Errorf("cost mismatch: expected %d, got %d\n  SQL: %s", *expected, actual, sql)
	}
	return nil
}

// parseNamesDirective parses a `# names: a, b, ?column?` directive line. Returns the
// asserted output column names and true, or (nil, false) if not a names directive
// (spec/design/conformance.md §1).
func parseNamesDirective(line string) ([]string, bool) {
	rest, ok := strings.CutPrefix(strings.TrimSpace(strings.TrimPrefix(line, "#")), "names:")
	if !ok {
		return nil, false
	}
	var names []string
	for _, part := range strings.Split(rest, ",") {
		if s := strings.TrimSpace(part); s != "" {
			names = append(names, s)
		}
	}
	return names, true
}

// assertNames checks the query's output column names match a pending `# names:` directive.
func assertNames(expected []string, actual []string, sql string) error {
	if expected != nil && !equal(expected, actual) {
		return fmt.Errorf("column-name mismatch\n  SQL: %s\n  expected: %v\n  actual:   %v", sql, expected, actual)
	}
	return nil
}

// parseTypesDirective parses a `# types: i16, text, decimal` directive line. Returns the
// asserted output column types — each the canonical name of a result column's resolved type (the
// integer WIDTH, the unconstrained `decimal`, `unknown` for an untyped NULL), beyond the
// `I`/`T`/`D` rendering tag (spec/design/conformance.md §1/§7). (nil, false) if not a types directive.
func parseTypesDirective(line string) ([]string, bool) {
	rest, ok := strings.CutPrefix(strings.TrimSpace(strings.TrimPrefix(line, "#")), "types:")
	if !ok {
		return nil, false
	}
	var types []string
	for _, part := range strings.Split(rest, ",") {
		if s := strings.TrimSpace(part); s != "" {
			types = append(types, s)
		}
	}
	return types, true
}

// assertTypes checks the query's output column types match a pending `# types:` directive.
func assertTypes(expected []string, actual []string, sql string) error {
	if expected != nil && !equal(expected, actual) {
		return fmt.Errorf("column-type mismatch\n  SQL: %s\n  expected: %v\n  actual:   %v", sql, expected, actual)
	}
	return nil
}

// runFile runs all records in one .test file against a fresh database.
func runFile(text string) error {
	db := jed.NewDatabase()
	lines := strings.Split(text, "\n")
	i := 0
	// A `# cost: N` / `# names: ...` / `# types: ...` / `# max_cost: N` directive sets these; the
	// next record consumes them.
	var pendingCost *int64
	var pendingNames []string
	var pendingTypes []string
	var pendingMaxCost *int64
	var pendingMaxSQLLength *int
	var pendingSeed *uint64
	var pendingClock *int64
	var pendingClockAdvance *clockAdvance
	// The session privilege envelope for the next record (spec/design/session.md §5.3); reset after
	// each record so a directive never leaks forward. grant/revoke accumulate across lines.
	var pendingDefaultPrivileges *jed.PrivilegeSet
	var pendingGrants []privDelta
	var pendingRevokes []privDelta
	var pendingAllowDDL *bool
	var pendingAllowTempDDL *bool
	var pendingAllowSharedTempDDL *bool
	var pendingTempBuffers *int
	var pendingSharedTempMem *int
	var pendingVars []varPair
	for i < len(lines) {
		line := strings.TrimSpace(lines[i])
		if line == "" {
			i++
			continue
		}
		if strings.HasPrefix(line, "#") {
			// `# load-collation:` is an ACTION (assert available now), not a pending assertion: the
			// named collation must be vendored in this build before the records run
			// (spec/design/collation.md §2/§9/§10).
			if name, _, ok := parseLoadCollationDirective(line); ok {
				if err := loadCollation(name); err != nil {
					return err
				}
				i++
				continue
			}
			// `# fixture:` (file-level) opens a PRE-BUILT image in place of the fresh NewDatabase()
			// above — appears in the header before any record (spec/design/conformance.md).
			if rel, ok := parseFixtureDirective(line); ok {
				fixtureDB, err := openFixture(rel)
				if err != nil {
					return err
				}
				db = fixtureDB
				i++
				continue
			}
			// `# cost:` / `# names:` / `# types:` bind to the next record; every other comment
			// is ignored.
			if n, ok := parseCostDirective(line); ok {
				pendingCost = &n
			} else if n, ok := parseLifetimeMaxCostDirective(line); ok {
				// Sticky (spec/design/session.md §5.4): apply immediately and persistently — the
				// session cumulative builds across records, so a later record can assert the 54P02
				// abort. Not a pending per-record directive (it must NOT reset between records).
				db.SetLifetimeMaxCost(n)
			} else if n, ok := parseMaxCostDirective(line); ok {
				pendingMaxCost = &n
			} else if n, ok := parseMaxSQLLengthDirective(line); ok {
				pendingMaxSQLLength = &n
			} else if p, ok := parseDefaultPrivilegesDirective(line); ok {
				pendingDefaultPrivileges = &p
			} else if g, ok := parseGrantDirective(line); ok {
				pendingGrants = append(pendingGrants, g)
			} else if r, ok := parseRevokeDirective(line); ok {
				pendingRevokes = append(pendingRevokes, r)
			} else if a, ok := parseAllowDDLDirective(line); ok {
				pendingAllowDDL = &a
			} else if a, ok := parseAllowSharedTempDDLDirective(line); ok {
				pendingAllowSharedTempDDL = &a
			} else if a, ok := parseAllowTempDDLDirective(line); ok {
				pendingAllowTempDDL = &a
			} else if n, ok := parseSharedTempMemDirective(line); ok {
				pendingSharedTempMem = &n
			} else if n, ok := parseTempBuffersDirective(line); ok {
				pendingTempBuffers = &n
			} else if vars, ok := parseSetDirective(line); ok {
				pendingVars = append(pendingVars, vars...)
			} else if s, ok := parseSeedDirective(line); ok {
				pendingSeed = &s
			} else if c, ok := parseClockDirective(line); ok {
				pendingClock = &c
			} else if ca, ok := parseClockAdvanceDirective(line); ok {
				pendingClockAdvance = &ca
			} else if names, ok := parseNamesDirective(line); ok {
				pendingNames = names
			} else if types, ok := parseTypesDirective(line); ok {
				pendingTypes = types
			}
			i++
			continue
		}
		// This record consumes any pending assertions (so they never leak forward).
		expectedCost := pendingCost
		expectedNames := pendingNames
		expectedTypes := pendingTypes
		pendingCost = nil
		pendingNames = nil
		pendingTypes = nil
		// Apply the per-record cost ceiling (0 = unlimited); set each record so it auto-resets.
		var maxCost int64
		if pendingMaxCost != nil {
			maxCost = *pendingMaxCost
		}
		db.SetMaxCost(maxCost)
		pendingMaxCost = nil
		// Apply the per-record input-size cap; absent ⇒ the engine default (1 MiB), so a
		// `# max_sql_length:` directive never leaks past its record (cost.md §7, api.md §8).
		maxSQLLength := jed.DefaultMaxSQLLength
		if pendingMaxSQLLength != nil {
			maxSQLLength = *pendingMaxSQLLength
		}
		db.SetMaxSQLLength(maxSQLLength)
		pendingMaxSQLLength = nil
		// Apply the per-record entropy seed + statement clock for the uuid generators (entropy.md
		// §6); absent ⇒ cleared (OS entropy / wall clock), so a directive never leaks forward.
		if pendingSeed != nil {
			db.SetRandomSource(jed.SeededRandomSource(*pendingSeed))
		} else {
			db.ClearRandomSource()
		}
		pendingSeed = nil
		// `# clock_advance:` (an advancing clock) takes precedence over `# clock:` (a fixed one);
		// a record uses at most one. Absent ⇒ cleared, so a clock directive never leaks forward.
		if pendingClockAdvance != nil {
			db.SetClockSource(jed.AdvancingClock(pendingClockAdvance.start, pendingClockAdvance.step))
		} else if pendingClock != nil {
			db.SetClockSource(jed.FixedClock(*pendingClock))
		} else {
			db.ClearClockSource()
		}
		pendingClock = nil
		pendingClockAdvance = nil
		// Apply the per-record session privilege envelope (spec/design/session.md §5.3): reset to fully
		// permissive (every table privilege, DDL allowed), then layer the pending directives, so a
		// # default_privileges: / # grant: / # revoke: / # allow_ddl: decorates only its record and
		// never leaks forward.
		db.ResetPrivileges()
		if pendingDefaultPrivileges != nil {
			db.SetDefaultPrivileges(*pendingDefaultPrivileges)
		}
		for _, g := range pendingGrants {
			db.Grant(g.privs, g.object)
		}
		for _, r := range pendingRevokes {
			db.Revoke(r.privs, r.object)
		}
		if pendingAllowDDL != nil {
			db.SetAllowDDL(*pendingAllowDDL)
		}
		// `# allow_temp_ddl:` / `# allow_shared_temp_ddl:` override the temp-DDL gates (temp-tables.md
		// §5); ResetPrivileges above set both back to permissive, so each decorates only its record.
		if pendingAllowTempDDL != nil {
			db.SetAllowTempDDL(*pendingAllowTempDDL)
		}
		if pendingAllowSharedTempDDL != nil {
			db.SetAllowSharedTempDDL(*pendingAllowSharedTempDDL)
		}
		pendingDefaultPrivileges = nil
		pendingGrants = nil
		pendingRevokes = nil
		pendingAllowDDL = nil
		pendingAllowTempDDL = nil
		pendingAllowSharedTempDDL = nil
		// Apply the per-record temp-storage budgets (temp-tables.md §7); absent ⇒ unlimited (0), so a
		// `# temp_buffers:` / `# shared_temp_mem:` directive never leaks past its record. Mirrors `# max_cost:`.
		tempBuffers := 0
		if pendingTempBuffers != nil {
			tempBuffers = *pendingTempBuffers
		}
		db.SetTempBuffers(tempBuffers)
		pendingTempBuffers = nil
		sharedTempMem := 0
		if pendingSharedTempMem != nil {
			sharedTempMem = *pendingSharedTempMem
		}
		db.SetSharedTempMem(sharedTempMem)
		pendingSharedTempMem = nil
		// Apply the per-record session variables (spec/design/session.md §6.1): clear, then set each
		// pending # set: pair, so a directive decorates only its record and never leaks forward.
		db.ResetVars()
		for _, v := range pendingVars {
			if err := db.SetVar(v.name, v.value); err != nil {
				return fmt.Errorf("# set: directive uses a non-dotted variable name %q: %w", v.name, err)
			}
		}
		pendingVars = nil
		fields := strings.Fields(line)
		switch fields[0] {
		case "statement":
			// `# names:` / `# types:` assert result columns, which a statement lacks.
			if expectedNames != nil {
				return fmt.Errorf("# names: directive precedes a non-query statement")
			}
			if expectedTypes != nil {
				return fmt.Errorf("# types: directive precedes a non-query statement")
			}
			expect := ""
			if len(fields) > 1 {
				expect = fields[1]
			}
			i++
			sql := takeSQL(lines, &i)
			outcome, err := jed.Execute(db, sql)
			switch expect {
			case "ok":
				if err != nil {
					return fmt.Errorf("statement expected ok, got error %s\n  SQL: %s", err.Error(), sql)
				}
				if cerr := assertCost(expectedCost, outcome.Cost, sql); cerr != nil {
					return cerr
				}
			case "error":
				want := ""
				if len(fields) > 2 {
					want = fields[2]
				}
				if err == nil {
					return fmt.Errorf("statement expected error %s, but it succeeded\n  SQL: %s", want, sql)
				}
				if got := codeOf(err); got != want {
					return fmt.Errorf("statement expected error %s, got %s\n  SQL: %s", want, got, sql)
				}
			default:
				return fmt.Errorf("unknown statement kind %q", expect)
			}
		case "query":
			coltypes := ""
			sortmode := "nosort"
			if len(fields) > 1 {
				coltypes = fields[1]
			}
			if len(fields) > 2 {
				sortmode = fields[2]
			}
			i++
			sql := takeSQLUntilSeparator(lines, &i)
			var expected []string
			for i < len(lines) && strings.TrimSpace(lines[i]) != "" {
				expected = append(expected, strings.TrimSpace(lines[i]))
				i++
			}
			outcome, err := jed.Execute(db, sql)
			if err != nil {
				return fmt.Errorf("query failed with %s\n  SQL: %s", err.Error(), sql)
			}
			cols := len(coltypes)
			if cols == 0 {
				cols = 1
			}
			actual := renderOutcome(outcome, cols, sortmode)
			expected = applySort(expected, cols, sortmode)
			if !equalColtyped(actual, expected, coltypes, cols) {
				return fmt.Errorf("query result mismatch\n  SQL: %s\n  expected: %v\n  actual:   %v", sql, expected, actual)
			}
			if cerr := assertCost(expectedCost, outcome.Cost, sql); cerr != nil {
				return cerr
			}
			if nerr := assertNames(expectedNames, outcome.ColumnNames, sql); nerr != nil {
				return nerr
			}
			if terr := assertTypes(expectedTypes, outcome.ColumnTypes, sql); terr != nil {
				return terr
			}
		default:
			return fmt.Errorf("unknown record kind %q", fields[0])
		}
	}
	return nil
}

func codeOf(err error) string {
	if e, ok := err.(*jed.EngineError); ok {
		return e.Code()
	}
	return "?"
}

func takeSQL(lines []string, i *int) string {
	var sql []string
	for *i < len(lines) && strings.TrimSpace(lines[*i]) != "" {
		sql = append(sql, lines[*i])
		*i++
	}
	return strings.Join(sql, "\n")
}

func takeSQLUntilSeparator(lines []string, i *int) string {
	var sql []string
	for *i < len(lines) {
		if strings.TrimSpace(lines[*i]) == "----" {
			*i++
			break
		}
		sql = append(sql, lines[*i])
		*i++
	}
	return strings.Join(sql, "\n")
}

func renderOutcome(o jed.Outcome, cols int, sortmode string) []string {
	if o.Kind != jed.OutcomeQuery {
		return nil
	}
	var flat []string
	for _, row := range o.Rows {
		for _, v := range row {
			flat = append(flat, v.Render())
		}
	}
	return applySort(flat, cols, sortmode)
}

func applySort(flat []string, cols int, sortmode string) []string {
	switch sortmode {
	case "valuesort":
		out := append([]string(nil), flat...)
		sort.Strings(out)
		return out
	case "rowsort":
		if cols < 1 {
			cols = 1
		}
		var rows [][]string
		for i := 0; i+cols <= len(flat); i += cols {
			rows = append(rows, flat[i:i+cols])
		}
		sort.Slice(rows, func(a, b int) bool {
			return strings.Join(rows[a], "\x00") < strings.Join(rows[b], "\x00")
		})
		var out []string
		for _, r := range rows {
			out = append(out, r...)
		}
		return out
	default:
		return flat
	}
}

func equal(a, b []string) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i] != b[i] {
			return false
		}
	}
	return true
}

// equalColtyped compares a query's flat rendered cells against the expected cells, honouring the
// `R` (real/float) render tag: an R-tagged column is compared BY VALUE (parse both to f64, equal
// within a small relative tolerance; NaN==NaN, ±Inf exact, -0==+0 — spec/design/float.md §9,
// conformance.md §1), every other coltype by exact string. cols is the column count (so flat
// index i maps to column i%cols → coltypes[i%cols]). With rowsort the cells stay column-aligned
// (cells move as whole rows); with valuesort alignment is lost, so R falls back to string compare
// (float suites use nosort/rowsort). An empty coltypes (cols defaulted to 1) is exact string.
func equalColtyped(a, b []string, coltypes string, cols int) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i] == b[i] {
			continue
		}
		// Determine this cell's coltype char (if any); only `R` gets the tolerant compare.
		var ct byte
		if cols > 0 && len(coltypes) == cols {
			ct = coltypes[i%cols]
		}
		if ct == 'R' && floatCellsEqual(a[i], b[i]) {
			continue
		}
		return false
	}
	return true
}

// floatCellsEqual compares two rendered float cells by VALUE: parse both to f64; both NaN → equal;
// exactly one NaN → not; a == b (covers ±Inf exact and -0 == +0) → equal; both finite → equal iff
// |a−b| ≤ 1e-9·max(|a|,|b|,1); otherwise not. A non-parseable cell falls back to (already-failed)
// string compare → not equal (spec/design/float.md §9, the `R` tag's tolerant rule).
func floatCellsEqual(as, bs string) bool {
	a, aerr := parseFloatCell(as)
	b, berr := parseFloatCell(bs)
	if aerr != nil || berr != nil {
		return false
	}
	aNaN, bNaN := math.IsNaN(a), math.IsNaN(b)
	if aNaN || bNaN {
		return aNaN && bNaN
	}
	if a == b { // exact (covers ±Inf and -0 == +0)
		return true
	}
	if math.IsInf(a, 0) || math.IsInf(b, 0) {
		return false // a non-equal infinity is a real mismatch
	}
	tol := 1e-9 * math.Max(math.Max(math.Abs(a), math.Abs(b)), 1.0)
	return math.Abs(a-b) <= tol
}

// parseFloatCell parses a rendered float cell, accepting the spec's PG spellings (Infinity /
// -Infinity / NaN, case-insensitive) alongside ordinary numerics, so the `R` compare reads jed's
// own render output as well as PG's.
func parseFloatCell(s string) (float64, error) {
	switch strings.ToLower(strings.TrimSpace(s)) {
	case "infinity", "+infinity", "inf", "+inf":
		return math.Inf(1), nil
	case "-infinity", "-inf":
		return math.Inf(-1), nil
	case "nan":
		return math.NaN(), nil
	}
	return strconv.ParseFloat(strings.TrimSpace(s), 64)
}
