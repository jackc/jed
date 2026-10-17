//go:build unix

package jed

// Page-service internals the real-process corpus cannot observe (spec/pageservice/README.md §3.2, §4,
// §5.2): the owner's invalidation answer and the endpoint path choice.

import (
	"os"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"testing"
)

func TestPageServiceInvalidationWindow(t *testing.T) {
	o := &pageServiceOwner{incarnation: id16{1}, windowStart: 10}
	o.published(11, nil, []uint32{9, 4, 7})
	o.published(12, nil, []uint32{4, 20})
	o.published(13, nil, []uint32{3})

	cases := []struct {
		name        string
		incarnation id16
		last, txid  uint64
		invalidate  uint8
		indices     []uint32
	}{
		{"other incarnation", id16{2}, 12, 13, psInvalidateAll, nil},
		{"no cache", id16{1}, 0, 13, psInvalidateAll, nil},
		{"ahead of the owner", id16{1}, 14, 13, psInvalidateAll, nil},
		{"older than the window", id16{1}, 9, 13, psInvalidateAll, nil},
		{"current", id16{1}, 13, 13, psInvalidateExact, nil},
		{"one behind", id16{1}, 12, 13, psInvalidateExact, []uint32{3}},
		{"window start", id16{1}, 10, 13, psInvalidateExact, []uint32{3, 4, 7, 9, 20}},
	}
	for _, c := range cases {
		invalidate, indices := o.invalidation(c.incarnation, c.last, c.txid)
		if invalidate != c.invalidate || !slices.Equal(indices, c.indices) {
			t.Errorf("%s: got (%d, %v), want (%d, %v)", c.name, invalidate, indices, c.invalidate, c.indices)
		}
	}
}

func TestPageServiceInvalidationWindowBounds(t *testing.T) {
	o := &pageServiceOwner{incarnation: id16{1}, windowStart: 0}
	for txid := uint64(1); txid <= psInvalidationWindowCommits+10; txid++ {
		o.published(txid, nil, []uint32{uint32(txid)})
	}
	if len(o.window) != psInvalidationWindowCommits || o.windowStart != 10 {
		t.Fatalf("window holds %d commits from %d", len(o.window), o.windowStart)
	}
	if invalidate, _ := o.invalidation(id16{1}, 9, 266); invalidate != psInvalidateAll {
		t.Errorf("a cache older than the window must be invalidated wholesale")
	}
	if invalidate, indices := o.invalidation(id16{1}, 10, 266); invalidate != psInvalidateExact || len(indices) != psInvalidationWindowCommits {
		t.Errorf("a cache at the window start gets the exact list, got (%d, %d indices)", invalidate, len(indices))
	}

	big := &pageServiceOwner{incarnation: id16{1}, windowStart: 0}
	huge := make([]uint32, psMaxInvalidationEntries+1)
	for i := range huge {
		huge[i] = uint32(i + 2)
	}
	big.published(1, nil, huge)
	if invalidate, _ := big.invalidation(id16{1}, 1, 1); invalidate != psInvalidateExact {
		t.Errorf("an up-to-date cache needs no list")
	}
	if invalidate, _ := big.invalidation(id16{1}, 0, 1); invalidate != psInvalidateAll {
		t.Errorf("no cache is invalidated wholesale")
	}
	big.windowStart = 0
	big.window = []psWindowEntry{{txid: 2, indices: huge}}
	if invalidate, _ := big.invalidation(id16{1}, 1, 2); invalidate != psInvalidateAll {
		t.Errorf("a list longer than %d entries must become invalidate-all", psMaxInvalidationEntries)
	}
}

func TestPageServiceEndpointPath(t *testing.T) {
	uid := currentUID()
	incarnation := id16{0xab, 0xcd, 1, 2, 3, 4, 5, 6}
	short := filepath.Join(t.TempDir(), "d.jed")
	if err := os.Mkdir(short+".lock", 0o755); err != nil {
		t.Fatal(err)
	}
	if got := endpointPath(short, uid, incarnation); !strings.HasPrefix(got, short+".lock/uid-") || !strings.HasSuffix(got, "/endpoint") {
		t.Errorf("a short path binds in the bundle, got %q", got)
	}
	if !privateDir(filepath.Dir(endpointPath(short, uid, incarnation)), uid) {
		t.Errorf("the bundle endpoint directory must be private (0700, owned)")
	}

	runtimeDir := t.TempDir()
	if err := os.Chmod(runtimeDir, 0o700); err != nil {
		t.Fatal(err)
	}
	t.Setenv("XDG_RUNTIME_DIR", runtimeDir)
	t.Setenv("TMPDIR", runtimeDir)
	deep := filepath.Join(t.TempDir(), strings.Repeat("x", 90), "d.jed")
	if err := os.MkdirAll(deep+".lock", 0o755); err != nil {
		t.Fatal(err)
	}
	got := endpointPath(deep, uid, incarnation)
	want := filepath.Join(runtimeDir, "jed-abcd010203040506.sock")
	if len(want) <= psEndpointMaxPathBytes && got != want {
		t.Errorf("a long path falls back to the runtime directory: got %q, want %q", got, want)
	}

	// A bundle endpoint directory that is not private is never served from.
	loose := filepath.Join(t.TempDir(), "e.jed")
	dir := filepath.Join(loose+".lock", "uid-"+strconv.FormatUint(uint64(uid), 10))
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	if got := endpointPath(loose, uid, incarnation); strings.HasPrefix(got, loose) {
		t.Errorf("a non-0700 bundle directory must not be used, got %q", got)
	}
}
