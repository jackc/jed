package jed

// Backup (spec/design/page-service.md §9; spec/pageservice/README.md §6.6): a consistent copy of the
// main database's committed snapshot, written to a new file, in one of two forms.
//
//   - Page-identical: every page reachable from the snapshot (the catalog chain, every table and index
//     B+tree, every GiST R-tree, every live overflow chain) and its free-list chain, each copied at its
//     own index; every other index below the snapshot's page count is zero; both meta slots carry the
//     snapshot's bootstrap meta (zero manifest fields). The copy keeps the page numbering and txid.
//   - Compacted: the from-scratch image of the snapshot at txid + 1, the file host compaction
//     (api.md §2.6) would leave for it.
//
// The backup pins the snapshot as a reader does and walks it with this process's own engine, so it
// works the same in every role: an owner or a handle outside the page service reads its own file, a
// page client reads through the owner with read_pages. It never takes the writer gate.

import (
	"path/filepath"
	"sort"
)

// BackupForTesting backs up database main to dest, which must not exist: page-identical when
// pageIdentical is set, compacted otherwise. The host API's name and shape are not yet decided
// (spec/design/page-service.md §9); this test-only entry point drives the shared implementation.
func BackupForTesting(db *Database, dest string, pageIdentical bool) error {
	return db.core.backup(dest, pageIdentical)
}

// backupMetaRetries bounds the re-pins a page-identical backup makes when two commits overwrite its
// pinned version's meta slot between the pin and the read (each retry pins a newer version).
const backupMetaRetries = 1000

func (c *sharedCore) backup(dest string, pageIdentical bool) error {
	st := c.storage
	files := st.fileHost()
	if pageIdentical && st.path == "" && c.handoff == nil {
		return newError(FeatureNotSupported, "a page-identical backup needs a file database; an in-memory database has only the compacted form")
	}
	exists, err := files.exists(dest)
	if err != nil {
		return err
	}
	if exists {
		return newError(DuplicateFile, "database file already exists: "+dest)
	}
	for attempt := 0; ; attempt++ {
		rt, v, gen, err := c.refreshAndPin()
		if err != nil {
			return err
		}
		snap := rt.committed
		paging := snap.storePaging
		noSync := c.handoff != nil && c.handoff.opts.SkipFsync
		_ = paging.withPager(func(p *pager) error { noSync = noSync || p.skipsSync(); return nil })
		var fill func(writeAt func(off int64, p []byte) error) error
		if pageIdentical {
			mt, found, err := backupMeta(paging, v)
			if err != nil {
				c.deregisterPin(gen, v)
				return err
			}
			if !found {
				// Two commits after the pin overwrote its meta slot: pin the newer version instead.
				c.deregisterPin(gen, v)
				if attempt >= backupMetaRetries {
					return newError(ObjectInUse, "could not read a stable meta page for the backup")
				}
				continue
			}
			pages, err := c.backupPages(paging, mt)
			if err != nil {
				c.deregisterPin(gen, v)
				return err
			}
			fill = func(writeAt func(off int64, p []byte) error) error {
				return c.writePageIdentical(paging, mt, pages, writeAt)
			}
		} else {
			fill = func(writeAt func(off int64, p []byte) error) error {
				_, err := snap.writeImage(st.pageSize, v+1, func(index uint32, page []byte) error {
					return writeAt(int64(index)*int64(st.pageSize), page)
				})
				return err
			}
		}
		err = writeBackupFile(files, dest, noSync, fill)
		c.deregisterPin(gen, v)
		return err
	}
}

// backupMeta reads the meta that published version v. Every commit writes slot txid & 1, so a later
// commit two versions on can overwrite it after the pin: found=false then.
func backupMeta(paging *sharedPaging, v uint64) (meta, bool, error) {
	var mt meta
	found := false
	err := paging.withPager(func(p *pager) error {
		for _, slot := range []uint32{uint32(v & 1), uint32(v&1) ^ 1} {
			block, err := p.readBlock(slot)
			if err != nil {
				return err
			}
			if m, ok := parseMeta(block); ok && m.txid == v {
				mt, found = m, true
				return nil
			}
		}
		return nil
	})
	return mt, found, err
}

