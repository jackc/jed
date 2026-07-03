package jed

// Physical lazy (read-on-touch) materialization of large values
// (spec/design/large-values.md §14, phase 2). A lazily-loaded record holds unfetched
// references for its external/compressed values; the scan layer resolves exactly the
// query's touched columns through the pager, the open-time reachability walk follows
// chains by headers only, and a dirty leaf's re-encode resolves what it must at commit.
// These tests pin all three physically: corrupting every overflow-chain *payload* on disk
// is invisible to open and to untouching queries, and surfaces as XX001 only when the
// spilled column is touched. Mirrors impl/rust/tests/lazy_large_values.rs and
// impl/ts/tests/lazy_large_values.test.ts. Uses fillerText from fileformat_golden_test.go.

import (
	"encoding/binary"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

const lazyPageSize = 256

// lazySeed creates one row per stored form at ps=256 (RECORD_MAX 114, cap 240): id 1
// external-plain (incompressible 600-char filler → a 3-page chain), id 2 external-compressed
// (half filler / half run → the ~212-byte block spills to a 1-page chain), id 3
// inline-compressed (a 600-char run), id 4 plain inline.
func lazySeed(t *testing.T, db dbHandle) {
	t.Helper()
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, body text)")
	plain := fillerText(600)
	extc := fillerText(200) + strings.Repeat("y", 200)
	inlc := strings.Repeat("x", 600)
	mustExec(t, db, fmt.Sprintf("INSERT INTO t VALUES (1, '%s'), (2, '%s'), (3, '%s'), (4, 'tiny')", plain, extc, inlc))
}

// corruptOverflowPayloads overwrites every overflow page's payload (offset 16+, v7) with 0xFF,
// keeping the 16-byte header (page_type / item_count / next_page) intact — so the header-only chain
// walk still works — then recomputes the v7 per-page CRC so the page stays checksum-valid. This
// isolates the decode-time failure (non-UTF-8 / malformed LZ4 block) from the per-page checksum: a
// checksum-inconsistent corruption is instead caught at open (crash_recovery_test.go's checksum case).
func corruptOverflowPayloads(t *testing.T, path string) {
	t.Helper()
	bytes, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	corrupted := 0
	for i := 2; (i+1)*lazyPageSize <= len(bytes); i++ {
		if bytes[i*lazyPageSize] == pageOverflow {
			for j := i*lazyPageSize + pageHeader; j < (i+1)*lazyPageSize; j++ {
				bytes[j] = 0xFF
			}
			page := bytes[i*lazyPageSize : (i+1)*lazyPageSize]
			binary.BigEndian.PutUint32(page[12:16], pageCRC(page))
			corrupted++
		}
	}
	if corrupted < 4 {
		t.Fatalf("expected several overflow pages to corrupt, got %d", corrupted)
	}
	if err := os.WriteFile(path, bytes, 0o644); err != nil {
		t.Fatal(err)
	}
}

// The core phase-2 pin: with every chain payload corrupted, open succeeds (the reachability
// walk reads headers only), untouching queries succeed (no chain read, no decompression), and
// touching the spilled column fails XX001 — read-on-touch, physically.
func TestLazyChainsAreReadOnlyWhenTouched(t *testing.T) {
	path := filepath.Join(t.TempDir(), "lazy_touch.jed")
	db, err := create(path, databaseOptions{PageSize: lazyPageSize})
	if err != nil {
		t.Fatal(err)
	}
	lazySeed(t, db)
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	corruptOverflowPayloads(t, path)

	// Open walks live chains by headers only — corrupt payloads are invisible.
	db, err = open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()

	// Untouching queries never read a chain or decompress a block.
	if rows := queryRows(t, db, "SELECT id FROM t"); len(rows) != 4 {
		t.Fatalf("SELECT id: got %d rows, want 4", len(rows))
	}
	if rows := queryRows(t, db, "SELECT count(*) FROM t"); rows[0][0].Render() != "4" {
		t.Fatalf("count(*): got %s", rows[0][0].Render())
	}

	// Touching the spilled column reads the chain: the corruption surfaces as XX001 —
	// non-UTF-8 for the external-plain text, a malformed LZ4 block for external-compressed.
	for _, id := range []int{1, 2} {
		_, err := db.Execute(fmt.Sprintf("SELECT body FROM t WHERE id = %d", id), nil)
		if err == nil {
			t.Fatalf("id %d: a corrupted chain must fail when touched", id)
		}
		if ee, ok := err.(*EngineError); !ok || ee.Code() != "XX001" {
			t.Fatalf("id %d: want XX001, got %v", id, err)
		}
	}

	// The inline-compressed and plain rows live in the (uncorrupted) leaf: still exact.
	if rows := queryRows(t, db, "SELECT body FROM t WHERE id = 3"); rows[0][0].Render() != strings.Repeat("x", 600) {
		t.Fatal("inline-compressed value should decode from the leaf")
	}
	if rows := queryRows(t, db, "SELECT body FROM t WHERE id = 4"); rows[0][0].Render() != "tiny" {
		t.Fatal("plain inline value should decode from the leaf")
	}
}

