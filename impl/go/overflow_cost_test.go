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
	"testing"
)

// page_size 256 ⇒ cap = 240, RECORD_MAX = 114. A 600-byte text payload spills into
// ceil(600/240) = 3 overflow pages; a 300-byte bytea into ceil(300/240) = 2. Payloads are
// incompressible filler (fillerText/fillerBytesHex — fileformat_golden_test.go) so Slice B's
// compress pass rejects them (store-smaller) and they genuinely spill plain — compression's own
// costs are pinned in compressed_cost_test.go.
const (
	overflowPageSize   = 256
	textChainPages     = 3
	byteaChainPages    = 2
	overflowBodyLength = 600
)

// overflowTables builds two tables of identical shape: `spill` row 1 carries a 600-char text
// (3-page chain), `control` keeps every value inline. Row 2 is inline in both.
func overflowTables(t *testing.T) *engine {
	t.Helper()
	db := withPageSize(overflowPageSize)
	big := fillerText(overflowBodyLength)
	mustExec(t, db, "CREATE TABLE spill (id i32 PRIMARY KEY, body text)")
	mustExec(t, db, "INSERT INTO spill VALUES (1, '"+big+"'), (2, 'small')")
	mustExec(t, db, "CREATE TABLE control (id i32 PRIMARY KEY, body text)")
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
	db := withPageSize(overflowPageSize)
	big := fillerText(overflowBodyLength)
	mustExec(t, db, "CREATE TABLE spill (id i32 PRIMARY KEY, body text)")
	mustExec(t, db, "INSERT INTO spill VALUES (1, 'small'), (2, '"+big+"')")
	mustExec(t, db, "CREATE TABLE control (id i32 PRIMARY KEY, body text)")
	mustExec(t, db, "INSERT INTO control VALUES (1, 'small'), (2, 'tiny')")
	spill := mustCost(t, db, "SELECT * FROM spill LIMIT 1")
	control := mustCost(t, db, "SELECT * FROM control LIMIT 1")
	if spill != control+textChainPages {
		t.Fatalf("LIMIT 1: spill cost %d, want control %d + %d", spill, control, textChainPages)
	}
}

func TestOverflowCostMutationScansChargeOnlyTouchedChains(t *testing.T) {
	// A DELETE whose filter READS the spilled column pays its chain (the touched set —
	// cost.md §3); a bare DELETE reads no column, so dropping the rows charges nothing extra.
	db := overflowTables(t)
	spillTouch := mustCost(t, db, "DELETE FROM spill WHERE body = 'nope'")
	controlTouch := mustCost(t, db, "DELETE FROM control WHERE body = 'nope'")
	if spillTouch != controlTouch+textChainPages {
		t.Fatalf("touching DELETE: spill %d, want control %d + %d", spillTouch, controlTouch, textChainPages)
	}
	spillBare := mustCost(t, db, "DELETE FROM spill")
	controlBare := mustCost(t, db, "DELETE FROM control")
	if spillBare != controlBare {
		t.Fatalf("bare DELETE: spill %d, want control %d", spillBare, controlBare)
	}
}

func TestOverflowCostUntouchedColumnsChargeNothing(t *testing.T) {
	// The touched set (cost.md §3 "The touched set"): a query that never references the spilled
	// column pays neither its chain pages nor anything else for it — the large-values.md §7
	// headline case — while one that does still pays.
	db := overflowTables(t)
	// Projection-only touch ...
	if s, c := mustCost(t, db, "SELECT id FROM spill"), mustCost(t, db, "SELECT id FROM control"); s != c {
		t.Fatalf("SELECT id: spill %d, want control %d", s, c)
	}
	// ... an aggregate touches only its argument (count(*) touches nothing) ...
	if s, c := mustCost(t, db, "SELECT count(*) FROM spill"), mustCost(t, db, "SELECT count(*) FROM control"); s != c {
		t.Fatalf("count(*): spill %d, want control %d", s, c)
	}
	// ... a WHERE reference is a touch even when only `id` is projected ...
	s := mustCost(t, db, "SELECT id FROM spill WHERE body = 'nope'")
	c := mustCost(t, db, "SELECT id FROM control WHERE body = 'nope'")
	if s != c+textChainPages {
		t.Fatalf("WHERE body: spill %d, want control %d + %d", s, c, textChainPages)
	}
	// ... and an UPDATE that ASSIGNS the spilled column without reading it (a constant
	// source) skips its chain too — only assignment sources touch, not targets.
	su := mustCost(t, db, "UPDATE spill SET body = 'tiny2' WHERE id = 2")
	cu := mustCost(t, db, "UPDATE control SET body = 'tiny2' WHERE id = 2")
	if su != cu {
		t.Fatalf("UPDATE const: spill %d, want control %d", su, cu)
	}
}

func TestOverflowCostMultipleChainsSum(t *testing.T) {
	// One record with two externalized values charges the sum of both chains: 3 + 2 = 5.
	db := withPageSize(overflowPageSize)
	bigText := fillerText(overflowBodyLength)
	bigHex := fillerBytesHex(300)
	mustExec(t, db, "CREATE TABLE spill (id i32 PRIMARY KEY, body text, blob bytea)")
	mustExec(t, db, "INSERT INTO spill VALUES (1, '"+bigText+"', '\\x"+bigHex+"')")
	mustExec(t, db, "CREATE TABLE control (id i32 PRIMARY KEY, body text, blob bytea)")
	mustExec(t, db, "INSERT INTO control VALUES (1, 'tiny', '\\xcafe')")
	spill := mustCost(t, db, "SELECT * FROM spill")
	control := mustCost(t, db, "SELECT * FROM control")
	if spill != control+textChainPages+byteaChainPages {
		t.Fatalf("two chains: spill cost %d, want control %d + %d", spill, control, textChainPages+byteaChainPages)
	}
}
