package jed

// Resealed structural corruption (storage.md §6): the per-page CRC proves a page was written whole,
// not that it is the page its referrer expected or that its directories are self-consistent. A page
// copied from another table, or a CRC-valid page whose in-page lengths and offsets disagree with each
// other, reaches the decoders. They must fail closed with a structured error — never panic, hang, or
// index past a slice. corruption_fuzz_test.go cannot reach these paths: its byte corruptions all
// break a CRC first.
//
// Each case mutates one body page of a golden fixture, recomputes that page's CRC, then opens the
// image and scans and deletes every table. Wrong rows are possible (a whole-page transposition is
// undetectable, see corruption_fuzz_test.go), so the oracle is only "structured error or success".

import (
	"encoding/binary"
	"fmt"
	"os"
	"path/filepath"
	"runtime/debug"
	"sort"
	"strings"
	"testing"
	"time"
)

// resealedMutations is the number of seeded mutations per body page. JED_RESEALED_MUTATIONS raises it
// for an exploration run.
func resealedMutations() int {
	if v := os.Getenv("JED_RESEALED_MUTATIONS"); v != "" {
		var n int
		fmt.Sscan(v, &n)
		if n > 0 {
			return n
		}
	}
	if testing.Short() {
		return 2
	}
	return 6
}

// resealedRNG is splitmix64, so a failing case is reproducible from its seed.
type resealedRNG struct{ s uint64 }

func (r *resealedRNG) next() uint64 {
	r.s += 0x9e3779b97f4a7c15
	z := r.s
	z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9
	z = (z ^ (z >> 27)) * 0x94d049bb133111eb
	return z ^ (z >> 31)
}

func (r *resealedRNG) intn(n int) int { return int(r.next() % uint64(n)) }

// interestingU32 are the directory values most likely to land on an unchecked bound.
func (r *resealedRNG) interestingU32(ps, pages int) uint32 {
	switch r.intn(11) {
	case 9, 10: // an in-range body page: a misdirected but readable pointer
		return uint32(2 + r.intn(pages-2))
	case 0:
		return 0
	case 1:
		return 1
	case 2:
		return uint32(ps)
	case 3:
		return uint32(ps - pageHeader)
	case 4:
		return uint32(pages + r.intn(4))
	case 5:
		return 0xFFFFFFFF
	case 6:
		return 0x7FFFFFFF
	case 7:
		return uint32(r.intn(64))
	default:
		return uint32(r.next())
	}
}

// mutatePage applies one seeded structural mutation to page p of image and reseals its CRC.
func mutatePage(image []byte, ps, p, pages int, r *resealedRNG) string {
	pg := image[p*ps : (p+1)*ps]
	what := ""
	switch r.intn(8) {
	case 0: // one byte anywhere outside the CRC field
		off := r.intn(ps - 4)
		if off >= 12 {
			off += 4
		}
		pg[off] = byte(r.next())
		what = fmt.Sprintf("byte %d", off)
	case 1: // a u32 field in the payload, aligned or not
		off := pageHeader + r.intn(ps-pageHeader-4)
		binary.BigEndian.PutUint32(pg[off:], r.interestingU32(ps, pages))
		what = fmt.Sprintf("u32 at %d", off)
	case 2: // a u16 field in the payload
		off := pageHeader + r.intn(ps-pageHeader-2)
		binary.BigEndian.PutUint16(pg[off:], uint16(r.interestingU32(ps, pages)))
		what = fmt.Sprintf("u16 at %d", off)
	case 3: // the item count
		binary.BigEndian.PutUint32(pg[4:8], r.interestingU32(ps, pages))
		what = "item count"
	case 4: // the next-page link
		binary.BigEndian.PutUint32(pg[8:12], r.interestingU32(ps, pages))
		what = "next page"
	case 5: // the page type
		pg[0] = byte(r.intn(10))
		what = "page type"
	case 6: // a whole other body page, copied with its own (valid) CRC
		src := 2 + r.intn(pages-2)
		copy(pg, image[src*ps:(src+1)*ps])
		return fmt.Sprintf("copy of page %d", src)
	default: // several bytes in the payload
		for k := 0; k < 2+r.intn(6); k++ {
			pg[pageHeader+r.intn(ps-pageHeader)] = byte(r.next())
		}
		what = "byte burst"
	}
	binary.BigEndian.PutUint32(pg[12:16], pageCRC(pg))
	return what
}