// All three lazy forms materialize exactly through the paged path (resolution correctness).
func TestLazyValuesRoundTripExactly(t *testing.T) {
	path := filepath.Join(t.TempDir(), "lazy_roundtrip.jed")
	db, err := create(path, databaseOptions{PageSize: lazyPageSize})
	if err != nil {
		t.Fatal(err)
	}
	lazySeed(t, db)
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	db, err = open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	rows := queryRows(t, db, "SELECT body FROM t")
	want := []string{
		fillerText(600),
		fillerText(200) + strings.Repeat("y", 200),
		strings.Repeat("x", 600),
		"tiny",
	}
	for i, w := range want {
		if rows[i][0].Render() != w {
			t.Fatalf("row %d: lazy value did not round-trip", i+1)
		}
	}
}

// An UPDATE that never touches the spilled column re-stores it without losing it: the
// rewritten row resolves its unfetched values as part of the rewrite, the dirty leaf's other
// rows resolve at commit, and a reopen reads everything back exactly (large-values.md §14 —
// resolve-at-commit; chain sharing stays the deferred follow-on).
func TestLazyUpdateOfOtherColumnsPreservesSpilledValues(t *testing.T) {
	path := filepath.Join(t.TempDir(), "lazy_update.jed")
	big := fillerText(600)
	db, err := create(path, databaseOptions{PageSize: lazyPageSize})
	if err != nil {
		t.Fatal(err)
	}
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, body text, n i32)")
	mustExec(t, db, fmt.Sprintf("INSERT INTO t VALUES (1, '%s', 10), (2, 'small', 20)", big))
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	db, err = open(path)
	if err != nil {
		t.Fatal(err)
	}
	// Dirties the leaf carrying row 1's unfetched body without touching it: row 2's rewrite
	// resolves nothing, row 1 resolves at commit.
	mustExec(t, db, "UPDATE t SET n = 99 WHERE id = 2")
	// Rewrites row 1 itself: the rewrite materializes its body (part of the write work).
	mustExec(t, db, "UPDATE t SET n = 11 WHERE id = 1")
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	db, err = open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	rows := queryRows(t, db, "SELECT body, n FROM t")
	if rows[0][0].Render() != big || rows[0][1].Render() != "11" {
		t.Fatal("row 1's spilled body or updated n did not survive the rewrite")
	}
	if rows[1][0].Render() != "small" || rows[1][1].Render() != "99" {
		t.Fatal("row 2 did not survive the rewrite")
	}
}

// Logical cost is mode-independent (cost.md §3): a demand-paged file and a fully-resident
// in-memory database charge identical costs for the same queries — the unfetched-reference
// units equal the resident disposition plan's by construction.
func TestLazyPagedAndResidentCostsMatch(t *testing.T) {
	path := filepath.Join(t.TempDir(), "lazy_cost.jed")
	mem := newInMemoryWithPageSize(lazyPageSize).Session(SessionOptions{})
	lazySeed(t, mem)
	db, err := create(path, databaseOptions{PageSize: lazyPageSize})
	if err != nil {
		t.Fatal(err)
	}
	lazySeed(t, db)
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	paged, err := open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer paged.Close()
	for _, sql := range []string{
		"SELECT * FROM t",
		"SELECT id FROM t",
		"SELECT count(*) FROM t",
		"SELECT min(body) FROM t",
		"SELECT body FROM t WHERE id = 1",
		"SELECT body FROM t WHERE id = 4",
		"SELECT id FROM t WHERE body = 'tiny'",
	} {
		if m, p := mustCost(t, mem, sql), mustCost(t, paged, sql); m != p {
			t.Fatalf("%q: in-memory cost %d != paged cost %d", sql, m, p)
		}
	}
}
