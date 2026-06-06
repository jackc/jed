package jed

// Host file layer for the Go core (spec/design/api.md §2): open/create/commit/close a
// single-file database durably (whole-image model). Pure os — no cgo, no FFI (CLAUDE.md §2),
// fully memory-safe. The crash-safe commit is temp-file + fsync + atomic rename + directory
// fsync (api.md §3); since a commit rewrites the whole file, rename gives all-or-nothing
// replacement for free.

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
	db.txid = 0
	if err := db.persist(); err != nil { // materialize the empty image (txid 0 -> 1)
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

// persist durably writes the whole current image to the backing file and increments txid — the
// single synchronous-commit chokepoint (spec/design/transactions.md §9). Called by Create (the
// initial image) and by the autocommit path after every successful write statement. An
// in-memory database (no path) is a no-op success. The future synchronous=off mode
// (batched/deferred fsync) gates here.
func (db *Database) persist() error {
	if db.path == "" {
		return nil
	}
	nextTxid := db.txid + 1
	bytes, err := db.ToImage(db.pageSize, nextTxid)
	if err != nil {
		return err
	}
	if err := writeAtomic(db.path, bytes); err != nil {
		return err
	}
	db.txid = nextTxid
	return nil
}

// Commit commits the current transaction (spec/design/api.md §2.2). jed autocommits each
// statement (transactions.md §4.1), so in this slice there is no open explicit transaction to
// publish — Commit is a lenient no-op success (transactions.md §4.2). Explicit BEGIN … COMMIT
// blocks, where Commit does the durable publish, arrive in P5.2.
func (db *Database) Commit() error { return nil }

// Rollback rolls back the current transaction (spec/design/api.md §2.2). With autocommit and no
// open explicit transaction (this slice), there is nothing uncommitted to discard — a no-op
// success. Discarding an open explicit block's working set arrives with BEGIN in P5.2.
func (db *Database) Rollback() error { return nil }

// Close releases the handle (spec/design/api.md §2.3). Under autocommit, every prior statement
// is already durable, so — unlike the original model — Close does NOT drop committed work; it
// would roll back an open explicit transaction (none in this slice). Durability is never hidden
// in a destructor. Idempotent.
func (db *Database) Close() error {
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
