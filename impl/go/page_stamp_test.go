package jed

import (
	"encoding/binary"
	"os"
	"path/filepath"
	"testing"
)

// The window check is the subtle part of the stamp (manifest-crc32.md §2.1): it compares low-16-bit
// stamps against a txid window that may straddle a 2^16 boundary.
func TestStampWindowContains(t *testing.T) {
	saved := pageTxidStamp
	pageTxidStamp = true
	defer func() { pageTxidStamp = saved }()

	cases := []struct {
		name          string
		floor, newest uint64
		stamp         uint16
		want          bool
	}{
		{"synchronous on: own txid", 41, 42, 42, true},
		{"synchronous on: previous commit", 41, 42, 41, false},
		{"synchronous on: a later commit", 41, 42, 43, false},
		{"batch: first pending commit", 100, 105, 101, true},
		{"batch: newest", 100, 105, 105, true},
		{"batch: the floor itself", 100, 105, 100, false},
		{"batch: past newest", 100, 105, 106, false},
		{"wraps 2^16: before the boundary", 65534, 65538, 0xFFFF, true},
		{"wraps 2^16: after the boundary", 65534, 65538, 1, true},
		{"wraps 2^16: the floor", 65534, 65538, 0xFFFE, false},
		{"wraps 2^16: just past newest", 65534, 65538, 3, false},
		{"stale by exactly 2^16 aliases (the CRC is the backstop)", 1<<16 + 41, 1<<16 + 42, 42, true},
		{"empty window admits", 42, 42, 7, true},
		{"window of 2^16 admits", 0, 1 << 16, 7, true},
		{"widest exact window", 0, 1<<16 - 1, 0, false},
	}
	for _, c := range cases {
		if got := (stampWindow{floor: c.floor, newest: c.newest}).contains(c.stamp); got != c.want {
			t.Errorf("%s: window (%d, %d], stamp %d: got %v, want %v", c.name, c.floor, c.newest, c.stamp, got, c.want)
		}
	}
}

func TestStampOffIsV52(t *testing.T) {
	saved := pageTxidStamp
	pageTxidStamp = false
	defer func() { pageTxidStamp = saved }()
	if stampOf(12345) != 0 {
		t.Fatal("an unstamped page must keep the v52 reserved zero")
	}
	if !(stampWindow{floor: 1, newest: 2}).contains(999) {
		t.Fatal("with the stamp off the window must admit every page")
	}
}

// TestStampIsAnIndependentDetector forges the one case a narrow manifest alone cannot catch: a listed
// page that carries an older commit's stamp but whose content CRC-32 the manifest entry agrees with.
// The stamp must reject it; with the stamp check disabled (the zero window) the same forged file must
// validate, which proves the rejection is the stamp's and not the CRC's.
func TestStampIsAnIndependentDetector(t *testing.T) {
	savedStamp, savedNarrow := pageTxidStamp, manifestCRC32
	pageTxidStamp, manifestCRC32 = true, true
	defer func() { pageTxidStamp, manifestCRC32 = savedStamp, savedNarrow }()

	const ps = 4096
	path := filepath.Join(t.TempDir(), "stamp.jed")
	db, err := create(path, databaseOptions{PageSize: ps, noSync: true})
	if err != nil {
		t.Fatal(err)
	}
	for _, sql := range []string{
		"CREATE TABLE t (k i32 PRIMARY KEY, v i32)",
		"INSERT INTO t VALUES (1, 10)",
		"INSERT INTO t VALUES (2, 20)",
	} {
		if _, err := execute(db, sql); err != nil {
			t.Fatal(err)
		}
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	image, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}

	// The newest slot is the candidate; the other holds its floor.
	m0, ok0 := parseMeta(image[:ps])
	m1, ok1 := parseMeta(image[ps : 2*ps])
	if !ok0 || !ok1 {
		t.Fatal("both meta slots should parse")
	}
	newest, floor, raw := m0, m1, append([]byte(nil), image[:ps]...)
	if m1.txid > m0.txid {
		newest, floor, raw = m1, m0, append([]byte(nil), image[ps:2*ps]...)
	}
	if newest.dirtyCount == 0 || newest.manifestPages != 0 {
		t.Fatalf("want an inline-only manifest with entries; dirty=%d overflow=%d", newest.dirtyCount, newest.manifestPages)
	}
	win := stampWindow{floor: floor.txid, newest: newest.txid}
	pcap := capOf(ps, false)
	lo, _ := manifestInlineAreaOf(raw, pcap)
	entry := raw[lo : lo+8]
	id := binary.BigEndian.Uint32(entry)

	pages := map[uint32][]byte{}
	read := func(i uint32) ([]byte, error) {
		if p, ok := pages[i]; ok {
			return p, nil
		}
		return image[int(i)*ps : int(i+1)*ps], nil
	}

	// Unforged, the generation validates under its window.
	if _, _, valid, err := commitManifestDependencies(newest, raw, ps, read, win); err != nil || !valid {
		t.Fatalf("the genuine generation must validate: valid=%v err=%v", valid, err)
	}

	// Forge: the page as an older commit would have stamped it, internally valid, and an entry that
	// agrees with its CRC-32. Only the stamp now says it is not this generation's page.
	forged := append([]byte(nil), image[int(id)*ps:int(id+1)*ps]...)
	binary.BigEndian.PutUint16(forged[2:], uint16(floor.txid))
	binary.BigEndian.PutUint32(forged[12:], pageCRC(forged))
	binary.BigEndian.PutUint32(entry[4:], binary.BigEndian.Uint32(forged[12:16]))
	pages[id] = forged

	if _, bad, valid, err := commitManifestDependencies(newest, raw, ps, read, win); err != nil || valid || bad != id {
		t.Fatalf("a page with a stale stamp must be rejected: valid=%v bad=%d (want %d) err=%v", valid, bad, id, err)
	}
	if _, _, valid, err := commitManifestDependencies(newest, raw, ps, read, stampWindow{}); err != nil || !valid {
		t.Fatalf("without the stamp check the forgery must pass the CRC-32 alone: valid=%v err=%v", valid, err)
	}
}
