package jed

// dbHandle is the small surface that shared white-box test helpers drive. The public *Session and
// *Database (the surface feature tests now run through) and the internal *engine (the storage/crash
// tests that legitimately stay white-box — the Go analogue of Rust's in-`src` #[cfg(test)] mods) all
// satisfy it, so a helper like queryOutcome/mustExec works from either world without a per-partition
// copy. It is test-only; production code never sees it.
//
// The surface is the real production seam — queryValues (sql, []Value) -> *Rows, the one total
// exec/query path callers use. White-box tests drain it through queryOutcome (helpers_test.go) into a
// materialized outcome for assertions; there is no test-only Execute path any more (the removed
// Execute/Outcome public API — a statement is observably a *Rows with no output columns).
type dbHandle interface {
	queryValues(sql string, params []Value) (*Rows, error)
	TableNames() []string
	Table(name string) (*catTable, bool)
	CompositeType(name string) *compositeType
	RowsInKeyOrder(name string) []storedRow
	ToImage(pageSize uint32, txid uint64) ([]byte, error)
	Txid() uint64
	PageSize() uint32
	PageCount() uint32
}

var (
	_ dbHandle = (*Session)(nil)
	_ dbHandle = (*engine)(nil)
	_ dbHandle = (*Database)(nil)
)
