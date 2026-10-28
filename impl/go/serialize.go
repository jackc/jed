package jed

import "errors"

// DeserializeOptions are the handle settings of an in-memory database built from bytes
// (spec/design/api.md §2.8). Its page size and contents come from the image.
type DeserializeOptions struct {
	// Environment is the environment the database runs in (spec/design/environment.md), as for
	// CreateOptions.Environment; the image's collation references resolve against it. nil ⇒ a fresh
	// empty environment.
	Environment *Environment
	// MaxStorageBytes is the committed-storage limit in bytes (spec/design/memory.md §8), as for
	// CreateOptions.MaxStorageBytes. Zero or negative is unlimited (the default).
	MaxStorageBytes int64
}

// DeserializeDatabase builds a new in-memory database whose initial state is the database image data
// (spec/design/api.md §2.8; SQLite's sqlite3_deserialize): the bytes of a jed database file, or what
// Database.Serialize returned. The bytes are copied — the caller keeps its buffer — and the image is
// opened as a file is, from its newer valid meta slot, at the image's own page size and version.
// Later commits change only the in-memory copy; serialize again to get them back as bytes. A
// malformed image is XX001; an encrypted one is 55000 (an in-memory database cannot be encrypted).
func DeserializeDatabase(data []byte, opts DeserializeOptions) (*Database, error) {
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

// Serialize exports database name — `main` or an attachment, exact name — as bytes
// (spec/design/api.md §2.8; SQLite's sqlite3_serialize): the clean from-scratch image of its
// committed state, at its page size and committed version, which DeserializeDatabase loads and which
// is also a valid database file. The image holds no dead pages and is byte-identical in every core
// for the same committed state. The export pins the committed snapshot as a read session does and
// never takes the writer gate: commits proceed meanwhile and are not included, and an open
// transaction's uncommitted changes never are. Errors: 42704 for a name that is not attached (and
// for temp), 0A000 for an encrypted database (the export would be plaintext). A host-API act — not
// metered and not reachable from SQL.
func (db *Database) Serialize(name string) ([]byte, error) {
	c := db.core
	var att *attachment
	if name != "main" {
		att = c.attachment(name)
		if att == nil || name == "temp" {
			return nil, newError(UndefinedObject, `database "`+name+`" is not attached`)
		}
		if att.svc != nil {
			// A file attachment under the page service is exported through its own handle.
			return att.svc.Serialize("main")
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
