package jed

import (
	"slices"
	"testing"
)

// Candidate inventory is an internal planner invariant the SQL corpus cannot render: EXPLAIN shows
// only the selected path. This mirrors the Rust/TS white-box case and uses one mixed relation with
// two eligible indexes of every index-bearing kind plus both interval-set kinds.
func TestScanCandidateInventoryIsCompleteCanonicalAndLegacyNeutral(t *testing.T) {
	t.Parallel()
	db := newEngine()
	for _, sql := range []string{
		"CREATE TABLE inventory (id i32 PRIMARY KEY, a i32, b i32, tags i32[], span i32range)",
		"CREATE INDEX z_btree ON inventory (b)",
		"CREATE INDEX a_btree ON inventory (a)",
		"CREATE INDEX z_gin ON inventory USING gin (tags)",
		"CREATE INDEX a_gin ON inventory USING gin (tags)",
		"CREATE INDEX z_gist ON inventory USING gist (span)",
		"CREATE INDEX a_gist ON inventory USING gist (span)",
		"INSERT INTO inventory VALUES (1, 1, 1, '{1}', '[1,3)')",
		"INSERT INTO inventory VALUES (2, 2, 2, '{1,2}', '[2,4)')",
		"INSERT INTO inventory VALUES (3, 3, 3, '{3}', '[5,8)')",
		"INSERT INTO inventory VALUES (4, 4, 4, '{4}', '[9,12)')",
	} {
		if _, err := execute(db, sql); err != nil {
			t.Fatalf("%s: %v", sql, err)
		}
	}

	filter := plannedInventoryFilter(t, db, `
		SELECT id FROM inventory WHERE
		(id = 1 OR id = 2) AND id >= 0 AND
		(a = 1 OR a = 2) AND a >= 0 AND
		(b = 1 OR b = 2) AND b >= 0 AND
		tags @> ARRAY[1] AND span && i32range(1, 3)
	`)
	table, ok := db.lkpTable("inventory")
	if !ok {
		t.Fatal("inventory table missing")
	}
	// Deliberately scramble the catalog slice: inventory order must come from canonical identity,
	// never the container's current iteration order.
	tableCopy := *table
	tableCopy.Indexes = append([]indexDef(nil), table.Indexes...)
	slices.Reverse(tableCopy.Indexes)
	rel := scopeRel{label: "inventory", table: &tableCopy, offset: 0}
	candidates := inventoryScanCandidates(filter, rel, db)
	want := []string{
		"pk",
		"btree:a_btree", "btree:z_btree",
		"gist:a_gist", "gist:z_gist",
		"gin:a_gin", "gin:z_gin",
		"pk_interval",
		"index_interval:a_btree", "index_interval:z_btree",
		"full",
	}
	got := make([]string, len(candidates))
	for i, candidate := range candidates {
		got[i] = candidate.identity.String()
		if candidate.residual != filter {
			t.Fatalf("%s does not retain the complete WHERE residual", got[i])
		}
		if candidate.identity.kind == scanCandidateFull {
			if candidate.bound != nil {
				t.Fatal("full candidate has a physical bound")
			}
		} else if candidate.bound == nil {
			t.Fatalf("%s has no physical bound", got[i])
		}
		switch candidate.identity.kind {
		case scanCandidateBtree, scanCandidateIndexInterval:
			if candidate.scanOrder.kind != scanOrderIndexKey || candidate.scanOrder.indexName != candidate.identity.indexName || candidate.scanOrder.reversible {
				t.Fatalf("%s has wrong ordered-index capability: %+v", got[i], candidate.scanOrder)
			}
		default:
			if candidate.scanOrder.kind != scanOrderStorageKey || !candidate.scanOrder.reversible {
				t.Fatalf("%s has wrong storage-key capability: %+v", got[i], candidate.scanOrder)
			}
		}
	}
	if !slices.Equal(got, want) {
		t.Fatalf("candidate identities\n got: %v\nwant: %v", got, want)
	}
	estimates := db.estimateScanCandidates(candidates, rel, true)
	if len(estimates) != len(candidates) {
		t.Fatalf("candidate estimates = %d, candidates = %d", len(estimates), len(candidates))
	}
	logicalRows := estimates[0].rows
	for i, estimate := range estimates {
		wantTie := candidateTieKey(estimatorAccessPathOrder[int(candidates[i].identity.kind)], candidates[i].identity.indexName)
		if estimate.tieKey != wantTie || estimate.rows != logicalRows || estimate.cost < 0 {
			t.Fatalf("%s candidate estimate = %+v, logical rows %d tie %q", got[i], estimate, logicalRows, wantTie)
		}
	}
	for _, check := range []struct {
		sql            string
		rows           int64
		emptyCandidate string
	}{
		{sql: "SELECT id FROM inventory WHERE a IN (1, 1, 1, 1, 1)", rows: 1},
		{sql: "SELECT id FROM inventory WHERE a = NULL", rows: 0, emptyCandidate: "btree:a_btree"},
		{sql: "SELECT id FROM inventory WHERE a = 1 AND a = 2", rows: 0, emptyCandidate: "btree:a_btree"},
		{sql: "SELECT id FROM inventory WHERE a > 3 AND a < 2", rows: 0, emptyCandidate: "btree:a_btree"},
	} {
		shapeFilter := plannedInventoryFilter(t, db, check.sql)
		shapeCandidates := inventoryScanCandidates(shapeFilter, rel, db)
		shapeEstimates := db.estimateScanCandidates(shapeCandidates, rel, true)
		for i, estimate := range shapeEstimates {
			if estimate.rows != check.rows {
				t.Fatalf("%s %s output rows = %d, want %d", check.sql, shapeCandidates[i].identity, estimate.rows, check.rows)
			}
			if shapeCandidates[i].identity.String() == check.emptyCandidate && estimate.cost != 0 {
				t.Fatalf("%s %s empty access cost = %d, want 0", check.sql, check.emptyCandidate, estimate.cost)
			}
		}
	}
	fullCandidates := inventoryScanCandidates(nil, rel, db)
	fullEstimate := db.estimateScanCandidates(fullCandidates, rel, true)[0]
	fullActual, err := execute(db, "SELECT id FROM inventory")
	if err != nil {
		t.Fatal(err)
	}
	if fullEstimate.cost != fullActual.Cost {
		t.Fatalf("exact full-scan estimate cost = %d, actual = %d", fullEstimate.cost, fullActual.Cost)
	}
	// The direct id>=0 / a>=0 / b>=0 conjuncts clip their interval unions. Legacy selection must
	// retain the pre-P3 exception where the clipped set replaces the broader contiguous PK bound.
	if selected := selectLegacyScanCandidate(candidates, selectScanBoundPolicy); selected == nil || selected.pkSet == nil {
		t.Fatalf("SELECT legacy selector lost clipped PK interval precedence: %+v", selected)
	}
	indexClipFilter := plannedInventoryFilter(t, db,
		"SELECT id FROM inventory WHERE (a = 1 OR a = 2) AND a >= 0 AND (b = 1 OR b = 2) AND b >= 0")
	selected := selectLegacyScanCandidate(inventoryScanCandidates(indexClipFilter, rel, db), selectScanBoundPolicy)
	if selected == nil || selected.indexSet == nil || selected.indexSet.nameKey != "a_btree" {
		t.Fatalf("SELECT legacy selector lost clipped lowest-index interval precedence: %+v", selected)
	}

	opclassFilter := plannedInventoryFilter(t, db,
		"SELECT id FROM inventory WHERE tags @> ARRAY[1] AND span && i32range(1, 3)")
	opclassCandidates := inventoryScanCandidates(opclassFilter, rel, db)
	selected = selectLegacyScanCandidate(opclassCandidates, selectScanBoundPolicy)
	if selected == nil || selected.gist == nil || selected.gist.nameKey != "a_gist" {
		t.Fatalf("SELECT legacy selector = %+v, want lowest GiST", selected)
	}
	selected = selectLegacyScanCandidate(opclassCandidates, mutationScanBoundPolicy)
	if selected == nil || selected.gin == nil || selected.gin.nameKey != "a_gin" {
		t.Fatalf("mutation legacy selector = %+v, want lowest GIN", selected)
	}
}

func plannedInventoryFilter(t *testing.T, db *engine, sql string) *rExpr {
	t.Helper()
	stmt, err := db.parse(sql)
	if err != nil {
		t.Fatal(err)
	}
	plan, err := db.planSelect(stmt.Select, nil, nil, &paramTypes{})
	if err != nil {
		t.Fatal(err)
	}
	if plan.filter == nil {
		t.Fatal("planned inventory query has no filter")
	}
	return plan.filter
}
