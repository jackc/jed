//! Host file layer for the Rust core (spec/design/api.md §2): open/create/commit/close a
//! single-file database durably. Pure `std::fs` — no dependencies, fully memory-safe (CLAUDE.md
//! §13). `create` lays down the from-scratch image (temp-file + fsync + atomic rename + directory
//! fsync, api.md §3); every later commit is an **incremental** copy-on-write write of just the dirty
//! pages, published by alternating the meta slot (spec/fileformat/format.md, P6.1 part B) — the
//! block seam below pwrites pages into the open file rather than rewriting it.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::{EngineError, Result, SqlState};
use crate::executor::{DEFAULT_PAGE_SIZE, Database, Snapshot};
use crate::pager::Pager;
use crate::paging::{DEFAULT_CACHE_BYTES, SharedPaging, cache_leaves};

/// Settings for a newly-created database file (spec/design/api.md §2). `page_size` is fixed
/// into the file's meta at creation and cannot change thereafter.
#[derive(Clone, Copy, Debug)]
pub struct DatabaseOptions {
    pub page_size: u32,
}

impl Default for DatabaseOptions {
    fn default() -> Self {
        DatabaseOptions {
            page_size: DEFAULT_PAGE_SIZE,
        }
    }
}

/// Open-time settings for a file-backed database (spec/design/api.md §2.1). Unlike
/// [`DatabaseOptions`] (create-time, fixed into the file), these are **handle** settings — not stored
/// in the file, so a different host may reopen the same file with different ones.
#[derive(Clone, Copy, Debug)]
pub struct OpenOptions {
    /// The buffer-pool budget **in bytes**: roughly the maximum memory the resident leaf cache holds
    /// at once (spec/design/pager.md §3, P6.4b/c). Bytes, not a page count, so the budget does not
    /// silently scale with the file's page size; the engine converts it to a leaf-page capacity by the
    /// file's page size as `max(1, cache_bytes / page_size)` ([`cache_leaves`](crate::paging)). The
    /// bound that lets a database far larger than RAM be served (pager.md §1); it never changes what a
    /// query observes (§3/§5). Default [`DEFAULT_CACHE_BYTES`](crate::paging) (256 MiB).
    pub cache_bytes: usize,
    /// Open the file **read-only** (api.md §2.1). The handle then behaves like PostgreSQL hot
    /// standby: every transaction defaults to READ ONLY, an explicit READ WRITE request and any
    /// write statement are `25006`, and the file is opened without write access, so it is never
    /// written (works on a read-only filesystem). Default `false`.
    pub read_only: bool,
}

impl Default for OpenOptions {
    fn default() -> Self {
        OpenOptions {
            cache_bytes: DEFAULT_CACHE_BYTES,
            read_only: false,
        }
    }
}

impl Database {
    /// Create a **new** file-backed database at `path` with `opts` (the page size is locked into
    /// the file). The path must not already exist — `58P02` otherwise. An initial empty image is
    /// written durably immediately, so the file exists with its page size fixed (api.md §2).
    pub fn create<P: AsRef<Path>>(path: P, opts: DatabaseOptions) -> Result<Database> {
        let path = path.as_ref();
        if path.exists() {
            return Err(EngineError::new(
                SqlState::DuplicateFile,
                format!("database file already exists: {}", path.display()),
            ));
        }
        let mut db = Database::new();
        db.path = Some(path.to_path_buf());
        db.page_size = opts.page_size;
        db.committed.txid = 1; // the initial empty image is committed as txid 1
        db.write_full_image()?; // lay down the from-scratch image; later commits are incremental
        // Adopt the just-written file as the open pager + buffer pool, so later commits write through
        // the seam without re-opening (spec/design/pager.md). A freshly-created database has no rows,
        // so nothing is `OnDisk` yet — tables built in this session stay resident until a reopen
        // demand-pages them.
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(io_error)?;
        db.paging = Some(SharedPaging::new(
            Pager::from_file(file)?,
            cache_leaves(DEFAULT_CACHE_BYTES, db.page_size),
        ));
        Ok(db)
    }

