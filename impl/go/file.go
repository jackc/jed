package jed

// Host file layer for the Go core (spec/design/api.md §2): open/create/commit/close a single-file
// database durably. Pure os — no cgo, no FFI (CLAUDE.md §2), fully memory-safe. Create lays down the
// from-scratch image (temp-file + fsync + atomic rename + directory fsync, api.md §3); every later
// commit is an incremental copy-on-write write of just the dirty pages, published by alternating the
// meta slot (spec/fileformat/format.md, P6.1 part B) — the block seam below pwrites pages (WriteAt)
// into the open file rather than rewriting it.

import (
	"bytes"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"time"
)

// Locking selects how a file database coordinates with other processes that open the same file.
// Locking is a property of the handle and is not stored in the file. On a platform without file
// locks, LockingAuto and LockingExclusive fail with SQLSTATE 0A000.
type Locking uint8

// The locking modes.
const (
	// LockingAuto, the default, lets several processes share the file: the first opener owns it
	// and serves it to the others, which read and commit through the owner. Where the platform
	// cannot serve a file, LockingAuto behaves as LockingExclusive.
	LockingAuto Locking = iota
	// LockingExclusive gives the file to one process at a time: a second opener waits up to its
	// FileLockTimeout, then fails with SQLSTATE 55006.
	LockingExclusive
	// LockingNone skips coordination entirely. It is unsafe if another process opens the file
	// at the same time; use it only when something else guarantees exclusive access.
	LockingNone
)

// Synchronous is the durability mode of a file-backed handle: whether a commit waits until it is
// durable on disk before returning. It is a handle setting, not stored in the file.
type Synchronous uint8

// The durability modes.
const (
	// SynchronousOn, the default, makes every commit durable before it returns.
	SynchronousOn Synchronous = iota
	// SynchronousOff returns from a commit once its data is written, without waiting for it to
	// reach the disk, and makes a batch of commits durable together later: when FlushInterval
	// passes, the batch grows large, or the handle closes. A power loss or OS crash can lose the
	// commits acknowledged since the last flush, but never corrupts the file.
	SynchronousOff
)

// DefaultFlushInterval is the default FlushInterval: how long SynchronousOff commits may wait
// before they are made durable.
const DefaultFlushInterval = time.Second

// flushIntervalMs resolves a FlushInterval option: nil is DefaultFlushInterval.
func flushIntervalMs(opt *time.Duration) uint64 {
	if opt == nil {
		return durationMillis(DefaultFlushInterval)
	}
	return durationMillis(*opt)
}

// ScratchEncryption selects whether a handle encrypts its scratch files: the temporary files that
// sorts, hash joins, and large temporary tables spill to. It is fixed when the handle is opened
// or created.
type ScratchEncryption uint8

// The scratch-file encryption modes.
const (
	// ScratchEncryptionAuto, the default, encrypts scratch files when the main database is
	// encrypted.
	ScratchEncryptionAuto ScratchEncryption = iota
	// ScratchEncryptionOn always encrypts scratch files. Use it on a plaintext or in-memory
	// database that will attach encrypted files.
	ScratchEncryptionOn
)

// scratchHost is the handle's scratch target: private files in the OS temp directory, sealed when
// encrypt with keys from a scratch stream of env's key entropy.
func scratchHost(encrypt bool, env *Environment) spillHost {
	return sealScratch(osSpillHost{dir: os.TempDir()}, encrypt, env)
}

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
	// An encrypted database never spills plaintext: its scratch files are sealed
	// (spec/design/encryption.md §6).
	db.spill = scratchHost(codec != nil, env)
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
	db.paging = newSharedPaging(p, cacheBudget(defaultCacheBytes))
	db.committed.storePaging = db.paging
	return db, nil
}

