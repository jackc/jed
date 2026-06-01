package abide

import "sort"

// In-memory storage seam (CLAUDE.md §9). Step-5a is in-memory only; on-disk
// persistence (the block device, the byte format, the Rust↔Go round-trip) is
// step-5b behind this same seam. Rows are keyed by their primary-key encoding
// (spec/design/encoding.md); iteration is in key order. Go has no sorted map, so
// rows are held in a map keyed by the encoded key as a string and sorted on
// iteration — string comparison in Go is bytewise, matching memcmp order.

// Row is a stored row: one value per column, in column order.
type Row []Value

// TableStore holds one table's rows, keyed by encoded primary key.
type TableStore struct {
	rows map[string]Row
	// nextRowid is the next synthetic rowid for a table with no primary key.
	// Monotonic — never reused, so a DELETE-then-INSERT cannot collide with a freed
	// key. Unused for tables with a primary key. Reconstructed on load
	// (spec/fileformat).
	nextRowid int64
}

// NewTableStore builds an empty store.
func NewTableStore() *TableStore {
	return &TableStore{rows: make(map[string]Row)}
}

// Insert adds a row under its encoded key. Returns false if the key already exists
// (primary-key uniqueness); the caller decides how to surface that.
func (s *TableStore) Insert(key []byte, row Row) bool {
	k := string(key)
	if _, ok := s.rows[k]; ok {
		return false
	}
	s.rows[k] = row
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
	s.rows[string(key)] = row
}

// Remove deletes the row at key (DELETE). Returns whether a row was present.
func (s *TableStore) Remove(key []byte) bool {
	k := string(key)
	if _, ok := s.rows[k]; !ok {
		return false
	}
	delete(s.rows, k)
	return true
}

// Get looks up a row by its exact encoded key.
func (s *TableStore) Get(key []byte) (Row, bool) {
	r, ok := s.rows[string(key)]
	return r, ok
}

// IterInKeyOrder returns the rows in primary-key (encoded byte) order.
func (s *TableStore) IterInKeyOrder() []Row {
	keys := make([]string, 0, len(s.rows))
	for k := range s.rows {
		keys = append(keys, k)
	}
	sort.Strings(keys) // bytewise == memcmp == order-preserving key order
	out := make([]Row, 0, len(keys))
	for _, k := range keys {
		out = append(out, s.rows[k])
	}
	return out
}

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
	keys := make([]string, 0, len(s.rows))
	for k := range s.rows {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	out := make([]Entry, 0, len(keys))
	for _, k := range keys {
		out = append(out, Entry{Key: []byte(k), Row: s.rows[k]})
	}
	return out
}

// Len returns the row count.
func (s *TableStore) Len() int { return len(s.rows) }
