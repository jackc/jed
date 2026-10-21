//! Backup (spec/design/page-service.md §9; spec/pageservice/README.md §6.6): a consistent copy of the
//! main database's committed snapshot, written to a new file, in one of two forms.
//!
//! - Page-identical: every page reachable from the snapshot (the catalog chain, every table and index
//!   B+tree, every GiST R-tree, every live overflow chain) and its free-list chain, each copied at its
//!   own index; every other index below the snapshot's page count is zero; both meta slots carry the
//!   snapshot's bootstrap meta (zero manifest fields). The copy keeps the page numbering and txid.
//! - Compacted: the from-scratch image of the snapshot at txid + 1, the file host compaction
//!   (api.md §2.6) would leave for it.
//!
//! The backup pins the snapshot as a reader does and walks it with this process's own engine, so it
//! works the same in every role: an owner or a handle outside the page service reads its own file, a
//! page client reads through the owner with read_pages. It never takes the writer gate.

use super::*;

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::PathBuf;

use crate::filehost::{FileHost, OsFileHost, WriteAt};
use crate::format::{LoadAt, Meta, PAGE_FREELIST, ROOT_PAGE};
use crate::paging::SharedPaging;

/// Bounds the re-pins a backup makes when later commits overwrite its pinned version's meta slot
/// between the pin and the read (each retry pins a newer version).
const BACKUP_META_RETRIES: usize = 1000;

impl Database {
    /// Back up database `main` to `dest`, which must not exist: page-identical when `page_identical`
    /// is set, compacted otherwise. The host API's name and shape are not yet decided
    /// (spec/design/page-service.md §9); this test-only entry point drives the shared implementation.
    #[doc(hidden)]
    pub fn backup_for_testing(&self, dest: impl AsRef<Path>, page_identical: bool) -> Result<()> {
        self.0.backup(dest.as_ref(), page_identical)
    }

    /// The page-identical backup set of the newest committed version of `main`, ascending, and the
    /// version: the pages a page-identical backup taken now copies (tests/backup.rs).
    #[cfg(test)]
    pub(crate) fn backup_pages_for_testing(&self) -> Result<(Vec<u32>, u64)> {
        let (snap, _, v, generation) = self.0.refresh_and_pin()?;
        let result = (|| {
            let paging = backup_paging(&snap)?;
            let mt = backup_meta(&paging, v)?
                .ok_or_else(|| EngineError::new(SqlState::ObjectInUse, "the meta moved"))?;
            self.0.backup_pages(&paging, &mt)
        })();
        self.0.unpin(generation, v);
        result.map(|pages| (pages, v))
    }
}

/// The paging context a pinned snapshot's pages are read through.
fn backup_paging(snap: &Snapshot) -> Result<Arc<SharedPaging>> {
    let paging = snap.store_paging().cloned().ok_or_else(|| {
        EngineError::new(
            SqlState::FeatureNotSupported,
            "a backup needs a database with storage",
        )
    })?;
    Ok(paging)
}

/// The meta that published version `v`. A commit writes the slot that does not hold the durable floor
/// (validated-cow.md §4): `txid & 1` under `synchronous = on`, so a commit two versions on can overwrite
/// it after the pin, and one slot for a whole relaxed batch, so the next commit of the batch can. Either
/// slot may hold it, and `None` means a later commit overwrote it.
fn backup_meta(paging: &SharedPaging, v: u64) -> Result<Option<Meta>> {
    let mut pager = paging.pager();
    for slot in [(v & 1) as u32, (v & 1) as u32 ^ 1] {
        let block = pager.read_block(slot)?;
        if let Some(m) = crate::format::parse_meta(&block)
            && m.txid == v
        {
            return Ok(Some(m));
        }
    }
    Ok(None)
}

fn corrupt(message: &str) -> EngineError {
    EngineError::new(SqlState::DataCorrupted, message)
}

