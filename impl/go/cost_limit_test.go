package jed

// Cost ceiling + deterministic abort (CLAUDE.md §13; spec/design/cost.md §6). A caller sets
// max_cost on the handle; the instant a statement's accrued execution cost reaches it, execution
// aborts with 54P01. The conformance corpus (spec/conformance/suites/resource/cost_limit.test) pins
// the cross-core abort points on small tables; this exercises what it cannot — that the bound is on
// ACTUAL accrued cost (a cheap point lookup survives a ceiling a full scan blows) and that the abort
// threads through SELECT / DELETE / UPDATE and a pathological expression.

import (
	"fmt"
	"strings"
	"testing"
)

// rowTable builds a table of n rows (id i32 PRIMARY KEY, v i32; v == id).
func rowTable(t *testing.T, n int) *Session {
	t.Helper()
	var b strings.Builder
	b.WriteString("INSERT INTO t VALUES ")
	for i := 1; i <= n; i++ {
		if i > 1 {
			b.WriteByte(',')
		}
		fmt.Fprintf(&b, "(%d,%d)", i, i)
	}
	return dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)", b.String())
}

func mustCost(t *testing.T, db dbHandle, sql string) int64 {
	t.Helper()
	out, err := queryOutcome(db, sql, nil)
	if err != nil {
		t.Fatalf("%s: unexpected error %v", sql, err)
	}
	return out.Cost
}

func assertAborts(t *testing.T, db dbHandle, sql string) {
	t.Helper()
	_, err := queryOutcome(db, sql, nil)
	if err == nil {
		t.Fatalf("expected cost-limit abort, but %q succeeded", sql)
	}
	if ee, ok := err.(*EngineError); !ok || ee.Code() != "54P01" {
		t.Fatalf("expected 54P01 abort, got %v", err)
	}
}

func TestCostLimitUnlimitedByDefault(t *testing.T) {
	db := rowTable(t, 100)
	if db.MaxCost() != 0 {
		t.Fatalf("default max_cost = %d, want 0 (unlimited)", db.MaxCost())
	}
	mustCost(t, db, "SELECT * FROM t") // runs to completion, no ceiling
}

func TestCostLimitAboveSucceedsBelowAborts(t *testing.T) {
	db := rowTable(t, 50)
	full := mustCost(t, db, "SELECT v FROM t")
	if full <= 10 {
		t.Fatalf("expected a non-trivial full-scan cost, got %d", full)
	}

	db.SetMaxCost(full + 100) // comfortably above → unchanged
	if got := mustCost(t, db, "SELECT v FROM t"); got != full {
		t.Errorf("under a high ceiling cost = %d, want %d", got, full)
	}

	db.SetMaxCost(full / 2) // below → aborts
	assertAborts(t, db, "SELECT v FROM t")

	db.SetMaxCost(0) // cleared → unlimited again
	if got := mustCost(t, db, "SELECT v FROM t"); got != full {
		t.Errorf("after clearing the ceiling cost = %d, want %d", got, full)
	}
}

func TestCostLimitExactBoundaryAborts(t *testing.T) {
	db := rowTable(t, 20)
	full := mustCost(t, db, "SELECT v FROM t")
	// The ceiling is the first DISALLOWED value: accrued reaching it aborts (CLAUDE.md §13).
	db.SetMaxCost(full)
	assertAborts(t, db, "SELECT v FROM t")
	db.SetMaxCost(full + 1)
	if got := mustCost(t, db, "SELECT v FROM t"); got != full {
		t.Errorf("one above the true cost should succeed: got %d, want %d", got, full)
	}
}

func TestCostLimitPointLookupSurvivesScanCeiling(t *testing.T) {
	db := rowTable(t, 200)
	full := mustCost(t, db, "SELECT v FROM t")
	lookup := mustCost(t, db, "SELECT v FROM t WHERE id = 100")
	if lookup*4 >= full {
		t.Fatalf("point lookup (%d) should be far cheaper than the full scan (%d)", lookup, full)
	}
	// A ceiling between the two lets the keyed lookup through but stops the scan. The bound is on
	// real accrued cost, not table size.
	db.SetMaxCost((lookup + full) / 2)
	if got := mustCost(t, db, "SELECT v FROM t WHERE id = 100"); got != lookup {
		t.Errorf("point lookup under the ceiling cost = %d, want %d", got, lookup)
	}
	assertAborts(t, db, "SELECT v FROM t")
}

func TestCostLimitThreadsThroughDeleteAndUpdate(t *testing.T) {
	db := rowTable(t, 50)
	scanCost := mustCost(t, db, "SELECT v FROM t")
	db.SetMaxCost(scanCost / 2)

	assertAborts(t, db, "DELETE FROM t WHERE v > 0")
	assertAborts(t, db, "UPDATE t SET v = v + 1 WHERE v > 0")

	// The aborts rolled back (autocommit): the table is untouched.
	db.SetMaxCost(0)
	out, err := queryOutcome(db, "SELECT v FROM t", nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(out.Rows) != 50 || out.Cost != scanCost {
		t.Errorf("after aborted mutations: rows=%d cost=%d, want 50 rows and cost %d", len(out.Rows), out.Cost, scanCost)
	}
}

func TestCostLimitPathologicalExpressionAbortsOnOneRow(t *testing.T) {
	db := rowTable(t, 1)
	// 1 + 1 + ... (many Adds) over the one row: the per-node eval guard stops it (cost.md §6).
	parts := make([]string, 80)
	for i := range parts {
		parts[i] = "1"
	}
	sql := "SELECT " + strings.Join(parts, " + ") + " FROM t"
	big := mustCost(t, db, sql)
	db.SetMaxCost(big / 2)
	assertAborts(t, db, sql)
}

func TestCostLimitEmptyBoundUnderTinyCeiling(t *testing.T) {
	// A provably-empty primary-key bound reads no page and no row, so it accrues 0 and survives even
	// a ceiling of 1 (a point-lookup MISS differs — it still visits a leaf, charging one page_read).
	db := rowTable(t, 10)
	db.SetMaxCost(1)
	out, err := queryOutcome(db, "SELECT v FROM t WHERE id > 5 AND id < 5", nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(out.Rows) != 0 || out.Cost != 0 {
		t.Errorf("empty bound got rows=%d cost=%d, want 0 rows and cost 0", len(out.Rows), out.Cost)
	}
}
