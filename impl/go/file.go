package jed

// Host file layer for the Go core (spec/design/api.md §2): open/create/commit/close a single-file
// database durably. Pure os — no cgo, no FFI (CLAUDE.md §2), fully memory-safe. Create lays down the
// from-scratch image (temp-file + fsync + atomic rename + directory fsync, api.md §3); every later
// commit is an incremental copy-on-write write of just the dirty pages, published by alternating the
// meta slot (spec/fileformat/format.md, P6.1 part B) — the block seam below pwrites pages (WriteAt)
// into the open file rather than rewriting it.

import (
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
)

// databaseOptions are the settings for a newly-created database file (spec/design/api.md §2).
// PageSize is fixed into the file's meta at creation and cannot change thereafter.
type databaseOptions struct {
	PageSize uint32
}

// defaultDatabaseOptions returns the default create settings (the default page size).
func defaultDatabaseOptions() databaseOptions {
	return databaseOptions{PageSize: DefaultPageSize}
}

// Create makes a new file-backed database at path with opts (the page size is locked into the
// file). The path must not already exist — 58P02 otherwise. An initial empty image is written
// durably immediately, so the file exists with its page size fixed (api.md §2).
func create(path string, opts databaseOptions) (*engine, error) {
	if _, err := os.Stat(path); err == nil {
		return nil, newError(DuplicateFile, "database file already exists: "+path)
	} else if !errors.Is(err, fs.ErrNotExist) {
		return nil, ioError(err)
	}
	db := newEngine()
	db.path = path
	db.pageSize = opts.PageSize
	db.committed.txid = 1                       // the initial empty image is committed as txid 1
	if err := db.writeFullImage(); err != nil { // lay down the from-scratch image; later commits are incremental
		return nil, err
	}
	// Adopt the just-written file as the open pager + buffer pool, so later commits write through the
	// seam without re-opening (spec/design/pager.md). Tables built in this session bind this pager at
	// creation (snapshot.storePaging), so their committed leaves demote at each commit and fault back
	// through the pool — same residency shape as after a reopen.
	f, err := os.OpenFile(path, os.O_RDWR, 0)
	if err != nil {
		return nil, ioError(err)
	}
	p, err := pagerFromStore(&fileBlockStore{f: f})
	if err != nil {
		_ = f.Close()
		return nil, err
	}
	db.paging = newSharedPaging(p, cacheLeaves(defaultCacheBytes, db.pageSize))
	db.committed.storePaging = db.paging
	return db, nil
}

// OpenOptions are open-time settings for a file-backed database (spec/design/api.md §2.1). Unlike
// databaseOptions (create-time, fixed into the file), these are handle settings — not stored in the
// file, so a different host may reopen the same file with different ones.
type OpenOptions struct {
	// CacheBytes is the buffer-pool budget in bytes: roughly the maximum memory the resident leaf cache
	// holds at once (spec/design/pager.md §3, P6.4b/c). Bytes, not a page count, so the budget does not
	// silently scale with the file's page size; the engine converts it to a leaf-page capacity by the
	// file's page size as max(1, CacheBytes / pageSize) (cacheLeaves). The bound that lets a database far
	// larger than RAM be served (pager.md §1); it never changes what a query observes (§3/§5). 0 →
	// DefaultCacheBytes (256 MiB).
	CacheBytes int
	// ReadOnly opens the file read-only (api.md §2.1). The handle then behaves like PostgreSQL
	// hot standby: every transaction defaults to READ ONLY, an explicit READ WRITE request and
	// any write statement are 25006, and the file is opened without write access, so it is never
	// written (works on a read-only filesystem).
	ReadOnly bool
	// WorkMem is the work-memory budget in bytes for a blocking operator before it spills to disk
	// (spec/design/spill.md §3, api.md §2.1): the ORDER BY external merge sort holds at most roughly
	// this many bytes of rows resident, then spills sorted runs. Like CacheBytes it is a handle
	// setting that never changes what a query observes (spill.md §6). 0 → DefaultWorkMem (256 MiB).
	WorkMem int
}

