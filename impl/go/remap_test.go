package jed

// PROTOTYPE (spec/design/deferred-interior.md §9): the (parent, slot) remap list. Byte layout,
// validation and end-to-end commit behavior -- internal invariants and on-disk bytes, out of the
// conformance corpus's reach (CLAUDE.md §10). These flip the package switch, so none is parallel: Go
// runs every parallel test only after the sequential ones finish.

import (
	"context"
	"encoding/binary"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func withDefer(t *testing.T, on bool) {
	t.Helper()
	saved := deferInterior
	deferInterior = on
	t.Cleanup(func() { deferInterior = saved })
}

// §9.2's table, and the inline manifest capacity beside the region for both entry widths.
func TestRemapCapacityTable(t *testing.T) {
	withDefer(t, true)
	savedNarrow := manifestCRC32
	t.Cleanup(func() { manifestCRC32 = savedNarrow })
	for _, c := range []struct {
		ps                     int
		encrypted              bool
		cap, wide, narrow, v52 int
	}{
		{256, false, 6, 11, 16, 16},
		{4096, false, 134, 224, 336, 336},
		{8192, false, 270, 452, 678, 677},
		{1024, true, 11, 50, 75, 74},
		{8192, true, 103, 448, 672, 671},
	} {
		pcap := capOf(uint32(c.ps), c.encrypted)
		if got := remapCapFor(c.ps, c.encrypted); got != c.cap {
			t.Errorf("%d/%v: cap %d, want %d", c.ps, c.encrypted, got, c.cap)
		}
		manifestCRC32 = false
		if got := manifestInlineCapacity(c.ps, pcap); got != c.wide {
			t.Errorf("%d/%v: 12-byte inline %d, want %d", c.ps, c.encrypted, got, c.wide)
		}
		manifestCRC32 = true
		if got := manifestInlineCapacity(c.ps, pcap); got != c.narrow {
			t.Errorf("%d/%v: 8-byte inline %d, want %d", c.ps, c.encrypted, got, c.narrow)
		}
		// v53's point (manifest-crc32.md §3): never below v52's capacity.
		if c.narrow < c.v52 {
			t.Errorf("%d/%v: v53 inline %d below v52's %d", c.ps, c.encrypted, c.narrow, c.v52)
		}
	}
}

func remapMeta(ps int, entries map[remapKey]uint32) []byte {
	m := make([]byte, ps)
	r := newRemapState()
	for k, p := range entries {
		r.set(k.parent, k.slot, pageRef{page: p})
	}
	encodeRemap(m, r)
	return m
}

func TestRemapRoundTrip(t *testing.T) {
	withDefer(t, true)
	want := map[remapKey]uint32{{40, 0}: 90, {40, 3}: 91, {7, 2}: 92, {41, 0}: 93}
	m := remapMeta(512, want)
	if got := binary.BigEndian.Uint32(m[60:]); got != 4 {
		t.Fatalf("remap_entries %d, want 4", got)
	}
	// Sorted by (parent, slot) from the start of the region.
	at := 512 - remapReserveFor(512, false)
	if p, s := binary.BigEndian.Uint32(m[at:]), binary.BigEndian.Uint16(m[at+4:]); p != 7 || s != 2 {
		t.Fatalf("first entry (%d, %d), want (7, 2)", p, s)
	}
	r, ok := decodeRemap(m, 4, 1000)
	if !ok || r.len() != 4 {
		t.Fatalf("decode: ok=%v len=%d", ok, r.len())
	}
	for k, p := range want {
		if to, ok := r.lookup(k.parent, int(k.slot)); !ok || to.page != p {
			t.Errorf("(%d, %d): %v %v, want %d", k.parent, k.slot, to, ok, p)
		}
	}
	if !r.keys(40) || r.keys(8) {
		t.Error("keys: 40 keys entries, 8 does not")
	}
	// A slot with no entries is a zero region and remap_entries 0.
	empty := remapMeta(512, nil)
	if r, ok := decodeRemap(empty, 4, 1000); !ok || r.len() != 0 || !zeroBytes(empty) {
		t.Fatalf("empty list: ok=%v len=%d", ok, r.len())
	}
}

// Every §9.3 rule makes the slot structurally invalid.
func TestRemapValidationRejects(t *testing.T) {
	withDefer(t, true)
	const ps = 512
	size := remapEntrySize(false)
	at := ps - remapReserveFor(ps, false)
	put := func(m []byte, i int, parent uint32, slot uint16, page uint32) {
		o := at + i*size
		binary.BigEndian.PutUint32(m[o:], parent)
		binary.BigEndian.PutUint16(m[o+4:], slot)
		binary.BigEndian.PutUint32(m[o+6:], page)
	}
	build := func(n uint32, f func(m []byte)) []byte {
		m := make([]byte, ps)
		binary.BigEndian.PutUint32(m[60:], n)
		f(m)
		return m
	}
	cases := map[string][]byte{
		"count past cap":          build(uint32(remapCapFor(ps, false)+1), func(m []byte) {}),
		"repeated key":            build(2, func(m []byte) { put(m, 0, 10, 1, 20); put(m, 1, 10, 1, 21) }),
		"descending":              build(2, func(m []byte) { put(m, 0, 10, 2, 20); put(m, 1, 10, 1, 21) }),
		"parent below the body":   build(1, func(m []byte) { put(m, 0, 3, 0, 20) }),
		"parent past page_count":  build(1, func(m []byte) { put(m, 0, 1000, 0, 20) }),
		"page past page_count":    build(1, func(m []byte) { put(m, 0, 10, 0, 1000) }),
		"page is its parent":      build(1, func(m []byte) { put(m, 0, 10, 0, 10) }),
		"two entries one page":    build(2, func(m []byte) { put(m, 0, 10, 0, 20); put(m, 1, 11, 0, 20) }),
		"bytes after the entries": build(1, func(m []byte) { put(m, 0, 10, 0, 20); m[ps-1] = 1 }),
	}
	for name, m := range cases {
		if _, ok := decodeRemap(m, 4, 1000); ok {
			t.Errorf("%s: accepted", name)
		}
	}
	// Without the layout, offset 60 is reserved zero, as through v52.
	deferInterior = false
	if _, ok := decodeRemap(build(1, func(m []byte) { put(m, 0, 10, 0, 20) }), 4, 1000); ok {
		t.Error("v52 layout accepted a nonzero offset 60")
	}
}

// When a pointer is resolved, an entry naming a slot past the parent's item count is XX001 (§9.3).
func TestRemapResolve(t *testing.T) {
	r := newRemapState()
	r.set(9, 1, pageRef{page: 77})
	children := []pageRef{{page: 1}, {page: 2}, {page: 3}}
	if err := r.resolve(9, children); err != nil || children[1].page != 77 || children[0].page != 1 {
		t.Fatalf("resolve: %v %v", err, children)
	}
	r.set(9, 5, pageRef{page: 78})
	if err := r.resolve(9, []pageRef{{page: 1}, {page: 2}, {page: 3}}); err == nil {
		t.Fatal("a slot past item_count must be corruption")
	}
	r.dropParent(9)
	if r.len() != 0 || r.keys(9) {
		t.Fatal("dropParent must forget every entry the parent keys")
	}
}

// A file under single-row commits defers, folds, survives merges and a dropped index, and reopens to
// the same rows -- with every generation passing the integrity check, which reads interiors through
// the list.
func TestRemapEndToEnd(t *testing.T) {
	withDefer(t, true)
	ctx := context.Background()
	path := filepath.Join(t.TempDir(), "remap.jed")
	db, err := CreateDatabase(CreateOptions{Path: path, PageSize: 512, SkipFsync: true, Locking: LockingExclusive})
	if err != nil {
		t.Fatal(err)
	}
	s := db.Session(SessionOptions{})
	exec := func(sql string) {
		t.Helper()
		if _, err := s.Exec(ctx, sql); err != nil {
			t.Fatalf("%s: %v", sql, err)
		}
	}
	newest := func() int {
		t.Helper()
		image, err := os.ReadFile(path)
		if err != nil {
			t.Fatal(err)
		}
		best, n := uint64(0), 0
		for slot := 0; slot < 2; slot++ {
			if m, ok := parseMeta(image[slot*512 : (slot+1)*512]); ok && m.txid >= best {
				best, n = m.txid, m.remap.len()
			}
		}
		return n
	}
	check := func(what string) {
		t.Helper()
		report, err := db.IntegrityCheck(ctx, "main", IntegrityCheckOptions{Level: IntegrityFull, MaxFindings: IntegrityUnlimited, Constraints: true})
		if err != nil || !report.OK() {
			t.Fatalf("%s: integrity %v %+v", what, err, report.Findings)
		}
	}
	exec("CREATE TABLE t (id i32 PRIMARY KEY, v i32, note text)")
	exec("CREATE INDEX t_v ON t (v)")
	exec("INSERT INTO t SELECT g, g, repeat('x', g % 40) FROM generate_series(1, 600) g")

	deferred, folds, prev := 0, 0, 0
	for i := 0; i < 120; i++ {
		exec(fmt.Sprintf("UPDATE t SET v = v + 1000 WHERE id = %d", 1+(i*37)%600))
		n := newest()
		if n > 0 {
			deferred++
		}
		if prev > 0 && n == 0 {
			folds++
		}
		prev = n
	}
	if deferred == 0 || folds == 0 {
		t.Fatalf("120 single-row updates should both defer and fold: deferred %d, folds %d", deferred, folds)
	}
	check("after updates")
	exec("DELETE FROM t WHERE id % 3 <> 0") // merges retire parents
	check("after merges")
	exec("DROP INDEX t_v") // a whole tree of parents retired at once
	for i := 0; i < 30; i++ {
		exec(fmt.Sprintf("UPDATE t SET note = 'y' WHERE id = %d", 3*(1+(i*11)%200)))
	}
	check("after drop")
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	db, err = OpenDatabaseWithOptions(path, OpenOptions{SkipFsync: true, Locking: LockingExclusive})
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	check("after reopen")
	got := query(t, db, "SELECT count(*), sum(v), count(*) FILTER (WHERE note = 'y') FROM t")
	// The expected aggregate, recomputed: the rows id % 3 == 0, each v = id + 1000 * its updates.
	hits := map[int]int{}
	for i := 0; i < 120; i++ {
		hits[1+(i*37)%600]++
	}
	ys := map[int]bool{}
	for i := 0; i < 30; i++ {
		ys[3*(1+(i*11)%200)] = true
	}
	count, sum := int64(0), int64(0)
	for id := 3; id <= 600; id += 3 {
		count++
		sum += int64(id + 1000*hits[id])
	}
	if len(got) != 1 || got[0][0].Int != count || got[0][1].Int != sum || got[0][2].Int != int64(len(ys)) {
		t.Fatalf("rows after reopen: %v, want count %d sum %d y %d", got, count, sum, len(ys))
	}
}

// The Ruby reference writes the cow_deferred_* histories by hand (spec/fileformat/verify.rb,
// DEFERRED_FIXTURES); this core, reading the same layout, must recover the same generation and rows
// and find nothing wrong with them. Without the layout a slot carrying entries is invalid: retained
// recovers the base generation, overwrite and retire (entries in both slots) open nowhere, and fold's
// newest slot -- an empty list, a zero region, its catalog root listed -- is a valid v52 slot, read
// correctly as it stands (deferred-interior.md §9.2: an empty list changes nothing a v52 reader sees).
func TestRemapRubyHistories(t *testing.T) {
	pad := func(id int, c string) string { return fmt.Sprintf("row-%02d-%s", id, strings.Repeat(c, 48)) }
	cases := []struct {
		name    string
		version uint64
		rows    map[int]string
	}{
		{"retained", 2, map[int]string{40: pad(40, "y"), 41: pad(41, "x"), 70: pad(70, "x")}},
		{"overwrite", 3, map[int]string{40: pad(40, "z"), 41: pad(41, "x"), 70: pad(70, "x")}},
		{"fold", 3, map[int]string{40: pad(40, "y"), 41: pad(41, "y"), 70: pad(70, "x")}},
		{"retire", 3, map[int]string{40: pad(40, "y"), 41: pad(41, "x"), 70: pad(70, "y")}},
	}
	ctx := context.Background()
	for _, c := range cases {
		file := specPath(t, "fileformat/fixtures/cow_deferred_"+c.name+".jed")
		t.Run(c.name, func(t *testing.T) {
			withDefer(t, true)
			db, err := openIntegrityCopy(t, file, nil)
			if err != nil {
				t.Fatal(err)
			}
			defer db.Close()
			if v := db.Version(); v != c.version {
				t.Fatalf("recovered version %d, want %d", v, c.version)
			}
			got := query(t, db, "SELECT count(*), sum(id) FROM t")
			if got[0][0].Int != 90 || got[0][1].Int != 4095 {
				t.Fatalf("count/sum %v", got)
			}
			for id, want := range c.rows {
				row := query(t, db, fmt.Sprintf("SELECT pad FROM t WHERE id = %d", id))
				if len(row) != 1 || row[0][0].str() != want {
					t.Errorf("row %d: %v, want %s", id, row, want)
				}
			}
			report, err := db.IntegrityCheck(ctx, "main", IntegrityCheckOptions{Level: IntegrityFull, MaxFindings: IntegrityUnlimited, Constraints: true})
			if err != nil || !report.OK() {
				t.Fatalf("integrity: %v %+v", err, report.Findings)
			}
		})
		t.Run(c.name+"/without layout", func(t *testing.T) {
			withDefer(t, false)
			db, err := openIntegrityCopy(t, file, nil)
			switch c.name {
			case "retained", "fold":
				want := map[string]uint64{"retained": 1, "fold": 3}[c.name]
				if err != nil || db.Version() != want {
					t.Fatalf("want version %d: %v", want, err)
				}
				db.Close()
				return
			}
			if err == nil {
				db.Close()
				t.Fatal("entries in both slots leave no slot a reader without the layout can select")
			}
		})
	}
}

// Cow history (e): the encrypted retained parent. The entry carries the tag of the leaf it names while
// the root's on-disk pointer still binds the old leaf's, so reading the edited row at all means this
// core took the tag from the entry (deferred-interior.md §9.3). Without the layout the newest slot is
// invalid and the base generation in slot 1 opens.
func TestRemapRubyEncryptedHistory(t *testing.T) {
	file := specPath(t, "fileformat/fixtures/encrypted_cow_deferred_retained.jed")
	for _, on := range []bool{true, false} {
		t.Run(fmt.Sprintf("layout %v", on), func(t *testing.T) {
			withDefer(t, on)
			db, err := openIntegrityCopy(t, file, encGoldenKey())
			if err != nil {
				t.Fatal(err)
			}
			defer db.Close()
			want, v := uint64(1), int64(1000)
			if on {
				want, v = 2, 1001
			}
			if db.Version() != want {
				t.Fatalf("recovered version %d, want %d", db.Version(), want)
			}
			got := query(t, db, "SELECT count(*), sum(id), (SELECT v FROM t WHERE id = 100) FROM t")
			if got[0][0].Int != 300 || got[0][1].Int != 45150 || got[0][2].Int != v {
				t.Fatalf("got %v, want 300 rows, sum 45150, v %d", got, v)
			}
			report, err := db.IntegrityCheck(context.Background(), "main", IntegrityCheckOptions{Level: IntegrityFull, MaxFindings: IntegrityUnlimited, Constraints: true})
			if err != nil || !report.OK() {
				t.Fatalf("integrity: %v %+v", err, report.Findings)
			}
		})
	}
}
