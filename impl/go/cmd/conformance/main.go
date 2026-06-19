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
	for i < len(lines) {
		line := strings.TrimSpace(lines[i])
		if line == "" {
			i++
			continue
		}
		if strings.HasPrefix(line, "#") {
			// `# cost:` / `# names:` / `# types:` bind to the next record; every other comment
			// is ignored.
			if n, ok := parseCostDirective(line); ok {
				pendingCost = &n
			} else if n, ok := parseMaxCostDirective(line); ok {
				pendingMaxCost = &n
			} else if n, ok := parseMaxSQLLengthDirective(line); ok {
				pendingMaxSQLLength = &n
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
