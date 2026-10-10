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

// Locking selects the shared-file coordination mode (spec/design/locking.md §7.1).
type Locking uint8

const (
	LockingAuto Locking = iota
	LockingShared
	LockingExclusive
	LockingNone
)

// databaseOptions are the settings for a newly-created database file (spec/design/api.md §2).
// PageSize is fixed into the file's meta at creation and cannot change thereafter.
type databaseOptions struct {
	PageSize uint32
	// noSync is the fsync=off host setting (api.md §2.1): the commit writes identical bytes in the same
	// order but skips the fdatasync barriers. Unlike PageSize this is NOT fixed into the file — it is a
	// runtime handle setting. DEV/TESTING only (durable across a process crash, not an OS crash).
	noSync bool
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
	db.spillDir = os.TempDir()
	db.pageSize = opts.PageSize
	db.committed.txid = 1                                  // the initial empty image is committed as txid 1
	if err := db.writeFullImage(opts.noSync); err != nil { // lay down the from-scratch image; later commits are incremental
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
	p, err := pagerFromStore(&fileBlockStore{f: f, noSync: opts.noSync})
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
	// SkipFsync turns off the per-commit fsync (the fsync=off host setting, api.md §2.1). A commit still
	// writes the same bytes in the same order, but the fdatasync barrier becomes a no-op — much faster.
	// DEV/TESTING ONLY: the data survives a process crash (the OS page cache still flushes) but NOT an
	// OS crash / power loss. Never changes what a query observes or the on-disk bytes; default false.
	SkipFsync bool
	Locking   Locking
	// nil is the 5s default; a non-nil pointer (including &0) is an explicit deadline.
	FileLockTimeoutMs *uint64
	// Extensions are host extensions to register on this handle (spec/design/extensibility.md §7):
	// scalar functions the host supplies, FROZEN for the handle's lifetime. A handle setting like the
	// rest — not stored in the file, so a reopening host brings its own (§14 step 3). nil ⇒ none.
	Extensions *ExtensionRegistry
	// MaxStorageBytes is the committed-storage limit in bytes over the file's live pages
	// (spec/design/memory.md §8.7): a commit that would grow livePages × pageSize past it fails 54P06.
	// Zero or negative is unlimited (the default). A handle setting, not stored in the file;
	// Database.SetMaxStorageBytes changes it.
	MaxStorageBytes int64
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
	p, err := pagerFromStore(&fileBlockStore{f: f, noSync: opts.SkipFsync})
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
	db.spillDir = os.TempDir()
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
func (db *engine) writeFullImage(noSync bool) error {
	bytes, err := db.committed.ToImage(db.pageSize, db.committed.txid)
	if err != nil {
		return err
	}
	if err := writeAtomic(db.path, bytes, noSync); err != nil {
		return err
	}
	db.pageCount = uint32(len(bytes) / int(db.pageSize))
	// create writes an empty database: every body page is live, and every one is a catalog page.
	body := db.pageCount - rootPage
	db.live = liveCount{live: body, catalog: body}
	return nil
}

// persist publishes a single-handle file transaction through validated COW. Dirty
// body pages and their checksummed manifest precede the alternating meta write;
// one final sync makes the complete dependency set durable. Recovery validates
// those dependencies before adopting a root, without a separate body barrier.
func (db *engine) persist(snap *snapshot) error {
	if db.paging == nil {
		return nil
	}
	if err := db.paging.withPager(func(p *pager) error { return p.checkValidatedCommit() }); err != nil {
		return err
	}
	write, err := snap.incrementalImage(db.pageSize, db.pageCount, db.freePages, true, db.paging)
	if err != nil {
		return err
	}
	// The live-page count this commit publishes (spec/design/memory.md §8.7): db.committed is still the
	// previous snapshot here.
	live, err := db.live.after(write, db.committed, db.paging, db.pageSize)
	if err != nil {
		return err
	}
	st := storage{
		pageSize: db.pageSize, pageCount: db.pageCount, freePages: db.freePages,
		paging: db.paging, liveAtCompaction: db.liveAtCompaction, freeGenTxid: db.freeGenTxid,
	}
	if err := st.commitFile(snap, write, live, db.openStreams == 0, true); err != nil {
		return err
	}
	db.pageCount, db.freePages = st.pageCount, st.freePages
	db.liveAtCompaction, db.freeGenTxid = st.liveAtCompaction, st.freeGenTxid
	db.live = st.live
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
// durable. Under noSync (fsync=off) the two fsyncs are skipped — the write + rename still happen,
// but the bytes are only in the OS page cache (dev/testing; no durability on an OS crash).
func writeAtomic(path string, bytes []byte, noSync bool) error {
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
	if !noSync {
		if err := f.Sync(); err != nil {
			f.Close()
			os.Remove(tmp)
			return ioError(err)
		}
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
	// opening a directory for fsync (Windows), and the rename is already atomic there. Skipped under
	// noSync (fsync=off).
	if !noSync {
		if d, derr := os.Open(dir); derr == nil {
			_ = d.Sync()
			_ = d.Close()
		}
	}
	return nil
}

// compactFile rewrites the file database at path as the from-scratch image of snap at txid and
// reopens it (host compaction, spec/design/api.md §2.6). The caller holds the writer gate and the
// reader watermark lock, so nothing else reads old while its file is swapped.
//
// The image streams page by page into the sibling temp file, which takes the old file's permission
// bits and is synced. Then old's file is closed (a closedBlockStore takes its place, so a stale
// snapshot fails closed rather than reading the new layout), the temp file is renamed over the real
// path, and the directory is synced. A failure before the rename leaves the old file and old exactly
// as they were. If the rename fails, the old file is reopened into old. If reopening the new file
// fails, old stays closed and poisoned, so the handle must be reopened. Returns the replacement paging
// context, a pool of capacity leaves over the new file.
func compactFile(path string, snap *snapshot, pageSize uint32, txid uint64, old *sharedPaging, capacity int) (*sharedPaging, error) {
	var noSync bool
	_ = old.withPager(func(p *pager) error { noSync = p.skipsSync(); return nil })
	// Rename over the real file, never over a symlink naming it (the symlink keeps pointing at it).
	real, err := filepath.EvalSymlinks(path)
	if err != nil {
		return nil, ioError(err)
	}
	tmp := real + ".jedtmp"
	if err := writeImageFile(tmp, real, snap, pageSize, txid, noSync); err != nil {
		os.Remove(tmp)
		return nil, err
	}

	// Close the old file before the rename: Windows cannot replace a file that is still open.
	_ = old.withPager(func(p *pager) error { return p.swapStore(closedBlockStore{}).close() })
	if err := os.Rename(tmp, real); err != nil {
		os.Remove(tmp)
		_ = old.withPager(func(p *pager) error {
			f, oerr := os.OpenFile(real, os.O_RDWR, 0)
			if oerr != nil {
				p.poison()
				return nil
			}
			p.swapStore(&fileBlockStore{f: f, noSync: noSync})
			return nil
		})
		return nil, ioError(err)
	}
	_ = old.withPager(func(p *pager) error { p.poison(); return nil })
	// Directory fsync makes the rename itself durable (best-effort, as in writeAtomic).
	if !noSync {
		if d, derr := os.Open(filepath.Dir(real)); derr == nil {
			_ = d.Sync()
			_ = d.Close()
		}
	}
	f, err := os.OpenFile(real, os.O_RDWR, 0)
	if err != nil {
		return nil, ioError(err)
	}
	p, err := pagerFromStore(&fileBlockStore{f: f, noSync: noSync})
	if err != nil {
		_ = f.Close()
		return nil, err
	}
	return newSharedPaging(p, capacity), nil
}

// writeImageFile streams snap's from-scratch image at txid into a fresh file at tmp with positioned
// writes, gives it the permission bits of the file at real, and syncs it unless noSync. The caller
// removes tmp on failure.
func writeImageFile(tmp, real string, snap *snapshot, pageSize uint32, txid uint64, noSync bool) error {
	f, err := os.Create(tmp)
	if err != nil {
		return ioError(err)
	}
	_, err = snap.writeImage(pageSize, txid, func(index uint32, page []byte) error {
		if _, werr := f.WriteAt(page, int64(index)*int64(pageSize)); werr != nil {
			return ioError(werr)
		}
		return nil
	})
	if err == nil {
		var info os.FileInfo
		if info, err = os.Stat(real); err != nil {
			err = ioError(err)
		} else if cerr := f.Chmod(info.Mode().Perm()); cerr != nil {
			err = ioError(cerr)
		}
	}
	if err == nil && !noSync {
		if serr := f.Sync(); serr != nil {
			err = ioError(serr)
		}
	}
	if cerr := f.Close(); err == nil && cerr != nil {
		err = ioError(cerr)
	}
	return err
}

func ioError(err error) error {
	return newError(IoError, fmt.Sprintf("I/O error: %v", err))
}
