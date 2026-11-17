package jed

import (
	"encoding/binary"
	"fmt"
	"math/rand"
	"os"
	"path/filepath"
	"testing"
)

// TestLeafEditModes runs the Phase A write workloads of spec/design/leaf-write-cost.md under each
// JED_LEAF_EDIT mode and checks that every mode leaves the same rows and the same file bytes. With
// JED_LEAF_EDIT_STATS set it also logs how many array slots each mode moved per edit.
func TestLeafEditModes(t *testing.T) {
	saved := leafEditMode
	defer func() { leafEditMode = saved }()
	var want map[string][]byte
	for _, mode := range []int{leafEditTail, leafEditShort, leafEditGap} {
		leafEditMode = mode
		got := map[string][]byte{}
		for _, w := range leafEditWorkloads {
			got[w.name] = runLeafEditWorkload(t, w)
			if leafEditCount {
				s := leafEditStats
				t.Logf("mode %d %-14s edits %7d moved %10d (%.1f/edit) closes %6d close-moved %9d",
					mode, w.name, s.edits, s.moved, float64(s.moved)/float64(max(s.edits, 1)), s.closes, s.closeMoved)
			}
		}
		if want == nil {
			want = got
			continue
		}
		for name, bytes := range got {
			if string(bytes) != string(want[name]) {
				t.Fatalf("mode %d: workload %s wrote different bytes", mode, name)
			}
		}
	}
}

type leafEditWorkload struct {
	name  string
	setup []string
	// txn returns the statements of transaction k.
	txn func(k int) []string
	// commit says whether the transactions commit (the file's bytes then record them) or roll back.
	commit bool
}

var leafEditWorkloads = []leafEditWorkload{
	{
		name: "cascade",
		setup: []string{
			"CREATE TABLE p (id bigint PRIMARY KEY)",
			"CREATE TABLE c (id bigint PRIMARY KEY, parent_id bigint REFERENCES p ON DELETE CASCADE)",
			"INSERT INTO p SELECT g FROM generate_series(CAST(1 AS bigint), CAST(10000 AS bigint)) AS g",
			"INSERT INTO c SELECT g, g % 10000 + 1 FROM generate_series(CAST(1 AS bigint), CAST(100000 AS bigint)) AS g",
			"CREATE INDEX c_parent ON c (parent_id)",
		},
		txn: func(k int) []string {
			lo := 1 + (k*7919)%9900
			return []string{fmt.Sprintf("DELETE FROM p WHERE id BETWEEN %d AND %d", lo, lo+99)}
		},
		commit: true,
	},
	{
		name: "insert",
		setup: []string{
			"CREATE TABLE orders (id bigint PRIMARY KEY, customer_id integer, amount integer, note text)",
			"INSERT INTO orders SELECT g, g * 7919 % 1000 + 1, g * 31 % 100000, 'note-' || g FROM generate_series(1, 10000) AS g",
			"CREATE INDEX orders_customer ON orders (customer_id)",
		},
		txn: func(k int) []string {
			r := rand.New(rand.NewSource(int64(k)))
			out := make([]string, 0, 1000)
			for i := 0; i < 1000; i++ {
				id := 20000001 + k*1000 + i
				out = append(out, fmt.Sprintf("INSERT INTO orders VALUES (%d, %d, %d, 'n%d')", id, 1+r.Intn(1000), r.Intn(100000), i))
			}
			return out
		},
		commit: true,
	},
	{
		name: "delete_range",
		setup: []string{
			"CREATE TABLE d (id bigint PRIMARY KEY, v bigint, note text)",
			"INSERT INTO d SELECT x, x * 7, 'note-' || x FROM generate_series(1, 100000) AS g(x)",
		},
		txn: func(k int) []string {
			lo := 1 + k*20000
			return []string{fmt.Sprintf("DELETE FROM d WHERE id BETWEEN %d AND %d", lo, lo+9999)}
		},
		commit: true,
	},
	{
		name: "churn",
		setup: []string{
			"CREATE TABLE d (id bigint PRIMARY KEY, v bigint, note text)",
			"INSERT INTO d SELECT x, x * 7, 'note-' || x FROM generate_series(1, 100000) AS g(x)",
		},
		txn: func(k int) []string {
			out := make([]string, 0, 1000)
			for i := 1; i <= 1000; i++ {
				id := k*1000 + i
				out = append(out, fmt.Sprintf("UPDATE d SET id = id + 1000000 WHERE id = %d", id))
			}
			return out
		},
		commit: true,
	},
}

