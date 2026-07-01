package jed

// dbHandle is the small surface that shared white-box test helpers drive. Both the public *Session
// (the envelope feature tests now run through) and the internal *engine (the storage/crash tests that
// legitimately stay white-box — the Go analogue of Rust's in-`src` #[cfg(test)] mods) satisfy it, so a
// helper like queryRows/mustExec works from either world without a per-partition copy. It is
// test-only; production code never sees it.
type dbHandle interface {
	Execute(sql string, params []Value) (Outcome, error)
	TableNames() []string
	Table(name string) (*catTable, bool)
	CompositeType(name string) *compositeType
	RowsInKeyOrder(name string) []storedRow
	ToImage(pageSize uint32, txid uint64) ([]byte, error)
	Txid() uint64
	PageSize() uint32
	PageCount() uint32
}

// Execute lets the internal engine satisfy dbHandle (test-only). It mirrors *Session.Execute's
// signature over the free executeParams helper; the storage/crash tests reach the engine directly, so
// this only exists to share helpers with the Session-routed feature tests.
func (db *engine) Execute(sql string, params []Value) (Outcome, error) {
	return executeParams(db, sql, params)
}

var (
	_ dbHandle = (*Session)(nil)
	_ dbHandle = (*engine)(nil)
	_ dbHandle = (*Database)(nil)
)