impl Shared {
    fn unpin(&self, generation: u64, v: u64) {
        self.live
            .lock()
            .expect("live lock not poisoned")
            .deregister_in(generation, v);
    }

    /// Whether this handle is an in-memory database (no file, not under the page service).
    fn backup_in_memory(&self) -> bool {
        #[cfg(unix)]
        if self.handoff.get().is_some() {
            return false;
        }
        self.storage
            .lock()
            .expect("storage lock not poisoned")
            .path
            .is_none()
    }

    pub(crate) fn backup(&self, dest: &Path, page_identical: bool) -> Result<()> {
        let (files, page_size) = {
            let st = self.storage.lock().expect("storage lock not poisoned");
            let files: Arc<dyn FileHost> = match &st.files {
                Some(files) => Arc::clone(files),
                None => Arc::new(OsFileHost),
            };
            (files, st.page_size)
        };
        if page_identical && self.backup_in_memory() {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "a page-identical backup needs a file database; an in-memory database has only the compacted form",
            ));
        }
        if files.exists(dest)? {
            return Err(EngineError::new(
                SqlState::DuplicateFile,
                format!("database file already exists: {}", dest.display()),
            ));
        }
        let mut attempt = 0;
        loop {
            let (snap, _, v, generation) = self.refresh_and_pin()?;
            let result = self.backup_pinned(&*files, dest, page_identical, page_size, &snap, v);
            self.unpin(generation, v);
            match result {
                Ok(true) => return Ok(()),
                Ok(false) => {
                    // A later commit overwrote its meta slot: pin the newer version.
                    if attempt >= BACKUP_META_RETRIES {
                        return Err(EngineError::new(
                            SqlState::ObjectInUse,
                            "could not read a stable meta page for the backup",
                        ));
                    }
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Back up the pinned snapshot `snap` at version `v`; `false` when its meta is gone (re-pin).
    fn backup_pinned(
        &self,
        files: &dyn FileHost,
        dest: &Path,
        page_identical: bool,
        page_size: u32,
        snap: &Snapshot,
        v: u64,
    ) -> Result<bool> {
        let paging = backup_paging(snap)?;
        #[allow(unused_mut)]
        let mut no_sync = paging.pager().skips_sync();
        #[cfg(unix)]
        if let Some(handoff) = self.handoff.get() {
            no_sync = no_sync || handoff.opts.skip_fsync;
        }
        if page_identical {
            let Some(mt) = backup_meta(&paging, v)? else {
                return Ok(false);
            };
            let pages = self.backup_pages(&paging, &mt)?;
            write_backup_file(files, dest, no_sync, &mut |write_at| {
                self.write_page_identical(&paging, &mt, &pages, page_size, write_at)
            })?;
        } else {
            // An encrypted database's copy keeps its data key and the file's selected key-block
            // copy, as the page-identical form does — a page client's codec still holds the copy it
            // unlocked from, which a credential change since has replaced — every page resealed
            // under fresh nonces, as compaction does (encrypted.md §13).
            let codec = match paging.codec() {
                Some(codec) => Some(codec.with_key_block(self.current_key_block(&paging)?)),
                None => None,
            };
            write_backup_file(files, dest, no_sync, &mut |write_at| {
                snap.write_image(page_size, v + 1, codec.as_ref(), &mut |index, page| {
                    write_at(index as u64 * page_size as u64, page)
                })
                .map(|_| ())
            })?;
        }
        Ok(true)
    }

    /// The selected copy of the key-block pair the file holds now (pages 2 and 3, read through its
    /// pager, so through the service page by page for a page client; encrypted.md §5), as its 128
    /// key-block bytes.
    fn current_key_block(&self, paging: &SharedPaging) -> Result<Vec<u8>> {
        let pair = read_local(paging, &[2, 3])?;
        let page = crate::crypto::selected_key_page(&pair[0], &pair[1])?;
        Ok(page[..crate::crypto::KEY_BLOCK_LEN].to_vec())
    }

    /// The page-identical backup's page set (page-service.md §9), ascending: the pages reachable from
    /// `mt`'s root — the catalog chain, every table and index B+tree node, every GiST R-tree page,
    /// every live overflow chain page — and `mt`'s free-list chain. The reachable count must equal the
    /// meta's live page count (format.md *Live page count*); a mismatch is corruption.
    fn backup_pages(&self, paging: &Arc<SharedPaging>, mt: &Meta) -> Result<Vec<u32>> {
        let gist: RefCell<HashSet<u32>> = RefCell::new(HashSet::new());
        // The catalog and the interior skeleton are loaded the way an adoption loads them, under the
        // storage lock that keeps the load from interleaving with a local commit into the same store.
        let loaded = {
            let _storage = self.storage.lock().expect("storage lock not poisoned");
            let hook = |p: u32| {
                gist.borrow_mut().insert(p);
            };
            Engine::open_shared_paging_at(
                Arc::clone(paging),
                &self.env,
                LoadAt {
                    meta: Some(mt),
                    gist: Some(&hook),
                },
            )?
        };
        let mut reached = gist.into_inner();
        let snap = &loaded.committed;
        let read = |p: crate::format::PageRef| paging.read_body(p);
        let mut seen: HashSet<u32> = HashSet::new();
        // Chains are followed through their bound pointers (encrypted.md §2), so every chain page of
        // an encrypted file is authenticated on the way.
        let cap = paging.cap();
        let first = paging
            .codec()
            .map_or(ROOT_PAGE, |_| crate::crypto::FIRST_BODY_PAGE);
        let mut p = crate::format::PageRef {
            page: mt.root_page,
            tag: mt.root_tag,
        };
        while p.page != 0 {
            if !seen.insert(p.page) || p.page < first || p.page >= mt.page_count {
                return Err(corrupt("invalid catalog chain"));
            }
            reached.insert(p.page);
            let block = read(p)?;
            p = crate::format::chain_step(&block, cap)?.1;
        }
        for store in snap.stores_by_name() {
            let Some(root) = store.tree_root() else {
                continue;
            };
            crate::format::collect_tree_pages(root, &mut reached);
            let root_page = root.page_ref();
            let col_types = Arc::new(store.col_types().to_vec());
            if root_page.page != 0 && crate::format::any_spillable(&col_types) {
                crate::format::collect_leaf_overflow_with(
                    &read,
                    paging.cap(),
                    root_page,
                    &col_types,
                    &mut reached,
                    &mut HashSet::new(),
                    1,
                )?;
            }
        }
        for istore in snap.index_stores_iter() {
            if let Some(root) = istore.tree_root() {
                crate::format::collect_tree_pages(root, &mut reached);
            }
        }
        if reached.len() as u64 != mt.live_pages as u64 {
            return Err(corrupt(
                "the pages reachable from the snapshot do not match its live page count",
            ));
        }
        let mut p = crate::format::PageRef {
            page: mt.free_list_head,
            tag: mt.free_list_tag,
        };
        while p.page != 0 {
            if reached.contains(&p.page) || p.page < first || p.page >= mt.page_count {
                return Err(corrupt("invalid free-list chain"));
            }
            reached.insert(p.page);
            let block = read(p)?;
            let (page_type, next) = crate::format::chain_step(&block, cap)?;
            if page_type != PAGE_FREELIST {
                return Err(corrupt("invalid free-list page"));
            }
            p = next;
        }
        let mut pages: Vec<u32> = Vec::with_capacity(reached.len());
        for p in reached {
            if p < first || p >= mt.page_count {
                return Err(corrupt("a reachable page lies outside the snapshot"));
            }
            pages.push(p);
        }
        pages.sort_unstable();
        Ok(pages)
    }

    /// Write the page-identical image: each page in `pages` copied at its index (a page client reads
    /// them in batches), every other index left zero, and the bootstrap meta in both slots.
    fn write_page_identical(
        &self,
        paging: &SharedPaging,
        mt: &Meta,
        pages: &[u32],
        page_size: u32,
        write_at: &mut WriteAt<'_>,
    ) -> Result<()> {
        let ps = page_size as u64;
        #[cfg(unix)]
        let client = self.page_client();
        #[cfg(unix)]
        let batch_size = client.as_ref().map_or(1, |cl| cl.max_read_pages().max(1));
        #[cfg(not(unix))]
        let batch_size = 1;
        for batch in pages.chunks(batch_size) {
            #[cfg(unix)]
            let blocks = match &client {
                Some(cl) => cl.read_pages(batch)?,
                None => read_local(paging, batch)?,
            };
            #[cfg(not(unix))]
            let blocks = read_local(paging, batch)?;
            for (index, block) in batch.iter().zip(&blocks) {
                // An encrypted page is ciphertext: the walk authenticated the chain pages and every
                // interior node, and a leaf or overflow page is authenticated when the copy reads it.
                if paging.codec().is_none() {
                    crate::format::page_header(block)?;
                }
                write_at(*index as u64 * ps, block)?;
            }
        }
        // An encrypted copy holds the key block at pages 2 and 3, which no pointer reaches: the
        // file's selected copy, written to both, so the pair is complete at one generation even while
        // a credential change runs (encrypted.md §13). A page client reads the pages through the
        // service; selection already tolerates a change in progress, which never leaves both
        // copies invalid.
        if paging.codec().is_some() {
            #[cfg(unix)]
            let pair = match &client {
                Some(cl) => cl.read_pages(&[2, 3])?,
                None => read_local(paging, &[2, 3])?,
            };
            #[cfg(not(unix))]
            let pair = read_local(paging, &[2, 3])?;
            let block = crate::crypto::selected_key_page(&pair[0], &pair[1])?;
            write_at(2 * ps, &block)?;
            write_at(3 * ps, &block)?;
        }
        let last = mt.page_count - 1;
        if pages.last() != Some(&last) {
            // The file must span the snapshot's page count: its trailing dead pages are zero.
            write_at(last as u64 * ps, &vec![0u8; page_size as usize])?;
        }
        let bootstrap = crate::format::meta_page(
            paging.cap(),
            page_size,
            mt.txid,
            crate::format::PageRef {
                page: mt.root_page,
                tag: mt.root_tag,
            },
            mt.page_count,
            crate::format::PageRef {
                page: mt.free_list_head,
                tag: mt.free_list_tag,
            },
            mt.live_pages,
        );
        // Each slot is sealed for its own index in an encrypted copy (encrypted.md §6).
        write_at(0, &crate::format::seal_meta(paging, 0, &bootstrap, &[])?)?;
        write_at(ps, &crate::format::seal_meta(paging, 1, &bootstrap, &[])?)
    }
}

/// Read `batch` through this handle's own pager.
fn read_local(paging: &SharedPaging, batch: &[u32]) -> Result<Vec<Vec<u8>>> {
    batch
        .iter()
        .map(|&p| paging.pager().read_block(p))
        .collect()
}

/// Write `dest` the way create writes a new database (api.md §2.1): a sibling temp file, filled and
/// synced, renamed into place, and the directory synced. A failure removes the temp file.
fn write_backup_file(
    files: &dyn FileHost,
    dest: &Path,
    no_sync: bool,
    fill: &mut dyn FnMut(&mut WriteAt<'_>) -> Result<()>,
) -> Result<()> {
    let mut tmp = dest.as_os_str().to_os_string();
    tmp.push(".jedtmp");
    let tmp = PathBuf::from(tmp);
    if let Err(e) = files.write_file(&tmp, None, no_sync, fill) {
        files.remove(&tmp);
        return Err(e);
    }
    if let Err(e) = files.rename(&tmp, dest) {
        files.remove(&tmp);
        return Err(e);
    }
    if !no_sync {
        let dir = match dest.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir,
            _ => Path::new("."),
        };
        files.sync_dir(dir);
    }
    Ok(())
}
