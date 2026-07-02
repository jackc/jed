//! Block-device pager — the host-independent storage policy (spec/design/pager.md) above the
//! [`BlockStore`](crate::blockstore) host seam (spec/design/hosts.md §3). It composes a `BlockStore`
//! kept open for the handle's life, expressing the rest of the core's page-level operations
//! (`read_block`/`write_block`/`reserve`/`sync`) over the host's byte device — converting a page index
//! to a byte offset (`index × page_size`), owning the 1 MiB preallocation chunk, and deciding which
//! durability barrier each step needs. The host (the file backing, [`FileBlockStore`](crate::blockstore))
//! is the only per-platform code below; everything here is identical across hosts (hosts.md §1).
//!
//! The whole-image load and the commit route through `read_block`/`write_block`; the bounded buffer
//! pool + lazy node loading that make the resident set bounded (P6.4b) read through this same
//! `read_block`. The **fault-injection seam** (spec/design/storage.md §7) lives here, not in the host
//! — it tests the *commit recipe*, which is host-independent (hosts.md §3).

use crate::blockstore::BlockStore;
use crate::error::{EngineError, Result, SqlState};

/// The **maximum** file-growth step — ~1 MiB worth of pages. The file grows *geometrically*
/// (≈doubling its current size — [`Pager::reserve`]), so a small database's file stays proportional to
/// its data instead of jumping to a fixed 1 MiB; this caps a step so a large database never
/// over-reserves more than ~1 MiB of slack. Real, durably-allocated zero blocks let a steady-state
/// commit write its body into **already-allocated** space, so the per-commit `fdatasync`
/// ([`Pager::sync`]) carries **no** ext4 metadata-journaling for a file-size change — the
/// durable-commit win (spec/design/pager.md §7, TODO.md). Each allocating `fsync` (in
/// [`Pager::reserve`]) amortizes across the pages it reserves.
const PREALLOC_CHUNK_BYTES: u32 = 1024 * 1024;

/// The **minimum** file-growth step — 16 KiB worth of pages. Floors the geometric growth so a fresh
/// load does not `fsync` every page or two while the file is still tiny; above it the doubling does the
/// amortizing. Denominated in bytes (not pages) so it scales with `page_size` like the cap — at a
/// 64 KiB page size it bottoms out at a single page, at 256 B it is 64 pages, reserving the same
/// ~16 KiB either way.
const PREALLOC_FLOOR_BYTES: u32 = 16 * 1024;

/// The preallocation **cap** in **pages** for a file of `page_size` bytes: `max(1, 1 MiB / page_size)`.
fn prealloc_chunk_pages(page_size: u32) -> u32 {
    (PREALLOC_CHUNK_BYTES / page_size.max(1)).max(1)
}

/// The preallocation **floor** in **pages** for a file of `page_size` bytes: `max(1, 16 KiB /
/// page_size)`. Always `≤ prealloc_chunk_pages` (16 KiB ≤ 1 MiB), so the clamp in [`Pager::reserve`]
/// is well-formed.
fn prealloc_floor_pages(page_size: u32) -> u32 {
    (PREALLOC_FLOOR_BYTES / page_size.max(1)).max(1)
}

/// A block device: fixed-size pages addressed by index, over a [`BlockStore`] host kept open for the
/// handle's life. One page at a time (storage.md §2); the demand-paging buffer pool (P6.4b) faults
/// pages in through [`Pager::read_block`].
pub(crate) struct Pager {
    store: Box<dyn BlockStore>,
    page_size: u32,
    /// The number of pages physically **allocated** on disk — the file length in pages, which the
    /// chunked preallocation ([`Pager::reserve`]) runs ahead of the committed high-water. A commit
    /// whose pages all fall below this never grows the file (storage.md §9). Distinct from the
    /// committed logical `page_count` the meta records: the slack pages in `[page_count,
    /// allocated_pages)` are unreferenced trailing zeros (no byte-contract impact — past the
    /// high-water).
    allocated_pages: u32,
    /// The armed one-shot commit fault — the **fault-injection seam** (spec/design/storage.md §7),
    /// `#[cfg(test)]` so it is **entirely absent from a production build** (zero footprint). `None`
    /// unless a test armed one with [`arm_fault`](Pager::arm_fault).
    #[cfg(test)]
    fault: Option<Fault>,
    /// Writes to **body** pages (index ≥ 2) seen since the fault was armed — drives `BodyWrite(n)`.
    #[cfg(test)]
    body_writes: u32,
    /// `sync()` calls seen since the fault was armed — drives `Sync(n)`.
    #[cfg(test)]
    syncs: u32,
}

