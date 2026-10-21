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

// backupMetaRetries bounds the re-pins a page-identical backup makes when later commits overwrite its
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
				// A later commit overwrote its meta slot: pin the newer version instead.
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
			// An encrypted database's copy keeps its data key and key block, every page resealed under
			// fresh nonces, as compaction does (spec/fileformat/encrypted.md §13).
			var codec *pageCodec
			if paging != nil {
				codec = paging.codec()
			}
			fill = func(writeAt func(off int64, p []byte) error) error {
				_, err := snap.writeImage(st.pageSize, v+1, codec, func(index uint32, page []byte) error {
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

// backupMeta reads the meta that published version v. A commit writes the slot that does not hold the
// durable floor (validated-cow.md §4): txid & 1 under synchronous = on, so a commit two versions on can
// overwrite it after the pin, and one slot for a whole relaxed batch, so the next commit of the batch
// can. Either slot may hold it, and found=false means a later commit overwrote it.
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
	// Chains are followed through their bound pointers (encrypted.md §2), so every chain page of an
	// encrypted file is authenticated on the way.
	readRef := func(r pageRef) ([]byte, error) { return paging.readBody(r) }
	pcap := paging.cap()
	first := pcap.firstBody()
	seen := map[uint32]bool{}
	for p := (pageRef{page: mt.rootPage, tag: mt.rootTag}); p.page != 0; {
		if seen[p.page] || p.page < first || p.page >= mt.pageCount {
			return nil, newError(DataCorrupted, "invalid catalog chain")
		}
		seen[p.page] = true
		reached[p.page] = true
		block, err := readRef(p)
		if err != nil {
			return nil, err
		}
		if _, p, err = chainStep(block, pcap); err != nil {
			return nil, err
		}
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
			if err := collectLeafOverflowWith(readRef, paging.cap(), root.ref(), store.colTypes, reached, map[uint32]bool{}, 1); err != nil {
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
	for p := (pageRef{page: mt.freeListHead, tag: mt.freeListTag}); p.page != 0; {
		if reached[p.page] || p.page < first || p.page >= mt.pageCount {
			return nil, newError(DataCorrupted, "invalid free-list chain")
		}
		reached[p.page] = true
		block, err := readRef(p)
		if err != nil {
			return nil, err
		}
		var pageType byte
		if pageType, p, err = chainStep(block, pcap); err != nil {
			return nil, err
		}
		if pageType != pageFreelist {
			return nil, newError(DataCorrupted, "invalid free-list page")
		}
	}
	pages := make([]uint32, 0, len(reached))
	for p := range reached {
		if p < first || p >= mt.pageCount {
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
	encrypted := paging.codec() != nil
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
			// An encrypted page is ciphertext: the walk authenticated the chain pages and every
			// interior node, and a leaf or overflow page is authenticated when the copy reads it.
			if !encrypted {
				if _, err := parsePage(block); err != nil {
					return err
				}
			}
			if err := writeAt(int64(batch[i])*ps, block); err != nil {
				return err
			}
		}
	}
	// An encrypted copy holds the key block at pages 2 and 3, which no pointer reaches: the file's
	// selected copy, written to both, so the pair is complete at one generation even while a credential
	// change runs (encrypted.md §13). A page client reads the pages through the service; selection
	// already tolerates a change in progress, which never leaves both copies invalid.
	if encrypted {
		pair, err := read([]uint32{2, 3})
		if err != nil {
			return err
		}
		block, err := selectKeyBlock(pair[0], pair[1])
		if err != nil {
			return err
		}
		page := make([]byte, ps)
		copy(page, block)
		if err := writeAt(2*ps, page); err != nil {
			return err
		}
		if err := writeAt(3*ps, page); err != nil {
			return err
		}
	}
	if last := mt.pageCount - 1; len(pages) == 0 || pages[len(pages)-1] != last {
		// The file must span the snapshot's page count: its trailing dead pages are zero.
		if err := writeAt(int64(last)*ps, make([]byte, ps)); err != nil {
			return err
		}
	}
	bootstrap := metaPage(paging.cap(), uint32(ps), mt.txid, pageRef{page: mt.rootPage, tag: mt.rootTag}, mt.pageCount, pageRef{page: mt.freeListHead, tag: mt.freeListTag}, mt.livePages)
	// Each slot is sealed for its own index in an encrypted copy (encrypted.md §6).
	for slot := uint32(0); slot < 2; slot++ {
		m := append([]byte(nil), bootstrap...)
		if encrypted {
			if err := paging.codec().sealMeta(slot, m, nil); err != nil {
				return err
			}
		}
		if err := writeAt(int64(slot)*ps, m); err != nil {
			return err
		}
	}
	return nil
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
