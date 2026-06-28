package jed

// Structure-aware corruption fuzzing of the on-disk format (.scratch/testing-ideas.md §1 item 4).
// The per-page CRC + the meta-slot validation (format_version 7) exist precisely to detect at-rest
// corruption — so this attacks them: it flips byte runs, zeroes / 0xFF-fills pages, truncates at page
// boundaries, and garbles individual meta-header fields, then asserts the engine FAILS CLOSED — never
// panics, never loops, never serves wrong rows. This is the Go carve-out's intrinsic oracle
// (testing-ideas.md §3): no differential needed. It generalizes checksum_test.go (one byte per page,
// XX001-or-inert) to structured, randomized corruption and adds the fuzz explorer.
//
// The sound oracle. For the byte-level corruptions here, a successful open+scan can only return ONE
// of two valid snapshots — the committed one (the corruption was inert: a dead page, trailing slack,
// or the inactive meta slot) or, when the ACTIVE meta slot is invalidated, the snapshot the loader
// falls back to (the other slot). Every other corruption of a LIVE page changes CRC-covered bytes and
// is rejected (XX001). So the acceptable outcomes are exactly: a structured *EngineError, OR rows
// equal to one of those two snapshots. A panic, a hang, or a third (silently wrong) row set fails.
//
// Whole-page TRANSPOSITION is deliberately out of scope: the per-page CRC covers a page's own bytes
// but NOT its page index (format.go pageCRC), so swapping two same-shape pages is not a CRC-detectable
// corruption — asserting it fails closed would be unsound. That gap (transposition detection) is a
// separate design question, not a property this oracle can assert.

import (
	"encoding/binary"
	"fmt"
	"os"
	"path/filepath"
	"testing"
)

const corruptScanSQL = "SELECT id, body FROM t ORDER BY id"

// seedCorruptTarget builds a file spanning every body-page kind — catalog, a multi-leaf B-tree
// (interior root), and an overflow chain (row 1's 600-char spilled body) — at page size 256, then
// returns it TRIMMED to its committed high-water (the trailing 1 MiB preallocation slack stripped, so
// corruption lands on live structure, not zero fill). Mirrors checksum_test.go's seed.
func seedCorruptTarget(tb testing.TB) []byte {
	tb.Helper()
	path := filepath.Join(tb.TempDir(), "corrupt_seed.jed")
	db, err := create(path, DatabaseOptions{PageSize: 256})
	if err != nil {
		tb.Fatal(err)
	}
	if _, err := execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, body text)"); err != nil {
		tb.Fatal(err)
	}
	sql := "INSERT INTO t VALUES (1, '" + fillerText(600) + "')"
	for id := 2; id <= 30; id++ {
		sql += fmt.Sprintf(", (%d, 'row%d')", id, id)
	}
	if _, err := execute(db, sql); err != nil {
		tb.Fatal(err)
	}
	if err := db.Close(); err != nil {
		tb.Fatal(err)
	}
	raw, err := os.ReadFile(path)
	if err != nil {
		tb.Fatal(err)
	}
	return trimToHighWater(raw)
}

// trimToHighWater cuts an image to its committed page high-water (max pageCount over the valid meta
// slots) × page size, dropping the trailing preallocation slack. The loader reads only pages below
// the active meta's pageCount, so the trim is loss-free; it just keeps the corruption target small.
func trimToHighWater(image []byte) []byte {
	if len(image) < 36 {
		return image
	}
	ps := int(binary.BigEndian.Uint32(image[8:12]))
	if ps < 36 || len(image) < 2*ps {
		return image
	}
	high := 0
	for slot := 0; slot < 2; slot++ {
		base := slot * ps
		if binary.BigEndian.Uint16(image[base+4:base+6]) != formatVersion {
			continue
		}
		if pc := int(binary.BigEndian.Uint32(image[base+24 : base+28])); pc > high {
			high = pc
		}
	}
	if high == 0 || high*ps > len(image) {
		return image
	}
	return image[:high*ps]
}