// OpenOptions are the settings for opening a file database with [Open]. They belong to the
// handle and are not stored in the file, so another process may open the same file with
// different ones. The zero value selects every default.
//
// A nil duration pointer selects the field's default and a pointer to zero is an explicit zero.
// Negative durations fail with SQLSTATE 22023, even in modes that ignore them, and positive
// durations round up to whole milliseconds. Size and count limits follow the [Unlimited] rule.
type OpenOptions struct {
	// CacheBytes is the page-cache budget in bytes: roughly the most memory the handle uses to
	// keep database pages in memory. It lets a database much larger than RAM be served and never
	// changes query results. 0 selects the default, 256 MiB; Unlimited keeps every page it reads.
	CacheBytes int
	// ReadOnly opens the file without write access, so it is never written and may sit on a
	// read-only filesystem. Like a PostgreSQL hot standby, every transaction defaults to read-only,
	// and a READ WRITE request or any write statement fails with SQLSTATE 25006.
	ReadOnly bool
	// WorkMem is the default memory budget, in bytes, of a sort or other blocking operator before
	// it spills to scratch files; a session can change its own with [Session.SetWorkMem]. It never
	// changes query results. 0 selects the default, 256 MiB; Unlimited never spills.
	WorkMem int
	// SkipFsync turns off the fsync after each commit, which makes commits much faster. It is for
	// development and testing only: committed data survives a process crash but not an OS crash
	// or power loss. It never changes the bytes written or query results. The default is false.
	SkipFsync bool
	// Synchronous is the durability mode, SynchronousOn by default. The handle's file attachments
	// use the same mode.
	Synchronous Synchronous
	// FlushInterval is how long SynchronousOff commits may wait before they are made durable,
	// measured on the Environment's clock. nil selects DefaultFlushInterval (one second); a pointer
	// to zero turns the time-based flush off. It is ignored under SynchronousOn.
	FlushInterval *time.Duration
	// Locking selects how the handle coordinates with other processes; LockingAuto by default.
	Locking Locking
	// FileLockTimeout is how long Open waits for another process that holds the file. When it
	// passes, Open fails with SQLSTATE 55006. nil selects the default, five seconds; a pointer to
	// zero tries once without waiting.
	FileLockTimeout *time.Duration
	// IdleInTransactionTimeout applies while this handle owns a shared file under LockingAuto: a
	// client process that holds an open write transaction and sends nothing for this long loses
	// the transaction, and its next request fails with SQLSTATE 25P03. nil selects the default,
	// 60 seconds; a pointer to zero disables the timeout.
	IdleInTransactionTimeout *time.Duration
	// IdleSessionTimeout applies while this handle owns a shared file under LockingAuto: it closes
	// a client connection that stays idle between transactions for this long. nil or a pointer to
	// zero never closes one.
	//
	// IdleInTransactionTimeout, IdleSessionTimeout, YieldGrace, ShutdownGrace, and MaxConnections
	// are kept by a handle that does not own the file, and take effect if it becomes the owner.
	IdleSessionTimeout *time.Duration
	// YieldGrace is how long an owner handing the file to another process lets running
	// transactions finish. nil selects the default, five seconds.
	YieldGrace *time.Duration
	// ShutdownGrace is how long a closing owner lets running transactions finish. nil selects the
	// default, 30 seconds.
	ShutdownGrace *time.Duration
	// MaxConnections limits how many client connections an owner accepts; a connection beyond it
	// fails with SQLSTATE 53300. 0 selects the default, no limit; Unlimited also sets none.
	MaxConnections int
	// Ownership is the handle's preference for owning the file under LockingAuto:
	// OwnershipPrefer (the default), OwnershipYield, or OwnershipNever.
	Ownership Ownership
	// Environment supplies the loaded Unicode and time-zone data the file's collations and zones
	// resolve against, the host functions, and the random and clock sources. Every session,
	// attachment, and temporary table of the handle shares it. It is not stored in the file, so a
	// host that reopens the file brings its own. nil uses a fresh, empty Environment.
	Environment *Environment
	// MaxStorageBytes limits the committed storage of the file, in bytes, counted as live pages
	// times the page size: a commit that would grow it past the limit fails with SQLSTATE 54P06.
	// 0 selects the default, no limit; Unlimited also sets none. It is not stored in the file;
	// Database.SetMaxStorageBytes changes it later.
	MaxStorageBytes int64
	// Credential unlocks an encrypted file. Opening an encrypted file without one fails with
	// SQLSTATE 28P02, with one that does not match fails with 28P01, and passing one for a
	// plaintext file fails with 28P03. It is never stored. When several processes share the file,
	// each opens it with its own credential.
	Credential *Credential
	// ScratchEncryption selects whether the handle encrypts its scratch files; by default it does
	// when the file is encrypted. Attaching an encrypted file needs a handle that encrypts its
	// scratch files, or it fails with SQLSTATE 55000.
	ScratchEncryption ScratchEncryption
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
// (P6.4b). The pool charges each resident leaf the bytes it holds against the byte budget.
// The budget is a handle setting, not stored in the file (§3). Later commits write through the same
// pager kept open for the handle's life.
func openWithOptions(path string, opts OpenOptions) (*engine, error) {
	return openVia(osFiles, path, opts)
}

// openVia is openWithOptions through the file host files (filehost.go).
func openVia(files fileHost, path string, opts OpenOptions) (*engine, error) {
	return openViaHeld(files, path, opts, nil)
}

// openViaHeld is openVia for a page client promoting itself to owner with the data key it already
// holds (held, spec/design/encryption.md §8): the DEK alone opens the file, so a credential changed
// since the client unlocked does not stop it. The held codec adopts the file's selected key-block copy,
// and a differing pair is left unrepaired (a process without the credential cannot know which copy it
// would be writing).
func openViaHeld(files fileHost, path string, opts OpenOptions, held *pageCodec) (*engine, error) {
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
	// LoadEnginePaged rejects an out-of-range page size as corrupt.
	env := opts.Environment
	if env == nil {
		env = NewEnvironment()
	}
	var db *engine
	if held != nil {
		if err = adoptHeldCodec(p, held); err == nil {
			db, err = loadEngineSharedPaging(newSharedPaging(p, cacheBudget(cacheBytes)), env)
			// A meta the held key cannot authenticate means the owner rekeyed the file before it left:
			// 28P04, so the host reopens with the credential (spec/fileformat/encrypted.md §14).
			var e *EngineError
			if errors.As(err, &e) && e.Code() == "XX001" && heldKeyStale(p) {
				err = errStaleKey()
			}
		}
	} else {
		db, err = loadEnginePagedCredential(p, cacheBudget(cacheBytes), env, opts.Credential)
	}
	if err != nil {
		_ = store.close()
		return nil, err
	}
	db.path = path
	if held == nil && !opts.ReadOnly {
		if err := db.repairKeyBlock(); err != nil {
			_ = db.Close()
			return nil, err
		}
	}
	// An encrypted database seals its scratch files (spec/design/encryption.md §6).
	db.spill = scratchHost(db.encrypted() || opts.ScratchEncryption == ScratchEncryptionOn, env)
	db.readOnly = opts.ReadOnly
	applyOpenWorkMem(db, opts.WorkMem)
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
	if incrOn() { // every body page of a fresh image is a catalog page
		in := &incrState{known: true}
		for p := capOf(db.pageSize, codec != nil).firstBody(); p < db.pageCount; p++ {
			in.catalog = append(in.catalog, p)
		}
		db.incr = in
	}
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
	st := storage{
		pageSize: db.pageSize, pageCount: db.pageCount, freePages: db.freePages, freeChain: db.freeChain,
		paging: db.paging, liveAtCompaction: db.liveAtCompaction, freeGenTxid: db.freeGenTxid,
		path: db.path, dirSynced: db.dirSynced, incr: db.incr, live: db.live,
	}
	var live liveCount
	if st.incrActive() { // incr_reclaim.go
		st.incrMerge(snap.txid) // a bare engine reuses ungated (oldest_live == committed)
		live, st.incr.tree, err = db.live.afterIDs(write, db.committed, db.paging)
	} else {
		live, err = db.live.after(write, db.committed, db.paging)
	}
	if err != nil {
		return err
	}
	_, err = st.commitFile(snap, write, live, db.openStreams == 0, true, nil)
	db.dirSynced = st.dirSynced
	if err != nil {
		return err
	}
	db.pageCount, db.freePages, db.freeChain = st.pageCount, st.freePages, st.freeChain
	db.liveAtCompaction, db.freeGenTxid = st.liveAtCompaction, st.freeGenTxid
	db.live = st.live
	db.incr = st.incr
	return nil
}

// ResidentLeaves is the number of leaf pages currently resident in the buffer pool — 0 for an
// in-memory database (it is fully resident, nothing to page). The OpenOptions.CacheBytes budget bounds
// the bytes those leaves are charged together (or one leaf, whatever the budget — spec/design/pager.md
// §3), so the count is at most about CacheBytes divided by a leaf's resident size.
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
	// An encrypted file keeps its data key and key block; every page is resealed under fresh nonces
	// (spec/design/encryption.md §5).
	return rewriteFile(files, path, snap, pageSize, txid, old, capacity, old.codec(), "", 0)
}

