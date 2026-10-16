// Command conformance is the Go core's conformance harness (CLAUDE.md §7).
//
// Walks spec/conformance/suites, and for each .test file whose `# requires:`
// capabilities are all in this core's SupportedCapabilities, runs the
// sqllogictest-style records against a fresh Engine and compares output. Files
// needing a capability the core does not declare are SKIPPED (not failed), so an
// incomplete engine reads as "fewer tests run" (spec/design/conformance.md §3).
//
// Needs no TOML: the per-impl gate is the file's `# requires:` header vs this core's
// declared capability set; the manifest/profile data is validated by `mise run verify`.
package main

import (
	"cmp"
	"context"
	"errors"
	"fmt"
	"math"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strconv"
	"strings"

	jed "github.com/jackc/jed/impl/go"
)

func main() {
	os.Exit(run())
}

func run() int {
	// The corpus runs in one of two storage MODES (spec/design/conformance.md §3): the default
	// "memory" mode drives a fresh in-memory Database, and the "disk" mode (arg `disk`) drives a
	// FILE-BACKED database that is REOPENED before every record, so each committed read faults its
	// leaves from disk through the demand-paged buffer pool. The two modes catch DIVERGENCES between
	// resident and on-disk-faulted reads (the window-operand touched-set bug the in-memory pass could
	// not see). Every record's SQL-in → rows/error/cost-out must be IDENTICAL in both modes.
	disk := len(os.Args) > 1 && os.Args[1] == "disk"
	// The disk pass recounts every commit's live pages by reachability (memory.md §8.7), so the whole
	// corpus checks the incremental count the file-backed storage limit relies on.
	jed.SetVerifyLivePages(disk)
	mode := "memory"
	if disk {
		mode = "disk"
	}

	suites := suitesDir()
	var files []string
	_ = filepath.WalkDir(suites, func(path string, d os.DirEntry, err error) error {
		if err == nil && !d.IsDir() && strings.HasSuffix(path, ".test") {
			files = append(files, path)
		}
		return nil
	})
	sort.Strings(files)
	if filter := os.Getenv("JED_CONFORMANCE_FILTER"); filter != "" {
		var selected []string
		for _, path := range files {
			if strings.Contains(path, filter) {
				selected = append(selected, path)
			}
		}
		files = selected
		if len(files) == 0 {
			fmt.Fprintln(os.Stderr, "JED_CONFORMANCE_FILTER matched no tests")
			return 1
		}
	}

	// The forced temp-spill mode walks only the files that create temp tables: giving a memory-mode
	// database a spill directory also lets blocking operators spill where the corpus pins that an
	// in-memory database cannot (the 54P05 query-memory records).
	forced, err := forcedTempBuffers()
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		return 1
	}
	if forced >= 0 {
		var selected []string
		for _, path := range files {
			if data, err := os.ReadFile(path); err == nil && createsTempTable(string(data)) {
				selected = append(selected, path)
			}
		}
		files = selected
	}

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

		peakRel = rel
		isConc := isConcurrencyFormat(text)
		// Disk mode cannot run a file whose semantics can't survive a per-record REOPEN: the
		// concurrency driver (multi-session schedule on one Database), or a file carrying reopen-fragile
		// session state — session-local temp tables, a spanning transaction, a sticky lifetime budget, or
		// a pre-built fixture image (spec/design/conformance.md §3). These are gated with `# skip: disk`
		// and covered by the memory pass only; none exercises the on-disk faulted read path anyway.
		if disk && (isConc || parseSkip(text, "disk")) {
			fmt.Printf("SKIP %s  (disk-mode)\n", rel)
			skipped++
			continue
		}
		// A file whose contract only holds on a file-backed database (`# skip: memory` — e.g. the
		// bounded-spill lane, which an in-memory database never takes) runs in the disk pass only.
		if !disk && parseSkip(text, "memory") {
			fmt.Printf("SKIP %s  (memory-mode)\n", rel)
			skipped++
			continue
		}

		// A `# format: concurrency` file is an explicit multi-session schedule run against a
		// Database (spec/design/concurrency-testing.md §4); everything else is the sequential
		// single-handle runner. Both share the result grammar; only the driver differs.
		var runErr error
		if isConc {
			runErr = runConcurrencyFile(text)
		} else {
			runErr = runFile(text, disk)
		}
		if runErr != nil {
			fmt.Printf("FAIL %s: %v\n", rel, runErr)
			failed++
		} else {
			fmt.Printf("PASS %s\n", rel)
			passed++
		}
	}

	fmt.Printf("\n[%s] %d passed, %d failed, %d skipped\n", mode, passed, failed, skipped)
	if forced >= 0 {
		spills := jed.TempSpills()
		fmt.Printf("[%s] forced temp spill: %d temp domain(s) spilled\n", mode, spills)
		if spills == 0 && !disk && passed > 0 {
			fmt.Fprintln(os.Stderr, "JED_CONFORMANCE_TEMP_BUFFERS set but no temp domain spilled")
			return 1
		}
	}
	if failed != 0 {
		return 1
	}
	return 0
}

