package jed

// Chain sharing on rewrite (spec/design/large-values.md §15). A rewritten record keeps an unchanged
// large value's stored form: an UPDATE that does not assign the column, and the other rows of a leaf
// a commit rewrites, write the value from its stored bytes and keep its overflow chain — no chain
// read, no decompression, no chain write. The plan, the bytes other than page numbers, and the cost
// are those of a fresh plan, so cost is the same whether a row was read from disk or is still
// resident. These tests pin the physical sharing (corrupt chain payloads survive a rewrite
// untouched; no overflow page is written) and the cost identity. Mirrors
// impl/rust/tests/chain_sharing.rs and impl/ts/tests/chain_sharing.test.ts. Uses fillerText from
// fileformat_golden_test.go and corruptOverflowPayloads from lazy_large_values_test.go.

import (
	"bytes"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

// chainSeed creates t with row 1 external-plain (a 3-page chain of incompressible filler), row 2
// external-compressed (a 1-page chain), row 3 inline-compressed, rows 4..12 small — at ps=256
// (RECORD_MAX 114) — in one statement, so every chain is written exactly once.
func chainSeed(t *testing.T, db dbHandle) {
	t.Helper()
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, body text, n i32)")
	plain := fillerText(600)
	extc := fillerText(200) + strings.Repeat("y", 200)
	inlc := strings.Repeat("x", 600)
	small := make([]string, 0, 9)
	for g := 4; g <= 12; g++ {
		small = append(small, fmt.Sprintf("(%d, 'small %d', %d)", g, g, g))
	}
	mustExec(t, db, fmt.Sprintf("INSERT INTO t VALUES (1, '%s', 1), (2, '%s', 2), (3, '%s', 3), %s",
		plain, extc, inlc, strings.Join(small, ", ")))
}

// chainCreate creates a seeded file database at path and closes it.
func chainCreate(t *testing.T, path string) {
	t.Helper()
	db, err := create(path, databaseOptions{PageSize: lazyPageSize, noSync: true})
	if err != nil {
		t.Fatal(err)
	}
	chainSeed(t, db)
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
}

// renderedRows renders every cell of sql's result.
func renderedRows(t *testing.T, db dbHandle, sql string) [][]string {
	t.Helper()
	var out [][]string
	for _, r := range queryRows(t, db, sql) {
		row := make([]string, len(r))
		for i, v := range r {
			row[i] = v.Render()
		}
		out = append(out, row)
	}
	return out
}

// overflowPages returns every overflow page in the file, by page index.
func overflowPages(t *testing.T, path string) map[int][]byte {
	t.Helper()
	b, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	out := map[int][]byte{}
	for i := 2; (i+1)*lazyPageSize <= len(b); i++ {
		if b[i*lazyPageSize] == pageOverflow {
			out[i] = bytes.Clone(b[i*lazyPageSize : (i+1)*lazyPageSize])
		}
	}
	return out
}

