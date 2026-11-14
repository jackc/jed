package jed

// EXPERIMENT (not part of the core's test contract). Three measurements on the format experiment's
// database plus the prose and array tables (zz_regioncodec_test.go), compacted, at the default page
// size:
//
//  1. where the file's bytes are in the current leaf format, tables and indexes apart;
//  2. how much heap a resident clean leaf holds beyond its page block in this core;
//  3. what a shared compression context would save when its scope is one leaf, a group of G
//     consecutive leaves (a "super block"), or the whole column.
//
//	JED_FORMAT_EXP=1 go test -run TestSuperBlockExperiment -v -timeout 60m .

import (
	"context"
	"fmt"
	"math/bits"
	"os"
	"path/filepath"
	"runtime"
	"sort"
	"testing"
)

type sbLeaf struct {
	n     int
	cur   int      // bytes the page spends on this region today
	spans [][]byte // each value's stored span (a view of the page)
}

type sbCol struct {
	tree, name string
	rows       int
	leaves     []sbLeaf
}

type sbTree struct {
	name, kind    string
	leaves, inner int
	rows          int
	acct          map[string]int
	// extra is the heap a resident leaf of this tree holds beyond its page, by cause (computed from
	// the parse; the measured figure is taken separately).
	extra map[string]int
	// alternatives, in bytes saved against today's regions
	bitpack, boolBits, frontKeys int
	dictPack, allNull, nullSlots int
	heapFault, heapScanned       int
}

func sbHeap() int {
	runtime.GC()
	runtime.GC()
	var m runtime.MemStats
	runtime.ReadMemStats(&m)
	return int(m.HeapAlloc)
}

func sbLCP(a, b []byte) int {
	n := 0
	for n < len(a) && n < len(b) && a[n] == b[n] {
		n++
	}
	return n
}

// sbSymCoded is a region's size under table tb: kind byte, raw bitmap, end offsets, coded bodies. An
// inline-plain value (tag 0x00 or 0x05) is coded without its tag; any other is stored as it is.
func sbSymCoded(tb *symTable, lf *sbLeaf) int {
	size := 1 + (lf.n+7)/8 + 2*lf.n
	for _, sp := range lf.spans {
		switch {
		case len(sp) == 0:
		case sp[0] == 0x00 || sp[0] == 0x05:
			size += tb.encLen(sp[1:])
		default:
			size += len(sp)
		}
	}
	return size
}

func sbPayloads(leaves []sbLeaf, limit int) [][]byte {
	total := 0
	for i := range leaves {
		for _, sp := range leaves[i].spans {
			total += len(sp)
		}
	}
	stride := 1
	if limit > 0 && total > limit {
		stride = (total + limit - 1) / limit
	}
	var out [][]byte
	for i := 0; i < len(leaves); i += stride {
		for _, sp := range leaves[i].spans {
			if len(sp) > 1 && (sp[0] == 0x00 || sp[0] == 0x05) {
				out = append(out, sp[1:])
			}
		}
	}
	return out
}

// sbGroup sizes one group of a column's leaves under a context shared by the group. It returns the
// group's bytes today, with a shared dictionary, with a shared symbol table, and with both; a leaf
// keeps today's region whenever that is smaller, and a context is charged once when any leaf uses it.
func sbGroup(leaves []sbLeaf, own bool) (cur, dict, sym, both int) {
	for i := range leaves {
		cur += leaves[i].cur
	}
	// Shared dictionary: every distinct span once, one- or two-byte codes.
	seen := map[string]struct{}{}
	dictBytes := 2
	for i := range leaves {
		for _, sp := range leaves[i].spans {
			if _, ok := seen[string(sp)]; !ok {
				seen[string(sp)] = struct{}{}
				dictBytes += 2 + len(sp)
			}
		}
	}
	cw := 0
	switch {
	case len(seen) <= 256:
		cw = 1
	case len(seen) <= 65536:
		cw = 2
	}
	// Shared symbol table: trained on at most 64 KiB of the group's payloads, both kinds, the one
	// that codes the group smaller. A leaf's own table (own) is the literal-preserving kind only.
	var tabs []*symTable
	pl := sbPayloads(leaves, 64<<10)
	if tb := trainSym(pl, false); tb != nil && len(tb.syms) > 0 {
		tabs = append(tabs, tb)
	}
	if !own {
		if tb := trainSym(pl, true); tb != nil && len(tb.syms) > 0 {
			tabs = append(tabs, tb)
		}
	}
	dict, sym, both = cur, cur, cur
	if cw > 0 {
		d := dictBytes
		for i := range leaves {
			d += min(leaves[i].cur, 1+cw*leaves[i].n)
		}
		dict = min(dict, d)
	}
	for _, tb := range tabs {
		s, b := tb.size, tb.size
		if cw > 0 {
			b += dictBytes
		}
		for i := range leaves {
			c := sbSymCoded(tb, &leaves[i])
			s += min(leaves[i].cur, c)
			if cw > 0 {
				c = min(c, 1+cw*leaves[i].n)
			}
			b += min(leaves[i].cur, c)
		}
		sym = min(sym, s)
		both = min(both, b)
	}
	both = min(both, dict, sym)
	return
}