// parseSkip reports whether a file carries a `# skip: <mode>` directive (spec/design/conformance.md
// §3). `# skip: disk` opts out of the on-disk reopen pass because its session state (temp tables, a
// spanning transaction, a sticky lifetime_max_cost budget) or its pre-built `# fixture:` image cannot
// survive a per-record reopen; `# skip: memory` opts out of the in-memory pass because its contract
// holds only on a file-backed database.
func parseSkip(text, mode string) bool {
	for _, line := range strings.Split(text, "\n") {
		t := strings.TrimSpace(line)
		if !strings.HasPrefix(t, "#") {
			continue
		}
		rest := strings.TrimSpace(strings.TrimPrefix(t, "#"))
		// `# skip: <mode>[ — free-text reason]` — the first whitespace-delimited token after `skip:` is
		// the mode; any trailing text is a documentary reason.
		if v, ok := strings.CutPrefix(rest, "skip:"); ok {
			if fields := strings.Fields(v); len(fields) > 0 && fields[0] == mode {
				return true
			}
		}
	}
	return false
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

// loadCollation asserts a collation named `name` is available to the records that follow
// (spec/design/collation.md §2/§9/§10). The harness acts as the HOST: the file's environment was built
// with jed's own pinned production JUCD bundle (spec/collation/fixtures/unicode.jucd) loaded, exactly
// as a production host would (fileEnvironment); a name that bundle does not provide fails the test,
// naming it (the directive's fixture paths are a documentary provenance note, not loaded).
func loadCollation(name string, env *jed.Environment) error {
	for _, c := range env.LoadedCollations() {
		if c.Name == name {
			return nil
		}
	}
	return fmt.Errorf("load-collation: collation %q is not provided by the loaded bundle", name)
}

// parseLoadTimezoneDirective parses a `# load-timezone: [<zone>]` line — the corpus's host-free way to
// make the IANA time-zone data available before the records that use AT TIME ZONE (timezones.md §11).
// Returns the (possibly empty) zone name, or false if not this directive.
func parseLoadTimezoneDirective(line string) (string, bool) {
	body, ok := strings.CutPrefix(strings.TrimSpace(strings.TrimPrefix(line, "#")), "load-timezone:")
	if !ok {
		return "", false
	}
	return strings.TrimSpace(body), true
}

// loadTimezone asserts the IANA time zones are available to the records that follow
// (timezones.md §3.3/§11). The harness acts as the HOST: the file's environment was built with jed's
// pinned production JTZ bundle (spec/tz/fixtures/tzdata.jtz) loaded (fileEnvironment); if a zone name
// was given, it must be one that bundle provides, or the test fails.
func loadTimezone(name string, env *jed.Environment) error {
	if name == "" {
		return nil
	}
	for _, z := range env.LoadedTimeZones() {
		if z.Name == name {
			return nil
		}
	}
	return fmt.Errorf("load-timezone: zone %q is not provided by the loaded bundle", name)
}

// parseTimezoneDirective parses a `# timezone: <zone>` line (spec/design/session.md §6.2, timezones.md
// §9.4): the SESSION time zone for the next record (the zone a timestamptz decomposes in). Per-record
// (reset to UTC after, like `# set:`). Distinct from `# load-timezone:` (which loads the bundle).
func parseTimezoneDirective(line string) (string, bool) {
	body, ok := strings.CutPrefix(strings.TrimSpace(strings.TrimPrefix(line, "#")), "timezone:")
	if !ok {
		return "", false
	}
	return strings.TrimSpace(body), true
}

// parseFixtureDirective parses a file-level `# fixture: <spec-relative-path>` line — the corpus's way
// to run a file against a PRE-BUILT database image instead of a fresh database, so a test can exercise
// on-disk state SQL cannot construct (a version-skewed collation pin + a wrong-for-loaded index — the
// skew read-safety regression, spec/design/collation.md §12/§14). The path is relative to spec/.
// Gated by the harness.fixture_open capability. The form `<path> error <SQLSTATE>` (capability
// harness.fixture_open_error) instead asserts that opening the image fails with that SQLSTATE — a
// forged image whose corruption is detected at open — and the file runs no records. Returns the path
// and the expected open-error code ("" for a plain fixture), or false if not this directive.
func parseFixtureDirective(line string) (string, string, bool) {
	body, ok := strings.CutPrefix(strings.TrimSpace(strings.TrimPrefix(line, "#")), "fixture:")
	if !ok {
		return "", "", false
	}
	fields := strings.Fields(body)
	switch {
	case len(fields) == 1:
		return fields[0], "", true
	case len(fields) == 3 && fields[1] == "error":
		return fields[0], fields[2], true
	default:
		return "", "", false
	}
}

// parseUpgradeCollationsDirective reports whether the line is a `# upgrade-collations:` directive — a
// file-level ACTION that runs the COLLATION UPGRADE migration (db.UpgradeCollations) on the running
// database, clearing a version-skew so the records after it run against the migrated (read-write)
// state (spec/design/collation.md §12; capability harness.upgrade_collations).
func parseUpgradeCollationsDirective(line string) bool {
	return strings.HasPrefix(strings.TrimSpace(strings.TrimPrefix(line, "#")), "upgrade-collations:")
}

// openFixture opens the pre-built database image named by a `# fixture:` directive (path relative to
// spec/). The harness acts as the host: env carries jed's pinned production bundle so any referenced
// collation resolves on open (a skewed pin still resolves — to a DIFFERENT version, which is the
// point), and the database is opened via the public host API. The golden image is COPIED to a
// temp file and that copy is opened read-WRITE — so a write against a skewed table exercises the real
// XX002 guard (collation.md §12), not a read-only-handle error, while the committed golden stays
// pristine. The returned cleanup closes the handle and removes the temp file.
func openFixture(rel string, env *jed.Environment) (*jed.Database, func(), error) {
	path := filepath.Join(repoRoot(), "spec", rel)
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, nil, fmt.Errorf("fixture: read %s: %w", path, err)
	}
	tmp, err := os.CreateTemp("", "jed-fixture-*.jed")
	if err != nil {
		return nil, nil, fmt.Errorf("fixture: temp file: %w", err)
	}
	tmpPath := tmp.Name()
	_, werr := tmp.Write(data)
	cerr := tmp.Close()
	if werr != nil || cerr != nil {
		os.Remove(tmpPath)
		return nil, nil, fmt.Errorf("fixture: write temp: %w", cmp.Or(werr, cerr))
	}
	db, err := jed.OpenDatabaseWithOptions(tmpPath, jed.OpenOptions{Environment: env})
	if err != nil {
		removeTempDB(tmpPath)
		return nil, nil, fmt.Errorf("fixture: open %s: %w", rel, err)
	}
	cleanup := func() {
		_ = db.Close()
		removeTempDB(tmpPath)
	}
	return db, cleanup, nil
}

