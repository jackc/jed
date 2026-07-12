package jed

// Compression cost accrual (spec/design/cost.md §3 "the compression units";
// spec/design/large-values.md §13). value_decompress joins a scan's up-front block —
// ceil(raw/C) slabs per compressed stored value the bound admits — and value_compress meters
// every disposition-plan compress ATTEMPT (adopted or rejected) at the INSERT/UPDATE write
// site. The conformance corpus cannot exercise this (its 8 KiB pages never trigger the plan),
// so these tests pin the accrual at page_size 256 (cap C = 240, RECORD_MAX = 114) with
// spill-vs-control table deltas. Mirrors impl/rust/tests/compressed_cost.rs and
// impl/ts/tests/compressed_cost.test.ts. Uses fillerText from fileformat_golden_test.go.

import (
	"fmt"
	"strings"
	"testing"
)

const (
	compressedPageSize = 256
	// A 600-byte payload = ceil(600/240) = 3 slabs (compress at write, decompress at scan);
	// a 400-byte payload = 2 slabs.
	slabs600 = 3
	slabs400 = 2
)

// compressedTables builds `comp` whose row 1 carries a 600-char "x" run → 0x03
// inline-compressed (LZ4 shrinks it far under RECORD_MAX, so no chain), and `control` of the
// same shape fully inline-plain. Row 2 is inline in both. Same tree shape (one leaf each), so
// cost deltas isolate the compression units.
func compressedTables(t *testing.T) *Session {
	t.Helper()
	db := newInMemoryWithPageSize(compressedPageSize).Session(SessionOptions{})
	run600 := strings.Repeat("x", 600)
	mustExec(t, db, "CREATE TABLE comp (id i32 PRIMARY KEY, body text)")
	mustExec(t, db, "INSERT INTO comp VALUES (1, '"+run600+"'), (2, 'small')")
	mustExec(t, db, "CREATE TABLE control (id i32 PRIMARY KEY, body text)")
	// control row 1 is `plain` (5 chars), not a 4-char `tiny`: it must be at least as long as the
	// `small` probe value the correlated test compares against, so `probe.body = body` charges the
	// SAME varlen_compare (min(5, len) = 5) on both tables — keeping the comp−control delta the pure
	// compression cost, not a length-of-comparison artifact (cost.md §3 "varlen_compare").
	mustExec(t, db, "INSERT INTO control VALUES (1, 'plain'), (2, 'small')")
	return db
}

func TestCompressedCostScanChargesDecompressSlabs(t *testing.T) {
	t.Parallel()
	db := compressedTables(t)
	comp := mustCost(t, db, "SELECT * FROM comp")
	control := mustCost(t, db, "SELECT * FROM control")
	// Identical plans, rows, and tree shape — the only difference is the ceil(600/240) = 3
	// value_decompress slabs (no chain: the compressed form fits inline, so page_read is equal).
	if comp != control+slabs600 {
		t.Fatalf("full scan: comp %d, control %d (want +%d)", comp, control, slabs600)
	}
}

func TestCompressedCostExternalCompressedChargesChainPlusSlabs(t *testing.T) {
	t.Parallel()
	// A 400-char half-filler/half-run text compresses to ~212 B — smaller than plain but still
	// over RECORD_MAX → 0x04 external-compressed: ceil(212/240) = 1 chain page_read PLUS
	// ceil(400/240) = 2 value_decompress slabs.
	db := newInMemoryWithPageSize(compressedPageSize).Session(SessionOptions{})
	mix := fillerText(200) + strings.Repeat("y", 200)
	mustExec(t, db, "CREATE TABLE comp (id i32 PRIMARY KEY, body text)")
	mustExec(t, db, "INSERT INTO comp VALUES (1, '"+mix+"')")
	mustExec(t, db, "CREATE TABLE control (id i32 PRIMARY KEY, body text)")
	mustExec(t, db, "INSERT INTO control VALUES (1, 'tiny')")
	comp := mustCost(t, db, "SELECT * FROM comp")
	control := mustCost(t, db, "SELECT * FROM control")
	if comp != control+1+slabs400 {
		t.Fatalf("external-compressed scan: comp %d, control %d (want +%d)", comp, control, 1+slabs400)
	}
}

func TestCompressedCostBoundedScanAndLimit(t *testing.T) {
	t.Parallel()
	db := compressedTables(t)
	// The point lookup that admits the compressed record pays its slabs ...
	if d := mustCost(t, db, "SELECT * FROM comp WHERE id = 1") - mustCost(t, db, "SELECT * FROM control WHERE id = 1"); d != slabs600 {
		t.Fatalf("admitting lookup delta = %d, want %d", d, slabs600)
	}
	// ... the one that admits only the inline record pays nothing extra ...
	if d := mustCost(t, db, "SELECT * FROM comp WHERE id = 2") - mustCost(t, db, "SELECT * FROM control WHERE id = 2"); d != 0 {
		t.Fatalf("inline lookup delta = %d, want 0", d)
	}
	// ... and LIMIT does not lower the up-front block (cost.md §3 "LIMIT short-circuit").
	if d := mustCost(t, db, "SELECT * FROM comp LIMIT 1") - mustCost(t, db, "SELECT * FROM control LIMIT 1"); d != slabs600 {
		t.Fatalf("LIMIT delta = %d, want %d", d, slabs600)
	}
}