// backupPages is the page-identical backup's page set (page-service.md §9), ascending: the pages
// reachable from mt's root — the catalog chain, every table and index B+tree node, every GiST R-tree
// page, every live overflow chain page — and mt's free-list chain. The reachable count must equal the
// meta's live page count (format.md *Live page count*); a mismatch is corruption.
func (c *sharedCore) backupPages(paging *sharedPaging, mt meta) ([]uint32, error) {
	reached := map[uint32]bool{}
	// The catalog and the interior skeleton are loaded the way an adoption loads them, under the
	// storage lock that keeps the load from interleaving with a local commit into the same store.
	c.storage.mu.Lock()
	loaded, err := loadEngineSharedPagingAt(paging, c.env, loadAt{meta: &mt, gist: func(p uint32) { reached[p] = true }})
	c.storage.mu.Unlock()
	if err != nil {
		return nil, err
	}
	snap := loaded.committed
	read := paging.readBlock
	seen := map[uint32]bool{}
	for p := mt.rootPage; p != 0; {
		if seen[p] || p < rootPage || p >= mt.pageCount {
			return nil, newError(DataCorrupted, "invalid catalog chain")
		}
		seen[p] = true
		reached[p] = true
		block, err := read(p)
		if err != nil {
			return nil, err
		}
		pg, err := parsePage(block)
		if err != nil {
			return nil, err
		}
		p = pg.nextPage
	}
	names := make([]string, 0, len(snap.stores))
	for name := range snap.stores {
		names = append(names, name)
	}
	sort.Strings(names)
	for _, name := range names {
		store := snap.stores[name]
		root := store.treeRoot()
		collectTreePages(root, reached)
		if root != nil && root.page != 0 && anySpillable(store.colTypes) {
			if err := collectLeafOverflowWith(read, root.page, store.colTypes, reached, map[uint32]bool{}, 1); err != nil {
				return nil, err
			}
		}
	}
	for _, istore := range snap.indexStores {
		collectTreePages(istore.treeRoot(), reached)
	}
	if uint32(len(reached)) != mt.livePages {
		return nil, newError(DataCorrupted, "the pages reachable from the snapshot do not match its live page count")
	}
	for p := mt.freeListHead; p != 0; {
		if reached[p] || p < rootPage || p >= mt.pageCount {
			return nil, newError(DataCorrupted, "invalid free-list chain")
		}
		reached[p] = true
		block, err := read(p)
		if err != nil {
			return nil, err
		}
		pg, err := parsePage(block)
		if err != nil {
			return nil, err
		}
		if pg.pageType != pageFreelist {
			return nil, newError(DataCorrupted, "invalid free-list page")
		}
		p = pg.nextPage
	}
	pages := make([]uint32, 0, len(reached))
	for p := range reached {
		if p < rootPage || p >= mt.pageCount {
			return nil, newError(DataCorrupted, "a reachable page lies outside the snapshot")
		}
		pages = append(pages, p)
	}
	sort.Slice(pages, func(i, j int) bool { return pages[i] < pages[j] })
	return pages, nil
}

// writePageIdentical writes the page-identical image: each page in pages copied at its index (a page
// client reads them in batches), every other index left zero, and the bootstrap meta in both slots.
func (c *sharedCore) writePageIdentical(paging *sharedPaging, mt meta, pages []uint32, writeAt func(off int64, p []byte) error) error {
	ps := int64(c.storage.pageSize)
	read := func(batch []uint32) ([][]byte, error) {
		out := make([][]byte, len(batch))
		for i, p := range batch {
			block, err := paging.readBlock(p)
			if err != nil {
				return nil, err
			}
			out[i] = block
		}
		return out, nil
	}
	batchSize := 1
	if cl := c.pageClient(); cl != nil {
		read = cl.readPages
		batchSize = cl.maxReadPages()
	}
	for start := 0; start < len(pages); start += batchSize {
		batch := pages[start:min(start+batchSize, len(pages))]
		blocks, err := read(batch)
		if err != nil {
			return err
		}
		for i, block := range blocks {
			if _, err := parsePage(block); err != nil {
				return err
			}
			if err := writeAt(int64(batch[i])*ps, block); err != nil {
				return err
			}
		}
	}
	if last := mt.pageCount - 1; len(pages) == 0 || pages[len(pages)-1] != last {
		// The file must span the snapshot's page count: its trailing dead pages are zero.
		if err := writeAt(int64(last)*ps, make([]byte, ps)); err != nil {
			return err
		}
	}
	bootstrap := metaPage(uint32(ps), mt.txid, mt.rootPage, mt.pageCount, mt.freeListHead, mt.livePages)
	if err := writeAt(0, bootstrap); err != nil {
		return err
	}
	return writeAt(ps, bootstrap)
}

// writeBackupFile writes dest the way create writes a new database (api.md §2.1): a sibling temp file,
// filled and synced, renamed into place, and the directory synced. A failure removes the temp file.
func writeBackupFile(files fileHost, dest string, noSync bool, fill func(writeAt func(off int64, p []byte) error) error) error {
	tmp := dest + ".jedtmp"
	if err := files.writeFile(tmp, "", noSync, fill); err != nil {
		files.remove(tmp)
		return err
	}
	if err := files.rename(tmp, dest); err != nil {
		files.remove(tmp)
		return err
	}
	if !noSync {
		files.syncDir(filepath.Dir(dest))
	}
	return nil
}