func TestSuperBlockExperiment(t *testing.T) {
	if os.Getenv("JED_FORMAT_EXP") == "" {
		t.Skip("set JED_FORMAT_EXP=1")
	}
	dir := os.Getenv("JED_FORMAT_EXP_DIR")
	if dir == "" {
		dir = t.TempDir()
	}
	path := filepath.Join(dir, "sb.jed")
	var db *Database
	if _, err := os.Stat(path); err != nil {
		db = expBuild(t, path, DefaultPageSize)
		expExtra(t, db)
		if err := db.Compact("main"); err != nil {
			t.Fatal(err)
		}
	} else if db, err = OpenDatabase(path); err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	pin, err := db.core.integrityPin(context.Background(), "main", nil)
	if err != nil {
		t.Fatal(err)
	}
	defer pin.release()
	w := &expWalker{paging: pin.paging, pcap: pin.paging.cap(), t: t}
	C := w.pcap.c
	var names []string
	for name := range pin.snap.tables {
		names = append(names, name)
	}
	sort.Strings(names)
	var trees []*sbTree
	var cols []*sbCol

	walk := func(name, kind string, root *pnode, colTypes []colType, colNames []string) {
		if root == nil {
			return
		}
		tr := &sbTree{name: name, kind: kind, acct: map[string]int{}, extra: map[string]int{}}
		trees = append(trees, tr)
		cs := make([]*sbCol, len(colTypes))
		et := &expTree{}
		var kept []*pnode
		var blocks [][]byte
		w.eachLeaf(root.ref(), colTypes, et, func(n int, payload []byte, leaf *paxLeaf) {
			tr.leaves++
			tr.rows += n
			used := leaf.keyStored + 2*(len(colTypes)+1)
			tr.acct["keys"] += leaf.keyStored
			tr.acct["column directory"] += 2 * (len(colTypes) + 1)
			tr.acct["page header"] += 16
			if leaf.keyPrefix != nil {
				full := 0
				for i := 0; i < n; i++ {
					full += len(leaf.keyPrefix) + len(leaf.suffix(i))
				}
				if leaf.keyLen == 0 {
					full += 4 * n
				}
				tr.extra["full keys (on first key read)"] += full
			}
			if kind == "index" {
				// Front coding with a full key every 16: a shared-length byte and a length byte per
				// key, a u16 restart offset per 16.
				fc := 1 + 2*((n+15)/16)
				if leaf.keyPrefix != nil {
					fc += 2 + len(leaf.keyPrefix)
				}
				for i := 0; i < n; i++ {
					s := leaf.suffix(i)
					if i%16 == 0 {
						fc += 2 + len(s)
					} else {
						fc += 2 + len(s) - min(255, sbLCP(s, leaf.suffix(i-1)))
					}
				}
				if fc < leaf.keyStored {
					tr.frontKeys += leaf.keyStored - fc
				}
			}
			for c, ty := range colTypes {
				r := &leaf.regions[c]
				switch {
				case r.elided:
					tr.extra["elided key column copy (at fault)"] += len(r.body) + len(r.bitmap)
				case r.width > 0:
					cur := 1 + len(r.bitmap) + len(r.body)
					label := "fixed-width, plain"
					if r.stored != 0 {
						cur, label = r.stored, "fixed-width, base plus offsets"
						tr.extra["base-plus-offsets copy (at fault)"] += len(r.body)
					}
					used += cur
					tr.acct[label] += cur
					nulls := 0
					for i := 0; i < n; i++ {
						if r.bitmap[i/8]&(0x80>>(i%8)) != 0 {
							nulls++
						}
					}
					if nulls == n {
						tr.allNull += cur - 1
					} else if r.stored == 0 {
						tr.nullSlots += nulls * r.width
					}
					if forEligible(ty) {
						var lo, hi uint64
						first := true
						for i := 0; i < n; i++ {
							if r.bitmap[i/8]&(0x80>>(i%8)) != 0 {
								continue
							}
							v := beUint(r.body[i*r.width : (i+1)*r.width])
							if first || v < lo {
								lo = v
							}
							if first || v > hi {
								hi = v
							}
							first = false
						}
						bp := 1 + len(r.bitmap) + r.width + 1 + (n*bits.Len64(hi-lo)+7)/8
						if bp < cur {
							tr.bitpack += cur - bp
						}
					} else if ty.Scalar == scalarBool && !ty.Composite && ty.Elem == nil && ty.RangeElem == nil {
						tr.boolBits += cur - (1 + 2*len(r.bitmap))
					}
				case r.lazy != nil:
					cur := 1 + len(r.lazy.raw)
					used += cur
					tr.acct["integer arrays, packed"] += cur
					m := r.lazy.plain()
					tr.extra["packed array rebuild (on first use)"] += len(m.ends) + len(m.body)
				default:
					cur := 1 + len(r.ends) + len(r.body)
					label := "variable-width, plain"
					if r.codes != nil {
						cur, label = r.stored, "variable-width, dictionary"
						if e := len(r.ends) / leaf.dw; e > 0 {
							tr.dictPack += n - (n*bits.Len(uint(e-1))+7)/8
						}
					}
					used += cur
					tr.acct[label] += cur
					if cs[c] == nil {
						cs[c] = &sbCol{tree: name, name: colNames[c]}
					}
					lf := sbLeaf{n: n, cur: cur, spans: make([][]byte, n)}
					for i := 0; i < n; i++ {
						sp, err := leaf.value(c, i)
						if err != nil {
							t.Fatal(err)
						}
						lf.spans[i] = sp
					}
					empty := true
					for _, sp := range lf.spans {
						if len(sp) != 0 {
							empty = false
							break
						}
					}
					if empty {
						tr.allNull += cur - 1
					}
					cs[c].rows += n
					cs[c].leaves = append(cs[c].leaves, lf)
				}
			}
			tr.acct["unused tail of the page"] += C - used
		})
		tr.inner = et.inner
		for _, c := range cs {
			if c != nil {
				cols = append(cols, c)
			}
		}
		// Measured heap: fault every leaf of the tree through the pager's decode and keep the nodes.
		var refs []pageRef
		var collect func(ref pageRef)
		collect = func(ref pageRef) {
			block, err := w.paging.readBody(ref)
			if err != nil {
				t.Fatal(err)
			}
			pg, err := parsePage(block)
			if err != nil {
				t.Fatal(err)
			}
			if pg.pageType == pageInterior {
				children, _, err := parseIntegrityInterior(pg, w.pcap)
				if err != nil {
					t.Fatal(err)
				}
				for _, c := range children {
					collect(c)
				}
				return
			}
			refs = append(refs, ref)
		}
		collect(root.ref())
		kept = make([]*pnode, 0, len(refs))
		blocks = make([][]byte, 0, len(refs))
		h0 := sbHeap()
		for _, ref := range refs {
			block, err := w.paging.readBody(ref)
			if err != nil {
				t.Fatal(err)
			}
			blocks = append(blocks, block)
		}
		hBlocks := sbHeap() - h0
		for i, ref := range refs {
			nd, err := decodeLeafNode(blocks[i], ref, colTypes, pin.paging)
			if err != nil {
				t.Fatal(err)
			}
			kept = append(kept, nd)
		}
		tr.heapFault = sbHeap() - h0
		for _, nd := range kept {
			if nd.packed.n > 0 {
				_ = nd.packed.key(0)
			}
			for c := range colTypes {
				if r := &nd.packed.dirs.regions[c]; r.lazy != nil {
					r.lazy.plain()
				}
			}
		}
		tr.heapScanned = sbHeap() - h0
		_ = hBlocks
		runtime.KeepAlive(kept)
		runtime.KeepAlive(blocks)
	}
	for _, name := range names {
		table, store := pin.snap.tables[name], pin.snap.stores[name]
		var colNames []string
		for _, c := range table.Columns {
			colNames = append(colNames, c.Name)
		}
		walk(name, "table", store.treeRoot(), store.colTypes, colNames)
		for _, def := range table.Indexes {
			if ist := pin.snap.indexStores[def.Name]; ist != nil {
				walk(def.Name, "index", ist.treeRoot(), nil, nil)
			}
		}
	}

	st, _ := os.Stat(path)
	ps := C + 16
	mb := func(x int) float64 { return float64(x) / (1 << 20) }
	fmt.Printf("\nfile: %.1f MB, %d pages of %d\n", mb(int(st.Size())), int(st.Size())/ps, ps)

	// 1. Where the bytes are.
	fmt.Printf("\n== 1. bytes by tree ==\n%-24s %-5s %8s %6s %5s %8s %7s %6s\n", "tree", "kind", "rows", "leaves", "inner", "MB", "B/row", "fill%")
	tot := map[string]map[string]int{"table": {}, "index": {}}
	pages := map[string]int{}
	inner := 0
	for _, tr := range trees {
		used := tr.leaves*ps - tr.acct["unused tail of the page"]
		fmt.Printf("%-24s %-5s %8d %6d %5d %8.1f %7.1f %6.1f\n", tr.name, tr.kind, tr.rows, tr.leaves, tr.inner,
			mb(tr.leaves*ps), float64(tr.leaves*ps)/float64(tr.rows), 100*float64(used)/float64(tr.leaves*ps))
		for k, v := range tr.acct {
			tot[tr.kind][k] += v
		}
		pages[tr.kind] += tr.leaves
		inner += tr.inner
	}
	leafBytes := (pages["table"] + pages["index"]) * ps
	fmt.Printf("\nleaf pages: %d table (%.1f MB), %d index (%.1f MB); interior pages: %d (%.1f MB)\n",
		pages["table"], mb(pages["table"]*ps), pages["index"], mb(pages["index"]*ps), inner, mb(inner*ps))
	for _, kind := range []string{"table", "index"} {
		fmt.Printf("\n%s leaves, by component (MB, share of all leaf bytes)\n", kind)
		var ks []string
		for k := range tot[kind] {
			ks = append(ks, k)
		}
		sort.Slice(ks, func(i, j int) bool { return tot[kind][ks[i]] > tot[kind][ks[j]] })
		for _, k := range ks {
			fmt.Printf("  %-34s %8.2f %5.1f%%\n", k, mb(tot[kind][k]), 100*float64(tot[kind][k])/float64(leafBytes))
		}
	}
	var bp, bb, fk, dp, an, ns int
	for _, tr := range trees {
		dp += tr.dictPack
		an += tr.allNull
		ns += tr.nullSlots
		bp += tr.bitpack
		bb += tr.boolBits
		fk += tr.frontKeys
	}
	fmt.Printf("\nper-leaf alternatives, bytes saved against today's regions (share of all leaf bytes)\n")
	fmt.Printf("  bit-packed offsets in place of byte-aligned   %8.2f MB %5.1f%%\n", mb(bp), 100*float64(bp)/float64(leafBytes))
	fmt.Printf("  bit-packed dictionary codes                   %8.2f MB %5.1f%%\n", mb(dp), 100*float64(dp)/float64(leafBytes))
	fmt.Printf("  all-NULL regions as a flag                    %8.2f MB %5.1f%%\n", mb(an), 100*float64(an)/float64(leafBytes))
	fmt.Printf("  NULL slots of plain fixed-width regions       %8.2f MB %5.1f%%\n", mb(ns), 100*float64(ns)/float64(leafBytes))
	fmt.Printf("  booleans as bits                              %8.2f MB %5.1f%%\n", mb(bb), 100*float64(bb)/float64(leafBytes))
	fmt.Printf("  index keys front-coded, restart every 16      %8.2f MB %5.1f%%\n", mb(fk), 100*float64(fk)/float64(leafBytes))

	// 2. Resident heap per leaf.
	fmt.Printf("\n== 2. heap held by resident clean leaves (Go core) ==\n%-24s %-5s %6s %9s %9s %9s\n", "tree", "kind", "leaves", "pages MB", "faulted", "scanned")
	var pg, hf, hs int
	extra := map[string]int{}
	for _, tr := range trees {
		p := tr.leaves * ps
		fmt.Printf("%-24s %-5s %6d %9.1f %8.2fx %8.2fx\n", tr.name, tr.kind, tr.leaves, mb(p), float64(tr.heapFault)/float64(p), float64(tr.heapScanned)/float64(p))
		pg += p
		hf += tr.heapFault
		hs += tr.heapScanned
		for k, v := range tr.extra {
			extra[k] += v
		}
	}
	fmt.Printf("%-24s %-5s %6s %9.1f %8.2fx %8.2fx   (heap %.1f MB faulted, %.1f MB scanned)\n", "all", "", "", mb(pg), float64(hf)/float64(pg), float64(hs)/float64(pg), mb(hf), mb(hs))
	fmt.Printf("\nbytes beyond the page, by cause (from the parse)\n")
	for k, v := range extra {
		fmt.Printf("  %-40s %8.1f MB  %5.1f%% of leaf pages\n", k, mb(v), 100*float64(v)/float64(pg))
	}

	// 3. Scope of a shared context for variable-width columns.
	scopes := []int{1, 8, 64, 512, 0}
	label := func(g int) string {
		switch g {
		case 1:
			return "one leaf"
		case 0:
			return "the column"
		}
		return fmt.Sprintf("%d leaves", g)
	}
	type res struct{ cur, dict, sym, both int }
	total := map[int]*res{}
	per := map[string]map[int]*res{}
	var order []string
	for _, col := range cols {
		name := col.tree + "." + col.name
		order = append(order, name)
		per[name] = map[int]*res{}
		for _, g := range scopes {
			r := &res{}
			size := g
			if g == 0 {
				size = len(col.leaves)
			}
			for at := 0; at < len(col.leaves); at += size {
				c, d, s, b := sbGroup(col.leaves[at:min(len(col.leaves), at+size)], g == 1)
				r.cur += c
				r.dict += d
				r.sym += s
				r.both += b
			}
			per[name][g] = r
			if total[g] == nil {
				total[g] = &res{}
			}
			total[g].cur += r.cur
			total[g].dict += r.dict
			total[g].sym += r.sym
			total[g].both += r.both
		}
	}
	if os.Getenv("JED_SB_SKIP_SCOPE") != "" {
		return
	}
	fmt.Printf("\n== 3. variable-width column regions under a context of each scope (MB) ==\n")
	fmt.Printf("%-14s %9s %12s %12s %9s %14s\n", "scope", "today", "shared dict", "symbol table", "both", "of leaf bytes")
	for _, g := range scopes {
		r := total[g]
		fmt.Printf("%-14s %9.1f %12.1f %12.1f %9.1f %13.1f%%\n", label(g), mb(r.cur), mb(r.dict), mb(r.sym), mb(r.both), 100*float64(r.cur-r.both)/float64(leafBytes))
	}
	fmt.Printf("\nbytes per value, 'both', by scope\n%-28s %8s", "column", "today")
	for _, g := range scopes {
		fmt.Printf(" %10s", label(g))
	}
	fmt.Println()
	for i, name := range order {
		rows := float64(cols[i].rows)
		fmt.Printf("%-28s %8.1f", name, float64(per[name][1].cur)/rows)
		for _, g := range scopes {
			fmt.Printf(" %10.1f", float64(per[name][g].both)/rows)
		}
		fmt.Println()
	}
}
