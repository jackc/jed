package jed

import (
	"fmt"
	"strings"
	"testing"
)

// The single-table streaming sort reads only the touched columns into a reused row, and its top-k
// heap takes a row only when it keeps it (packed-leaf.md §11 Track A5; mirrors the Rust core's
// top_n_matches_full_sort). Every LIMIT/OFFSET window must be that slice of the full sort, and the
// full sort must agree with the same query over a derived table, which the eager executor sorts —
// across fixed-width, bounded-decimal, unbounded and text keys (on a file the last two take the
// external sorter), with ties, NULLs, a filter, a writer's uncommitted leaves, and query-memory
// budgets.

// sortedRowList is one ordered query's rows, or its error code.
func sortedRowList(s *Session, sql string) ([]string, string) {
	out, err := queryOutcome(s, sql, nil)
	if err != nil {
		if e, ok := err.(*EngineError); ok {
			return nil, e.Code()
		}
		return nil, err.Error()
	}
	return strings.Split(laneRows(out, sql), "\n"), ""
}

func checkTopN(t *testing.T, s *Session, budgets bool) {
	t.Helper()
	keys := []string{
		"d DESC, id",
		"d NULLS FIRST, id DESC",
		"n, d DESC NULLS LAST, id",
		"big DESC NULLS FIRST, id",
		"wide DESC NULLS LAST, id",
		"s, id",
		"f DESC, m, id",
		"m DESC NULLS LAST, id",
	}
	type cs struct{ key, cols string }
	var cases []cs
	// Every key over a projection that also touches text and an unbounded decimal (on a file: the
	// external sorter), and the keys that need neither over a fixed-width and bounded-decimal
	// projection (on a file: the top-k heap).
	for _, k := range keys {
		cases = append(cases, cs{k, "id, d, s, big, n, wide, f, m"})
	}
	for _, k := range keys {
		if !strings.Contains(k, "wide") && !strings.HasPrefix(k, "s,") {
			cases = append(cases, cs{k, "id, d, big, n, f, m"})
		}
	}
	windows := [][2]int{{10, 0}, {7, 5}, {1, 3003}, {5000, 0}, {0, 0}, {3, 2999}}
	for _, c := range cases {
		for _, filter := range []string{"", " WHERE n > 1 AND d <> 0", " WHERE id < -100"} {
			base := fmt.Sprintf("SELECT %s FROM gt%s", c.cols, filter)
			full, code := sortedRowList(s, base+" ORDER BY "+c.key)
			if code != "" {
				t.Fatalf("%s ORDER BY %s: %s", base, c.key, code)
			}
			eager, code := sortedRowList(s, fmt.Sprintf("SELECT * FROM (%s) q ORDER BY %s", base, c.key))
			if code != "" || strings.Join(full, "\n") != strings.Join(eager, "\n") {
				t.Fatalf("full sort vs eager sort (%s): %s ORDER BY %s", code, base, c.key)
			}
			if len(full) == 1 && full[0] == "" {
				full = nil // no rows
			}
			for _, w := range windows {
				limit, offset := w[0], w[1]
				sql := fmt.Sprintf("%s ORDER BY %s LIMIT %d OFFSET %d", base, c.key, limit, offset)
				var want []string
				if offset < len(full) {
					want = full[offset:min(len(full), offset+limit)]
				}
				check := func(what string) {
					got, code := sortedRowList(s, sql)
					if len(got) == 1 && got[0] == "" {
						got = nil
					}
					if code == "54P05" && what != "" {
						return // under a budget a statement may fail 54P05, but never returns other rows
					}
					if code != "" || strings.Join(got, "\n") != strings.Join(want, "\n") {
						t.Fatalf("%s%s: code %q, %d rows, want %d", what, sql, code, len(got), len(want))
					}
				}
				check("")
				// Budgets on the two windows a heap serves; tiny ones make the sorter spill row by row.
				if budgets && limit > 1 && limit < 100 {
					for _, budget := range []int64{4096, 1024 * 1024} {
						s.SetMaxQueryMemoryBytes(budget)
						check(fmt.Sprintf("budget %d: ", budget))
						s.SetMaxQueryMemoryBytes(0)
					}
				}
			}
		}
	}
}

func topNMatchesFullSort(t *testing.T, file string) {
	s := laneSession(t, file, gtSeed)
	workMems := []int{defaultWorkMem}
	if file != "" {
		workMems = []int{40_000, defaultWorkMem} // the smaller one makes the external sorter spill
	}
	for _, workMem := range workMems {
		s.SetWorkMem(workMem)
		checkTopN(t, s, true)
	}
	// A writer's uncommitted (Decoded) leaves. A budget failure would abort the open transaction, so
	// this pass runs without budgets.
	mustExec(t, s, "BEGIN")
	mustExec(t, s, "INSERT INTO gt VALUES (5000, 9, 9, 42, 249999.99, 1e40, 'zz', 3.5, 7), (5001, 1, 1, 42, 249999.99, NULL, '', 3.5, NULL)")
	mustExec(t, s, "UPDATE gt SET d = d + 1, s = s || '!' WHERE n = 3")
	checkTopN(t, s, false)
	mustExec(t, s, "ROLLBACK")
}

func TestTopNMatchesFullSortOnAFile(t *testing.T) {
	t.Parallel()
	topNMatchesFullSort(t, "top_n_lanes.jed")
}

func TestTopNMatchesFullSortInMemory(t *testing.T) {
	t.Parallel()
	topNMatchesFullSort(t, "")
}
