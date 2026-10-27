package jed

// The deterministic-simulation integrity checker (spec/design/simulation-testing.md §8.3). It opens
// a copy of the disk image the way a new process would — so the meta selection and manifest
// validation of validated-cow.md §1 run first — checks that the selected generation is the expected
// version, and runs the engine's integrity check (spec/design/integrity-check.md) over it at the full
// level with no finding limit: page verification, tree shape, key order and primary keys, row counts,
// overflow chains (one record per chain, large-values.md §15), index agreement, the free list, and the
// live page count. Any finding fails the step with XX001.

import (
	"context"
	"fmt"
	"sort"
	"strings"
)

func simIntegrity(image []byte, want uint64, cred *Credential) (live, pages, free uint32, err error) {
	p, err := pagerFromStore(newMemoryBlockStore(image))
	if err != nil {
		return 0, 0, 0, fmt.Errorf("open: %w", err)
	}
	env := NewEnvironment()
	if err := unlockPager(p, cred, env); err != nil {
		return 0, 0, 0, fmt.Errorf("open: %w", err)
	}
	e, err := loadEngineSharedPaging(newSharedPaging(p, cacheLeaves(defaultCacheBytes, p.pageSize)), env)
	if err != nil {
		return 0, 0, 0, fmt.Errorf("open: %w", err)
	}
	mt, err := p.selectValidatedMeta()
	if err != nil {
		return 0, 0, 0, fmt.Errorf("meta: %w", err)
	}
	if mt.txid != want {
		return 0, 0, 0, fmt.Errorf("the image holds version %d, want %d", mt.txid, want)
	}
	gen := integrityGeneration{meta: mt, file: true, protected: p.manifestProtected}
	k := newIntegrityChecker(context.Background(), "main", IntegrityCheckOptions{MaxFindings: IntegrityUnlimited}, e.committed, e.paging, gen)
	report, err := k.run(nil)
	if err != nil {
		return 0, 0, 0, fmt.Errorf("integrity check: %w", err)
	}
	if !report.OK() {
		var lines []string
		for _, f := range report.Findings {
			line := f.Kind
			if f.Object != "" {
				line += " " + f.Object
			}
			if f.Page != nil {
				line += fmt.Sprintf(" page %d", *f.Page)
			}
			if f.Key != nil {
				line += fmt.Sprintf(" key %x", f.Key)
			}
			lines = append(lines, line+": "+f.Detail)
		}
		return 0, 0, 0, newError(DataCorrupted, "integrity check: "+strings.Join(lines, "; "))
	}
	return mt.livePages, mt.pageCount, uint32(len(e.freePages)), nil
}

// simCheckLive checks the LIVE process, not a fresh open of its image: every page its committed
// in-memory trees reference — tree nodes, and the overflow chains reached through resident rows'
// unfetched references as well as faulted leaves — must lie below the page high-water and off the
// in-memory free list the next commit allocates from. A fresh open re-derives both from disk, so
// only this view sees a resident node whose references went stale.
func simCheckLive(db *Database) error {
	core := db.core
	snap := core.roots.Load().committed
	st := core.storage
	st.mu.Lock()
	pageCount := st.pageCount
	free := make(map[uint32]bool, len(st.freePages))
	for _, pg := range st.freePages {
		free[pg] = true
	}
	st.mu.Unlock()
	paging := st.paging
	fetch := paging.readBody

	check := func(what string, pg uint32) error {
		switch {
		case pg >= pageCount:
			return fmt.Errorf("%s references page %d past the high-water %d", what, pg, pageCount)
		case free[pg]:
			return fmt.Errorf("%s references page %d, which is on the free list", what, pg)
		}
		return nil
	}
	names := make([]string, 0, len(snap.stores))
	for name := range snap.stores {
		names = append(names, name)
	}
	sort.Strings(names)
	claimed := map[uint32]bool{}
	for _, name := range names {
		store := snap.stores[name]
		nodes := map[uint32]bool{}
		collectTreePages(store.treeRoot(), nodes)
		for pg := range nodes {
			if err := check("table "+name+"'s tree", pg); err != nil {
				return err
			}
		}
		entries, err := store.EntriesInKeyOrder()
		if err != nil {
			return fmt.Errorf("table %s: %w", name, err)
		}
		for _, e := range entries {
			for _, v := range e.Row {
				first, length, ok := simExternalRef(v)
				if !ok {
					continue
				}
				pages, err := chainPages(first, length, fetch, paging.cap())
				if err != nil {
					return fmt.Errorf("table %s: an overflow chain: %w", name, err)
				}
				for _, pg := range pages {
					if err := check("table "+name+"'s overflow chain", pg); err != nil {
						return err
					}
					// Chain exclusivity (large-values.md §15): a kept chain moves to the rewritten
					// record; two committed records never share one.
					if claimed[pg] {
						return fmt.Errorf("table %s: overflow page %d is referenced twice", name, pg)
					}
					claimed[pg] = true
				}
			}
		}
	}
	for name, ist := range snap.indexStores {
		nodes := map[uint32]bool{}
		collectTreePages(ist.treeRoot(), nodes)
		for pg := range nodes {
			if err := check("index "+name+"'s tree", pg); err != nil {
				return err
			}
		}
	}
	return nil
}

// simExternalRef returns the overflow chain a stored value names — its first page and the bytes the
// chain carries — or ok false for an inline or resident value.
func simExternalRef(v Value) (first pageRef, length int, ok bool) {
	if v.Kind != ValUnfetched {
		return pageRef{}, 0, false
	}
	u := v.unfetched()
	if u.Form != tagExternal && u.Form != tagExternalComp {
		return pageRef{}, 0, false
	}
	return u.first(), int(u.StoredLen), true
}