// Neither an UPDATE that does not assign the spilled column nor a commit that rewrites the leaf
// around it reads the chain: with every chain payload corrupt, both succeed, and the corruption is
// still there afterwards (the chain was kept, not rewritten).
func TestChainSharingRewritesNeverReadAKeptChain(t *testing.T) {
	SetVerifyLivePages(true)
	t.Cleanup(func() { SetVerifyLivePages(false) })
	path := filepath.Join(t.TempDir(), "chain_share_corrupt.jed")
	chainCreate(t, path)
	corruptOverflowPayloads(t, path)
	db, err := openWithOptions(path, OpenOptions{SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	mustExec(t, db, "UPDATE t SET n = n + 100")
	mustExec(t, db, "INSERT INTO t VALUES (13, 'new', 13)")
	mustExec(t, db, "UPDATE t SET id = 0 WHERE id = 2")
	if got := renderedRows(t, db, "SELECT sum(n) FROM t"); got[0][0] != "1291" {
		t.Fatalf("sum(n) = %s, want 1291", got[0][0])
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	db, err = openWithOptions(path, OpenOptions{SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	for _, id := range []int{0, 1} {
		if got := fkErr(t, db, fmt.Sprintf("SELECT body FROM t WHERE id = %d", id)); got != "XX001" {
			t.Fatalf("id %d: the kept chain should still be the corrupt one, got %s", id, got)
		}
	}
	if got := renderedRows(t, db, "SELECT body FROM t WHERE id = 3"); got[0][0] != strings.Repeat("x", 600) {
		t.Fatal("the inline-compressed value should decode from the leaf")
	}
}

// A rewrite keeps a chain even when the statement reads the column (in its WHERE), as long as it does
// not assign it: no overflow page is written or changed, and the values, a compaction of the result,
// and the costs all come back exactly.
func TestChainSharingReadButUnassignedValuesKeepTheirChains(t *testing.T) {
	SetVerifyLivePages(true)
	t.Cleanup(func() { SetVerifyLivePages(false) })
	path := filepath.Join(t.TempDir(), "chain_share_read.jed")
	chainCreate(t, path)
	before := overflowPages(t, path)
	if len(before) != 4 {
		t.Fatalf("got %d overflow pages, want 4", len(before))
	}
	db, err := openWithOptions(path, OpenOptions{SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	mustExec(t, db, "UPDATE t SET n = -n WHERE length(body) > 3")
	mustExec(t, db, "UPDATE t SET id = id + 100 WHERE id <= 2")
	want := renderedRows(t, db, "SELECT id, body, n FROM t ORDER BY id")
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(overflowPages(t, path), before) {
		t.Fatal("a chain page was written")
	}

	host, err := OpenDatabaseWithOptions(path, OpenOptions{SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	defer host.Close()
	queries := []string{"SELECT * FROM t", "SELECT body FROM t WHERE id = 101"}
	costs := func() []int64 {
		s := host.Session(SessionOptions{})
		defer s.Close()
		if got := renderedRows(t, s, "SELECT id, body, n FROM t ORDER BY id"); !reflect.DeepEqual(got, want) {
			t.Fatal("rows changed")
		}
		out := make([]int64, len(queries))
		for i, q := range queries {
			out[i] = mustCost(t, s, q)
		}
		return out
	}
	first := costs()
	if err := host.Compact("main"); err != nil {
		t.Fatal(err)
	}
	if after := costs(); !reflect.DeepEqual(first, after) {
		t.Fatalf("compaction changed costs: %v -> %v", first, after)
	}
}

// Cost stays logical (cost.md §3): the same statements charge the same whether the rows were read
// back from disk (deferred values, kept chains) or are still resident in memory (planned from their
// values).
func TestChainSharingRewriteCostsDoNotDependOnResidency(t *testing.T) {
	t.Parallel()
	path := filepath.Join(t.TempDir(), "chain_share_cost.jed")
	mem := newInMemoryWithPageSize(lazyPageSize, NewEnvironment()).Session(SessionOptions{})
	chainSeed(t, mem)
	chainCreate(t, path)
	paged, err := openWithOptions(path, OpenOptions{SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	defer paged.Close()
	for _, sql := range []string{
		"UPDATE t SET n = n + 1",
		"UPDATE t SET n = 7 WHERE body > 'a'",
		"UPDATE t SET id = id + 50 WHERE id IN (1, 2, 3)",
		"UPDATE t SET body = body || 'z' WHERE id = 51",
		"UPDATE t SET body = 'short' WHERE id = 52",
		"INSERT INTO t VALUES (14, 'more', 14)",
		"SELECT * FROM t",
		"SELECT body FROM t WHERE id = 53",
	} {
		if m, p := mustCost(t, mem, sql), mustCost(t, paged, sql); m != p {
			t.Fatalf("%q: in-memory cost %d != paged cost %d", sql, m, p)
		}
	}
	if !reflect.DeepEqual(renderedRows(t, mem, "SELECT * FROM t ORDER BY id"), renderedRows(t, paged, "SELECT * FROM t ORDER BY id")) {
		t.Fatal("in-memory and paged rows differ")
	}
}

// The statements of TestChainSharingKeptChainsStayExclusive, in three sessions over the file (each
// phase starts from a fresh open, so its rows are read from disk). FILLER is replaced with a
// 900-byte incompressible value.
var chainOwnershipPhases = [][]string{
	{
		"BEGIN",
		"SAVEPOINT a",
		"UPDATE t SET n = n + 1000",
		"UPDATE t SET id = id + 100 WHERE id <= 3",
		"ROLLBACK TO SAVEPOINT a",
		"UPDATE t SET n = n + 1 WHERE id <= 3",
		"SAVEPOINT b",
		"UPDATE t SET id = id + 200 WHERE id = 1",
		"RELEASE SAVEPOINT b",
		"SAVEPOINT c",
		"UPDATE t SET id = id + 400 WHERE id = 2",
		"ROLLBACK TO SAVEPOINT c",
		"COMMIT",
	},
	{
		"INSERT INTO t VALUES (2, 'ignored', 0) ON CONFLICT (id) DO UPDATE SET id = 300",
		"INSERT INTO t SELECT id + 1000, body, n FROM t WHERE id IN (201, 300)",
	},
	{
		"DELETE FROM t WHERE id IN (201, 300)",
		"INSERT INTO t VALUES (13, 'FILLER', 13)",
		"UPDATE t SET n = n + 1",
	},
}

// A chain belongs to exactly one record per snapshot (large-values.md §15.3), across savepoint
// rollbacks, primary-key re-keys (UPDATE and upsert DO UPDATE), and row copies: rows rolled back to a
// savepoint keep their original chains, a re-keyed row takes its chain along, and a copy gets its
// own. After each phase the live checker and the image checker (live page count, chain exclusivity)
// pass, and the rows equal an in-memory replay.
func TestChainSharingKeptChainsStayExclusive(t *testing.T) {
	SetVerifyLivePages(true)
	t.Cleanup(func() { SetVerifyLivePages(false) })
	path := filepath.Join(t.TempDir(), "chain_share_ownership.jed")
	mem := newInMemoryWithPageSize(lazyPageSize, NewEnvironment()).Session(SessionOptions{})
	chainSeed(t, mem)
	chainCreate(t, path)
	originals := renderedRows(t, mem, "SELECT body FROM t WHERE id IN (1, 2) ORDER BY id")
	const q = "SELECT id, body, n FROM t ORDER BY id"
	for i, phase := range chainOwnershipPhases {
		host, err := OpenDatabaseWithOptions(path, OpenOptions{SkipFsync: true})
		if err != nil {
			t.Fatal(err)
		}
		s := host.Session(SessionOptions{})
		for _, sql := range phase {
			sql = strings.ReplaceAll(sql, "FILLER", fillerText(900))
			mustExec(t, mem, sql)
			mustExec(t, s, sql)
		}
		if got, want := renderedRows(t, s, q), renderedRows(t, mem, q); !reflect.DeepEqual(got, want) {
			t.Fatalf("phase %d: rows differ from the in-memory replay", i)
		}
		if err := simCheckLive(host); err != nil {
			t.Fatalf("phase %d: live check: %v", i, err)
		}
		txid := host.Txid()
		s.Close()
		if err := host.Close(); err != nil {
			t.Fatal(err)
		}
		image, err := os.ReadFile(path)
		if err != nil {
			t.Fatal(err)
		}
		if _, _, _, err := simIntegrity(image, txid, nil); err != nil {
			t.Fatalf("phase %d: image check: %v", i, err)
		}
	}
	db, err := openWithOptions(path, OpenOptions{SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	if got := renderedRows(t, db, "SELECT body FROM t WHERE id IN (1201, 1300) ORDER BY id"); !reflect.DeepEqual(got, originals) {
		t.Fatal("the copies should keep their own chains")
	}
}
