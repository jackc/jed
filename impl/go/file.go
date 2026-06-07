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

// DatabaseOptions are the settings for a newly-created database file (spec/design/api.md §2).
// PageSize is fixed into the file's meta at creation and cannot change thereafter.
type DatabaseOptions struct {
	PageSize uint32
}

// DefaultDatabaseOptions returns the default create settings (the default page size).
func DefaultDatabaseOptions() DatabaseOptions {
	return DatabaseOptions{PageSize: DefaultPageSize}
}

// Create makes a new file-backed database at path with opts (the page size is locked into the
// file). The path must not already exist — 58P02 otherwise. An initial empty image is written
// durably immediately, so the file exists with its page size fixed (api.md §2).
func Create(path string, opts DatabaseOptions) (*Database, error) {
	if _, err := os.Stat(path); err == nil {
		return nil, NewError(DuplicateFile, "database file already exists: "+path)
	} else if !errors.Is(err, fs.ErrNotExist) {
		return nil, ioError(err)
	}
	db := NewDatabase()
	db.path = path
	db.pageSize = opts.PageSize
	db.committed.txid = 1                       // the initial empty image is committed as txid 1
	if err := db.writeFullImage(); err != nil { // lay down the from-scratch image; later commits are incremental
		return nil, err
	}
	return db, nil
}

// Open opens an existing file-backed database at path (loading its committed state and adopting
// its page size / txid). The path must exist — 58P01 otherwise; a malformed file is XX001, a
// read failure 58030 (api.md §2).
func Open(path string) (*Database, error) {
	bytes, err := os.ReadFile(path)
	if err != nil {
		if errors.Is(err, fs.ErrNotExist) {
			return nil, NewError(UndefinedFile, "database file does not exist: "+path)
		}
		return nil, ioError(err)
	}
	db, err := LoadDatabase(bytes)
	if err != nil {
		return nil, err
	}
	db.path = path
	return db, nil
}

// writeFullImage lays down the whole from-scratch image of the committed snapshot (the all-dirty
// special case — spec/fileformat/format.md) durably via temp-file + rename, and records the on-disk
// page high-water. Used by Create to establish a fresh file with both meta slots seeded; every later
// commit is incremental (persist).
func (db *Database) writeFullImage() error {
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
// synchronous-commit chokepoint. Append the dirty pages this transaction introduced, Sync, write the
// alternate meta slot (snap.txid & 1), Sync. Clean pages are never rewritten; pages an old root drops
// are leaked (P6.2 reclaims). A crash between the two syncs leaves the prior meta — and thus the prior
// snapshot — intact (the body pages were only appended). An in-memory database (no path) is a no-op
// success: it does not mutate db, and the committed swap happens in commitTx only after this returns
// nil. db.pageCount advances only after both syncs succeed, so a write failure leaves db, committed,
// and the file's prior meta untouched (the working snapshot is then discarded). The future
// synchronous=off mode gates here.
func (db *Database) persist(snap *Snapshot) error {
	if db.path == "" {
		return nil
	}
	write, err := snap.incrementalImage(db.pageSize, db.pageCount)
	if err != nil {
		return err
	}
	f, err := os.OpenFile(db.path, os.O_RDWR, 0)
	if err != nil {
		return ioError(err)
	}
	defer func() { _ = f.Close() }()
	ps := int64(db.pageSize)
	for _, pg := range write.pages {
		if _, err := f.WriteAt(pg.bytes, int64(pg.index)*ps); err != nil {
			return ioError(err)
		}
	}
	if err := f.Sync(); err != nil { // body pages durable before the meta can reference them
		return ioError(err)
	}
	meta := metaPage(db.pageSize, snap.txid, write.rootPage, write.pageCount)
	if _, err := f.WriteAt(meta, int64(snap.txid&1)*ps); err != nil {
		return ioError(err)
	}
	if err := f.Sync(); err != nil { // the commit is published
		return ioError(err)
	}
	db.pageCount = write.pageCount
	return nil
}

// Commit commits the current transaction (spec/design/api.md §2.2, transactions.md §4.2).
// Publishes the open explicit block durably (per synchronous); a Commit with no open block is a
// lenient no-op success (under autocommit each statement already committed). Drives the same
// mechanism as SQL COMMIT.
func (db *Database) Commit() error {
	_, err := db.commitTx()
	return err
}

// Rollback rolls back the current transaction (spec/design/api.md §2.2, transactions.md §4.2).
// Discards the open explicit block's working set; a Rollback with no open block is a no-op
// success. Drives the same mechanism as SQL ROLLBACK.
func (db *Database) Rollback() error {
	_, err := db.rollbackTx()
	return err
}

// Close releases the handle (spec/design/api.md §2.3). It rolls back any open explicit
// transaction (its in-progress work is discarded) and does not commit one. Under autocommit every
// prior statement is already durable, so — unlike the original model — Close does NOT drop
// committed work; durability is never hidden in a destructor. Idempotent.
func (db *Database) Close() error {
	_, _ = db.rollbackTx()
	db.path = ""
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
	return NewError(IoError, fmt.Sprintf("I/O error: %v", err))
}