func runLeafEditWorkload(t *testing.T, w leafEditWorkload) []byte {
	t.Helper()
	path := filepath.Join(t.TempDir(), w.name+".jed")
	db, err := Create(CreateOptions{Path: path, SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	sess := mustSession(db, SessionOptions{})
	for _, sql := range w.setup {
		mustExec(t, sess, sql)
	}
	leafEditStats.edits, leafEditStats.moved, leafEditStats.closes, leafEditStats.closeMoved = 0, 0, 0, 0
	for k := 0; k < 3; k++ {
		mustExec(t, sess, "BEGIN")
		for _, sql := range w.txn(k) {
			mustExec(t, sess, sql)
		}
		if w.commit {
			mustExec(t, sess, "COMMIT")
		} else {
			mustExec(t, sess, "ROLLBACK")
		}
	}
	sess.Close()
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	b, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	return b
}

// TestLeafShapes logs, for the benchmark tables of spec/design/leaf-write-cost.md, how many leaves
// each tree has, their rows and how full they are (JED_LEAF_SHAPES=1).
func TestLeafShapes(t *testing.T) {
	if os.Getenv("JED_LEAF_SHAPES") == "" {
		t.Skip("set JED_LEAF_SHAPES=1")
	}
	setups := map[string][]string{
		"wide": {
			"CREATE TABLE wide (id bigint PRIMARY KEY, n integer, body text)",
			"INSERT INTO wide SELECT g, 0, CASE WHEN g % 2 = 0 THEN (SELECT string_agg(md5(CAST(g * 1000 + i AS text)), '' ORDER BY i) FROM generate_series(1, 300) i) ELSE repeat(md5(CAST(g AS text)), 300) END FROM generate_series(1, 2000) g",
		},
		"c":      leafEditWorkloads[0].setup,
		"orders": leafEditWorkloads[1].setup,
		"d":      leafEditWorkloads[2].setup,
	}
	for table, setup := range setups {
		path := filepath.Join(t.TempDir(), table+".jed")
		db, err := Create(CreateOptions{Path: path, SkipFsync: true})
		if err != nil {
			t.Fatal(err)
		}
		sess := mustSession(db, SessionOptions{})
		for _, sql := range setup {
			mustExec(t, sess, sql)
		}
		sess.Close()
		if err := db.Close(); err != nil {
			t.Fatal(err)
		}
		file, err := os.ReadFile(path)
		if err != nil {
			t.Fatal(err)
		}
		img, err := openImage(file)
		if err != nil {
			t.Fatal(err)
		}
		snap := img.committed
		logTree := func(name string, store *tableStore) {
			src := store.leafSrc()
			leaves, rows, bytes := 0, 0, 0
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
				leaves++
				rows += n.keyLen()
				bytes += n.payload(store.cap, store.shape)
			}
			if root := store.rows.rootNode(); root != nil {
				walk(root)
			}
			t.Logf("%-22s leaves %5d  rows/leaf %7.1f  fill %5.1f%% of C=%d", name, leaves, float64(rows)/float64(max(leaves, 1)),
				100*float64(bytes)/float64(max(leaves, 1))/float64(store.cap.c), store.cap.c)
		}
		logTree(table, snap.store(table))
		for name, ix := range snap.indexStores {
			logTree(fmt.Sprintf("%s/%v", table, name), ix)
		}
		img.Close()
	}
}

// TestLeafWidenGrowth logs the live page count of tables loaded in key order before and after a
// committed UPDATE that widens one value in about every leaf, under each #5 option of
// spec/design/leaf-write-cost.md (JED_LEAF_SHAPES=1).
func TestLeafWidenGrowth(t *testing.T) {
	if os.Getenv("JED_LEAF_SHAPES") == "" {
		t.Skip("set JED_LEAF_SHAPES=1")
	}
	savedSlack, savedPFOR, savedRuns, savedBlocks, savedExc := leafSlack, leafPFOR, leafRuns, leafBlocks, leafBlocksExc
	defer func() {
		leafSlack, leafPFOR, leafRuns, leafBlocks, leafBlocksExc = savedSlack, savedPFOR, savedRuns, savedBlocks, savedExc
	}()
	cases := []struct {
		name   string
		setup  []string
		widen  []string
		widen2 []string
	}{
		{"wide", []string{
			"CREATE TABLE wide (id bigint PRIMARY KEY, n integer, body text)",
			"INSERT INTO wide SELECT g, 0, repeat(md5(CAST(g AS text)), 300) FROM generate_series(1, 2000) g",
		}, []string{"UPDATE wide SET n = n + 1 WHERE id % 100 = 0"}, []string{"UPDATE wide SET n = n + 1 WHERE id % 100 = 50"}},
		{"d", []string{
			"CREATE TABLE d (id bigint PRIMARY KEY, v bigint, note text)",
			"INSERT INTO d SELECT x, x * 7, 'note-' || x FROM generate_series(1, 100000) AS g(x)",
		}, []string{"UPDATE d SET v = v + 100000000 WHERE id % 500 = 0"}, []string{"UPDATE d SET v = v + 100000000 WHERE id % 500 = 250"}},
		{"c", []string{
			"CREATE TABLE c (id bigint PRIMARY KEY, parent_id bigint)",
			"INSERT INTO c SELECT g, g % 10000 + 1 FROM generate_series(CAST(1 AS bigint), CAST(100000 AS bigint)) AS g",
		}, []string{"UPDATE c SET parent_id = 900000 + id WHERE id % 2000 = 0"}, []string{"UPDATE c SET parent_id = 900000 + id WHERE id % 2000 = 1000"}},
	}
	modes := []struct {
		name   string
		slack  int
		pfor   int
		runs   int
		blocks int
		bexc   bool
	}{
		{"v53", 0, 0, 0, 0, false},
		{"slack32", 32, 0, 0, 0, false},
		{"slack16", 16, 0, 0, 0, false},
		{"v54", 0, pforMaxK, 0, 0, false},
		{"runs-s", 0, 0, leafRunsSearch, 0, false},
		{"runs-r", 0, 0, leafRunsRank, 0, false},
		{"blk64", 0, 0, 0, 64, false},
		{"blk64x", 0, 0, 0, 64, true},
		{"blk32x", 0, 0, 0, 32, true},
	}
	for _, c := range cases {
		for _, m := range modes {
			leafSlack, leafPFOR, leafRuns, leafBlocks, leafBlocksExc = m.slack, m.pfor, m.runs, m.blocks, m.bexc
			path := filepath.Join(t.TempDir(), c.name+".jed")
			db, err := Create(CreateOptions{Path: path, SkipFsync: true})
			if err != nil {
				t.Fatal(err)
			}
			sess := mustSession(db, SessionOptions{})
			for _, sql := range c.setup {
				mustExec(t, sess, sql)
			}
			live := func() uint32 { return fileLivePages(t, path) }
			l0 := live()
			for _, sql := range c.widen {
				mustExec(t, sess, sql)
			}
			l1 := live()
			for _, sql := range c.widen2 {
				mustExec(t, sess, sql)
			}
			l2 := live()
			t.Logf("%-5s %-8s live pages: loaded %6d, after one widening per leaf %6d (%+.1f%%), after a second %6d (%+.1f%%)",
				c.name, m.name, l0, l1, 100*float64(int(l1)-int(l0))/float64(l0), l2, 100*float64(int(l2)-int(l0))/float64(l0))
			sess.Close()
			if err := db.Close(); err != nil {
				t.Fatal(err)
			}
		}
	}
}

// fileLivePages reads the live page count (meta offset 56) of the newest meta slot of the file at path.
func fileLivePages(t *testing.T, path string) uint32 {
	t.Helper()
	b, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	ps := int(binary.BigEndian.Uint32(b[8:12]))
	var best uint64
	var live uint32
	for slot := 0; slot < 2; slot++ {
		m := b[slot*ps:]
		if txid := binary.BigEndian.Uint64(m[12:20]); txid >= best {
			best, live = txid, binary.BigEndian.Uint32(m[56:60])
		}
	}
	return live
}