// validSnapshots returns the rendered row sets a CLEAN open may legitimately produce after a
// byte-level corruption: the committed snapshot, plus — if it loads cleanly — the snapshot the loader
// falls back to when the active meta slot is rejected. A corruption's successful scan must match one
// of these; anything else is silent data corruption.
func validSnapshots(tb testing.TB, seed []byte) [][][]string {
	tb.Helper()
	baseRows, err := guardedQuery(seed, corruptScanSQL)
	if err != nil {
		tb.Fatalf("the clean seed must scan cleanly: %v", err)
	}
	out := [][][]string{renderRows(baseRows)}

	// Invalidate the ACTIVE (higher-txid) slot's CRC-covered header, forcing the loader to the other
	// slot — its rows (if it loads) are the legitimate fall-back result.
	ps := int(binary.BigEndian.Uint32(seed[8:12]))
	active := 0
	if slotTxid(seed, 1) > slotTxid(seed, 0) {
		active = 1
	}
	fb := append([]byte(nil), seed...)
	fb[active*ps] ^= 0xFF // breaks the active slot's crc32 over [0,32); stored crc at [32,36) unchanged
	if rows, err := guardedQuery(fb, corruptScanSQL); err == nil {
		out = append(out, renderRows(rows))
	}
	return out
}

// assertFailsClosed is the corruption oracle: a corrupted image must either fail closed with a
// structured *EngineError, or scan cleanly to one of the valid snapshots. A panic / hang (a
// non-EngineError) or a scan matching no valid snapshot (silent wrong rows) fails the test.
func assertFailsClosed(tb testing.TB, image []byte, valid [][][]string, what string) {
	tb.Helper()
	rows, err := guardedQuery(image, corruptScanSQL)
	if err != nil {
		if !isEngineError(err) {
			tb.Fatalf("%s: not fail-closed — panic/hang or unstructured error: %v", what, err)
		}
		return // structured fail-closed — good
	}
	got := renderRows(rows)
	for _, ok := range valid {
		if equalRows(got, ok) {
			return
		}
	}
	tb.Fatalf("%s: SILENT corruption — scan succeeded with rows matching no valid snapshot (%d rows)", what, len(got))
}

// --- corruption operators (each returns a fresh corrupted copy of seed) ---

func corruptCopy(seed []byte) []byte { return append([]byte(nil), seed...) }

// flipRun XORs 0xFF across n bytes at off (wrapping length to the image).
func flipRun(seed []byte, off, n int) []byte {
	b := corruptCopy(seed)
	for i := 0; i < n && off+i < len(b); i++ {
		b[off+i] ^= 0xFF
	}
	return b
}

// fillRun sets n bytes at off to val.
func fillRun(seed []byte, off, n int, val byte) []byte {
	b := corruptCopy(seed)
	for i := 0; i < n && off+i < len(b); i++ {
		b[off+i] = val
	}
	return b
}

// truncateAt returns the image cut to length n (clamped to [0, len]).
func truncateAt(seed []byte, n int) []byte {
	if n < 0 {
		n = 0
	}
	if n > len(seed) {
		n = len(seed)
	}
	return append([]byte(nil), seed[:n]...)
}

// fillPage sets page p (page size ps) entirely to val.
func fillPage(seed []byte, p, ps int, val byte) []byte {
	return fillRun(seed, p*ps, ps, val)
}

// metaFieldOffsets are the byte ranges of the meta-header fields (format.go metaPage) within a slot,
// each a juicy corruption target — garbling any of them must be caught (CRC mismatch → fall back or
// XX001), never silently mis-read.
var metaFieldOffsets = []struct {
	name       string
	start, end int
}{
	{"magic", 0, 4},
	{"version", 4, 6},
	{"pagesize", 8, 12},
	{"txid", 12, 20},
	{"root", 20, 24},
	{"pagecount", 24, 28},
	{"crc", 32, 36},
}

// corruptMetaField overwrites one meta-header field of slot with 0xFF bytes.
func corruptMetaField(seed []byte, slot, ps, field int) []byte {
	b := corruptCopy(seed)
	f := metaFieldOffsets[field%len(metaFieldOffsets)]
	for i := f.start; i < f.end; i++ {
		if slot*ps+i < len(b) {
			b[slot*ps+i] = 0xFF
		}
	}
	return b
}

