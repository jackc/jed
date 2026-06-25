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
	// database's life. colTypes are the resolved column types (scalar or composite —
	// spec/design/composite.md §4), for computing record weights and the recursive value codec.
	cap      int
	colTypes []ColType
	// paging is the shared pager + leaf buffer pool for a file-backed database (spec/design/pager.md):
	// the read/mutation path faults OnDisk leaves through it. nil for an in-memory database and for a
	// table created in-session (fully resident until the file is reopened); attached by the
	// demand-paged file load. Shared (pointer) — a snapshot clone shares the one pool per database.
	paging *sharedPaging
}

// NewTableStore builds an empty store for a table whose columns have the given resolved types,
// serializing at page payload cap (= page_size − 12). In-memory (no paging) until attachPaging.
func NewTableStore(cap int, colTypes []ColType) *TableStore {
	return &TableStore{cap: cap, colTypes: colTypes}
}

// clone returns an independent O(1) snapshot of the store: the PMap value-copy shares structure
// (nodes are immutable), so mutating one store leaves the clone untouched. The foundation of the
// transaction model (spec/design/transactions.md §2). The shared paging context is shared, not copied
// (one pool per database).
func (s *TableStore) clone() *TableStore {
	return &TableStore{rows: s.rows, nextRowid: s.nextRowid, cap: s.cap, colTypes: s.colTypes, paging: s.paging}
}

// attachPaging attaches this database's shared paging context (the demand-paged file load, format.go):
// the store's OnDisk leaves now fault through the pool. One pool per database, shared by every store
// and snapshot.
func (s *TableStore) attachPaging(p *sharedPaging) { s.paging = p }

// pagedSource is the buffer-pool leaf source for one store (spec/design/pager.md §4): faults a clean
// leaf page through this database's shared pool, decoding it with this table's column types.
type pagedSource struct {
	paging   *sharedPaging
	colTypes []ColType
}

func (ps *pagedSource) loadLeaf(page uint32) (*pnode, error) {
	return ps.paging.faultLeaf(page, ps.colTypes)
}

// leafSrc builds this store's leaf source, or nil (a true nil interface) for an in-memory store that
// never faults.
func (s *TableStore) leafSrc() leafSource {
	if s.paging == nil {
		return nil
	}
	return &pagedSource{paging: s.paging, colTypes: s.colTypes}
}

// weight is this row's on-disk record size — the weight the page-backed B-tree splits on. Accounts
// for out-of-line spill at cap (an externalized value weighs its pointer, not its full body —
// large-values.md §12), so split points match the serialized pages.
func (s *TableStore) weight(key []byte, row Row) uint32 {
	return uint32(recordSize(s.colTypes, key, row, s.cap))
}