// resealedStatements are run against every table. The UPDATE rewrites every row through its
// indexes; the DELETE materializes each leaf through the mutation path and visits every index.
func resealedStatements(tables []*catTable) []string {
	quote := func(n string) string { return `"` + strings.ReplaceAll(n, `"`, `""`) + `"` }
	var out []string
	for _, t := range tables {
		q := quote(t.Name)
		out = append(out, "SELECT * FROM "+q, "SELECT count(*) FROM "+q)
		for _, c := range t.Columns {
			if !c.PrimaryKey {
				out = append(out, "UPDATE "+q+" SET "+quote(c.Name)+" = "+quote(c.Name))
				break
			}
		}
		out = append(out, "DELETE FROM "+q)
	}
	return out
}

// runResealed opens image and runs stmts, converting a panic or hang into a non-engine error.
func runResealed(image []byte, stmts []string) error {
	ch := make(chan error, 1)
	go func() {
		defer func() {
			if r := recover(); r != nil {
				ch <- fmt.Errorf("panic: %v at %s", r, panicSite(debug.Stack()))
			}
		}()
		db, err := openImage(image)
		if err != nil {
			ch <- err
			return
		}
		defer db.Close()
		for _, s := range stmts {
			if _, err := execute(db, s); err != nil && !isEngineError(err) {
				ch <- err
				return
			}
		}
		ch <- nil
	}()
	select {
	case err := <-ch:
		return err
	case <-time.After(10 * time.Second):
		return errScanHang
	}
}

func TestResealedPageCorruptionFailsClosed(t *testing.T) {
	loadFixtureBundle(t)
	dir := specPath(t, "fileformat/fixtures")
	paths, err := filepath.Glob(filepath.Join(dir, "*.jed"))
	if err != nil || len(paths) == 0 {
		t.Fatalf("no fixtures in %s: %v", dir, err)
	}
	sort.Strings(paths)
	per := resealedMutations()
	cases, failed, skipped := 0, 0, 0
	defer func() { t.Logf("%d resealed cases, %d failed closed, %d fixtures skipped", cases, failed, skipped) }()
	for _, path := range paths {
		base := filepath.Base(path)
		raw, err := os.ReadFile(path)
		if err != nil {
			t.Fatal(err)
		}
		clean, err := openImage(raw)
		if err != nil {
			skipped++
			continue // a fixture that is malformed by design
		}
		var names []string
		for name := range clean.committed.tables {
			names = append(names, name)
		}
		sort.Strings(names)
		tables := make([]*catTable, len(names))
		for i, name := range names {
			tables[i] = clean.committed.tables[name]
		}
		clean.Close()
		stmts := resealedStatements(tables)

		image := trimToHighWater(raw)
		ps := int(binary.BigEndian.Uint32(image[8:12]))
		pages := len(image) / ps
		for p := 2; p < pages; p++ {
			for m := 0; m < per; m++ {
				seed := uint64(p)<<32 | uint64(m)
				r := &resealedRNG{s: seed ^ uint64(len(base))<<48}
				mutated := append([]byte(nil), image...)
				what := mutatePage(mutated, ps, p, pages, r)
				cases++
				err := runResealed(mutated, stmts)
				if err != nil && !isEngineError(err) {
					t.Errorf("%s page %d mutation %d (%s): %v", base, p, m, what, err)
				} else if err != nil {
					failed++
				}
			}
		}
	}
}

// panicSite is the first engine frame below the panic in a goroutine stack, so a failure names the
// decoder that indexed out of range.
func panicSite(stack []byte) string {
	lines := strings.Split(string(stack), "\n")
	for i, l := range lines {
		if strings.HasPrefix(l, "panic(") {
			for j := i + 2; j+1 < len(lines); j += 2 {
				if strings.Contains(lines[j], "jed/impl/go.") && !strings.Contains(lines[j], "runResealed") {
					var sites []string
					for k := j + 1; k < len(lines) && len(sites) < 4; k += 2 {
						f := strings.TrimSpace(lines[k])
						sites = append(sites, f[strings.LastIndex(f, "/")+1:])
					}
					return strings.Join(sites, " < ")
				}
			}
		}
	}
	return "unknown"
}