// Open opens an existing file-backed database at path with default open settings — the buffer-pool
// budget defaults to DefaultCacheBytes (256 MiB). See OpenWithOptions to set the budget. The path must
// exist — 58P01 otherwise; a malformed file is XX001, a read failure 58030 (api.md §2.1).
func open(path string) (*engine, error) {
	return openWithOptions(path, OpenOptions{})
}

// OpenWithOptions opens an existing file-backed database at path with explicit open settings (the
// memory budget, opts.CacheBytes). Loads its committed state, adopting its page size / txid.
//
// The demand-paged loader builds only the interior B-tree skeleton resident, faulting each leaf through
// the bounded buffer pool on access, so the resident set is bounded by the pool — not the file size
// (P6.4b). The byte budget is converted to a leaf-page capacity by the file's page size (cacheLeaves).
// The budget is a handle setting, not stored in the file (§3). Later commits write through the same
// pager kept open for the handle's life.
func openWithOptions(path string, opts OpenOptions) (*engine, error) {
	cacheBytes := opts.CacheBytes
	if cacheBytes <= 0 {
		cacheBytes = defaultCacheBytes
	}
	// A read-only open never writes the file, so it is not opened for writing at all — the OS
	// enforces what the executor's 25006 guards promise (api.md §2.1).
	flag := os.O_RDWR
	if opts.ReadOnly {
		flag = os.O_RDONLY
	}
	f, err := os.OpenFile(path, flag, 0)
	if err != nil {
		if errors.Is(err, fs.ErrNotExist) {
			return nil, newError(UndefinedFile, "database file does not exist: "+path)
		}
		return nil, ioError(err)
	}
	p, err := pagerFromStore(&fileBlockStore{f: f})
	if err != nil {
		_ = f.Close()
		return nil, err
	}
	// Convert the byte budget to a leaf-page capacity by the file's page size; LoadEnginePaged
	// rejects an out-of-range page size as corrupt (cacheLeaves clamps the divisor so a malformed
	// pageSize = 0 cannot divide by zero before that check runs).
	db, err := loadEnginePaged(p, cacheLeaves(cacheBytes, p.pageSize))
	if err != nil {
		_ = f.Close()
		return nil, err
	}
	db.path = path
	db.readOnly = opts.ReadOnly
	if opts.WorkMem != 0 {
		db.session.workMem = opts.WorkMem
	}
	return db, nil
}

// writeFullImage lays down the whole from-scratch image of the committed snapshot (the all-dirty
// special case — spec/fileformat/format.md) durably via temp-file + rename, and records the on-disk
// page high-water. Used by Create to establish a fresh file with both meta slots seeded; every later
// commit is incremental (persist).
func (db *engine) writeFullImage() error {
	bytes, err := db.committed.ToImage(db.pageSize, db.committed.txid)
	if err != nil {
		return err
	}
	if err := writeAtomic(db.path, bytes); err != nil {
		return err
	}
	db.pageCount = uint32(len(bytes) / int(db.pageSize))
	return nil
}

// persist durably publishes snap to the backing file via an incremental copy-on-write commit
// (spec/fileformat/format.md *Allocation & incremental commit*; transactions.md §9) — the
// synchronous-commit chokepoint. Write the dirty pages this transaction introduced — reusing free-list
// pages a prior root abandoned before extending the file (P6.2) — Sync, write the alternate meta slot
// (snap.txid & 1), Sync. Clean pages are never rewritten. A crash between the two syncs leaves the
// prior meta — and thus the prior snapshot — intact (its pages were not overwritten: a reused free page
// is reachable from no live snapshot). An in-memory database (no path) is a no-op success: it does not
// mutate db, and the committed swap happens in commitTx only after this returns nil. db.pageCount /
// db.freePages advance only after both syncs succeed, so a write failure leaves db, committed, and the
// file's prior meta untouched (the working snapshot is then discarded). The future synchronous=off mode
// gates here.
func (db *engine) persist(snap *snapshot) error {
	// An in-memory database has no paging context — a no-op success (the committed swap happens in
	// commitTx after this returns nil).
	if db.paging == nil {
		return nil
	}
	write, err := snap.incrementalImage(db.pageSize, db.pageCount, db.freePages, db.paging)
	if err != nil {
		return err
	}
	meta := metaPage(db.pageSize, snap.txid, write.rootPage, write.pageCount)
	// Write the dirty pages + meta through the shared pager (under the pool lock, so a concurrent
	// fault cannot race): body pages, Sync, then the alternate meta slot, Sync.
	if err := db.paging.withPager(func(p *pager) error {
		// Preallocate the file ahead of the high-water in chunks, so this commit's body write — and
		// most later commits' — lands in already-allocated space and the body fdatasync below carries
		// no file-growth metadata journaling (spec/design/pager.md §7).
		if err := p.reserve(write.pageCount); err != nil {
			return err
		}
		for _, pg := range write.pages {
			if err := p.writeBlock(pg.index, pg.bytes); err != nil {
				return err
			}
			// Drop any stale pool entry for a rewritten page (bufferpool.go invalidate): a no-op unless a
			// reclaim domain reused a freed page id, in which case the pool's prior decode must be evicted.
			db.paging.pool.invalidate(pg.index)
		}
		if err := p.sync(); err != nil { // body pages durable before the meta can reference them
			return err
		}
		if err := p.writeBlock(uint32(snap.txid&1), meta); err != nil {
			return err
		}
		return p.sync() // the commit is published
	}); err != nil {
		return err
	}
	db.pageCount = write.pageCount
	db.freePages = write.freeRemaining
	return nil
}

