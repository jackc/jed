package jed

import (
	"bytes"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// lazyWorkload runs a write workload against a file-backed database, committing between statements
// so the leaves it touches are clean packed pages, and returns a transcript of every query result
// and the file's bytes after a final reopen.
func lazyWorkload(t *testing.T, lazy bool) (string, []byte) {
	t.Helper()
	if setLeafLazy(lazy) != lazy {
		t.Skip("lazy leaves are off while a symbol-table switch is on")
	}
	path := filepath.Join(t.TempDir(), "lazy.jed")
	db, err := CreateDatabase(CreateOptions{Path: path, PageSize: 4096, SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	sess := db.Session(SessionOptions{})
	var out strings.Builder
	query := func(sql string) {
		t.Helper()
		fmt.Fprintf(&out, "%s\n", sql)
		for _, row := range renderRows(queryRows(t, sess, sql)) {
			fmt.Fprintf(&out, "  %s\n", strings.Join(row, "|"))
		}
	}
	exec := func(format string, args ...any) {
		t.Helper()
		mustExec(t, sess, fmt.Sprintf(format, args...))
	}
	reopen := func() {
		t.Helper()
		sess.Close()
		if err := db.Close(); err != nil {
			t.Fatal(err)
		}
		if db, err = OpenDatabaseWithOptions(path, OpenOptions{SkipFsync: true}); err != nil {
			t.Fatal(err)
		}
		sess = db.Session(SessionOptions{})
	}
	// Integers, a text with few distinct values, a text with unique values, an integer array, a
	// boolean, NULLs.
	exec(`CREATE TABLE w (id bigint PRIMARY KEY, a bigint, b integer, c text, d text, e bigint[],
		f bigint, g integer, h text, k bigint, m boolean)`)
	exec("CREATE INDEX w_c ON w (c)")
	exec(`INSERT INTO w SELECT x, x * 7, CASE WHEN x %% 11 = 0 THEN NULL ELSE x %% 100 END,
		'status-' || (x %% 5), 'row-' || x, ARRAY[x, x + 1, x %% 9], 1000000 + x, x %% 3,
		CASE WHEN x %% 4 = 0 THEN NULL ELSE 'h' || (x %% 2) END, x * 1000, x %% 2 = 0
		FROM generate_series(1, 4000) AS g(x)`)
	reopen()

	// Single-row statements, each its own commit: one column of one row, of every kind.
	for i := 0; i < 120; i++ {
		id := 1 + (i*331)%4000
		switch i % 8 {
		case 0:
			exec("UPDATE w SET a = a + 1 WHERE id = %d", id)
		case 1:
			exec("UPDATE w SET c = 'changed-' || (id %% 3) WHERE id = %d", id)
		case 2:
			exec("UPDATE w SET d = 'second-' || id WHERE id = %d", id)
		case 3:
			exec("UPDATE w SET e = ARRAY[id, id * 1000000007] WHERE id = %d", id)
		case 4:
			exec("UPDATE w SET h = NULL, b = NULL WHERE id = %d", id)
		case 5:
			exec("UPDATE w SET g = g WHERE id = %d", id) // every column unchanged
		case 6:
			exec("UPDATE w SET k = 9000000000000 + id WHERE id = %d", id)
		case 7:
			exec("UPDATE w SET e = NULL, c = NULL WHERE id = %d", id)
		}
	}
	query("SELECT count(*), sum(a), sum(b), sum(k), count(c), count(h) FROM w")

	// Several rows of one leaf in one statement, then the same leaves again.
	exec("UPDATE w SET a = a - 3 WHERE id BETWEEN 2000 AND 2100")
	exec("UPDATE w SET c = 'bulk' WHERE id BETWEEN 2050 AND 2150")
	exec("UPDATE w SET f = f WHERE id BETWEEN 2000 AND 2400")
	exec("UPDATE w SET d = d || '!' WHERE id %% 50 = 0")
	exec("UPDATE w SET m = NOT m WHERE id %% 97 = 0")
	exec("UPDATE w SET m = NULL WHERE id = 2222")

	// Deletes and inserts on leaves patched above.
	for i := 0; i < 40; i++ {
		id := 1 + (i*331)%4000
		exec("DELETE FROM w WHERE id = %d", id+1)
		exec("INSERT INTO w VALUES (%d, 1, NULL, 'status-1', 'new', '{}', 5, 6, 'h0', 7, true)", 100000+id)
	}
	exec("INSERT INTO w VALUES (2, 14, 2, 'status-2', 'back', ARRAY[2], 5, 6, NULL, 7, false) ON CONFLICT (id) DO NOTHING")
	query("SELECT id, a, c, d, e, h FROM w WHERE id BETWEEN 2040 AND 2060 ORDER BY id")

	// Rows that grow: enough to split a leaf, then past the inline limit (a compressed or external
	// value, which the patch encoder leaves to the full one).
	for i := 0; i < 30; i++ {
		exec("UPDATE w SET d = repeat('x', 300) || id WHERE id = %d", 3000+i*7)
	}
	exec("UPDATE w SET d = repeat('long value ', 900) WHERE id = 3500")
	exec("UPDATE w SET a = 0 WHERE id = 3501")
	exec("UPDATE w SET d = 'short again' WHERE id = 3500")
	exec("UPDATE w SET a = 1 WHERE id = 3502")
	for i := 0; i < 12; i++ {
		id := 3600 + i*29
		exec("UPDATE w SET h = repeat('large-%d ', 1200) WHERE id = %d", i, id)
		exec("UPDATE w SET a = a + 1 WHERE id = %d", id+1)
		exec("UPDATE w SET k = k + 1 WHERE id = %d", id)
		exec("UPDATE w SET c = 'beside' WHERE id = %d", id-1)
		if i%3 == 0 {
			exec("DELETE FROM w WHERE id = %d", id+2)
			exec("UPDATE w SET h = 'small' WHERE id = %d", id)
		}
	}

	// A transaction of several statements: later statements find the lazy leaves of earlier ones, and
	// reads inside it see them.
	exec("BEGIN")
	exec("UPDATE w SET a = -1 WHERE id = 1200")
	exec("UPDATE w SET c = 'txn' WHERE id = 1201")
	query("SELECT id, a, b, c, d, e, f, g, h, k FROM w WHERE id BETWEEN 1195 AND 1205 ORDER BY id")
	exec("DELETE FROM w WHERE id = 1202")
	exec("UPDATE w SET k = 1 WHERE id = 1203")
	query("SELECT count(*), sum(a), sum(k) FROM w WHERE id BETWEEN 1100 AND 1300")
	query("SELECT id, c FROM w WHERE c = 'txn'")
	exec("COMMIT")

	// One leaf edited by many statements of a transaction: columns measured one after another as they
	// change, a row replaced twice, a savepoint (a later statement then copies the lazy leaf), a
	// delete and an insert among the replacements, and a partial rollback.
	exec("BEGIN")
	exec("UPDATE w SET b = 77 WHERE id = 1400")
	exec("UPDATE w SET k = 5 WHERE id = 1401")
	exec("UPDATE w SET e = ARRAY[5, 6] WHERE id = 1402")
	exec("UPDATE w SET c = 'zz' WHERE id = 1402")
	exec("UPDATE w SET c = 'status-1', a = 3 WHERE id = 1402")
	exec("UPDATE w SET k = NULL WHERE id BETWEEN 1380 AND 1420")
	exec("UPDATE w SET k = 12 WHERE id = 1390")
	exec("SAVEPOINT s1")
	exec("UPDATE w SET d = 'sp' WHERE id = 1403")
	query("SELECT id, a, b, c, d, e, k FROM w WHERE id BETWEEN 1398 AND 1408 ORDER BY id")
	exec("DELETE FROM w WHERE id = 1404")
	exec("UPDATE w SET a = 9 WHERE id = 1405")
	exec("INSERT INTO w VALUES (1404, 1, 2, 'status-9', 'again', ARRAY[4], 5, 6, 'h1', 7, NULL)")
	exec("UPDATE w SET a = 10, c = 'status-9' WHERE id = 1407")
	exec("SAVEPOINT s2")
	exec("UPDATE w SET g = 5 WHERE id = 1408")
	exec("ROLLBACK TO SAVEPOINT s2")
	exec("UPDATE w SET h = 'after' WHERE id = 1406")
	query("SELECT id, a, b, c, d, e, k FROM w WHERE id BETWEEN 1398 AND 1408 ORDER BY id")
	exec("COMMIT")
	exec("BEGIN")
	exec("UPDATE w SET f = 1 WHERE id = 1500")
	exec("SAVEPOINT s1")
	exec("UPDATE w SET f = 2, d = 'x' WHERE id = 1501")
	exec("UPDATE w SET f = 3 WHERE id = 1500")
	exec("ROLLBACK TO SAVEPOINT s1")
	exec("UPDATE w SET g = 9 WHERE id = 1502")
	exec("COMMIT")

	// The same, rolled back.
	exec("BEGIN")
	exec("UPDATE w SET a = -2, d = 'gone' WHERE id BETWEEN 700 AND 760")
	exec("DELETE FROM w WHERE id BETWEEN 720 AND 740")
	exec("UPDATE w SET e = ARRAY[1, 2, 3] WHERE id = 750")
	query("SELECT count(*), sum(a) FROM w WHERE id BETWEEN 690 AND 770")
	exec("ROLLBACK")
	query("SELECT count(*), sum(a) FROM w WHERE id BETWEEN 690 AND 770")
	reopen()

	// Range deletes that merge leaves, some of them patched above.
	exec("DELETE FROM w WHERE id BETWEEN 500 AND 1150")
	exec("DELETE FROM w WHERE id %% 3 = 0 AND id < 3000")
	exec("UPDATE w SET b = b + 1 WHERE id %% 7 = 0")
	exec("UPDATE w SET a = id")
	for i := 0; i < 20; i++ {
		exec("UPDATE w SET g = %d WHERE id = %d", i, 1300+i*13)
	}
	query("SELECT count(*), sum(a), sum(b), sum(g) FROM w")

	reopen()
	query("SELECT * FROM w ORDER BY id")
	sess.Close()
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	file, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	checkPackedSizes(t, file, "w")
	return out.String(), file
}

// checkPackedSizes compares, for every leaf of a committed table, the size facts read from the page
// (measured in full, and as the page shows them) with a measurement of the decoded rows.
func checkPackedSizes(t *testing.T, file []byte, table string) {
	t.Helper()
	db, err := openImage(file)
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	store := db.committed.store(table)
	shape, src := store.shape, store.leafSrc()
	leaves := 0
	var walk func(n *pnode)
	walk = func(n *pnode) {
		if !n.isLeaf() {
			for _, c := range n.children {
				child, err := resolveChild(c, src)
				if err != nil {
					t.Fatal(err)
				}
				walk(child)
			}
			return
		}
		if n.packed == nil {
			return
		}
		leaves++
		keys, rows, _, err := n.decodedParts()
		if err != nil {
			t.Fatal(err)
		}
		want := newLeafSizes(shape, store.cap, keys, rows, nil, false).saving(shape, false)
		if got := newLeafSizesPacked(shape, store.cap, n.packed).saving(shape, false); got != want {
			t.Fatalf("a leaf measured from its page saves %d bytes, from its rows %d", got, want)
		}
		shown := 0
		for _, fc := range shape.forCols {
			shown += n.packed.regionSaving(fc.col)
		}
		for _, c := range shape.itemCols {
			shown += n.packed.regionSaving(c)
		}
		if shown != want {
			t.Fatalf("a leaf's page shows %d bytes saved, its rows measure %d", shown, want)
		}
	}
	if root := store.rows.rootNode(); root != nil {
		walk(root)
	}
	if leaves == 0 {
		t.Fatal("no packed leaf was checked")
	}
}

// A dirty leaf that keeps its page's rows undecoded, sizes itself from the page and is encoded as a
// patch of the page must produce the rows, query results and file bytes of the eager paths. The
// workload runs with the size-fact and encoder cross-checks on.
func TestLazyLeavesMatchEagerLeaves(t *testing.T) {
	was := leafLazy.Load()
	defer setLeafLazy(was)
	defer leafSizesCheck.Store(leafSizesCheck.Load())
	defer leafPatchCheck.Store(leafPatchCheck.Load())
	leafSizesCheck.Store(true)
	leafPatchCheck.Store(true)
	SetVerifyLivePages(true)
	defer SetVerifyLivePages(false)

	patched, spans, full := lazyStats.patched.Load(), lazyStats.spans.Load(), lazyStats.full.Load()
	lazyRows, lazyFile := lazyWorkload(t, true)
	patched, spans, full = lazyStats.patched.Load()-patched, lazyStats.spans.Load()-spans, lazyStats.full.Load()-full
	if patched == 0 || spans == 0 || full == 0 {
		t.Fatalf("the workload patched %d lazy leaves, rebuilt %d from spans and encoded %d in full; it must do all three", patched, spans, full)
	}
	t.Logf("lazy leaves at commit: %d patched, %d rebuilt from spans, %d encoded in full", patched, spans, full)

	eagerRows, eagerFile := lazyWorkload(t, false)
	if lazyRows != eagerRows {
		t.Fatal("query results differ between lazy and eager leaves")
	}
	if !bytes.Equal(lazyFile, eagerFile) {
		t.Fatalf("file bytes differ between lazy and eager leaves (%d and %d bytes)", len(lazyFile), len(eagerFile))
	}
}

// rootWorkload writes tables that stay a single root leaf, some rows holding values stored out of
// line or compressed. A persisted root leaf is re-faulted for lazy edits only when that cannot change
// which overflow chains the next commit writes (storage.go demoteCleanLeaves).
func rootWorkload(t *testing.T, lazy bool) (string, []byte) {
	t.Helper()
	if setLeafLazy(lazy) != lazy {
		t.Skip("lazy leaves are off while a symbol-table switch is on")
	}
	path := filepath.Join(t.TempDir(), "root.jed")
	db, err := CreateDatabase(CreateOptions{Path: path, PageSize: 512, SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	sess := db.Session(SessionOptions{})
	var out strings.Builder
	query := func(sql string) {
		t.Helper()
		fmt.Fprintf(&out, "%s\n", sql)
		for _, row := range renderRows(queryRows(t, sess, sql)) {
			fmt.Fprintf(&out, "  %s\n", strings.Join(row, "|"))
		}
	}
	exec := func(format string, args ...any) {
		t.Helper()
		mustExec(t, sess, fmt.Sprintf(format, args...))
	}
	reopen := func() {
		t.Helper()
		sess.Close()
		if err := db.Close(); err != nil {
			t.Fatal(err)
		}
		if db, err = OpenDatabaseWithOptions(path, OpenOptions{SkipFsync: true}); err != nil {
			t.Fatal(err)
		}
		sess = db.Session(SessionOptions{})
	}
	// Incompressible text: stored in an overflow chain at this page size.
	big := func(seed, n int) string {
		x := seed
		b := make([]byte, n)
		for i := range b {
			x = (x*1103515245 + 12345) & 0x7fffffff
			b[i] = byte(97 + x%26)
		}
		return string(b)
	}
	exec("CREATE TABLE r (id integer PRIMARY KEY, n bigint, body text, tag text)")
	exec("CREATE TABLE s (id integer PRIMARY KEY, n bigint, t text)")
	// A root leaf whose resident rows include an external value, then writes beside it.
	exec("INSERT INTO r VALUES (1, 10, '%s', 'a')", big(1, 1500))
	exec("INSERT INTO r VALUES (2, 20, 'small', 'a')")
	exec("UPDATE r SET n = n + 1 WHERE id = 2")
	exec("UPDATE r SET tag = 'b' WHERE id = 1")
	exec("INSERT INTO r VALUES (3, 30, repeat('compressible ', 60), 'c')")
	exec("UPDATE r SET n = n + 1 WHERE id = 3")
	exec("UPDATE r SET n = 0")
	query("SELECT id, n, length(body), tag FROM r ORDER BY id")
	// A root leaf of inline rows only: re-faulted and edited lazily between commits.
	for i := 1; i <= 12; i++ {
		exec("INSERT INTO s VALUES (%d, %d, 'v%d')", i, i*100, i%3)
	}
	for i := 1; i <= 12; i++ {
		exec("UPDATE s SET n = n + %d WHERE id = %d", i, (i*5)%12+1)
	}
	exec("UPDATE s SET t = 'changed' WHERE id = 4")
	exec("DELETE FROM s WHERE id = 5")
	exec("INSERT INTO s VALUES (40, 1, NULL)")
	exec("BEGIN")
	exec("UPDATE s SET n = -1 WHERE id = 6")
	exec("UPDATE s SET t = NULL WHERE id = 7")
	query("SELECT * FROM s ORDER BY id")
	exec("COMMIT")
	exec("UPDATE s SET n = n * 2 WHERE id = 6")
	reopen()
	// The same after a reopen: the root is a clean page with chains.
	exec("UPDATE r SET n = 5 WHERE id = 2")
	exec("UPDATE r SET n = 6 WHERE id = 1")
	exec("UPDATE r SET body = '%s' WHERE id = 2", big(2, 900))
	exec("UPDATE r SET tag = 'd' WHERE id = 3")
	exec("DELETE FROM r WHERE id = 1")
	exec("INSERT INTO r VALUES (4, 40, '%s', 'e')", big(3, 1200))
	exec("UPDATE r SET n = n + 1")
	exec("UPDATE s SET n = n + 1")
	query("SELECT id, n, length(body), tag FROM r ORDER BY id")
	reopen()
	query("SELECT id, n, body, tag FROM r ORDER BY id")
	query("SELECT * FROM s ORDER BY id")
	sess.Close()
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	file, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	return out.String(), file
}

// Single-leaf tables — the root leaf, which has no parent to demote it — must produce the same query
// results and file bytes with lazy leaves on and off, whether their rows hold resident large values,
// chain references or inline values only. Mirrors the TS test "lazy root leaves match eager root
// leaves" (impl/ts/tests/leaf_lazy.test.ts).
func TestLazyRootLeavesMatchEagerRootLeaves(t *testing.T) {
	was := leafLazy.Load()
	defer setLeafLazy(was)
	defer leafSizesCheck.Store(leafSizesCheck.Load())
	defer leafPatchCheck.Store(leafPatchCheck.Load())
	leafSizesCheck.Store(true)
	leafPatchCheck.Store(true)
	SetVerifyLivePages(true)
	defer SetVerifyLivePages(false)

	lazyRows, lazyFile := rootWorkload(t, true)
	eagerRows, eagerFile := rootWorkload(t, false)
	if lazyRows != eagerRows {
		t.Fatal("query results differ between lazy and eager root leaves")
	}
	if !bytes.Equal(lazyFile, eagerFile) {
		t.Fatalf("file bytes differ between lazy and eager root leaves (%d and %d bytes)", len(lazyFile), len(eagerFile))
	}
}

// Lazy leaves cannot be turned on while a symbol-table switch is on: the patch encoder would drop
// the symbol-table regions.
func TestLazyLeavesStayOffWithSymbolTables(t *testing.T) {
	was := leafLazy.Load()
	defer setLeafLazy(was)
	if got, want := setLeafLazy(true), !leafSym && !leafSymCol; got != want || leafLazy.Load() != want {
		t.Fatalf("setLeafLazy(true) = %v, leafLazy = %v; want %v", got, leafLazy.Load(), want)
	}
	if setLeafLazy(false) || leafLazy.Load() {
		t.Fatal("setLeafLazy(false) left lazy leaves on")
	}
}