/// A point in the commit write sequence at which the [fault-injection seam](Pager::arm_fault) can
/// simulate a crash (spec/design/storage.md §7). Pages 0/1 are always the meta slots and every
/// body/catalog page is ≥ 2 (format.md), so `MetaWrite` is identified by the page **index**, never by
/// counting body pages. Testing only.
#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(crate) enum FaultPoint {
    /// The `n`-th write to a **body** page (index ≥ 2), 1-based, counted since the fault was armed —
    /// a clean crash mid-body, before the body `sync()`.
    BodyWrite(u32),
    /// The write to a **meta** slot (index < 2) — the publish, after the body is written and synced
    /// (the critical between-syncs window §4 protects).
    MetaWrite,
    /// The `n`-th `sync()` since the fault was armed (`1` = body barrier, `2` = meta barrier).
    Sync(u32),
}

/// A one-shot crash/tear the pager simulates at a chosen commit point — the **fault-injection seam**
/// (spec/design/storage.md §7). **Testing only** (`#[cfg(test)]`). Not a §8 byte contract: like the
/// buffer pool (pager.md §3), each core realizes it idiomatically; the cross-core contract is the
/// *recovery outcome*, not the mechanism.
#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct Fault {
    point: FaultPoint,
    /// For a write point: write this many **leading** bytes of the page before failing (a torn page);
    /// `None` = write nothing (a clean crash before the page lands). Ignored for `Sync`.
    tear_bytes: Option<usize>,
}

#[cfg(test)]
impl Fault {
    pub(crate) fn new(point: FaultPoint, tear_bytes: Option<usize>) -> Fault {
        Fault { point, tear_bytes }
    }
}

impl Pager {
    /// Adopt an already-open `store` as the byte backing, reading the page size from its meta header
    /// (offset 8, format.md). The host layer (`file.rs`) opens the host — mapping a missing path to
    /// `58P01` — and hands it here wrapped in a [`BlockStore`]. A store smaller than a meta header, or
    /// a zero page size, is `XX001`.
    pub(crate) fn from_store(mut store: Box<dyn BlockStore>) -> Result<Pager> {
        let size = store.size()?;
        if size < 12 {
            return Err(corrupt("database file smaller than a meta header"));
        }
        let header = store.read_at(0, 12)?;
        let page_size = u32::from_be_bytes([header[8], header[9], header[10], header[11]]);
        if page_size == 0 {
            return Err(corrupt("zero page size in meta header"));
        }
        // The allocation high-water is the current file length in pages — already past the committed
        // page_count if a prior session preallocated slack (reused for free on this session's growth).
        let allocated_pages = (size / page_size as u64) as u32;
        Ok(Pager {
            store,
            page_size,
            allocated_pages,
            #[cfg(test)]
            fault: None,
            #[cfg(test)]
            body_writes: 0,
            #[cfg(test)]
            syncs: 0,
        })
    }

    /// Arm a one-shot commit fault (the **fault-injection seam**, spec/design/storage.md §7) and
    /// reset the since-arm counters, so the next commit's body-write / meta-write / `sync()` sequence
    /// triggers it. The fault auto-disarms when it fires. **Testing only.**
    #[cfg(test)]
    pub(crate) fn arm_fault(&mut self, fault: Fault) {
        self.fault = Some(fault);
        self.body_writes = 0;
        self.syncs = 0;
    }

    /// The page size fixed into this file's meta header (format.md) — the block width the demand-
    /// paged loader and fault path read at.
    pub(crate) fn page_size(&self) -> u32 {
        self.page_size
    }

    /// Read one page (block `index`) — random access, the demand-paging read path (P6.4b). Converts
    /// the page index to a byte offset for the host's [`read_at`](BlockStore::read_at).
    pub(crate) fn read_block(&mut self, index: u32) -> Result<Vec<u8>> {
        self.store.read_at(
            index as u64 * self.page_size as u64,
            self.page_size as usize,
        )
    }

    /// Write one page (`bytes`) at block `index`. Overwrites in place — `persist` always
    /// [`reserve`](Pager::reserve)s the high-water first, so the target is already-allocated space
    /// (a reused free page, or a preallocated slot past the old high-water). `bytes` is one page wide.
    pub(crate) fn write_block(&mut self, index: u32, bytes: &[u8]) -> Result<()> {
        self.fault_on_write(index, bytes)?;
        self.store
            .write_at(index as u64 * self.page_size as u64, bytes)
    }