// ResidentLeaves is the number of leaf pages currently resident in the buffer pool — 0 for an
// in-memory database (it is fully resident, nothing to page). The read-only gauge the
// OpenOptions.CacheBytes budget bounds (≤ CacheBytes / pageSize by construction; spec/design/pager.md §3).
func (db *engine) ResidentLeaves() int {
	if db.paging == nil {
		return 0
	}
	return db.paging.residentLeaves()
}

// Commit commits the current transaction (spec/design/api.md §2.2, transactions.md §4.2).
// Publishes the open explicit block durably (per synchronous); a Commit with no open block is a
// lenient no-op success (under autocommit each statement already committed). Drives the same
// mechanism as SQL COMMIT.
func (db *engine) Commit() error {
	_, err := db.commitTx()
	return err
}

// Rollback rolls back the current transaction (spec/design/api.md §2.2, transactions.md §4.2).
// Discards the open explicit block's working set; a Rollback with no open block is a no-op
// success. Drives the same mechanism as SQL ROLLBACK.
func (db *engine) Rollback() error {
	_, err := db.rollbackTx()
	return err
}

// Close releases the handle (spec/design/api.md §2.3). It rolls back any open explicit
// transaction (its in-progress work is discarded) and does not commit one. Under autocommit every
// prior statement is already durable, so — unlike the original model — Close does NOT drop
// committed work; durability is never hidden in a destructor. Idempotent.
func (db *engine) Close() error {
	_, _ = db.rollbackTx()
	db.path = ""
	if db.paging != nil {
		_ = db.paging.close() // drop the open file (close it)
		db.paging = nil
	}
	return nil
}

// writeAtomic writes bytes to path crash-safely (spec/design/api.md §3): a sibling temp file,
// fsync, atomic rename over the target, then a best-effort directory fsync so the rename is
// durable.
func writeAtomic(path string, bytes []byte) error {
	dir := filepath.Dir(path)
	tmp := path + ".jedtmp"
	f, err := os.Create(tmp)
	if err != nil {
		return ioError(err)
	}
	if _, err := f.Write(bytes); err != nil {
		f.Close()
		os.Remove(tmp)
		return ioError(err)
	}
	if err := f.Sync(); err != nil {
		f.Close()
		os.Remove(tmp)
		return ioError(err)
	}
	if err := f.Close(); err != nil {
		os.Remove(tmp)
		return ioError(err)
	}
	if err := os.Rename(tmp, path); err != nil {
		os.Remove(tmp)
		return ioError(err)
	}
	// Directory fsync makes the rename itself durable. Best-effort: not every platform allows
	// opening a directory for fsync (Windows), and the rename is already atomic there.
	if d, derr := os.Open(dir); derr == nil {
		_ = d.Sync()
		_ = d.Close()
	}
	return nil
}

func ioError(err error) error {
	return newError(IoError, fmt.Sprintf("I/O error: %v", err))
}
