package jed

// Incremental free-list reclamation (spec/design/incremental-reclaim.md; format.md *Reclamation*).
// Each file commit lists the pages it orphans in its own persisted free list, so dead pages do not wait
// for a reachability rebuild.
//
// The rules (design doc §3):
//   - commit T lists its orphans in its own free list, but never reuses them itself: they are
//     reachable from T's fallback meta (T−1);
//   - from T+1 they are reusable, subject to the reader watermark: in memory each listed set keeps its
//     generation (T) until no reader pins a version older than T (incrState.gens);
//   - under synchronous = off an orphan that was written inside the pending batch is in the floor's
//     manifest, so it is held in memory and listed only by the first commit planned with nothing
//     pending (incrState.held); the batch reuse rule (only pages free at the floor) does the rest;
//   - a rare rebuild still runs, triggered by the exact count of unlisted dead pages (incrLeaked).

import (
	"fmt"
	"os"
	"sync/atomic"
)

// incrVerify makes every incremental commit check its listed pages against a reachability walk (test
// tooling; expensive). JED_INCR_VERIFY_STRICT=on also fails a commit that leaves a dead page unlisted
// beyond what its lifetime began with.
var incrVerify = os.Getenv("JED_INCR_VERIFY") == "on"

// incrOn is whether file commits reclaim incrementally. The Go-only symbol-table prototype
// (leaf_sym.go) keeps pages off the free list by rules of its own, so under it the commit falls back
// to the compact_growth rebuild.
//
// Deferred interiors (format v53, remap.go) run alongside this rule: its retained interiors are
// withheld from the list the way the batch's pages are (incrementalWrite.retainedIDs). Its entries,
// keyed by (parent page, slot), die in the commit that orphans their parent, so they hold no page off
// the list (deferred-interior.md §9.5) -- the held set the earlier global keying needed, and the bug
// it caused (§8.3), are gone.
func incrOn() bool { return !leafSym && !leafSymCol }

// reclaimWalks counts reachability rebuilds of file commits (test observability).
var reclaimWalks atomic.Uint64

// incrState is a file storage's incremental-reclamation state. Nothing in it is persisted: it is
// rebuilt at open from what the load reads anyway, and its loss (a role change, a foreign commit)
// only leaks pages until the leak-triggered rebuild.
type incrState struct {
	// known is whether catalog, gist and manifest name the committed state's catalog chain, GiST
	// R-tree pages and manifest-overflow pages. A page client's commit leaves them unknown (the owner
	// did not plan it); the next owner commit then leaks them.
	known    bool
	catalog  []uint32
	gist     []uint32
	manifest []uint32
	// manifestCount is how many manifest-overflow pages the committed meta names (known or not).
	manifestCount uint32
	// gens is the free pages listed since the base generation (storage.freeGenTxid), oldest first,
	// each tagged with the version from which it is dead.
	gens []freeGen
	// held is the orphans written inside the pending relaxed batch (validated-cow.md §4), sorted.
	held []uint32
	// tolerated is the unlisted dead pages the state started with (a reopen loses the held pages and a
	// one-entry list; a page client's commit leaks its orphans); incrCheck's strict mode allows them.
	tolerated     int
	unknownBefore bool
	// tree is the B-tree / overflow orphans of the commit being planned (admitFile computes them with
	// the live-page delta).
	tree []uint32
}

// incrPlan is what planValidatedCommit needs of an incremental commit: the orphans to list (never
// reused by this commit) and whether the leak trigger fires.
type incrPlan struct {
	listed []uint32
	due    bool
}

type freeGen struct {
	txid  uint64
	pages []uint32 // sorted
}

// incrActive is whether this storage's commits reclaim incrementally: a file's owner (or alone
// process). A page client's commit is planned against its owner's free list and leaves orphans
// unlisted, like a rebuild-mode commit.
func (st *storage) incrActive() bool {
	if !incrOn() || st.path == "" || st.serviceClient {
		return false
	}
	if st.incr == nil {
		st.incr = &incrState{}
	}
	return true
}

// incrMerge folds every generation no reader can still observe into the base generation.
func (st *storage) incrMerge(oldest uint64) {
	in := st.incr
	n := 0
	for n < len(in.gens) && in.gens[n].txid <= oldest {
		st.freeGenTxid = max(st.freeGenTxid, in.gens[n].txid)
		n++
	}
	if n > 0 {
		in.gens = append(in.gens[:0:0], in.gens[n:]...)
	}
}