    /// Open an **existing** file-backed database at `path` with default open settings — the buffer-pool
    /// budget defaults to [`DEFAULT_CACHE_BYTES`](crate::paging) (256 MiB). See
    /// [`open_with_options`](Database::open_with_options) to set the budget.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Database> {
        Database::open_with_options(path, OpenOptions::default())
    }

    /// Open an **existing** file-backed database at `path` with explicit open settings (the memory
    /// budget, [`OpenOptions::cache_bytes`]). Loads its committed state, adopting its page size / txid.
    /// The path must exist — `58P01` otherwise; a malformed file is `XX001`, a read failure `58030`
    /// (api.md §2.1).
    ///
    /// The demand-paged loader builds only the interior B-tree skeleton resident, faulting each leaf
    /// through the bounded buffer pool on access, so the resident set is bounded by the pool — not the
    /// file size (P6.4b, spec/design/pager.md). The byte budget is converted to a leaf-page capacity by
    /// the file's page size (`cache_leaves`). The budget is a **handle** setting, not stored in the file
    /// (§3). Later commits write through the same pager kept open for the handle's life.
    pub fn open_with_options<P: AsRef<Path>>(path: P, opts: OpenOptions) -> Result<Database> {
        let path = path.as_ref();
        // A read-only open never writes the file, so it is not opened for writing at all —
        // the OS enforces what the executor's 25006 guards promise (api.md §2.1).
        let file = match fs::OpenOptions::new()
            .read(true)
            .write(!opts.read_only)
            .open(path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(EngineError::new(
                    SqlState::UndefinedFile,
                    format!("database file does not exist: {}", path.display()),
                ));
            }
            Err(e) => return Err(io_error(e)),
        };
        let pager = Pager::from_file(file)?;
        // Convert the byte budget to a leaf-page capacity by the file's page size; `open_paged`
        // rejects an out-of-range page size as corrupt (`cache_leaves` clamps the divisor so a
        // malformed `page_size = 0` cannot divide by zero before that check runs).
        let capacity = cache_leaves(opts.cache_bytes, pager.page_size());
        let mut db = Database::open_paged(pager, capacity)?;
        db.path = Some(path.to_path_buf());
        db.read_only = opts.read_only;
        Ok(db)
    }

    /// The number of leaf pages currently resident in the buffer pool — `0` for an in-memory database
    /// (it is fully resident, nothing to page). The read-only gauge the [`OpenOptions::cache_bytes`]
    /// budget bounds (`≤ cache_bytes / page_size` by construction; spec/design/pager.md §3).
    pub fn resident_leaves(&self) -> usize {
        self.paging.as_ref().map_or(0, |p| p.resident_leaves())
    }

    /// Lay down the whole from-scratch image of the committed snapshot (the all-dirty special case —
    /// spec/fileformat/format.md) durably via temp-file + rename, and record the on-disk page
    /// high-water. Used by `create` to establish a fresh file with both meta slots seeded; every later
    /// commit is incremental (`persist`).
    fn write_full_image(&mut self) -> Result<()> {
        let path = self.path.clone().expect("write_full_image requires a path");
        let bytes = self
            .committed
            .to_image(self.page_size, self.committed.txid)?;
        write_atomic(&path, &bytes)?;
        self.page_count = (bytes.len() / self.page_size as usize) as u32;
        Ok(())
    }

    /// Durably publish `snap` to the backing file via an **incremental** copy-on-write commit
    /// (spec/fileformat/format.md *Allocation & incremental commit*; transactions.md §9) — the
    /// synchronous-commit chokepoint. Write the dirty pages this transaction introduced — reusing
    /// free-list pages a prior root abandoned before extending the file (P6.2) — `sync`, write the
    /// **alternate** meta slot (`snap.txid & 1`), `sync`. Clean pages are never rewritten. A crash
    /// between the two syncs leaves the prior meta — and thus the prior snapshot — intact (its pages
    /// were not overwritten: a reused free page is reachable from no live snapshot). An in-memory
    /// database (no path) is a **no-op success**: it does not mutate `self`, and the committed swap
    /// happens in `commit_tx` only after this returns Ok. `page_count` / `free_pages` advance only
    /// after both syncs succeed, so a write failure leaves `self`, `committed`, and the file's prior
    /// meta untouched (the working snapshot is then discarded). The `synchronous=off` mode gates here.
    pub(crate) fn persist(&mut self, snap: &Snapshot) -> Result<()> {
        // An in-memory database has no paging context — a no-op success (the committed swap happens
        // in `commit_tx` after this returns Ok). Compute the dirty-page set + meta before locking the
        // pager (so `self.free_pages`/`page_size`/`page_count` are read first).
        if self.paging.is_none() {
            return Ok(());
        }
        let free = self.free_pages.clone();
        let write = snap.incremental_image(
            self.page_size,
            self.page_count,
            &free,
            self.paging.as_deref(),
        )?;
        let meta =
            crate::format::meta_page(self.page_size, snap.txid, write.root_page, write.page_count);
        {
            let paging = self.paging.as_ref().expect("paging present");
            let mut pager = paging.pager();
            for (index, bytes) in &write.pages {
                pager.write_block(*index, bytes)?;
            }
            pager.sync()?; // body pages durable before the meta can reference them
            pager.write_block((snap.txid & 1) as u32, &meta)?;
            pager.sync()?; // the commit is published
        }
        self.page_count = write.page_count;
        self.free_pages = write.free_remaining;
        Ok(())
    }

    /// Commit the current transaction (spec/design/api.md §2.2, transactions.md §4.2). Publishes
    /// the open explicit block durably (per `synchronous`); a `commit` with no open block is a
    /// **lenient no-op success** (under autocommit each statement already committed). Drives the
    /// same mechanism as SQL `COMMIT`.
    pub fn commit(&mut self) -> Result<()> {
        self.commit_tx().map(|_| ())
    }

    /// Roll back the current transaction (spec/design/api.md §2.2, transactions.md §4.2). Discards
    /// the open explicit block's working set; a `rollback` with no open block is a **no-op
    /// success**. Drives the same mechanism as SQL `ROLLBACK`.
    pub fn rollback(&mut self) -> Result<()> {
        self.rollback_tx().map(|_| ())
    }

    /// Release the handle (spec/design/api.md §2.3). It **rolls back any open explicit
    /// transaction** (its in-progress work is discarded) and does not commit one. Under
    /// autocommit every prior statement is already durable, so — unlike the original model —
    /// `close` does **not** drop committed work; durability is never hidden in a destructor.
    /// Idempotent.
    pub fn close(mut self) -> Result<()> {
        let _ = self.rollback_tx();
        self.path = None;
        // Drop this handle's reference to the shared paging context; the file closes when the last
        // reference (any store in the committed snapshot, dropped as `self` is consumed) goes away.
        self.paging = None;
        Ok(())
    }
}