// rewriteFile is compactFile under codec (nil: plaintext) instead of old's — DEK rotation and
// conversion write the rebuilt snap this way (spec/design/encryption.md §5). With a positive limit,
// an image whose live pages exceed database name's max_storage_bytes fails 54P06 before the rename.
func rewriteFile(files fileHost, path string, snap *snapshot, pageSize uint32, txid uint64, old *sharedPaging, capacity int, codec *pageCodec, name string, limit int64) (*sharedPaging, error) {
	var noSync bool
	_ = old.withPager(func(p *pager) error { noSync = p.skipsSync(); return nil })
	// Rename over the real file, never over a symlink naming it (the symlink keeps pointing at it).
	real, err := files.realPath(path)
	if err != nil {
		return nil, err
	}
	tmp := real + ".jedtmp"
	var pageCount uint32
	if err := files.writeFile(tmp, real, noSync, func(writeAt func(off int64, p []byte) error) error {
		var err error
		pageCount, err = snap.writeImage(pageSize, txid, codec, func(index uint32, page []byte) error {
			return writeAt(int64(index)*int64(pageSize), page)
		})
		return err
	}); err != nil {
		files.remove(tmp)
		return nil, err
	}
	if limit > 0 {
		// A from-scratch image has no free page: every body page is live (memory.md §8.7).
		live := pageCount - capOf(pageSize, codec != nil).firstBody()
		if int64(live)*int64(pageSize) > limit {
			files.remove(tmp)
			return nil, errStorageLimit(name, limit)
		}
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

// repairKeyBlock finishes a credential change a crash interrupted (spec/design/encryption.md §5): when
// the two key-block copies differ, the opener that unwrapped the selected copy writes it over the
// other, between barriers. A read-only open leaves the pair alone, which is safe because selection
// already prefers the newer copy.
func (db *engine) repairKeyBlock() error {
	if !db.encrypted() {
		return nil
	}
	codec := db.paging.codec()
	return db.paging.withPager(func(p *pager) error {
		page2, err := p.readBlock(2)
		if err != nil {
			return err
		}
		page3, err := p.readBlock(3)
		if err != nil {
			return err
		}
		if bytes.Equal(page2, page3) {
			return nil
		}
		stale := unselectedKeyPage(page2, page3)
		if err := p.sync(); err != nil {
			return err
		}
		if err := p.writeBlock(stale, codec.keyBlockPage(int(db.pageSize))); err != nil {
			return err
		}
		return p.sync()
	})
}

// encrypted reports whether this database's file is encrypted (spec/fileformat/encrypted.md).
func (db *engine) encrypted() bool {
	return db.paging != nil && db.paging.codec() != nil
}

func ioError(err error) error {
	return newError(IOError, fmt.Sprintf("I/O error: %v", err))
}
