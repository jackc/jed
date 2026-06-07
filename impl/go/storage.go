package jed

// In-memory storage seam (CLAUDE.md §9). A table's rows are held in a PMap — a persistent
// (copy-on-write) ordered map keyed by the primary-key encoding (spec/design/encoding.md), so
// iteration is in key order (the order-preserving encoding makes that the correct logical order
// with no comparator) and the whole store is an O(1) clone that snapshots independently of its
// source. That cheap, structurally-shared clone is what carries the §3 staging-buffer /
// transaction model (spec/design/transactions.md §2): a TableStore clone is the committed
// version a reader holds while a writer mutates its own copy.

// Row is a stored row: one value per column, in column order.
type Row []Value

// TableStore holds one table's rows, keyed by encoded primary key. Since Phase 6 (P6.1) the PMap is
// the page-backed B-tree, so the store carries the page payload cap (= page_size − 12) and the
// column types to weigh each record (recordSize) for the size-driven split (spec/fileformat/format.md).
type TableStore struct {
	rows PMap
	// nextRowid is the next synthetic rowid for a table with no primary key.
	// Monotonic — never reused, so a DELETE-then-INSERT cannot collide with a freed
	// key. Unused for tables with a primary key. Reconstructed on load
	// (spec/fileformat).
	nextRowid int64
	// cap is the page payload capacity C = page_size − 12 (the split threshold). Fixed for the
	// database's life. colTypes are the column types, for computing record weights.
	cap      int
	colTypes []ScalarType
}

// NewTableStore builds an empty store for a table whose columns have the given types, serializing at
// page payload cap (= page_size − 12).
func NewTableStore(cap int, colTypes []ScalarType) *TableStore {
	return &TableStore{cap: cap, colTypes: colTypes}
}

// clone returns an independent O(1) snapshot of the store: the PMap value-copy shares structure
// (nodes are immutable), so mutating one store leaves the clone untouched. The foundation of the
// transaction model (spec/design/transactions.md §2).
func (s *TableStore) clone() *TableStore {
	return &TableStore{rows: s.rows, nextRowid: s.nextRowid, cap: s.cap, colTypes: s.colTypes}
}

// weight is this row's on-disk record size — the weight the page-backed B-tree splits on.
func (s *TableStore) weight(key []byte, row Row) uint32 {
	return uint32(recordSize(s.colTypes, key, row))
}

// Insert adds a row under its encoded key. Returns false if the key already exists
// (primary-key uniqueness); the caller decides how to surface that.
func (s *TableStore) Insert(key []byte, row Row) bool {
	if _, ok := s.rows.Get(key); ok {
		return false
	}
	s.rows.Insert(key, row, s.weight(key, row), s.cap)
	return true
}

// AllocRowid returns the next monotonic rowid (for a table with no primary key) and
// advances the counter. Never returns a previously-issued value.
func (s *TableStore) AllocRowid() int64 {
	r := s.nextRowid
	s.nextRowid++
	return r
}

// BumpRowidTo ensures the rowid counter is at least n (used on load to set it past
// every rowid already present, so future inserts don't collide).
func (s *TableStore) BumpRowidTo(n int64) {
	if n > s.nextRowid {
		s.nextRowid = n
	}
}

// Replace overwrites the row stored at an existing key (UPDATE). The key is
// unchanged, so key order and the rowid counter are untouched.
func (s *TableStore) Replace(key []byte, row Row) {
	s.rows.Insert(key, row, s.weight(key, row), s.cap)
}

// Remove deletes the row at key (DELETE). Returns whether a row was present.
func (s *TableStore) Remove(key []byte) bool {
	_, ok := s.rows.Remove(key, s.cap)
	return ok
}

// Get looks up a row by its exact encoded key.
func (s *TableStore) Get(key []byte) (Row, bool) {
	return s.rows.Get(key)
}

// IterInKeyOrder returns the rows in primary-key (encoded byte) order.
func (s *TableStore) IterInKeyOrder() []Row {
	_, vals := s.rows.inorder()
	return vals
}

// NodeCount is the number of B-tree nodes (pages) in this store — the page_read count a full
// scan charges (spec/design/cost.md §3 "page_read"). 0 for an empty table.
func (s *TableStore) NodeCount() int { return s.rows.nodeCount() }

// Entry is one stored (encoded key, row) pair.
type Entry struct {
	Key []byte
	Row Row
}

// EntriesInKeyOrder returns all (key, row) pairs in encoded-key order. Used by the
// on-disk serializer (spec/fileformat/format.md), which stores each row's key
// verbatim (the key is not always reconstructable from the row — e.g. a no-PK
// table's synthetic rowid).
func (s *TableStore) EntriesInKeyOrder() []Entry {
	keys, vals := s.rows.inorder()
	out := make([]Entry, len(keys))
	for i := range keys {
		out[i] = Entry{Key: keys[i], Row: vals[i]}
	}
	return out
}

// treeRoot is the root B-tree node of this store, for the page-backed serializer
// (spec/fileformat/format.md). nil for an empty table.
func (s *TableStore) treeRoot() *pnode { return s.rows.rootNode() }

// setTree installs a loaded B-tree as this store's contents (format.go LoadDatabase).
func (s *TableStore) setTree(root *pnode, length int) { s.rows = fromLoaded(root, length) }

// Len returns the row count.
func (s *TableStore) Len() int { return s.rows.Len() }