// --- page replay (simulation-testing.md §8.3 *Replays*, slice S13) ----------------------------

// simPointerKinds classifies the pages of image's selected generation by the kind of bound pointer
// that names each (encrypted.md §2), and returns the pages that generation's manifest depends on: its
// listed pages and its manifest-overflow pages. A page's kind is its pointer's, so a chain's head is
// `catalog`, `free`, or an overflow kind, and every later page of the chain its `_next` kind; a tree's
// root is `root` and every other node `child` (`gist_child` in a GiST R-tree).
func simPointerKinds(image []byte, cred *Credential) (map[string][]uint32, map[uint32]bool, error) {
	pgr, err := pagerFromStore(newMemoryBlockStore(image))
	if err != nil {
		return nil, nil, err
	}
	env := NewEnvironment()
	if err := unlockPager(pgr, cred, env); err != nil {
		return nil, nil, err
	}
	paging := newSharedPaging(pgr, cacheLeaves(defaultCacheBytes, pgr.pageSize))
	var gist []uint32
	e, err := loadEngineSharedPagingAt(paging, env, loadAt{gist: func(pg uint32) { gist = append(gist, pg) }})
	if err != nil {
		return nil, nil, err
	}
	mt, err := pgr.selectValidatedMeta()
	if err != nil {
		return nil, nil, err
	}
	deps := map[uint32]bool{}
	for pg := range pgr.manifestProtected {
		deps[pg] = true
	}
	sets := map[string]map[uint32]bool{}
	add := func(kind string, pg uint32) {
		if sets[kind] == nil {
			sets[kind] = map[uint32]bool{}
		}
		sets[kind][pg] = true
	}
	pcap := paging.cap()
	chain := func(head pageRef, first, next string) error {
		kind := first
		for p := head; p.page != 0; kind = next {
			add(kind, p.page)
			block, err := paging.readBody(p)
			if err != nil {
				return err
			}
			pg, err := parsePage(block)
			if err != nil {
				return err
			}
			if p, _, err = chainContent(pg, pcap); err != nil {
				return err
			}
		}
		return nil
	}
	if err := chain(pageRef{page: mt.rootPage, tag: mt.rootTag}, "catalog", "catalog_next"); err != nil {
		return nil, nil, err
	}
	if err := chain(pageRef{page: mt.freeListHead, tag: mt.freeListTag}, "free", "free_next"); err != nil {
		return nil, nil, err
	}
	// Manifest-overflow pages are plaintext; the meta's tag binds them through its associated data.
	for id, n := mt.manifestHead, uint32(0); id != 0 && n < mt.manifestPages; n++ {
		add("manifest", id)
		block, err := pgr.readBlock(id)
		if err != nil {
			return nil, nil, err
		}
		pg, err := parsePage(block)
		if err != nil {
			return nil, nil, err
		}
		id = pg.nextPage
	}
	var walk func(n *pnode, kind string)
	walk = func(n *pnode, kind string) {
		if n.page != 0 {
			add(kind, n.page)
		}
		for _, c := range n.children {
			if c.node == nil {
				add("child", c.page)
			} else {
				walk(c.node, "child")
			}
		}
	}
	snap := e.committed
	for _, st := range snap.stores {
		if root := st.treeRoot(); root != nil {
			walk(root, "root")
		}
		entries, err := st.EntriesInKeyOrder()
		if err != nil {
			return nil, nil, err
		}
		for _, entry := range entries {
			for _, v := range entry.Row {
				first, length, ok := simExternalRef(v)
				if !ok {
					continue
				}
				pages, err := chainPages(first, length, paging.readBody, pcap)
				if err != nil {
					return nil, nil, err
				}
				head := "overflow"
				if v.unfetched().Form == tagExternalComp {
					head = "overflow_lz4"
				}
				for k, pg := range pages {
					if k == 0 {
						add(head, pg)
					} else {
						add("overflow_next", pg)
					}
				}
			}
		}
	}
	for _, ist := range snap.indexStores {
		if root := ist.treeRoot(); root != nil && root.page != 0 {
			walk(root, "root")
		}
	}
	// A GiST R-tree is read whole at load (gist.md §4.1): its pages are the ones the load visited. Each
	// interior's children come from the loader's own parser, reading only that page; whatever no
	// interior names is a root.
	named := map[uint32]bool{}
	for _, pg := range gist {
		stored, err := pgr.readBlock(pg)
		if err != nil {
			return nil, nil, err
		}
		ref := pageRef{page: pg}
		if pgr.codec != nil {
			copy(ref.tag[:], stored[len(stored)-len(ref.tag):]) // the load already checked it against its pointer
		}
		block, err := paging.openBlock(ref, stored)
		if err != nil {
			return nil, nil, err
		}
		if parsed, err := parsePage(block); err != nil || parsed.pageType != pageGistInterior {
			continue
		}
		var keys [][]byte
		read := func(p pageRef) (byte, uint32, []byte, error) {
			if p.page != pg {
				named[p.page] = true
				return pageGistLeaf, 0, nil, nil
			}
			parsed, err := parsePage(block)
			return parsed.pageType, parsed.itemCount, parsed.payload, err
		}
		if err := readGistLeafKeys(read, pageRef{page: pg}, pcap, &keys, map[uint32]bool{}, 1); err != nil {
			return nil, nil, err
		}
	}
	for _, pg := range gist {
		if named[pg] {
			add("gist_child", pg)
		} else {
			add("root", pg)
		}
	}
	kinds := make(map[string][]uint32, len(sets))
	for kind, set := range sets {
		pages := make([]uint32, 0, len(set))
		for pg := range set {
			pages = append(pages, pg)
		}
		sort.Slice(pages, func(a, b int) bool { return pages[a] < pages[b] })
		kinds[kind] = pages
	}
	return kinds, deps, nil
}
