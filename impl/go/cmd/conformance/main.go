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

		if err := runFile(text); err != nil {
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

// runFile runs all records in one .test file against a fresh database.
func runFile(text string) error {
	db := jed.NewDatabase()
	lines := strings.Split(text, "\n")
	i := 0
	// A `# cost: N` / `# names: ...` directive sets these; the next record consumes them.
	var pendingCost *int64
	var pendingNames []string
	for i < len(lines) {
		line := strings.TrimSpace(lines[i])
		if line == "" {
			i++
			continue
		}
		if strings.HasPrefix(line, "#") {
			// `# cost:` / `# names:` bind to the next record; every other comment is ignored.
			if n, ok := parseCostDirective(line); ok {
				pendingCost = &n
			} else if names, ok := parseNamesDirective(line); ok {
				pendingNames = names
			}
			i++
			continue
		}
		// This record consumes any pending assertions (so they never leak forward).
		expectedCost := pendingCost
		expectedNames := pendingNames
		pendingCost = nil
		pendingNames = nil
		fields := strings.Fields(line)
		switch fields[0] {
		case "statement":
			// A `# names:` directive asserts result columns, which a statement lacks.
			if expectedNames != nil {
				return fmt.Errorf("# names: directive precedes a non-query statement")
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
			if !equal(actual, expected) {
				return fmt.Errorf("query result mismatch\n  SQL: %s\n  expected: %v\n  actual:   %v", sql, expected, actual)
			}
			if cerr := assertCost(expectedCost, outcome.Cost, sql); cerr != nil {
				return cerr
			}
			if nerr := assertNames(expectedNames, outcome.ColumnNames, sql); nerr != nil {
				return nerr
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