    /// The fault-injection seam's write hook (spec/design/storage.md §7). In a non-test build this is
    /// a no-op (`Ok(())`), so `write_block` carries **zero** fault-injection footprint in production.
    /// Under test, if an armed [`Fault`] targets this write it optionally performs a **torn** partial
    /// write, then disarms and returns an injected-crash error so `persist` aborts mid-commit.
    #[cfg(not(test))]
    #[inline]
    fn fault_on_write(&mut self, _index: u32, _bytes: &[u8]) -> Result<()> {
        Ok(())
    }

    #[cfg(test)]
    fn fault_on_write(&mut self, index: u32, bytes: &[u8]) -> Result<()> {
        let Some(fault) = self.fault else {
            return Ok(());
        };
        let hit = match fault.point {
            FaultPoint::MetaWrite => index < 2,
            FaultPoint::BodyWrite(n) => {
                if index >= 2 {
                    self.body_writes += 1;
                    self.body_writes == n
                } else {
                    false
                }
            }
            FaultPoint::Sync(_) => false,
        };
        if !hit {
            return Ok(());
        }
        self.fault = None; // one-shot
        if let Some(k) = fault.tear_bytes {
            let k = k.min(bytes.len());
            self.store
                .write_at(index as u64 * self.page_size as u64, &bytes[..k])?;
        }
        Err(injected_crash())
    }

    /// Ensure the file has at least `min_pages` physically-allocated pages, growing it *geometrically*
    /// when short: each step adds the current size (≈doubling), floored at [`prealloc_floor_pages`] and
    /// capped at [`prealloc_chunk_pages`] (1 MiB). So a small database's file stays proportional to its
    /// data (no fixed 1 MiB minimum) while a large one still grows in 1 MiB chunks — the physical size
    /// stays bounded by ≈2× the committed high-water. The preallocation *policy* is host-independent and
    /// stays here; the durable grow itself — real zero blocks + a full `fsync` — is the host's
    /// [`set_size`](BlockStore::set_size), the metadata barrier (hosts.md §2.1/§3). Called by `persist`
    /// before each commit's body write with the new committed high-water, so that write — and almost
    /// every commit's — lands entirely in already-allocated space and its data-only [`sync`](Pager::sync)
    /// pays no metadata journaling (spec/design/pager.md §7). Crash-safe: the preallocated pages are
    /// unreferenced zeros past the committed `page_count`, so a crash before the next commit publishes
    /// simply ignores them.
    pub(crate) fn reserve(&mut self, min_pages: u32) -> Result<()> {
        if min_pages <= self.allocated_pages {
            return Ok(());
        }
        let floor = prealloc_floor_pages(self.page_size);
        let cap = prealloc_chunk_pages(self.page_size);
        let mut target = self.allocated_pages;
        while target < min_pages {
            // ≈double, clamped to [floor, cap]; saturate rather than wrap at the u32 page ceiling.
            target = target.saturating_add(target.clamp(floor, cap));
        }
        self.store.set_size(target as u64 * self.page_size as u64)?;
        self.allocated_pages = target;
        Ok(())
    }

    /// Metadata-free durability barrier — the host's data-only [`sync`](BlockStore::sync)
    /// (`fdatasync`). Called twice per commit — body pages, then the meta — to honour the
    /// body-before-meta write-ordering rule (format.md, file.rs `persist`). Data-only, not a full
    /// `fsync`, so an overwrite into the preallocated region ([`Pager::reserve`]) flushes only the
    /// data, never a file-size/inode-timestamp metadata journal (spec/design/pager.md §7).
    pub(crate) fn sync(&mut self) -> Result<()> {
        self.fault_on_sync()?;
        self.store.sync()
    }

    /// The fault-injection seam's `sync()` hook (spec/design/storage.md §7) — a no-op in a non-test
    /// build. Under test, fails the `n`-th `sync()` since arming **before flushing** (the data is
    /// already written-through; only the durability barrier is skipped), then disarms.
    #[cfg(not(test))]
    #[inline]
    fn fault_on_sync(&mut self) -> Result<()> {
        Ok(())
    }

    #[cfg(test)]
    fn fault_on_sync(&mut self) -> Result<()> {
        if let Some(Fault {
            point: FaultPoint::Sync(n),
            ..
        }) = self.fault
        {
            self.syncs += 1;
            if self.syncs == n {
                self.fault = None; // one-shot
                return Err(injected_crash());
            }
        }
        Ok(())
    }
}

fn corrupt(msg: &str) -> EngineError {
    EngineError::new(SqlState::DataCorrupted, msg)
}

/// The error an armed [`Fault`] returns to abort `persist` mid-commit — a simulated crash, reported as
/// an ordinary I/O failure so the commit path rolls back exactly as a real write error would.
#[cfg(test)]
fn injected_crash() -> EngineError {
    EngineError::new(SqlState::IoError, "injected commit crash (fault injection)")
}