// Insert adds a row under its encoded key. Returns (false, nil) if the key already exists
// (primary-key uniqueness); the caller decides how to surface that. May fault the target leaf through
// the buffer pool (an I/O error then propagates).
func (s *TableStore) Insert(key []byte, row Row) (bool, error) {
	src := s.leafSrc()
	if _, ok, err := s.rows.Get(key, src); err != nil {
		return false, err
	} else if ok {
		return false, nil
	}
	if _, _, err := s.rows.Insert(key, row, s.weight(key, row), s.cap, src); err != nil {
		return false, err
	}
	return true, nil
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
// unchanged, so key order and the rowid counter are untouched. May fault the target leaf.
func (s *TableStore) Replace(key []byte, row Row) error {
	_, _, err := s.rows.Insert(key, row, s.weight(key, row), s.cap, s.leafSrc())
	return err
}

// Remove deletes the row at key (DELETE). Returns whether a row was present. May fault leaves the
// delete descends into / rebalances against.
func (s *TableStore) Remove(key []byte) (bool, error) {
	_, ok, err := s.rows.Remove(key, s.cap, s.leafSrc())
	return ok, err
}

// Get looks up a row by its exact encoded key. May fault the holding leaf through the buffer pool.
func (s *TableStore) Get(key []byte) (Row, bool, error) {
	return s.rows.Get(key, s.leafSrc())
}

// IterInKeyOrder returns the rows in primary-key (encoded byte) order. Eager: leaves fault through the
// pool during the walk and are dropped (GC) as their rows are collected, so the resident leaf set
// stays bounded by the pool, not the table (spec/design/pager.md §4).
func (s *TableStore) IterInKeyOrder() ([]Row, error) {
	_, vals, err := s.rows.inorder(s.leafSrc())
	return vals, err
}

// NodeCount is the number of B-tree nodes (pages) in this store — the page_read count a full
// scan charges (spec/design/cost.md §3 "page_read"). 0 for an empty table.
func (s *TableStore) NodeCount() int { return s.rows.nodeCount() }

// RangeRows returns the rows whose primary key lies within the bound, in key order — a bounded
// B-tree scan that faults only the leaves the bound spans (spec/design/cost.md §3 "bounded scan").
func (s *TableStore) RangeRows(b keyBound) ([]Row, error) {
	_, vals, err := s.rows.rangeEntries(b, s.leafSrc())
	return vals, err
}

// RangeEntries returns the (key, row) pairs whose primary key lies within the bound, in key order
// (the mutation paths need the keys to Remove/Replace).
func (s *TableStore) RangeEntries(b keyBound) ([]Entry, error) {
	keys, vals, err := s.rows.rangeEntries(b, s.leafSrc())
	if err != nil {
		return nil, err
	}
	out := make([]Entry, len(keys))
	for i := range keys {
		out[i] = Entry{Key: keys[i], Row: vals[i]}
	}
	return out, nil
}

// OverlapNodeCount is the number of B-tree nodes a bounded scan over b visits — the page_read it
// charges (spec/design/cost.md §3). Equals NodeCount for the unbounded bound.
func (s *TableStore) OverlapNodeCount(b keyBound) int { return s.rows.overlapNodeCount(b) }

// ScanUnits is the up-front cost block a FULL scan of this store charges, as
// (page_read, value_decompress) units: every B-tree node plus — for the query's TOUCHED columns
// (mask, cost.md §3 "The touched set") — one page_read per overflow chain page and ceil(raw/C)
// value_decompress slabs per compressed stored value (spec/design/large-values.md §8/§12/§14).
// Equals (NodeCount, 0) when no touched record spills or compresses — and the row walk is
// skipped entirely when no touched column type can spill, so fixed-width tables and untouching
// queries pay nothing extra.
func (s *TableStore) ScanUnits(mask []bool) (pages, slabs int, err error) {
	pages = s.NodeCount()
	if anySpillableMasked(s.colTypes, mask) {
		entries, err := s.EntriesInKeyOrder()
		if err != nil {
			return 0, 0, err
		}
		for _, e := range entries {
			p, d := recordScanUnits(s.colTypes, e.Key, e.Row, s.cap, mask)
			pages += p
			slabs += d
		}
	}
	return pages, slabs, nil
}

// OverlapScanUnits is the up-front cost block a BOUNDED scan over b charges, as
// (page_read, value_decompress) units: the nodes the bound's key range intersects plus the chain
// pages and decompress slabs of the records the bound admits (cost.md §3;
// spec/design/large-values.md §8/§12/§13). An empty bound or a point-lookup miss admits no record
// and adds nothing beyond the path nodes.
func (s *TableStore) OverlapScanUnits(b keyBound, mask []bool) (pages, slabs int, err error) {
	pages = s.OverlapNodeCount(b)
	if anySpillableMasked(s.colTypes, mask) {
		entries, err := s.RangeEntries(b)
		if err != nil {
			return 0, 0, err
		}
		for _, e := range entries {
			p, d := recordScanUnits(s.colTypes, e.Key, e.Row, s.cap, mask)
			pages += p
			slabs += d
		}
	}
	return pages, slabs, nil
}

// RangeScanWithUnits is the fused single-descent bounded scan: the admitted (key, row) entries
// PLUS the (page_read, value_decompress) cost block the bound charges — exactly RangeEntries +
// OverlapScanUnits, computed in ONE B-tree traversal instead of three (the windowed walk visits
// precisely the nodes overlapNodeCount counts, and the per-admitted-record spill/compress units
// are computed inline from the entries it collects). Byte-identical cost and rows by construction.
func (s *TableStore) RangeScanWithUnits(b keyBound, mask []bool) ([]Entry, int, int, error) {
	keys, vals, pages, err := s.rows.rangeEntriesCounted(b, s.leafSrc())
	if err != nil {
		return nil, 0, 0, err
	}
	out := make([]Entry, len(keys))
	for i := range keys {
		out[i] = Entry{Key: keys[i], Row: vals[i]}
	}
	slabs := 0
	if anySpillableMasked(s.colTypes, mask) {
		for _, e := range out {
			p, d := recordScanUnits(s.colTypes, e.Key, e.Row, s.cap, mask)
			pages += p
			slabs += d
		}
	}
	return out, pages, slabs, nil
}

// ScanWithUnits is the fused single-descent full scan: every (key, row) entry PLUS the full-scan
// cost block — EntriesInKeyOrder + ScanUnits in one traversal (the unbounded bound visits every
// node, so the count equals NodeCount).
func (s *TableStore) ScanWithUnits(mask []bool) ([]Entry, int, int, error) {
	return s.RangeScanWithUnits(unboundedBound(), mask)
}

// GetWithUnits is the fused single-descent point lookup: the row at key (if any) PLUS the
// (page_read, value_decompress) block its point bound charges — the index fetch path's Get +
// OverlapScanUnits in one descent.
func (s *TableStore) GetWithUnits(key []byte, mask []bool) (Row, bool, int, int, error) {
	point := keyBound{lo: key, loInc: true, hi: key, hiInc: true}
	entries, pages, slabs, err := s.RangeScanWithUnits(point, mask)
	if err != nil {
		return nil, false, 0, 0, err
	}
	if len(entries) == 0 {
		return nil, false, pages, slabs, nil
	}
	return entries[0].Row, true, pages, slabs, nil
}

// WriteCompressUnits is the value_compress slabs storing this record costs — one ceil(raw/C)
// block per disposition-plan compression attempt (cost.md §3; large-values.md §13). Charged by
// the executor once per stored row version at the INSERT/UPDATE write site. Zero whenever the
// record fits inline-plain (no attempt runs), so existing costs do not move.
func (s *TableStore) WriteCompressUnits(key []byte, row Row) int {
	if !anySpillable(s.colTypes) {
		return 0
	}
	return recordCompressUnits(s.colTypes, key, row, s.cap)
}

// resolveColumns returns row with the unfetched values in the columns mask selects
// materialized through this store's pager (spec/design/large-values.md §14). The scan layer
// calls this per admitted row with the query's touched-set mask — the same static set the cost
// block charges (cost.md §3), so the physical chain reads / decompressions are exactly what the
// page_read/value_decompress units metered. When nothing needs resolution the row is returned
// as-is; otherwise a fresh copy is built — stored rows are shared with the tree and must never
// be mutated in place. Repeated scans therefore re-read (and are re-charged) consistently.
func (s *TableStore) resolveColumns(row Row, mask []bool) (Row, error) {
	needs := false
	for i, v := range row {
		if mask[i] && v.Kind == ValUnfetched {
			needs = true
			break
		}
	}
	if !needs {
		return row, nil
	}
	if s.paging == nil {
		panic("an unfetched value implies a paged store")
	}
	fetch := func(p uint32) ([]byte, error) { return s.paging.readBlock(p) }
	out := make(Row, len(row))
	copy(out, row)
	for i := range out {
		if mask[i] && out[i].Kind == ValUnfetched {
			v, err := resolveUnfetched(s.colTypes[i], out[i].Unf, fetch)
			if err != nil {
				return nil, err
			}
			out[i] = v
		}
	}
	return out, nil
}

// resolveAll materializes EVERY unfetched value in row (all columns). The mutation path uses
// this on a row it is about to re-store (UPDATE), so the stored row is fully resident and its
// weight/disposition re-plan exactly like an eager writer's (large-values.md §14).
func (s *TableStore) resolveAll(row Row) (Row, error) {
	mask := make([]bool, len(s.colTypes))
	for i := range mask {
		mask[i] = true
	}
	return s.resolveColumns(row, mask)
}

// ScanRange streams the rows whose primary key lies within the bound to visit, in key order, stopping
// (without faulting further leaves) the moment visit returns a false `continue` — the genuine LIMIT
// short-circuit (spec/design/cost.md §3 "LIMIT short-circuit").
func (s *TableStore) ScanRange(b keyBound, visit func(key []byte, row Row) (bool, error)) error {
	return s.rows.scanRange(b, s.leafSrc(), visit)
}

// ScanRangeRev is ScanRange in reverse: it yields the in-bound rows in DESCENDING key order — a
// DESC reverse scan (spec/design/cost.md §3), stopping the same way on a false `continue` so a
// reverse top-N short-circuits from the high end.
func (s *TableStore) ScanRangeRev(b keyBound, visit func(key []byte, row Row) (bool, error)) error {
	return s.rows.scanRangeRev(b, s.leafSrc(), visit)
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
func (s *TableStore) EntriesInKeyOrder() ([]Entry, error) {
	keys, vals, err := s.rows.inorder(s.leafSrc())
	if err != nil {
		return nil, err
	}
	out := make([]Entry, len(keys))
	for i := range keys {
		out[i] = Entry{Key: keys[i], Row: vals[i]}
	}
	return out, nil
}

// treeRoot is the root B-tree node of this store, for the page-backed serializer
// (spec/fileformat/format.md). nil for an empty table.
func (s *TableStore) treeRoot() *pnode { return s.rows.rootNode() }

// setTree installs a loaded B-tree as this store's contents (format.go LoadDatabase).
func (s *TableStore) setTree(root *pnode, length int) { s.rows = fromLoaded(root, length) }

// Len returns the row count.
func (s *TableStore) Len() int { return s.rows.Len() }

// storedBytes is the total on-disk record bytes this store holds — the deterministic,
// cross-core-identical footprint measure the temp-table budget sums (spec/design/temp-tables.md §7).
func (s *TableStore) storedBytes() uint64 { return s.rows.residentRecordBytes() }