// incrNewer is the listed pages a reader may still observe: those of the unmerged generations.
func (st *storage) incrNewer() map[uint32]bool {
	if len(st.incr.gens) == 0 {
		return nil
	}
	newer := map[uint32]bool{}
	for _, g := range st.incr.gens {
		for _, p := range g.pages {
			newer[p] = true
		}
	}
	return newer
}

// excludeNewer narrows a reuse restriction (nil: every free page) by the newer generations.
func excludeNewer(allowed, newer map[uint32]bool, free []uint32) map[uint32]bool {
	out := make(map[uint32]bool, len(free))
	for _, p := range free {
		if (allowed == nil || allowed[p]) && !newer[p] {
			out[p] = true
		}
	}
	return out
}

// incrLeaked is the exact count of dead pages no list holds, in the committed state: every page below
// the high-water is a meta or key-block page, live, free, a free-list chain page, a manifest-overflow
// page of the committed meta, held, or leaked.
func (st *storage) incrLeaked() int64 {
	chain := 0
	if st.freeChain != nil {
		chain = len(st.freeChain.refs)
	}
	in := st.incr
	return int64(st.pageCount) - int64(st.paging.cap().firstBody()) - int64(len(st.freePages)) -
		int64(st.live.live) - int64(chain) - int64(in.manifestCount) - int64(len(in.held))
}

// incrDue is the incremental mode's rebuild trigger.
func (st *storage) incrDue() bool {
	leaked := st.incrLeaked()
	return leaked > int64(compactMinPages) && uint64(leaked)*reclaimLeakDen > uint64(st.live.live)
}

// incrOrphans is what commit write orphans, split into the pages its free list may list now and those
// written inside the pending batch, which are held (pending is the batch's pending set). A commit
// planned with nothing pending also lists the held pages.
func (st *storage) incrOrphans(write incrementalWrite, pending []manifestEntry) (listable, held []uint32) {
	in := st.incr
	all := append([]uint32(nil), in.tree...)
	keySet := make(map[uint32]bool, len(write.retainedIDs))
	// Deferral (remap.go): a retained interior is still live at the page the previous snapshot's node holds, which the
	// orphan walk reports because it has to descend through that node.
	for _, p := range write.retainedIDs {
		keySet[p] = true
	}
	if in.known {
		if write.catalogIDs != nil {
			all = append(all, in.catalog...)
		}
		all = append(all, in.gist...)
		all = append(all, in.manifest...)
	}
	if st.freeChain != nil {
		for _, r := range st.freeChain.refs {
			all = append(all, r.page)
		}
	}
	if len(pending) == 0 {
		all = append(all, in.held...)
	}
	if len(keySet) > 0 {
		kept := all[:0]
		for _, p := range all {
			if !keySet[p] {
				kept = append(kept, p)
			}
		}
		all = kept
	}
	sortUint32s(all)
	for i := 1; i < len(all); i++ {
		if all[i] == all[i-1] {
			panic(fmt.Sprintf("incremental reclaim: page %d orphaned twice", all[i]))
		}
	}
	if len(pending) == 0 {
		return all, nil
	}
	inBatch := make(map[uint32]bool, len(pending))
	for _, e := range pending {
		inBatch[e.index] = true
	}
	for _, p := range all {
		if inBatch[p] {
			held = append(held, p)
		} else {
			listable = append(listable, p)
		}
	}
	return listable, held
}

