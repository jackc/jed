package jed

// Overflow-chain page_read accrual (spec/design/large-values.md §8.1/§12; cost.md §3 "page_read").
// A scan's up-front page_read block counts the B-tree nodes the bound intersects PLUS one per
// overflow chain page of every record the bound admits. The conformance corpus cannot exercise
// this (its tables use the 8 KiB default page, where nothing spills), so these tests pin the
// accrual at page_size 256 by comparing a spilling table against a control table of identical
// shape (same schema, same keys, same row count, one leaf each) whose values stay inline — the
// cost delta is exactly the chain pages. Mirrored in Rust (tests/overflow_cost.rs) and TS
// (tests/overflow_cost.test.ts).

import (
	"strings"
	"testing"
)

// page_size 256 ⇒ cap = 244, RECORD_MAX = 116. A 600-byte text payload spills into
// ceil(600/244) = 3 overflow pages; a 300-byte bytea into ceil(300/244) = 2.
const (
	overflowPageSize   = 256
	textChainPages     = 3
	byteaChainPages    = 2
	overflowBodyLength = 600
)

// overflowTables builds two tables of identical shape: `spill` row 1 carries a 600-char text
// (3-page chain), `control` keeps every value inline. Row 2 is inline in both.
func overflowTables(t *testing.T) *Database {
	t.Helper()
	db := WithPageSize(overflowPageSize)
	big := strings.Repeat("x", overflowBodyLength)
	mustExec(t, db, "CREATE TABLE spill (id int32 PRIMARY KEY, body text)")
	mustExec(t, db, "INSERT INTO spill VALUES (1, '"+big+"'), (2, 'small')")
	mustExec(t, db, "CREATE TABLE control (id int32 PRIMARY KEY, body text)")
	mustExec(t, db, "INSERT INTO control VALUES (1, 'tiny'), (2, 'small')")
	return db
}

func TestOverflowCostFullScanChargesChainPages(t *testing.T) {
	db := overflowTables(t)
	spill := mustCost(t, db, "SELECT * FROM spill")
	control := mustCost(t, db, "SELECT * FROM control")
	// Identical plans, rows, and tree shape — the only difference is the 3-page chain.
	if spill != control+textChainPages {
		t.Fatalf("full scan: spill cost %d, want control %d + %d", spill, control, textChainPages)
	}
}

func TestOverflowCostBoundedScanChargesOnlyAdmittedChains(t *testing.T) {
	db := overflowTables(t)
	// The point lookup that admits the spilled record pays its chain ...
	spillHit := mustCost(t, db, "SELECT * FROM spill WHERE id = 1")
	controlHit := mustCost(t, db, "SELECT * FROM control WHERE id = 1")
	if spillHit != controlHit+textChainPages {
		t.Fatalf("spilled lookup: cost %d, want control %d + %d", spillHit, controlHit, textChainPages)
	}
	// ... the one that admits only the inline record pays nothing extra.
	spillInline := mustCost(t, db, "SELECT * FROM spill WHERE id = 2")
	controlInline := mustCost(t, db, "SELECT * FROM control WHERE id = 2")
	if spillInline != controlInline {
		t.Fatalf("inline lookup: cost %d, want %d", spillInline, controlInline)
	}
}

func TestOverflowCostLimitDoesNotLowerTheBlock(t *testing.T) {
	// The spilled record is row 2, so LIMIT 1 emits only the inline row 1 — yet the page_read
	// block (which never short-circuits — cost.md §3 "LIMIT short-circuit") still counts the
	// bound's chain pages.
	db := WithPageSize(overflowPageSize)
	big := strings.Repeat("x", overflowBodyLength)
	mustExec(t, db, "CREATE TABLE spill (id int32 PRIMARY KEY, body text)")
	mustExec(t, db, "INSERT INTO spill VALUES (1, 'small'), (2, '"+big+"')")
	mustExec(t, db, "CREATE TABLE control (id int32 PRIMARY KEY, body text)")
	mustExec(t, db, "INSERT INTO control VALUES (1, 'small'), (2, 'tiny')")
	spill := mustCost(t, db, "SELECT * FROM spill LIMIT 1")
	control := mustCost(t, db, "SELECT * FROM control LIMIT 1")
	if spill != control+textChainPages {
		t.Fatalf("LIMIT 1: spill cost %d, want control %d + %d", spill, control, textChainPages)
	}
}

func TestOverflowCostMutationScansChargeChainPages(t *testing.T) {
	db := overflowTables(t)
	spill := mustCost(t, db, "DELETE FROM spill")
	control := mustCost(t, db, "DELETE FROM control")
	if spill != control+textChainPages {
		t.Fatalf("DELETE: spill cost %d, want control %d + %d", spill, control, textChainPages)
	}
}

func TestOverflowCostMultipleChainsSum(t *testing.T) {
	// One record with two externalized values charges the sum of both chains: 3 + 2 = 5.
	db := WithPageSize(overflowPageSize)
	bigText := strings.Repeat("x", overflowBodyLength)
	bigHex := strings.Repeat("ab", 300)
	mustExec(t, db, "CREATE TABLE spill (id int32 PRIMARY KEY, body text, blob bytea)")
	mustExec(t, db, "INSERT INTO spill VALUES (1, '"+bigText+"', '\\x"+bigHex+"')")
	mustExec(t, db, "CREATE TABLE control (id int32 PRIMARY KEY, body text, blob bytea)")
	mustExec(t, db, "INSERT INTO control VALUES (1, 'tiny', '\\xcafe')")
	spill := mustCost(t, db, "SELECT * FROM spill")
	control := mustCost(t, db, "SELECT * FROM control")
	if spill != control+textChainPages+byteaChainPages {
		t.Fatalf("two chains: spill cost %d, want control %d + %d", spill, control, textChainPages+byteaChainPages)
	}
}