// removeTempDB removes a harness temp database image and the `<path>.lock/` coordination bundle that
// opening it created beside it (spec/design/locking.md). Removing only the image leaks one directory
// per corpus file into the temp dir. Call it after the handle is closed.
func removeTempDB(path string) {
	os.Remove(path)
	os.RemoveAll(path + ".lock")
}

// parseAttachDirective parses a file-level `# attach: <name>` line (spec/design/attached-databases.md
// §6) — the corpus's way to attach a fresh, empty READ-WRITE in-memory database named <name> to the
// running handle before the records run, so SQL can `CREATE TABLE <name>.t`, populate it, and join
// across attachments. Returns the name, or ("", false) if not this directive. Gated by the
// harness.attach capability. In-memory attachments cannot survive the disk-mode reopen, so an
// # attach: file is # skip: disk.
func parseAttachDirective(line string) (string, bool) {
	body, ok := strings.CutPrefix(strings.TrimSpace(strings.TrimPrefix(line, "#")), "attach:")
	if !ok {
		return "", false
	}
	body = strings.TrimSpace(body)
	if body == "" {
		return "", false
	}
	return body, true
}

// parseMaxStorageBytesDirective parses a `# max_storage_bytes: N [database]` line
// (spec/design/memory.md §8.1): an ACTION that sets the committed-storage limit of `main` (or the
// named attachment) on the running handle from this point of the file on. The disk pass re-applies
// main's limit after each per-record reopen; an in-memory database and a file-backed one measure
// different things (§8.2 vs §8.7), so a file that pins trip points runs in one mode only. Returns the
// limit and database name, or ok false if not this directive.
func parseMaxStorageBytesDirective(line string) (int64, string, bool) {
	body, ok := strings.CutPrefix(strings.TrimSpace(strings.TrimPrefix(line, "#")), "max_storage_bytes:")
	if !ok {
		return 0, "", false
	}
	parts := strings.Fields(body)
	if len(parts) == 0 {
		return 0, "", false
	}
	bytes, err := strconv.ParseInt(parts[0], 10, 64)
	if err != nil {
		return 0, "", false
	}
	name := "main"
	if len(parts) > 1 {
		name = parts[1]
	}
	return bytes, name, true
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
// the file (the cumulative cost builds across records on the one Engine the file runs against), so
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

// privDelta is a parsed `# grant:` / `# revoke:` directive: a privilege set and the exact object
// name it applies to.
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

// parseWorkMemDirective parses a `# work_mem: N` directive line (spec/design/spill.md §3): the
// work-memory budget in bytes (0 ⇒ unlimited, never spill) the next record runs under. It overrides
// JED_CONFORMANCE_WORK_MEM, so a record whose result depends on when operators spill (a query-memory
// threshold in the bounded-spill lane) pins it. Per-record, like `# max_cost:`.
func parseWorkMemDirective(line string) (int, bool) {
	rest, ok := strings.CutPrefix(strings.TrimSpace(strings.TrimPrefix(line, "#")), "work_mem:")
	if !ok {
		return 0, false
	}
	n, err := strconv.Atoi(strings.TrimSpace(rest))
	if err != nil || n < 0 {
		return 0, false
	}
	return n, true
}

// forcedTempBuffers is the forced temp-spill mode's resident budget (JED_CONFORMANCE_TEMP_BUFFERS,
// bytes): memory-mode databases get a spill directory and every session this temp_buffers, so each temp
// domain past it spills to a scratch file (spec/design/temp-tables.md §6). -1 when unset.
func forcedTempBuffers() (int, error) {
	bytes := os.Getenv("JED_CONFORMANCE_TEMP_BUFFERS")
	if bytes == "" {
		return -1, nil
	}
	n, err := strconv.Atoi(bytes)
	if err != nil || n < 0 {
		return 0, fmt.Errorf("JED_CONFORMANCE_TEMP_BUFFERS must be nonnegative bytes")
	}
	return n, nil
}

// createsTempTable reports whether a corpus file creates a session-local temp table (the forced
// temp-spill mode's file set).
func createsTempTable(text string) bool {
	upper := strings.ToUpper(text)
	return strings.Contains(upper, "CREATE TEMP ") || strings.Contains(upper, "CREATE TEMPORARY ")
}

// parseMaxTempStorageDirective parses a `# max_temp_storage_bytes: N` directive line
// (spec/design/temp-tables.md §7): the per-session temp-table storage ceiling (bytes) to run the next
// record under (0 ⇒ unlimited). Mirrors `# max_cost:` — per-record, reset after — so a record can set a
// small ceiling and assert that an over-ceiling temp write traps 54P03.
func parseMaxTempStorageDirective(line string) (int, bool) {
	rest, ok := strings.CutPrefix(strings.TrimSpace(strings.TrimPrefix(line, "#")), "max_temp_storage_bytes:")
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

// parseParamsDirective parses a `# params: 1, 'a', NULL` directive line (spec/design/conformance.md
// §1.2): the host values the next record binds as $1..$N. (nil, false, nil) if not a params directive;
// a malformed value list is an error.
func parseParamsDirective(line string) ([]jed.Value, bool, error) {
	rest, ok := strings.CutPrefix(strings.TrimSpace(strings.TrimPrefix(line, "#")), "params:")
	if !ok {
		return nil, false, nil
	}
	values := []jed.Value{}
	rest = strings.TrimSpace(rest)
	for rest != "" {
		var v jed.Value
		var err error
		v, rest, err = takeParamValue(rest)
		if err != nil {
			return nil, true, fmt.Errorf("# params: %w", err)
		}
		values = append(values, v)
		rest = strings.TrimSpace(rest)
		if rest == "" {
			break
		}
		after, ok := strings.CutPrefix(rest, ",")
		if !ok {
			return nil, true, fmt.Errorf("# params: expected `,` before %q", rest)
		}
		rest = strings.TrimSpace(after)
	}
	return values, true, nil
}

// takeParamValue reads one `# params:` value from the front of s: a single-quoted text (a doubled
// ” is one quote), NULL, true/false, an integer, or a decimal with its written scale.
func takeParamValue(s string) (jed.Value, string, error) {
	if m := paramListRe.FindString(s); m != "" {
		closer, isArray := ")", strings.HasPrefix(m, "ARRAY")
		if isArray {
			closer = "]"
		}
		rest := strings.TrimSpace(s[len(m):])
		var items []jed.Value
		for !strings.HasPrefix(rest, closer) {
			if rest == "" {
				return jed.Value{}, "", fmt.Errorf("unterminated list value %q", s)
			}
			item, after, err := takeParamValue(rest)
			if err != nil {
				return jed.Value{}, "", err
			}
			items = append(items, item)
			rest = strings.TrimSpace(after)
			if strings.HasPrefix(rest, closer) {
				break
			}
			after, ok := strings.CutPrefix(rest, ",")
			if !ok {
				return jed.Value{}, "", fmt.Errorf("expected `,` or `%s` before %q", closer, rest)
			}
			rest = strings.TrimSpace(after)
		}
		if isArray {
			return jed.ArrayValue(items), rest[1:], nil
		}
		if len(items) == 0 {
			return jed.Value{}, "", fmt.Errorf("ROW() needs at least one field")
		}
		return jed.CompositeValue(items), rest[1:], nil
	}
	if strings.HasPrefix(s, "'") {
		var text strings.Builder
		for i := 1; i < len(s); i++ {
			if s[i] != '\'' {
				text.WriteByte(s[i])
				continue
			}
			if i+1 < len(s) && s[i+1] == '\'' {
				text.WriteByte('\'')
				i++
				continue
			}
			return jed.TextValue(text.String()), s[i+1:], nil
		}
		return jed.Value{}, "", fmt.Errorf("unterminated text value %q", s)
	}
	token, rest := s, ""
	// A scalar token runs to the next separator or the close of an enclosing ARRAY[…] / ROW(…).
	if end := strings.IndexAny(s, ",])"); end >= 0 {
		token, rest = s[:end], s[end:]
	}
	token = strings.TrimSpace(token)
	switch {
	case token == "NULL":
		return jed.NullValue(), rest, nil
	case token == "true" || token == "false":
		return jed.BoolValue(token == "true"), rest, nil
	case paramIntRe.MatchString(token):
		n, err := strconv.ParseInt(token, 10, 64)
		if err != nil {
			return jed.Value{}, "", fmt.Errorf("integer %s is outside i64", token)
		}
		return jed.IntValue(n), rest, nil
	case paramDecimalRe.MatchString(token):
		d, err := jed.ParseDecimal(token)
		if err != nil {
			return jed.Value{}, "", err
		}
		return jed.DecimalValue(d), rest, nil
	}
	return jed.Value{}, "", fmt.Errorf("unrecognized value %q", token)
}

var (
	paramListRe    = regexp.MustCompile(`^(ARRAY\s*\[|ROW\s*\()`)
	paramIntRe     = regexp.MustCompile(`^-?[0-9]+$`)
	paramDecimalRe = regexp.MustCompile(`^-?[0-9]+\.[0-9]+$`)
)

// assertTypes checks the query's output column types match a pending `# types:` directive.
func assertTypes(expected []string, actual []string, sql string) error {
	if expected != nil && !equal(expected, actual) {
		return fmt.Errorf("column-type mismatch\n  SQL: %s\n  expected: %v\n  actual:   %v", sql, expected, actual)
	}
	return nil
}

// runFile runs all records in one .test file against a fresh database, driving the public single-handle
// *Database (its default autocommit session — the back-compat bridge, spec/design/session.md §2.1).
// peakSink is the open output of JED_CONFORMANCE_QUERY_MEMORY_PEAKS (the per-record query-memory
// peak mode, mise run conformance:query_memory), or nil; peakRel is the file being walked.
var (
	peakSink *os.File
	peakRel  string
)

// recordPeak writes the peak query-memory balance of record ordinal of the current file when the
// peak mode is on. Every core writes the same file<TAB>ordinal<TAB>peak lines, so a diff of the three
// outputs checks every record's minimal passing budget across cores (memory.md §8). Each line also
// carries the main database's committed storage after the record (memory.md §8.4): the cross-core
// check that in-memory page allocation and compaction agree record by record.
func recordPeak(ordinal int, db *jed.Database) {
	if peakSink == nil {
		path := os.Getenv("JED_CONFORMANCE_QUERY_MEMORY_PEAKS")
		if path == "" {
			return
		}
		f, err := os.OpenFile(path, os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0o644)
		if err != nil {
			panic(fmt.Sprintf("open JED_CONFORMANCE_QUERY_MEMORY_PEAKS: %v", err))
		}
		peakSink = f
	}
	storage, err := db.StorageBytes("main")
	if err != nil {
		panic(fmt.Sprintf("main is always present: %v", err))
	}
	fmt.Fprintf(peakSink, "%s\t%d\t%d\t%d\n", peakRel, ordinal, jed.QueryMemoryPeak(), storage)
}

func runFile(text string, disk bool) error {
	forcedTemp, err := forcedTempBuffers()
	if err != nil {
		return err
	}
	lines := strings.Split(text, "\n")
	// The file's environment: its data bundles + the switchable sources the per-record # seed: /
	// # clock: / # clock_advance: directives drive. Every handle of the file (the disk reopens, a
	// fixture) opens under it.
	random, clock := newHarnessRandom(), newHarnessClock()
	env, err := fileEnvironment(lines, random, clock)
	if err != nil {
		return err
	}
	// In DISK mode the file is backed by a temp .jed image reopened before every record (below), so each
	// committed read faults from disk; in MEMORY mode it is a fresh in-memory Database. tmpPath is "" in
	// memory mode. (spec/design/conformance.md §3.)
	var db *jed.Database
	tmpPath := ""
	if disk {
		f, err := os.CreateTemp("", "jed-conformance-*.jed")
		if err != nil {
			return fmt.Errorf("disk mode: temp file: %w", err)
		}
		tmpPath = f.Name()
		f.Close()
		os.Remove(tmpPath) // CreateDatabase writes the initial image at this path
		// fsync=off (api.md §2.1): the disk pass reopens a throwaway temp image before every record to
		// exercise the on-disk faulted read path — durability across an OS crash is irrelevant, so skip
		// the per-commit fdatasync (the ~20x cost) while keeping the identical on-disk bytes + read path.
		db, err = jed.CreateDatabase(jed.CreateOptions{Path: tmpPath, SkipFsync: true, Environment: env})
		if err != nil {
			removeTempDB(tmpPath)
			return fmt.Errorf("disk mode: create %s: %w", tmpPath, err)
		}
	} else {
		var err error
		db, err = jed.CreateDatabase(jed.CreateOptions{Environment: env}) // in-memory: no path
		if err != nil {
			return fmt.Errorf("in-memory create: %w", err)
		}
		// Forced temp spill (JED_CONFORMANCE_TEMP_BUFFERS, temp-tables.md §6): an in-memory database has
		// no spill target, so give it one — temp domains then spill to scratch files.
		if forcedTemp >= 0 {
			jed.SetSpillDirForTooling(db, os.TempDir())
		}
	}
	// Database no longer owns a persistent default session (it mints a fresh session per convenience
	// call), so the harness drives one explicit session per file — preserving the cross-record state the
	// sequential corpus relies on (sticky lifetime_max_cost, session-local temp tables, the per-record
	// # set:/privilege resets). Re-minted whenever a `# fixture:` swaps the underlying database, or (disk
	// mode) whenever the file is reopened before a record.
	sess := db.Session(jed.SessionOptions{})
	// The handle's default work_mem, restored on every record without `# work_mem:` or the override.
	defaultWorkMem := sess.WorkMem()
	// onTemp tracks whether db/sess still point at the reopenable temp-file handle (a `# fixture:` swap
	// flips it off — but fixtures are `# skip: disk`, so that never coexists with disk mode).
	onTemp := disk
	// mainStorageLimit is main's `# max_storage_bytes:` limit, re-applied to each disk-mode reopen (a
	// handle setting).
	mainStorageLimit := int64(0)
	// reopenDisk closes the current handle and reopens the temp image, so the next record reads a fully
	// demand-paged store (leaves fault on access — the on-disk read path the memory pass never reaches).
	// A fresh session is minted; only per-record directives (re-applied below) and committed data (on the
	// file) carry across — which is exactly why reopen-fragile files are `# skip: disk`.
	reopenDisk := func() error {
		sess.Close()
		if err := db.Close(); err != nil {
			return fmt.Errorf("disk reopen: close: %w", err)
		}
		reDB, err := jed.OpenDatabaseWithOptions(tmpPath, jed.OpenOptions{SkipFsync: true, Environment: env}) // fsync=off (throwaway image)
		if err != nil {
			return fmt.Errorf("disk reopen: open %s: %w", tmpPath, err)
		}
		db = reDB
		if err := db.SetMaxStorageBytes("main", mainStorageLimit); err != nil {
			return fmt.Errorf("disk reopen: max_storage_bytes: %w", err)
		}
		sess = db.Session(jed.SessionOptions{})
		return nil
	}
	// cleanup releases the current handle; a `# fixture:` directive swaps in a file-backed handle whose
	// cleanup also removes the temp image. The deferred call reads the latest cleanup.
	cleanup := func() {}
	if disk {
		cleanup = func() {
			sess.Close()
			db.Close()
			removeTempDB(tmpPath)
		}
	}
	defer func() { cleanup() }()
	i := 0
	// A `# cost: N` / `# names: ...` / `# types: ...` / `# max_cost: N` directive sets these; the
	// next record consumes them.
	var pendingCost *int64
	var pendingNames []string
	var pendingTypes []string
	var pendingParams []jed.Value
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
	var pendingMaxTempStorage *int
	var pendingWorkMem *int
	var pendingScalarBytes *int64
	var pendingQueryMemory *int64
	recordOrdinal := 0
	// The whole-corpus accounting mode (`mise run conformance:query_memory`): a budget applied to every
	// record without its own directive, so every query shape exercises the accounting.
	var queryMemoryDefault int64
	if v := os.Getenv("JED_CONFORMANCE_QUERY_MEMORY"); v != "" {
		n, err := strconv.ParseInt(v, 10, 64)
		if err != nil {
			return fmt.Errorf("JED_CONFORMANCE_QUERY_MEMORY must be integer bytes")
		}
		queryMemoryDefault = n
	}
	var pendingVars []varPair
	var pendingTimezone *string
	for i < len(lines) {
		line := strings.TrimSpace(lines[i])
		if line == "" {
			i++
			continue
		}
		if strings.HasPrefix(line, "#") {
			// `# load-collation:` is an ACTION (assert available now), not a pending assertion: the
			// named collation must be provided by the file's environment before the records run
			// (spec/design/collation.md §2/§9/§10).
			if name, _, ok := parseLoadCollationDirective(line); ok {
				if err := loadCollation(name, env); err != nil {
					return err
				}
				i++
				continue
			}
			// `# load-timezone: [<zone>]` is an ACTION: the file's environment carries jed's pinned JTZ
			// bundle (and the directive optionally asserts a zone resolves) before the records that use
			// AT TIME ZONE (timezones.md §11).
			if name, ok := parseLoadTimezoneDirective(line); ok {
				if err := loadTimezone(name, env); err != nil {
					return err
				}
				i++
				continue
			}
			// `# attach: <name>` (file-level) attaches a fresh empty read-write in-memory database to the
			// running handle (attached-databases.md §6): the records then CREATE / populate / join it by
			// the `<name>.table` qualifier. An ACTION applied to the Database, so every session over it
			// sees the attachment (refreshCommitted re-pins roots.attached per statement). In-memory
			// attachments cannot survive the disk reopen, so # attach: files are # skip: disk.
			if name, ok := parseAttachDirective(line); ok {
				if err := db.Attach(name, jed.AttachMemory(), false); err != nil {
					return fmt.Errorf("attach %q: %w", name, err)
				}
				i++
				continue
			}
			// `# max_storage_bytes: N [database]` (file-level) sets the committed-storage limit of main or
			// an attachment on the running handle (memory.md §8.1), sticky from this point of the file on.
			if bytes, name, ok := parseMaxStorageBytesDirective(line); ok {
				if err := db.SetMaxStorageBytes(name, bytes); err != nil {
					return fmt.Errorf("max_storage_bytes %q: %w", name, err)
				}
				if name == "main" {
					mainStorageLimit = bytes
				}
				i++
				continue
			}
			// `# fixture:` (file-level) opens a PRE-BUILT image in place of the fresh NewEngine()
			// above — appears in the header before any record (spec/design/conformance.md).
			if rel, wantCode, ok := parseFixtureDirective(line); ok {
				fixtureDB, fixtureCleanup, err := openFixture(rel, env)
				if wantCode != "" {
					if err == nil {
						fixtureCleanup()
						return fmt.Errorf("fixture %s opened, expected error %s", rel, wantCode)
					}
					var ee *jed.EngineError
					if !errors.As(err, &ee) || ee.Code() != wantCode {
						return fmt.Errorf("fixture %s: expected open error %s, got %v", rel, wantCode, err)
					}
					return nil // the open failed as required; the file has no records
				}
				if err != nil {
					return err
				}
				cleanup() // release the prior (in-memory no-op) handle before swapping in the fixture
				db = fixtureDB
				cleanup = fixtureCleanup
				sess = db.Session(jed.SessionOptions{})
				onTemp = false // the handle is now the fixture image, not the reopenable temp file
				i++
				continue
			}
			// `# upgrade-collations:` (file-level) runs the COLLATION UPGRADE migration on the running
			// DB — the privileged host op (db.UpgradeCollations) that clears a version-skew
			// (collation.md §12); the records after it assert the table is read-write again.
			if parseUpgradeCollationsDirective(line) {
				if _, err := sess.UpgradeCollations(); err != nil {
					return fmt.Errorf("upgrade-collations: %s", err.Error())
				}
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
				sess.SetLifetimeMaxCost(n)
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
			} else if a, ok := parseAllowTempDDLDirective(line); ok {
				pendingAllowTempDDL = &a
			} else if n, ok := parseMaxCostDirective(strings.Replace(line, "max_scalar_bytes:", "max_cost:", 1)); ok {
				pendingScalarBytes = &n
			} else if n, ok := parseMaxCostDirective(strings.Replace(line, "max_query_memory_bytes:", "max_cost:", 1)); ok {
				pendingQueryMemory = &n
			} else if n, ok := parseMaxTempStorageDirective(line); ok {
				pendingMaxTempStorage = &n
			} else if n, ok := parseWorkMemDirective(line); ok {
				pendingWorkMem = &n
			} else if vars, ok := parseSetDirective(line); ok {
				pendingVars = append(pendingVars, vars...)
			} else if z, ok := parseTimezoneDirective(line); ok {
				pendingTimezone = &z
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
			} else if params, ok, err := parseParamsDirective(line); ok {
				if err != nil {
					return err
				}
				pendingParams = params
			}
			i++
			continue
		}
		// DISK mode: reopen the temp image before this record so its reads fault leaves from disk (the
		// committed state carries on the file; the fresh session re-receives the per-record directives
		// applied just below). Writes reopen too — an UPDATE/DELETE over a faulted leaf exercises the
		// resolve-and-rewrite path. No-op in memory mode or after a fixture swap (which is `# skip: disk`).
		if disk && onTemp {
			if err := reopenDisk(); err != nil {
				return err
			}
		}
		// This record consumes any pending assertions (so they never leak forward).
		// `# work_mem:` pins this record's budget; otherwise the forced-spill override
		// (JED_CONFORMANCE_WORK_MEM), otherwise the default — set every record so none leaks forward.
		workMem := defaultWorkMem
		if pendingWorkMem != nil {
			workMem = *pendingWorkMem
			pendingWorkMem = nil
		} else if bytes := os.Getenv("JED_CONFORMANCE_WORK_MEM"); bytes != "" {
			budget, err := strconv.Atoi(bytes)
			if err != nil || budget < 0 {
				return fmt.Errorf("JED_CONFORMANCE_WORK_MEM must be nonnegative bytes")
			}
			workMem = budget
		}
		sess.SetWorkMem(workMem)
		// The temp-table resident budget: the forced-spill override (JED_CONFORMANCE_TEMP_BUFFERS),
		// otherwise the default. Residency only — results, cost, and 54P03 never depend on it.
		if forcedTemp >= 0 {
			sess.SetTempBuffers(forcedTemp)
		} else {
			sess.SetTempBuffers(jed.DefaultTempBuffers)
		}
		expectedCost := pendingCost
		expectedNames := pendingNames
		expectedTypes := pendingTypes
		params := pendingParams
		pendingCost = nil
		pendingNames = nil
		pendingTypes = nil
		pendingParams = nil
		// Apply the per-record cost ceiling (0 = unlimited); set each record so it auto-resets.
		var maxCost int64
		if pendingMaxCost != nil {
			maxCost = *pendingMaxCost
		}
		sess.SetMaxCost(maxCost)
		pendingMaxCost = nil
		// Apply the per-record input-size cap; absent ⇒ the engine default (1 MiB), so a
		// `# max_sql_length:` directive never leaks past its record (cost.md §7, api.md §8).
		maxSQLLength := jed.DefaultMaxSQLLength
		if pendingMaxSQLLength != nil {
			maxSQLLength = *pendingMaxSQLLength
		}
		sess.SetMaxSQLLength(maxSQLLength)
		pendingMaxSQLLength = nil
		// Apply the per-record entropy seed + statement clock for the uuid generators (entropy.md
		// §6) by switching the environment's harness sources; absent ⇒ OS entropy / the wall clock, so
		// a directive never leaks forward. `# clock_advance:` (an advancing clock) takes precedence
		// over `# clock:` (a fixed one); a record uses at most one.
		random.seed(pendingSeed)
		pendingSeed = nil
		clock.set(pendingClock, pendingClockAdvance)
		pendingClock = nil
		pendingClockAdvance = nil
		// Apply the per-record session privilege envelope (spec/design/session.md §5.3): reset to fully
		// permissive (every table privilege, DDL allowed), then layer the pending directives, so a
		// # default_privileges: / # grant: / # revoke: / # allow_ddl: decorates only its record and
		// never leaks forward.
		sess.ResetPrivileges()
		if pendingDefaultPrivileges != nil {
			sess.SetDefaultPrivileges(*pendingDefaultPrivileges)
		}
		for _, g := range pendingGrants {
			sess.Grant(g.privs, g.object)
		}
		for _, r := range pendingRevokes {
			sess.Revoke(r.privs, r.object)
		}
		if pendingAllowDDL != nil {
			sess.SetAllowDDL(*pendingAllowDDL)
		}
		// `# allow_temp_ddl:` overrides the temp-DDL gate (temp-tables.md §5); ResetPrivileges above set
		// it back to permissive, so it decorates only its record.
		if pendingAllowTempDDL != nil {
			sess.SetAllowTempDDL(*pendingAllowTempDDL)
		}
		pendingDefaultPrivileges = nil
		pendingGrants = nil
		pendingRevokes = nil
		pendingAllowDDL = nil
		pendingAllowTempDDL = nil
		// Apply the per-record temp-storage ceiling (temp-tables.md §7); absent ⇒ unlimited (0), so a
		// `# max_temp_storage_bytes:` directive never leaks past its record. Mirrors `# max_cost:`.
		maxTempStorage := 0
		if pendingMaxTempStorage != nil {
			maxTempStorage = *pendingMaxTempStorage
		}
		scalarBytes := int64(0)
		if pendingScalarBytes != nil {
			scalarBytes = *pendingScalarBytes
		}
		sess.SetMaxScalarBytes(scalarBytes)
		pendingScalarBytes = nil
		// `# max_query_memory_bytes:` (memory.md §2) decorates only its record; absent ⇒ unlimited, or
		// the whole-corpus accounting budget of JED_CONFORMANCE_QUERY_MEMORY.
		queryMemory := queryMemoryDefault
		if pendingQueryMemory != nil {
			queryMemory = *pendingQueryMemory
		}
		sess.SetMaxQueryMemoryBytes(queryMemory)
		pendingQueryMemory = nil
		underflowsBefore := jed.QueryMemoryUnderflows()
		jed.ResetQueryMemoryPeak()
		recordOrdinal++
		sess.SetMaxTempStorageBytes(maxTempStorage)
		pendingMaxTempStorage = nil
		// Apply the per-record session variables (spec/design/session.md §6.1): clear, then set each
		// pending # set: pair, so a directive decorates only its record and never leaks forward.
		sess.ResetVars()
		for _, v := range pendingVars {
			if err := sess.SetVar(v.name, v.value); err != nil {
				return fmt.Errorf("# set: directive uses a non-dotted variable name %q: %w", v.name, err)
			}
		}
		pendingVars = nil
		// Apply the per-record session time zone (spec/design/session.md §6.2, timezones.md §9.4):
		// reset to UTC, then set the pending # timezone:, so a directive decorates only its record and
		// never leaks forward. A named zone must already be loaded (# load-timezone:).
		tz := "UTC"
		if pendingTimezone != nil {
			tz = *pendingTimezone
		}
		if err := sess.SetTimeZone(tz); err != nil {
			return fmt.Errorf("# timezone: %w", err)
		}
		pendingTimezone = nil
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
			// A statement runs through the total Query seam too (a no-row statement is a valid Query).
			_, _, _, cost, err := drainQuery(sess, sql, params, "nosort", 1)
			switch expect {
			case "ok":
				if err != nil {
					return fmt.Errorf("statement expected ok, got error %s\n  SQL: %s", err.Error(), sql)
				}
				if cerr := assertCost(expectedCost, cost, sql); cerr != nil {
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
			cols := len(coltypes)
			if cols == 0 {
				cols = 1
			}
			actual, names, types, cost, err := drainQuery(sess, sql, params, sortmode, cols)
			if err != nil {
				return fmt.Errorf("query failed with %s\n  SQL: %s", err.Error(), sql)
			}
			expected = applySort(expected, cols, sortmode)
			if !equalColtyped(actual, expected, coltypes, cols) {
				return fmt.Errorf("query result mismatch\n  SQL: %s\n  expected: %v\n  actual:   %v", sql, expected, actual)
			}
			if cerr := assertCost(expectedCost, cost, sql); cerr != nil {
				return cerr
			}
			if nerr := assertNames(expectedNames, names, sql); nerr != nil {
				return nerr
			}
			if terr := assertTypes(expectedTypes, types, sql); terr != nil {
				return terr
			}
		default:
			return fmt.Errorf("unknown record kind %q", fields[0])
		}
		recordPeak(recordOrdinal, db)
		if jed.QueryMemoryUnderflows() != underflowsBefore {
			return fmt.Errorf("query-memory accounting released more than it reserved (memory.md §5)\n  record: %s", line)
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

// drainQuery runs sql through the session's TOTAL query seam and fully drains it, returning the
// flattened+sorted result cells, the column metadata, and the final accrued cost. Routing BOTH
// statement and query directives through the ergonomic Query (there is no raw Execute/Outcome
// primitive any more) is the harness's standing proof that the seam is total: a statement that returns
// no rows is a valid query with an empty column list (spec/design/api.md §11). The cursor is always
// Closed (releasing any streaming reader-liveness pin), and Cost is read after the full drain — a
// streaming query's cost is final only at exhaustion.
func drainQuery(sess *jed.Session, sql string, params []jed.Value, sortmode string, cols int) (cells, names, types []string, cost int64, err error) {
	args := make([]any, len(params))
	for i, p := range params {
		args[i] = p
	}
	rows, qerr := sess.Query(context.Background(), sql, args...)
	if qerr != nil {
		return nil, nil, nil, 0, qerr
	}
	defer rows.Close()
	var values [][]jed.Value
	for rows.Next() {
		values = append(values, append([]jed.Value(nil), rows.Row()...))
	}
	if derr := rows.Err(); derr != nil {
		return nil, nil, nil, 0, derr
	}
	flat, rerr := renderCells(sess, values)
	if rerr != nil {
		return nil, nil, nil, 0, rerr
	}
	return applySort(flat, cols, sortmode), rows.ColumnNames(), rows.ColumnTypes(), rows.Cost(), nil
}

// renderCells renders every drained result cell through the session that ran the record
// (Session.Render), so a timestamptz shows in that session's `# timezone:` zone (spec/design/
// timezones.md §9.5). It runs after the full drain, while the record's session zone is still in
// effect; a render failure (22008, only at jed's out-of-PG-range extremes) fails the record like an
// output-time error fails a PostgreSQL query.
func renderCells(sess *jed.Session, values [][]jed.Value) ([]string, error) {
	var flat []string
	for _, row := range values {
		for _, v := range row {
			s, err := sess.Render(v)
			if err != nil {
				return nil, err
			}
			flat = append(flat, s)
		}
	}
	return flat, nil
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
