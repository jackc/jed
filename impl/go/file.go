package jed

// Host file layer for the Go core (spec/design/api.md §2): open/create/commit/close a single-file
// database durably. Pure os — no cgo, no FFI (CLAUDE.md §2), fully memory-safe. Create lays down the
// from-scratch image (temp-file + fsync + atomic rename + directory fsync, api.md §3); every later
// commit is an incremental copy-on-write write of just the dirty pages, published by alternating the
// meta slot (spec/fileformat/format.md, P6.1 part B) — the block seam below pwrites pages (WriteAt)
// into the open file rather than rewriting it.

import (
	"fmt"
	"os"
	"path/filepath"
)

// Locking selects how a file database coordinates with other processes (spec/design/locking.md,
// page-service.md): LockingAuto (the default) is the page service — the opener that takes presence EX
// owns the file and serves it, every other opener is its page client — where the host supports it, and
// a non-serving owner elsewhere; LockingExclusive is a non-serving owner (a second opener waits, then
// fails 55006); LockingNone skips coordination (an unsafe escape hatch).
type Locking uint8

const (
	LockingAuto Locking = iota
	LockingExclusive
	LockingNone
)

// Synchronous is the durability mode of a file-backed handle (spec/design/transactions.md §9, api.md
// §2.1): when a commit's durable barrier runs.
type Synchronous uint8

const (
	// SynchronousOn (the default) makes every commit durable before it returns.
	SynchronousOn Synchronous = iota
	// SynchronousOff acknowledges a commit once its pages and meta are written and batches the barrier:
	// a later flush (the batch cap, Close, …) makes a run of commits durable together. A power loss or
	// OS crash can lose the commits acknowledged since the last flush, never the file's integrity
	// (spec/design/validated-cow.md §4).
	SynchronousOff
)

// databaseOptions are the settings for a newly-created database file (spec/design/api.md §2).
// PageSize is fixed into the file's meta at creation and cannot change thereafter.
type databaseOptions struct {
	PageSize uint32
	// noSync is the fsync=off host setting (api.md §2.1): the commit writes identical bytes in the same
	// order but skips the fdatasync barriers. Unlike PageSize this is NOT fixed into the file — it is a
	// runtime handle setting. DEV/TESTING only (durable across a process crash, not an OS crash).
	noSync bool
	// env is the handle's Environment (spec/design/environment.md); nil ⇒ a fresh empty one.
	env *Environment
	// credential, when set, creates the file encrypted under it (spec/design/encryption.md,
	// spec/fileformat/encrypted.md); kdf stretches a passphrase credential (zero: the default).
	credential *Credential
	kdf        Argon2Params
}

// defaultDatabaseOptions returns the default create settings (the default page size).
func defaultDatabaseOptions() databaseOptions {
	return databaseOptions{PageSize: DefaultPageSize}
}

// Create makes a new file-backed database at path with opts (the page size is locked into the
// file). The path must not already exist — 58P02 otherwise. An initial empty image is written
// durably immediately, so the file exists with its page size fixed (api.md §2).
func create(path string, opts databaseOptions) (*engine, error) {
	return createVia(osFiles, path, opts)
}