// incrAfter records a published incremental commit.
func (st *storage) incrAfter(txid uint64, write incrementalWrite, plan validatedCommitPlan, listed, held []uint32, pending []manifestEntry) {
	in := st.incr
	in.unknownBefore = !in.known
	if plan.rebuilt {
		in.tolerated = 0
		// The rebuild listed every dead page except the batch's: only those stay held.
		in.gens = nil
		inBatch := make(map[uint32]bool, len(pending))
		for _, e := range pending {
			inBatch[e.index] = true
		}
		var keep []uint32
		for _, p := range mergeSortedU32(in.held, held) {
			if inBatch[p] {
				keep = append(keep, p)
			}
		}
		in.held = keep
	} else {
		if len(listed) > 0 {
			in.gens = append(in.gens, freeGen{txid: txid, pages: listed})
		}
		if len(pending) == 0 {
			in.held = nil
		} else {
			in.held = mergeSortedU32(in.held, held)
		}
	}
	if plan.pageCount < st.pageCount {
		in.gens = trimGens(in.gens, plan.pageCount)
	}
	if write.catalogIDs != nil {
		in.catalog = write.catalogIDs
	}
	in.gist = write.gistIDs
	in.manifest = in.manifest[:0:0]
	for _, pg := range plan.manifest {
		in.manifest = append(in.manifest, pg.index)
	}
	in.manifestCount = uint32(len(in.manifest))
	in.known = true
	in.tree = nil
}

// incrForeign records a commit this storage did not plan (a page client's, applied by its owner): the
// listed pages that survive keep their generations; the wholesale pages of the new state are unknown.
func (st *storage) incrForeign(free []uint32, manifestPages uint32) {
	if st.incr == nil {
		return
	}
	in := st.incr
	listed := make(map[uint32]bool, len(free))
	for _, p := range free {
		listed[p] = true
	}
	var gens []freeGen
	for _, g := range in.gens {
		var keep []uint32
		for _, p := range g.pages {
			if listed[p] {
				keep = append(keep, p)
			}
		}
		if len(keep) > 0 {
			gens = append(gens, freeGen{txid: g.txid, pages: keep})
		}
	}
	in.gens = gens
	in.known = false
	in.catalog, in.gist, in.manifest = nil, nil, nil
	in.manifestCount = manifestPages
	in.tree = nil
}

func trimGens(gens []freeGen, pageCount uint32) []freeGen {
	var out []freeGen
	for _, g := range gens {
		var keep []uint32
		for _, p := range g.pages {
			if p < pageCount {
				keep = append(keep, p)
			}
		}
		if len(keep) > 0 {
			out = append(out, freeGen{txid: g.txid, pages: keep})
		}
	}
	return out
}

// mergeSortedU32 merges two ascending lists (no duplicates across them).
func mergeSortedU32(a, b []uint32) []uint32 {
	if len(b) == 0 {
		return a
	}
	if len(a) == 0 {
		return b
	}
	out := make([]uint32, 0, len(a)+len(b))
	i, j := 0, 0
	for i < len(a) && j < len(b) {
		switch {
		case a[i] < b[j]:
			out = append(out, a[i])
			i++
		case a[i] > b[j]:
			out = append(out, b[j])
			j++
		default:
			panic(fmt.Sprintf("incremental reclaim: page %d listed twice", a[i]))
		}
	}
	out = append(out, a[i:]...)
	return append(out, b[j:]...)
}

// afterIDs is liveCount.after that also returns the orphaned B-tree and overflow page ids.
func (lc liveCount) afterIDs(write incrementalWrite, prev *snapshot, paging *sharedPaging) (liveCount, []uint32, error) {
	ids, err := orphanedTreePageIDs(prev, write.kept, write.sharedChains, paging, paging.cap())
	if err != nil {
		return liveCount{}, nil, err
	}
	live := int64(lc.live) + int64(len(write.pages)) + int64(write.retained) - int64(lc.catalog) - int64(lc.gist) - int64(len(ids))
	if live < 0 || live > 1<<32-1 {
		return liveCount{}, nil, newError(DataCorrupted, "live page count out of range")
	}
	return liveCount{live: uint32(live), catalog: write.catalogPages, gist: write.gistPages}, ids, nil
}

// orphanedTreePageIDs is orphanedTreePages listing the pages instead of counting them. An orphaned
// leaf's overflow chains are followed page by page (chainPages reads each page's header).
func orphanedTreePageIDs(prev *snapshot, kept, shared map[uint32]bool, paging *sharedPaging, capacity pageCap) ([]uint32, error) {
	var out []uint32
	var err error
	for _, st := range prev.stores {
		var colTypes []colType
		if anySpillable(st.colTypes) {
			colTypes = st.colTypes
		}
		if out, err = orphanedNodeIDs(st.treeRoot(), kept, shared, paging, colTypes, capacity, out); err != nil {
			return nil, err
		}
	}
	for _, ist := range prev.indexStores {
		if out, err = orphanedNodeIDs(ist.treeRoot(), kept, shared, paging, nil, capacity, out); err != nil {
			return nil, err
		}
	}
	return out, nil
}