func TestCompressedCostInsertMetersAttemptsAdoptedOrRejected(t *testing.T) {
	t.Parallel()
	db := newInMemoryWithPageSize(compressedPageSize).Session(SessionOptions{})
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, body text)")
	// A fully-inline row attempts nothing: INSERT stays zero-cost.
	if c := mustCost(t, db, "INSERT INTO t VALUES (1, 'small')"); c != 0 {
		t.Fatalf("inline INSERT cost = %d, want 0", c)
	}
	// An adopted compression (the "x" run) costs its ceil(600/240) = 3 attempt slabs ...
	if c := mustCost(t, db, "INSERT INTO t VALUES (2, '"+strings.Repeat("x", 600)+"')"); c != slabs600 {
		t.Fatalf("adopted-attempt INSERT cost = %d, want %d", c, slabs600)
	}
	// ... and a REJECTED attempt (incompressible filler → external-plain) costs the same
	// slabs — the encoder ran either way (cost.md §3).
	if c := mustCost(t, db, "INSERT INTO t VALUES (3, '"+fillerText(600)+"')"); c != slabs600 {
		t.Fatalf("rejected-attempt INSERT cost = %d, want %d", c, slabs600)
	}
}

func TestCompressedCostUpdateMetersAttemptsPerRewrittenRow(t *testing.T) {
	t.Parallel()
	db := compressedTables(t)
	// Same bounded scan and evals both times; the only delta is the new value's compress
	// attempt: 3 slabs vs 0 (see the Rust mirror for the full reasoning).
	run600 := strings.Repeat("x", 600)
	big := mustCost(t, db, fmt.Sprintf("UPDATE comp SET body = '%s' WHERE id = 1", run600))
	small := mustCost(t, db, "UPDATE comp SET body = 'small' WHERE id = 1")
	if big != small+slabs600 {
		t.Fatalf("UPDATE delta: big %d, small %d (want +%d)", big, small, slabs600)
	}
}

func TestCompressedCostAlterAddColumnMetersAttemptsPerRewrittenRow(t *testing.T) {
	t.Parallel()
	db := compressedTables(t)
	readDelta := mustCost(t, db, "SELECT * FROM comp") - mustCost(t, db, "SELECT * FROM control")
	comp := mustCost(t, db, "ALTER TABLE comp ADD extra i32")
	control := mustCost(t, db, "ALTER TABLE control ADD extra i32")
	// Both ALTERs do the same scan/rewrite. The compressed table additionally pays the old value's
	// decompression slabs plus the replacement row's fresh compression attempt.
	if readDelta != slabs600 {
		t.Fatalf("read delta = %d, want %d", readDelta, slabs600)
	}
	if comp != control+readDelta+slabs600 {
		t.Fatalf("ALTER delta: comp %d, control %d, read %d (want another +%d)", comp, control, readDelta, slabs600)
	}
}

func TestCompressedCostDecimalPayloadsCompressToo(t *testing.T) {
	t.Parallel()
	// A long-coefficient decimal's body is a spillable payload like text/bytea
	// (large-values.md §12/§13): 801 digits → 201 base-10⁴ groups → a 407-byte payload,
	// ceil(407/240) = 2 slabs both ways.
	db := newInMemoryWithPageSize(compressedPageSize).Session(SessionOptions{})
	digits := strings.Repeat("12", 400) + ".5"
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, d numeric)")
	if c := mustCost(t, db, fmt.Sprintf("INSERT INTO t VALUES (1, %s)", digits)); c != 2 {
		t.Fatalf("decimal INSERT cost = %d, want 2 (the compress attempt)", c)
	}
	mustExec(t, db, "CREATE TABLE control (id i32 PRIMARY KEY, d numeric)")
	mustExec(t, db, "INSERT INTO control VALUES (1, 7)")
	comp := mustCost(t, db, "SELECT * FROM t")
	control := mustCost(t, db, "SELECT * FROM control")
	if comp != control+2 {
		t.Fatalf("decimal scan: comp %d, control %d (want +2 decompress slabs)", comp, control)
	}
}

func TestCompressedCostUntouchedColumnsChargeNoSlabs(t *testing.T) {
	t.Parallel()
	// The touched set (cost.md §3 "The touched set"): a query that never references the
	// compressed column pays no decompress slabs; an aggregate's ARGUMENT is a touch.
	db := compressedTables(t)
	if a, b := mustCost(t, db, "SELECT id FROM comp"), mustCost(t, db, "SELECT id FROM control"); a != b {
		t.Fatalf("SELECT id: comp %d, want control %d", a, b)
	}
	if a, b := mustCost(t, db, "SELECT count(*) FROM comp"), mustCost(t, db, "SELECT count(*) FROM control"); a != b {
		t.Fatalf("count(*): comp %d, want control %d", a, b)
	}
	a := mustCost(t, db, "SELECT min(body) FROM comp")
	b := mustCost(t, db, "SELECT min(body) FROM control")
	if a != b+slabs600 {
		t.Fatalf("min(body): comp %d, want control %d + %d", a, b, slabs600)
	}
}

func TestCompressedCostCorrelatedOuterReferenceIsATouch(t *testing.T) {
	t.Parallel()
	// A nested subquery's outer reference back into the scanned relation counts as a touch
	// (collected depth-aware — cost.md §3). `probe` holds the one value that matches both
	// tables' row 2, so the two queries emit identical row counts and differ only in the
	// outer table's storage — isolating the slabs600 the outer reference charges.
	db := compressedTables(t)
	mustExec(t, db, "CREATE TABLE probe (id i32 PRIMARY KEY, body text)")
	mustExec(t, db, "INSERT INTO probe VALUES (1, 'small')")
	a := mustCost(t, db, "SELECT id FROM comp WHERE EXISTS (SELECT 1 FROM probe WHERE probe.body = comp.body)")
	b := mustCost(t, db, "SELECT id FROM control WHERE EXISTS (SELECT 1 FROM probe WHERE probe.body = control.body)")
	if a != b+slabs600 {
		t.Fatalf("correlated touch: comp %d, want control %d + %d", a, b, slabs600)
	}
}