// createVia is create through the file host files (filehost.go): the existence check, the temp file,
// its rename, the directory barrier, and the open all go through it.
func createVia(files fileHost, path string, opts databaseOptions) (*engine, error) {
	if opts.credential != nil && int(opts.PageSize) < minEncryptedPageSize {
		return nil, newError(FeatureNotSupported, "an encrypted database needs a page size of at least 1024")
	}
	if exists, err := files.exists(path); err != nil {
		return nil, err
	} else if exists {
		return nil, newError(DuplicateFile, "database file already exists: "+path)
	}
	env := opts.env
	if env == nil {
		env = NewEnvironment()
	}
	// A new encrypted file's key block and codec (spec/fileformat/encrypted.md §5, §8): the data key,
	// KDF salt and wrap nonce are the first draws of the database's key-entropy stream.
	var codec *pageCodec
	if opts.credential != nil {
		var err error
		if codec, err = createCodec(opts.credential, opts.kdf, env.keyStream()); err != nil {
			return nil, err
		}
	}
	db := newEngineIn(env)
	db.path = path
	// An encrypted database never spills plaintext: it has no scratch target until scratch encryption
	// lands (spec/fileformat/encrypted.md §10).
	if codec == nil {
		db.spill = osSpillHost{dir: os.TempDir()}
	}
	db.pageSize = opts.PageSize
	db.committed.txid = 1                                                // the initial empty image is committed as txid 1
	if err := db.writeFullImage(files, opts.noSync, codec); err != nil { // lay down the from-scratch image; later commits are incremental
		return nil, err
	}
	// Adopt the just-written file as the open pager + buffer pool, so later commits write through the
	// seam without re-opening (spec/design/pager.md). Tables built in this session bind this pager at
	// creation (snapshot.storePaging), so their committed leaves demote at each commit and fault back
	// through the pool — same residency shape as after a reopen.
	store, err := files.openStore(path, opts.noSync, false)
	if err != nil {
		return nil, err
	}
	p, err := pagerFromStore(store)
	if err != nil {
		_ = store.close()
		return nil, err
	}
	// Both slots carry the bootstrap txid, so the floor slot is txid & 1 (format.md, *Slot alternation*).
	p.floorSlot = uint32(db.committed.txid & 1)
	p.codec = codec
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
	// Synchronous is the durability mode (SynchronousOn, the default, or SynchronousOff — transactions.md
	// §9). A handle setting, not stored in the file; the handle's file attachments follow it.
	Synchronous Synchronous
	Locking     Locking
	// nil is the 5s default; a non-nil pointer (including &0) is an explicit deadline.
	FileLockTimeoutMs *uint64
	// IdleInTransactionTimeoutMs is a page-service owner's idle-in-transaction timeout for client write
	// transactions (spec/design/page-service.md §11.1): a client holding the writer gate that sends
	// nothing for this long is revoked (25P03 at its next request). nil is the 60 s default; &0
	// disables it. Only the page service (LockingAuto) reads it.
	IdleInTransactionTimeoutMs *uint64
	// IdleSessionTimeoutMs closes a page-service connection idle between transactions for this long
	// (page-service.md §11.1); nil or &0 never does. YieldGraceMs and ShutdownGraceMs are how long a
	// yielding or closing owner lets running transactions finish (nil: 5 s and 30 s). MaxConnections
	// bounds an owner's connections (53300 beyond it); 0 is unlimited. Only the page service (LockingAuto) reads them,
	// as an owner's limits; a client keeps them for the day it becomes owner.
	IdleSessionTimeoutMs *uint64
	YieldGraceMs         *uint64
	ShutdownGraceMs      *uint64
	MaxConnections       int
	// Ownership is the opener's ownership preference under the page service (page-service.md §8.2):
	// OwnershipPrefer (the default), OwnershipYield, or OwnershipNever. Only the page service (LockingAuto) reads it.
	Ownership Ownership
	// Environment is the handle's outside inputs (spec/design/environment.md): the loaded Unicode and
	// time-zone data the file's references resolve against, the host functions, and the random + clock
	// sources. Shared by the handle's sessions, attachments, and temp domains; not stored in the file,
	// so a reopening host brings its own. nil ⇒ a fresh empty environment (no process-wide default).
	Environment *Environment
	// MaxStorageBytes is the committed-storage limit in bytes over the file's live pages
	// (spec/design/memory.md §8.7): a commit that would grow livePages × pageSize past it fails 54P06.
	// Zero or negative is unlimited (the default). A handle setting, not stored in the file;
	// Database.SetMaxStorageBytes changes it.
	MaxStorageBytes int64
	// Credential unlocks an encrypted file (spec/design/encryption.md §5): required for one (28P02
	// without it, 28P01 when it does not unwrap the key), refused for a plaintext file (28P03). A handle
	// argument only — never stored, never shown. An encrypted file is never served by the page service,
	// so a credential turns LockingAuto into an exclusive owner.
	Credential *Credential
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
	return openVia(osFiles, path, opts)
}