func orphanedNodeIDs(n *pnode, kept, shared map[uint32]bool, paging *sharedPaging, colTypes []colType, capacity pageCap, out []uint32) ([]uint32, error) {
	if n == nil || n.page == 0 || kept[n.page] {
		return out, nil
	}
	if len(n.children) == 0 {
		return orphanedLeafIDs(n.ref(), shared, paging, colTypes, capacity, out)
	}
	out = append(out, n.page)
	var err error
	for _, c := range n.children {
		switch {
		case c.node != nil:
			out, err = orphanedNodeIDs(c.node, kept, shared, paging, colTypes, capacity, out)
		case kept[c.page]:
		default:
			out, err = orphanedLeafIDs(c.ref(), shared, paging, colTypes, capacity, out)
		}
		if err != nil {
			return nil, err
		}
	}
	return out, nil
}

func orphanedLeafIDs(pageIdx pageRef, shared map[uint32]bool, paging *sharedPaging, colTypes []colType, capacity pageCap, out []uint32) ([]uint32, error) {
	out = append(out, pageIdx.page)
	if colTypes == nil {
		return out, nil
	}
	block, err := paging.readBody(pageIdx)
	if err != nil {
		return nil, err
	}
	pg, err := parsePage(block)
	if err != nil {
		return nil, err
	}
	if pg.pageType != pageLeaf {
		return nil, newError(DataCorrupted, "expected a B-tree leaf page")
	}
	n := int(pg.itemCount)
	leaf, err := parsePaxLeaf(pg.payload, n, colTypes)
	if err != nil {
		return nil, err
	}
	for c, ty := range colTypes {
		if _, fixed := fixedValueWidth(ty); fixed {
			continue
		}
		for i := 0; i < n; i++ {
			vb, skip, err := leaf.chainSpan(c, i)
			if skip {
				continue
			}
			if err != nil {
				return nil, err
			}
			p := 0
			v, err := readValueLazy(colTypes, c, vb, &p, capacity, nil)
			if err != nil {
				return nil, err
			}
			if v.Kind != ValUnfetched {
				continue
			}
			switch u := v.unfetched(); u.Form {
			case tagExternal, tagExternalComp:
				if shared[u.FirstPage] {
					continue
				}
				pages, err := chainPages(u.first(), int(u.StoredLen), paging.readBody, capacity)
				if err != nil {
					return nil, err
				}
				out = append(out, pages...)
			}
		}
	}
	return out, nil
}

// incrCheck (JED_INCR_VERIFY) checks a planned incremental commit against a reachability walk of the
// new root: no listed page is reachable or written by the commit, and, when the storage knows the
// state it orphaned from, no dead page is left unlisted except the held ones.
func (st *storage) incrCheck(snap *snapshot, write incrementalWrite, plan validatedCommitPlan) {
	reached, err := snap.reachablePages(st.paging, write.root)
	if err != nil {
		return // an injected read fault: the check cannot run
	}
	for _, pg := range write.pages {
		reached[pg.index] = true
	}
	for _, pg := range plan.pages {
		reached[pg.index] = true
	}
	for _, p := range plan.free {
		if reached[p] {
			panic(fmt.Sprintf("incremental reclaim: listed page %d is live or written by txid %d", p, snap.txid))
		}
	}
	free := map[uint32]bool{}
	for _, p := range plan.free {
		free[p] = true
	}
	held := map[uint32]bool{}
	for _, p := range st.incr.held {
		held[p] = true
	}
	var leaked []uint32
	for p := st.paging.cap().firstBody(); p < plan.pageCount; p++ {
		if !reached[p] && !free[p] && !held[p] {
			leaked = append(leaked, p)
		}
	}
	if len(leaked) > st.incr.tolerated {
		if os.Getenv("JED_INCR_VERIFY_STRICT") == "on" {
			panic(fmt.Sprintf("incremental reclaim: txid %d leaves %d dead pages unlisted: %v", snap.txid, len(leaked), leaked[:min(len(leaked), 20)]))
		}
	}
}
