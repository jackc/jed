package jed

// The deterministic-simulation integrity checker (spec/design/simulation-testing.md §8.3). It opens
// a copy of the disk image the way a new process would — so the meta selection and manifest
// validation of validated-cow.md §1 run first — and then walks the selected generation:
//
//   - the live page count at meta offset 56 equals the pages reachable from the catalog root;
//   - every reachable page lies in [2, page_count);
//   - the free list has no duplicates and is disjoint from the reachable set and from the selected
//     generation's manifest dependencies;
//   - each table's storage keys ascend strictly and equal the primary-key encoding of their rows;
//   - each table's persisted row count equals its leaf total;
//   - each column index holds exactly the entries derived from its table's rows.

import (
	"bytes"
	"fmt"
	"sort"
)

func simIntegrity(image []byte, want uint64) (live, pages, free uint32, err error) {
	p, err := pagerFromStore(newMemoryBlockStore(image))
	if err != nil {
		return 0, 0, 0, fmt.Errorf("open: %w", err)
	}
	e, err := loadEnginePaged(p, cacheLeaves(defaultCacheBytes, p.pageSize))
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
	snap := e.committed

	reached, err := snap.reachablePages(e.paging, mt.rootPage)
	if err != nil {
		return 0, 0, 0, fmt.Errorf("reachability walk: %w", err)
	}
	if uint32(len(reached)) != mt.livePages {
		return 0, 0, 0, fmt.Errorf("live page count %d, but %d pages are reachable", mt.livePages, len(reached))
	}
	for pg := range reached {
		if pg < rootPage || pg >= mt.pageCount {
			return 0, 0, 0, fmt.Errorf("reachable page %d outside [%d, %d)", pg, rootPage, mt.pageCount)
		}
	}
	seen := make(map[uint32]bool, len(e.freePages))
	for _, pg := range e.freePages {
		switch {
		case seen[pg]:
			return 0, 0, 0, fmt.Errorf("free page %d listed twice", pg)
		case pg < rootPage || pg >= mt.pageCount:
			return 0, 0, 0, fmt.Errorf("free page %d outside [%d, %d)", pg, rootPage, mt.pageCount)
		case reached[pg]:
			return 0, 0, 0, fmt.Errorf("free page %d is reachable", pg)
		case p.manifestProtected[pg]:
			return 0, 0, 0, fmt.Errorf("free page %d is a dependency of the selected generation", pg)
		}
		seen[pg] = true
	}

	names := make([]string, 0, len(snap.tables))
	for name := range snap.tables {
		names = append(names, name)
	}
	sort.Strings(names)
	for _, name := range names {
		if err := simCheckTable(snap, name); err != nil {
			return 0, 0, 0, fmt.Errorf("table %s: %w", name, err)
		}
	}
	return mt.livePages, mt.pageCount, uint32(len(e.freePages)), nil
}

func simCheckTable(snap *snapshot, name string) error {
	table := snap.tables[name]
	store := snap.stores[name]
	if store == nil {
		return fmt.Errorf("no row store")
	}
	entries, err := store.EntriesInKeyOrder()
	if err != nil {
		return err
	}
	colls := make([]*Collation, len(table.Columns))
	for i, c := range table.Columns {
		if c.Collation != "" {
			colls[i] = snap.resolveCollation(c.Collation)
		}
	}
	for i := range entries {
		row, err := store.resolveAll(entries[i].Row)
		if err != nil {
			return fmt.Errorf("row %d: %w", i, err)
		}
		entries[i].Row = row
		if i > 0 && bytes.Compare(entries[i-1].Key, entries[i].Key) >= 0 {
			return fmt.Errorf("storage keys out of order at row %d", i)
		}
		if len(table.PK) > 0 {
			want, err := encodePkKey(store.colTypes, table.PK, colls, row)
			if err != nil {
				return err
			}
			if !bytes.Equal(want, entries[i].Key) {
				return fmt.Errorf("row %d is stored under a key that is not its primary key", i)
			}
		}
	}
	if count, known := store.Count(); known && count != int64(len(entries)) {
		return fmt.Errorf("persisted row count %d, but the tree holds %d rows", count, len(entries))
	}
	for _, def := range table.Indexes {
		if def.columnOrdinals() == nil || def.Predicate != nil || def.Kind != indexBtree {
			continue // expression, partial, GIN, and GiST indexes are not generated yet
		}
		var want [][]byte
		for _, e := range entries {
			keys, err := indexEntryKeysColumns(table.Columns, store.colTypes, colls, def, e.Key, e.Row)
			if err != nil {
				return err
			}
			want = append(want, keys...)
		}
		sort.Slice(want, func(a, b int) bool { return bytes.Compare(want[a], want[b]) < 0 })
		ist := snap.indexStores[def.Name]
		if ist == nil {
			return fmt.Errorf("index %s has no store", def.Name)
		}
		got, err := ist.EntriesInKeyOrder()
		if err != nil {
			return err
		}
		if len(got) != len(want) {
			return fmt.Errorf("index %s holds %d entries, want %d", def.Name, len(got), len(want))
		}
		for i := range got {
			if !bytes.Equal(got[i].Key, want[i]) {
				return fmt.Errorf("index %s entry %d does not match its table", def.Name, i)
			}
		}
	}
	return nil
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
	fetch := func(p uint32) ([]byte, error) { return paging.readBlock(p) }

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
				if v.Kind != ValUnfetched {
					continue
				}
				u := v.unfetched()
				if u.Form != tagExternal && u.Form != tagExternalComp {
					continue
				}
				pages, err := chainPages(u.FirstPage, int(u.StoredLen), fetch)
				if err != nil {
					return fmt.Errorf("table %s: an overflow chain: %w", name, err)
				}
				for _, pg := range pages {
					if err := check("table "+name+"'s overflow chain", pg); err != nil {
						return err
					}
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
