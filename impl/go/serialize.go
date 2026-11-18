package jed

import (
	"context"
	"errors"
)

// DeserializeOptions are the settings of an in-memory database built by [Deserialize]. The page
// size and contents come from the image itself.
type DeserializeOptions struct {
	// Environment supplies the loaded Unicode and time-zone data, host functions, and random and
	// clock sources, as for CreateOptions.Environment; the image's collation references resolve
	// against it. nil uses a fresh, empty Environment.
	Environment *Environment
	// MaxStorageBytes is the committed-storage limit in bytes, as for
	// CreateOptions.MaxStorageBytes. 0 selects the default, no limit; Unlimited also sets none.
	MaxStorageBytes int64
}

// Deserialize builds a new in-memory database whose initial state is the database image data:
// the bytes of a jed database file, or what [Database.Serialize] returned. data is copied, so the
// caller keeps its buffer. Later commits change only the in-memory copy; call Serialize again to
// get them back as bytes. A malformed image fails with SQLSTATE XX001, and an encrypted one with
// 55000 (an in-memory database cannot be encrypted).
func Deserialize(data []byte, opts DeserializeOptions) (*Database, error) {
	if err := opts.normalizeLimits(); err != nil {
		return nil, err
	}
	env := opts.Environment
	if env == nil {
		env = NewEnvironment()
	}
	e, err := loadEngine(data, env)
	if err != nil {
		var ee *EngineError
		if errors.As(err, &ee) && ee.State == CredentialRequired {
			return nil, errCredentialWithoutFile()
		}
		return nil, err
	}
	c := sharedCoreFromEngine(e)
	c.storage.budget.limit = opts.MaxStorageBytes
	return databaseOver(c), nil
}

// Serialize returns the committed state of database name ("main" or an attachment) as bytes:
// a compact image with no dead pages, which [Deserialize] loads and which is also a valid
// database file. The same committed state always produces the same bytes. Serialize reads a
// snapshot and never waits for the writer slot: commits that land meanwhile are not included,
// and neither are an open transaction's uncommitted changes. ctx is checked before the export
// starts. It fails with SQLSTATE 42704 for a name that is not attached (including temp) and
// 0A000 for an encrypted database, since the image would be plaintext. Serialize is not
// reachable from SQL and is not charged to any cost limit.
func (db *Database) Serialize(ctx context.Context, name string) ([]byte, error) {
	if err := doneErr(contextDone(ctx)); err != nil {
		return nil, contextError(ctx, err)
	}
	c := db.core
	var att *attachment
	if name != "main" {
		att = c.attachment(name)
		if att == nil || name == "temp" {
			return nil, newError(UndefinedObject, `database "`+name+`" is not attached`)
		}
		if att.svc != nil {
			// A file attachment under the page service is exported through its own handle.
			return att.svc.Serialize(ctx, "main")
		}
	}
	var rt *roots
	var v, gen uint64
	st := c.storage
	if att == nil {
		var err error
		if rt, v, gen, err = c.refreshAndPin(); err != nil {
			return nil, err
		}
	} else {
		rt, v, gen = c.pinLatest()
		st = att.storage
	}
	// The pin keeps the snapshot's pages from reuse while the image reads them.
	defer c.deregisterPin(gen, v)
	snap := rt.committed
	if att != nil {
		if snap = rt.attached[name]; snap == nil {
			return nil, newError(UndefinedObject, `database "`+name+`" is not attached`)
		}
	}
	st.mu.Lock()
	pageSize, encrypted := st.pageSize, st.paging != nil && st.paging.codec() != nil
	st.mu.Unlock()
	if encrypted {
		return nil, newError(FeatureNotSupported, `cannot serialize encrypted database "`+name+`"`)
	}
	return snap.ToImage(pageSize, snap.txid)
}