// TestStructuredCorruptionFailsClosed runs a deterministic battery of structured corruptions over a
// rich seed and asserts every one fails closed (or is inert) — never a panic, hang, or silent wrong
// rows. The structured ops (truncation, whole-page zero/fill, per-field meta garble, multi-byte flip
// runs) are exactly what the single-byte checksum_test.go does not reach.
func TestStructuredCorruptionFailsClosed(t *testing.T) {
	seed := seedCorruptTarget(t)
	ps := int(binary.BigEndian.Uint32(seed[8:12]))
	pages := len(seed) / ps
	if pages < 6 {
		t.Fatalf("seed should span several pages, got %d", pages)
	}
	valid := validSnapshots(t, seed)

	// (a) Truncate at every page boundary and a few sub-page points.
	for p := 0; p <= pages; p++ {
		assertFailsClosed(t, truncateAt(seed, p*ps), valid, fmt.Sprintf("truncate to page %d", p))
	}
	for _, off := range []int{0, 1, 5, 11, 35, ps / 2, ps - 1, ps + 1} {
		assertFailsClosed(t, truncateAt(seed, off), valid, fmt.Sprintf("truncate to byte %d", off))
	}

	// (b) Zero and (c) 0xFF-fill each whole page.
	for p := 0; p < pages; p++ {
		assertFailsClosed(t, fillPage(seed, p, ps, 0x00), valid, fmt.Sprintf("zero page %d", p))
		assertFailsClosed(t, fillPage(seed, p, ps, 0xFF), valid, fmt.Sprintf("0xFF-fill page %d", p))
	}

	// (d) Garble each meta-header field in each slot.
	for slot := 0; slot < 2; slot++ {
		for field := range metaFieldOffsets {
			assertFailsClosed(t, corruptMetaField(seed, slot, ps, field), valid,
				fmt.Sprintf("corrupt meta slot %d field %s", slot, metaFieldOffsets[field].name))
		}
	}

	// (e) Multi-byte flip runs striding across the whole live region.
	for off := 0; off < len(seed); off += 7 {
		assertFailsClosed(t, flipRun(seed, off, 3), valid, fmt.Sprintf("flip 3 bytes at %d", off))
	}
}

// FuzzCorruptFile is the corruption explorer (testing-ideas.md §3: Go explores, the intrinsic oracle
// judges). The fuzz input selects an operator and its parameters; every corrupted image must fail
// closed or stay inert. The f.Add seeds run inside `go test` (covered by `rake ci`); `-fuzz` runs the
// campaign (rake fuzz:corruption).
func FuzzCorruptFile(f *testing.F) {
	seed := seedCorruptTarget(f)
	ps := int(binary.BigEndian.Uint32(seed[8:12]))
	valid := validSnapshots(f, seed)

	f.Add(uint8(0), uint32(0), uint16(4), uint8(0xFF))    // flip run at the magic
	f.Add(uint8(1), uint32(700), uint16(16), uint8(0x00)) // zero run mid-tree
	f.Add(uint8(2), uint32(0), uint16(0), uint8(0xFF))    // 0xFF-fill a page
	f.Add(uint8(3), uint32(300), uint16(0), uint8(0))     // truncate
	f.Add(uint8(4), uint32(0), uint16(0), uint8(0))       // meta-field garble
	f.Fuzz(func(t *testing.T, op uint8, off uint32, length uint16, val uint8) {
		if len(seed) == 0 {
			return
		}
		pos := int(off) % len(seed)
		n := int(length)%512 + 1
		pages := len(seed) / ps
		var image []byte
		var what string
		switch op % 6 {
		case 0:
			image, what = flipRun(seed, pos, n), "flip"
		case 1:
			image, what = fillRun(seed, pos, n, 0x00), "zero"
		case 2:
			image, what = fillRun(seed, pos, n, val), "fill"
		case 3:
			image, what = truncateAt(seed, pos), "truncate"
		case 4:
			image, what = corruptMetaField(seed, int(off)%2, ps, int(length)), "meta-field"
		default:
			image, what = fillPage(seed, int(off)%pages, ps, val), "page-fill"
		}
		assertFailsClosed(t, image, valid, what)
	})
}