// openVia is openWithOptions through the file host files (filehost.go).
func openVia(files fileHost, path string, opts OpenOptions) (*engine, error) {
	cacheBytes := opts.CacheBytes
	if cacheBytes <= 0 {
		cacheBytes = defaultCacheBytes
	}
	store, err := files.openStore(path, opts.SkipFsync, opts.ReadOnly)
	if err != nil {
		return nil, err
	}
	p, err := pagerFromStore(store)
	if err != nil {
		_ = store.close()
		return nil, err
	}
	// Convert the byte budget to a leaf-page capacity by the file's page size; LoadEnginePaged
	// rejects an out-of-range page size as corrupt (cacheLeaves clamps the divisor so a malformed
	// pageSize = 0 cannot divide by zero before that check runs).
	env := opts.Environment
	if env == nil {
		env = NewEnvironment()
	}
	db, err := loadEnginePagedCredential(p, cacheLeaves(cacheBytes, p.pageSize), env, opts.Credential)
	if err != nil {
		_ = store.close()
		return nil, err
	}
	db.path = path
	// An encrypted database has no scratch target (spec/fileformat/encrypted.md §10).
	if !db.encrypted() {
		db.spill = osSpillHost{dir: os.TempDir()}
	}
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
func (db *engine) writeFullImage(files fileHost, noSync bool, codec *pageCodec) error {
	bytes, err := db.committed.toImageSealed(db.pageSize, db.committed.txid, codec)
	if err != nil {
		return err
	}
	if err := writeAtomic(files, db.path, bytes, noSync); err != nil {
		return err
	}
	db.pageCount = uint32(len(bytes) / int(db.pageSize))
	// create writes an empty database: every body page is live, and every one is a catalog page.
	body := db.pageCount - capOf(db.pageSize, codec != nil).firstBody()
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
	live, err := db.live.after(write, db.committed, db.paging)
	if err != nil {
		return err
	}
	st := storage{
		pageSize: db.pageSize, pageCount: db.pageCount, freePages: db.freePages,
		paging: db.paging, liveAtCompaction: db.liveAtCompaction, freeGenTxid: db.freeGenTxid,
		path: db.path, dirSynced: db.dirSynced,
	}
	err = st.commitFile(snap, write, live, db.openStreams == 0, true, nil)
	db.dirSynced = st.dirSynced
	if err != nil {
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
	db.releaseTempDomain()
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
func writeAtomic(files fileHost, path string, bytes []byte, noSync bool) error {
	tmp := path + ".jedtmp"
	if err := files.writeFile(tmp, "", noSync, func(writeAt func(off int64, p []byte) error) error {
		return writeAt(0, bytes)
	}); err != nil {
		files.remove(tmp)
		return err
	}
	if err := files.rename(tmp, path); err != nil {
		files.remove(tmp)
		return err
	}
	// Directory fsync makes the rename itself durable. Best-effort: not every platform allows
	// opening a directory for fsync (Windows), and the rename is already atomic there. Skipped under
	// noSync (fsync=off).
	if !noSync {
		files.syncDir(filepath.Dir(path))
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
func compactFile(files fileHost, path string, snap *snapshot, pageSize uint32, txid uint64, old *sharedPaging, capacity int) (*sharedPaging, error) {
	var noSync bool
	_ = old.withPager(func(p *pager) error { noSync = p.skipsSync(); return nil })
	// Rename over the real file, never over a symlink naming it (the symlink keeps pointing at it).
	real, err := files.realPath(path)
	if err != nil {
		return nil, err
	}
	tmp := real + ".jedtmp"
	// An encrypted file keeps its data key and key block; every page is resealed under fresh nonces
	// (spec/design/encryption.md §5).
	codec := old.codec()
	if err := files.writeFile(tmp, real, noSync, func(writeAt func(off int64, p []byte) error) error {
		_, err := snap.writeImage(pageSize, txid, codec, func(index uint32, page []byte) error {
			return writeAt(int64(index)*int64(pageSize), page)
		})
		return err
	}); err != nil {
		files.remove(tmp)
		return nil, err
	}

	// Close the old file before the rename: Windows cannot replace a file that is still open.
	_ = old.withPager(func(p *pager) error { return p.swapStore(closedBlockStore{}).close() })
	if err := files.rename(tmp, real); err != nil {
		files.remove(tmp)
		_ = old.withPager(func(p *pager) error {
			store, oerr := files.openStore(real, noSync, false)
			if oerr != nil {
				p.poison()
				return nil
			}
			p.swapStore(store)
			return nil
		})
		return nil, err
	}
	_ = old.withPager(func(p *pager) error { p.poison(); return nil })
	// Directory fsync makes the rename itself durable (best-effort, as in writeAtomic).
	if !noSync {
		files.syncDir(filepath.Dir(real))
	}
	store, err := files.openStore(real, noSync, false)
	if err != nil {
		return nil, err
	}
	p, err := pagerFromStore(store)
	if err != nil {
		_ = store.close()
		return nil, err
	}
	p.codec = codec
	return newSharedPaging(p, capacity), nil
}

// encrypted reports whether this database's file is encrypted (spec/fileformat/encrypted.md).
func (db *engine) encrypted() bool {
	return db.paging != nil && db.paging.codec() != nil
}

func ioError(err error) error {
	return newError(IoError, fmt.Sprintf("I/O error: %v", err))
}
