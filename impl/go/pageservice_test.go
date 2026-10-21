//go:build unix

package jed

// Page-service internals the real-process corpus cannot observe (spec/pageservice/README.md §3.2, §4,
// §5.2): the owner's invalidation answer and the endpoint path choice.

import (
	"errors"
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

// testDelta builds a valid CommitDelta over a committed state of committedPages pages: dirty pages at
// the given indices (the first is the catalog root), its meta, and its manifest chain.
func testDelta(t *testing.T, ps, committedPages uint32, indices []uint32) (psCommitBegin, []psIndexedPage) {
	t.Helper()
	dirty := make([]dirtyPage, len(indices))
	pageCount := committedPages
	for i, index := range indices {
		pageType := pageLeaf
		if i == 0 {
			pageType = pageCatalog
		}
		dirty[i] = dirtyPage{index: index, bytes: makePage(int(ps), pageType, 1, 0, []byte{byte(i), byte(i >> 8), 7})}
		pageCount = max(pageCount, index+1)
	}
	left := max(0, len(dirty)-manifestInlineCapacity(int(ps), plainCap(ps)))
	overflow := (left + manifestOverflowCapacity(int(ps)) - 1) / manifestOverflowCapacity(int(ps))
	ids := make([]uint32, overflow)
	for i := range ids {
		ids[i] = pageCount
		pageCount++
	}
	meta, chain := commitManifest(plainCap(ps), ps, 6, plainRef(indices[0]), pageCount, pageRef{}, uint32(len(indices)), manifestEntriesOf(dirty), ids)
	cb := psCommitBegin{txid: 6, meta: meta}
	for _, pg := range chain {
		cb.manifestPages = append(cb.manifestPages, psIndexedPage{index: pg.index, page: pg.bytes})
	}
	body := make([]psIndexedPage, len(dirty))
	for i, pg := range dirty {
		body[i] = psIndexedPage{index: pg.index, page: pg.bytes}
	}
	return cb, body
}

// applyDelta runs the owner's three checks over a delta, returning the first failure.
func applyDelta(base psDeltaBase, cb psCommitBegin, body []psIndexedPage) error {
	d, err := beginDelta(base, cb)
	if err != nil {
		return err
	}
	for _, pg := range body {
		if !base.legal(pg.index, d.m.pageCount) {
			return errDelta("accepted an illegal page")
		}
		if err := d.checkPage(pg); err != nil {
			return err
		}
	}
	return d.complete()
}

func cloneDelta(cb psCommitBegin, body []psIndexedPage) (psCommitBegin, []psIndexedPage) {
	out := psCommitBegin{txid: cb.txid, meta: append([]byte(nil), cb.meta...)}
	for _, pg := range cb.manifestPages {
		out.manifestPages = append(out.manifestPages, psIndexedPage{index: pg.index, page: append([]byte(nil), pg.page...)})
	}
	pages := make([]psIndexedPage, len(body))
	for i, pg := range body {
		pages[i] = psIndexedPage{index: pg.index, page: append([]byte(nil), pg.page...)}
	}
	return out, pages
}

// TestPageServiceDeltaValidation checks the owner's CommitDelta validation (README.md §3.5): a valid
// delta passes, and each structural fault is refused with 08P01 before the page it guards is written.
func TestPageServiceDeltaValidation(t *testing.T) {
	const ps = 512
	base := psDeltaBase{committedTxid: 5, pageSize: ps, pageCount: 10, granted: map[uint32]bool{3: true}}
	// 40 dirty pages overflow the 512-byte meta's 37 inline entries into one manifest page.
	indices := []uint32{12, 3}
	for i := uint32(13); len(indices) < 40; i++ {
		indices = append(indices, i)
	}
	cb, body := testDelta(t, ps, 10, indices)
	if len(cb.manifestPages) != 1 {
		t.Fatalf("expected one manifest page, got %d", len(cb.manifestPages))
	}
	if err := applyDelta(base, cb, body); err != nil {
		t.Fatalf("a valid delta was refused: %v", err)
	}
	cases := []struct {
		name   string
		mutate func(*psDeltaBase, *psCommitBegin, *[]psIndexedPage)
	}{
		{"txid does not follow", func(_ *psDeltaBase, cb *psCommitBegin, _ *[]psIndexedPage) { cb.txid = 7 }},
		{"committed txid moved", func(b *psDeltaBase, _ *psCommitBegin, _ *[]psIndexedPage) { b.committedTxid = 4 }},
		{"no reuse grant", func(b *psDeltaBase, _ *psCommitBegin, _ *[]psIndexedPage) { b.granted = nil }},
		{"page count shrinks", func(b *psDeltaBase, _ *psCommitBegin, _ *[]psIndexedPage) { b.pageCount = 80 }},
		{"page size differs", func(b *psDeltaBase, _ *psCommitBegin, _ *[]psIndexedPage) { b.pageSize = 1024 }},
		{"manifest page missing", func(_ *psDeltaBase, cb *psCommitBegin, _ *[]psIndexedPage) { cb.manifestPages = nil }},
		{"body page missing", func(_ *psDeltaBase, _ *psCommitBegin, body *[]psIndexedPage) { *body = (*body)[1:] }},
		{"body page twice", func(_ *psDeltaBase, _ *psCommitBegin, body *[]psIndexedPage) { *body = append(*body, (*body)[0]) }},
		{"unlisted page", func(_ *psDeltaBase, _ *psCommitBegin, body *[]psIndexedPage) {
			(*body)[0].index = 99
		}},
		{"page swapped", func(_ *psDeltaBase, _ *psCommitBegin, body *[]psIndexedPage) {
			(*body)[0].page, (*body)[1].page = (*body)[1].page, (*body)[0].page
		}},
	}
	for _, c := range cases {
		b := base
		mc, mb := cloneDelta(cb, body)
		c.mutate(&b, &mc, &mb)
		if err := applyDelta(b, mc, mb); err == nil || psErrCode(err) != "08P01" {
			t.Errorf("%s: got %v, want 08P01", c.name, err)
		}
	}
	// Every single-byte change to the meta, the manifest, or a body page is caught: each is covered by
	// a CRC the owner checks before writing anything it guards.
	rng := uint64(0x9E3779B97F4A7C15)
	next := func(n int) int {
		rng ^= rng << 13
		rng ^= rng >> 7
		rng ^= rng << 17
		return int(rng % uint64(n))
	}
	for trial := 0; trial < 3000; trial++ {
		mc, mb := cloneDelta(cb, body)
		var target []byte
		switch next(3) {
		case 0:
			target = mc.meta
		case 1:
			target = mc.manifestPages[0].page
		default:
			target = mb[next(len(mb))].page
		}
		target[next(len(target))] ^= byte(1 + next(255))
		if err := applyDelta(base, mc, mb); err == nil {
			t.Fatalf("trial %d: a corrupted delta passed validation", trial)
		}
	}
}

func psErrCode(err error) string {
	var engine *EngineError
	if errors.As(err, &engine) {
		return engine.Code()
	}
	return ""
}

func TestPageServiceResolutionWindow(t *testing.T) {
	o := &pageServiceOwner{metaTxid: 12}
	for txid := uint64(10); txid <= 12; txid++ {
		meta := make([]byte, 64)
		meta[0] = byte(txid)
		o.recordResolution(txid, meta)
	}
	digest := func(txid uint64) uint64 {
		meta := make([]byte, 64)
		meta[0] = byte(txid)
		return commitCRC64(0, meta)
	}
	cases := []struct {
		name   string
		txid   uint64
		digest uint64
		want   uint8
	}{
		{"committed", 11, digest(11), psCommitStatusCommitted},
		{"another commit took the txid", 11, digest(99), psCommitStatusNotCommitted},
		{"above the committed txid", 13, digest(13), psCommitStatusNotCommitted},
		{"below the window", 9, digest(9), psCommitStatusUnknown},
	}
	for _, c := range cases {
		if got := o.resolve(c.txid, c.digest); got != c.want {
			t.Errorf("%s: got %d, want %d", c.name, got, c.want)
		}
	}
	for txid := uint64(13); txid < 13+psResolutionWindowCommits; txid++ {
		o.recordResolution(txid, make([]byte, 64))
	}
	if len(o.resolution) != psResolutionWindowCommits || o.resolution[0].txid != 13 {
		t.Errorf("the resolution window must keep the newest %d commits", psResolutionWindowCommits)
	}
}
