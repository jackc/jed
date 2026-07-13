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