/// Write `bytes` to `path` crash-safely (spec/design/api.md §3): a sibling temp file, fsync,
/// atomic rename over the target, then a best-effort directory fsync so the rename is durable.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    let tmp = tmp_path(path);
    {
        let mut f = File::create(&tmp).map_err(io_error)?;
        f.write_all(bytes).map_err(io_error)?;
        f.sync_all().map_err(io_error)?;
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(io_error(e));
    }
    // Directory fsync makes the rename itself durable. Best-effort: not every platform allows
    // opening a directory for fsync (Windows), and the rename is already atomic there.
    if let Some(dir) = dir {
        if let Ok(d) = File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

/// The sibling temp path used during an atomic commit. A single writer (CLAUDE.md §3) means no
/// two commits race for it.
fn tmp_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".jedtmp");
    PathBuf::from(s)
}

fn io_error(e: std::io::Error) -> EngineError {
    EngineError::new(SqlState::IoError, format!("I/O error: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execute;
    use crate::value::Value;

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(name)
    }

    /// Demand paging (P6.4b, spec/design/pager.md §1/§4): a file-backed database with many leaf pages,
    /// reopened with a tiny buffer-pool budget, still scans and mutates **correctly** while keeping
    /// only a **bounded** number of leaves resident — the residency win, exercised end to end.
    #[test]
    fn demand_paging_scans_correctly_with_bounded_residency() {
        let path = tmp("jed_p64b_paging.jed");
        let _ = std::fs::remove_file(&path);
        let n = 600i64;
        const CAP: usize = 3;

        // Build a multi-level tree at a small page size, so a few hundred rows span many pages.
        {
            let mut db = Database::create(&path, DatabaseOptions { page_size: 256 }).unwrap();
            execute(&mut db, "CREATE TABLE t (k int32 PRIMARY KEY, v int32)").unwrap();
            execute(&mut db, "BEGIN").unwrap(); // one commit, not 600
            for k in 0..n {
                execute(&mut db, &format!("INSERT INTO t VALUES ({k}, {})", k * 2)).unwrap();
            }
            execute(&mut db, "COMMIT").unwrap();
            db.close().unwrap();
        }

        // Reopen demand-paged with a 3-leaf budget.
        let db = Database::open_with_options(
            &path,
            OpenOptions {
                cache_bytes: CAP * 256,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        // A PK table's skeleton load faults no leaves (it reads them only to count rows, uncached),
        // so the pool starts empty — and the file holds many pages.
        assert_eq!(db.resident_leaves(), 0, "skeleton load caches no leaf");
        assert!(
            db.page_count as usize > CAP * 5,
            "file has many more pages ({}) than the pool budget",
            db.page_count
        );

        // A full scan faults every leaf through the bounded pool: results are exact, residency bounded.
        let rows = db.rows_in_key_order("t").unwrap();
        assert_eq!(rows.len(), n as usize);
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row[0], Value::Int(i as i64));
            assert_eq!(row[1], Value::Int(i as i64 * 2));
        }
        assert!(
            db.resident_leaves() <= CAP,
            "resident leaves {} exceed the pool budget {CAP}",
            db.resident_leaves()
        );
        db.close().unwrap();

        // Mutate through the pool (each statement faults the leaf it touches), reopen, verify.
        {
            let mut db = Database::open_with_options(
                &path,
                OpenOptions {
                    cache_bytes: CAP * 256,
                    ..OpenOptions::default()
                },
            )
            .unwrap();
            execute(&mut db, "DELETE FROM t WHERE k = 100").unwrap();
            execute(&mut db, "UPDATE t SET v = 999 WHERE k = 200").unwrap();
            execute(&mut db, "INSERT INTO t VALUES (600, 1200)").unwrap();
            assert!(
                db.resident_leaves() <= CAP,
                "mutations keep residency bounded"
            );
            db.close().unwrap(); // autocommit already persisted each statement
        }
        let db = Database::open_with_options(
            &path,
            OpenOptions {
                cache_bytes: CAP * 256,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let rows = db.rows_in_key_order("t").unwrap();
        assert_eq!(rows.len(), n as usize, "one deleted, one inserted");
        assert!(
            rows.iter().all(|r| r[0] != Value::Int(100)),
            "k=100 was deleted"
        );
        let k200 = rows.iter().find(|r| r[0] == Value::Int(200)).unwrap();
        assert_eq!(k200[1], Value::Int(999), "k=200 was updated");
        let k600 = rows.iter().find(|r| r[0] == Value::Int(600)).unwrap();
        assert_eq!(k600[1], Value::Int(1200), "k=600 was inserted");
        db.close().unwrap();

        let _ = std::fs::remove_file(&path);
    }

    /// P6.4c memory-budget API + large-file hardening (spec/design/pager.md §6): a database whose leaf
    /// pages far exceed a tiny `cache_bytes` budget opens via the public API, and a repeated point-query
    /// workload keeps `resident_leaves()` within the budget throughout (each scan faults leaves through
    /// the pool, which evicts under CLOCK) — proving the resident set stays bounded under sustained reads.
    #[test]
    fn memory_budget_bounds_residency_under_lookups() {
        let path = tmp("jed_p64c_budget.jed");
        let _ = std::fs::remove_file(&path);
        let n = 2000i64;
        const CAP: usize = 4;

        {
            let mut db = Database::create(&path, DatabaseOptions { page_size: 256 }).unwrap();
            execute(&mut db, "CREATE TABLE t (k int32 PRIMARY KEY, v int32)").unwrap();
            execute(&mut db, "BEGIN").unwrap();
            for k in 0..n {
                execute(&mut db, &format!("INSERT INTO t VALUES ({k}, {})", k + 1)).unwrap();
            }
            execute(&mut db, "COMMIT").unwrap();
            db.close().unwrap();
        }

        let mut db = Database::open_with_options(
            &path,
            OpenOptions {
                cache_bytes: CAP * 256,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        // The data dwarfs the budget: far more pages than CAP, yet nothing is resident until a read.
        assert!(
            db.page_count as usize > CAP * 20,
            "file ({} pages) should dwarf the {CAP}-page budget",
            db.page_count
        );
        assert_eq!(db.resident_leaves(), 0);

        // A spread of point queries (each a full scan, no index) repeatedly faults leaves through the
        // bounded pool; residency never exceeds the budget, and every answer is correct.
        for k in (0..n).step_by(97) {
            let out = execute(&mut db, &format!("SELECT v FROM t WHERE k = {k}")).unwrap();
            match out {
                crate::Outcome::Query { rows, .. } => {
                    assert_eq!(rows.len(), 1);
                    assert_eq!(rows[0][0], Value::Int(k + 1), "value at k={k}");
                }
                _ => panic!("expected a query result"),
            }
            assert!(
                db.resident_leaves() <= CAP,
                "resident leaves {} exceed the budget {CAP} at k={k}",
                db.resident_leaves()
            );
        }
        db.close().unwrap();
        let _ = std::fs::remove_file(&path);
    }

    /// P6.4c (spec/design/pager.md §3, api.md §2.1): a byte budget smaller than a single page still
    /// keeps **one** leaf resident — the `max(1, cache_bytes / page_size)` floor — and still scans
    /// correctly. This is the `page_size > cache_bytes` case.
    #[test]
    fn tiny_budget_keeps_one_leaf_resident() {
        let path = tmp("jed_p64c_tiny_budget.jed");
        let _ = std::fs::remove_file(&path);
        let n = 400i64;

        {
            let mut db = Database::create(&path, DatabaseOptions { page_size: 256 }).unwrap();
            execute(&mut db, "CREATE TABLE t (k int32 PRIMARY KEY, v int32)").unwrap();
            execute(&mut db, "BEGIN").unwrap();
            for k in 0..n {
                execute(&mut db, &format!("INSERT INTO t VALUES ({k}, {})", k + 1)).unwrap();
            }
            execute(&mut db, "COMMIT").unwrap();
            db.close().unwrap();
        }

        // A 1-byte budget is far below the 256-byte page size: it must clamp to one resident leaf,
        // not zero (zero would be unable to walk a root→leaf path).
        let db = Database::open_with_options(
            &path,
            OpenOptions {
                cache_bytes: 1,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let rows = db.rows_in_key_order("t").unwrap();
        assert_eq!(rows.len(), n as usize);
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row[0], Value::Int(i as i64));
            assert_eq!(row[1], Value::Int(i as i64 + 1));
        }
        assert_eq!(
            db.resident_leaves(),
            1,
            "a sub-page budget keeps exactly one leaf resident"
        );
        db.close().unwrap();
        let _ = std::fs::remove_file(&path);
    }

    /// P6.4c page-size hardening (format.md *Page model*): `create` rejects a page size above
    /// `MAX_PAGE_SIZE` (64 KiB) — without the cap a huge page size forces a multi-gigabyte allocation.
    #[test]
    fn create_rejects_oversized_page_size() {
        let path = tmp("jed_p64c_huge_page.jed");
        let _ = std::fs::remove_file(&path);
        let err = Database::create(&path, DatabaseOptions { page_size: 1 << 20 })
            .err()
            .expect("oversized page size must be rejected");
        assert_eq!(err.state, SqlState::FeatureNotSupported);
        assert!(
            err.message.contains("too large"),
            "message names the cause: {}",
            err.message
        );
        // create must not leave a partial file behind on a rejected page size.
        assert!(!path.exists(), "no file written on a rejected page size");
        let _ = std::fs::remove_file(&path);
    }

    /// P6.4c page-size hardening (format.md *Page model*): the read path rejects a file whose meta
    /// records an out-of-range `page_size` as corrupt — the range check runs *before* any allocation
    /// against that size, so a hostile file cannot force a giant allocation (CLAUDE.md §13).
    #[test]
    fn open_rejects_oversized_page_size() {
        // A crafted meta header recording page_size = 70000 (> MAX_PAGE_SIZE) in big-endian at offset 8.
        let mut image = vec![0u8; 200];
        image[0..4].copy_from_slice(b"JEDB");
        image[8..12].copy_from_slice(&70000u32.to_be_bytes());
        let err = Database::from_image(&image)
            .err()
            .expect("an out-of-range page size must be rejected");
        assert_eq!(err.state, SqlState::DataCorrupted);
        assert!(err.message.contains("page size"), "{}", err.message);
    }

    /// Large values (spec/design/large-values.md §12) through the **default demand-paged** file path:
    /// a value too big for a record spills to an overflow chain; the paged read faults the leaf and
    /// follows the chain to materialize it exactly; and after a delete + reopen the dead chain's pages
    /// are reclaimed, so re-inserting a large value reuses them rather than growing the file.
    #[test]
    fn external_value_through_paged_file_and_reclaims() {
        let path = tmp("jed_large_values.jed");
        let _ = std::fs::remove_file(&path);
        // Incompressible filler (xorshift32 "JEDB" over a 64-char alphabet — format.md
        // "Fixtures"): ≫ RECORD_MAX at ps 256 AND immune to Slice B's compress pass, so the
        // value genuinely spills to a multi-page overflow chain.
        let big: String = {
            const ALPHA64: &[u8] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut x: u32 = 0x4A45_4442;
            (0..1500)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 17;
                    x ^= x << 5;
                    ALPHA64[(x % 64) as usize] as char
                })
                .collect()
        };

        {
            let mut db = Database::create(&path, DatabaseOptions { page_size: 256 }).unwrap();
            execute(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, body text)").unwrap();
            execute(&mut db, &format!("INSERT INTO t VALUES (1, '{big}')")).unwrap();
            execute(&mut db, "INSERT INTO t VALUES (2, 'small')").unwrap();
            db.close().unwrap();
        }

        // Reopen demand-paged (the default `open`): the big value reconstructs exactly through the
        // pager-backed chain read.
        {
            let db = Database::open(&path).unwrap();
            let rows = db.rows_in_key_order("t").unwrap();
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0][1], Value::Text(big.clone()));
            assert_eq!(rows[1][1], Value::Text("small".to_string()));
            db.close().unwrap();
        }

        // Delete the big row; its overflow chain is orphaned (leaked this session).
        {
            let mut db = Database::open(&path).unwrap();
            execute(&mut db, "DELETE FROM t WHERE id = 1").unwrap();
            db.close().unwrap();
        }

        // Reopen: the free-list reconstruction collects only *live* chains, so the dead chain's pages
        // are now free. Re-inserting a large value reuses them — the high-water grows by a handful of
        // pages, not by a whole fresh chain (~7 pages).
        let (before, after) = {
            let mut db = Database::open(&path).unwrap();
            let before = db.page_count;
            execute(&mut db, &format!("INSERT INTO t VALUES (3, '{big}')")).unwrap();
            let after = db.page_count;
            db.close().unwrap();
            (before, after)
        };
        assert!(
            after <= before + 3,
            "re-insert reused reclaimed overflow pages (page_count {before} → {after})"
        );

        // Final correctness through the paged path.
        {
            let db = Database::open(&path).unwrap();
            let rows = db.rows_in_key_order("t").unwrap();
            assert_eq!(rows.len(), 2);
            let r3 = rows
                .iter()
                .find(|r| r[0] == Value::Int(3))
                .expect("the re-inserted big row");
            assert_eq!(r3[1], Value::Text(big));
            db.close().unwrap();
        }
        let _ = std::fs::remove_file(&path);
    }
}
