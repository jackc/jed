package jed

import (
	"bytes"
	"fmt"
	"math"
	"path/filepath"
	"reflect"
	"slices"
	"sort"
	"strconv"
	"strings"
	"sync/atomic"
)

// exprEqual reports whether two parsed expression trees are STRUCTURALLY equal (spec/design/grammar.md
// §10) — the Go equivalent of the Rust core's derived PartialEq on Expr. Used by the SELECT DISTINCT
// ORDER BY restriction to decide whether an expression sort key matches a select-list expression. The
// AST carries no source positions, so textually-identical fragments (`a + b` here and there) compare
// equal; following the node pointers is exactly the recursive tree comparison.
func exprEqual(a, b exprNode) bool { return reflect.DeepEqual(a, b) }

// Statement executor (CLAUDE.md §10).
//
// SCAFFOLD (step-5 Phase A): dispatches a parsed statement; each arm is filled in
// feature-by-feature (Phases B–E).

// outcomeKind distinguishes a bare statement result from a query result set.
type outcomeKind int

const (
	// outcomeStatement is a statement producing no result set (CREATE, INSERT).
	outcomeStatement outcomeKind = iota
	// outcomeQuery is a query result set.
	outcomeQuery
)

// outcome is the result of executing one statement. Cost is the deterministic execution
// cost accrued while running it (CLAUDE.md §13) — a DML statement accrues its scan +
// filter cost even though it returns no rows.
type outcome struct {
	Kind outcomeKind
	// ColumnNames are the output column names of a query result (nil for a non-query
	// statement); the column count is len(ColumnNames) (spec/design/grammar.md §8).
	ColumnNames []string
	// ColumnTypes is the canonical name of each output column's resolved type (parallel to
	// ColumnNames; nil for a non-query statement) — i16/i32/i64/text/boolean/decimal/…,
	// or "unknown" for an untyped NULL column. It is the resolved SCALAR type — for decimal the
	// unconstrained "decimal", not the numeric(p,s) typmod (spec/design/conformance.md §7).
	ColumnTypes []string
	Rows        [][]Value
	Cost        int64
	// RowsAffected is how many rows a DML statement (INSERT/UPDATE/DELETE without
	// RETURNING) touched — PostgreSQL's command-tag count (spec/design/api.md §4).
	// HasRowsAffected distinguishes a DML statement that matched nothing (0, true)
	// from DDL and transaction control, which have no row count (0, false).
	RowsAffected    int64
	HasRowsAffected bool
}

// DefaultPageSize is the default serialization page size (8 KiB — spec/design/storage.md §3),
// used for a fresh in-memory or newly-created database when no explicit size is given.
const DefaultPageSize uint32 = 8192

// DefaultMaxSQLLength is the default per-handle input-SQL byte limit (1 MiB — CLAUDE.md §13;
// spec/design/api.md §8, cost.md §7). The §13 input-size gate's default ceiling: generous for
// hand-written / ORM SQL, yet bounds the parse tree to a few MB so unbounded untrusted input
// cannot exhaust memory. A caller raises it (trusted bulk loads) or sets 0 for unlimited via
// SetMaxSQLLength. Identical across cores (§8).
const DefaultMaxSQLLength = 1 << 20

// DefaultTempBuffers is the default per-session storage budget for SESSION-LOCAL temporary tables, in
// BYTES (spec/design/temp-tables.md §7). Temp tables RETAIN bytes across statements, which neither the
// per-statement cost ceiling (maxCost) nor the cumulative budget (lifetimeMaxCost) bounds, so
// tempBuffers is the §13 gate that does: the instant a session's resident temp storage (byte-identical
// on-disk record bytes) would exceed it, the write aborts 54P03. 0 ⇒ unlimited (a trusted handle).
// Identical across cores (§8); the abort point is part of the cross-core contract.
const defaultTempBuffers = 32 << 20

// maxCompositeDepth is the maximum composite-type nesting depth (CLAUDE.md §13; spec/design/cost.md
// §7b). A composite type's depth is the length of its deepest chain of nested composites, counting
// itself: a row of scalars is depth 1, `CREATE TYPE b AS (x a)` is `1 + depth(a)`, and an array
// field counts as its element (array levels are not composite levels — CompositeRefOf looks through
// one array level the same way). A CREATE TYPE whose result would exceed this is rejected 54001, and
// a loaded catalog that exceeds it is treated as corrupt XX001 — bounding the native recursion of
// every derived walk (value codec, comparator, record_out/record_in, ResolveColType) at the two
// producers (DDL + load) so all downstream walks are transitively stack-safe. A fixed, cross-core
// constant like maxExprDepth (§8). The chain is built across many cheap statements, so neither the
// per-statement input-size cap nor the parser nesting counter sees it (cost.md §7).
const maxCompositeDepth = 32

// Snapshot is an immutable committed (or in-progress working) database state — the catalog + each
// table's store + the commit counter (spec/design/transactions.md §2). The committed state is one
// of these; a write transaction builds a new one from it (the persistent stores clone O(1) —
// pmap.go / §3). A reader holds a *Snapshot and is thereby stable for its life: a later commit
// produces a new Snapshot and never mutates this one. (P5.3a is single-handle; sharing a *Snapshot
// across goroutines is P5.3b.)
type snapshot struct {
	// txid is the snapshot's version — the commit counter (transactions.md §8; the watermark unit).
	txid uint64
	// catGen is the catalog generation — a monotonic counter bumped by every schema mutation
	// (CREATE/DROP/ALTER of a table/type/index), carried forward across clone(). Unlike txid it does
	// NOT move on data writes and is defined for in-memory databases too, so a prepared statement's
	// plan cache keys its committed-plan validity on it: a cached plan is reusable iff the read
	// snapshot's catGen still equals the plan's (spec/design/api.md §2.4). NOT bumped by sequence
	// nextval (a data write on the nextval path), only by sequence DDL — a SELECT plan binds no
	// sequence.
	catGen uint64
	tables map[string]*catTable
	// types holds user-defined composite (row) types, keyed by lowercased name
	// (spec/design/composite.md). A database-level object set, separate from tables; serialized
	// into the catalog's composite-type entries (spec/fileformat/format.md). Sorted by key when
	// serialized so map-iteration order never leaks (CLAUDE.md §8).
	types  map[string]*compositeType
	stores map[string]*tableStore
	// indexStores holds each secondary index's B-tree (spec/design/indexes.md §3): a
	// TableStore with ZERO value columns (entry keys only — the on-disk empty-payload
	// record), keyed by the lowercased index name (index names live in the relation
	// namespace, globally unique). Which table owns an index is recorded in that table's
	// Indexes list.
	indexStores map[string]*tableStore
	// sequences holds sequences, keyed by lowercased name (spec/design/sequences.md). A
	// database-level object set separate from tables/types; serialized into the catalog's
	// sequence entries (spec/fileformat/format.md, entry_kind = 2). The mutable counter
	// (LastValue/IsCalled) lives here, so nextval advances the working snapshot and rolls back
	// with it (sequences.md §5).
	sequences map[string]*sequenceDef
	// collations caches collations RESOLVED from the file's reference entries on open, keyed by their
	// exact (CASE-SENSITIVE) name — collation names are quoted identifiers ("en-US",
	// spec/design/collation.md §1). C is never stored (table-free, built in). Under the reference-only
	// model (§4.2) the file holds only a metadata entry per collation the schema references; the table
	// comes from the binary's vendored set (entry_kind = 3, format_version 18 —
	// spec/fileformat/format.md).
	collations map[string]*Collation
	// defaultCollation is the per-database default collation name, or "" for C (collation.md §1/§5).
	// An un-annotated text column inherits this at CREATE TABLE. Persisted as the is_default flag bit
	// on that collation's entry_kind = 3 reference entry, restored on load.
	defaultCollation string
	// gistTrees holds each GiST index's resident R-tree (spec/design/gist.md §4.1), keyed by the
	// lowercased index name. The leaf-key store (indexStores) stays the maintained source of truth;
	// this tree is the acceleration structure the planner descends. Rebuilt CANONICALLY
	// (buildGistFromLeafKeys — content-deterministic, gist.md §3) at every mutating statement and on
	// load, so a committed snapshot always carries a fresh, cross-core-identical tree a SELECT can
	// descend. Never mutated in place (replaced wholesale on rebuild), so clone shallow-copies it.
	gistTrees map[string]*gistTree
	// storePaging is this snapshot's domain paging context — the pager a store created IN-SESSION
	// (putTableResolved / putIndexStore / putIndex) binds at creation, so it joins the post-commit
	// residency flip (demoteCleanLeaves) instead of staying a fully-resident decoded tree forever.
	// Every domain sets it: the main file/in-memory snapshot binds the storage identity's paging at
	// load/create (format.go / file.go), a session-local temp snapshot its per-domain MemoryBlockStore
	// pager (spec/design/temp-tables.md §6), an attachment its own storage's pager. nil only on a bare
	// scratch engine that never persists. Stores loaded FROM a file attach the same pager individually
	// at load; binding at creation is what covers the stores load never sees. Carried through clone()
	// so a tx's working snapshot creates stores against the same domain page space. NEVER serialized.
	storePaging *sharedPaging
}

// newSnapshot builds an empty snapshot.
func newSnapshot() *snapshot {
	return &snapshot{
		tables:      make(map[string]*catTable),
		types:       make(map[string]*compositeType),
		stores:      make(map[string]*tableStore),
		indexStores: make(map[string]*tableStore),
		sequences:   make(map[string]*sequenceDef),
		collations:  make(map[string]*Collation),
		gistTrees:   make(map[string]*gistTree),
	}
}

// clone returns an independent copy: the catalog map is shallow (Table structs are never mutated
// in place — only added/removed) and each store is an O(1) persistent-map clone (pmap.go).
func (s *snapshot) clone() *snapshot {
	tables := make(map[string]*catTable, len(s.tables))
	for k, v := range s.tables {
		tables[k] = v
	}
	// Composite types, like Table, are never mutated in place — only added/removed — so the map
	// copy is shallow (spec/design/composite.md §3).
	types := make(map[string]*compositeType, len(s.types))
	for k, v := range s.types {
		types[k] = v
	}
	stores := make(map[string]*tableStore, len(s.stores))
	for k, v := range s.stores {
		stores[k] = v.clone()
	}
	indexStores := make(map[string]*tableStore, len(s.indexStores))
	for k, v := range s.indexStores {
		indexStores[k] = v.clone()
	}
	// Sequences, like Table/CompositeType, are never mutated in place — only added/removed/replaced
	// (nextval inserts a fresh struct) — so the map copy is shallow (spec/design/sequences.md §2).
	sequences := make(map[string]*sequenceDef, len(s.sequences))
	for k, v := range s.sequences {
		sequences[k] = v
	}
	// Collations, like Table, are never mutated in place — only added — so the map copy is shallow
	// (spec/design/collation.md §4).
	collations := make(map[string]*Collation, len(s.collations))
	for k, v := range s.collations {
		collations[k] = v
	}
	// GiST trees are never mutated in place — only replaced wholesale on rebuild — so the map copy is
	// shallow (spec/design/gist.md §4.1).
	gistTrees := make(map[string]*gistTree, len(s.gistTrees))
	for k, v := range s.gistTrees {
		gistTrees[k] = v
	}
	return &snapshot{txid: s.txid, catGen: s.catGen, tables: tables, types: types, stores: stores, indexStores: indexStores, sequences: sequences, collations: collations, defaultCollation: s.defaultCollation, gistTrees: gistTrees, storePaging: s.storePaging}
}

// demoteCleanLeaves demotes every store's clean, persisted resident leaves to OnDisk references —
// the post-commit residency flip over the whole snapshot (bplus-reshape.md B4), run after a
// successful persist so the published committed tree is the skeletal `interiors + OnDisk leaves`
// shape on every host. Table stores and btree/GIN index stores flip; a GiST leaf-key store's nodes
// are never persisted (its on-disk form is the R-tree), so it no-ops naturally. Map iteration
// order cannot leak: each store's flip is independent and order-insensitive (CLAUDE.md §8).
func (s *snapshot) demoteCleanLeaves() {
	for _, store := range s.stores {
		store.demoteCleanLeaves()
	}
	for _, store := range s.indexStores {
		store.demoteCleanLeaves()
	}
}

// resolveCollation resolves a collation name for USE — query resolution and key encoding
// (spec/design/collation.md §2/§9). The collations the database has resolved (a cache populated on
// open from the file's reference entries, carrying their version pin) first, then the engine-global
// LOADED set (db.LoadUnicodeData, §4). nil ⇒ neither has it (the resolver raises 42704). C is handled
// by the caller (built-in). This is the reference-only read path: a collation is never baked into the
// file — the file references it by name and the table comes from a loaded bundle.
func (s *snapshot) resolveCollation(name string) *Collation {
	if c := s.collations[name]; c != nil {
		return c
	}
	return LoadedCollation(name)
}

// collationSkew is the slice-2d version-skew verdict for a referenced collation (collation.md §12):
// (fileUnicode, fileCldr, loadedUnicode, loadedCldr, true) when this database's keys were built under
// a different (unicode, cldr) than the loaded bundle provides — the object using it is read-only
// (XX002 on write) — else skewed=false (Full: same version, or this collation has no catalog-local
// file pin so it is freshly the loaded version, an in-memory-only database). A pure comparison of the
// file pin already in the catalog (§5) vs the engine-global loaded set; the Snapshot wiring of
// collation.VersionSkew.
func (s *snapshot) collationSkew(name string) (fileU, fileC, loadedU, loadedC string, skewed bool) {
	cat := s.collations[name]
	if cat == nil {
		return "", "", "", "", false
	}
	lu, lc, sk := versionSkew(name, cat.UnicodeVersion, cat.CldrVersion)
	if !sk {
		return "", "", "", "", false
	}
	return cat.UnicodeVersion, cat.CldrVersion, lu, lc, true
}

// referencedCollations returns the collations the database SCHEMA references — every column's frozen
// collation plus the per-database default — resolved (catalog-local set, then the binary's vendored
// set) and sorted by exact name. Under the reference-only model (spec/design/collation.md §2/§5)
// these, not an imported set, are what earn a metadata entry on disk: a collation is recorded because
// the schema uses it, regardless of whether it was ever passed to a (now-removed) import call. C
// columns (empty Collation) reference nothing. A referenced name this build does not vendor is a bug
// surfaced here (the precursor to the slice-2d open-time verdict).
func (s *snapshot) referencedCollations() ([]*Collation, error) {
	names := map[string]struct{}{}
	for _, t := range s.tables {
		for _, col := range t.Columns {
			if col.Collation != "" {
				names[col.Collation] = struct{}{}
			}
		}
	}
	if s.defaultCollation != "" {
		names[s.defaultCollation] = struct{}{}
	}
	sorted := make([]string, 0, len(names))
	for n := range names {
		sorted = append(sorted, n)
	}
	sort.Strings(sorted)
	out := make([]*Collation, len(sorted))
	for i, name := range sorted {
		c := s.resolveCollation(name)
		if c == nil {
			return nil, newError(UndefinedObject,
				fmt.Sprintf("collation %q referenced by the schema is not provided by a loaded bundle", name))
		}
		out[i] = c
	}
	return out, nil
}

// upgradeCollations is the REINDEX / COLLATION UPGRADE migration (spec/design/collation.md §12):
// rebuild every collated key stored under a version-SKEWED collation against the LOADED table and
// advance that collation's pin to the loaded version — clearing the skew so the affected tables are
// read-write again and their collated indexes regain pushdown (a Full index, encoding.md §2.12).
// Returns the number of collations re-pinned (0 ⇒ nothing skewed, a no-op).
//
// Whole-database, per-collation pin: the pin is ONE entry per collation NAME (§5), so a collation's
// pin may advance only once every key under it is rebuilt — else a not-yet-rebuilt table would
// falsely read as Full (corruption). The caller swaps the result in atomically. resolveCollation
// already yields the loaded table data (the file entry carries the file pin but loaded
// singles/contractions), so re-encoding produces loaded-version sort keys; the re-pin realigns the label.
func (s *snapshot) upgradeCollations(pageSize uint32) (int, error) {
	refs, err := s.referencedCollations()
	if err != nil {
		return 0, err
	}
	skewed := map[string]bool{}
	for _, c := range refs {
		if _, _, _, _, sk := s.collationSkew(c.Name); sk {
			skewed[c.Name] = true
		}
	}
	if len(skewed) == 0 {
		return 0, nil
	}
	isSkewed := func(coll string) bool { return coll != "" && skewed[coll] }

	// Sorted table order (no map-iteration leak, CLAUDE.md §8; the per-table rebuilds are independent
	// and the re-pin is order-free, so the result is order-invariant regardless).
	tableKeys := make([]string, 0, len(s.tables))
	for k := range s.tables {
		tableKeys = append(tableKeys, k)
	}
	sort.Strings(tableKeys)
	for _, key := range tableKeys {
		table := s.tables[key]
		// A collated PK re-encode moves every storage key ⇒ a full table rewrite, and an index entry
		// carries the storage key as its suffix (indexes.md §3) ⇒ every index of the table is rebuilt.
		// Else only the indexes whose own key columns use a skewed collation are rebuilt.
		pkSkewed := false
		for _, i := range table.PK {
			if isSkewed(table.Columns[i].Collation) {
				pkSkewed = true
				break
			}
		}
		var indexes []indexDef
		for _, idx := range table.Indexes {
			affected := pkSkewed
			for _, c := range idx.Columns {
				if isSkewed(table.Columns[c].Collation) {
					affected = true
				}
			}
			if affected {
				indexes = append(indexes, idx)
			}
		}
		if !pkSkewed && len(indexes) == 0 {
			continue
		}
		colls := make([]*Collation, len(table.Columns))
		for i, c := range table.Columns {
			if c.Collation != "" {
				colls[i] = s.resolveCollation(c.Collation)
			}
		}
		// Read every (storage key, row) pair, fully materialized (a spilled non-key value must
		// survive a rewrite; a collated key column never spills — §2.12 narrowing b).
		entries, err := s.store(table.Name).EntriesInKeyOrder()
		if err != nil {
			return 0, err
		}
		for i := range entries {
			r, err := s.store(table.Name).resolveAll(entries[i].Row)
			if err != nil {
				return 0, err
			}
			entries[i].Row = r
		}
		// The NEW storage key per row: re-encoded under the loaded collation if the PK moved, else
		// the existing key (unchanged — includes a synthetic-rowid table, which has no PK).
		if pkSkewed {
			for i := range entries {
				k, err := encodePkKey(table, table.PK, colls, entries[i].Row)
				if err != nil {
					return 0, err
				}
				entries[i].Key = k
			}
			s.putTable(table, pageSize) // fresh empty store (+ re-register the same table)
			for _, e := range entries {
				if _, err := s.store(table.Name).Insert(e.Key, e.Row); err != nil {
					return 0, err
				}
			}
		}
		// Rebuild each affected index store from the (re-keyed) rows.
		c := pagePayload(pageSize)
		for _, def := range indexes {
			var ekeys [][]byte
			for _, e := range entries {
				eks, err := indexEntryKeys(table.Columns, colls, def, e.Key, e.Row)
				if err != nil {
					return 0, err
				}
				ekeys = append(ekeys, eks...)
			}
			sort.Slice(ekeys, func(a, b int) bool { return bytes.Compare(ekeys[a], ekeys[b]) < 0 })
			fresh := newTableStore(c, nil)
			for _, ek := range ekeys {
				if _, err := fresh.Insert(ek, storedRow{}); err != nil {
					return 0, err
				}
			}
			s.putIndexStore(strings.ToLower(def.Name), fresh)
		}
	}
	// Advance each skewed collation's pin to the loaded version.
	for name := range skewed {
		if loaded := LoadedCollation(name); loaded != nil {
			s.collations[name] = loaded
		}
	}
	return len(skewed), nil
}

// table looks up a table definition by name (case-insensitive).
func (s *snapshot) table(name string) (*catTable, bool) {
	t, ok := s.tables[strings.ToLower(name)]
	return t, ok
}

// store returns a table's store (the table is known to exist).
func (s *snapshot) store(name string) *tableStore { return s.stores[strings.ToLower(name)] }

// compositeType looks up a composite type definition by name (case-insensitive); nil if absent.
func (s *snapshot) compositeType(name string) *compositeType {
	return s.types[strings.ToLower(name)]
}

// bumpCatGen advances the catalog generation — called by every schema mutator (see catGen). A
// SELECT plan cached against a prior generation is thereby invalidated on the next execute.
func (s *snapshot) bumpCatGen() { s.catGen++ }

// putType registers a composite type (CREATE TYPE). The lower-cased name is the key. The caller
// has already resolved field types and checked for a duplicate.
func (s *snapshot) putType(ct *compositeType) {
	s.bumpCatGen()
	s.types[strings.ToLower(ct.Name)] = ct
}

// removeType removes a composite type (DROP TYPE). The caller has checked there are no dependents.
func (s *snapshot) removeType(key string) {
	s.bumpCatGen()
	delete(s.types, key)
}

// compositeTypesSorted returns all composite types in ascending lowercased-name order — the
// on-disk emission order (spec/fileformat/format.md) and a deterministic order with no
// map-iteration leak (CLAUDE.md §8).
func (s *snapshot) compositeTypesSorted() []*compositeType {
	keys := make([]string, 0, len(s.types))
	for k := range s.types {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	out := make([]*compositeType, len(keys))
	for i, k := range keys {
		out[i] = s.types[k]
	}
	return out
}

// tablesSorted returns all tables in ascending lowercased-name order — a deterministic order
// with no map-iteration leak (CLAUDE.md §8); the jed_tables / jed_columns generation order
// (spec/design/introspection.md §5).
func (s *snapshot) tablesSorted() []*catTable {
	keys := make([]string, 0, len(s.tables))
	for k := range s.tables {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	out := make([]*catTable, len(keys))
	for i, k := range keys {
		out[i] = s.tables[k]
	}
	return out
}

// sequence looks up a sequence definition by name (case-insensitive); nil if absent.
func (s *snapshot) sequence(name string) *sequenceDef {
	return s.sequences[strings.ToLower(name)]
}

// putSequence registers a sequence (CREATE SEQUENCE). The lower-cased name is the key. The caller
// has already validated the option set and checked the relation namespace for a collision.
func (s *snapshot) putSequence(seq *sequenceDef) {
	s.sequences[strings.ToLower(seq.Name)] = seq
}

// removeSequence removes a sequence (DROP SEQUENCE). The caller has checked it exists.
func (s *snapshot) removeSequence(key string) {
	delete(s.sequences, key)
}

// sequencesSorted returns all sequences in ascending lowercased-name order — the on-disk emission
// order (spec/fileformat/format.md) and a deterministic order with no map-iteration leak
// (CLAUDE.md §8).
func (s *snapshot) sequencesSorted() []*sequenceDef {
	keys := make([]string, 0, len(s.sequences))
	for k := range s.sequences {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	out := make([]*sequenceDef, len(keys))
	for i, k := range keys {
		out[i] = s.sequences[k]
	}
	return out
}

// sequencesOwnedBy returns the lowercased keys of every sequence OWNED BY the table name
// (case-insensitive) — the serial-created sequences DROP TABLE must auto-drop
// (spec/design/sequences.md §12). Sorted so the auto-drop is deterministic (no map-iteration
// leak, CLAUDE.md §8).
func (s *snapshot) sequencesOwnedBy(name string) []string {
	var keys []string
	for k, seq := range s.sequences {
		if seq.OwnedBy != nil && strings.EqualFold(seq.OwnedBy.Table, name) {
			keys = append(keys, k)
		}
	}
	sort.Strings(keys)
	return keys
}

// compositeDependent reports whether any table column or composite-type field still references the
// composite type name (case-insensitive) — the DROP TYPE ... RESTRICT dependency check (2BP01). It
// returns the first dependent's description for the error detail, or ("", false) if there are no
// dependents. Tables and types are scanned in lowercased-name order so the chosen dependent is
// deterministic (CLAUDE.md §8).
func (s *snapshot) compositeDependent(name string) (string, bool) {
	key := strings.ToLower(name)
	tableKeys := make([]string, 0, len(s.tables))
	for k := range s.tables {
		tableKeys = append(tableKeys, k)
	}
	sort.Strings(tableKeys)
	// CompositeRefOf looks through one array level, so an addr[] column / field counts as a
	// dependent of addr exactly as a bare addr one does (spec/design/array.md §12).
	for _, tk := range tableKeys {
		t := s.tables[tk]
		for _, c := range t.Columns {
			if r := c.Type.CompositeRefOf(); r != nil && strings.EqualFold(r.Name, key) {
				return "column " + c.Name + " of table " + t.Name, true
			}
		}
	}
	typeKeys := make([]string, 0, len(s.types))
	for k := range s.types {
		typeKeys = append(typeKeys, k)
	}
	sort.Strings(typeKeys)
	for _, ck := range typeKeys {
		ct := s.types[ck]
		for _, f := range ct.Fields {
			if r := f.Type.CompositeRefOf(); r != nil && strings.EqualFold(r.Name, key) {
				return "field " + f.Name + " of type " + ct.Name, true
			}
		}
	}
	return "", false
}

// fkDependent is one FOREIGN KEY dependent surfaced by a multi-table DROP TABLE's dependency scan
// (spec/design/grammar.md §13): an FK on a table that survives the drop, referencing a table being
// dropped. RESTRICT formats refTableName/fkName/droppedName into its 2BP01 detail; CASCADE uses
// refTableKey/fkName to remove the now-dangling constraint.
type fkDependent struct {
	refTableKey  string // lowercased key of the (surviving) referencing table — for the CASCADE removal
	fkName       string // the FK constraint's name
	refTableName string // canonical referencing-table name — for the RESTRICT detail
	droppedName  string // canonical name of the dropped table the FK references — for the RESTRICT detail
}

// foreignKeyDependentsExcluding returns every FK on a table NOT in dropping (a set of lowercased
// table keys) that references a table that IS in dropping — the dependency scan for a multi-table
// DROP TABLE (spec/design/grammar.md §13, constraints.md §6.10). A dependent whose referencing
// table is itself being dropped does not count (the drop-set exclusion), so a FK between two tables
// both named in the same statement never blocks. Referencing tables are scanned in ascending
// lowercased key order (each table's ForeignKeys is already name-ordered) for determinism (§8).
// RESTRICT raises 2BP01 on the first entry; CASCADE removes every entry's FK.
func (s *snapshot) foreignKeyDependentsExcluding(dropping map[string]bool) []fkDependent {
	tableKeys := make([]string, 0, len(s.tables))
	for k := range s.tables {
		tableKeys = append(tableKeys, k)
	}
	sort.Strings(tableKeys)
	var out []fkDependent
	for _, tk := range tableKeys {
		if dropping[tk] {
			continue // the referencing table is itself being dropped — no dependency
		}
		t := s.tables[tk]
		for _, fk := range t.ForeignKeys {
			refKey := strings.ToLower(fk.RefTable)
			if dropping[refKey] {
				droppedName := fk.RefTable
				if d, ok := s.tables[refKey]; ok {
					droppedName = d.Name
				}
				out = append(out, fkDependent{
					refTableKey:  tk,
					fkName:       fk.Name,
					refTableName: t.Name,
					droppedName:  droppedName,
				})
			}
		}
	}
	return out
}

// removeForeignKey removes the named FK constraint from tableKey in place — a copy-on-write of the
// table + its ForeignKeys slice so the committed snapshot is untouched — preserving the table's
// store and rows. DROP TABLE … CASCADE's removal of a dependent FK on a table that survives the
// drop (spec/design/grammar.md §13). An FK owns no B-tree (constraints.md §6), so only the catalog
// list changes.
func (s *snapshot) removeForeignKey(tableKey, fkName string) {
	old, ok := s.tables[tableKey]
	if !ok {
		return
	}
	s.bumpCatGen()
	kept := make([]foreignKey, 0, len(old.ForeignKeys))
	for _, fk := range old.ForeignKeys {
		if !strings.EqualFold(fk.Name, fkName) {
			kept = append(kept, fk)
		}
	}
	t := *old
	t.ForeignKeys = kept
	s.tables[tableKey] = &t
}

// validateCompositeTypes validates the loaded composite-type catalog (the on-disk two-pass load —
// spec/design/composite.md §3): every composite a field references must exist, the reference graph
// must be acyclic, and no type may nest deeper than maxCompositeDepth. A dangling, cyclic, or
// over-deep reference is a malformed file (XX001). Called once after the whole catalog is read, and
// BEFORE any store is built — so the subsequent ResolveColType walks (and every later
// value-codec/comparator walk) recurse over a depth-bounded catalog and stay stack-safe (CLAUDE.md
// §13; cost.md §7b).
func (s *snapshot) validateCompositeTypes() error {
	// Existence: every nested-composite field names a registered type. Visit in name order so the
	// first reported dangling reference is deterministic.
	keys := make([]string, 0, len(s.types))
	for k := range s.types {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	for _, k := range keys {
		ct := s.types[k]
		for _, f := range ct.Fields {
			// CompositeRefOf looks through one array level, so an array-of-composite field
			// (`addr[]`) is validated like a bare `addr` one (spec/design/array.md §12).
			if r := f.Type.CompositeRefOf(); r != nil && s.compositeType(r.Name) == nil {
				return newError(DataCorrupted,
					"composite type "+ct.Name+" references unknown type "+r.Name)
			}
		}
	}
	// One DFS over the type → referenced-types graph that enforces BOTH acyclicity and the
	// nesting-depth bound (color: 0 unvisited, 1 on-stack, 2 done; cache memoizes each done type's
	// absolute nesting depth). Two guards make it stack-safe AND sound regardless of visitation
	// order: levelsAbove >= maxCompositeDepth bounds the native recursion on a fresh descent, and the
	// post-compute depth > maxCompositeDepth check catches an over-deep type reached via a memoized
	// (color-2) shortcut — which the descent guard alone would miss when the catalog is colored
	// bottom-up. Existence ran first, so every referenced type is present.
	color := make(map[string]uint8)
	cache := make(map[string]int)
	var visit func(key string, levelsAbove int) (int, error)
	visit = func(key string, levelsAbove int) (int, error) {
		if levelsAbove >= maxCompositeDepth {
			return 0, newError(DataCorrupted,
				fmt.Sprintf("composite type nesting exceeds the maximum depth of %d", maxCompositeDepth))
		}
		switch color[key] {
		case 1:
			return 0, newError(DataCorrupted, "composite type definition cycle through "+key)
		case 2:
			return cache[key], nil
		}
		color[key] = 1
		child := 0
		if ct, ok := s.types[key]; ok {
			for _, f := range ct.Fields {
				r := f.Type.CompositeRefOf()
				if r == nil {
					continue
				}
				d, err := visit(strings.ToLower(r.Name), levelsAbove+1)
				if err != nil {
					return 0, err
				}
				if d > child {
					child = d
				}
			}
		}
		depth := 1 + child
		if depth > maxCompositeDepth {
			return 0, newError(DataCorrupted,
				fmt.Sprintf("composite type nesting exceeds the maximum depth of %d", maxCompositeDepth))
		}
		color[key] = 2
		cache[key] = depth
		return depth, nil
	}
	for _, k := range keys {
		if color[k] == 0 {
			if _, err := visit(k, 0); err != nil {
				return err
			}
		}
	}
	return nil
}

// compositeTypeDepth returns the composite-type nesting depth of ty against this snapshot's type
// catalog, memoized in cache (lowercased name → depth): a scalar is 0, T[] is depth(T) (array levels
// are not composite levels — CompositeRefOf looks through one array level the same way), and a
// composite is 1 + max(field depths) (an empty composite is 1). The CREATE TYPE gate uses this
// against the *existing* catalog, every type of which already satisfies depth ≤ maxCompositeDepth
// (the load + create invariant), so the recursion is bounded by the limit; memoization keeps a
// diamond-shaped reference graph linear (spec/design/cost.md §7b).
func (s *snapshot) compositeTypeDepth(ty dataType, cache map[string]int) int {
	r := ty.CompositeRefOf()
	if r == nil {
		return 0 // a scalar (or a scalar array) adds no composite level
	}
	key := strings.ToLower(r.Name)
	if d, ok := cache[key]; ok {
		return d
	}
	depth := 1
	if def, ok := s.types[key]; ok {
		child := 0
		for _, f := range def.Fields {
			if d := s.compositeTypeDepth(f.Type, cache); d > child {
				child = d
			}
		}
		depth = 1 + child
	}
	cache[key] = depth
	return depth
}

// putTable registers a new table and its empty store. The store carries the page payload cap (=
// page_size − 16) and the column types so the page-backed B-tree can weigh records for its
// size-driven split (spec/fileformat/format.md).
func (s *snapshot) putTable(t *catTable, pageSize uint32) {
	// Resolve each column's ColType against the (already-registered) composite-type catalog — the
	// codec/coercion tree the store keeps so neither re-walks the type catalog per row
	// (spec/design/composite.md §4). Composite types are registered before any table (the types-first
	// catalog order / CREATE TYPE-before-CREATE TABLE rule), so the lookup inside ResolveColType
	// always resolves.
	colTypes := make([]colType, len(t.Columns))
	for i, c := range t.Columns {
		colTypes[i] = resolveColType(c.Type, s.types)
	}
	s.putTableResolved(t, colTypes, pageSize)
}

// putTableResolved registers a table whose column ColTypes are ALREADY resolved — used when staging a
// TEMP table (spec/design/temp-tables.md §8): a temp table's composite columns must resolve against
// the MAIN snapshot's type catalog (composites are never temp — CREATE TYPE is persistent), not this
// (temp) snapshot's empty types map. The resolved ColType tree is fully self-contained
// (spec/design/composite.md §4), so the store needs nothing from the catalog thereafter. The plain
// putTable resolves against s.types and delegates here.
func (s *snapshot) putTableResolved(t *catTable, colTypes []colType, pageSize uint32) {
	s.bumpCatGen()
	key := strings.ToLower(t.Name)
	st := newTableStore(pagePayload(pageSize), colTypes)
	// Bind the domain's pager (snapshot.storePaging) so the new store demand-pages like a loaded one:
	// its committed leaves demote at each commit (demoteCleanLeaves) and fault back through the pool,
	// instead of staying fully-resident decoded for the handle's lifetime. nil only on a bare scratch
	// engine that never persists.
	if s.storePaging != nil {
		st.attachPaging(s.storePaging)
	}
	s.stores[key] = st
	s.tables[key] = t
}

// removeTable removes a table's definition, its store, and its indexes' stores (DROP
// TABLE — the indexes have no independent life, spec/design/indexes.md §2).
func (s *snapshot) removeTable(key string) {
	s.bumpCatGen()
	if t, ok := s.tables[key]; ok {
		for _, idx := range t.Indexes {
			delete(s.indexStores, strings.ToLower(idx.Name))
		}
	}
	delete(s.tables, key)
	delete(s.stores, key)
}

// indexStore returns a secondary index's store (the index is known to exist). nameKey is
// the lowercased index name.
func (s *snapshot) indexStore(nameKey string) *tableStore { return s.indexStores[nameKey] }

// hasIndexStore reports whether this snapshot holds a store for the named index (lowercased key).
// Used to route index access to the session temp snapshot vs the main snapshot (temp-tables.md §2).
func (s *snapshot) hasIndexStore(nameKey string) bool {
	_, ok := s.indexStores[nameKey]
	return ok
}

// storageBytes is the total on-disk record bytes of every table store + index store in this snapshot
// — the temp budget's deterministic footprint measure (spec/design/temp-tables.md §7), summed over
// the session temp snapshot. Iteration order does not matter (it is a sum).
func (s *snapshot) storageBytes() uint64 {
	var total uint64
	for _, st := range s.stores {
		total += st.storedBytes()
	}
	for _, st := range s.indexStores {
		total += st.storedBytes()
	}
	return total
}

// putIndex registers a new (empty) secondary index on tableKey: insert its definition
// into the table's Indexes in ascending lowercased-name order (the catalog/planner order —
// spec/design/indexes.md §6) and create its zero-column store. The Table struct is
// re-allocated (catalog Tables are never mutated in place — snapshots share them).
func (s *snapshot) putIndex(tableKey string, def indexDef, pageSize uint32) {
	s.bumpCatGen()
	nameKey := strings.ToLower(def.Name)
	fresh := newTableStore(pagePayload(pageSize), nil)
	if s.storePaging != nil {
		fresh.attachPaging(s.storePaging) // bind the domain pager, like putTableResolved/putIndexStore
	}
	s.indexStores[nameKey] = fresh
	old := s.tables[tableKey]
	t := *old
	pos := len(old.Indexes)
	for i, ix := range old.Indexes {
		if strings.ToLower(ix.Name) > nameKey {
			pos = i
			break
		}
	}
	t.Indexes = make([]indexDef, 0, len(old.Indexes)+1)
	t.Indexes = append(t.Indexes, old.Indexes[:pos]...)
	t.Indexes = append(t.Indexes, def)
	t.Indexes = append(t.Indexes, old.Indexes[pos:]...)
	s.tables[tableKey] = &t
}

// setColumnDefaultExpr replaces a table column's expression default in place — used by ALTER
// SEQUENCE … RENAME of an owned sequence to rewrite the owning column's nextval default
// (spec/design/sequences.md §15.3), leaving the table's rows/store untouched. The Table and its
// Columns slice are re-allocated (catalog tables are never mutated in place — snapshots share them).
// A no-op if the table or column ordinal is absent.
func (s *snapshot) setColumnDefaultExpr(tableKey string, column int, de *defaultExprDef) {
	old, ok := s.tables[tableKey]
	if !ok || column < 0 || column >= len(old.Columns) {
		return
	}
	s.bumpCatGen()
	t := *old
	t.Columns = make([]catColumn, len(old.Columns))
	copy(t.Columns, old.Columns)
	t.Columns[column].DefaultExpr = de
	s.tables[tableKey] = &t
}

// putIndexStore registers a loaded index store under its (lowercased) name — the file
// loader's hook (format.go): the owning table's Indexes list came from its catalog entry,
// so only the store is registered here.
func (s *snapshot) putIndexStore(nameKey string, store *tableStore) {
	// An index store created in-session binds the domain's pager like a table store (putTableResolved)
	// so it joins the post-commit residency flip; a store loaded from a file already attached it.
	if s.storePaging != nil && store.paging == nil {
		store.attachPaging(s.storePaging)
	}
	s.indexStores[nameKey] = store
}

// gistTreeFor returns the resident GiST R-tree of the named index (lowercased key), or nil if the
// index is not GiST / not present (spec/design/gist.md §4.1). The planner descends it for a &&/@>
// bound.
func (s *snapshot) gistTreeFor(nameKey string) *gistTree { return s.gistTrees[nameKey] }

// rebuildGistTrees rebuilds EVERY GiST index's resident R-tree from its leaf-key store
// (spec/design/gist.md §3/§4.1). Called after any statement that may have changed a GiST index's
// leaf set (the mutating-statement hook) and on load, so the working snapshot always carries a fresh
// tree a subsequent read descends. Each tree is built CANONICALLY (buildGistFromLeafKeys), making it
// a pure function of the leaf SET — content-deterministic, cross-core identical, and identical to the
// on-disk persisted R-tree. Trees whose index has been dropped are removed. A whole-tree rewrite (the
// §4.1(b) narrowing extended to in-memory writes); the O(rows)-per-mutation cost is unmetered
// structure maintenance on the (trusted) write path — the untrusted surface is SELECT-only.
func (s *snapshot) rebuildGistTrees() error {
	type spec struct {
		nameKey string
		ops     []gistOpclass
	}
	var specs []spec
	for _, t := range s.tables {
		for i := range t.Indexes {
			idx := &t.Indexes[i]
			if idx.Kind != indexGist {
				continue
			}
			// One opclass per indexed column (gist.md §7): single for a GX1/GX2 index, one per
			// WITH column for an EXCLUDE backing index.
			specs = append(specs, spec{nameKey: strings.ToLower(idx.Name), ops: gistOpclassesFor(idx.Columns, t.Columns)})
		}
	}
	live := make(map[string]bool, len(specs))
	for _, sp := range specs {
		live[sp.nameKey] = true
	}
	for k := range s.gistTrees {
		if !live[k] {
			delete(s.gistTrees, k)
		}
	}
	for _, sp := range specs {
		var keys [][]byte
		if store := s.indexStores[sp.nameKey]; store != nil {
			entries, err := store.EntriesInKeyOrder()
			if err != nil {
				return err
			}
			keys = make([][]byte, len(entries))
			for i, e := range entries {
				keys[i] = e.Key
			}
		}
		tree, err := buildGistFromLeafKeys(sp.ops, keys)
		if err != nil {
			return err
		}
		s.gistTrees[sp.nameKey] = tree
	}
	return nil
}

// removeIndex removes one secondary index (DROP INDEX): its definition from the owning
// table and its store.
func (s *snapshot) removeIndex(tableKey, nameKey string) {
	s.bumpCatGen()
	if old, ok := s.tables[tableKey]; ok {
		t := *old
		t.Indexes = nil
		for _, ix := range old.Indexes {
			if strings.ToLower(ix.Name) != nameKey {
				t.Indexes = append(t.Indexes, ix)
			}
		}
		s.tables[tableKey] = &t
	}
	delete(s.indexStores, nameKey)
}

// findIndex finds the table owning the named index (case-insensitive): (tableKey, def, true).
func (s *snapshot) findIndex(name string) (string, indexDef, bool) {
	key := strings.ToLower(name)
	for tk, t := range s.tables {
		for _, ix := range t.Indexes {
			if strings.ToLower(ix.Name) == key {
				return tk, ix, true
			}
		}
	}
	return "", indexDef{}, false
}

// Engine is the database handle: the last committed Snapshot plus, while a transaction is open,
// the writer's working snapshot (CLAUDE.md §3, transactions.md §2). Reads run against the visible
// snapshot — the open transaction's working if any, else committed; a write mutates working and
// commit swaps committed := working (rollback drops working, since committed was never touched).
// Every write — autocommit included — runs as a transaction, which unifies the two paths.
type engine struct {
	committed *snapshot
	// session is the DEFAULT SESSION (spec/design/session.md §2.1): the per-connection state this
	// handle runs statements through — the open transaction (the Idle/Open/Failed machine, §2.2),
	// the relocated settings (maxCost/maxSQLLength/workMem, the entropy/clock seam), and the
	// currval/lastval session state. A bare Engine IS committed storage + this one long-lived
	// stateful default session; the convenience methods operate on it. NewSession mints additional
	// independent sessions (run sequentially on this single-threaded handle by swapping in here).
	session sessionState
	// path is the backing file (empty for an in-memory database). Set by the host API
	// Open/Create (spec/design/api.md §2); Commit writes here.
	path string
	// pageSize is the page size this database serializes with (fixed for the life of a file).
	pageSize uint32
	// pageCount is the on-disk page high-water — the index an incremental commit extends at when the
	// free-list is exhausted (spec/fileformat/format.md). Set from the file's meta on Open, from the
	// initial image on Create; 0 (unused) for an in-memory database.
	pageCount uint32
	// freePages is the free-list (P6.2 + v25): page indices a prior root abandoned, reusable by the
	// next incremental commit (spec/fileformat/format.md *Reclamation*). Read from the persisted chain
	// on Open (v25 — meta offset 28), and returned to within-session by periodic compaction; drawn
	// lowest-first before the file is extended. A page leaves the list only by being allocated into a
	// new committed version, so it is reachable from no live snapshot and reuse is torn-write-safe. nil
	// for a freshly-created file (a from-scratch image leaks nothing).
	freePages []uint32
	// liveAtCompaction is the live (reachable) page count recorded at this handle's last within-session
	// compaction — the cheap periodic trigger basis (v25): a bare-engine file commit re-runs the
	// reclamation walk only once the high-water passes ~2× it, mirroring storage (shared.go). 0 for an
	// in-memory database (no persistence).
	liveAtCompaction uint32
	// paging is the shared paging context for a file-backed database (spec/design/pager.md): the open
	// pager (kept for the handle's life) + the bounded leaf buffer pool, shared with every table store
	// so reads fault OnDisk leaves through the one pool. The load reads pages through it and every
	// commit writes through it. nil for an in-memory database (persist is then a no-op); set by
	// Open/Create, dropped by Close.
	paging *sharedPaging
	// readOnly marks a handle opened read-only (spec/design/api.md §2.1, OpenOptions.ReadOnly).
	// A read-only handle behaves like PostgreSQL hot standby: every transaction defaults to READ
	// ONLY, an explicit READ WRITE request and any write statement are 25006, and the file is
	// opened without write access, so it is never written. Always false for an in-memory or
	// normally-opened database.
	readOnly bool
	// tempStorage is the SESSION-LOCAL temp domain's storage identity (temp-tables.md §6): the private
	// in-RAM memoryBlockStore + pager + pinned pool its temp tables ride, with within-session compaction
	// on. Created lazily on the first session-local temp DDL (newTempStorage); nil until then. Its
	// pageCount is the domain's footprint — the page-based temp budget.
	tempStorage *storage
	// openStreams counts this handle's live streaming cursors (Query's pull source, not a materialized
	// result). A streaming cursor pins a snapshot it faults lazily, so while one is open a temp-domain
	// compaction (persistTemp → maybeCompact) must NOT reclaim pages — it could free one the cursor still
	// faults. Incremented when a streaming Rows opens, decremented on Close (single-threaded per handle).
	openStreams int
	// core is the shared core this engine's session belongs to (attached-databases.md §5), or nil for a
	// bare/transient engine (a test engine, a snapshotEngine, committedEngine — none of which see
	// attachments). It is the engine's route to the core-owned attachment registry (core.attachments)
	// during a commit persist; the READ view of attachments is the pinned attachedCommitted below.
	core *sharedCore
	// attachedCommitted is the PINNED committed root of every host-attached DATABASE-scoped database
	// (attached-databases.md §5), keyed by lowercased name — this session's stable read view, snapshot
	// isolated: refreshed from core.roots.attached at each autocommit statement (refreshCommitted) and
	// pinned for the life of an explicit BEGIN block. nil/empty when nothing is attached. Session-local
	// temp is NOT here (it is on sessionState.tempCommitted); this is only the Database-scoped roots.
	attachedCommitted map[string]*snapshot
}

// SessionOptions are the relocatable session settings (spec/design/session.md §3 — the bucket-A
// envelope subset landed in S1): the cost ceiling, the input-size limit, and the work-memory
// budget. Passed to (*Engine).NewSession. A zero MaxSQLLength or WorkMem takes its default at
// construction (use the setter for the 0 ⇒ unlimited form); a zero MaxCost IS unlimited (the
// genuine default). The entropy/clock seam is injected via Session.SetRandomSource/SetClockSource.
type SessionOptions struct {
	MaxCost int64
	// LifetimeMaxCost is the per-session cumulative cost budget (spec/design/session.md §5.4); 0 ⇒
	// unlimited (the default). Bounds the whole session: the instant the session's running total
	// reaches it, the in-flight statement aborts 54P02 (and once spent, every further statement is
	// rejected at admission). Sibling to MaxCost, which bounds one statement.
	LifetimeMaxCost int64
	MaxSQLLength    int
	WorkMem         int
	// DefaultPrivileges is the table-privilege set granted to every table — the GRANT … ON ALL TABLES
	// default (spec/design/session.md §5.3). nil ⇒ all four (the default), so a fresh session is
	// unrestricted; PrivSetEmpty.With(PrivSelect) is a read-only session. A pointer so the zero
	// SessionOptions stays permissive (the empty set is a meaningful, distinct value).
	DefaultPrivileges *PrivilegeSet
	// AllowDDL governs whether PERSISTENT DDL (CREATE/DROP/ALTER of persistent relations) is permitted;
	// a denied schema change is 42501 (§5.3). nil ⇒ on (the default). A pointer so the zero
	// SessionOptions allows DDL. Its scope narrows with temporary tables (temp-tables.md §5): AllowTempDDL
	// is the temp-scoped sibling gate.
	AllowDDL *bool
	// AllowTempDDL governs whether SESSION-LOCAL temporary-table DDL is permitted
	// (spec/design/temp-tables.md §5); a denied temp DDL is 42501. nil ⇒ INHERIT AllowDDL's value
	// (back-compat: a session left as-is behaves as before, one gate governing all DDL). The
	// untrusted-scratch pattern is AllowDDL=false + AllowTempDDL=&true — private scratch tables only.
	AllowTempDDL *bool
	// TempBuffers is the per-session storage budget for session-local temp tables, in BYTES
	// (spec/design/temp-tables.md §7); 0 ⇒ unlimited; nil ⇒ the engine default (DefaultTempBuffers).
	// Bounds the RETAINED temp storage neither cost ceiling covers — an over-budget temp write aborts 54P03.
	TempBuffers *int
	// TimeZone is the session time zone (spec/design/session.md §6.2, timezones.md §9.4): the zone a
	// timestamptz is decomposed in by date_trunc / EXTRACT / the cross-family casts. "" ⇒ UTC. Accepts
	// UTC, a fixed ±HH:MM offset, or a named IANA zone a loaded JTZ bundle provides; an invalid value
	// here falls back to UTC at mint (the validated setter is Session.SetTimeZone — 22023).
	TimeZone string
}

// TxStatus is the session transaction status (spec/design/session.md §2.2) — PostgreSQL's three
// connection states made explicit on the session, derived from the open transaction: no
// transaction ⇒ Idle (autocommit); an open clean block ⇒ Open; an open block a statement aborted ⇒
// Failed (only ROLLBACK/COMMIT accepted, everything else 25P02).
type TxStatus int

const (
	TxIdle TxStatus = iota
	TxOpen
	TxFailed
)

func (s TxStatus) String() string {
	switch s {
	case TxOpen:
		return "Open"
	case TxFailed:
		return "Failed"
	default:
		return "Idle"
	}
}

func txStatusOf(tx *activeTx) TxStatus {
	switch {
	case tx == nil:
		return TxIdle
	case tx.failed:
		return TxFailed
	default:
		return TxOpen
	}
}

// sessionState is the per-connection SESSION envelope (spec/design/session.md §2.1/§2.4): the
// configured, stateful context a host runs statements through, un-fused from the committed storage on
// Engine. It owns the open transaction (the Idle/Open/Failed machine), the relocated handle settings,
// the entropy/clock seam, and the currval/lastval session state. An Engine holds one as its default
// session; the host-facing Session (shared.go) wraps an Engine and exposes this envelope, delegating
// its setters/getters here. (Pre-§2.4 this type was the exported `Session`; the convergence renamed
// it and made the per-caller handle the public `Session`.)
type sessionState struct {
	// tx is the open transaction, or nil under autocommit (transactions.md §4.1); a single-statement
	// autocommit write opens one implicitly for its duration. The Idle/Open/Failed status (session.md
	// §2.2) is derived from this (txStatusOf).
	tx *activeTx
	// maxCost is the execution-cost ceiling (CLAUDE.md §13; spec/design/api.md §8), or 0 for
	// unlimited. Bounds every statement run on this session: its Meter aborts 54P01 the instant
	// accrued cost reaches it. The primary guard for untrusted queries.
	maxCost int64
	// lifetimeMaxCost is the per-session cumulative cost budget (spec/design/session.md §5.4), or 0
	// for unlimited. Bounds the whole session: the instant lifetimeTotal reaches it the in-flight
	// statement aborts 54P02, and once spent every further statement is rejected 54P02 at admission.
	// Sibling to maxCost (one statement).
	lifetimeMaxCost int64
	// lifetimeTotal points at the session's running CUMULATIVE execution cost (spec/design/session.md
	// §5.4) — the gauge LifetimeCost reads and the 54P02 budget bounds. A *int64 (heap) shared with
	// every statement Meter, which live-charges into it, so partial cost of an aborted statement
	// counts; a pointer so the activate() VALUE swap of the session keeps the same counter. SESSION
	// state, not snapshot state: it does NOT roll back when a transaction rolls back.
	lifetimeTotal *int64
	// cancel is the per-statement cancellation poll the ergonomic API arms for one statement
	// (spec/design/api.md §11.4): nil unless a host cancellation handle (Go context.Context, …) is
	// active. newMeter copies it into the statement's meter, whose Guard() polls it at each metering
	// checkpoint, so a flipped handle aborts a long-running statement (57014) — not only at the
	// cursor boundary. Set/cleared by engine.armCancel around a single statement (ergonomic.go); a
	// single atomic load on the hot path.
	cancel func() bool
	// maxSQLLength is the maximum input SQL length in bytes (CLAUDE.md §13; cost.md §7); 0 =
	// unlimited; default DefaultMaxSQLLength (1 MiB). Over-limit input is rejected 54000 at parse,
	// before lexing.
	maxSQLLength int
	// workMem is the work-memory budget in bytes (spec/design/spill.md §2): the memory a blocking
	// operator (the ORDER BY external merge sort) holds before it spills. 0 = unlimited; default
	// DefaultWorkMem. Never changes what a query observes (spill.md §6); an in-memory database
	// ignores it.
	workMem int
	// seam is the entropy + clock seam for the uuid generators / clock functions (entropy.md): two
	// host-injectable functions (a random source + a clock), each nil ⇒ the platform primitive.
	// Tests inject SeededRandomSource + FixedClock (the # seed: / # clock: directives) for
	// byte-identical cross-core output.
	seam seam
	// sessionSeq is the SESSION currval state (sequences.md §6): the last value nextval/setval(…,true)
	// produced IN THIS SESSION for each sequence (lowercased name). NOT in the snapshot, NOT persisted.
	sessionSeq map[string]int64
	// sessionLastName is the SESSION lastval state (sequences.md §6): the lowercased name of the
	// sequence the most recent nextval (of any sequence) ran on — "" before the first nextval.
	sessionLastName string
	// pendingSeq is the per-STATEMENT running sequence advances (sequences.md §4); flushed into the
	// working snapshot on success, discarded on error (the transactional rollback of the advance, §5).
	pendingSeq map[string]*sequenceDef
	// pendingCurrval is the per-STATEMENT running currval updates → flushed into sessionSeq on success.
	pendingCurrval map[string]int64
	// pendingLastName is the per-STATEMENT running lastval update → flushed into sessionLastName.
	pendingLastName string
	// privileges is the authorization envelope (spec/design/session.md §5.3): the GRANT/REVOKE-style
	// per-object privilege model the host configures and the engine enforces (42501) at name
	// resolution. A fresh session is fully permissive (every table privilege, every function EXECUTE).
	privileges Privileges
	// allowDDL governs whether PERSISTENT DDL (CREATE/DROP/ALTER of persistent relations) is permitted
	// on this session (§5.3); a denied schema change is 42501. Default on. Its scope narrows with
	// temporary tables (temp-tables.md §5): allowTempDDL is the temp-scoped sibling gate.
	allowDDL bool
	// allowTempDDL governs whether session-local TEMPORARY-table DDL is permitted
	// (spec/design/temp-tables.md §5); a denied temp DDL is 42501. Resolved at session creation from
	// SessionOptions.AllowTempDDL (defaulting to allowDDL's value when unset).
	allowTempDDL bool
	// tempBuffers is the per-session temp-table storage budget in BYTES (temp-tables.md §7); 0 ⇒
	// unlimited. An over-budget temp write aborts 54P03.
	tempBuffers int
	// tempCommitted is the session-local temporary-table catalog + stores (spec/design/temp-tables.md
	// §2): a Snapshot holding only this session's temp tables, their stores, and their (UNIQUE) index
	// stores. NEVER serialized — only Engine.committed is written to the file, so a temp table makes
	// ZERO file writes. Private to this Session (it carries across the additional-session swap and is
	// invisible to other sessions), and dropped wholesale when the session is. Transactional like the
	// main snapshot: an open transaction clones it into activeTx.tempWorking, which a successful COMMIT
	// adopts back here and a ROLLBACK discards.
	tempCommitted *snapshot
	// vars are the session variables (spec/design/session.md §6.1): PostgreSQL's GUC model scoped to
	// the session — a string→string map (PG GUCs are all text) the host sets (SetVar/ResetVar) and SQL
	// reads with current_setting. Custom (dotted) names only in v1. SESSION state, not snapshot state:
	// it does NOT roll back with a transaction (PG SET SESSION). The map is a reference type, so the
	// activate() value swap keeps each session's own map (like the privilege envelope).
	vars map[string]string
	// timeZone is the resolved session time zone (spec/design/session.md §6.2, timezones.md §9.4): the
	// zone a timestamptz is decomposed in by date_trunc / EXTRACT / the cross-family casts. Resolved
	// once (from SessionOptions.TimeZone at mint, or SetTimeZone) to a cheap ZoneRef (UTC = Fixed 0);
	// the evaluator reads it via the active session. SESSION state (no storage effect).
	timeZone ZoneRef
	// readPin is the read pin for a data-modifying WITH statement (spec/design/writable-cte.md §2):
	// the single pre-statement snapshot every sub-statement reads, so the data-modifying CTEs and the
	// primary cannot observe each other's table writes (their writes still accumulate into the
	// transaction's working). Set by the writable-CTE orchestrator before the first sub-statement runs
	// and cleared when it finishes (success or error); nil for every other statement, where reads fall
	// through to working/committed as usual (readSnap).
	readPin *snapshot
}

// requireCustomVarName validates + canonicalizes a session-variable name (spec/design/session.md
// §6.1). A variable must be namespaced like a PostgreSQL custom GUC — a dotted name (myapp.tenant);
// a non-dotted name would be a built-in setting, and v1 exposes none through this map (the time_zone
// built-in is a separate slice), so it is 42704. Returns the case-folded (lowercase, PG GUC names are
// case-insensitive) map key.
func requireCustomVarName(name string) (string, error) {
	if strings.Contains(name, ".") {
		return strings.ToLower(name), nil
	}
	return "", newError(UndefinedObject, "unrecognized configuration parameter: "+name)
}

// newSession builds a fresh default session: no open transaction, default settings, empty state.
func newSession() sessionState {
	return newSessionWithOptions(SessionOptions{})
}

// newSessionWithOptions builds a session configured from opts (spec/design/session.md §2.1). A zero
// MaxSQLLength or WorkMem takes its default; the rest of the per-connection state starts empty.
func newSessionWithOptions(opts SessionOptions) sessionState {
	if opts.MaxSQLLength == 0 {
		opts.MaxSQLLength = DefaultMaxSQLLength
	}
	if opts.WorkMem == 0 {
		opts.WorkMem = defaultWorkMem
	}
	s := sessionState{
		maxCost:         opts.MaxCost,
		lifetimeMaxCost: opts.LifetimeMaxCost,
		lifetimeTotal:   new(int64),
		maxSQLLength:    opts.MaxSQLLength,
		workMem:         opts.WorkMem,
		privileges:      newPrivileges(),
		allowDDL:        true,
		tempBuffers:     defaultTempBuffers,
		tempCommitted:   newSnapshot(),
		vars:            map[string]string{},
	}
	if opts.DefaultPrivileges != nil {
		s.privileges.SetDefaultTable(*opts.DefaultPrivileges)
	}
	if opts.AllowDDL != nil {
		s.allowDDL = *opts.AllowDDL
	}
	// Back-compat default-inheritance (temp-tables.md §5): an unset AllowTempDDL takes allowDDL's value
	// (resolved above), so a session configured before temp tables existed behaves exactly as it did
	// (one gate governing all DDL).
	s.allowTempDDL = s.allowDDL
	if opts.AllowTempDDL != nil {
		s.allowTempDDL = *opts.AllowTempDDL
	}
	if opts.TempBuffers != nil {
		s.tempBuffers = *opts.TempBuffers
	}
	// Resolve the configured zone once; an invalid value falls back to UTC at mint (the validated
	// path is SetTimeZone, which surfaces 22023). timezones.md §9.4.
	tzName := opts.TimeZone
	if tzName == "" {
		tzName = "UTC"
	}
	if zr, ok := ResolveZone(tzName); ok {
		s.timeZone = zr
	} else {
		s.timeZone = ZoneRef{Fixed: true, Off: 0}
	}
	return s
}

// SetTimeZone sets the session time zone (spec/design/session.md §6.2, timezones.md §9.4): the zone a
// timestamptz is decomposed in. Accepts UTC, a fixed ±HH:MM offset, or a named IANA zone a loaded JTZ
// bundle provides; a name no bundle provides (and not a built-in) is 22023, the value unchanged.
func (s *sessionState) SetTimeZone(zone string) error {
	zr, ok := ResolveZone(zone)
	if !ok {
		return newError(InvalidParameterValue, fmt.Sprintf("time zone %q not recognized", zone))
	}
	s.timeZone = zr
	return nil
}

// activeTx is an open transaction (spec/design/transactions.md §4.2). writable is the access mode
// (READ WRITE vs READ ONLY — a write in a READ ONLY block is 25006); failed marks an aborted block
// (every later statement but COMMIT/ROLLBACK is 25P02 — §6). working is the transaction's snapshot:
// a writable tx mutates it in place and publishes it at commit; a read-only tx reads it unchanged
// (read-your-snapshot, §4.3). committed is untouched until commit, so ROLLBACK just drops this.
type activeTx struct {
	writable bool
	failed   bool
	working  *snapshot
	// savedSessionSeq / savedSessionLastName capture the handle's currval/lastval session state
	// (spec/design/sequences.md §6) when this transaction opened. A nextval/setval inside the block
	// updates the handle's session state per-statement (so an in-block currval sees its own
	// advance), but those updates must ROLL BACK with the transaction (§5) — so ROLLBACK (and a
	// failed/read-only COMMIT) restores these, while a successful COMMIT keeps the advanced state.
	savedSessionSeq      map[string]int64
	savedSessionLastName string
	// tempWorking is the transaction's working copy of the session's temp-table snapshot
	// (spec/design/temp-tables.md §5): cloned from Session.tempCommitted at tx open (cheap — persistent
	// stores clone O(1)), mutated by temp DDL/DML, adopted back into tempCommitted on a successful COMMIT
	// and discarded on ROLLBACK. The temp analogue of working, kept SEPARATE so it is never serialized.
	tempWorking *snapshot
	// mainDirty is whether this transaction mutated the MAIN (persistent) snapshot — set by
	// (*Engine).workingMut. Drives the commit's persist decision so a transaction that touched ONLY
	// temp tables makes zero file writes (temp-tables.md §2).
	mainDirty bool
	// tempDirty is whether this transaction mutated the SESSION-LOCAL TEMP snapshot — set by the temp
	// write funnels. With mainDirty it decides whether COMMIT persists the main image (a pure-temp
	// commit skips it; an empty block still persists, preserving prior behavior).
	tempDirty bool
	// attachWorking is the transaction's working copy of a host-attached database's snapshot
	// (attached-databases.md §5), keyed by lowercased attachment name — the attachment analogue of
	// tempWorking. Cloned lazily from engine.attachedCommitted[name] on the first write to that
	// attachment (attachWriteSnap), so a read-only cross-attachment query allocates nothing here.
	// Adopted into engine.attachedCommitted + persisted+published on a successful COMMIT, discarded on
	// ROLLBACK. nil until an attachment is written.
	attachWorking map[string]*snapshot
	// attachDirty records which attachments this transaction mutated (lowercased name → true), the
	// per-attachment analogue of mainDirty/tempDirty — the set the commit persists + publishes.
	attachDirty map[string]bool
}

// NewEngine builds an empty in-memory database.
func newEngine() *engine {
	return &engine{committed: newSnapshot(), pageSize: DefaultPageSize, session: newSession()}
}

// WithPageSize returns an in-memory handle that serializes at pageSize. The page-backed B-tree's
// fan-out tracks the page size (spec/fileformat/format.md), so the in-memory tree must be built at
// the size it will serialize to — this builds fixtures / tests a non-default page size; a normal
// in-memory database uses NewEngine (the default page size).
func withPageSize(pageSize uint32) *engine {
	return &engine{committed: newSnapshot(), pageSize: pageSize, session: newSession()}
}

// readSnap is the snapshot a read sees: the read pin if one is set (a data-modifying WITH statement
// pins the pre-statement snapshot so every sub-statement reads it — writable-cte.md §2), else the
// open transaction's working (read-your-writes for a writable tx; the pinned snapshot for a
// read-only tx), else the committed snapshot.
func (db *engine) readSnap() *snapshot {
	if db.session.readPin != nil {
		return db.session.readPin
	}
	if db.session.tx != nil {
		return db.session.tx.working
	}
	return db.committed
}

// columnCollations resolves each column's frozen collation (Column.Collation, the name) to its
// baked table, indexed by column ordinal — nil for a C / non-text column (the fast path). The key
// encoders (§2.12) consult colls[ci] to pick a text column's key form.
func (db *engine) ensureCollationsWritable(columns []catColumn) error {
	// Refuse a WRITE that would maintain a collated B-tree under a version-skewed collation (the
	// slice-2d verdict, spec/design/collation.md §12/§14): if any of columns carries a collation the
	// file pinned to a different (unicode, cldr) than the loaded bundle provides, an
	// insert/update/delete/index-build would mix two orderings in one tree and corrupt it, so the
	// whole table is read-only until a REINDEX migration (deferred) rebuilds + re-pins it. XX002,
	// naming the collation + both versions. Reads never call this — they recompute against the loaded
	// table (the heap-scan fallback, compatibility.md §8). Per-table granularity: one skewed column
	// collation makes the table read-only (finer per-index gating is a follow-on).
	snap := db.readSnap()
	for i := range columns {
		if columns[i].Collation == "" {
			continue
		}
		if fu, fc, lu, lc, skewed := snap.collationSkew(columns[i].Collation); skewed {
			return newError(CollationVersionMismatch, fmt.Sprintf(
				"collation %q version mismatch: this database's keys were built under %s/%s but the "+
					"loaded bundle is %s/%s; tables using it are read-only until a REINDEX migration rebuilds them",
				columns[i].Collation, fu, fc, lu, lc,
			))
		}
	}
	return nil
}

func (db *engine) columnCollations(columns []catColumn) []*Collation {
	snap := db.readSnap()
	out := make([]*Collation, len(columns))
	for i := range columns {
		if columns[i].Collation != "" {
			out[i] = snap.resolveCollation(columns[i].Collation)
		}
	}
	return out
}

// collatedTextKey is the order-preserving key body for a text value (encoding.md §2.12): the
// collation's UCA sort key when coll is non-nil (a non-C collated column), else the C
// text-terminated-escape body (§2.4). The sort key can fail (0A000) on a code point the collation
// does not map — propagated, so a collated INSERT of an unmapped string aborts the write.
func collatedTextKey(coll *Collation, s string) ([]byte, error) {
	if coll != nil {
		return sortKey(coll, s)
	}
	return encodeTerminated([]byte(s)), nil
}

// tempDomainPaging returns the MemoryBlockStore paging context for the session-local temp domain
// (temp-tables.md §6), lazily creating the domain's storage identity (newTempStorage — a private
// in-RAM store + pinned pool with within-session compaction on) on first use.
func (db *engine) tempDomainPaging() *sharedPaging {
	if db.tempStorage == nil {
		db.tempStorage = newTempStorage(db.pageSize)
	}
	return db.tempStorage.paging
}

// working is the snapshot a write mutates — the open transaction's working. A write only ever runs
// with a transaction open (autocommit opens one implicitly), so tx is non-nil here.
func (db *engine) working() *snapshot {
	// Mark the main image dirty so the commit knows to persist it; a temp-only transaction never
	// reaches here (it writes via the temp funnels) and so makes zero file writes (temp-tables.md §2).
	db.session.tx.mainDirty = true
	return db.session.tx.working
}

// tempSnap is the session's temp-table snapshot for READS (spec/design/temp-tables.md §2): the open
// transaction's tempWorking, else the session's committed temp state. The temp analogue of readSnap
// (it does not consult readPin — a writable-CTE pins only the main snapshot).
func (db *engine) tempSnap() *snapshot {
	if db.session.tx != nil {
		return db.session.tx.tempWorking
	}
	return db.session.tempCommitted
}

// isTempTable reports whether name resolves to a SESSION-LOCAL temporary table in the visible temp
// snapshot (spec/design/temp-tables.md §3). Preclude-overlaps guarantees a name is temp XOR
// persistent, so this is the routing predicate the table/store funnels use.
func (db *engine) isTempTable(name string) bool {
	_, ok := db.tempSnap().table(name)
	return ok
}

// checkTableQualifier validates an optional database qualifier on a table reference against the
// implicit scope (spec/design/attached-databases.md §3, Slice 1a). A qualified name reaches a specific
// database: `main` (the file / persistent database) or `temp` (the session-local domain) — the two
// reserved implicit qualifiers this slice recognizes; a host-attached database arrives in Slice 1b, so
// any other qualifier is 42P01 "database … is not attached". Because jed precludes overlaps (a name is
// temp XOR persistent within a session, §3), a valid qualifier resolves to the SAME store the bare name
// would, so this is a VALIDATION GATE, not a routing change: it asserts the named relation lives in the
// claimed database (else 42P01), and the downstream temp-first funnel then resolves it to the matching
// scope. A nil qualifier (a bare, implicit-scope name) always passes. The qualifier is matched
// case-insensitively (unquoted identifiers fold to lower case).
func (db *engine) checkTableQualifier(qualifier *string, name string) error {
	if qualifier == nil {
		return nil
	}
	switch strings.ToLower(*qualifier) {
	case "temp":
		if !db.isTempTable(name) {
			return newError(UndefinedTable, `relation "temp.`+name+`" does not exist`)
		}
	case "main":
		if _, ok := db.readSnap().table(name); !ok {
			return newError(UndefinedTable, `relation "main.`+name+`" does not exist`)
		}
	default:
		snap := db.attachReadSnap(strings.ToLower(*qualifier))
		if snap == nil {
			return newError(UndefinedTable, `database "`+*qualifier+`" is not attached`)
		}
		if _, ok := snap.table(name); !ok {
			return newError(UndefinedTable, `relation "`+*qualifier+`.`+name+`" does not exist`)
		}
	}
	return nil
}

// checkAttachmentWritable rejects a WRITE (DML or DDL) targeting a READ-ONLY host attachment with 25006
// (attached-databases.md §4), before any I/O. A nil scope, or `main`/`temp` (never read-only via a
// qualifier — the read-only handle path is separate), or a read-write attachment passes. Unknown
// attachments are caught by the qualifier gate, so this only inspects the attachment's mode.
func (db *engine) checkAttachmentWritable(scope *string) error {
	if scope == nil || db.core == nil {
		return nil
	}
	name := strings.ToLower(*scope)
	if name == "main" || name == "temp" {
		return nil
	}
	if att := db.core.attachments[name]; att != nil && att.mode == attachReadOnly {
		return newError(ReadOnlySqlTransaction,
			`cannot write to read-only database "`+*scope+`"`)
	}
	return nil
}

// isReservedScope reports whether a database qualifier names one of the two implicit reserved scopes
// `main` / `temp` (attached-databases.md §3), which resolve to the SAME store the bare name would — so
// a qualified reference to one keeps every existing fast path. A nil qualifier (a bare implicit-scope
// name) counts as reserved for routing: it too keeps the temp-first funnels.
func isReservedScope(q *string) bool {
	if q == nil {
		return true
	}
	switch strings.ToLower(*q) {
	case "main", "temp":
		return true
	}
	return false
}

// isAttachmentScope reports whether a database qualifier names a HOST-ATTACHED database (not nil, not
// reserved main/temp) — the case that routes to the attachment registry rather than the implicit
// temp-first funnels, and the case that gates off index-bound pushdown / cross-scope catalog lookups
// this slice (attached-databases.md §8).
func isAttachmentScope(q *string) bool { return !isReservedScope(q) }

// isAttachment reports whether this relation targets a host-attached database (attached-databases.md
// §3) rather than the implicit main/temp scope. Index/PK/GiST/GIN bound pushdown is gated off for
// attachment relations this slice: the bounded-scan exec path resolves index stores through the
// UNSCOPED lkpIndexStore funnel, so an attachment relation must full-scan (correct, perf-only — index
// acceleration for attachments is a Slice 1b perf follow-on). A full scan reads the scoped store.
func (rel scopeRel) isAttachment() bool { return isAttachmentScope(rel.db) }

// attachReadSnap returns the READ snapshot of a host-attached database (attached-databases.md §5) — the
// transaction's working clone if this tx wrote it, else the pinned committed root (attachedCommitted).
// nil when no attachment is named `name` (the caller raises 42P01). name is expected lowercased.
func (db *engine) attachReadSnap(name string) *snapshot {
	if db.session.tx != nil {
		if ws := db.session.tx.attachWorking[name]; ws != nil {
			return ws
		}
	}
	return db.attachedCommitted[name]
}

// attachWriteSnap returns the WRITE snapshot of a host-attached database, cloning the pinned committed
// root into the transaction's per-attachment working set on first write and marking it dirty (the
// attachment analogue of working()/tempWorking). Returns nil if the attachment is unknown (unreachable
// after the qualifier gate). name is expected lowercased.
func (db *engine) attachWriteSnap(name string) *snapshot {
	tx := db.session.tx
	if tx.attachWorking == nil {
		tx.attachWorking = make(map[string]*snapshot)
		tx.attachDirty = make(map[string]bool)
	}
	if ws := tx.attachWorking[name]; ws != nil {
		tx.attachDirty[name] = true
		return ws
	}
	base := db.attachedCommitted[name]
	if base == nil {
		return nil
	}
	ws := base.clone()
	tx.attachWorking[name] = ws
	tx.attachDirty[name] = true
	return ws
}

// attachPageSize is the page size of a host attachment's OWN page space (attached-databases.md §2) —
// used to build its NEW stores (CREATE TABLE / CREATE INDEX) at the size its commit serializes to. A
// FILE attachment carries its own page size, baked into the file, which may differ from main's; an
// in-memory attachment matches main. The attachment is known to exist (the qualifier gate passed).
func (db *engine) attachPageSize(name string) uint32 {
	return db.core.attachments[name].storage.pageSize
}

// attachReadView returns the current READ view of every attached database — the transaction's working
// clone where this tx wrote it, else the pinned committed root — as one frozen map. Used to freeze a
// snapshotEngine's attachment view (whose own tx is nil, so it reads straight from this map). Returns
// attachedCommitted directly when no attachment has been written this tx (the common case, no alloc).
func (db *engine) attachReadView() map[string]*snapshot {
	tx := db.session.tx
	if tx == nil || len(tx.attachWorking) == 0 {
		return db.attachedCommitted
	}
	view := make(map[string]*snapshot, len(db.attachedCommitted)+len(tx.attachWorking))
	for k, v := range db.attachedCommitted {
		view[k] = v
	}
	for k, v := range tx.attachWorking {
		view[k] = v
	}
	return view
}

// snapForScope returns the READ snapshot for an explicit database qualifier (attached-databases.md §3):
// `main` / `temp` / a host attachment. Used only when scope != nil; a nil scope keeps the bare
// temp-first funnels (a name is temp XOR persistent). nil for an unknown attachment (the qualifier gate
// already raised 42P01, so unreachable in practice).
//
// This funnel IS where Slice 1c's "temp is an implicit in-memory attachment" reframe is realized
// (attached-databases.md §6): `temp`, `main`, and every host attachment resolve through one
// scoped-routing path, so a temp table is a citizen of the same mechanism an attachment is. What stays
// deliberately distinct is temp's *lifecycle* — it is SESSION-SCOPED (tempSnap reads session-private
// state; commit lands on session.tempCommitted with no cross-session roots publish; its reclamation
// watermark is db.openStreams, not the Database-wide live registry). That divergence is correct, not a
// gap: relocating temp into the Database-scoped attachment registry would re-share it across sessions
// (what Slice 0 removed). So temp routes like an attachment here but keeps its own home.
func (db *engine) snapForScope(scope string) *snapshot {
	switch strings.ToLower(scope) {
	case "temp":
		return db.tempSnap()
	case "main":
		return db.readSnap()
	default:
		return db.attachReadSnap(strings.ToLower(scope))
	}
}

// lkpTableScoped resolves a table's catalog entry honoring an explicit database qualifier
// (attached-databases.md §3); a nil scope keeps the bare temp-first walk.
func (db *engine) lkpTableScoped(scope *string, name string) (*catTable, bool) {
	if scope == nil {
		return db.lkpTable(name)
	}
	snap := db.snapForScope(*scope)
	if snap == nil {
		return nil, false
	}
	return snap.table(name)
}

// lkpStoreScoped resolves a table's READ store honoring an explicit database qualifier; nil scope keeps
// the bare temp-first funnel.
func (db *engine) lkpStoreScoped(scope *string, name string) *tableStore {
	if scope == nil {
		return db.lkpStore(name)
	}
	snap := db.snapForScope(*scope)
	if snap == nil {
		return nil
	}
	return snap.store(name)
}

// writeStoreScoped resolves a table's WRITE store honoring an explicit database qualifier, marking the
// right domain dirty (main / temp / the attachment); nil scope keeps the bare temp-first funnel.
func (db *engine) writeStoreScoped(scope *string, name string) *tableStore {
	if scope == nil {
		return db.writeStore(name)
	}
	switch strings.ToLower(*scope) {
	case "temp":
		db.session.tx.tempDirty = true
		return db.session.tx.tempWorking.store(name)
	case "main":
		return db.working().store(name)
	default:
		ws := db.attachWriteSnap(strings.ToLower(*scope))
		if ws == nil {
			return nil
		}
		return ws.store(name)
	}
}

// lkpIndexStoreScoped / writeIndexStoreScoped are the index-store analogues of lkpStoreScoped /
// writeStoreScoped: an index belongs to the same database as its table, so the DML target's scope
// routes them. nil scope keeps the bare temp-first funnel.
func (db *engine) lkpIndexStoreScoped(scope *string, nameKey string) *tableStore {
	if scope == nil {
		return db.lkpIndexStore(nameKey)
	}
	snap := db.snapForScope(*scope)
	if snap == nil {
		return nil
	}
	return snap.indexStore(nameKey)
}

func (db *engine) writeIndexStoreScoped(scope *string, nameKey string) *tableStore {
	if scope == nil {
		return db.writeIndexStore(nameKey)
	}
	switch strings.ToLower(*scope) {
	case "temp":
		db.session.tx.tempDirty = true
		return db.session.tx.tempWorking.indexStore(nameKey)
	case "main":
		return db.working().indexStore(nameKey)
	default:
		ws := db.attachWriteSnap(strings.ToLower(*scope))
		if ws == nil {
			return nil
		}
		return ws.indexStore(nameKey)
	}
}

// compositeDependentAny is the DROP TYPE … RESTRICT dependency check across every visible scope
// (spec/design/temp-tables.md §8): the main image (tables + composite fields), then the visible
// session-local temp snapshot (its tables). A composite type is always persistent, but a TEMP table
// column may reference it, so dropping the type while such a temp table exists is 2BP01 — matching the
// persistent case (PostgreSQL blocks the drop). A session sees only its own session-local temp tables
// (another session's private temp table is invisible by design — and its resolved ColType is
// self-contained, so it keeps working regardless).
func (db *engine) compositeDependentAny(name string) (string, bool) {
	if dep, ok := db.readSnap().compositeDependent(name); ok {
		return dep, true
	}
	return db.tempSnap().compositeDependent(name)
}

// isTempIndex reports whether name is a secondary index on a SESSION-LOCAL temp table
// (spec/design/temp-tables.md §8) — the index analogue of isTempTable, used to gate (allowTempDDL)
// and route a DROP INDEX of a temp index. Preclude-overlaps keeps an index name in one scope.
func (db *engine) isTempIndex(name string) bool {
	_, _, ok := db.tempSnap().findIndex(name)
	return ok
}

// sequence resolves a sequence by name along the resolution walk session-local → persistent
// (spec/design/sequences.md + temp-tables.md §8). Preclude-overlaps keeps a name in at most one scope
// (the shared relation namespace), so this is just "where the sequence lives". Every sequence READ
// (nextval/currval/setval resolution, DROP/ALTER SEQUENCE) goes through here, so a serial/IDENTITY
// column's OWNED temp sequence resolves exactly like a persistent one.
func (db *engine) sequence(name string) *sequenceDef {
	if s := db.tempSnap().sequence(name); s != nil {
		return s
	}
	return db.readSnap().sequence(name)
}

// isTempSequence reports whether name is a sequence in the SESSION-LOCAL temp snapshot
// (temp-tables.md §8) — the sequence analogue of isTempTable. A temp sequence only ever arises from a
// serial/IDENTITY temp column (standalone CREATE SEQUENCE is always persistent), so it is always owned.
func (db *engine) isTempSequence(name string) bool {
	return db.tempSnap().sequence(name) != nil
}

// anyTempSequence reports whether any name in a DROP SEQUENCE list is a session-local temp sequence —
// the gate classifier for a temp DROP SEQUENCE (§5/§8).
func (db *engine) anyTempSequence(names []string) bool {
	for _, n := range names {
		if db.isTempSequence(n) {
			return true
		}
	}
	return false
}

// anyTempTable reports whether any name in a multi-table DROP TABLE resolves to a session-local temp
// table — the DDL capability gate's classification of a mixed list (temp-tables.md §5): if any target
// is temp-scoped the whole statement is gated by the temp-DDL grant.
func (db *engine) anyTempTable(names []string) bool {
	for _, n := range names {
		if db.isTempTable(n) {
			return true
		}
	}
	return false
}

// putSequenceRouted stages a sequence def into whichever scope currently owns its name (flagging the
// matching dirty bit): session-local temp, else the main working set. A serial/IDENTITY temp column's
// owned sequence advances (nextval flush) into its temp snapshot — like the table's rows, zero file
// writes (temp-tables.md §2); a brand-new persistent sequence is absent from the temp scope and lands
// in the main image.
func (db *engine) putSequenceRouted(def *sequenceDef) {
	if db.isTempSequence(def.Name) {
		db.session.tx.tempDirty = true
		db.session.tx.tempWorking.putSequence(def)
	} else {
		db.working().putSequence(def)
	}
}

// removeSequenceRouted removes a sequence from whichever scope owns its name (the routed analogue of
// putSequenceRouted). Used by DROP SEQUENCE and DROP TABLE's owned-sequence auto-drop.
func (db *engine) removeSequenceRouted(name string) {
	key := strings.ToLower(name)
	if db.isTempSequence(name) {
		db.session.tx.tempDirty = true
		db.session.tx.tempWorking.removeSequence(key)
	} else {
		db.working().removeSequence(key)
	}
}

// setColumnDefaultExprRouted rewrites a column's stored DEFAULT expression in whichever scope owns the
// table — the routed analogue used by ALTER SEQUENCE … RENAME of an owned sequence (temp-tables.md §8),
// so a renamed owned TEMP sequence's nextval default is rewritten in the temp snapshot.
func (db *engine) setColumnDefaultExprRouted(tableKey string, column int, de *defaultExprDef) {
	if db.isTempTable(tableKey) {
		db.session.tx.tempDirty = true
		db.session.tx.tempWorking.setColumnDefaultExpr(tableKey, column, de)
	} else {
		db.working().setColumnDefaultExpr(tableKey, column, de)
	}
}

// lkpTable resolves a table by name along the resolution walk session-local → persistent
// (temp-tables.md §3). Preclude-overlaps keeps a name in at most one scope, so this is just "where it lives".
func (db *engine) lkpTable(name string) (*catTable, bool) {
	if t, ok := db.tempSnap().table(name); ok {
		return t, true
	}
	return db.readSnap().table(name)
}

// lkpStore returns a table's store for READS, routing by the resolution walk (session-local temp →
// visible main snapshot — temp-tables.md §2). No dirty flag — reads never persist.
func (db *engine) lkpStore(name string) *tableStore {
	if db.isTempTable(name) {
		return db.tempSnap().store(name)
	}
	return db.readSnap().store(name)
}

// writeStore returns a table's store for MUTATION, routing a session-local temp write to tempWorking
// (flagging tempDirty) and a persistent write to working (which flags mainDirty) — so a pure-temp
// transaction leaves the main image untouched (temp-tables.md §2).
func (db *engine) writeStore(name string) *tableStore {
	if db.isTempTable(name) {
		db.session.tx.tempDirty = true
		return db.session.tx.tempWorking.store(name)
	}
	return db.working().store(name)
}

// lkpIndexStore returns a secondary index's store for READS, walking session-local → main
// (temp-tables.md §8).
func (db *engine) lkpIndexStore(nameKey string) *tableStore {
	if db.tempSnap().hasIndexStore(nameKey) {
		return db.tempSnap().indexStore(nameKey)
	}
	return db.readSnap().indexStore(nameKey)
}

// writeIndexStore returns a secondary index's store for MUTATION, walking session-local → main
// (flagging the matching dirty bit).
func (db *engine) writeIndexStore(nameKey string) *tableStore {
	if db.tempSnap().hasIndexStore(nameKey) {
		db.session.tx.tempDirty = true
		return db.session.tx.tempWorking.indexStore(nameKey)
	}
	return db.working().indexStore(nameKey)
}

// InTransaction reports whether an explicit transaction block is currently open
// (spec/design/transactions.md §4.2). False under autocommit. Used by the host Transaction surface.
func (db *engine) InTransaction() bool { return db.session.tx != nil }

// Txid is the monotonic commit counter (spec/design/api.md §2): the committed snapshot's version.
func (db *engine) Txid() uint64 { return db.committed.txid }

// OldestLiveTxid is the oldest still-live snapshot's txid (spec/design/transactions.md §8) — the
// Phase-6 free-list reclamation gate. Single-handle (P5.3a) it is trivially the committed txid; the
// P5.3b shared read snapshots make it meaningful.
func (db *engine) OldestLiveTxid() uint64 { return db.committed.txid }

// PageSize is the page size this database serializes with (spec/design/api.md §2).
func (db *engine) PageSize() uint32 { return db.pageSize }

// PageCount is the committed logical page high-water — the number of pages the on-disk image
// references (the count the meta records, format.md), the size an incremental commit extends at
// (spec/fileformat/format.md *Reclamation*). It is not the physical file length, which the chunked
// preallocation (pager.go, spec/design/pager.md §7) runs ahead of with trailing zero slack. 0 for a
// fresh in-memory database.
func (db *engine) PageCount() uint32 { return db.pageCount }

// Path is the backing file path, or "" for an in-memory database.
func (db *engine) Path() string { return db.path }

// SetMaxCost sets the execution-cost ceiling for statements run on this handle (CLAUDE.md §13;
// spec/design/api.md §8). A positive limit bounds every subsequent statement: it aborts with
// 54P01 the instant accrued cost reaches limit (spec/design/cost.md §6). limit <= 0 (the default)
// is unlimited. The primary guard for safely evaluating untrusted, user-supplied queries; a handle
// setting, not stored in the file.
func (db *engine) SetMaxCost(limit int64) { db.session.maxCost = limit }

// SetLifetimeMaxCost sets the PER-SESSION cumulative cost budget on the default session
// (spec/design/session.md §5.4); limit <= 0 (the default) is unlimited. Where max_cost bounds one
// statement (54P01), this bounds the whole session: the instant the session's running cumulative
// cost reaches limit the in-flight statement aborts 54P02, and once spent every further statement is
// rejected 54P02 at admission. The multi-tenant / untrusted-host gate atop max_cost; a handle
// setting, not stored in the file.
func (db *engine) SetLifetimeMaxCost(limit int64) { db.session.lifetimeMaxCost = limit }

// LifetimeMaxCost is the default session's per-session cumulative cost budget (0 ⇒ unlimited).
func (db *engine) LifetimeMaxCost() int64 { return db.session.lifetimeMaxCost }

// LifetimeCost is the default session's running CUMULATIVE execution cost so far
// (spec/design/session.md §5.4) — the gauge the lifetime_max_cost budget bounds. Tracked even when
// unlimited; survives a transaction rollback (session state, not snapshot state).
func (db *engine) LifetimeCost() int64 { return *db.session.lifetimeTotal }

// SetDefaultPrivileges replaces the default session's default table-privilege set — the
// GRANT … ON ALL TABLES default (spec/design/session.md §5.3). PrivSetEmpty.With(PrivSelect) makes
// the session read-only (a write resolves to 42501). A handle setting, not stored in the file.
func (db *engine) SetDefaultPrivileges(privs PrivilegeSet) {
	db.session.privileges.SetDefaultTable(privs)
}

// Grant grants privs on a specific object (table or function) on the default session, beyond the
// default (§5.3).
func (db *engine) Grant(privs PrivilegeSet, object string) {
	db.session.privileges.Grant(privs, object)
}

// Revoke revokes privs from a specific object on the default session (revoke wins over grant and the
// default, §5.3).
func (db *engine) Revoke(privs PrivilegeSet, object string) {
	db.session.privileges.Revoke(privs, object)
}

// ResetPrivileges resets the default session's authorization envelope to fully permissive — every
// table privilege, no per-object delta, DDL allowed (§5.3). The conformance harness calls this before
// each record so a # default_privileges: / # grant: / # revoke: / # allow_ddl: directive never leaks
// past the record it decorates.
func (db *engine) ResetPrivileges() {
	db.session.privileges = newPrivileges()
	db.session.allowDDL = true
	// The temp-DDL gate is part of the authorization envelope (temp-tables.md §5); reset it with the
	// rest so a # allow_temp_ddl: directive never leaks past its record. Default-inherit allowDDL=true.
	db.session.allowTempDDL = true
}

// Privileges is read-only access to the default session's authorization envelope (§5.3).
func (db *engine) Privileges() *Privileges { return &db.session.privileges }

// SetAllowDDL sets whether DDL is permitted on the default session (§5.3); a denied schema change is
// 42501.
func (db *engine) SetAllowDDL(allow bool) { db.session.allowDDL = allow }

// AllowDDL reports whether DDL is permitted on the default session.
func (db *engine) AllowDDL() bool { return db.session.allowDDL }

// SetAllowTempDDL sets whether session-local temporary-table DDL is permitted on the default session
// (spec/design/temp-tables.md §5) — the temp-scoped split of AllowDDL; a denied temp DDL is 42501.
func (db *engine) SetAllowTempDDL(allow bool) { db.session.allowTempDDL = allow }

// AllowTempDDL reports whether session-local temporary-table DDL is permitted on the default session.
func (db *engine) AllowTempDDL() bool { return db.session.allowTempDDL }

// SetTempBuffers sets the default session's per-session temp-table storage budget in BYTES
// (spec/design/temp-tables.md §7); 0 ⇒ unlimited. An over-budget temp write aborts 54P03.
func (db *engine) SetTempBuffers(bytes int) { db.session.tempBuffers = bytes }

// TempBuffers reports the default session's per-session temp-table storage budget (0 ⇒ unlimited).
func (db *engine) TempBuffers() int { return db.session.tempBuffers }

// SetVar sets a session variable on the default session (spec/design/session.md §6.1). Custom
// variables must be namespaced (a dotted name); a non-dotted name is 42704. Read it back in SQL with
// current_setting('name'[, missing_ok]).
func (db *engine) SetVar(name, value string) error { return db.session.SetVar(name, value) }

// ResetVar clears a session variable on the default session (§6.1); a non-dotted name is 42704.
func (db *engine) ResetVar(name string) error { return db.session.ResetVar(name) }

// Var reads a session variable's value on the default session (§6.1); ok is false if it is not set.
func (db *engine) Var(name string) (string, bool) { return db.session.Var(name) }

// ResetVars clears every session variable on the default session (§6.1) — PostgreSQL's RESET ALL for
// the variable map (also the conformance harness # set: reset hook).
func (db *engine) ResetVars() { db.session.ResetVars() }

// SetTimeZone sets the time zone on the default session (spec/design/session.md §6.2, timezones.md
// §9.4): the zone a timestamptz is decomposed in by date_trunc / EXTRACT / the cross-family casts.
// Accepts UTC, a fixed ±HH:MM offset, or a named IANA zone a loaded JTZ bundle provides; else 22023.
func (db *engine) SetTimeZone(zone string) error { return db.session.SetTimeZone(zone) }

// SetMaxSQLLength sets the maximum input SQL length, in bytes, accepted on this handle (CLAUDE.md
// §13; spec/design/api.md §8). A statement whose text exceeds bytes is rejected with 54000 at
// parse entry, before lexing — the §13 input-size gate (cost.md §7). 0 is unlimited (a trusted
// caller's opt-out); the default is DefaultMaxSQLLength (1 MiB). A handle setting, not stored in
// the file (mirrors SetMaxCost).
func (db *engine) SetMaxSQLLength(bytes int) { db.session.maxSQLLength = bytes }

// MaxSQLLength is the current input-SQL byte limit (0 = unlimited). See SetMaxSQLLength.
func (db *engine) MaxSQLLength() int { return db.session.maxSQLLength }

// parse parses one statement from sql, first enforcing this handle's maxSQLLength input-size limit
// (CLAUDE.md §13; spec/design/api.md §8, cost.md §7). The §13 input-size gate: an over-limit
// statement is rejected with 54000 before lexing, so unbounded untrusted input cannot exhaust
// parse memory/CPU (the cost meter cannot catch this — parsing precedes metering). maxSQLLength
// == 0 is unlimited. Every handle-bound parse path routes through here (queryValues/Exec/
// Prepare/the session handles), so the per-handle limit has no hole. The byte length is
// len(sql) (Go strings are UTF-8).
func (db *engine) parse(sql string) (statement, error) {
	if db.session.maxSQLLength > 0 && len(sql) > db.session.maxSQLLength {
		return statement{}, newError(ProgramLimitExceeded, fmt.Sprintf("SQL statement exceeds the maximum length of %d bytes", db.session.maxSQLLength))
	}
	return parseSQL(sql)
}

// SetRandomSource injects a random source for the uuid generators (spec/design/entropy.md §6) — the
// deterministic / reproducible path. Pass SeededRandomSource for a byte-identical cross-core stream
// (the conformance # seed: directive). ClearRandomSource returns to the OS CSPRNG, drawn per value.
func (db *engine) SetRandomSource(f RandomSource) { db.session.seam.SetRandom(f) }
func (db *engine) ClearRandomSource()             { db.session.seam.ClearRandom() }

// SetClockSource injects a clock source for uuidv7 (entropy.md §6) — e.g. FixedClock (the # clock:
// directive). ClearClockSource returns to the wall clock.
func (db *engine) SetClockSource(f ClockSource) { db.session.seam.SetClock(f) }
func (db *engine) ClearClockSource()            { db.session.seam.ClearClock() }

// MaxCost is the current execution-cost ceiling (0 ⇒ unlimited). See SetMaxCost.
func (db *engine) MaxCost() int64 { return db.session.maxCost }

// SetWorkMem sets the work-memory budget (in bytes) for blocking operators run on this handle
// (spec/design/spill.md §3, api.md §2.1): the ORDER BY external merge sort holds at most roughly
// this many bytes of rows resident before it spills sorted runs to disk. 0 is unlimited (never
// spill). It never changes what a query observes (results + cost are invariant — spill.md §6), only
// when an operator spills; an in-memory database ignores it. A handle setting, not stored in the
// file (mirrors SetMaxCost).
func (db *engine) SetWorkMem(bytes int) { db.session.workMem = bytes }

// WorkMem is the current work-memory budget in bytes (0 ⇒ unlimited). See SetWorkMem.
func (db *engine) WorkMem() int { return db.session.workMem }

// Status reports the DEFAULT session's transaction status (Idle/Open/Failed, spec/design/session.md
// §2.2) — the explicit three-state machine the convenience methods drive.
func (db *engine) Status() TxStatus { return txStatusOf(db.session.tx) }

// Status reports this session's transaction status (Idle/Open/Failed, session.md §2.2).
func (s *sessionState) Status() TxStatus { return txStatusOf(s.tx) }

// InTransaction reports whether an explicit transaction block is open on this session.
func (s *sessionState) InTransaction() bool { return s.tx != nil }

// MaxCost / SetMaxCost — the per-statement execution-cost ceiling (0 ⇒ unlimited).
func (s *sessionState) MaxCost() int64         { return s.maxCost }
func (s *sessionState) SetMaxCost(limit int64) { s.maxCost = limit }

// LifetimeMaxCost / SetLifetimeMaxCost — the per-session cumulative cost budget (0 ⇒ unlimited,
// spec/design/session.md §5.4). Bounds the whole session: a statement aborts 54P02 the instant the
// session's cumulative cost reaches limit, and once spent every further statement is rejected 54P02
// at admission.
func (s *sessionState) LifetimeMaxCost() int64         { return s.lifetimeMaxCost }
func (s *sessionState) SetLifetimeMaxCost(limit int64) { s.lifetimeMaxCost = limit }

// LifetimeCost is the session's running CUMULATIVE execution cost so far (spec/design/session.md
// §5.4) — the gauge the lifetime_max_cost budget bounds. Tracked even when unlimited; survives a
// transaction rollback (session state, not snapshot state).
func (s *sessionState) LifetimeCost() int64 { return *s.lifetimeTotal }

// newMeter builds the Meter for a statement run on this session: the per-statement max_cost ceiling
// (54P01) plus a handle to the session's cumulative total + budget (54P02). Every statement's meter
// is minted here, so all execution cost live-charges into the cumulative.
func (s *sessionState) newMeter() *costMeter {
	return &costMeter{Limit: s.maxCost, lifetimeTotal: s.lifetimeTotal, lifetimeLimit: s.lifetimeMaxCost, cancel: s.cancel}
}

// MaxSQLLength / SetMaxSQLLength — the input-SQL byte limit (0 ⇒ unlimited).
func (s *sessionState) MaxSQLLength() int     { return s.maxSQLLength }
func (s *sessionState) SetMaxSQLLength(b int) { s.maxSQLLength = b }

// WorkMem / SetWorkMem — the work-memory budget in bytes (0 ⇒ unlimited).
func (s *sessionState) WorkMem() int     { return s.workMem }
func (s *sessionState) SetWorkMem(b int) { s.workMem = b }

// SetDefaultPrivileges replaces the default table-privilege set — the GRANT … ON ALL TABLES default
// (§5.3). A read-only session is PrivSetEmpty.With(PrivSelect).
func (s *sessionState) SetDefaultPrivileges(privs PrivilegeSet) { s.privileges.SetDefaultTable(privs) }

// Grant grants privs on a specific object (table or function), beyond the default (§5.3).
func (s *sessionState) Grant(privs PrivilegeSet, object string) { s.privileges.Grant(privs, object) }

// Revoke revokes privs from a specific object (revoke wins over grant and the default, §5.3).
func (s *sessionState) Revoke(privs PrivilegeSet, object string) { s.privileges.Revoke(privs, object) }

// Privileges is read-only access to this session's authorization envelope (§5.3).
func (s *sessionState) Privileges() *Privileges { return &s.privileges }

// AllowDDL / SetAllowDDL — whether DDL is permitted on this session (§5.3); a denied change is 42501.
func (s *sessionState) AllowDDL() bool         { return s.allowDDL }
func (s *sessionState) SetAllowDDL(allow bool) { s.allowDDL = allow }

// AllowTempDDL / SetAllowTempDDL — whether session-local temporary-table DDL is permitted on this
// session (spec/design/temp-tables.md §5); a denied temp DDL is 42501.
func (s *sessionState) AllowTempDDL() bool         { return s.allowTempDDL }
func (s *sessionState) SetAllowTempDDL(allow bool) { s.allowTempDDL = allow }

// TempBuffers / SetTempBuffers — the per-session temp-table storage budget in BYTES
// (spec/design/temp-tables.md §7); 0 ⇒ unlimited. An over-budget temp write aborts 54P03.
func (s *sessionState) TempBuffers() int         { return s.tempBuffers }
func (s *sessionState) SetTempBuffers(bytes int) { s.tempBuffers = bytes }

// SetVar sets a session variable (spec/design/session.md §6.1) — PostgreSQL's GUC model, scoped to
// the session. Custom variables must be namespaced (a dotted name like myapp.tenant); a non-dotted
// name is 42704 (no built-in setting is reachable through this map in v1 — the time_zone built-in is
// its own slice). The name is case-insensitive (folded to lowercase, PG); the value is text. Session
// state, not snapshot state — it does NOT roll back with a transaction.
func (s *sessionState) SetVar(name, value string) error {
	key, err := requireCustomVarName(name)
	if err != nil {
		return err
	}
	if s.vars == nil {
		s.vars = map[string]string{}
	}
	s.vars[key] = value
	return nil
}

// ResetVar clears a session variable (§6.1). A non-dotted name is 42704 (as for SetVar); an unset
// name is a no-op success (PG RESET of an unset custom variable).
func (s *sessionState) ResetVar(name string) error {
	key, err := requireCustomVarName(name)
	if err != nil {
		return err
	}
	delete(s.vars, key)
	return nil
}

// Var reads a session variable's value (§6.1); ok is false if it is not set. The host getter never
// errors — it is the SQL current_setting read that raises 42704 on an unset name.
func (s *sessionState) Var(name string) (string, bool) {
	v, ok := s.vars[strings.ToLower(name)]
	return v, ok
}

// ResetVars clears every session variable (§6.1) — PostgreSQL's RESET ALL for the variable map (also
// the per-record reset hook the conformance harness's # set: directive uses).
func (s *sessionState) ResetVars() { s.vars = map[string]string{} }

// SetRandomSource / ClearRandomSource — the uuid-generator entropy seam (entropy.md §6).
func (s *sessionState) SetRandomSource(f RandomSource) { s.seam.SetRandom(f) }
func (s *sessionState) ClearRandomSource()             { s.seam.ClearRandom() }

// SetClockSource / ClearClockSource — the uuidv7 / clock-function clock seam (entropy.md §6).
func (s *sessionState) SetClockSource(f ClockSource) { s.seam.SetClock(f) }
func (s *sessionState) ClearClockSource()            { s.seam.ClearClock() }

// ReadOnly reports whether this handle was opened read-only (spec/design/api.md §2.1): every
// transaction defaults to READ ONLY, writes are 25006, and the file is never written.
func (db *engine) ReadOnly() bool { return db.readOnly }

// Table looks up a table definition by name (case-insensitive) in the visible snapshot.
func (db *engine) Table(name string) (*catTable, bool) {
	return db.readSnap().table(name)
}

// CompositeType looks up a composite type definition by name (case-insensitive) in the visible
// snapshot (spec/design/composite.md); nil if absent.
func (db *engine) CompositeType(name string) *compositeType {
	return db.readSnap().compositeType(name)
}

// putTable registers a new table and its empty store in the working snapshot (DDL is
// transactional — transactions.md §4.5).
func (db *engine) putTable(t *catTable) {
	db.working().putTable(t, db.pageSize)
}

// CollationVerdict is the slice-2d version-skew verdict for one referenced collation
// (spec/design/collation.md §12, compatibility.md §7). VerdictFull ⇒ a loaded bundle provides the
// name at the file's pinned (unicode, cldr), so the collation's objects are read-write. VerdictSkewed
// ⇒ a loaded bundle provides it at a DIFFERENT version, so its objects are read-only (reads recompute
// against the loaded table — the heap-scan fallback; a write raises XX002). A pure comparison of the
// file pin (§5) vs the loaded set — every core computes the identical verdict (the §10 contract).
type collationVerdict int

const (
	verdictFull collationVerdict = iota
	verdictSkewed
)

// CollationInfo is introspection metadata for one loaded collation (db.Collations,
// spec/design/collation.md §1). ContentHash is the CRC-32 of the compiled table (the reference-mode
// stamp, §3/§4); Description is provenance, excluded from the hash. Verdict is the slice-2d
// version-skew verdict (§12) — VerdictFull for the engine-global loaded set (it IS the reference);
// for a database's referenced collations it is VerdictSkewed when the file's pin differs from the
// loaded bundle's.
type collationInfo struct {
	Name           string
	UnicodeVersion string
	CLDRVersion    string
	ContentHash    uint32
	Description    string
	IsDefault      bool
	Verdict        collationVerdict
}

// ImportCollation / ExportCollation are GONE (the reference-only pivot, spec/design/collation.md
// §4.2): a collation is provided by a host-loaded bundle and used by name, never loaded into a
// database. There is no runtime path that constructs or bakes a collation table — the only load is
// LoadUnicodeData of jed's own pinned bundle bytes.

// LoadUnicodeData loads a JUCD Unicode-data bundle (db.LoadUnicodeData, spec/design/collation.md
// §4.2): its collations become resolvable by name for COLLATE, per-column collation, and ORDER BY …
// COLLATE. The loaded set is ENGINE-GLOBAL (§9), so a bundle loaded through any handle is visible
// everywhere — including to a later Engine.Open of a file that REFERENCES one of its collations.
// Privileged host op (not SQL-reachable, no path, no engine I/O — §11); ADDITIVE and idempotent for
// an already-loaded bundle. A malformed bundle is XX001. (Mirrors the package-level LoadUnicodeData,
// which the host may call before opening any file.)
func (db *engine) LoadUnicodeData(data []byte) error {
	return LoadUnicodeData(data)
}

// LoadTimeZoneData loads a JTZ time-zone bundle into the engine-global loaded set
// (db.LoadTimeZoneData, spec/design/timezones.md §3.3). The bytes are jed's own pinned TZif (RFC
// 8536) wrapped in a manifest; the loaded zones become usable by AT TIME ZONE. Like the collation
// seam, this is a privileged host op (not SQL-reachable, no path, no engine I/O — §10), additive and
// idempotent, engine-global so it may be called before open. A malformed bundle is XX001. (UTC and
// fixed offsets are built in and need no load.)
func (db *engine) LoadTimeZoneData(data []byte) error {
	return LoadTimeZoneData(data)
}

// LoadedTimeZones introspects the engine-global loaded zone set (db.LoadedTimeZones, timezones.md
// §3.3) — every named zone (and alias) a loaded bundle provides, ascending by name. A property of the
// running engine, not of this database. UTC and fixed offsets are built in and not listed.
func (db *engine) LoadedTimeZones() []timeZoneInfo {
	return loadedTimeZones()
}

// LoadedCollations introspects the engine-global LOADED collation set (db.LoadedCollations,
// spec/design/collation.md §4.2) — every collation a loaded bundle provides, available to any
// database on this handle, ascending by name. A property of the running ENGINE, not of this database;
// for the collations this database references, use Engine.Collations. IsDefault is always false here
// (that is a per-database property). C is built in and not listed.
func (db *engine) LoadedCollations() []collationInfo {
	colls := loadedCollationTables()
	out := make([]collationInfo, len(colls))
	for i, c := range colls {
		out[i] = collationInfo{
			Name:           c.Name,
			UnicodeVersion: c.UnicodeVersion,
			CLDRVersion:    c.CldrVersion,
			ContentHash:    crc32IEEE(serializeTable(c)),
			Description:    c.Description,
			IsDefault:      false,
			// The loaded set IS the version reference — it can never be skewed against itself.
			Verdict: verdictFull,
		}
	}
	return out
}

// SetDefaultCollation sets the per-database default collation (db.SetDefaultCollation,
// spec/design/collation.md §1). "C" resets to byte order; any other name must be a LOADED collation
// (else 42704). Persisted as the is_default flag on that collation's reference entry at the next
// commit (the entry is emitted because the default references it — §5).
// UpgradeCollations adopts a newly-loaded Unicode version for this database's skewed collations
// (the REINDEX / COLLATION UPGRADE migration, spec/design/collation.md §12). A privileged host op
// like SetDefaultCollation — NOT SQL-reachable, so an untrusted query can never trigger it
// (CLAUDE.md §13). For every collation whose file pin differs from the loaded bundle (Skewed) it
// rebuilds the collated keys (PK + indexes) under the loaded table and re-pins the stamp, clearing
// the skew so the affected tables are read-write again and regain collated-index pushdown.
// Whole-database + atomic (the rebuild stages in a snapshot clone swapped in only on success);
// idempotent (no skew ⇒ a no-op returning 0). Persisted by the next explicit Commit. Returns the
// number of collations re-pinned.
func (db *engine) UpgradeCollations() (int, error) {
	work := db.committed.clone()
	n, err := work.upgradeCollations(db.pageSize)
	if err != nil {
		return 0, err
	}
	if n > 0 {
		db.committed = work
	}
	return n, nil
}

func (db *engine) SetDefaultCollation(name string) error {
	if name == "C" {
		db.committed.defaultCollation = ""
		return nil
	}
	if db.committed.resolveCollation(name) == nil {
		return newError(UndefinedObject, fmt.Sprintf("collation %q does not exist", name))
	}
	db.committed.defaultCollation = name
	return nil
}

// DefaultCollation returns the per-database default collation name — "C" unless SetDefaultCollation
// moved it (db.DefaultCollation, spec/design/collation.md §1).
func (db *engine) DefaultCollation() string {
	if db.committed.defaultCollation == "" {
		return "C"
	}
	return db.committed.defaultCollation
}

// Collations introspects the collations THIS DATABASE references (db.Collations,
// spec/design/collation.md §4.2) — every collation its schema uses (a column's COLLATE, or the
// per-database default), in ascending name order. This is the per-file view; for the engine-global
// LOADED set, use Engine.LoadedCollations. C is built in and not listed.
func (db *engine) Collations() []collationInfo {
	// referencedCollations resolves each referenced name (from a loaded bundle).
	colls, err := db.committed.referencedCollations()
	if err != nil {
		return nil
	}
	out := make([]collationInfo, len(colls))
	for i, c := range colls {
		verdict := verdictFull
		// The slice-2d verdict: Skewed when the file's pin differs from the loaded bundle's version
		// (the object is read-only), else Full (collation.md §12).
		if _, _, _, _, skewed := db.committed.collationSkew(c.Name); skewed {
			verdict = verdictSkewed
		}
		out[i] = collationInfo{
			Name:           c.Name,
			UnicodeVersion: c.UnicodeVersion,
			CLDRVersion:    c.CldrVersion,
			ContentHash:    crc32IEEE(serializeTable(c)),
			Description:    c.Description,
			IsDefault:      db.committed.defaultCollation == c.Name,
			Verdict:        verdict,
		}
	}
	return out
}

// ExecuteStmt executes one parsed statement with no bind parameters.
func (db *engine) ExecuteStmt(stmt statement) (outcome, error) {
	return db.ExecuteStmtParams(stmt, nil)
}

// ExecuteStmtParams executes one parsed statement, binding params to its $N placeholders (nil
// for an unparameterized statement). DDL statements take no parameters — supplying any is a
// 42601 (spec/design/api.md §5).
//
// Transaction control (BEGIN/COMMIT/ROLLBACK) drives the handle's current-transaction state
// directly (spec/design/transactions.md §4.2). Otherwise the statement runs either inside the
// open explicit block or, with none open, under autocommit (§4.1):
//
//   - Inside a block (§4.2/§6): an aborted block rejects every statement but COMMIT/ROLLBACK with
//     25P02; a write in a READ ONLY block is 25006; otherwise the statement runs against the
//     working set in place — no per-statement durable write (the block publishes once, at COMMIT).
//     ANY statement error aborts the block (it enters the failed state); the statement's own
//     two-phase pass already guarantees it wrote nothing partial (§6), so the whole working set is
//     discarded only at ROLLBACK.
//   - Autocommit (§4.1): a read runs against the committed state directly; a write is its own
//     transaction — the committed state is captured first (the stores are O(1) clones via the
//     persistent map, pmap.go), the statement runs, and on success the change is made durable
//     (synchronous, the single persist chokepoint). Any failure — in the statement or in the
//     durable write — restores the captured state (rollback-on-error, discarding partial work and
//     any rowid allocations, §7). For an in-memory database persist is a no-op.
func (db *engine) ExecuteStmtParams(stmt statement, params []Value) (outcome, error) {
	switch {
	case stmt.Begin != nil:
		return db.beginTx(stmt.Begin.Writable, stmt.Begin.ModeSet)
	case stmt.Commit != nil:
		return db.commitTx()
	case stmt.Rollback != nil:
		return db.rollbackTx()
	}
	// Fresh per-statement sequence-advance scratch (a prior statement's error may have left it
	// populated — it is discarded, not flushed, on error; sequences.md §5).
	db.session.pendingSeq = nil
	db.session.pendingCurrval = nil
	db.session.pendingLastName = ""

	// Inside an explicit block?
	if db.session.tx != nil {
		if db.session.tx.failed {
			return outcome{}, newError(InFailedSqlTransaction,
				"current transaction is aborted, commands ignored until end of transaction block")
		}
		// Run the statement; ANY error aborts the block (it enters the failed state — §6).
		var out outcome
		var err error
		if stmtIsWrite(stmt) && !db.session.tx.writable {
			err = newError(ReadOnlySqlTransaction,
				"cannot execute "+stmtKind(stmt)+" in a read-only transaction")
		} else {
			out, err = db.dispatchStmt(stmt, params)
		}
		// Enforce the temp-storage budget after a successful temp write (temp-tables.md §7): an
		// over-budget statement (session-local tempBuffers) becomes a 54P03 error, which aborts the
		// block (the staged temp rows roll back at ROLLBACK). A no-op for non-temp statements.
		if err == nil {
			err = db.checkTempBudget()
		}
		if err != nil {
			db.session.tx.failed = true
			return outcome{}, err
		}
		// Land any nextval advances into the block's working snapshot; COMMIT publishes them,
		// ROLLBACK discards them with the rest of the working set (sequences.md §5).
		db.flushPendingSequences()
		return out, nil
	}

	// Autocommit (no open block): an autocommit write runs as an implicit single-statement
	// transaction — open a working snapshot off committed, run, then commit on success / discard on
	// error. Because the write mutates only working, an error leaves committed untouched (no restore
	// needed); rolled-back rowid allocations vanish with working (§7).
	if !stmtIsWrite(stmt) {
		return db.dispatchStmt(stmt, params)
	}
	// On a read-only handle the implicit transaction is READ ONLY (PostgreSQL hot-standby
	// behavior — api.md §2.1), so an autocommit write fails exactly like a write inside a
	// READ ONLY block.
	if db.readOnly {
		return outcome{}, newError(ReadOnlySqlTransaction,
			"cannot execute "+stmtKind(stmt)+" in a read-only transaction")
	}
	db.session.tx = db.newTx(true)
	out, err := db.dispatchStmt(stmt, params)
	// Enforce the temp-storage budget before committing (temp-tables.md §7): an over-budget temp write
	// in this implicit transaction (session-local tempBuffers) is discarded (rolling back the temp +
	// main changes) and surfaces 54P03.
	if err == nil {
		err = db.checkTempBudget()
	}
	if err != nil {
		// The statement failed before any flush, so session state is untouched; restore from the
		// captured copy anyway to keep the discard path uniform (sequences.md §6).
		db.restoreSessionState(db.session.tx)
		db.session.tx = nil
		return outcome{}, err
	}
	// Persist any nextval advances into the working snapshot before publishing it (sequences.md
	// §5); a non-sequence statement flushes nothing.
	db.flushPendingSequences()
	if _, cerr := db.commitTx(); cerr != nil {
		return outcome{}, cerr
	}
	return out, nil
}

// beginTx opens an explicit transaction (spec/design/transactions.md §4.2). A nested BEGIN (a block
// is already open) is 25001. writable/modeSet carry the *requested* access mode: with modeSet
// false the mode was unspecified and defaults to READ WRITE on a normal handle, READ ONLY on a
// read-only handle (PostgreSQL hot-standby behavior — api.md §2.1); requesting READ WRITE on a
// read-only handle is 25006. The committed snapshot is captured as the transaction's working
// snapshot — a writable tx mutates it in place; a read-only tx reads it unchanged (read-your-
// snapshot, §4.3). Cheap: the persistent stores clone O(1) (pmap.go) and the catalog is shallow.
// committed is untouched until commit.
func (db *engine) beginTx(writable, modeSet bool) (outcome, error) {
	if db.session.tx != nil {
		return outcome{}, newError(ActiveSqlTransaction, "there is already a transaction in progress")
	}
	if modeSet && writable && db.readOnly {
		return outcome{}, newError(ReadOnlySqlTransaction,
			"cannot set transaction read-write mode on a read-only database")
	}
	if !modeSet {
		writable = !db.readOnly
	}
	db.session.tx = db.newTx(writable)
	return outcome{Kind: outcomeStatement, Cost: 0}, nil
}

// newTx opens a transaction over a clone of the committed snapshot, capturing the handle's
// currval/lastval session state so it can be restored if the transaction is discarded (the
// rollback of any in-block nextval/setval session updates — spec/design/sequences.md §5/§6).
func (db *engine) newTx(writable bool) *activeTx {
	saved := make(map[string]int64, len(db.session.sessionSeq))
	for k, v := range db.session.sessionSeq {
		saved[k] = v
	}
	return &activeTx{
		writable:             writable,
		working:              db.committed.clone(),
		tempWorking:          db.session.tempCommitted.clone(),
		savedSessionSeq:      saved,
		savedSessionLastName: db.session.sessionLastName,
	}
}

// restoreSessionState restores the handle's currval/lastval session state from a discarded
// transaction's captured copy (spec/design/sequences.md §5/§6) — the rollback of any in-block
// nextval/setval session updates. Called wherever a transaction is dropped without publishing.
func (db *engine) restoreSessionState(tx *activeTx) {
	db.session.sessionSeq = tx.savedSessionSeq
	db.session.sessionLastName = tx.savedSessionLastName
}

// commitTx commits the current transaction (spec/design/transactions.md §4.2). With no open block
// it is a lenient no-op success. A failed block, or any read-only tx, publishes nothing — the
// working snapshot is dropped (a failed COMMIT is thus a ROLLBACK, PostgreSQL). A READ WRITE block
// publishes its working snapshot: bump its txid (file-backed only — an in-memory database stays at
// txid 0), make it durable (the single persist chokepoint, §9), then swap it in as committed. A
// durable-write failure leaves committed untouched and propagates. Returns to autocommit.
func (db *engine) commitTx() (outcome, error) {
	tx := db.session.tx
	if tx == nil {
		return outcome{Kind: outcomeStatement, Cost: 0}, nil
	}
	db.session.tx = nil
	if tx.failed || !tx.writable {
		// A failed or read-only block publishes nothing — a failed COMMIT is a ROLLBACK (PG), so any
		// in-block session updates revert with the discarded working set (§5/§6). The discarded
		// tempWorking rolls back temp changes too (dropped with tx).
		db.restoreSessionState(tx)
		return outcome{Kind: outcomeStatement, Cost: 0}, nil
	}
	working := tx.working
	// One durable writer per transaction (attached-databases.md §5): at most one FILE-backed database —
	// MAIN or an attached file — may be written per tx (any number of in-memory attachments + session
	// temp are free). Checked here, before any durable page is written (in the shared-core path the main
	// persist is deferred to Session.publish, and the attachment durable commits are the loop below), so a
	// violating tx commits nothing and rolls back cleanly. Deterministic (a count, order-independent).
	if err := db.checkOneDurableWriter(tx); err != nil {
		return outcome{}, err
	}
	// Persist the main image when it changed; a transaction that touched ONLY session-local temp tables
	// skips it entirely so a temp table makes ZERO file writes (temp-tables.md §2). An empty block (no
	// kind dirty) still persists, preserving prior behavior. Temp state is adopted regardless — never
	// serialized, only swapped into the in-memory committed temp snapshot.
	pureTemp := !tx.mainDirty && tx.tempDirty
	if !pureTemp {
		if db.path != "" {
			working.txid = db.committed.txid + 1
		}
		if err := db.persist(working); err != nil { // no-op for an in-memory database
			return outcome{}, err
		}
		db.committed = working
	}
	// A dirty session-local temp domain materializes its working snapshot into its MemoryBlockStore
	// (compact packed leaves + within-session compaction) before it is adopted — zero main-file writes
	// (temp-tables.md §6). Compaction is safe iff no streaming cursor holds an older temp tree.
	if tx.tempDirty && db.tempStorage != nil {
		if err := db.tempStorage.persistTemp(tx.tempWorking, db.openStreams == 0); err != nil {
			return outcome{}, err
		}
	}
	db.session.tempCommitted = tx.tempWorking
	// Adopt each dirtied host-attached database (attached-databases.md §5, the N-root commit) and adopt it
	// into this engine's pinned attached view, so publish swaps a new roots.attached. An IN-MEMORY
	// attachment materializes into its block store persistTemp-style (the same incremental copy-on-write
	// pack as temp, NO fsync — no durability barrier); a FILE attachment (Slice 2) commits DURABLY through
	// commitDurable (dirty pages + alternating meta slot + fsync, its own page space) and takes the
	// post-commit residency flip. The root is DATABASE-scoped (published, cross-session-visible). At most
	// one file attachment is dirty here (the one-durable-writer check above), so ≤1 fsync path runs.
	// Within-session compaction (in-memory only) is safe iff no cross-session reader pins an older root
	// (the live-registry watermark — the committing writer holds the gate but is not in `live`).
	if len(tx.attachDirty) > 0 {
		na := make(map[string]*snapshot, len(db.attachedCommitted))
		for k, v := range db.attachedCommitted {
			na[k] = v
		}
		canReclaim := db.core == nil || !db.core.hasLiveReaders()
		for name := range tx.attachDirty {
			ws := tx.attachWorking[name]
			att := db.core.attachments[name]
			if att == nil {
				continue // detached mid-transaction (unreachable under the writer gate) — nothing to persist
			}
			if att.isFile() {
				// Advance the version for the alternating meta slot + reopen (like the main file commit).
				ws.txid = db.attachedCommitted[name].txid + 1
				if err := att.storage.commitDurable(ws, canReclaim); err != nil {
					return outcome{}, err
				}
				ws.demoteCleanLeaves() // post-commit residency flip (bplus-reshape.md B4), like Session.publish
			} else if err := att.storage.persistTemp(ws, canReclaim); err != nil {
				return outcome{}, err
			}
			na[name] = ws
		}
		db.attachedCommitted = na
	}
	return outcome{Kind: outcomeStatement, Cost: 0}, nil
}

// checkOneDurableWriter enforces the one-durable-writer rule (attached-databases.md §5): a single
// transaction may modify at most one FILE-backed (durable) database — MAIN or one attached file. Any
// number of in-memory attachments and the session temp domain are free (their commit is a crash-free
// pointer swap). Counts the durable databases this tx dirtied; > 1 is 0A000 (the honest v1 narrowing —
// multi-file atomic write is Slice 3). Called at commit, before any durable page is written.
func (db *engine) checkOneDurableWriter(tx *activeTx) error {
	durable := 0
	if tx.mainDirty && db.mainIsDurable() {
		durable++
	}
	if db.core != nil {
		for name := range tx.attachDirty {
			if att := db.core.attachments[name]; att != nil && att.isFile() {
				durable++
			}
		}
	}
	if durable > 1 {
		return newError(FeatureNotSupported, "a transaction may modify at most one durable database")
	}
	return nil
}

// mainIsDurable reports whether this handle's MAIN database is file-backed (durable) rather than
// in-memory — the input to the one-durable-writer count (§5). In the shared-core path the backing path
// lives on the core's storage; a standalone engine carries it on db.path.
func (db *engine) mainIsDurable() bool {
	if db.core != nil {
		return db.core.storage.path != ""
	}
	return db.path != ""
}

// rollbackTx rolls back the current transaction (spec/design/transactions.md §4.2). With no open
// block it is a no-op success. Otherwise the working snapshot is dropped — every staged
// INSERT/UPDATE/DELETE and DDL CREATE/DROP, plus any rowid allocations (§7), vanish with it;
// committed was never mutated, so there is nothing to restore there. The handle's currval/lastval
// session state, however, was updated in place by in-block nextval/setval, so it is restored from
// the block's captured copy (sequences.md §5/§6).
func (db *engine) rollbackTx() (outcome, error) {
	if db.session.tx != nil {
		db.restoreSessionState(db.session.tx)
	}
	db.session.tx = nil
	return outcome{Kind: outcomeStatement, Cost: 0}, nil
}

// seqNextval implements nextval('name') (spec/design/sequences.md §4): advance the named sequence
// and return the new value. The running state lives in pendingSeq, seeded from the working
// snapshot on first touch this statement, and is flushed into the working snapshot + sessionSeq on
// statement success (flushPendingSequences). A missing sequence is 42P01; advancing past a bound
// without CYCLE is 2200H.
func (db *engine) seqNextval(name string) (int64, error) {
	key := strings.ToLower(name)
	var def sequenceDef
	if db.session.pendingSeq != nil {
		if d, ok := db.session.pendingSeq[key]; ok {
			def = *d
		} else if snapDef := db.sequence(name); snapDef != nil {
			def = *snapDef
		} else {
			return 0, newError(UndefinedTable, "relation does not exist: "+name)
		}
	} else if snapDef := db.sequence(name); snapDef != nil {
		def = *snapDef
	} else {
		return 0, newError(UndefinedTable, "relation does not exist: "+name)
	}
	var result int64
	if !def.IsCalled {
		// The first nextval returns START (the current LastValue) without incrementing.
		def.IsCalled = true
		result = def.LastValue
	} else {
		// Advance by increment, treating an i64 overflow or a bound crossing identically.
		next, overflow := checkedAddInt64(def.LastValue, def.Increment)
		inRange := !overflow &&
			((def.Increment > 0 && next <= def.MaxValue) ||
				(def.Increment < 0 && next >= def.MinValue))
		if !inRange {
			if def.Cycle {
				if def.Increment > 0 {
					next = def.MinValue
				} else {
					next = def.MaxValue
				}
			} else {
				kind := "maximum"
				if def.Increment < 0 {
					kind = "minimum"
				}
				return 0, newError(SequenceGeneratorLimitExceeded,
					"nextval: reached "+kind+" value of sequence "+name)
			}
		}
		def.LastValue = next
		result = next
	}
	if db.session.pendingSeq == nil {
		db.session.pendingSeq = make(map[string]*sequenceDef)
	}
	d := def
	db.session.pendingSeq[key] = &d
	// nextval defines this session's currval for the sequence AND makes it the lastval target (the
	// most-recent-nextval sequence; lastval then reads its current session value — §6).
	if db.session.pendingCurrval == nil {
		db.session.pendingCurrval = make(map[string]int64)
	}
	db.session.pendingCurrval[key] = result
	db.session.pendingLastName = key
	return result, nil
}

// seqSetval implements setval('name', n) / setval('name', n, isCalled) (spec/design/sequences.md
// §4): set the sequence's counter directly and return n. A missing sequence is 42P01; n outside
// [MinValue, MaxValue] is 22003. LastValue = n, IsCalled = the flag (default true); when isCalled is
// true the value also defines this session's currval (PG: isCalled=false leaves currval untouched).
// setval never updates lastval (PG — §6).
func (db *engine) seqSetval(name string, n int64, isCalled bool) (int64, error) {
	key := strings.ToLower(name)
	var def sequenceDef
	if d, ok := db.session.pendingSeq[key]; ok {
		def = *d
	} else if snapDef := db.sequence(name); snapDef != nil {
		def = *snapDef
	} else {
		return 0, newError(UndefinedTable, "relation does not exist: "+name)
	}
	if n < def.MinValue || n > def.MaxValue {
		return 0, newError(NumericValueOutOfRange,
			fmt.Sprintf("setval: value %d is out of bounds for sequence %s (%d..%d)",
				n, name, def.MinValue, def.MaxValue))
	}
	def.LastValue = n
	def.IsCalled = isCalled
	if db.session.pendingSeq == nil {
		db.session.pendingSeq = make(map[string]*sequenceDef)
	}
	d := def
	db.session.pendingSeq[key] = &d
	// currval is defined only when isCalled (PG do_setval: elm->last_valid set iff iscalled).
	if isCalled {
		if db.session.pendingCurrval == nil {
			db.session.pendingCurrval = make(map[string]int64)
		}
		db.session.pendingCurrval[key] = n
	}
	return n, nil
}

// seqCurrval implements currval('name') (spec/design/sequences.md §6): the value nextval/
// setval(…,true) last produced for this sequence IN THIS SESSION. Resolves the name against the
// catalog first (42P01 if absent), then reads the running update this statement (pendingCurrval)
// else the session value (sessionSeq); 55000 if it has not been defined this session.
func (db *engine) seqCurrval(name string) (int64, error) {
	if db.sequence(name) == nil {
		return 0, newError(UndefinedTable, "relation does not exist: "+name)
	}
	key := strings.ToLower(name)
	if v, ok := db.session.pendingCurrval[key]; ok {
		return v, nil
	}
	if v, ok := db.session.sessionSeq[key]; ok {
		return v, nil
	}
	return 0, newError(ObjectNotInPrerequisiteState,
		"currval of sequence "+name+" is not yet defined in this session")
}

// seqLastval implements lastval() (spec/design/sequences.md §6): the CURRENT session value of the
// sequence the most recent nextval (of any sequence) ran on IN THIS SESSION — PG reads the last-used
// sequence's cached value, so a setval on that same sequence is reflected, while a setval on a
// different sequence is not. Takes no name argument (no 42P01); 55000 before the first nextval. The
// effective name and its value both honor the statement's running updates over the session state.
func (db *engine) seqLastval() (int64, error) {
	key := db.session.pendingLastName
	if key == "" {
		key = db.session.sessionLastName
	}
	if key == "" {
		return 0, newError(ObjectNotInPrerequisiteState,
			"lastval is not yet defined in this session")
	}
	if v, ok := db.session.pendingCurrval[key]; ok {
		return v, nil
	}
	if v, ok := db.session.sessionSeq[key]; ok {
		return v, nil
	}
	// A nextval always defines the sequence's session value, so a recorded last-name with no value
	// is unreachable; fall back to 55000 defensively rather than returning a wrong value.
	return 0, newError(ObjectNotInPrerequisiteState,
		"lastval is not yet defined in this session")
}

// flushPendingSequences lands the statement's pending sequence advances into the working snapshot
// (so a commit persists them) and the pending session updates into sessionSeq/sessionLastName (so
// currval/lastval see them). Called on the success of a sequence-advancing statement, while a write
// transaction is open; a no-op when nothing advanced. On statement error the pending state is
// instead discarded (cleared at the next statement), giving the transactional rollback (§5).
func (db *engine) flushPendingSequences() {
	for _, def := range db.session.pendingSeq {
		// Route each advance to its owning scope (temp-tables.md §8): a serial/IDENTITY temp column's
		// owned sequence flushes into its temp snapshot (zero file writes), a persistent one into main.
		db.putSequenceRouted(def)
	}
	if len(db.session.pendingCurrval) > 0 && db.session.sessionSeq == nil {
		db.session.sessionSeq = make(map[string]int64)
	}
	for key, v := range db.session.pendingCurrval {
		db.session.sessionSeq[key] = v
	}
	if db.session.pendingLastName != "" {
		db.session.sessionLastName = db.session.pendingLastName
	}
	db.session.pendingSeq = nil
	db.session.pendingCurrval = nil
	db.session.pendingLastName = ""
}

// checkedAddInt64 adds a + b, reporting overflow=true (and an undefined sum) when the result does
// not fit in an i64 — the overflow-safe sequence advance (sequences.md §4).
func checkedAddInt64(a, b int64) (sum int64, overflow bool) {
	sum = a + b
	// Overflow iff the operands share a sign that the sum does not.
	if (a > 0 && b > 0 && sum < 0) || (a < 0 && b < 0 && sum >= 0) {
		return 0, true
	}
	return sum, false
}

// stmtIsWrite reports whether a statement mutates the database (so autocommit must capture +
// durably persist it, and a READ ONLY transaction must reject it — transactions.md §4.1/§4.3).
// Reads (SELECT, set operations) and transaction control run with no data mutation.
func stmtIsWrite(stmt statement) bool {
	// EXPLAIN is a read: plain EXPLAIN plans without executing (even of a DML inner — it never
	// mutates). Only EXPLAIN ANALYZE runs the inner statement, so it is a write iff the inner is
	// (spec/design/explain.md §3).
	if stmt.Explain != nil {
		return stmt.Explain.Analyze && stmtIsWrite(*stmt.Explain.Inner)
	}
	if stmt.CreateTable != nil || stmt.DropTable != nil ||
		stmt.CreateIndex != nil || stmt.DropIndex != nil ||
		stmt.CreateType != nil || stmt.DropType != nil ||
		stmt.CreateSequence != nil || stmt.AlterSequence != nil || stmt.DropSequence != nil ||
		stmt.Insert != nil || stmt.Update != nil || stmt.Delete != nil {
		return true
	}
	// A WITH statement with any data-modifying part is a write (it stages INSERT/UPDATE/DELETE effects
	// — writable-cte.md): it must take the write gate, accumulate into working, and commit.
	if stmt.With != nil && withHasDml(stmt.With) {
		return true
	}
	// A read-shaped statement that calls a sequence-mutating function (nextval/setval) IS a write
	// (spec/design/sequences.md §4): it must take the write gate, stage the advance, and commit
	// (autocommit) — and is 25006 in a READ ONLY transaction, exactly like any other write.
	return stmtCallsSeqMutator(stmt)
}

// stmtCallsSeqMutator reports whether stmt's expression trees contain a sequence-MUTATING function
// call (nextval; in S2, setval) anywhere — which makes an otherwise read-shaped statement a write
// (sequences.md §4). Only the read-shaped statements need checking: INSERT/UPDATE/DELETE/DDL are
// already writes (stmtIsWrite short-circuits before this), and an INSERT VALUES slot is
// literal-only (no function call). currval is a pure read and is NOT counted. The Expr walk is
// exhaustive, so no expression position is missed.
func stmtCallsSeqMutator(stmt statement) bool {
	switch {
	case stmt.Select != nil:
		return selectCallsSeqMutator(stmt.Select)
	case stmt.SetOp != nil:
		return setopCallsSeqMutator(stmt.SetOp)
	case stmt.With != nil:
		for i := range stmt.With.Ctes {
			if cteBodyCallsSeqMutator(&stmt.With.Ctes[i].Body) {
				return true
			}
		}
		return cteBodyCallsSeqMutator(&stmt.With.Body)
	default:
		return false
	}
}

// cteBodyCallsSeqMutator reports whether a cte_body calls a sequence-mutating function. A query body
// delegates to the query walk; a data-modifying body already makes the WITH a write (via withHasDml),
// so this is not reached for it via stmtCallsSeqMutator — it is treated as a write regardless
// (writable-cte.md).
func cteBodyCallsSeqMutator(body *cteBody) bool {
	if body.Query != nil {
		return queryCallsSeqMutator(body.Query)
	}
	return true
}

func queryCallsSeqMutator(qe *queryExpr) bool {
	if qe.Select != nil {
		return selectCallsSeqMutator(qe.Select)
	}
	if qe.SetOp != nil {
		return setopCallsSeqMutator(qe.SetOp)
	}
	if qe.With != nil {
		// A nested WITH's CTE bodies and main body may call a sequence mutator (cte.md §7).
		for i := range qe.With.Ctes {
			if cteBodyCallsSeqMutator(&qe.With.Ctes[i].Body) {
				return true
			}
		}
		return queryCallsSeqMutator(qe.With.Body)
	}
	return false
}

func setopCallsSeqMutator(so *setOp) bool {
	return queryCallsSeqMutator(&so.Lhs) || queryCallsSeqMutator(&so.Rhs)
}

func selectCallsSeqMutator(s *selectStmt) bool {
	for i := range s.Items.Items {
		if exprCallsSeqMutator(&s.Items.Items[i].Expr) {
			return true
		}
	}
	if s.From != nil && tableRefCallsSeqMutator(s.From) {
		return true
	}
	for i := range s.Joins {
		if tableRefCallsSeqMutator(&s.Joins[i].Table) {
			return true
		}
		if s.Joins[i].On != nil && exprCallsSeqMutator(s.Joins[i].On) {
			return true
		}
	}
	if s.Filter != nil && exprCallsSeqMutator(s.Filter) {
		return true
	}
	for i := range s.GroupBy {
		found := false
		s.GroupBy[i].forEachExpr(func(e *exprNode) {
			if exprCallsSeqMutator(e) {
				found = true
			}
		})
		if found {
			return true
		}
	}
	if s.Having != nil && exprCallsSeqMutator(s.Having) {
		return true
	}
	return false
}

func tableRefCallsSeqMutator(t *tableRef) bool {
	for _, a := range t.Args {
		if exprCallsSeqMutator(a) {
			return true
		}
	}
	if t.Subquery != nil && queryCallsSeqMutator(t.Subquery) {
		return true
	}
	for _, row := range t.Values {
		for _, e := range row {
			if exprCallsSeqMutator(e) {
				return true
			}
		}
	}
	return false
}

// exprCallsSeqMutator is exhaustive over Expr: true iff the tree contains a nextval call.
func exprCallsSeqMutator(e *exprNode) bool {
	switch e.Kind {
	case exprFuncCall:
		if strings.EqualFold(e.FuncCall.Name, "nextval") || strings.EqualFold(e.FuncCall.Name, "setval") {
			return true
		}
		for _, a := range e.FuncCall.Args {
			if exprCallsSeqMutator(a) {
				return true
			}
		}
		return false
	case exprColumn, exprQualifiedColumn, exprLiteral, exprTypedLiteral, exprParam:
		return false
	case exprRow, exprArray:
		for i := range e.RowItems {
			if exprCallsSeqMutator(&e.RowItems[i]) {
				return true
			}
		}
		return false
	case exprFieldAccess, exprFieldStar:
		return exprCallsSeqMutator(e.Base)
	case exprQualifiedStar:
		return false // `t.*` is a leaf relation reference — no sub-expression

	case exprSubscript:
		if exprCallsSeqMutator(e.Base) {
			return true
		}
		for i := range e.Subscripts {
			sub := &e.Subscripts[i]
			if sub.Index != nil && exprCallsSeqMutator(sub.Index) {
				return true
			}
			if sub.Lower != nil && exprCallsSeqMutator(sub.Lower) {
				return true
			}
			if sub.Upper != nil && exprCallsSeqMutator(sub.Upper) {
				return true
			}
		}
		return false
	case exprCast:
		return exprCallsSeqMutator(&e.Cast.Inner)
	case exprExtract:
		return exprCallsSeqMutator(&e.Extract.Source)
	case exprCollate:
		return exprCallsSeqMutator(&e.Collate.Inner)
	case exprUnary:
		return exprCallsSeqMutator(&e.Unary.Operand)
	case exprIsNull:
		return exprCallsSeqMutator(&e.IsNullOf.Operand)
	case exprIsJson:
		return exprCallsSeqMutator(&e.IsJsonOf.Operand)
	case exprJsonCtor:
		return exprCallsSeqMutator(&e.JsonCtorOf.Operand)
	case exprJsonExists:
		return exprCallsSeqMutator(&e.JsonExists.Ctx) || exprCallsSeqMutator(&e.JsonExists.Path)
	case exprJsonValue:
		return exprCallsSeqMutator(&e.JsonValue.Ctx) || exprCallsSeqMutator(&e.JsonValue.Path)
	case exprJsonQuery:
		return exprCallsSeqMutator(&e.JsonQuery.Ctx) || exprCallsSeqMutator(&e.JsonQuery.Path)
	case exprBinary:
		return exprCallsSeqMutator(&e.Binary.Lhs) || exprCallsSeqMutator(&e.Binary.Rhs)
	case exprIsDistinct:
		return exprCallsSeqMutator(&e.IsDistinct.Lhs) || exprCallsSeqMutator(&e.IsDistinct.Rhs)
	case exprLike:
		return exprCallsSeqMutator(&e.Like.Lhs) || exprCallsSeqMutator(&e.Like.Rhs)
	case exprRegex:
		return exprCallsSeqMutator(&e.Regex.Lhs) || exprCallsSeqMutator(&e.Regex.Rhs)
	case exprIn:
		if exprCallsSeqMutator(&e.In.Lhs) {
			return true
		}
		for i := range e.In.List {
			if exprCallsSeqMutator(&e.In.List[i]) {
				return true
			}
		}
		return false
	case exprBetween:
		return exprCallsSeqMutator(&e.Between.Lhs) ||
			exprCallsSeqMutator(&e.Between.Lo) ||
			exprCallsSeqMutator(&e.Between.Hi)
	case exprCase:
		if e.Case.Operand != nil && exprCallsSeqMutator(e.Case.Operand) {
			return true
		}
		for i := range e.Case.Whens {
			if exprCallsSeqMutator(&e.Case.Whens[i].Cond) || exprCallsSeqMutator(&e.Case.Whens[i].Result) {
				return true
			}
		}
		if e.Case.Els != nil && exprCallsSeqMutator(e.Case.Els) {
			return true
		}
		return false
	case exprScalarSubquery, exprExists:
		return queryCallsSeqMutator(e.Subquery)
	case exprInSubquery:
		return exprCallsSeqMutator(&e.InSubquery.Lhs) || queryCallsSeqMutator(&e.InSubquery.Query)
	case exprQuantifiedSubquery:
		return exprCallsSeqMutator(&e.QuantifiedSubquery.Lhs) || queryCallsSeqMutator(&e.QuantifiedSubquery.Query)
	case exprQuantified:
		return exprCallsSeqMutator(&e.Quantified.Lhs) || exprCallsSeqMutator(&e.Quantified.Array)
	default:
		return false
	}
}

// privTableReq is one (table, required privilege) pair collected from a statement.
type privTableReq struct {
	name string
	priv Privilege
}

// privReq is the privilege requirements collected from one statement (spec/design/session.md §5.3):
// the per-table privileges, the named functions (each needs EXECUTE), and whether the statement is
// DDL (gated by allowDDL). Collected by an exhaustive AST walk (mirroring exprCallsSeqMutator).
type privReq struct {
	tables    []privTableReq
	functions []string
	isDDL     bool
	// isTempDDL is whether the DDL targets a SESSION-LOCAL temporary table (CREATE TEMP TABLE) — gated
	// by allowTempDDL instead of allowDDL (spec/design/temp-tables.md §5). Set only for a CREATE TEMP;
	// a DROP is classified by resolving the name.
	isTempDDL bool
}

func (r *privReq) needTable(name string, p Privilege) {
	r.tables = append(r.tables, privTableReq{name: name, priv: p})
}
func (r *privReq) needFunction(name string) { r.functions = append(r.functions, name) }

// checkPrivileges enforces the session's authorization envelope for stmt (spec/design/session.md
// §5.3). A fully-permissive session (the default) needs no check. Otherwise DDL is gated by allowDDL,
// and DML requires a per-table privilege for each table it reads (SELECT) or writes
// (INSERT/UPDATE/DELETE) and EXECUTE for each named function it calls. Enforcement is at name
// resolution: a table privilege is required only for a name that resolves to an existing catalog
// table (a missing table stays 42P01; a CTE / derived-table label is statement-local, not a catalog
// object). Missing privilege → 42501.
// checkLifetimeAdmission rejects a statement at admission when the session's lifetime cost budget is
// already spent (spec/design/session.md §5.4): if a budget is set and the session's cumulative cost
// has reached it, no further statement may run (it "cannot accrue") — 54P02. A no-op when the budget
// is unlimited (the default), so the common path pays one comparison.
func (db *engine) checkLifetimeAdmission() error {
	limit := db.session.lifetimeMaxCost
	total := *db.session.lifetimeTotal
	if limit > 0 && total >= limit {
		return newError(SessionCostLimitExceeded, fmt.Sprintf(
			"session exceeded the lifetime cost limit of %d (accrued %d)", limit, total,
		))
	}
	return nil
}

// checkTempBudget enforces the per-session temp-table storage budget (tempBuffers, spec/design/
// temp-tables.md §7) — the §13 gate on RETAINED temp bytes. Checked after each temp-writing statement:
// if the session's temp footprint (byte-identical on-disk record bytes, summed over every temp table +
// index) EXCEEDS the budget, abort 54P03. The over-budget write is in tempWorking, so the abort
// discards it (autocommit) or fails the block (rolled back at ROLLBACK) — nothing commits. tempBuffers
// 0 ⇒ unlimited; a transaction that did not touch temp cannot have grown it, so the check self-gates on
// tempDirty and is a no-op for ordinary (persistent) statements. The WITHIN-statement bound is maxCost.
func (db *engine) checkTempBudget() error {
	limit := db.session.tempBuffers
	if limit == 0 {
		return nil
	}
	if db.session.tx == nil || !db.session.tx.tempDirty {
		return nil
	}
	// Page-based footprint of the session-local temp domain (temp-tables.md §7, Design decision 3): the
	// committed MemoryBlockStore high-water × page size — the honest resident-RAM measure now that temp
	// rides a pager (a record-byte walk would skip demoted OnDisk leaves and undercount a multi-leaf temp
	// table, defeating the §13 bound). Deterministic and cross-core-identical: pageCount is a pure
	// function of operations via the B+tree + within-session compaction. It reflects the state one commit
	// behind (the pending write commits at statement end), so a domain already over budget aborts the NEXT
	// temp write and rolls it back — the "already over budget ⇒ further writes abort" contract (§7).
	var used uint64
	if db.tempStorage != nil {
		used = uint64(db.tempStorage.pageCount) * uint64(db.pageSize)
	}
	if used > uint64(limit) {
		return newError(TempStorageLimitExceeded, fmt.Sprintf(
			"temporary table storage exceeded the limit of %d bytes", limit,
		))
	}
	return nil
}

func (db *engine) checkPrivileges(stmt statement) error {
	// Fast path: a session that allows ALL DDL (persistent + temp) and grants every privilege pays
	// nothing. Both gates must be on, since temp DDL now has its own gate (§5).
	if db.session.allowDDL && db.session.allowTempDDL && db.session.privileges.IsPermissive() {
		return nil
	}
	var req privReq
	collectStmtPrivs(stmt, &req)
	if req.isDDL {
		// DDL is gated by the kind of relation it targets (temp-tables.md §5): a session-local temp
		// table by allowTempDDL, everything else (persistent) by allowDDL. A CREATE TABLE is classified
		// statically; the rest by resolving the name — a DROP TABLE / CREATE INDEX by its target table,
		// a DROP INDEX by the index (preclude-overlaps keeps a name in one scope).
		var allowed bool
		switch {
		case req.isTempDDL ||
			(stmt.DropTable != nil && db.anyTempTable(stmt.DropTable.Names)) ||
			(stmt.CreateIndex != nil && db.isTempTable(stmt.CreateIndex.Table)) ||
			(stmt.DropIndex != nil && db.isTempIndex(stmt.DropIndex.Name)) ||
			(stmt.DropSequence != nil && db.anyTempSequence(stmt.DropSequence.Names)) ||
			(stmt.AlterSequence != nil && db.isTempSequence(stmt.AlterSequence.Name)):
			allowed = db.session.allowTempDDL
		default:
			allowed = db.session.allowDDL
		}
		if !allowed {
			return newError(InsufficientPrivilege, "permission denied: DDL is not permitted in this session")
		}
	}
	snap := db.readSnap()
	for _, t := range req.tables {
		key := strings.ToLower(t.name)
		// Only a name that resolves to an existing catalog table is privilege-checked; a missing one is
		// left to raise 42P01 in execution (existence before authorization). A built-in catalog relation
		// (jed_tables / jed_columns) is gated exactly like a user table — per-table SELECT on its own
		// name under the session envelope, no special case (introspection.md §5) — so an explicit-grant
		// session sees the schema only if the host granted it.
		exists := isCatalogRelName(key)
		if !exists {
			_, exists = snap.table(key)
		}
		if exists && !db.session.privileges.AllowsTable(key, t.priv) {
			return newError(InsufficientPrivilege, "permission denied for table "+key)
		}
	}
	for _, fn := range req.functions {
		key := strings.ToLower(fn)
		if !db.session.privileges.AllowsFunction(key) {
			return newError(InsufficientPrivilege, "permission denied for function "+key)
		}
	}
	return nil
}

// gateReadLanes runs the admission gates that the lazy read lanes (tryScanQuery / tryDeferredQuery)
// would otherwise skip. Those gates live on the materialized dispatchStmt / ExecuteStmtParams path, but
// a SELECT served by a streaming/deferred cursor never reaches it — so before Exec/Query became the one
// total seam, a read through the ergonomic Query path bypassed authorization entirely (a §13 hole).
// Enforcing them here makes Query a total AND safe seam: a read inside a failed block is 25P02, a
// lifetime-exhausted session is 54P02, and a restricted read is 42501 — whichever lane ends up serving
// it. The caller applies this only to reads (transaction control must still work in a failed block, and
// a write keeps its existing gating inside dispatch); the three checks are pure, so a read that falls
// through to the materialized path re-running them is harmless (identical result).
func (db *engine) gateReadLanes(stmt statement) error {
	if db.session.tx != nil && db.session.tx.failed {
		return newError(InFailedSqlTransaction,
			"current transaction is aborted, commands ignored until end of transaction block")
	}
	if err := db.checkLifetimeAdmission(); err != nil {
		return err
	}
	return db.checkPrivileges(stmt)
}

// failOpenBlock puts an open, failable transaction block into the aborted state (tx.failed). A no-op
// outside a block, and idempotent. This is the block-abort that a lazy read lane bypasses: the
// materialized ExecuteStmtParams poisons in its block branch, but a SELECT served by a streaming /
// deferred cursor never reaches it (transactions.md §6). PostgreSQL aborts a block on ANY statement
// error, so a failing read has to poison here — otherwise the next statement wrongly succeeds instead
// of 25P02. Only reads reach these paths (transaction control and writes go to dispatch, which
// self-poisons with the right nuance — a nested BEGIN's 25001 must NOT abort).
func (db *engine) failOpenBlock() {
	if db.session.tx != nil {
		db.session.tx.failed = true
	}
}

// poisonOnLaneErr aborts an open block when a lazy read lane returns an error at open time (a missing
// table, a denied read, a plan-time trap) — the counterpart to gateReadLanes: gateReadLanes enforces
// the admission gates the lanes skip, poisonOnLaneErr the block-abort they skip. Wraps a lane error
// return; the returned err is unchanged.
func (db *engine) poisonOnLaneErr(err error) error {
	if err != nil {
		db.failOpenBlock()
	}
	return err
}

// attachBlockPoison hooks a lazy-lane cursor so a DRAIN-time read error inside an open block aborts it
// too. A streaming (S3) / deferred (S7) cursor's error surfaces during the caller's Next(), after
// queryStmt has returned, so the open-time poisonOnLaneErr can't see it — the cursor's onErr hook does
// (executor's blocking buffered read already surfaces its error at open, poisoned above). A no-op when
// no block is open; the hook re-checks the block at error time (a read may outlive the block it began
// in — poisoning an already-ended block is harmless).
func (db *engine) attachBlockPoison(rows *Rows) *Rows {
	if db.session.tx != nil {
		rows.attachErrHook(func(error) { db.failOpenBlock() })
	}
	return rows
}

// collectStmtPrivs collects the privilege requirements of stmt (spec/design/session.md §5.3).
// Transaction control carries none (handled before dispatch); DDL just sets isDDL.
func collectStmtPrivs(stmt statement, req *privReq) {
	locals := map[string]bool{}
	switch {
	case stmt.CreateTable != nil:
		req.isDDL = true
		// A temp table's DDL is gated by the temp-scoped split of allowDDL (temp-tables.md §5):
		// allowTempDDL for a session-local temp table.
		req.isTempDDL = stmt.CreateTable.Temp
	case stmt.DropTable != nil, stmt.CreateIndex != nil, stmt.DropIndex != nil,
		stmt.CreateType != nil, stmt.DropType != nil, stmt.CreateSequence != nil, stmt.DropSequence != nil,
		stmt.AlterSequence != nil:
		req.isDDL = true
	case stmt.Insert != nil:
		collectInsertPrivs(stmt.Insert, req, locals)
	case stmt.Select != nil:
		collectSelectPrivs(stmt.Select, req, locals)
	case stmt.SetOp != nil:
		collectSetopPrivs(stmt.SetOp, req, locals)
	case stmt.With != nil:
		collectWithPrivs(stmt.With, req, locals)
	case stmt.Update != nil:
		collectUpdatePrivs(stmt.Update, req, locals)
	case stmt.Delete != nil:
		collectDeletePrivs(stmt.Delete, req, locals)
	case stmt.Explain != nil:
		// EXPLAIN requires the inner statement's privileges (EXPLAIN INSERT needs INSERT, matching
		// PG). Plain EXPLAIN never executes, but authorization is checked on the inner regardless.
		collectStmtPrivs(*stmt.Explain.Inner, req)
	}
}

func collectInsertPrivs(ins *insert, req *privReq, locals map[string]bool) {
	// The write target needs INSERT. A bare INSERT … VALUES reads nothing (the slots are literals /
	// params), so it needs only INSERT; an INSERT … SELECT source needs SELECT on its tables.
	req.needTable(ins.Table, PrivInsert)
	if ins.Select != nil {
		collectSelectPrivs(ins.Select, req, locals)
	}
	if ins.OnConflict != nil && ins.OnConflict.DoUpdate {
		for i := range ins.OnConflict.Assignments {
			collectExprPrivs(&ins.OnConflict.Assignments[i].Value, req, locals)
		}
		if ins.OnConflict.Filter != nil {
			collectExprPrivs(ins.OnConflict.Filter, req, locals)
		}
	}
	collectItemsPrivs(ins.Returning, req, locals)
}

func collectUpdatePrivs(upd *update, req *privReq, locals map[string]bool) {
	req.needTable(upd.Table, PrivUpdate)
	// SELECT on the target if it reads any column — a WHERE, a RETURNING, or a column/subquery-
	// referencing assignment RHS (a constant-only SET a = 1 with no WHERE/RETURNING reads nothing).
	reads := upd.Filter != nil || upd.Returning != nil
	for i := range upd.Assignments {
		if exprReadsColumns(&upd.Assignments[i].Value) {
			reads = true
		}
	}
	if reads {
		req.needTable(upd.Table, PrivSelect)
	}
	for i := range upd.Assignments {
		collectExprPrivs(&upd.Assignments[i].Value, req, locals)
	}
	if upd.Filter != nil {
		collectExprPrivs(upd.Filter, req, locals)
	}
	collectItemsPrivs(upd.Returning, req, locals)
}

func collectDeletePrivs(del *deleteStmt, req *privReq, locals map[string]bool) {
	req.needTable(del.Table, PrivDelete)
	// DELETE reads the target's columns through a WHERE or a RETURNING.
	if del.Filter != nil || del.Returning != nil {
		req.needTable(del.Table, PrivSelect)
	}
	if del.Filter != nil {
		collectExprPrivs(del.Filter, req, locals)
	}
	collectItemsPrivs(del.Returning, req, locals)
}

func collectQueryPrivs(qe *queryExpr, req *privReq, locals map[string]bool) {
	if qe.Select != nil {
		collectSelectPrivs(qe.Select, req, locals)
	} else if qe.SetOp != nil {
		collectSetopPrivs(qe.SetOp, req, locals)
	} else if qe.With != nil {
		// A nested WITH establishes its own CTE scope (spec/design/cte.md §7): the enclosing locals
		// are NOT inherited (an enclosing CTE name resolves to a base table inside, so it is
		// privilege-checked), and the nested CTE names shadow base tables only within this node.
		scope := map[string]bool{}
		for i := range qe.With.Ctes {
			collectCteBodyPrivs(&qe.With.Ctes[i].Body, req, scope)
			scope[strings.ToLower(qe.With.Ctes[i].Name)] = true
		}
		collectQueryPrivs(qe.With.Body, req, scope)
	}
}

func collectSetopPrivs(so *setOp, req *privReq, locals map[string]bool) {
	collectQueryPrivs(&so.Lhs, req, locals)
	collectQueryPrivs(&so.Rhs, req, locals)
}

func collectWithPrivs(wq *withQuery, req *privReq, locals map[string]bool) {
	// A CTE name shadows a base table inside the WITH (a FROM <cte> is not a catalog object), so it is
	// added to the local scope and never privilege-checked. Forward-only visibility: each CTE body
	// sees the CTE names declared before it. A data-modifying body / primary needs the write privilege
	// on its target table (writable-cte.md).
	scope := map[string]bool{}
	for k := range locals {
		scope[k] = true
	}
	for i := range wq.Ctes {
		collectCteBodyPrivs(&wq.Ctes[i].Body, req, scope)
		scope[strings.ToLower(wq.Ctes[i].Name)] = true
	}
	collectCteBodyPrivs(&wq.Body, req, scope)
}

// collectCteBodyPrivs collects the privilege requirements of a cte_body — a query, or a
// data-modifying statement (spec/design/writable-cte.md) which needs the write privilege on its
// target.
func collectCteBodyPrivs(body *cteBody, req *privReq, locals map[string]bool) {
	switch {
	case body.Query != nil:
		collectQueryPrivs(body.Query, req, locals)
	case body.Insert != nil:
		collectInsertPrivs(body.Insert, req, locals)
	case body.Update != nil:
		collectUpdatePrivs(body.Update, req, locals)
	default:
		collectDeletePrivs(body.Delete, req, locals)
	}
}

func collectSelectPrivs(s *selectStmt, req *privReq, locals map[string]bool) {
	if s.From != nil {
		collectTableRefPrivs(s.From, req, locals)
	}
	for i := range s.Joins {
		collectTableRefPrivs(&s.Joins[i].Table, req, locals)
		if s.Joins[i].On != nil {
			collectExprPrivs(s.Joins[i].On, req, locals)
		}
	}
	for i := range s.Items.Items {
		collectExprPrivs(&s.Items.Items[i].Expr, req, locals)
	}
	if s.Filter != nil {
		collectExprPrivs(s.Filter, req, locals)
	}
	for i := range s.GroupBy {
		s.GroupBy[i].forEachExpr(func(e *exprNode) {
			collectExprPrivs(e, req, locals)
		})
	}
	if s.Having != nil {
		collectExprPrivs(s.Having, req, locals)
	}
}

func collectTableRefPrivs(t *tableRef, req *privReq, locals map[string]bool) {
	switch {
	case t.IsFunc:
		// A set-returning function used as a row source — EXECUTE on the function; its args are exprs.
		req.needFunction(t.Name)
		for _, a := range t.Args {
			collectExprPrivs(a, req, locals)
		}
	case t.Subquery != nil:
		collectQueryPrivs(t.Subquery, req, locals)
	case t.Values != nil:
		for _, row := range t.Values {
			for _, e := range row {
				collectExprPrivs(e, req, locals)
			}
		}
	default:
		// A base-table reference (not a CTE / derived-table label) — needs SELECT.
		if !locals[strings.ToLower(t.Name)] {
			req.needTable(t.Name, PrivSelect)
		}
	}
}

func collectItemsPrivs(items *selectItems, req *privReq, locals map[string]bool) {
	if items == nil {
		return
	}
	for i := range items.Items {
		collectExprPrivs(&items.Items[i].Expr, req, locals)
	}
}

// collectExprPrivs is exhaustive over Expr (mirroring exprCallsSeqMutator): collect every named
// function call (EXECUTE) and walk every subquery (its tables need SELECT).
func collectExprPrivs(e *exprNode, req *privReq, locals map[string]bool) {
	switch e.Kind {
	case exprFuncCall:
		req.needFunction(e.FuncCall.Name)
		for _, a := range e.FuncCall.Args {
			collectExprPrivs(a, req, locals)
		}
	case exprColumn, exprQualifiedColumn, exprLiteral, exprTypedLiteral, exprParam:
		// leaf — nothing to collect
	case exprRow, exprArray:
		for i := range e.RowItems {
			collectExprPrivs(&e.RowItems[i], req, locals)
		}
	case exprFieldAccess, exprFieldStar:
		collectExprPrivs(e.Base, req, locals)
	case exprQualifiedStar:
		// `t.*` names a relation already in FROM — its SELECT privilege is required by the FROM
		// clause itself, so the star adds no new function/table privilege here.
	case exprSubscript:
		collectExprPrivs(e.Base, req, locals)
		for i := range e.Subscripts {
			sub := &e.Subscripts[i]
			if sub.Index != nil {
				collectExprPrivs(sub.Index, req, locals)
			}
			if sub.Lower != nil {
				collectExprPrivs(sub.Lower, req, locals)
			}
			if sub.Upper != nil {
				collectExprPrivs(sub.Upper, req, locals)
			}
		}
	case exprCast:
		collectExprPrivs(&e.Cast.Inner, req, locals)
	case exprExtract:
		collectExprPrivs(&e.Extract.Source, req, locals)
	case exprCollate:
		collectExprPrivs(&e.Collate.Inner, req, locals)
	case exprUnary:
		collectExprPrivs(&e.Unary.Operand, req, locals)
	case exprIsNull:
		collectExprPrivs(&e.IsNullOf.Operand, req, locals)
	case exprIsJson:
		collectExprPrivs(&e.IsJsonOf.Operand, req, locals)
	case exprJsonCtor:
		collectExprPrivs(&e.JsonCtorOf.Operand, req, locals)
	case exprJsonExists:
		collectExprPrivs(&e.JsonExists.Ctx, req, locals)
		collectExprPrivs(&e.JsonExists.Path, req, locals)
	case exprJsonValue:
		collectExprPrivs(&e.JsonValue.Ctx, req, locals)
		collectExprPrivs(&e.JsonValue.Path, req, locals)
	case exprJsonQuery:
		collectExprPrivs(&e.JsonQuery.Ctx, req, locals)
		collectExprPrivs(&e.JsonQuery.Path, req, locals)
	case exprBinary:
		collectExprPrivs(&e.Binary.Lhs, req, locals)
		collectExprPrivs(&e.Binary.Rhs, req, locals)
	case exprIsDistinct:
		collectExprPrivs(&e.IsDistinct.Lhs, req, locals)
		collectExprPrivs(&e.IsDistinct.Rhs, req, locals)
	case exprLike:
		collectExprPrivs(&e.Like.Lhs, req, locals)
		collectExprPrivs(&e.Like.Rhs, req, locals)
	case exprRegex:
		collectExprPrivs(&e.Regex.Lhs, req, locals)
		collectExprPrivs(&e.Regex.Rhs, req, locals)
	case exprIn:
		collectExprPrivs(&e.In.Lhs, req, locals)
		for i := range e.In.List {
			collectExprPrivs(&e.In.List[i], req, locals)
		}
	case exprBetween:
		collectExprPrivs(&e.Between.Lhs, req, locals)
		collectExprPrivs(&e.Between.Lo, req, locals)
		collectExprPrivs(&e.Between.Hi, req, locals)
	case exprCase:
		if e.Case.Operand != nil {
			collectExprPrivs(e.Case.Operand, req, locals)
		}
		for i := range e.Case.Whens {
			collectExprPrivs(&e.Case.Whens[i].Cond, req, locals)
			collectExprPrivs(&e.Case.Whens[i].Result, req, locals)
		}
		if e.Case.Els != nil {
			collectExprPrivs(e.Case.Els, req, locals)
		}
	case exprScalarSubquery, exprExists:
		collectQueryPrivs(e.Subquery, req, locals)
	case exprInSubquery:
		collectExprPrivs(&e.InSubquery.Lhs, req, locals)
		collectQueryPrivs(&e.InSubquery.Query, req, locals)
	case exprQuantifiedSubquery:
		collectExprPrivs(&e.QuantifiedSubquery.Lhs, req, locals)
		collectQueryPrivs(&e.QuantifiedSubquery.Query, req, locals)
	case exprQuantified:
		collectExprPrivs(&e.Quantified.Lhs, req, locals)
		collectExprPrivs(&e.Quantified.Array, req, locals)
	}
}

// exprReadsColumns reports whether e reads a stored column or a subquery's rows — the trigger for an
// UPDATE's SELECT requirement on its target (spec/design/session.md §5.3). A column reference or any
// subquery counts; a pure constant / parameter expression does not. Exhaustive over Expr.
func exprReadsColumns(e *exprNode) bool {
	switch e.Kind {
	case exprColumn, exprQualifiedColumn:
		return true
	case exprScalarSubquery, exprExists, exprInSubquery, exprQuantifiedSubquery:
		return true
	case exprLiteral, exprTypedLiteral, exprParam:
		return false
	case exprRow, exprArray:
		for i := range e.RowItems {
			if exprReadsColumns(&e.RowItems[i]) {
				return true
			}
		}
		return false
	case exprFieldAccess, exprFieldStar:
		return exprReadsColumns(e.Base)
	case exprQualifiedStar:
		return true // `t.*` reads the relation's columns (e.g. `RETURNING t.*`)

	case exprSubscript:
		if exprReadsColumns(e.Base) {
			return true
		}
		for i := range e.Subscripts {
			sub := &e.Subscripts[i]
			if sub.Index != nil && exprReadsColumns(sub.Index) {
				return true
			}
			if sub.Lower != nil && exprReadsColumns(sub.Lower) {
				return true
			}
			if sub.Upper != nil && exprReadsColumns(sub.Upper) {
				return true
			}
		}
		return false
	case exprCast:
		return exprReadsColumns(&e.Cast.Inner)
	case exprExtract:
		return exprReadsColumns(&e.Extract.Source)
	case exprCollate:
		return exprReadsColumns(&e.Collate.Inner)
	case exprUnary:
		return exprReadsColumns(&e.Unary.Operand)
	case exprIsNull:
		return exprReadsColumns(&e.IsNullOf.Operand)
	case exprIsJson:
		return exprReadsColumns(&e.IsJsonOf.Operand)
	case exprJsonCtor:
		return exprReadsColumns(&e.JsonCtorOf.Operand)
	case exprJsonExists:
		return exprReadsColumns(&e.JsonExists.Ctx) || exprReadsColumns(&e.JsonExists.Path)
	case exprJsonValue:
		return exprReadsColumns(&e.JsonValue.Ctx) || exprReadsColumns(&e.JsonValue.Path)
	case exprJsonQuery:
		return exprReadsColumns(&e.JsonQuery.Ctx) || exprReadsColumns(&e.JsonQuery.Path)
	case exprFuncCall:
		for _, a := range e.FuncCall.Args {
			if exprReadsColumns(a) {
				return true
			}
		}
		return false
	case exprBinary:
		return exprReadsColumns(&e.Binary.Lhs) || exprReadsColumns(&e.Binary.Rhs)
	case exprIsDistinct:
		return exprReadsColumns(&e.IsDistinct.Lhs) || exprReadsColumns(&e.IsDistinct.Rhs)
	case exprLike:
		return exprReadsColumns(&e.Like.Lhs) || exprReadsColumns(&e.Like.Rhs)
	case exprRegex:
		return exprReadsColumns(&e.Regex.Lhs) || exprReadsColumns(&e.Regex.Rhs)
	case exprIn:
		if exprReadsColumns(&e.In.Lhs) {
			return true
		}
		for i := range e.In.List {
			if exprReadsColumns(&e.In.List[i]) {
				return true
			}
		}
		return false
	case exprBetween:
		return exprReadsColumns(&e.Between.Lhs) || exprReadsColumns(&e.Between.Lo) || exprReadsColumns(&e.Between.Hi)
	case exprCase:
		if e.Case.Operand != nil && exprReadsColumns(e.Case.Operand) {
			return true
		}
		for i := range e.Case.Whens {
			if exprReadsColumns(&e.Case.Whens[i].Cond) || exprReadsColumns(&e.Case.Whens[i].Result) {
				return true
			}
		}
		if e.Case.Els != nil && exprReadsColumns(e.Case.Els) {
			return true
		}
		return false
	case exprQuantified:
		return exprReadsColumns(&e.Quantified.Lhs) || exprReadsColumns(&e.Quantified.Array)
	default:
		return false
	}
}

// stmtKind is a short label for a statement kind, for the 25006 read-only-violation message (the
// message text is informational — never matched; spec/design/conformance.md §2).
func stmtKind(stmt statement) string {
	switch {
	case stmt.CreateTable != nil:
		return "CREATE TABLE"
	case stmt.DropTable != nil:
		return "DROP TABLE"
	case stmt.CreateIndex != nil:
		return "CREATE INDEX"
	case stmt.DropIndex != nil:
		return "DROP INDEX"
	case stmt.CreateType != nil:
		return "CREATE TYPE"
	case stmt.DropType != nil:
		return "DROP TYPE"
	case stmt.CreateSequence != nil:
		return "CREATE SEQUENCE"
	case stmt.AlterSequence != nil:
		return "ALTER SEQUENCE"
	case stmt.DropSequence != nil:
		return "DROP SEQUENCE"
	case stmt.Insert != nil:
		return "INSERT"
	case stmt.Update != nil:
		return "UPDATE"
	case stmt.Delete != nil:
		return "DELETE"
	case stmt.Explain != nil:
		return "EXPLAIN"
	default:
		return "statement"
	}
}

// dispatchStmt routes one parsed statement to its executor. The autocommit transaction handling
// (capture / durable commit / rollback-on-error) lives in ExecuteStmtParams.
func (db *engine) dispatchStmt(stmt statement, params []Value) (outcome, error) {
	// Lifetime budget admission (spec/design/session.md §5.4): once the session's cumulative cost has
	// reached lifetime_max_cost, every further statement is rejected 54P02 BEFORE it can accrue —
	// checked ahead of privileges/existence, so an exhausted session runs nothing. A no-op when the
	// budget is unlimited (the default). Transaction control (BEGIN/COMMIT/ROLLBACK) never reaches
	// dispatch (handled earlier), so an exhausted session can still close out an open block.
	if err := db.checkLifetimeAdmission(); err != nil {
		return outcome{}, err
	}
	// Authorization (spec/design/session.md §5.3): enforce the session's privilege envelope before the
	// statement runs — DDL gated by allowDDL, DML by per-table/per-function privileges, all 42501.
	// Skipped on a fully-permissive session (the default), so the common path pays nothing. The
	// physical access-mode gate (25006) is checked earlier in ExecuteStmtParams, so it wins when both
	// apply.
	if err := db.checkPrivileges(stmt); err != nil {
		return outcome{}, err
	}
	out, err := db.dispatchStmtBody(stmt, params)
	// Keep each GiST index's resident R-tree current: after a statement that mutated the main image,
	// rebuild it from the (now-updated) leaf store so the next read descends a fresh tree (gist.md
	// §3/§4.1). A no-op for reads / temp-only writes (mainDirty unset).
	if err == nil {
		if herr := db.rebuildMainGistTreesIfDirty(); herr != nil {
			return outcome{}, herr
		}
	}
	return out, err
}

// rebuildMainGistTreesIfDirty refreshes the main working snapshot's resident GiST trees iff the
// current statement mutated the main image (gist.md §3/§4.1). Gated on mainDirty (set by the
// statement's own working() writes): a read or a temp-only write leaves it unset, so this is a no-op
// and never forces a spurious main-image persist (the temp-no-file-write invariant). GiST on a temp
// table is 0A000 this slice, so only the main working snapshot is refreshed.
func (db *engine) rebuildMainGistTreesIfDirty() error {
	if db.session.tx != nil && db.session.tx.mainDirty {
		return db.session.tx.working.rebuildGistTrees()
	}
	return nil
}

func (db *engine) dispatchStmtBody(stmt statement, params []Value) (outcome, error) {
	switch {
	case stmt.CreateTable != nil:
		if err := rejectParamsForDDL(params); err != nil {
			return outcome{}, err
		}
		return db.executeCreateTable(stmt.CreateTable)
	case stmt.DropTable != nil:
		if err := rejectParamsForDDL(params); err != nil {
			return outcome{}, err
		}
		return db.executeDropTable(stmt.DropTable)
	case stmt.CreateIndex != nil:
		if err := rejectParamsForDDL(params); err != nil {
			return outcome{}, err
		}
		return db.executeCreateIndex(stmt.CreateIndex)
	case stmt.DropIndex != nil:
		if err := rejectParamsForDDL(params); err != nil {
			return outcome{}, err
		}
		return db.executeDropIndex(stmt.DropIndex)
	case stmt.CreateType != nil:
		if err := rejectParamsForDDL(params); err != nil {
			return outcome{}, err
		}
		return db.executeCreateType(stmt.CreateType)
	case stmt.DropType != nil:
		if err := rejectParamsForDDL(params); err != nil {
			return outcome{}, err
		}
		return db.executeDropType(stmt.DropType)
	case stmt.CreateSequence != nil:
		if err := rejectParamsForDDL(params); err != nil {
			return outcome{}, err
		}
		return db.executeCreateSequence(stmt.CreateSequence)
	case stmt.AlterSequence != nil:
		if err := rejectParamsForDDL(params); err != nil {
			return outcome{}, err
		}
		return db.executeAlterSequence(stmt.AlterSequence)
	case stmt.DropSequence != nil:
		if err := rejectParamsForDDL(params); err != nil {
			return outcome{}, err
		}
		return db.executeDropSequence(stmt.DropSequence)
	case stmt.Insert != nil:
		return db.executeInsert(stmt.Insert, params, cteCtx{})
	case stmt.Select != nil:
		return db.executeSelect(stmt.Select, params)
	case stmt.SetOp != nil:
		return db.executeSetOp(stmt.SetOp, params)
	case stmt.With != nil:
		return db.executeWith(stmt.With, params)
	case stmt.Update != nil:
		return db.executeUpdate(stmt.Update, params, cteCtx{})
	case stmt.Delete != nil:
		return db.executeDelete(stmt.Delete, params, cteCtx{})
	case stmt.Explain != nil:
		return db.executeExplain(stmt.Explain, params)
	default:
		return outcome{}, newError(SyntaxError, "empty statement")
	}
}

// rejectParamsForDDL errors (42601) if bind parameters are supplied to a CREATE/DROP TABLE
// (which has no expressions to bind — spec/design/api.md §5).
func rejectParamsForDDL(params []Value) error {
	if len(params) > 0 {
		return newError(SyntaxError, "bind parameters are not allowed in a DDL statement")
	}
	return nil
}

// executeCreateTable analyzes and runs a CREATE TABLE: resolve each column's type
// name, enforce a single primary key across both forms (column-level and the
// table-level PRIMARY KEY (a, b, ...) constraint — which is implicitly NOT NULL per
// member), reject duplicate table and column names, then register the table.
// Constraint checks mirror PostgreSQL's order (oracle-probed, constraints.md §3):
// a second primary key traps 42P16 before its members resolve; members resolve
// left to right (unknown 42703, repeated 42701); then the jed narrowings — the
// declaration-order rule and the per-member key-type gate — trap 0A000.
func (db *engine) executeCreateTable(ct *createTable) (outcome, error) {
	// A session-local temporary table (spec/design/temp-tables.md) is built exactly like a persistent
	// one but registered into the session temp snapshot at the end (§2), so it makes zero file writes.
	// FOREIGN KEY on a temp table is deferred this slice (§8) — rejected HERE, before any persistent
	// parent resolves, so the error is a clean 0A000. The other temp narrowings (composite/collated
	// columns, serial/IDENTITY) are checked just before registration, once the columns are built.
	//
	// Resolve the optional database qualifier (attached-databases.md §3, Slice 1b): `main`/`temp` fold
	// into the implicit scope (main = bare persistent, temp = TEMP); a host-attached name routes the new
	// table INTO that attachment's working snapshot (§6). TEMP with an explicit database is
	// contradictory unless the database IS `temp` (42601).
	targetTemp := ct.Temp
	attachName := ""
	if ct.DB != nil {
		switch strings.ToLower(*ct.DB) {
		case "main":
			if ct.Temp {
				return outcome{}, newError(SyntaxError, `cannot create a TEMP table in database "main"`)
			}
		case "temp":
			targetTemp = true
		default:
			if ct.Temp {
				return outcome{}, newError(SyntaxError, "cannot create a TEMP table in an attached database")
			}
			attachName = strings.ToLower(*ct.DB)
			if db.attachReadSnap(attachName) == nil {
				return outcome{}, newError(UndefinedTable, `database "`+*ct.DB+`" is not attached`)
			}
			// A DDL write to a READ-ONLY attachment is 25006 before any work (attached-databases.md §4).
			if err := db.checkAttachmentWritable(ct.DB); err != nil {
				return outcome{}, err
			}
		}
	}
	if targetTemp && len(ct.Excludes) > 0 {
		// An EXCLUDE constraint's backing GiST index would live on the temp snapshot — deferred with
		// the rest of the GiST-on-temp narrowing (spec/design/gist.md §11), a clean 0A000.
		return outcome{}, newError(FeatureNotSupported, "an EXCLUDE constraint on a temporary table is not yet supported")
	}
	if targetTemp && len(ct.ForeignKeys) > 0 {
		return outcome{}, newError(FeatureNotSupported, "FOREIGN KEY on a temporary table is not yet supported")
	}
	// Deferred narrowings on an attached-database table this slice (attached-databases.md §8), each a
	// clean 0A000 before any column work: FOREIGN KEY and EXCLUDE (their probe/backing structures would
	// need cross-scope catalog access this slice does not thread). Serial/IDENTITY and composite/collated
	// columns are checked just before registration, once the columns are built (as for temp).
	if attachName != "" {
		if len(ct.ForeignKeys) > 0 {
			return outcome{}, newError(FeatureNotSupported, "FOREIGN KEY on an attached-database table is not supported yet")
		}
		if len(ct.Excludes) > 0 {
			return outcome{}, newError(FeatureNotSupported, "an EXCLUDE constraint on an attached-database table is not supported yet")
		}
	}
	if err := checkReservedName("table", ct.Name); err != nil {
		return outcome{}, err
	}
	// The relation namespace is shared between tables and indexes (indexes.md §2), so a CREATE TABLE
	// colliding with either kind is the same 42P07 — PG's "relation" word. For a bare/main/temp target
	// relationExists is temp-aware (a temp name collides with temp + persistent alike — temp-tables.md
	// §3); an attachment target checks its OWN snapshot's namespace (each attached database is
	// independent, §3).
	if attachName != "" {
		as := db.attachReadSnap(attachName)
		if _, ok := as.table(ct.Name); ok {
			return outcome{}, newError(DuplicateTable, "relation already exists: "+ct.Name)
		}
		if _, _, ok := as.findIndex(ct.Name); ok {
			return outcome{}, newError(DuplicateTable, "relation already exists: "+ct.Name)
		}
	} else if db.relationExists(ct.Name) {
		return outcome{}, newError(DuplicateTable, "relation already exists: "+ct.Name)
	}

	columns := make([]catColumn, 0, len(ct.Columns))
	// pk is the primary-key member ordinals in KEY order (constraints.md §3): the
	// column-level form is the one-member case; the table-level list below records its
	// own order.
	var pk []int
	pkSeen := false
	// The OWNED sequences a serial column desugars to (spec/design/sequences.md §12), collected
	// during the column walk and staged into the working snapshot only after the whole CREATE TABLE
	// validates — so a later failure (e.g. a bad CHECK) discards them with the statement.
	var pendingSerials []*sequenceDef
	for _, def := range ct.Columns {
		for _, c := range columns {
			if strings.EqualFold(c.Name, def.Name) {
				return outcome{}, newError(DuplicateColumn, "duplicate column name: "+def.Name)
			}
		}
		// Resolve the column type: a built-in scalar, or a user-defined composite referenced by name
		// (spec/design/composite.md §3). An unknown name is 42704. A composite column carries no
		// typmod (the composite's fields carry their own); a type modifier written on a composite
		// column is rejected (0A000). A composite column is storable (S3) but never keyable — the PK
		// gate below rejects it 0A000 (§6).
		// A serial / bigserial / smallserial pseudo-type (spec/design/sequences.md §12): CREATE TABLE
		// sugar for an integer column that is NOT NULL with a DEFAULT nextval(...) backed by a
		// newly-created OWNED sequence. Here we only resolve the underlying integer type; the
		// desugaring (the owned sequence + default + NOT NULL force) happens below. serial[] is NOT a
		// serial column (it falls to the array branch as an unknown element type — §12.1).
		serialKind, isSerial := serialPseudoType(def.TypeName)
		var colType dataType
		var decimal *decimalTypmod
		var varcharLen *uint32
		isComposite := false
		isArray := false
		isRange := false
		if isSerial {
			// A serial column takes no typmod (serial(5) is 42601) and no [] (the array branch).
			if def.TypeMod != nil {
				return outcome{}, newError(SyntaxError,
					"type modifier is not allowed for type "+def.TypeName)
			}
			colType = scalarT(serialKind)
		} else if base, ok := strings.CutSuffix(def.TypeName, "[]"); ok {
			// An array column (spec/design/array.md §3). The element type is a scalar or a
			// previously-defined composite (array-of-composite, §12 AC1 — element_type_code 14 +
			// name); a nested-array element and an array typmod (numeric(p,s)[]) stay deferred (0A000).
			if def.TypeMod != nil {
				return outcome{}, newError(FeatureNotSupported,
					"a type modifier on an array type is not supported yet")
			}
			if elemScalar, scalarOK := scalarTypeFromName(base); scalarOK {
				colType = arrayT(scalarT(elemScalar))
			} else if ctype := db.readSnap().compositeType(base); ctype != nil {
				colType = arrayT(compositeT(ctype.Name))
			} else {
				return outcome{}, newError(UndefinedObject, "type does not exist: "+base)
			}
			isArray = true
		} else if rdesc, ok := rangeByName(def.TypeName); ok {
			// A range column (spec/design/ranges.md §3): structural like array, the element carried
			// inline. A range takes no typmod (numrange(10,2) is not a thing — the element is the
			// unconstrained subtype), so a type modifier is rejected.
			if def.TypeMod != nil {
				return outcome{}, newError(FeatureNotSupported,
					"a type modifier on a range type is not supported")
			}
			colType = rangeT(scalarT(elementScalar(rdesc)))
			isRange = true
		} else if _, ok := scalarTypeFromName(def.TypeName); ok {
			ty, d, vl, err := resolveTypeAndTypmod(def.TypeName, def.TypeMod)
			if err != nil {
				return outcome{}, err
			}
			// jsonpath is literal-only this slice (P1a) — a jsonpath COLUMN is 0A000, like a J0-stage
			// json column (a storable jsonpath is a follow-on).
			if ty == scalarJsonPath {
				return outcome{}, newError(FeatureNotSupported, "a jsonpath column is not supported yet")
			}
			colType = scalarT(ty)
			decimal = d
			varcharLen = vl
		} else if ctype := db.readSnap().compositeType(def.TypeName); ctype != nil {
			if def.TypeMod != nil {
				return outcome{}, newError(FeatureNotSupported,
					"a type modifier is not supported for composite type "+def.TypeName)
			}
			colType = compositeT(ctype.Name)
			isComposite = true
		} else {
			return outcome{}, newError(UndefinedObject, "type does not exist: "+def.TypeName)
		}
		if def.PrimaryKey {
			// The key-encodable scalars may be a PRIMARY KEY. The fixed-width ones — integers,
			// boolean (bool-byte §2.9), uuid (uuid-raw16 §2.7), timestamp/timestamptz (i64
			// int-be-signflip, timestamp.md §6), date (i32, date.md §5), interval (interval-span-i128,
			// the 16-byte span key §2.10) — plus the variable-width text/bytea (…-terminated-escape
			// §2.4/§2.6) and decimal (decimal-order-preserving §2.5), all self-delimiting so they
			// compose in composite keys / index suffixes — plus the range container (range-bounds
			// §2.11, the first container key) and the array container (array-elements-terminated
			// §2.14, the second container key — keyable when its element is a key-encodable scalar,
			// isArrayKeyable, INCLUDING a float element since the §2.8 lift) — plus float itself
			// (float-order-preserving §2.8, the last scalar to become keyable). Still 0A000: only a
			// composite-element array and the recursive composite container.
			if isComposite || (isArray && !isArrayKeyable(colType)) {
				// A composite PRIMARY KEY (composite.md §6) or a non-keyable array PRIMARY KEY (a
				// composite element) is rejected 0A000. colType.CanonicalName() gives the
				// canonical type name (e.g. addr[], even when declared with an alias).
				return outcome{}, newError(FeatureNotSupported,
					"a "+colType.CanonicalName()+" primary key is not supported yet")
			}
			// A range / keyable array is a container key (encoding.md §2.11/§2.14); every other
			// keyable column is a scalar, gated here.
			if !isRange && !isArray {
				if ty := colType.Scalar; !ty.IsInteger() && !ty.IsBool() && !ty.IsText() && !ty.IsBytea() && !ty.IsDecimal() && !ty.IsUuid() && !ty.IsTimestamp() && !ty.IsTimestamptz() && !ty.IsDate() && !ty.IsInterval() && !ty.IsFloat() {
					return outcome{}, newError(FeatureNotSupported,
						"a "+ty.CanonicalName()+" primary key is not supported yet")
				}
			}
			if pkSeen {
				return outcome{}, newError(InvalidTableDefinition,
					"multiple primary keys for table "+ct.Name+" are not allowed")
			}
			pkSeen = true
			pk = append(pk, len(columns)) // this column's ordinal (appended below)
		}
		// Classify the DEFAULT by syntactic form (constraints.md §2). A bad default fails at
		// CREATE TABLE either way; NOT NULL is NOT enforced here (notNull=false), so a DEFAULT
		// NULL on a NOT NULL column is accepted and traps 23502 only when applied.
		//   - a bare literal is pre-evaluated + type-coerced to a constant value (the fast-path:
		//     out of range 22003, cross-family 42804, decimal rounded to typmod);
		//   - any other expression is validated (structural pre-walk, then resolved against an
		//     EMPTY scope — a default may not reference a column — then its result type is
		//     checked assignable to the column, 42804) and stored as text for per-row eval.
		var defaultVal *Value
		var defaultExpr *defaultExprDef
		var identityKind *identityKind
		// A serial pseudo-type OR a GENERATED … AS IDENTITY constraint both desugar to an
		// auto-numbered column: an OWNED sequence + a synthesized DEFAULT nextval(...) + NOT NULL
		// (sequences.md §12/§13). Identity additionally records ALWAYS/BY DEFAULT and gates the
		// column type to i16/i32/i64.
		if isSerial || def.Identity != nil {
			// IDENTITY type gate: the declared column type must be smallint/integer/bigint
			// (sequences.md §13.1). serial's type is the pseudo-type (always integer), so this only
			// bites an identity column written on a non-integer type.
			if def.Identity != nil && !colType.IsInteger() {
				return outcome{}, newError(InvalidParameterValue,
					"identity column type must be smallint, integer, or bigint")
			}
			// Conflicts (42601, sequences.md §13.2). An explicit DEFAULT — or a serial type, itself a
			// synthesized default — alongside IDENTITY is "both default and identity"; a serial column
			// with its own explicit DEFAULT is "multiple default values" (the S3 message, unchanged).
			if def.Identity != nil && (def.Default != nil || isSerial) {
				return outcome{}, newError(SyntaxError, fmt.Sprintf(
					"both default and identity specified for column %s of table %s", def.Name, ct.Name,
				))
			}
			if isSerial && def.Default != nil {
				return outcome{}, newError(SyntaxError, fmt.Sprintf(
					"multiple default values specified for column %s of table %s", def.Name, ct.Name,
				))
			}
			// Create the OWNED sequence — a default ascending i64 for serial, or the IDENTITY column's
			// `( seq_options )` (defaulting the same way) — and synthesize the DEFAULT nextval(...)
			// expression default (format_version 8 mechanism).
			seqName := db.chooseSerialSeqName(ct.Name, def.Name, pendingSerials)
			owner := &seqOwner{Table: ct.Name, Column: uint16(len(columns))} // this column's ordinal
			var opts seqOptions
			if def.Identity != nil {
				opts = def.Identity.Options
			}
			// The owned sequence's data type follows the column (§14): serial → the pseudo-type,
			// identity → the column type. An explicit `AS` inside the identity `( … )` options
			// conflicts with that — 42601 (PG: "conflicting or redundant options"). serial carries no
			// parsed options, so this only fires for identity.
			if opts.DataType != "" {
				return outcome{}, newError(SyntaxError, "conflicting or redundant options")
			}
			seqScalar := serialKind
			if !isSerial {
				seqScalar = colType.ScalarTy()
			}
			seqDtype, ok := seqDataTypeForScalar(seqScalar)
			if !ok {
				// Unreachable: a serial / identity column is i16/i32/i64 (gated above).
				return outcome{}, newError(InvalidParameterValue,
					"serial / identity column is i16/i32/i64")
			}
			opts.DataType = seqDtype.PgName()
			seqDef, err := buildSequenceDef(seqName, opts, owner)
			if err != nil {
				return outcome{}, err
			}
			pendingSerials = append(pendingSerials, seqDef)
			// Render the synthetic default exactly as the parser would the equivalent
			// DEFAULT nextval('<seqName>') (space-joined tokens — the canonical expression-text form),
			// so the in-memory expr matches what reload re-parses. The seqName is a lowercased
			// identifier-derived name, so the quoting is always safe.
			exprText := "nextval ( '" + strings.ReplaceAll(seqName, "'", "''") + "' )"
			expr, err := parseExpression(exprText)
			if err != nil {
				return outcome{}, err
			}
			defaultExpr = &defaultExprDef{ExprText: exprText, Expr: expr}
			if def.Identity != nil {
				k := identityByDefault
				if def.Identity.Always {
					k = identityAlways
				}
				identityKind = &k
			}
		} else if isComposite || isArray || isRange {
			// A DEFAULT on a composite-, array-, or range-typed column is not supported this slice
			// (composite.md §12 / array.md §12 / ranges.md §8).
			if def.Default != nil {
				return outcome{}, newError(FeatureNotSupported,
					"a DEFAULT on a composite-, array-, or range-typed column is not supported yet")
			}
		} else if def.Default != nil {
			ty := colType.Scalar
			if def.Default.Expr.Kind == exprLiteral {
				dv, err := storeValue(literalToValue(*def.Default.Expr.Literal), ty, decimal, varcharLen, false, def.Name)
				if err != nil {
					return outcome{}, err
				}
				defaultVal = &dv
			} else {
				if err := rejectDefaultStructure(def.Default.Expr); err != nil {
					return outcome{}, err
				}
				_, rt, err := resolve(emptyScope(db), def.Default.Expr, &ty, &aggCtx{collecting: false}, &paramTypes{})
				if err != nil {
					return outcome{}, err
				}
				if !assignableTo(rt, ty) {
					return outcome{}, typeError(fmt.Sprintf(
						"column %s is of type %s but default expression is of type %s",
						def.Name, ty.CanonicalName(), rtName(rt),
					))
				}
				defaultExpr = &defaultExprDef{ExprText: def.Default.Text, Expr: def.Default.Expr}
			}
		}
		// The column's effective collation, frozen now (spec/design/collation.md §1). An explicit
		// COLLATE "name" is text-only (42804) and must name a loaded collation or C (42704); a text
		// column without a clause inherits the per-database default. A C effective collation stores
		// as "" (the fast path).
		collation := ""
		if def.Collation != "" {
			if !colType.IsText() {
				return outcome{}, typeError(fmt.Sprintf(
					"collations are not supported by type %s", colType.CanonicalName(),
				))
			}
			if _, err := resolveCollationName(db, def.Collation); err != nil {
				return outcome{}, err
			}
			if def.Collation != "C" {
				collation = def.Collation
			}
		} else if colType.IsText() {
			collation = db.readSnap().defaultCollation
		}
		columns = append(columns, catColumn{
			Name:       def.Name,
			Type:       colType,
			Decimal:    decimal,
			VarcharLen: varcharLen,
			PrimaryKey: def.PrimaryKey,
			// PRIMARY KEY ⇒ NOT NULL; a serial or IDENTITY column is NOT NULL too (sequences.md §12/§13).
			NotNull:     def.PrimaryKey || def.NotNull || isSerial || def.Identity != nil,
			Default:     defaultVal,
			DefaultExpr: defaultExpr,
			Identity:    identityKind,
			Collation:   collation,
		})
	}

	// Table-level PRIMARY KEY (a, b, ...) constraints (constraints.md §3). Check order
	// mirrors PostgreSQL (oracle-probed): a second primary key is 42P16 before its
	// members resolve; members resolve left to right (42703 unknown, 42701 repeated).
	// The LIST order is the KEY order — it may differ from declaration order (the v5
	// catalog persists the ordinal list; the old 0A000 narrowing is lifted). The
	// per-member key-type gate (0A000) remains.
	for _, pkList := range ct.TablePKs {
		if pkSeen {
			return outcome{}, newError(InvalidTableDefinition,
				"multiple primary keys for table "+ct.Name+" are not allowed")
		}
		pkSeen = true
		indices := make([]int, 0, len(pkList))
		for _, name := range pkList {
			idx := -1
			for i := range columns {
				if strings.EqualFold(columns[i].Name, name) {
					idx = i
					break
				}
			}
			if idx < 0 {
				return outcome{}, newError(UndefinedColumn,
					"column "+name+" named in key does not exist")
			}
			if slices.Contains(indices, idx) {
				return outcome{}, newError(DuplicateColumn,
					"column "+name+" appears twice in primary key constraint")
			}
			indices = append(indices, idx)
		}
		for _, i := range indices {
			ty := columns[i].Type
			if !ty.IsInteger() && !ty.IsBool() && !ty.IsText() && !ty.IsBytea() && !ty.IsDecimal() && !ty.IsUuid() && !ty.IsTimestamp() && !ty.IsTimestamptz() && !ty.IsDate() && !ty.IsInterval() && !ty.IsFloat() && !ty.IsRange() && !isArrayKeyable(ty) {
				return outcome{}, newError(FeatureNotSupported,
					"a "+ty.CanonicalName()+" primary key is not supported yet")
			}
			columns[i].PrimaryKey = true
			columns[i].NotNull = true // PRIMARY KEY ⇒ NOT NULL, per member
		}
		pk = indices
	}

	// UNIQUE constraints (constraints.md §5.1): resolve members in textual definition
	// order, AFTER the PRIMARY KEY constraints and BEFORE any CHECK validates (PG's
	// order, oracle-probed — transformIndexConstraint runs first). Each member must exist
	// (42703, PG's "named in key" wording), appear once (42701), and be of a key-encodable
	// type (0A000 — the same narrowing as a PK member / index key column; unlike a PK
	// member it stays nullable). Folding + naming happen LAST (after check naming),
	// mirroring PG's index_create-at-execution timing.
	type resolvedUnique struct {
		name string
		cols []int
	}
	runiques := make([]resolvedUnique, 0, len(ct.Uniques))
	for _, u := range ct.Uniques {
		indices := make([]int, 0, len(u.Columns))
		for _, cname := range u.Columns {
			idx := -1
			for i := range columns {
				if strings.EqualFold(columns[i].Name, cname) {
					idx = i
					break
				}
			}
			if idx < 0 {
				return outcome{}, newError(UndefinedColumn,
					"column "+cname+" named in key does not exist")
			}
			if slices.Contains(indices, idx) {
				return outcome{}, newError(DuplicateColumn,
					"column "+cname+" appears twice in unique constraint")
			}
			indices = append(indices, idx)
		}
		for _, i := range indices {
			ty := columns[i].Type
			if !ty.IsInteger() && !ty.IsBool() && !ty.IsText() && !ty.IsBytea() && !ty.IsDecimal() && !ty.IsUuid() && !ty.IsTimestamp() && !ty.IsTimestamptz() && !ty.IsDate() && !ty.IsInterval() && !ty.IsFloat() && !ty.IsRange() && !isArrayKeyable(ty) {
				return outcome{}, newError(FeatureNotSupported,
					"a "+ty.CanonicalName()+" unique constraint member is not supported yet")
			}
		}
		runiques = append(runiques, resolvedUnique{name: u.Name, cols: indices})
	}

	// CHECK constraints (constraints.md §4). All validation runs first, in textual
	// definition order, AFTER the PRIMARY KEY constraints resolved (PG's order,
	// oracle-probed); naming follows in a second pass, so a 42703 in a later check fires
	// before a 42710 between earlier ones. Resolution needs a catalog *Table, so build it
	// now (checks attach below, before putTable).
	table := &catTable{Name: ct.Name, Columns: columns, PK: pk}
	for i := range ct.Checks {
		def := &ct.Checks[i]
		// Structural rejections first (a single pre-walk — a documented micro-order
		// divergence from PG, which interleaves them with name/type resolution): subquery
		// 0A000, aggregate 42803, bind parameter 42P02 (constraints.md §4.1).
		if err := rejectCheckStructure(def.Expr); err != nil {
			return outcome{}, err
		}
		s := singleScope(db, table)
		_, ty, err := resolve(s, def.Expr, nil, &aggCtx{collecting: false}, &paramTypes{})
		if err != nil {
			return outcome{}, err
		}
		if ty.kind != rtBool && ty.kind != rtNull {
			return outcome{}, typeError("argument of CHECK must be boolean")
		}
	}
	// Naming (constraints.md §4.3): a single pass in textual order. An explicit name is
	// used as written; a derived name is built from the LOWERCASED table/column names —
	// `<table>_<col>_check` when the expression references exactly one distinct column,
	// else `<table>_check` — suffixed with the smallest positive integer that frees it. A
	// collision (case-insensitive, PG folds) is 42710; derived names never yield to a later
	// explicit one (oracle-probed).
	checks := make([]checkConstraint, 0, len(ct.Checks))
	nameTaken := func(name string) bool {
		for _, c := range checks {
			if strings.EqualFold(c.Name, name) {
				return true
			}
		}
		return false
	}
	for i := range ct.Checks {
		def := &ct.Checks[i]
		name := def.Name
		if name != "" {
			if nameTaken(name) {
				return outcome{}, newError(DuplicateObject,
					"constraint "+name+" for relation "+table.Name+" already exists")
			}
		} else {
			cols := checkReferencedColumns(def.Expr, columns)
			var base string
			if len(cols) == 1 {
				base = strings.ToLower(table.Name) + "_" + strings.ToLower(columns[cols[0]].Name) + "_check"
			} else {
				base = strings.ToLower(table.Name) + "_check"
			}
			name = base
			for suffix := 1; nameTaken(name); suffix++ {
				name = base + strconv.Itoa(suffix)
			}
		}
		checks = append(checks, checkConstraint{Name: name, ExprText: def.Text, Expr: def.Expr})
	}
	// Evaluation (and on-disk) order: ascending byte order of the lowercased name
	// (constraints.md §4.4 — PG evaluates checks sorted by name, oracle-probed).
	sort.SliceStable(checks, func(i, j int) bool {
		return strings.ToLower(checks[i].Name) < strings.ToLower(checks[j].Name)
	})
	table.Checks = checks

	// UNIQUE fold + naming (constraints.md §5.2/§5.3, PG-probed). Fold first: a
	// constraint whose member list equals the primary key's (same order) creates nothing;
	// identical lists fold into the first occurrence, the surviving name being the first
	// explicitly-named one's. Then each survivor names its backing index in textual order:
	// an explicit name checks the relation namespace (42P07 — existing relations, the
	// table being created, and the statement's earlier indexes) before the table's
	// constraint names (42710); a derived `<table>_<cols>_key` suffix-walks past BOTH
	// namespaces.
	var survivors []resolvedUnique
	for _, ru := range runiques {
		if slices.Equal(ru.cols, table.PK) {
			continue
		}
		folded := false
		for i := range survivors {
			if slices.Equal(survivors[i].cols, ru.cols) {
				if survivors[i].name == "" {
					survivors[i].name = ru.name
				}
				folded = true
				break
			}
		}
		if !folded {
			survivors = append(survivors, ru)
		}
	}
	relationTaken := func(n string) bool {
		if db.relationExists(n) || strings.EqualFold(table.Name, n) {
			return true
		}
		for _, ix := range table.Indexes {
			if strings.EqualFold(ix.Name, n) {
				return true
			}
		}
		return false
	}
	checkNameTaken := func(n string) bool {
		for _, c := range table.Checks {
			if strings.EqualFold(c.Name, n) {
				return true
			}
		}
		return false
	}
	for _, ru := range survivors {
		name := ru.name
		if name != "" {
			// A named UNIQUE constraint IS its backing index (constraints.md §5), so the
			// user-written name enters the relation namespace — reserved-prefix checked like
			// any relation name (introspection.md §4).
			if err := checkReservedName("constraint", name); err != nil {
				return outcome{}, err
			}
			if relationTaken(name) {
				return outcome{}, newError(DuplicateTable, "relation already exists: "+name)
			}
			if checkNameTaken(name) {
				return outcome{}, newError(DuplicateObject,
					"constraint "+name+" for relation "+table.Name+" already exists")
			}
		} else {
			base := strings.ToLower(table.Name)
			for _, i := range ru.cols {
				base += "_" + strings.ToLower(table.Columns[i].Name)
			}
			base += "_key"
			name = base
			for suffix := 1; relationTaken(name) || checkNameTaken(name); suffix++ {
				name = base + strconv.Itoa(suffix)
			}
		}
		// Insert in catalog (ascending lowercased-name) order — indexes.md §6.
		def := indexDef{Name: name, Columns: ru.cols, Unique: true, Kind: indexBtree}
		nameKey := strings.ToLower(name)
		pos := len(table.Indexes)
		for i, ix := range table.Indexes {
			if strings.ToLower(ix.Name) > nameKey {
				pos = i
				break
			}
		}
		table.Indexes = slices.Insert(table.Indexes, pos, def)
	}

	// FOREIGN KEY constraints (constraints.md §6). Resolved AFTER the PK / UNIQUE / CHECK
	// constraints (PG's order), each in textual definition order: resolve the local columns
	// (42703/42701) against this table; look up the parent (42P01, or the table itself for a
	// self-reference); resolve the referenced columns (default to the parent PK, 42704 if it
	// has none); check the arity (42830); name the constraint (explicit collision 42710, else
	// derive `<table>_<cols>_fkey` with a suffix walk through the constraint namespace); reject
	// the unsupported write-actions (0A000); require the referenced columns to be the parent PK
	// or a UNIQUE set (42830); and require same-type pairing (42804, stricter than PG). An FK
	// owns no B-tree — enforcement probes the parent at every write (§6.4/§6.5).
	resolvedFks := make([]foreignKey, 0, len(ct.ForeignKeys))
	for _, fk := range ct.ForeignKeys {
		// 1. Local (referencing) columns into this table.
		local := make([]int, 0, len(fk.Columns))
		for _, cname := range fk.Columns {
			idx := -1
			for i := range table.Columns {
				if strings.EqualFold(table.Columns[i].Name, cname) {
					idx = i
					break
				}
			}
			if idx < 0 {
				return outcome{}, newError(UndefinedColumn,
					"column "+cname+" named in key does not exist")
			}
			if slices.Contains(local, idx) {
				return outcome{}, newError(DuplicateColumn,
					"column "+cname+" appears twice in foreign key constraint")
			}
			local = append(local, idx)
		}
		// 2. Parent table — a self-reference resolves against the in-progress definition.
		selfRef := strings.EqualFold(fk.RefTable, table.Name)
		var parent *catTable
		if selfRef {
			parent = table
		} else {
			p, ok := db.Table(fk.RefTable)
			if !ok {
				return outcome{}, newError(UndefinedTable, "table does not exist: "+fk.RefTable)
			}
			parent = p
		}
		// 3. Referenced columns into the parent (default to the parent's primary key).
		var refs []int
		if fk.RefColumns == nil {
			if len(parent.PK) == 0 {
				// Omitting the referenced list defaults to the parent's PRIMARY KEY; a parent
				// without one is 42704 (PG's code here — undefined_object — even when the parent
				// has a UNIQUE), distinct from the explicit-no-match 42830.
				return outcome{}, newError(UndefinedObject,
					"there is no primary key for referenced table "+parent.Name)
			}
			refs = append([]int(nil), parent.PK...)
		} else {
			refs = make([]int, 0, len(fk.RefColumns))
			for _, cname := range fk.RefColumns {
				idx := -1
				for i := range parent.Columns {
					if strings.EqualFold(parent.Columns[i].Name, cname) {
						idx = i
						break
					}
				}
				if idx < 0 {
					return outcome{}, newError(UndefinedColumn,
						"column "+cname+" named in key does not exist")
				}
				if slices.Contains(refs, idx) {
					return outcome{}, newError(DuplicateColumn,
						"column "+cname+" appears twice in foreign key constraint")
				}
				refs = append(refs, idx)
			}
		}
		// 4. Referencing/referenced count must agree.
		if len(local) != len(refs) {
			return outcome{}, newError(InvalidForeignKey,
				"number of referencing and referenced columns for foreign key disagree")
		}
		// 5. Name — the per-table constraint namespace, shared with CHECK (§6.2/§6.7).
		var name string
		if fk.Name != "" {
			collide := false
			for _, c := range table.Checks {
				if strings.EqualFold(c.Name, fk.Name) {
					collide = true
					break
				}
			}
			if !collide {
				for _, f := range resolvedFks {
					if strings.EqualFold(f.Name, fk.Name) {
						collide = true
						break
					}
				}
			}
			if collide {
				return outcome{}, newError(DuplicateObject,
					"constraint "+fk.Name+" for relation "+table.Name+" already exists")
			}
			name = fk.Name
		} else {
			base := strings.ToLower(table.Name)
			for _, i := range local {
				base += "_" + strings.ToLower(table.Columns[i].Name)
			}
			base += "_fkey"
			fkNameTaken := func(candidate string) bool {
				for _, c := range table.Checks {
					if strings.EqualFold(c.Name, candidate) {
						return true
					}
				}
				for _, f := range resolvedFks {
					if strings.EqualFold(f.Name, candidate) {
						return true
					}
				}
				return false
			}
			name = base
			for suffix := 1; fkNameTaken(name); suffix++ {
				name = base + strconv.Itoa(suffix)
			}
		}
		// 6. Reject the unsupported write-actions (§6.6).
		onDelete, err := newFkAction(fk.OnDelete, "DELETE")
		if err != nil {
			return outcome{}, err
		}
		onUpdate, err := newFkAction(fk.OnUpdate, "UPDATE")
		if err != nil {
			return outcome{}, err
		}
		// 7. The referenced columns must be the parent's PK or a UNIQUE set (§6.2).
		refSet := sortedUnique(refs)
		matchesUnique := len(parent.PK) > 0 && slices.Equal(sortedUnique(parent.PK), refSet)
		if !matchesUnique {
			for _, ix := range parent.Indexes {
				if ix.Unique && slices.Equal(sortedUnique(ix.Columns), refSet) {
					matchesUnique = true
					break
				}
			}
		}
		if !matchesUnique {
			return outcome{}, newError(InvalidForeignKey,
				"there is no unique constraint matching given keys for referenced table "+parent.Name)
		}
		// 8. Same-type pairing (§6.2). Because the referenced columns are a PK/UNIQUE key they
		// are key-encodable, so a same-typed local column is key-encodable too — no separate
		// 0A000 type gate is needed.
		for i := range local {
			lt := table.Columns[local[i]].Type
			rt := parent.Columns[refs[i]].Type
			if !typesEqual(lt, rt) {
				return outcome{}, newError(DatatypeMismatch, fmt.Sprintf(
					"foreign key constraint %s cannot be implemented: key columns %s and %s are of incompatible types: %s and %s",
					name,
					table.Columns[local[i]].Name,
					parent.Columns[refs[i]].Name,
					lt.CanonicalName(),
					rt.CanonicalName(),
				))
			}
		}
		resolvedFks = append(resolvedFks, foreignKey{
			Name:       name,
			Columns:    local,
			RefTable:   parent.Name,
			RefColumns: refs,
			OnDelete:   onDelete,
			OnUpdate:   onUpdate,
		})
	}
	// Held in ascending lowercased-name order (the catalog's on-disk + evaluation order, §6.9).
	sort.SliceStable(resolvedFks, func(i, j int) bool {
		return strings.ToLower(resolvedFks[i].Name) < strings.ToLower(resolvedFks[j].Name)
	})
	table.ForeignKeys = resolvedFks

	// EXCLUDE constraints (spec/design/gist.md §7). Resolved AFTER the PK / UNIQUE / CHECK / FK
	// constraints, each in textual order: resolve the element columns (42703/42701) and the WITH
	// operators against the column types (42704 no-opclass / 0A000 deferred-or-unsupported), name the
	// constraint + its backing GiST index (the constraint IS its index — they share a name;
	// 42P07/42710 across the relation + constraint namespaces), and build the MULTI-COLUMN GiST index
	// that enforces it. The probe + 23P01 live in INSERT/UPDATE.
	for _, exc := range ct.Excludes {
		if exc.Using != "" && !strings.EqualFold(exc.Using, "gist") {
			return outcome{}, newError(UndefinedObject, "access method "+exc.Using+" does not support exclusion constraints")
		}
		indices := make([]int, 0, len(exc.Elements))
		elements := make([]exclusionElement, 0, len(exc.Elements))
		for _, el := range exc.Elements {
			ci := -1
			for i := range table.Columns {
				if strings.EqualFold(table.Columns[i].Name, el.Column) {
					ci = i
					break
				}
			}
			if ci < 0 {
				return outcome{}, newError(UndefinedColumn, "column "+el.Column+" named in key does not exist")
			}
			if slices.Contains(indices, ci) {
				return outcome{}, newError(DuplicateColumn, "column "+el.Column+" appears twice in exclusion constraint")
			}
			ty := table.Columns[ci].Type
			// The WITH operator must pair with the column's GiST opclass (gist.md §7): && over a
			// range column (range_ops), = over a fixed-width keyable scalar (the in-core btree_gist).
			var op exclusionOp
			switch el.Op {
			case "&&":
				if !ty.IsRange() {
					return outcome{}, newError(UndefinedObject,
						"data type "+ty.CanonicalName()+" has no default operator class for access method gist that accepts operator &&")
				}
				op = exclOverlaps
			case "=":
				switch {
				case isGistScalarType(ty):
					op = exclEqual
				case isGistDeferredScalarType(ty):
					return outcome{}, newError(FeatureNotSupported,
						"an exclusion constraint with = over "+ty.CanonicalName()+" is not supported yet")
				default:
					return outcome{}, newError(UndefinedObject,
						"data type "+ty.CanonicalName()+" has no default operator class for access method gist")
				}
			default:
				return outcome{}, newError(FeatureNotSupported, "exclusion constraint operator "+el.Op+" is not supported yet")
			}
			indices = append(indices, ci)
			elements = append(elements, exclusionElement{Column: ci, Op: op})
		}
		// Name the constraint (= its backing index name). An explicit name checks the relation
		// namespace (42P07) then the table's constraint names (42710); a derived `<table>_<cols>_excl`
		// suffix-walks both.
		relTaken := func(n string) bool {
			if db.relationExists(n) || strings.EqualFold(table.Name, n) {
				return true
			}
			for _, ix := range table.Indexes {
				if strings.EqualFold(ix.Name, n) {
					return true
				}
			}
			return false
		}
		conTaken := func(n string) bool {
			for _, c := range table.Checks {
				if strings.EqualFold(c.Name, n) {
					return true
				}
			}
			for _, f := range table.ForeignKeys {
				if strings.EqualFold(f.Name, n) {
					return true
				}
			}
			for _, e := range table.Exclusions {
				if strings.EqualFold(e.Name, n) {
					return true
				}
			}
			return false
		}
		var name string
		if exc.Name != "" {
			// The named EXCLUDE constraint's backing GiST index carries the user-written name
			// into the relation namespace (introspection.md §4).
			if err := checkReservedName("constraint", exc.Name); err != nil {
				return outcome{}, err
			}
			if relTaken(exc.Name) {
				return outcome{}, newError(DuplicateTable, "relation already exists: "+exc.Name)
			}
			if conTaken(exc.Name) {
				return outcome{}, newError(DuplicateObject, "constraint "+exc.Name+" for relation "+table.Name+" already exists")
			}
			name = exc.Name
		} else {
			base := strings.ToLower(table.Name)
			for _, i := range indices {
				base += "_" + strings.ToLower(table.Columns[i].Name)
			}
			base += "_excl"
			name = base
			for suffix := 1; relTaken(name) || conTaken(name); suffix++ {
				name = base + strconv.Itoa(suffix)
			}
		}
		// Insert the backing GiST index in catalog (ascending lowercased-name) order.
		def := indexDef{Name: name, Columns: indices, Unique: false, Kind: indexGist}
		nameKey := strings.ToLower(name)
		pos := len(table.Indexes)
		for i, ix := range table.Indexes {
			if strings.ToLower(ix.Name) > nameKey {
				pos = i
				break
			}
		}
		table.Indexes = slices.Insert(table.Indexes, pos, def)
		table.Exclusions = append(table.Exclusions, exclusionConstraint{Name: name, Index: name, Elements: elements})
	}
	// Held in ascending lowercased-name order (the catalog's on-disk order — gist.md §8).
	sort.SliceStable(table.Exclusions, func(i, j int) bool {
		return strings.ToLower(table.Exclusions[i].Name) < strings.ToLower(table.Exclusions[j].Name)
	})

	if attachName != "" {
		// Deferred narrowings on an attached-database table this slice (attached-databases.md §8), each a
		// clean 0A000: a COMPOSITE-typed column (its type lives in the MAIN catalog — no cross-scope type
		// reference this slice), a serial/IDENTITY column (its OWNED sequence would be a cross-scope
		// sequence), and a collated column (the attachment snapshot carries no collation catalog). Plain
		// scalar / array / range / decimal columns with PK / NOT NULL / DEFAULT / CHECK / UNIQUE and
		// secondary btree indexes are fully supported.
		for _, c := range table.Columns {
			if c.Type.IsComposite() {
				return outcome{}, newError(FeatureNotSupported, "a composite-typed column on an attached-database table is not supported yet")
			}
			if c.Collation != "" {
				return outcome{}, newError(FeatureNotSupported, "COLLATE on an attached-database-table column "+c.Name+" is not yet supported")
			}
		}
		if len(pendingSerials) > 0 {
			return outcome{}, newError(FeatureNotSupported, "a serial / IDENTITY column on an attached-database table is not supported yet")
		}
		// Register into the attachment's working snapshot (attached-databases.md §6) — never the main
		// image; published into roots.attached at commit (N-root commit, §5). attachWriteSnap clones the
		// attachment's committed root on first write and marks it dirty. Its NEW stores bind to the
		// attachment's own paging (the storePaging seam — the same one temp/in-memory main use).
		ws := db.attachWriteSnap(attachName)
		ws.storePaging = db.core.attachments[attachName].storage.paging
		mainTypes := db.readSnap().types
		colTypes := make([]colType, len(table.Columns))
		for i, c := range table.Columns {
			colTypes[i] = resolveColType(c.Type, mainTypes)
		}
		// Build the attachment's new stores at ITS OWN page size (§2) — a file attachment may serialize at
		// a different page size than main, and its records must split to match its physical pages.
		aps := db.attachPageSize(attachName)
		ws.putTableResolved(table, colTypes, aps)
		for _, ix := range table.Indexes {
			ws.putIndexStore(strings.ToLower(ix.Name), newTableStore(pagePayload(aps), nil))
		}
		return outcome{Kind: outcomeStatement, Cost: 0}, nil
	}

	if targetTemp {
		// Deferred narrowing on a temp table this slice (spec/design/temp-tables.md §8), a clean 0A000:
		// a collated column (needs the temp snapshot to carry the collation catalog). Plain
		// scalar/array/range/decimal columns with PK / NOT NULL / DEFAULT / CHECK / UNIQUE,
		// serial/IDENTITY columns (the OWNED sequence is staged into the same temp snapshot below), and
		// COMPOSITE-typed columns (resolved against the MAIN type catalog just below) are fully supported.
		for _, c := range table.Columns {
			if c.Collation != "" {
				return outcome{}, newError(FeatureNotSupported, "COLLATE on temporary-table column "+c.Name+" is not yet supported")
			}
		}
		// Resolve each column's ColType against the MAIN snapshot's composite-type catalog
		// (spec/design/temp-tables.md §8): composites are always persistent (CREATE TYPE is persistent
		// DDL), so the temp snapshot's own types map is empty — resolving there would miss a composite
		// reference. The resulting ColType tree is self-contained, so the temp store needs nothing from
		// the catalog after this (composite.md §4).
		mainTypes := db.readSnap().types
		colTypes := make([]colType, len(table.Columns))
		for i, c := range table.Columns {
			colTypes[i] = resolveColType(c.Type, mainTypes)
		}
		// Register into the session-local temp snapshot — never the main image, so the table makes zero
		// file writes (§2). Flag tempDirty so the commit can skip persisting the main image.
		db.session.tx.tempDirty = true
		ts := db.session.tx.tempWorking
		// The session-local temp snapshot rides a per-domain MemoryBlockStore pager (temp-tables.md §6):
		// lazily create the domain storage on first use and stamp its paging onto this working snapshot, so
		// putTableResolved / putIndexStore attach it to every temp store.
		ts.storePaging = db.tempDomainPaging()
		ts.putTableResolved(table, colTypes, db.pageSize)
		for _, ix := range table.Indexes {
			ts.putIndexStore(strings.ToLower(ix.Name), newTableStore(pagePayload(db.pageSize), nil))
		}
		// Stage each serial/IDENTITY column's OWNED sequence into the SAME temp snapshot
		// (spec/design/sequences.md §12, temp-tables.md §8) — never the main image, so the sequence
		// (like the table) makes zero file writes and is dropped with the table. The names were resolved
		// collision-free during the column walk (relationExists is temp-aware); nextval resolves and
		// advances them via the scope-aware sequence funnel.
		for _, s := range pendingSerials {
			ts.putSequence(s)
		}
		return outcome{Kind: outcomeStatement, Cost: 0}, nil
	}

	db.putTable(table)
	// The table is brand new (no rows), so each backing index store starts empty.
	for _, ix := range table.Indexes {
		db.working().putIndexStore(strings.ToLower(ix.Name), newTableStore(pagePayload(db.pageSize), nil))
	}
	// Stage each serial column's OWNED sequence now that the table validated
	// (spec/design/sequences.md §12). The names were resolved (collision-free) during the column
	// walk; the table is in the catalog, so a DROP TABLE will auto-drop these.
	for _, s := range pendingSerials {
		db.working().putSequence(s)
	}
	// DDL touches no rows and evaluates no expressions: zero cost.
	return outcome{Kind: outcomeStatement, Cost: 0}, nil
}

// resolveChecks resolves a table's CHECK constraints for a write statement: each stored
// expression against a one-relation scope, in the catalog's (evaluation/name) order.
// Cannot fail for a catalog produced by CREATE TABLE or a well-formed file (both
// validated); a hand-corrupted expression surfaces its natural resolve error.
func (db *engine) resolveChecks(table *catTable) ([]namedCheck, error) {
	if len(table.Checks) == 0 {
		return nil, nil
	}
	s := singleScope(db, table)
	out := make([]namedCheck, 0, len(table.Checks))
	for i := range table.Checks {
		node, _, err := resolve(s, table.Checks[i].Expr, nil, &aggCtx{collecting: false}, &paramTypes{})
		if err != nil {
			return nil, err
		}
		out = append(out, namedCheck{name: table.Checks[i].Name, node: node})
	}
	return out, nil
}

// resolveDefaultExprs resolves each column's EXPRESSION default (constraints.md §2) to an
// rExpr, once per INSERT statement — insertRows (and the VALUES DEFAULT-keyword
// materialization) evaluate it per omitted/DEFAULT slot. Returns a slot per column (parallel to
// table.Columns): a non-nil node for an expression default, nil for a column with a constant
// default or no default. The default resolves against an EMPTY scope (no columns; a column
// reference was rejected 0A000 at CREATE TABLE) with the column's type as the operand hint.
func (db *engine) resolveDefaultExprs(table *catTable) ([]*rExpr, error) {
	out := make([]*rExpr, len(table.Columns))
	for i := range table.Columns {
		de := table.Columns[i].DefaultExpr
		if de == nil {
			continue
		}
		colScalar := table.Columns[i].Type.ScalarTy()
		node, _, err := resolve(emptyScope(db), de.Expr, &colScalar, &aggCtx{collecting: false}, &paramTypes{})
		if err != nil {
			return nil, err
		}
		out[i] = node
	}
	return out, nil
}

// evalDefault is the value an omitted column or a DEFAULT value slot takes (constraints.md §2):
// the column's pre-evaluated constant (col.Default, or NULL when it has none), OR — for an
// expression default — the resolved rExpr evaluated against an empty row through the
// per-statement seam/clock (rng) and metered (operator_eval per node). Reused by the VALUES
// materialization (a DEFAULT keyword) and insertRows (an omitted column), sharing ONE StmtRng
// so a multi-row DEFAULT uuidv7() stays monotonic. defaultRExpr is nil for a constant/no default.
func (db *engine) evalDefault(col catColumn, defaultRExpr *rExpr, rng *stmtRng, meter *costMeter) (Value, error) {
	if defaultRExpr == nil {
		return defaultOrNull(col), nil
	}
	if err := meter.Guard(); err != nil {
		return Value{}, err
	}
	env := &evalEnv{exec: db, rng: rng}
	return defaultRExpr.eval(nil, env, meter)
}

// namedCheck is one statement-resolved CHECK constraint: its name (for the 23514
// message) and the resolved expression evaluated per candidate row.
type namedCheck struct {
	name string
	node *rExpr
}

// evalChecks evaluates a row's CHECK constraints in name order (constraints.md §4.4):
// TRUE and NULL pass; the first FALSE aborts with 23514 and PG's message. Shared by the
// INSERT and UPDATE write paths.
func evalChecks(checks []namedCheck, relation string, row storedRow, env *evalEnv, meter *costMeter) error {
	for _, c := range checks {
		v, err := c.node.eval(row, env, meter)
		if err != nil {
			return err
		}
		if v.Kind == ValBool && !v.boolVal() {
			return newError(CheckViolation,
				"new row for relation "+relation+" violates check constraint "+c.name)
		}
	}
	return nil
}

// dropScope is the scope a resolved DROP TABLE target lives in (temp-tables.md §3) — it governs
// which working snapshot the removal routes to.
type dropScope int

const (
	dropTemp dropScope = iota
	dropPersistent
)

type dropTarget struct {
	key   string // lowercased catalog key
	scope dropScope
}

// executeDropTable runs a DROP TABLE [IF EXISTS] a [, …] [CASCADE | RESTRICT]: remove each named
// table's definition and row store from the catalog (keyed by lower-cased name). Two-phase /
// all-or-nothing (spec/design/grammar.md §13): every name is resolved and validated first — a
// missing table is 42P01 (unless IF EXISTS skips just that name), a non-table relation is 42809,
// and an external FK dependent is 2BP01 under RESTRICT — and only if the whole list checks out is
// anything removed. A repeated name is deduplicated; a FK between two tables both in the drop set
// never blocks; CASCADE drops the surviving tables' now-dangling FK constraints. Like CREATE TABLE
// it touches no rows and evaluates no expression tree, so it accrues zero cost.
func (db *engine) executeDropTable(dt *dropTable) (outcome, error) {
	// ---- Phase 1: resolve & classify every name into the drop set. Nothing is removed yet. A
	// repeated name is deduplicated (PG collects the targets into a set, so `DROP TABLE a, a` drops
	// `a` once and succeeds); seen is the set of lowercased keys actually being dropped.
	var targets []dropTarget
	seen := map[string]bool{}
	for _, name := range dt.Names {
		key := strings.ToLower(name)
		if seen[key] {
			continue // already resolved this exact target (deduplicated)
		}
		// A built-in catalog relation resolves BEFORE the user catalog (introspection.md §5), and a
		// system relation cannot be dropped: 42809. IF EXISTS does not suppress this (the relation
		// exists — this is a kind rejection, not a missing name).
		if isCatalogRelName(key) {
			return outcome{}, newError(WrongObjectType, `cannot drop system relation "`+key+`"`)
		}
		// Resolution walk: session-local temp → persistent. Preclude-overlaps keeps a name in at most one
		// scope, so this is just "where it lives" (temp-tables.md §3).
		var scope dropScope
		switch {
		case db.isTempTable(name):
			scope = dropTemp
		default:
			if _, ok := db.readSnap().table(name); ok {
				scope = dropPersistent
			} else {
				// Not a table in any scope. An index's name is the wrong object kind (42809 —
				// indexes.md §2); IF EXISTS does NOT suppress this. Otherwise a missing table is
				// 42P01, unless IF EXISTS makes it a no-op for just this name.
				if _, _, ok := db.findIndex(name); ok {
					return outcome{}, newError(WrongObjectType, name+" is not a table")
				}
				if dt.IfExists {
					continue
				}
				return outcome{}, newError(UndefinedTable, "table does not exist: "+name)
			}
		}
		seen[key] = true
		targets = append(targets, dropTarget{key: key, scope: scope})
	}
	// ---- Phase 2: FK dependency check (RESTRICT) / removal collection (CASCADE). Only a persistent
	// table can be an FK parent (a temp table never is, §8), so the scan runs over the persistent
	// snapshot; a dependent whose referencing table is itself in the drop set does not count (the
	// drop-set exclusion is the whole seen set, so `DROP TABLE parent, child` succeeds even under
	// RESTRICT).
	deps := db.readSnap().foreignKeyDependentsExcluding(seen)
	var cascadeRemovals []fkDependent
	if dt.Cascade {
		cascadeRemovals = deps
	} else if len(deps) > 0 {
		// RESTRICT (the default, and the bare form's behavior): an external FK dependent blocks the
		// drop with 2BP01 — the same message the single-table check produced.
		d := deps[0]
		return outcome{}, newError(DependentObjectsStillExist,
			"cannot drop table "+d.droppedName+" because other objects depend on it: constraint "+
				d.fkName+" on table "+d.refTableName)
	}
	// ---- Phase 3: apply. CASCADE first drops each surviving table's now-dangling FK constraint (in
	// place, preserving its rows). A FK only ever lives on a persistent table (temp tables reject FKs
	// at CREATE), so the removal routes to the main working snapshot.
	for _, d := range cascadeRemovals {
		db.working().removeForeignKey(d.refTableKey, d.fkName)
	}
	// Then remove every target from its own scope, auto-dropping the sequences it owns — a
	// serial/IDENTITY column's owned sequence (spec/design/sequences.md §12; an owned sequence is
	// never an FK dependent, so the phase-2 check never blocked on it). A temp drop touches only its
	// temp snapshot, never the main image, so it makes zero file writes.
	for _, tgt := range targets {
		switch tgt.scope {
		case dropTemp:
			db.session.tx.tempDirty = true
			ts := db.tempSnap()
			for _, sk := range ts.sequencesOwnedBy(tgt.key) {
				ts.removeSequence(sk)
			}
			ts.removeTable(tgt.key)
		case dropPersistent:
			ownedSeqs := db.readSnap().sequencesOwnedBy(tgt.key)
			w := db.working()
			for _, sk := range ownedSeqs {
				w.removeSequence(sk)
			}
			w.removeTable(tgt.key)
		}
	}
	return outcome{Kind: outcomeStatement, Cost: 0}, nil
}

// chooseSerialSeqName chooses the auto-generated name for a serial column's OWNED sequence
// (spec/design/sequences.md §12), matching PostgreSQL: lower(table)_lower(column)_seq, with the
// smallest integer suffix 1, 2, … appended until the name is free in the relation namespace — not
// taken by an existing relation, not equal to the table being created, and not already chosen by an
// earlier serial column of the same statement (pending). All-lowercase identifier-derived.
func (db *engine) chooseSerialSeqName(table, column string, pending []*sequenceDef) string {
	base := strings.ToLower(table) + "_" + strings.ToLower(column) + "_seq"
	taken := func(c string) bool {
		if db.relationExists(c) || strings.EqualFold(c, table) {
			return true
		}
		for _, s := range pending {
			if strings.EqualFold(s.Name, c) {
				return true
			}
		}
		return false
	}
	if !taken(base) {
		return base
	}
	for n := 1; ; n++ {
		cand := fmt.Sprintf("%s%d", base, n)
		if !taken(cand) {
			return cand
		}
	}
}

// buildSequenceDef resolves a parsed SeqOptions set into a validated SequenceDef
// (spec/design/sequences.md §1/§14), shared by CREATE SEQUENCE and an IDENTITY column's
// `( seq_options )` (§13). The AS type (or the serial/identity-supplied default) sets the default +
// validated bounds; then validates INCREMENT (≠ 0), CACHE (≥ 1), explicit MIN/MAX within the type
// range, MINVALUE ≤ MAXVALUE, and START in [min, max] (each 22023); a fresh sequence starts with
// LastValue = Start, IsCalled = false. ownedBy carries the IDENTITY / serial owner link (nil for a
// plain CREATE SEQUENCE).
func buildSequenceDef(name string, options seqOptions, ownedBy *seqOwner) (*sequenceDef, error) {
	// The value type (§14): `AS <type>` → the named type (22023 if not an integer type), else bigint.
	dtype := seqBigInt
	if options.DataType != "" {
		dt, ok := seqDataTypeFromName(options.DataType)
		if !ok {
			return nil, newError(InvalidParameterValue,
				"sequence type must be smallint, integer, or bigint")
		}
		dtype = dt
	}
	typeMin, typeMax := dtype.Range()
	increment := int64(1)
	if options.Increment != nil {
		increment = *options.Increment
	}
	if increment == 0 {
		return nil, newError(InvalidParameterValue, "INCREMENT must not be zero")
	}
	cache := int64(1)
	if options.Cache != nil {
		cache = *options.Cache
	}
	if cache < 1 {
		return nil, newError(InvalidParameterValue,
			fmt.Sprintf("CACHE (%d) must be greater than zero", cache))
	}
	defMin, defMax := dtype.DefaultBounds(increment)
	// An explicit MAXVALUE/MINVALUE outside the type range is 22023 — checked (MAX first, PG order)
	// BEFORE the MIN > MAX consistency check (§14.2).
	if options.MaxValue != nil && !options.MaxValue.NoValue && options.MaxValue.Value > typeMax {
		return nil, newError(InvalidParameterValue, fmt.Sprintf(
			"MAXVALUE (%d) is out of range for sequence data type %s", options.MaxValue.Value, dtype.PgName(),
		))
	}
	if options.MinValue != nil && !options.MinValue.NoValue && options.MinValue.Value < typeMin {
		return nil, newError(InvalidParameterValue, fmt.Sprintf(
			"MINVALUE (%d) is out of range for sequence data type %s", options.MinValue.Value, dtype.PgName(),
		))
	}
	// A non-nil SeqBound with NoValue selects the default; with a value sets the explicit bound; a
	// nil SeqBound means the option was unset → the default (the Rust Some(Some)/Some(None)/None).
	minValue := defMin
	if options.MinValue != nil && !options.MinValue.NoValue {
		minValue = options.MinValue.Value
	}
	maxValue := defMax
	if options.MaxValue != nil && !options.MaxValue.NoValue {
		maxValue = options.MaxValue.Value
	}
	// PG requires MINVALUE strictly less than MAXVALUE (a one-value sequence is rejected); jed
	// previously allowed `==` — corrected here so CREATE and ALTER (sequences.md §15.2) agree with PG.
	if minValue >= maxValue {
		return nil, newError(InvalidParameterValue,
			fmt.Sprintf("MINVALUE (%d) must be less than MAXVALUE (%d)", minValue, maxValue))
	}
	// START defaults to MINVALUE (ascending) / MAXVALUE (descending) and must lie in [min, max].
	start := minValue
	if increment < 0 {
		start = maxValue
	}
	if options.Start != nil {
		start = *options.Start
	}
	if err := seqBoundCheckStart(start, minValue, maxValue); err != nil {
		return nil, err
	}
	cycle := false
	if options.Cycle != nil {
		cycle = *options.Cycle
	}
	return &sequenceDef{
		Name:      name,
		Increment: increment,
		MinValue:  minValue,
		MaxValue:  maxValue,
		Start:     start,
		Cache:     cache,
		Cycle:     cycle,
		LastValue: start,
		IsCalled:  false,
		OwnedBy:   ownedBy,
	}, nil
}

// seqBoundCheckStart is PG's START-in-bounds cross-check (init_params): start ∈ [min, max], else
// 22023 with PG's wording. Shared by CREATE (buildSequenceDef) and ALTER (applySeqAlter).
func seqBoundCheckStart(start, minValue, maxValue int64) error {
	if start < minValue {
		return newError(InvalidParameterValue,
			fmt.Sprintf("START value (%d) cannot be less than MINVALUE (%d)", start, minValue))
	}
	if start > maxValue {
		return newError(InvalidParameterValue,
			fmt.Sprintf("START value (%d) cannot be greater than MAXVALUE (%d)", start, maxValue))
	}
	return nil
}

// seqBoundCheckLast is PG's last_value (RESTART) cross-check (init_params): the post-edit last_value ∈
// [min, max], else 22023. PG uses the "RESTART value …" wording even with no RESTART written (§15.2).
func seqBoundCheckLast(lastValue, minValue, maxValue int64) error {
	if lastValue < minValue {
		return newError(InvalidParameterValue,
			fmt.Sprintf("RESTART value (%d) cannot be less than MINVALUE (%d)", lastValue, minValue))
	}
	if lastValue > maxValue {
		return newError(InvalidParameterValue,
			fmt.Sprintf("RESTART value (%d) cannot be greater than MAXVALUE (%d)", lastValue, maxValue))
	}
	return nil
}

// applySeqAlter re-edits an existing SequenceDef per ALTER SEQUENCE s <options>
// (spec/design/sequences.md §15.2) — PG init_params with isInit=false. Only the WRITTEN options
// change; LastValue/IsCalled are preserved unless restart is given. The value type is not persisted
// (§14.4), so NO MINVALUE/NO MAXVALUE reset the open direction to the bigint bound and an explicit
// bound is i64-checked only. options.DataType must be "" (the caller rejects AS as 0A000 first).
func applySeqAlter(existing *sequenceDef, options seqOptions, restart *seqRestart) (*sequenceDef, error) {
	def := *existing
	if options.Increment != nil {
		if *options.Increment == 0 {
			return nil, newError(InvalidParameterValue, "INCREMENT must not be zero")
		}
		def.Increment = *options.Increment
	}
	if options.Cache != nil {
		if *options.Cache < 1 {
			return nil, newError(InvalidParameterValue,
				fmt.Sprintf("CACHE (%d) must be greater than zero", *options.Cache))
		}
		def.Cache = *options.Cache
	}
	// NO MINVALUE/NO MAXVALUE recompute the default for the (possibly new) INCREMENT sign — against
	// the bigint range (the value type is not persisted, §14.4). An explicit bound is taken as
	// written; an unwritten bound is preserved (PG keeps it even when the sign flips).
	defMin, defMax := seqBigInt.DefaultBounds(def.Increment)
	if options.MinValue != nil {
		if options.MinValue.NoValue {
			def.MinValue = defMin
		} else {
			def.MinValue = options.MinValue.Value
		}
	}
	if options.MaxValue != nil {
		if options.MaxValue.NoValue {
			def.MaxValue = defMax
		} else {
			def.MaxValue = options.MaxValue.Value
		}
	}
	if def.MinValue >= def.MaxValue {
		return nil, newError(InvalidParameterValue,
			fmt.Sprintf("MINVALUE (%d) must be less than MAXVALUE (%d)", def.MinValue, def.MaxValue))
	}
	if options.Start != nil {
		def.Start = *options.Start
	}
	// Cross-check 1: START ∈ [min, max].
	if err := seqBoundCheckStart(def.Start, def.MinValue, def.MaxValue); err != nil {
		return nil, err
	}
	// RESTART (applied last, before the last_value cross-check).
	if restart != nil {
		if restart.ToStart {
			def.LastValue = def.Start
		} else {
			def.LastValue = restart.Value
		}
		def.IsCalled = false
	}
	// Cross-check 2: the preserved/restarted last_value ∈ [min, max].
	if err := seqBoundCheckLast(def.LastValue, def.MinValue, def.MaxValue); err != nil {
		return nil, err
	}
	if options.Cycle != nil {
		def.Cycle = *options.Cycle
	}
	return &def, nil
}

// serialPseudoType maps a serial pseudo-type name to its underlying integer scalar
// (spec/design/sequences.md §12) — serial/serial4 → Int32, bigserial/serial8 → Int64,
// smallserial/serial2 → Int16. The bool is false for any other name. Recognized only in a
// CREATE TABLE column-type position; the match is case-insensitive.
func serialPseudoType(name string) (scalarType, bool) {
	switch strings.ToLower(name) {
	case "serial", "serial4":
		return scalarInt32, true
	case "bigserial", "serial8":
		return scalarInt64, true
	case "smallserial", "serial2":
		return scalarInt16, true
	default:
		return 0, false
	}
}

// findIndex finds the table owning the named index in the visible snapshot
// (case-insensitive).
func (db *engine) findIndex(name string) (string, indexDef, bool) {
	return db.readSnap().findIndex(name)
}

// checkReservedName rejects a USER-written catalog object name beginning jed_ — the prefix is
// reserved for the engine's own catalog relations (spec/design/introspection.md §4). Case-insensitive
// (resolution folds case and there is no quoted-identifier escape — grammar.md §3). Engine-GENERATED
// names (a serial's <table>_<col>_seq, an index auto-name — both legal for a table named jed) never
// pass through here; the check sits with each site's namespace-collision check so established
// validation orders (42P01/42703 before name checks) are preserved. kind is the object word in the
// message: table / index / sequence / type.
func checkReservedName(kind, name string) error {
	if len(name) >= 4 && strings.EqualFold(name[:4], "jed_") {
		return newError(ReservedName, kind+" name "+name+" is reserved (the jed_ prefix is reserved for system objects)")
	}
	return nil
}

// relationExists reports whether name is taken in the shared relation namespace (a table
// OR an index — spec/design/indexes.md §2), case-insensitively.
func (db *engine) relationExists(name string) bool {
	// Session-local temp tables + their (UNIQUE) index names join the namespace too, so a name colliding
	// with any temp relation is also 42P07 (preclude-overlaps — spec/design/temp-tables.md §3). db.Table
	// is persistent-only, so the temp snapshot is checked explicitly.
	if _, ok := db.Table(name); ok {
		return true
	}
	if _, ok := db.tempSnap().table(name); ok {
		return true
	}
	if _, _, ok := db.findIndex(name); ok {
		return true
	}
	if _, _, ok := db.tempSnap().findIndex(name); ok {
		return true
	}
	// The sequence funnel walks session-local → persistent, so an owned TEMP sequence's name joins the
	// namespace (temp-tables.md §8) — a collision with it is 42P07 too.
	return db.sequence(name) != nil
}

// executeCreateIndex analyzes and runs a CREATE INDEX (spec/design/indexes.md §2).
// Validation mirrors PostgreSQL's order (oracle-probed): the table must exist (42P01);
// each key column, in list order, must exist (42703) and be of a key-encodable type
// (0A000 — the same narrowing as a PRIMARY KEY member); then an explicit name is checked
// against the shared relation namespace (42P07), or an omitted name derives PG's choice —
// the lowercased <table>_<col>..._idx with the smallest free suffix. The index is then
// built by scanning the table once: page_read per node + storage_row_read per row (the
// metered build scan — cost.md §3); maintenance thereafter is unmetered.
func (db *engine) executeCreateIndex(ci *createIndex) (outcome, error) {
	// A standalone CREATE INDEX targets whichever scope owns the table — session-local temp,
	// persistent, or a host-attached database (spec/design/temp-tables.md §8, attached-databases.md §3).
	// The build below is scope-agnostic (the scoped lkpTable/lkpStore/writeIndexStore funnels route by
	// the qualifier + resolution walk; the cost meter, UNIQUE validation, naming/namespace collision,
	// and the storage budget are all generic); only the catalog putIndex write must target the owning
	// snapshot, so the routing happens there.
	// A built-in catalog relation cannot be indexed (introspection.md §5): 42809, checked by NAME
	// before qualifier validation, like the DML targets.
	if err := checkCatalogRelWrite(ci.Table); err != nil {
		return outcome{}, err
	}
	// A DDL write to a READ-ONLY host attachment is 25006 before any work — checked BEFORE the qualifier
	// existence gate so a read-only attachment refuses the write deterministically (attached-databases.md §4).
	if err := db.checkAttachmentWritable(ci.DB); err != nil {
		return outcome{}, err
	}
	if err := db.checkTableQualifier(ci.DB, ci.Table); err != nil { // attached-databases.md §3
		return outcome{}, err
	}
	attachName := ""
	if isAttachmentScope(ci.DB) {
		attachName = strings.ToLower(*ci.DB)
	}
	table, ok := db.lkpTableScoped(ci.DB, ci.Table)
	if !ok {
		return outcome{}, newError(UndefinedTable, "table does not exist: "+ci.Table)
	}
	tableKey := strings.ToLower(table.Name)
	columns := table.Columns
	// Refuse building a collated index on a version-skewed table (slice 2d, collation.md §12, XX002):
	// the new B-tree would be pinned inconsistently with the file's other structures.
	if err := db.ensureCollationsWritable(columns); err != nil {
		return outcome{}, err
	}
	// Per-column frozen collations for the collated text key form (§2.12); nil everywhere for a
	// C-only / non-text table (the fast path).
	colls := db.columnCollations(columns)
	// Resolve the access method (spec/design/gin.md §3): the default / "btree" is the ordered
	// B-tree, "gin" a GIN inverted index; an unknown method is 42704. Resolved here (not in the
	// parser) so the error is the resolve-time undefined_object, after the table-exists check.
	var kind indexKind
	switch strings.ToLower(ci.Using) {
	case "", "btree":
		kind = indexBtree
	case "gin":
		kind = indexGin
	case "gist":
		kind = indexGist
	default:
		return outcome{}, newError(UndefinedObject, "access method does not exist: "+ci.Using)
	}
	cols := make([]int, 0, len(ci.Columns))
	for _, name := range ci.Columns {
		idx := table.ColumnIndex(name)
		if idx < 0 {
			return outcome{}, newError(UndefinedColumn, "column does not exist: "+name)
		}
		ty := columns[idx].Type
		switch kind {
		case indexBtree:
			if !ty.IsInteger() && !ty.IsBool() && !ty.IsText() && !ty.IsBytea() && !ty.IsDecimal() && !ty.IsUuid() && !ty.IsTimestamp() && !ty.IsTimestamptz() && !ty.IsDate() && !ty.IsInterval() && !ty.IsFloat() && !ty.IsRange() && !isArrayKeyable(ty) {
				return outcome{}, newError(FeatureNotSupported,
					"a "+ty.CanonicalName()+" index column is not supported yet")
			}
		case indexGin:
			// GIN needs an operator class for the column type: only an array has one (else 42704),
			// and only a FIXED-WIDTH KEY-ENCODABLE element type (else 0A000) — the GIN term IS that
			// element's key encoding (gin.md §3/§4), so the admitted set is the integers, boolean,
			// uuid, date, timestamp, timestamptz (interval's GIN-element support is a separate
			// follow-on — its key landed but the GIN slice has not; gin.md §3/§10).
			if ty.Array == nil {
				return outcome{}, newError(UndefinedObject,
					"data type "+ty.CanonicalName()+" has no default operator class for access method gin")
			}
			if elem, ok := ty.Array.AsScalar(); !ok || !isGinElementType(elem) {
				return outcome{}, newError(FeatureNotSupported,
					"a gin index on "+ty.CanonicalName()+" is not supported yet")
			}
		case indexGist:
			// GiST opclasses (gist.md §5/§6): range_ops over a range column, or the in-core
			// btree_gist-equivalent scalar `=` opclass over a FIXED-WIDTH keyable scalar (integers /
			// boolean / uuid / date / timestamp / timestamptz — its bound is [min,max] over that type's
			// order-preserving key encoding, all pure byte comparison). A keyable-but-deferred scalar
			// (text / bytea / decimal / interval) is 0A000 — we will support it (the GIN element-staging
			// precedent, §11); any other type (float / json / array / composite / jsonpath) has no GiST
			// opclass at all — 42704 (PG's wording).
			if !ty.IsRange() {
				switch {
				case isGistScalarType(ty):
					// supported scalar `=` opclass — ok
				case isGistDeferredScalarType(ty):
					return outcome{}, newError(FeatureNotSupported,
						"a gist index on "+ty.CanonicalName()+" is not supported yet")
				default:
					return outcome{}, newError(UndefinedObject,
						"data type "+ty.CanonicalName()+" has no default operator class for access method gist")
				}
			}
		}
		// A duplicate column in the list is ALLOWED (PostgreSQL allows it — indexes.md §1).
		cols = append(cols, idx)
	}
	// GIN narrowings this slice (spec/design/gin.md §3): no uniqueness (undefined for an inverted
	// index) and a single column only — both deferred 0A000.
	if kind == indexGin {
		if ci.Unique {
			return outcome{}, newError(FeatureNotSupported, "access method gin does not support unique indexes")
		}
		if len(cols) != 1 {
			return outcome{}, newError(FeatureNotSupported, "a multi-column gin index is not supported yet")
		}
	}
	// GiST narrowings (gist.md §1/§5/§11): no uniqueness (express it as EXCLUDE … WITH =, GX3) and a
	// single column only (multi-column GiST is GX2/GX3). A GiST index on a TEMP table is 0A000 (its
	// resident R-tree would live on the temp snapshot — deferred, gist.md §11). File persistence
	// landed in GX1b, so a file-backed GiST index is supported.
	if kind == indexGist {
		if ci.Unique {
			return outcome{}, newError(FeatureNotSupported, "access method gist does not support unique indexes")
		}
		if len(cols) != 1 {
			return outcome{}, newError(FeatureNotSupported, "a multi-column gist index is not supported yet")
		}
		if db.isTempTable(ci.Table) {
			return outcome{}, newError(FeatureNotSupported, "a gist index on a temporary table is not supported yet")
		}
	}
	// A non-btree (GIN / GiST) index on an attached-database table is a deferred narrowing this slice
	// (attached-databases.md §8) — the attachment stores only btree PK / UNIQUE / secondary indexes.
	if attachName != "" && kind != indexBtree {
		return outcome{}, newError(FeatureNotSupported, "a "+ci.Using+" index on an attached-database table is not supported yet")
	}
	// relationExistsScoped checks the namespace of the target scope: an attachment's OWN snapshot for an
	// attached table (each attached database is an independent namespace, §3), else the temp-aware
	// implicit namespace.
	relationTaken := func(n string) bool {
		if attachName != "" {
			as := db.attachReadSnap(attachName)
			if _, ok := as.table(n); ok {
				return true
			}
			_, _, ok := as.findIndex(n)
			return ok
		}
		return db.relationExists(n)
	}
	name := ci.Name
	if name != "" {
		if err := checkReservedName("index", name); err != nil {
			return outcome{}, err
		}
		if relationTaken(name) {
			return outcome{}, newError(DuplicateTable, "relation already exists: "+name)
		}
	} else {
		// PG's ChooseIndexName (probed): lowercased table + every listed column name
		// (list order, duplicates included) + "idx", then the smallest free suffix.
		base := tableKey
		for _, cn := range ci.Columns {
			base += "_" + strings.ToLower(cn)
		}
		base += "_idx"
		name = base
		for suffix := 1; relationTaken(name); suffix++ {
			name = base + strconv.Itoa(suffix)
		}
	}

	// The build scan (cost.md §3): page_read per table-tree node + storage_row_read per
	// row, with the indexed columns as the touched set (fixed-width — the chain/decompress
	// terms are structurally zero). An empty table charges 0. The entries are computed
	// here, against the pre-index store; the writes below are unmetered.
	meter := db.session.newMeter()
	mask := make([]bool, len(columns))
	for _, c := range cols {
		mask[c] = true
	}
	def := indexDef{Name: name, Columns: cols, Unique: ci.Unique, Kind: kind}
	store := db.lkpStoreScoped(ci.DB, ci.Table)
	stored, nodes, slabs, err := store.ScanWithUnits(mask)
	if err != nil {
		return outcome{}, err
	}
	meter.Charge(costs.PageRead*int64(nodes) + costs.ValueDecompress*int64(slabs))
	entries := make([][]byte, 0, len(stored))
	// A UNIQUE build verifies the existing rows before the index is registered
	// (indexes.md §8): two rows sharing a fully-non-NULL key tuple — i.e. an exempt-free
	// prefix — trap 23505 and create nothing. Unmetered validation (cost.md §3).
	seenPrefixes := make(map[string]bool)
	for _, e := range stored {
		if err := meter.Guard(); err != nil { // enforce the cost ceiling per scanned row
			return outcome{}, err
		}
		meter.Charge(costs.StorageRowRead)
		// The build reads the indexed key columns directly; resolve a faulted row's inline-deferred
		// values (lazy-record.md §5b — always inline for a key column, so cost-free) before encoding.
		row, err := store.resolveInlineColumns(e.Row)
		if err != nil {
			return outcome{}, err
		}
		if def.Unique {
			prefix, ok, err := indexPrefixKey(columns, colls, def, row)
			if err != nil {
				return outcome{}, err
			}
			if ok {
				if seenPrefixes[string(prefix)] {
					return outcome{}, newError(UniqueViolation,
						"duplicate key value violates unique constraint: "+def.Name)
				}
				seenPrefixes[string(prefix)] = true
			}
		}
		eks, err := indexEntryKeys(columns, colls, def, e.Key, row)
		if err != nil {
			return outcome{}, err
		}
		entries = append(entries, eks...)
	}
	if err := meter.Guard(); err != nil {
		return outcome{}, err
	}

	nameKey := strings.ToLower(def.Name)
	// Register the index catalog entry + its (empty) store in the snapshot that owns the table (the
	// resolution walk — temp-tables.md §2/§4/§8): a session-local temp table's index lives in the
	// session temp snapshot, so the index makes ZERO file writes (the dirty bit lets the commit skip the
	// main image). The entry writes below then route through writeIndexStore, which finds the new store
	// in that same temp snapshot.
	switch {
	case attachName != "":
		// The attachment's index catalog entry + (empty) store live in its working snapshot, published
		// into roots.attached at commit (attached-databases.md §5/§6). attachWriteSnap marks it dirty.
		ws := db.attachWriteSnap(attachName)
		ws.storePaging = db.core.attachments[attachName].storage.paging
		ws.putIndex(tableKey, def, db.attachPageSize(attachName)) // the attachment's own page space (§2)
	case db.isTempTable(ci.Table):
		db.session.tx.tempDirty = true
		db.session.tx.tempWorking.putIndex(tableKey, def, db.pageSize)
	default:
		db.working().putIndex(tableKey, def, db.pageSize)
	}
	istore := db.writeIndexStoreScoped(ci.DB, nameKey)
	// Insert sorted by entry key (indexes.md §1): every insert is then a right-edge append,
	// so the built tree packs ~full instead of splintering under the storage-key order the
	// scan produced (random in entry-key space). Part of the byte contract — the sort fixes
	// the built tree's shape across cores.
	slices.SortFunc(entries, bytes.Compare)
	for _, ek := range entries {
		inserted, err := istore.Insert(ek, nil)
		if err != nil {
			return outcome{}, err
		}
		if !inserted {
			panic("index entry keys are unique (storage-key suffix)")
		}
	}
	return outcome{Kind: outcomeStatement, Cost: meter.Accrued}, nil
}

// executeDropIndex runs a DROP INDEX (spec/design/indexes.md §2): a table's name is
// 42809, a missing one 42704. A pure catalog edit — zero cost, like DROP TABLE. The index is
// resolved along the resolution walk (session-local → persistent — temp-tables.md §8) and removed
// from the snapshot that owns it, so dropping a temp table's index makes zero file writes.
func (db *engine) executeDropIndex(di *dropIndex) (outcome, error) {
	// lkpTable covers both scopes, so DROP INDEX naming a table is 42809 regardless of kind.
	if _, ok := db.lkpTable(di.Name); ok {
		return outcome{}, newError(WrongObjectType, di.Name+" is not an index")
	}
	nameKey := strings.ToLower(di.Name)
	switch {
	case db.isTempIndex(di.Name):
		tableKey, _, _ := db.tempSnap().findIndex(di.Name)
		db.session.tx.tempDirty = true
		db.session.tx.tempWorking.removeIndex(tableKey, nameKey)
	default:
		tableKey, _, ok := db.findIndex(di.Name)
		if !ok {
			return outcome{}, newError(UndefinedObject, "index does not exist: "+di.Name)
		}
		// An index that backs an EXCLUDE constraint cannot be dropped directly — the constraint owns
		// it (the UNIQUE-backing precedent; jed has no ALTER TABLE … DROP CONSTRAINT yet). 2BP01,
		// matching PG's "cannot drop index … because constraint … requires it" (gist.md §7).
		if t, tok := db.lkpTable(tableKey); tok {
			for _, e := range t.Exclusions {
				if strings.EqualFold(e.Index, di.Name) {
					return outcome{}, newError(DependentObjectsStillExist,
						"cannot drop index "+di.Name+" because constraint "+di.Name+" on table "+t.Name+" requires it")
				}
			}
		}
		db.working().removeIndex(tableKey, nameKey)
	}
	return outcome{Kind: outcomeStatement, Cost: 0}, nil
}

// executeCreateType analyzes and runs a CREATE TYPE (spec/design/composite.md): reject a duplicate
// type name (42710), resolve each field's type (a built-in scalar, or a previously-defined
// composite — 42704 if unknown; no self- or forward-reference), reject a duplicate field name
// (42701), then register the composite type in the catalog. Named composites only.
func (db *engine) executeCreateType(ct *createType) (outcome, error) {
	if err := checkReservedName("type", ct.Name); err != nil {
		return outcome{}, err
	}
	if db.readSnap().compositeType(ct.Name) != nil {
		return outcome{}, newError(DuplicateObject, "type "+ct.Name+" already exists")
	}
	fields := make([]compositeField, 0, len(ct.Fields))
	for _, f := range ct.Fields {
		for _, g := range fields {
			if strings.EqualFold(g.Name, f.Name) {
				return outcome{}, newError(DuplicateColumn, "attribute "+f.Name+" specified more than once")
			}
		}
		var fty dataType
		var fdecimal *decimalTypmod
		var fvarchar *uint32
		if base, ok := strings.CutSuffix(f.TypeName, "[]"); ok {
			// An array-typed field (spec/design/array.md §12 — the mirror of an array-of-composite
			// element). The element is a scalar or a previously-defined composite (element_type_code
			// 14 + name on disk); a nested-array element and an array typmod stay deferred (0A000),
			// exactly as for an array column.
			if f.TypeMod != nil {
				return outcome{}, newError(FeatureNotSupported,
					"a type modifier on an array type is not supported yet")
			}
			if elemScalar, scalarOK := scalarTypeFromName(base); scalarOK {
				fty = arrayT(scalarT(elemScalar))
			} else if ctype := db.readSnap().compositeType(base); ctype != nil {
				fty = arrayT(compositeT(ctype.Name))
			} else {
				return outcome{}, newError(UndefinedObject, "type does not exist: "+base)
			}
		} else if _, ok := scalarTypeFromName(f.TypeName); ok {
			s, d, vl, err := resolveTypeAndTypmod(f.TypeName, f.TypeMod)
			if err != nil {
				return outcome{}, err
			}
			fty, fdecimal, fvarchar = scalarT(s), d, vl
		} else if _, ok := rangeByName(f.TypeName); ok {
			// A range-typed composite field (a range inside CREATE TYPE) is deferred this slice (only
			// range *columns* are storable — spec/design/ranges.md §3); the type name IS known, so this
			// is 0A000, not the 42704 below.
			return outcome{}, newError(FeatureNotSupported,
				"a range-typed composite field ("+f.TypeName+") is not supported yet")
		} else if db.readSnap().compositeType(f.TypeName) != nil {
			if f.TypeMod != nil {
				return outcome{}, newError(FeatureNotSupported,
					"a type modifier is not supported for composite type "+f.TypeName)
			}
			fty = compositeT(f.TypeName)
		} else {
			return outcome{}, newError(UndefinedObject, "type does not exist: "+f.TypeName)
		}
		fields = append(fields, compositeField{Name: f.Name, Type: fty, Decimal: fdecimal, VarcharLen: fvarchar, NotNull: f.NotNull})
	}
	// Bound composite-type nesting depth (CLAUDE.md §13; cost.md §7b). A chain of CREATE TYPEs each
	// nesting the previous (`a`, `b AS (x a)`, …) builds unbounded depth across many cheap statements —
	// invisible to the per-statement input-size cap and the parser nesting counter — and every derived
	// recursive walk (codec, comparator, record_out/in, ResolveColType) recurses to this depth. Reject
	// at the producer so no over-deep type enters the catalog and every downstream walk stays
	// stack-safe. Fields reference only existing types (each already ≤ maxCompositeDepth), so this
	// depth computation's recursion is itself bounded.
	cache := make(map[string]int)
	maxField := 0
	for _, f := range fields {
		if d := db.readSnap().compositeTypeDepth(f.Type, cache); d > maxField {
			maxField = d
		}
	}
	if depth := 1 + maxField; depth > maxCompositeDepth {
		return outcome{}, newError(StatementTooComplex,
			fmt.Sprintf("composite type %s nesting depth %d exceeds the maximum of %d", ct.Name, depth, maxCompositeDepth))
	}
	db.working().putType(&compositeType{Name: ct.Name, Fields: fields})
	return outcome{Kind: outcomeStatement, Cost: 0}, nil
}

// executeDropType analyzes and runs a DROP TYPE (spec/design/composite.md §7). RESTRICT (the only
// behavior this slice): a missing type is 42704 unless IF EXISTS; if any table column or composite
// field still references the type, 2BP01; otherwise remove it from the catalog.
func (db *engine) executeDropType(dt *dropType) (outcome, error) {
	if db.readSnap().compositeType(dt.Name) == nil {
		if dt.IfExists {
			return outcome{Kind: outcomeStatement, Cost: 0}, nil
		}
		return outcome{}, newError(UndefinedObject, "type does not exist: "+dt.Name)
	}
	if dep, ok := db.compositeDependentAny(dt.Name); ok {
		return outcome{}, newError(DependentObjectsStillExist,
			"cannot drop type "+dt.Name+" because other objects depend on it: "+dep)
	}
	db.working().removeType(strings.ToLower(dt.Name))
	return outcome{Kind: outcomeStatement, Cost: 0}, nil
}

// executeCreateSequence analyzes and runs a CREATE SEQUENCE (spec/design/sequences.md). Resolve
// the option overrides against the INCREMENT sign's type defaults, validate the set (22023),
// reject a relation-namespace collision (42P07 unless IF NOT EXISTS), and register the sequence.
func (db *engine) executeCreateSequence(cs *createSequence) (outcome, error) {
	// The reservation is not a collision, so IF NOT EXISTS does not suppress it
	// (spec/design/introspection.md §4).
	if err := checkReservedName("sequence", cs.Name); err != nil {
		return outcome{}, err
	}
	if db.relationExists(cs.Name) {
		if cs.IfNotExists {
			return outcome{Kind: outcomeStatement, Cost: 0}, nil
		}
		return outcome{}, newError(DuplicateTable, "relation already exists: "+cs.Name)
	}
	def, err := buildSequenceDef(cs.Name, cs.Options, nil)
	if err != nil {
		return outcome{}, err
	}
	db.working().putSequence(def)
	return outcome{Kind: outcomeStatement, Cost: 0}, nil
}

// executeDropSequence analyzes and runs a DROP SEQUENCE (spec/design/sequences.md §1).
// RESTRICT-only: a missing sequence is 42P01 unless IF EXISTS. No dependency tracking this slice
// (a plain DEFAULT nextval('s') creates none — PG). Multiple names are dropped left to right.
func (db *engine) executeDropSequence(ds *dropSequence) (outcome, error) {
	for _, name := range ds.Names {
		// Missing → 42P01 (unless IF EXISTS). An OWNED (serial) sequence has a dependent — its
		// column's default — so RESTRICT (the only mode this slice; CASCADE 0A000) is 2BP01
		// (spec/design/sequences.md §12).
		seq := db.sequence(name)
		if seq == nil {
			if ds.IfExists {
				continue
			}
			return outcome{}, newError(UndefinedTable, "sequence does not exist: "+name)
		}
		if seq.OwnedBy != nil {
			// The owning table is always present (its own DROP TABLE would auto-drop this sequence
			// first), so the column name for the detail resolves. The scope-aware lkpTable finds an
			// owned TEMP sequence's temp owner (temp-tables.md §8).
			colName, tableName := "", seq.OwnedBy.Table
			if t, ok := db.lkpTable(seq.OwnedBy.Table); ok {
				tableName = t.Name
				if int(seq.OwnedBy.Column) < len(t.Columns) {
					colName = t.Columns[seq.OwnedBy.Column].Name
				}
			}
			return outcome{}, newError(DependentObjectsStillExist, fmt.Sprintf(
				"cannot drop sequence %s because other objects depend on it: default value for column %s of table %s depends on sequence %s",
				seq.Name, colName, tableName, seq.Name,
			))
		}
		// Not owned: remove from whichever scope owns it (a temp sequence is always owned, so this
		// routed path is reached only for a plain persistent sequence — temp-tables.md §8).
		db.removeSequenceRouted(name)
	}
	return outcome{Kind: outcomeStatement, Cost: 0}, nil
}

// executeAlterSequence analyzes and runs an ALTER SEQUENCE [IF EXISTS] s <action>
// (spec/design/sequences.md §4/§15). A missing sequence is 42P01 unless IF EXISTS (then a no-op).
// The option form re-edits the definition (PG init_params, isInit=false — only written options
// change, the counter preserved unless RESTART); RENAME TO moves the catalog key. Touches no session
// state (currval/lastval unchanged). A catalog write (the write path, transactional, §5).
func (db *engine) executeAlterSequence(as *alterSequence) (outcome, error) {
	snapDef := db.sequence(as.Name)
	if snapDef == nil {
		if as.IfExists {
			return outcome{Kind: outcomeStatement, Cost: 0}, nil
		}
		return outcome{}, newError(UndefinedTable, "relation does not exist: "+as.Name)
	}
	existing := *snapDef
	if as.RenameTo != "" {
		if err := db.alterSequenceRename(&existing, as.RenameTo); err != nil {
			return outcome{}, err
		}
	} else {
		// AS type on ALTER is 0A000 — the value type is not persisted (sequences.md §14.4), so the
		// original type for re-deriving a default bound is gone.
		if as.Options.DataType != "" {
			return outcome{}, newError(FeatureNotSupported, "ALTER SEQUENCE ... AS type is not supported")
		}
		newDef, err := applySeqAlter(&existing, as.Options, as.Restart)
		if err != nil {
			return outcome{}, err
		}
		db.putSequenceRouted(newDef)
	}
	return outcome{Kind: outcomeStatement, Cost: 0}, nil
}

// alterSequenceRename implements ALTER SEQUENCE s RENAME TO s2 (spec/design/sequences.md §15.3): a
// collision with any relation — including s itself — is 42P07; otherwise move the entry to the new
// key. For an OWNED sequence, the owning column's DEFAULT nextval('s') text is rewritten in place to
// nextval('s2') (the rows survive — not via putTable) so a later INSERT still advances the renamed
// sequence (jed resolves the sequence by name, unlike PG's OID reference).
func (db *engine) alterSequenceRename(existing *sequenceDef, newName string) error {
	if err := checkReservedName("sequence", newName); err != nil {
		return err
	}
	if db.relationExists(newName) {
		return newError(DuplicateTable, "relation already exists: "+newName)
	}
	if existing.OwnedBy != nil {
		exprText := "nextval ( '" + strings.ReplaceAll(strings.ToLower(newName), "'", "''") + "' )"
		expr, err := parseExpression(exprText)
		if err != nil {
			return err
		}
		// Route to the owner's scope so a renamed owned TEMP sequence rewrites its column default in
		// the temp snapshot (temp-tables.md §8).
		db.setColumnDefaultExprRouted(strings.ToLower(existing.OwnedBy.Table),
			int(existing.OwnedBy.Column), &defaultExprDef{ExprText: exprText, Expr: expr})
	}
	// Capture the owning scope BEFORE the remove: after dropping the old key the new name is in no
	// scope, so a post-remove route would wrongly default to the main image (temp-tables.md §8).
	isTemp := db.isTempSequence(existing.Name)
	def := *existing
	def.Name = newName
	var w *snapshot
	if isTemp {
		db.session.tx.tempDirty = true
		w = db.session.tx.tempWorking
	} else {
		w = db.working()
	}
	w.removeSequence(strings.ToLower(existing.Name))
	w.putSequence(&def)
	return nil
}

// indexEntryKey builds a secondary-index entry key (spec/design/indexes.md §3): each
// indexed column as the encoding.md §2.2 nullable slot — 0x00 + the type's bare
// order-preserving key bytes when present, the lone 0x01 for NULL (always tagged, even
// for a NOT NULL column) — then the row's storage key as the suffix. Indexable types are
// fixed-width and never spill, so the values are always resident (never unfetched).
func indexEntryKey(columns []catColumn, colls []*Collation, def indexDef, storageKey []byte, row storedRow) ([]byte, error) {
	var out []byte
	for _, ci := range def.Columns {
		v := row[ci]
		switch v.Kind {
		case ValNull:
			out = append(out, 0x01)
		case ValInt:
			out = append(out, 0x00)
			out = append(out, encodeInt(columns[ci].Type.ScalarTy(), v.Int)...)
		case ValBool:
			out = append(out, 0x00)
			out = append(out, encodeBool(v.boolVal())...)
		case ValUuid:
			out = append(out, 0x00)
			out = append(out, v.str()...)
		case ValTimestamp, ValTimestamptz, ValDate:
			out = append(out, 0x00)
			out = append(out, encodeInt(columns[ci].Type.ScalarTy(), v.Int)...)
		case ValText:
			// text: C terminated-escape (§2.4) or the collated UCA sort key (§2.12).
			b, err := collatedTextKey(colls[ci], v.str())
			if err != nil {
				return nil, err
			}
			out = append(out, 0x00)
			out = append(out, b...)
		case ValBytea:
			out = append(out, 0x00)
			out = append(out, encodeTerminated([]byte(v.str()))...)
		case ValDecimal:
			out = append(out, 0x00)
			out = append(out, v.decimal().EncodeKey()...)
		case ValInterval:
			out = append(out, 0x00)
			out = append(out, v.interval().EncodeKey()...)
		case ValFloat64:
			// float: the fixed-width float-order-preserving key (encoding.md §2.8).
			out = append(out, 0x00)
			out = append(out, encodeFloat64Key(uint64(v.Int))...)
		case ValFloat32:
			out = append(out, 0x00)
			out = append(out, encodeFloat32Key(uint32(v.Int))...)
		case ValRange:
			// the recursive range-bounds container key (encoding.md §2.11)
			out = append(out, 0x00)
			elem, _ := columns[ci].Type.RangeElement()
			out = append(out, encodeRangeKey(elem.ScalarTy(), v.rangeVal())...)
		case ValArray:
			// the recursive array-elements-terminated container key (encoding.md §2.14)
			out = append(out, 0x00)
			ab, err := encodeTypedKey(columns[ci].Type, v, nil)
			if err != nil {
				return nil, err
			}
			out = append(out, ab...)
		default:
			panic("an index column is a key-encodable type (CREATE INDEX gate)")
		}
	}
	out = append(out, storageKey...)
	return out, nil
}

// indexEntryKeys returns the index entries a row contributes (spec/design/gin.md §4/§5): exactly
// one for an ordered (B-tree) index — the §3 nullable-slot entry key — or one per DISTINCT non-NULL
// element for a GIN index. Every write path (build, INSERT, DELETE, UPDATE) treats an index
// uniformly as "a row maps to a set of entries." colls (column-ordinal-indexed) selects each text
// key column's collated form (§2.12); GIN elements are fixed-width, so a GIN index never collates.
func indexEntryKeys(columns []catColumn, colls []*Collation, def indexDef, storageKey []byte, row storedRow) ([][]byte, error) {
	if def.Kind == indexGin {
		return ginEntries(columns, def, storageKey, row), nil
	}
	if def.Kind == indexGist {
		return gistEntries(columns, def, storageKey, row), nil
	}
	ek, err := indexEntryKey(columns, colls, def, storageKey, row)
	if err != nil {
		return nil, err
	}
	return [][]byte{ek}, nil
}

// gistEntries builds a GiST index's entry keys for one row (spec/design/gist.md §4.1): exactly one
// leaf key, encodeRangeBody(bound) ‖ storage_key (the GIN term ‖ skey pattern), so all existing
// index maintenance (insert/update/delete) reuses it unchanged. A NULL range value is not indexed;
// the empty range is a real value and IS indexed.
func gistEntries(columns []catColumn, def indexDef, storageKey []byte, row storedRow) [][]byte {
	ops := make([]gistOpclass, len(def.Columns))
	bound := make([]gistBound, len(def.Columns))
	for i, ci := range def.Columns {
		col := columns[ci]
		v := row[ci]
		if v.Kind == ValNull {
			return nil // any NULL excluded column → row not indexed (the §7 NULL rule)
		}
		if rt, ok := col.Type.RangeElement(); ok {
			// range_ops: the row range's value-codec bytes.
			ops[i] = gistOpclass{scalar: false, elem: scalarColType(rt.Scalar)}
			bound[i] = gistBound{rng: v.rangeVal()}
			continue
		}
		// scalar `=` opclass: the value's order-preserving KEY bytes (gist.md §6). The column is a
		// FIXED-WIDTH keyable (the gate), so the key encoding is collation-free and infallible.
		k, err := encodeKeyValue(col.Type.ScalarTy(), v, nil)
		if err != nil {
			panic("a fixed-width GiST scalar key is infallible (no collation)")
		}
		ops[i] = gistOpclass{scalar: true}
		bound[i] = gistBound{smin: k, smax: k}
	}
	return [][]byte{gistLeafKey(ops, bound, storageKey)}
}

// exclusionProbeQuery builds a row's EXCLUDE conjunction probe (spec/design/gist.md §7): one GiST
// query operand + strategy per excluded column, in the backing index's column order. Returns ok=false
// (the row is EXEMPT, never conflicts) when the NULL rule fires (any excluded column is NULL) or when
// a && element holds the empty range (empty && anything is FALSE, so the conjunction can never be
// TRUE — this also sidesteps the empty-range overlap-descend trap, gist.md §5). The query is fed to
// the resident GiST tree's search, whose leaf recheck IS the full conjunction, so a hit is a conflict.
func exclusionProbeQuery(columns []catColumn, exc exclusionConstraint, row storedRow) ([]gistQuery, []gistStrategy, bool) {
	q := make([]gistQuery, 0, len(exc.Elements))
	strats := make([]gistStrategy, 0, len(exc.Elements))
	for _, el := range exc.Elements {
		ci := el.Column
		v := row[ci]
		if v.Kind == ValNull {
			return nil, nil, false // NULL rule: exempt
		}
		switch el.Op {
		case exclOverlaps:
			if v.rangeVal().Empty {
				return nil, nil, false // empty && anything is FALSE → exempt
			}
			q = append(q, gistQuery{rng: v.rangeVal()})
			strats = append(strats, gistOverlaps)
		case exclEqual:
			k, err := encodeKeyValue(columns[ci].Type.ScalarTy(), v, nil)
			if err != nil {
				panic("a fixed-width GiST scalar key is infallible (no collation)")
			}
			q = append(q, gistQuery{skey: k})
			strats = append(strats, gistEqual)
		}
	}
	return q, strats, true
}

// exclusionPairConflicts reports whether the (expr_i op_i) conjunction holds between two rows
// (spec/design/gist.md §7). Used for the in-batch new-row-vs-new-row check (the resident GiST tree
// holds only stored rows). A NULL in any excluded column of either row, or an empty range under &&
// (rangeOverlaps of an empty range is FALSE), makes that element not-TRUE → no conflict. Returns true
// only when EVERY element is definitely TRUE.
func exclusionPairConflicts(columns []catColumn, exc exclusionConstraint, a, b storedRow) bool {
	for _, el := range exc.Elements {
		ci := el.Column
		va, vb := a[ci], b[ci]
		if va.Kind == ValNull || vb.Kind == ValNull {
			return false
		}
		var ok bool
		switch el.Op {
		case exclOverlaps:
			ok = rangeOverlaps(va.rangeVal(), vb.rangeVal())
		case exclEqual:
			ka, err := encodeKeyValue(columns[ci].Type.ScalarTy(), va, nil)
			if err != nil {
				panic("a fixed-width GiST scalar key is infallible")
			}
			kb, err := encodeKeyValue(columns[ci].Type.ScalarTy(), vb, nil)
			if err != nil {
				panic("a fixed-width GiST scalar key is infallible")
			}
			ok = bytes.Equal(ka, kb)
		}
		if !ok {
			return false
		}
	}
	return true
}

// isGinElementType reports whether elem is an element type a GIN (array_ops) index admits —
// the integers, boolean, uuid, date, timestamp, timestamptz (spec/design/gin.md §3): a GIN term IS
// the element's order-preserving key encoding (§4) and a term carries no length/terminator framing,
// so only the FIXED-WIDTH keyables qualify. The variable-width keyables (text, bytea, decimal) —
// valid ordered-index / PK keys — are 0A000 here, as is float. interval is fixed-width keyable (its
// 16-byte span key landed, encoding.md §2.10) but its GIN element support is a separate follow-on
// slice (gin.md §3/§10), so it is not yet admitted here.
func isGinElementType(elem scalarType) bool {
	return elem.IsInteger() || elem.IsBool() || elem.IsUuid() ||
		elem.IsTimestamp() || elem.IsTimestamptz() || elem.IsDate()
}

// isGistScalarType reports whether the scalar `=` GiST opclass admits this column type (gist.md §6):
// the FIXED-WIDTH keyables — integers, boolean, uuid, date, timestamp, timestamptz — whose bound is
// [min,max] over the order-preserving key encoding, compared as raw bytes (no decode, no collation).
// Exactly isGinElementType's set, kept a separate predicate so the two surfaces evolve independently.
func isGistScalarType(ty dataType) bool {
	return ty.IsInteger() || ty.IsBool() || ty.IsUuid() ||
		ty.IsTimestamp() || ty.IsTimestamptz() || ty.IsDate()
}

// isGistDeferredScalarType reports a keyable scalar the GiST scalar `=` opclass will eventually admit
// but defers this slice (gist.md §6/§11): the VARIABLE-width / collation-sensitive keyables — text,
// bytea, decimal, interval. A column of one of these is 0A000 ("not supported yet"), not 42704.
func isGistDeferredScalarType(ty dataType) bool {
	return ty.IsText() || ty.IsBytea() || ty.IsDecimal() || ty.IsInterval()
}

// ginEntries builds a GIN index's entry keys for one row (spec/design/gin.md §4): one entry per
// DISTINCT non-NULL array element — encode(element) ‖ storage_key, NO presence tag (a term is never
// NULL) and an empty payload. A NULL array column value and an empty array yield no entries (so
// they appear in no posting list). Returned sorted by encoded term (= key-encoding byte order, which
// is order-preserving for every admitted element type). array_ops over any fixed-width key-encodable
// element type.
func ginEntries(columns []catColumn, def indexDef, storageKey []byte, row storedRow) [][]byte {
	ci := def.Columns[0]
	elemTy := columns[ci].Type.Array.ScalarTy()
	v := row[ci]
	if v.Kind != ValArray {
		return nil
	}
	// Dedup by the encoded term (the encoding is a bijection: byte-dedup == value-dedup, byte-sort
	// == value-sort) generically over every admitted element type.
	seen := make(map[string]bool)
	var terms [][]byte
	for _, el := range v.arrayVal().Elements {
		if el.Kind == ValNull {
			continue // a NULL element carries no term; a non-keyable element is impossible under the gate
		}
		// a GIN element is fixed-width (isGinElementType excludes text), so it never collates and
		// the key encoding is infallible.
		t, err := encodeKeyValue(elemTy, el, nil)
		if err != nil {
			panic("a GIN element key is infallible (fixed-width, no collation)")
		}
		if !seen[string(t)] {
			seen[string(t)] = true
			terms = append(terms, t)
		}
	}
	slices.SortFunc(terms, bytes.Compare)
	entries := make([][]byte, 0, len(terms))
	for _, t := range terms {
		entry := append(append([]byte{}, t...), storageKey...)
		entries = append(entries, entry)
	}
	return entries
}

// bytesDiff returns the entries in a that are not in b (set difference over byte slices),
// preserving a's order — the UPDATE symmetric-difference for GIN / B-tree maintenance (gin.md §5).
func bytesDiff(a, b [][]byte) [][]byte {
	var out [][]byte
	for _, x := range a {
		found := false
		for _, y := range b {
			if bytes.Equal(x, y) {
				found = true
				break
			}
		}
		if !found {
			out = append(out, x)
		}
	}
	return out
}

// indexPrefixKey builds a row's UNIQUENESS PROBE KEY for one unique index
// (spec/design/indexes.md §8): the §3 entry key's slot prefix — without the storage-key
// suffix — or ok=false when any component is NULL (NULLS DISTINCT: such a tuple never
// conflicts). Two rows conflict iff they yield the same prefix.
func indexPrefixKey(columns []catColumn, colls []*Collation, def indexDef, row storedRow) ([]byte, bool, error) {
	var out []byte
	for _, ci := range def.Columns {
		v := row[ci]
		switch v.Kind {
		case ValNull:
			return nil, false, nil
		case ValInt:
			out = append(out, 0x00)
			out = append(out, encodeInt(columns[ci].Type.ScalarTy(), v.Int)...)
		case ValBool:
			out = append(out, 0x00)
			out = append(out, encodeBool(v.boolVal())...)
		case ValUuid:
			out = append(out, 0x00)
			out = append(out, v.str()...)
		case ValTimestamp, ValTimestamptz, ValDate:
			out = append(out, 0x00)
			out = append(out, encodeInt(columns[ci].Type.ScalarTy(), v.Int)...)
		case ValText:
			// text: C terminated-escape (§2.4) or the collated UCA sort key (§2.12).
			b, err := collatedTextKey(colls[ci], v.str())
			if err != nil {
				return nil, false, err
			}
			out = append(out, 0x00)
			out = append(out, b...)
		case ValBytea:
			out = append(out, 0x00)
			out = append(out, encodeTerminated([]byte(v.str()))...)
		case ValDecimal:
			out = append(out, 0x00)
			out = append(out, v.decimal().EncodeKey()...)
		case ValInterval:
			out = append(out, 0x00)
			out = append(out, v.interval().EncodeKey()...)
		case ValFloat64:
			// float: the fixed-width float-order-preserving key (encoding.md §2.8).
			out = append(out, 0x00)
			out = append(out, encodeFloat64Key(uint64(v.Int))...)
		case ValFloat32:
			out = append(out, 0x00)
			out = append(out, encodeFloat32Key(uint32(v.Int))...)
		case ValRange:
			// the recursive range-bounds container key (encoding.md §2.11)
			out = append(out, 0x00)
			elem, _ := columns[ci].Type.RangeElement()
			out = append(out, encodeRangeKey(elem.ScalarTy(), v.rangeVal())...)
		case ValArray:
			// the recursive array-elements-terminated container key (encoding.md §2.14)
			out = append(out, 0x00)
			ab, err := encodeTypedKey(columns[ci].Type, v, nil)
			if err != nil {
				return nil, false, err
			}
			out = append(out, ab...)
		default:
			panic("an index column is a key-encodable type (CREATE INDEX gate)")
		}
	}
	return out, true, nil
}

// uniqueProbeBound is the half-open byte range [prefix, byte-successor(prefix)) — every
// index entry whose slot prefix equals prefix (the suffix makes tree keys unique, so
// equal prefixes sit adjacent). The uniqueness probes range over it (indexes.md §8).
func uniqueProbeBound(prefix []byte) keyBound {
	return keyBound{lo: prefix, loInc: true, hi: prefixSuccessor(prefix), hiInc: false}
}

// executeInsert analyzes and runs an INSERT whose rows come from a VALUES list or a SELECT
// (spec/design/grammar.md §12 / §24). An optional column list names the target columns (unknown
// → 42703, duplicate → 42701); an unlisted column, or a DEFAULT keyword slot, takes the column's
// stored default else NULL. Each value is type-checked (NULL into NOT NULL traps 23502; an integer
// outside the column type's range traps 22003 — CLAUDE.md §8); a duplicate primary key traps
// 23505. An INSERT is two-phase / all-or-nothing, mirroring UPDATE: every row is validated —
// including its storage key — before any row is inserted, so a mid-batch failure stores nothing.
// The two sources differ only in where the candidate rows come from and in cost: VALUES is zero
// (literals + constant defaults), SELECT is the embedded query's accrued cost. The SELECT source
// additionally validates output arity (42601) and per-column type assignability (42804) up front,
// before any row is produced — so both fire even over an empty source.
// encodePkKey is a row's PRIMARY-KEY STORAGE KEY (spec/design/encoding.md §2.3): the
// concatenation of the members' bare encodings in key order. Each component is either
// fixed-width or self-delimiting (text/bytea terminate, §2.4/§2.6), so the concatenation stays
// self-delimiting and bytes.Compare equals the tuple's logical order. Shared by the INSERT
// duplicate check and the ON CONFLICT arbiter probe (upsert.md §3); a PK column is NOT NULL, so
// there is no presence tag.
func encodePkKey(table *catTable, pk []int, colls []*Collation, row storedRow) ([]byte, error) {
	var key []byte
	for _, i := range pk {
		switch {
		case table.Columns[i].Type.IsUuid():
			// uuid: the bare 16 bytes (uuid-raw16, encoding.md §2.7).
			key = append(key, row[i].str()...)
		case table.Columns[i].Type.IsBool():
			// boolean: the bare 1-byte bool-byte (encoding.md §2.9).
			key = append(key, encodeBool(row[i].boolVal())...)
		case table.Columns[i].Type.IsText():
			// text: the C …-terminated-escape body (encoding.md §2.4), or the collation's UCA
			// sort key for a non-C collated column (text-collated-sortkey, §2.12).
			b, err := collatedTextKey(colls[i], row[i].str())
			if err != nil {
				return nil, err
			}
			key = append(key, b...)
		case table.Columns[i].Type.IsBytea():
			// bytea: the variable-width bytea-terminated-escape body (encoding.md §2.6).
			key = append(key, encodeTerminated([]byte(row[i].str()))...)
		case table.Columns[i].Type.IsDecimal():
			// decimal: the variable-width decimal-order-preserving body (encoding.md §2.5).
			key = append(key, row[i].decimal().EncodeKey()...)
		case table.Columns[i].Type.IsInterval():
			// interval: the fixed 16-byte interval-span-i128 span key (encoding.md §2.10).
			key = append(key, row[i].interval().EncodeKey()...)
		case table.Columns[i].Type.IsRange():
			// range: the recursive range-bounds container key (encoding.md §2.11, the first
			// container key — empty/±∞/inclusivity framing around the element key).
			elem, _ := table.Columns[i].Type.RangeElement()
			key = append(key, encodeRangeKey(elem.ScalarTy(), row[i].rangeVal())...)
		case table.Columns[i].Type.IsArray():
			// array: the recursive array-elements-terminated container key (encoding.md §2.14, the
			// second container key — element markers + terminator + shape suffix).
			b, err := encodeArrayKey(table.Columns[i].Type.Array.ScalarTy(), row[i].arrayVal())
			if err != nil {
				return nil, err
			}
			key = append(key, b...)
		case table.Columns[i].Type.IsFloat():
			// float: the fixed-width float-order-preserving key (encoding.md §2.8) — NOT the integer
			// codec (the float bits do not sort numerically as an int).
			if table.Columns[i].Type.ScalarTy() == scalarFloat32 {
				key = append(key, encodeFloat32Key(uint32(row[i].Int))...)
			} else {
				key = append(key, encodeFloat64Key(uint64(row[i].Int))...)
			}
		default:
			// integers / timestamp / timestamptz / date: the fixed-width key codec.
			key = append(key, encodeInt(table.Columns[i].Type.ScalarTy(), row[i].Int)...)
		}
	}
	return key, nil
}

// arbiter is which uniqueness constraint an ON CONFLICT arbitrates (spec/design/upsert.md §2):
// the primary key (isPK), or a unique index by position in table.Indexes (indexPos).
type arbiter struct {
	isPK     bool
	indexPos int
}

// conflictPlan is a resolved ON CONFLICT clause (spec/design/upsert.md), built by resolveOnConflict.
type conflictPlan struct {
	// arb is the arbiter constraint; nil = no target (legal only with DO NOTHING — any
	// uniqueness conflict is then skipped).
	arb *arbiter
	// doUpdate true = DO UPDATE (assignments + filter); false = DO NOTHING.
	doUpdate    bool
	assignments []assignPlan
	filter      *rExpr
}

// resolveArbiter resolves an ON CONFLICT target into an *arbiter (spec/design/upsert.md §2): a
// column list is matched as an order-independent SET against a unique index / the primary key (no
// match → 42P10); ON CONSTRAINT name names a unique index or the synthesized <table>_pkey (miss →
// 42704). A nil target → nil arbiter (legal only with DO NOTHING).
func resolveArbiter(table *catTable, target *conflictTarget) (*arbiter, error) {
	if target == nil {
		return nil, nil
	}
	pk := table.PKIndices()
	if !target.IsConstraint {
		want := make(map[int]struct{}, len(target.Columns))
		for _, c := range target.Columns {
			idx := table.ColumnIndex(c)
			if idx < 0 {
				return nil, newError(UndefinedColumn, "column does not exist: "+c)
			}
			want[idx] = struct{}{}
		}
		if len(pk) > 0 && sameIntSet(pk, want) {
			return &arbiter{isPK: true}, nil
		}
		for i, def := range table.Indexes {
			if def.Unique && sameIntSet(def.Columns, want) {
				return &arbiter{indexPos: i}, nil
			}
		}
		return nil, newError(InvalidColumnReference,
			"there is no unique or exclusion constraint matching the ON CONFLICT specification")
	}
	pkey := strings.ToLower(table.Name) + "_pkey"
	if len(pk) > 0 && strings.EqualFold(target.Constraint, pkey) {
		return &arbiter{isPK: true}, nil
	}
	for i, def := range table.Indexes {
		if def.Unique && strings.EqualFold(def.Name, target.Constraint) {
			return &arbiter{indexPos: i}, nil
		}
	}
	return nil, newError(UndefinedObject, fmt.Sprintf(
		"constraint %s for table %s does not exist", target.Constraint, table.Name,
	))
}

// sameIntSet reports whether the slice's values (as a set) equal the given set.
func sameIntSet(s []int, set map[int]struct{}) bool {
	seen := make(map[int]struct{}, len(s))
	for _, v := range s {
		seen[v] = struct{}{}
	}
	if len(seen) != len(set) {
		return false
	}
	for v := range seen {
		if _, ok := set[v]; !ok {
			return false
		}
	}
	return true
}

// arbiterKey is the arbiter key of a candidate row (spec/design/upsert.md §3): the storage key for
// a PK arbiter (never NULL), or the unique-index prefix for an index arbiter (the bool is false
// when a nullable arbiter column is NULL — NULLS DISTINCT, so the row never conflicts).
func arbiterKey(arb *arbiter, table *catTable, pk []int, colls []*Collation, row storedRow) ([]byte, bool, error) {
	if arb.isPK {
		k, err := encodePkKey(table, pk, colls, row)
		if err != nil {
			return nil, false, err
		}
		return k, true, nil
	}
	return indexPrefixKey(table.Columns, colls, table.Indexes[arb.indexPos], row)
}

// resolveOnConflict resolves an ON CONFLICT clause (spec/design/upsert.md §2/§5) into a
// conflictPlan: the arbiter, plus — for DO UPDATE — the resolved SET assignment plans and the
// optional WHERE filter, both resolved against the [existing | excluded] scope. Threads the
// statement ptypes so a $N in a SET/WHERE unifies with the rest of the INSERT.
func (db *engine) resolveOnConflict(table *catTable, oc *onConflict, ptypes *paramTypes) (*conflictPlan, error) {
	arb, err := resolveArbiter(table, oc.Target)
	if err != nil {
		return nil, err
	}
	if !oc.DoUpdate {
		return &conflictPlan{arb: arb, doUpdate: false}, nil
	}
	// DO UPDATE requires a target (spec/design/upsert.md §2) — PostgreSQL's message.
	if arb == nil {
		return nil, newError(SyntaxError,
			"ON CONFLICT DO UPDATE requires inference specification or constraint name")
	}
	s := onConflictExcludedScope(db, table)
	pkMembers := table.PKIndices()
	plans := make([]assignPlan, 0, len(oc.Assignments))
	for _, a := range oc.Assignments {
		idx := table.ColumnIndex(a.Column)
		if idx < 0 {
			return nil, newError(UndefinedColumn, "column does not exist: "+a.Column)
		}
		if c := table.Columns[idx].Identity; c != nil && *c == identityAlways {
			return nil, newError(GeneratedAlways,
				fmt.Sprintf("column %s can only be updated to DEFAULT", a.Column))
		}
		// Assigning a PRIMARY KEY member in DO UPDATE remains deferred (0A000, upsert.md §5/§9):
		// the standalone UPDATE re-keying has landed (§11 step 6), but extending it to the upsert
		// conflict path is a separate follow-on.
		if slices.Contains(pkMembers, idx) {
			return nil, newError(FeatureNotSupported, "updating a primary key column is not supported")
		}
		for _, p := range plans {
			if p.idx == idx {
				return nil, newError(DuplicateColumn, "column "+a.Column+" assigned more than once")
			}
		}
		col := table.Columns[idx]
		// Updating a non-scalar column (composite / range / array) on the ON CONFLICT DO UPDATE path
		// is deferred (0A000): standalone UPDATE of a range/array column has landed, but extending the
		// conflict-action path to non-scalar columns is a separate follow-on (upsert.md §9).
		if _, ok := col.Type.AsScalar(); !ok {
			noun := "composite"
			switch {
			case col.Type.IsRange():
				noun = "range"
			case col.Type.IsArray():
				noun = "array"
			}
			return nil, newError(FeatureNotSupported,
				"updating "+noun+" column "+a.Column+" is not supported yet")
		}
		colScalar := col.Type.ScalarTy()
		src, ty, err := resolve(s, a.Value, &colScalar, &aggCtx{collecting: false}, ptypes)
		if err != nil {
			return nil, err
		}
		if err := requireAssignable(ty, colScalar, a.Column); err != nil {
			return nil, err
		}
		plans = append(plans, assignPlan{
			idx: idx, name: col.Name, target: colScalar, decimal: col.Decimal, varcharLen: col.VarcharLen, notNull: col.NotNull, source: src,
		})
	}
	var filter *rExpr
	if oc.Filter != nil {
		f, err := resolveBooleanFilter(s, oc.Filter, ptypes)
		if err != nil {
			return nil, err
		}
		filter = f
	}
	return &conflictPlan{arb: arb, doUpdate: true, assignments: plans, filter: filter}, nil
}

// arbiterExisting looks up the EXISTING (committed) conflicting row for an arbiter key
// (spec/design/upsert.md §3): always a committed row (an in-batch row sharing the arbiter key was
// caught earlier by the proposed-arbiter set). Returns (storageKey, fully-resident row, found).
func (db *engine) arbiterExisting(arb *arbiter, store *tableStore, table *catTable, ak []byte) ([]byte, storedRow, bool, error) {
	if arb.isPK {
		row, exists, err := store.Get(ak)
		if err != nil || !exists {
			return nil, nil, false, err
		}
		row, err = store.resolveAll(row)
		if err != nil {
			return nil, nil, false, err
		}
		return ak, row, true, nil
	}
	def := table.Indexes[arb.indexPos]
	istore := db.lkpIndexStore(strings.ToLower(def.Name))
	entries, err := istore.RangeEntries(uniqueProbeBound(ak))
	if err != nil {
		return nil, nil, false, err
	}
	if len(entries) == 0 {
		return nil, nil, false, nil
	}
	suffix := append([]byte(nil), entries[0].Key[len(ak):]...)
	row, exists, err := store.Get(suffix)
	if err != nil {
		return nil, nil, false, err
	}
	if !exists {
		panic("a unique-index entry points at a live row")
	}
	row, err = store.resolveAll(row)
	if err != nil {
		return nil, nil, false, err
	}
	return suffix, row, true, nil
}

// rowConflictsCommitted reports whether a candidate row conflicts with a COMMITTED row on the
// primary key or any unique index (the no-target DO NOTHING skip test — spec/design/upsert.md §2).
// NULLS DISTINCT: a unique tuple with any NULL component never conflicts.
func (db *engine) rowConflictsCommitted(store *tableStore, table *catTable, pk []int, colls []*Collation, row storedRow) (bool, error) {
	if len(pk) > 0 {
		k, err := encodePkKey(table, pk, colls, row)
		if err != nil {
			return false, err
		}
		if _, exists, err := store.Get(k); err != nil {
			return false, err
		} else if exists {
			return true, nil
		}
	}
	for _, def := range table.Indexes {
		if !def.Unique {
			continue
		}
		prefix, ok, err := indexPrefixKey(table.Columns, colls, def, row)
		if err != nil {
			return false, err
		}
		if !ok {
			continue
		}
		entries, err := db.lkpIndexStore(strings.ToLower(def.Name)).RangeEntries(uniqueProbeBound(prefix))
		if err != nil {
			return false, err
		}
		if len(entries) > 0 {
			return true, nil
		}
	}
	return false, nil
}

func (db *engine) executeInsert(ins *insert, params []Value, ctx cteCtx) (outcome, error) {
	// A catalog relation is read-only (introspection.md §5): a DML target naming one is 42809,
	// checked by NAME before qualifier validation (the built-in resolves in every database).
	if err := checkCatalogRelWrite(ins.Table); err != nil {
		return outcome{}, err
	}
	// A write to a READ-ONLY host attachment is 25006 before any I/O — checked BEFORE the qualifier
	// existence gate so a read-only attachment refuses the write deterministically (attached-databases.md §4).
	if err := db.checkAttachmentWritable(ins.DB); err != nil {
		return outcome{}, err
	}
	if err := db.checkTableQualifier(ins.DB, ins.Table); err != nil { // attached-databases.md §3
		return outcome{}, err
	}
	// ON CONFLICT into a host attachment is a deferred narrowing this slice (attached-databases.md §8):
	// the conflict path resolves index stores unscoped. A clean 0A000 before any planning.
	if ins.OnConflict != nil && isAttachmentScope(ins.DB) {
		return outcome{}, newError(FeatureNotSupported, "ON CONFLICT on an attached-database table is not supported yet")
	}
	table, ok := db.lkpTableScoped(ins.DB, ins.Table) // scope-aware temp-first (temp-tables.md §3)
	if !ok {
		return outcome{}, newError(UndefinedTable, "table does not exist: "+ins.Table)
	}
	// Refuse the write if any of this table's collated keys are version-skewed (slice 2d): a
	// maintained B-tree would mix two orderings (collation.md §12, XX002).
	if err := db.ensureCollationsWritable(table.Columns); err != nil {
		return outcome{}, err
	}
	store := db.writeStoreScoped(ins.DB, ins.Table) // routes a temp / attachment INSERT to its working snapshot
	// The key members in key order — one for a single-column PK, several for a composite
	// (constraints.md §3), empty for a no-PK (rowid) table.
	pk := table.PKIndices()
	// The CHECK constraints, resolved once per statement in evaluation (name) order;
	// insertRows evaluates them per candidate row (constraints.md §4.4).
	checks, err := db.resolveChecks(table)
	if err != nil {
		return outcome{}, err
	}
	// Each column's EXPRESSION default, resolved once per statement (constraints.md §2);
	// applied per omitted column / DEFAULT slot, sharing one per-statement StmtRng.
	defaultExprs, err := db.resolveDefaultExprs(table)
	if err != nil {
		return outcome{}, err
	}
	stmtRng := newStmtRng()

	// Resolve the optional column list once. provided[i] >= 0 means table column i takes that
	// value position in each row; -1 means column i is omitted (its default, else NULL). With no
	// list it is the identity over all columns. arity is how many values each row must carry (for
	// a SELECT source, how many columns it must project).
	n := len(table.Columns)
	provided := make([]int, n)
	arity := n
	if ins.Columns != nil {
		for i := range provided {
			provided[i] = -1
		}
		for p, name := range ins.Columns {
			idx := table.ColumnIndex(name)
			if idx < 0 {
				return outcome{}, newError(UndefinedColumn, fmt.Sprintf(
					"column %s of relation %s does not exist", name, table.Name,
				))
			}
			if provided[idx] >= 0 {
				return outcome{}, newError(DuplicateColumn,
					"column "+table.Columns[idx].Name+" specified more than once")
			}
			provided[idx] = p
		}
		arity = len(ins.Columns)
	} else {
		for i := range provided {
			provided[i] = i
		}
	}

	// IDENTITY column handling (spec/design/sequences.md §13). OVERRIDING USER VALUE discards any
	// supplied value for every identity column and uses its sequence instead — modeled by treating
	// the column as omitted (provided[i] = -1, so its nextval default applies). Apply it before the
	// GENERATED ALWAYS gate below so a User-overridden ALWAYS column needs no further check.
	if ins.Overriding != nil && *ins.Overriding == overridingUser {
		for i, col := range table.Columns {
			if col.Identity != nil {
				provided[i] = -1
			}
		}
	}
	// The GENERATED ALWAYS columns still explicitly targeted (and not OVERRIDING SYSTEM VALUE):
	// supplying a non-DEFAULT value to one is 428C9. Collected as (column ordinal, value position)
	// so the source branches can enforce it (VALUES per-row, SELECT up-front).
	type alwaysTarget struct{ col, pos int }
	var alwaysTargeted []alwaysTarget
	if !(ins.Overriding != nil && *ins.Overriding == overridingSystem) {
		for i, col := range table.Columns {
			if col.Identity != nil && *col.Identity == identityAlways && provided[i] >= 0 {
				alwaysTargeted = append(alwaysTargeted, alwaysTarget{col: i, pos: provided[i]})
			}
		}
	}

	if ins.Select != nil {
		// GENERATED ALWAYS gate (sequences.md §13.3): a SELECT projection always supplies an
		// explicit value, so targeting an ALWAYS identity column without OVERRIDING SYSTEM VALUE is
		// 428C9 — raised up front (PG raises it at rewrite), firing even over a zero-row source.
		if len(alwaysTargeted) > 0 {
			return outcome{}, newError(GeneratedAlways, fmt.Sprintf(
				"cannot insert a non-DEFAULT value into column %s", table.Columns[alwaysTargeted[0].col].Name,
			))
		}
		// SELECT source (§24). Plan the source query, then resolve the RETURNING projection
		// (PostgreSQL's analysis order — both precede any execution), threading ONE paramTypes
		// so a $N shared by the source and the RETURNING list unifies statement-wide (api.md
		// §5). The source returns OWNED rows, so a self-insert (INSERT INTO t SELECT ... FROM
		// t) reads the pre-insert snapshot, then writes.
		// The source query (and the RETURNING sublinks) see the statement's CTE bindings
		// (writable-cte.md) — the move-rows idiom INSERTs a SELECT over a CTE buffer.
		ptypes := &paramTypes{}
		plan, err := db.planQuery(queryExpr{Select: ins.Select}, nil, ctx.bindings, ptypes)
		if err != nil {
			return outcome{}, err
		}
		var retNodes []*rExpr
		var retNames []string
		var retTypes []string
		if ins.Returning != nil {
			if retNodes, retNames, retTypes, err = db.resolveReturning(table, *ins.Returning, false, ctx.bindings, ptypes); err != nil {
				return outcome{}, err
			}
		}
		var cplan *conflictPlan
		if ins.OnConflict != nil {
			if cplan, err = db.resolveOnConflict(table, ins.OnConflict, ptypes); err != nil {
				return outcome{}, err
			}
		}
		ptys, err := ptypes.finalize()
		if err != nil {
			return outcome{}, err
		}
		bound, err := bindParams(params, ptys)
		if err != nil {
			return outcome{}, err
		}
		meter := db.session.newMeter()
		if err := db.foldUncorrelatedInPlan(&plan, bound, ctx, &meter.Accrued); err != nil {
			return outcome{}, err
		}
		// Uncorrelated subqueries in the RETURNING list fold once (cost.md §3), reading the
		// pre-statement snapshot (grammar.md §32). They see the statement's CTE bindings
		// (writable-cte.md) via ctx.
		for _, node := range retNodes {
			if err := db.foldUncorrelatedInRExpr(node, bound, ctx, &meter.Accrued); err != nil {
				return outcome{}, err
			}
		}
		if err := db.foldConflictPlan(cplan, bound, &meter.Accrued); err != nil {
			return outcome{}, err
		}
		q, err := db.execQueryPlan(&plan, nil, bound, ctx)
		if err != nil {
			return outcome{}, err
		}
		// Arity: the SELECT's output column count must match the target — checked before any
		// row is produced, so it fires even when the source returns zero rows.
		if len(q.columnNames) != arity {
			noun := "columns"
			if arity == 1 {
				noun = "column"
			}
			return outcome{}, newError(SyntaxError, fmt.Sprintf(
				"INSERT into table %s has %d target %s but SELECT produces %d",
				table.Name, arity, noun, len(q.columnNames),
			))
		}
		// Type-assignability, the up-front PostgreSQL gate (§24): each projected column's TYPE
		// must be assignable to its target column. Fires even at zero rows (this is the difference
		// from per-row checking). The per-row storeValue in insertRows then still range-checks
		// values (22003) and enforces NOT NULL.
		for i, col := range table.Columns {
			if p := provided[i]; p >= 0 {
				// INSERT ... SELECT into a composite column lands in a later slice (the VALUES +
				// ROW(...) path is S3 — spec/design/composite.md §12).
				if col.Type.IsComposite() {
					return outcome{}, newError(FeatureNotSupported, fmt.Sprintf(
						"INSERT ... SELECT into composite column %s is not supported yet", col.Name,
					))
				}
				// INSERT ... SELECT into a range column is deferred (the VALUES + range literal/cast
				// path is the supported input — spec/design/ranges.md §1).
				if col.Type.IsRange() {
					return outcome{}, newError(FeatureNotSupported, fmt.Sprintf(
						"INSERT ... SELECT into range column %s is not supported yet", col.Name,
					))
				}
				if !assignableTo(q.columnTypes[p], col.Type.ScalarTy()) {
					return outcome{}, typeError(fmt.Sprintf(
						"column %s is of type %s but expression is of type %s",
						col.Name, col.Type.CanonicalName(), rtName(q.columnTypes[p]),
					))
				}
			}
		}
		// Cost = the embedded SELECT's accrued cost (§24) plus the disposition plan's
		// compression attempts for over-RECORD_MAX rows (value_compress, cost.md §3) plus the
		// RETURNING projection; storing the rows themselves stays unmetered. One meter keeps
		// one ceiling over the whole statement.
		meter.Charge(q.cost)
		affected, returned, err := db.runInsertRows(table, store, ins.DB, pk, checks, defaultExprs, stmtRng, provided, q.rows, cplan, retNodes, bound, ctx, meter)
		if err != nil {
			return outcome{}, err
		}
		return dmlOutcome(retNames, retTypes, returned, affected, meter.Accrued), nil
	}

	// VALUES source. A $N in a VALUES slot is typed as its TARGET COLUMN's type. Collect those
	// types across every row (a $N reused under two columns unifies; spec/design/api.md §5), then
	// bind the supplied values up front so a bad bind fails before any row is stored.
	ptypes := &paramTypes{}
	for _, values := range ins.Rows {
		if len(values) != arity {
			expected := "columns are"
			if ins.Columns != nil {
				expected = "target columns are"
			}
			return outcome{}, newError(SyntaxError, fmt.Sprintf(
				"INSERT row has %d values but %d %s expected for table %s",
				len(values), arity, expected, table.Name,
			))
		}
		for i, col := range table.Columns {
			if p := provided[i]; p >= 0 && p < len(values) {
				// Only a scalar column gives a top-level $N an inferable type; a composite-column
				// param stays untyped (42P18 at finalize this slice — composite.md §12).
				if iv := values[p]; iv.IsParam && !col.Type.IsComposite() {
					ct := col.Type.ScalarTy()
					if err := ptypes.note(int(iv.Param)-1, &ct); err != nil {
						return outcome{}, err
					}
				}
			}
		}
	}
	// GENERATED ALWAYS gate (sequences.md §13.3): an explicit (non-DEFAULT) value targeting an
	// ALWAYS identity column without OVERRIDING SYSTEM VALUE is 428C9. Statement-level — fires
	// before any row is materialized; an all-DEFAULT column is fine. Arity is validated above, so
	// values[pos] is in range.
	for _, at := range alwaysTargeted {
		nonDefault := false
		for _, values := range ins.Rows {
			if !values[at.pos].IsDefault {
				nonDefault = true
				break
			}
		}
		if nonDefault {
			return outcome{}, newError(GeneratedAlways, fmt.Sprintf(
				"cannot insert a non-DEFAULT value into column %s", table.Columns[at.col].Name,
			))
		}
	}
	// Resolve the RETURNING projection after the source (PostgreSQL's analysis order) and
	// before binding/execution — a 42703 here beats a would-be 23505 (grammar.md §32).
	var retNodes []*rExpr
	var retNames []string
	var retTypes []string
	if ins.Returning != nil {
		var rerr error
		if retNodes, retNames, retTypes, rerr = db.resolveReturning(table, *ins.Returning, false, ctx.bindings, ptypes); rerr != nil {
			return outcome{}, rerr
		}
	}
	var cplan *conflictPlan
	if ins.OnConflict != nil {
		var cerr error
		if cplan, cerr = db.resolveOnConflict(table, ins.OnConflict, ptypes); cerr != nil {
			return outcome{}, cerr
		}
	}
	ptys, err := ptypes.finalize()
	if err != nil {
		return outcome{}, err
	}
	bound, err := bindParams(params, ptys)
	if err != nil {
		return outcome{}, err
	}

	// INSERT ... VALUES reads no rows; with only literal values and constant defaults it
	// evaluates no expression tree (leaves), so a plain fully-inline insert still costs zero. An
	// EXPRESSION default (DEFAULT uuidv7()) evaluates a tree per application — operator_eval per
	// node — the documented exception (constraints.md §2, like CHECK). Other metered work: the
	// disposition plan's compression attempts for over-RECORD_MAX rows (value_compress) and the
	// RETURNING projection. The meter is created here (before materialization) so a
	// DEFAULT-keyword expression default charges it too.
	meter := db.session.newMeter()

	// Materialize each row into its value-position-indexed candidates (length arity, checked
	// above) resolving each slot: a literal, a bound $N, or a DEFAULT keyword → that column's
	// default (a constant, or its expression evaluated for this row through the shared stmtRng).
	// The shared insertRows then builds the declaration-order row and applies OMITTED defaults.
	rows := make([][]Value, 0, len(ins.Rows))
	for _, values := range ins.Rows {
		rv := make([]Value, arity)
		for i, col := range table.Columns {
			if p := provided[i]; p >= 0 {
				iv := values[p]
				if iv.IsDefault {
					// DEFAULT at the top level → the column's default (constant or per-row expression).
					dv, err := db.evalDefault(col, defaultExprs[i], stmtRng, meter)
					if err != nil {
						return outcome{}, err
					}
					rv[p] = dv
				} else {
					// A ROW(...) / literal / $N slot is materialized against the column's resolved type
					// (composite-aware — spec/design/composite.md §1/§4).
					mv, err := materializeInsertValue(iv, store.colTypes[i], bound)
					if err != nil {
						return outcome{}, err
					}
					rv[p] = mv
				}
			}
		}
		rows = append(rows, rv)
	}
	// Uncorrelated subqueries in the RETURNING list fold once (cost.md §3), reading the
	// pre-statement snapshot (grammar.md §32). They see the statement's CTE bindings via ctx.
	for _, node := range retNodes {
		if err := db.foldUncorrelatedInRExpr(node, bound, ctx, &meter.Accrued); err != nil {
			return outcome{}, err
		}
	}
	if err := db.foldConflictPlan(cplan, bound, &meter.Accrued); err != nil {
		return outcome{}, err
	}
	affected, returned, err := db.runInsertRows(table, store, ins.DB, pk, checks, defaultExprs, stmtRng, provided, rows, cplan, retNodes, bound, ctx, meter)
	if err != nil {
		return outcome{}, err
	}
	return dmlOutcome(retNames, retTypes, returned, affected, meter.Accrued), nil
}

// insertRows runs phase 1 + phase 2 of an INSERT, shared by the VALUES and SELECT sources. Each
// element of rows is one row's candidate values indexed by VALUE POSITION p (length arity); the
// declaration-order stored row is built via provided (an omitted column takes its default else
// NULL) and each value is type-coerced + range-checked by storeValue (23502 / 22003 / 22P02 /
// 42804). The storage key is computed and checked for a duplicate (23505 — within this batch via
// seenKeys AND against the store) BEFORE any row is written; only once every row validates are
// they all inserted (phase 2), allocating a fresh monotonic rowid in row order for a no-PK table.
// All-or-nothing: a failure leaves the store untouched and burns no rowids.
//
// returning is the resolved RETURNING projection (grammar.md §32), evaluated over the
// validated rows after every check passes and BEFORE phase 2 writes — so its subqueries
// observe the pre-statement snapshot and a ceiling abort stays all-or-nothing; params feeds
// its $Ns. Returns the projected output rows, nil without a clause.
func (db *engine) insertRows(table *catTable, store *tableStore, dbScope *string, pk []int, checks []namedCheck, defaultExprs []*rExpr, rng *stmtRng, provided []int, rows [][]Value, returning []*rExpr, params []Value, ctes cteCtx, meter *costMeter) ([][]Value, error) {
	n := len(table.Columns)
	// Per-column frozen collations for the collated text key form (§2.12), resolved before any
	// mutation; nil everywhere for a C-only / non-text table (the fast path).
	colls := db.columnCollations(table.Columns)
	type preparedRow struct {
		key []byte // nil for a no-PK table (rowid allocated in phase 2)
		row storedRow
	}
	prepared := make([]preparedRow, 0, len(rows))
	seenKeys := make(map[string]struct{})
	// Per UNIQUE index (catalog/name order), the prefixes earlier rows of this batch
	// claimed — an in-batch duplicate traps 23505 like a stored one (indexes.md §8).
	var uniqDefs []indexDef
	for _, def := range table.Indexes {
		if def.Unique {
			uniqDefs = append(uniqDefs, def)
		}
	}
	seenPrefixes := make([]map[string]struct{}, len(uniqDefs))
	for i := range seenPrefixes {
		seenPrefixes[i] = make(map[string]struct{})
	}
	var cunits int64
	for _, values := range rows {
		row := make(storedRow, n)
		for i, col := range table.Columns {
			var candidate Value
			if p := provided[i]; p >= 0 {
				candidate = values[p]
			} else {
				// An omitted column takes its default — a constant, or its expression
				// evaluated for this row through the shared per-statement seam/clock
				// (constraints.md §2). evalDefault charges operator_eval for an expression
				// default; a constant (or no default → NULL) is free.
				dv, err := db.evalDefault(col, defaultExprs[i], rng, meter)
				if err != nil {
					return nil, err
				}
				candidate = dv
			}
			// The columns' resolved ColTypes (a scalar, or a composite resolved to its field tree),
			// for composite-aware store coercion (spec/design/composite.md §4).
			v, err := coerceForStore(candidate, store.colTypes[i], col.Decimal, col.VarcharLen, col.NotNull, col.Name)
			if err != nil {
				return nil, err
			}
			row[i] = v
		}

		// CHECK constraints, in name order, on the fully-coerced candidate row — after NOT
		// NULL (storeValue above), before the key/duplicate check (PG's per-row order,
		// constraints.md §4.4). TRUE and NULL pass; the first FALSE aborts the whole
		// statement (two-phase — nothing has been written). Evaluation is metered
		// expression work (operator_eval), so guard the ceiling per checked row. The
		// per-statement rng is shared with the default evaluation above (one StmtRng).
		if len(checks) > 0 {
			if err := meter.Guard(); err != nil {
				return nil, err
			}
			env := &evalEnv{exec: db, rng: rng}
			if err := evalChecks(checks, table.Name, row, env, meter); err != nil {
				return nil, err
			}
		}

		var key []byte
		if len(pk) > 0 {
			// The composite key is the concatenation of the members' bare encodings in key
			// order (encoding.md §2.3 — encodePkKey); a single-column key is the one-member
			// case of the same rule.
			k, err := encodePkKey(table, pk, colls, row)
			if err != nil {
				return nil, err
			}
			key = k
			// The PK's 23505 reports PostgreSQL's derived auto-name for the PK index,
			// `<table>_pkey` — jed persists/reserves no such relation (constraints.md §5.4).
			if _, dup := seenKeys[string(key)]; dup {
				return nil, newError(UniqueViolation,
					"duplicate key value violates unique constraint: "+strings.ToLower(table.Name)+"_pkey")
			}
			// The duplicate probe reads the pin (readSnap) — under the writable-CTE read pin
			// (writable-cte.md §2) it sees the PRE-statement table, not an earlier sub-statement's
			// staged rows; a cross-sub-statement key collision is caught in phase 2 below instead.
			// readSnap == working for an ordinary INSERT, so this is unchanged there.
			if _, exists, err := db.lkpStoreScoped(dbScope, table.Name).Get(key); err != nil {
				return nil, err
			} else if exists {
				return nil, newError(UniqueViolation,
					"duplicate key value violates unique constraint: "+strings.ToLower(table.Name)+"_pkey")
			}
			seenKeys[string(key)] = struct{}{}
		}
		// UNIQUE-index probes (indexes.md §8), AFTER the primary-key duplicate check (PG
		// reports the PK first when both are violated — probed): per unique index in
		// catalog (name) order, a fully-non-NULL key tuple (its slot prefix) must match no
		// existing entry and no earlier row of this batch. Unmetered validation, like the
		// PK duplicate check (cost.md §3).
		for u, def := range uniqDefs {
			prefix, ok, err := indexPrefixKey(table.Columns, colls, def, row)
			if err != nil {
				return nil, err
			}
			if !ok {
				continue
			}
			istore := db.lkpIndexStoreScoped(dbScope, strings.ToLower(def.Name))
			stored, err := istore.RangeEntries(uniqueProbeBound(prefix))
			if err != nil {
				return nil, err
			}
			if _, dup := seenPrefixes[u][string(prefix)]; dup || len(stored) > 0 {
				return nil, newError(UniqueViolation,
					"duplicate key value violates unique constraint: "+def.Name)
			}
			seenPrefixes[u][string(prefix)] = struct{}{}
		}
		// Meter the row's disposition-plan compression attempts (value_compress, cost.md §3).
		// For a no-PK table the synthetic rowid is allocated in phase 2; only the key LENGTH
		// feeds the plan, so an 8-byte placeholder stands in deterministically.
		kb := key
		if kb == nil {
			kb = make([]byte, 8)
		}
		cunits += int64(store.WriteCompressUnits(kb, row))
		prepared = append(prepared, preparedRow{key: key, row: row})
	}

	// FOREIGN KEY existence (constraints.md §6.4) — after all candidate rows are prepared, so the
	// check sees the statement's batch END STATE (a later row may supply an earlier row's parent
	// key; a self-reference resolves within the batch — PG's end-of-statement semantics). Unmetered
	// validation, like the PK/UNIQUE probes, and before any write (all-or-nothing). MATCH SIMPLE: a
	// row with any NULL local column is exempt.
	relation := table.Name
	for fki := range table.ForeignKeys {
		fk := &table.ForeignKeys[fki]
		// The parent exists (validated at CREATE TABLE; DROP TABLE refuses to drop a referenced
		// table — §6.10), so a consistent catalog always finds it.
		parent, ok := db.Table(fk.RefTable)
		if !ok {
			continue
		}
		// The probe matches the parent's stored key, so a collated parent key column uses the
		// PARENT's collation (§2.12).
		parentColls := db.columnCollations(parent.Columns)
		// Only a self-reference can satisfy against this statement's batch (a different parent
		// table is unchanged by this INSERT). Collect the parent keys the batch supplies.
		batch := make(map[string]struct{})
		if strings.EqualFold(fk.RefTable, relation) {
			for _, pr := range prepared {
				probe, ok, err := buildFkProbe(fk, parent, parentColls, pr.row, fk.RefColumns)
				if err != nil {
					return nil, err
				}
				if ok {
					batch[string(probe.bytes)] = struct{}{}
				}
			}
		}
		for _, pr := range prepared {
			probe, ok, err := buildFkProbe(fk, parent, parentColls, pr.row, fk.Columns)
			if err != nil {
				return nil, err
			}
			if !ok {
				continue // a NULL local column → exempt (MATCH SIMPLE)
			}
			if _, inBatch := batch[string(probe.bytes)]; inBatch {
				continue
			}
			hit, err := db.fkProbeHits(probe, fk.RefTable)
			if err != nil {
				return nil, err
			}
			if !hit {
				return nil, newError(ForeignKeyViolation,
					"insert or update on table "+relation+" violates foreign key constraint "+fk.Name)
			}
		}
	}

	// EXCLUDE constraints (spec/design/gist.md §7), after FK existence — a batch pass over the
	// statement's END STATE: each new row must conflict with no STORED row (probe the backing GiST
	// tree, whose leaf recheck is the full (expr_i op_i) conjunction) and no OTHER new row of this
	// batch (pairwise — the resident tree holds only stored rows). The NULL rule / empty-range exempt
	// a row. Unmetered validation, before any write.
	if len(table.Exclusions) > 0 {
		tcols := table.Columns
		for _, exc := range table.Exclusions {
			ikey := strings.ToLower(exc.Index)
			for _, pr := range prepared {
				q, strats, ok := exclusionProbeQuery(tcols, exc, pr.row)
				if !ok {
					continue // exempt
				}
				conflict := false
				if tree := db.readSnap().gistTreeFor(ikey); tree != nil {
					hits, _, _ := tree.search(q, strats)
					conflict = len(hits) > 0
				}
				if conflict {
					return nil, newError(ExclusionViolation,
						"conflicting key value violates exclusion constraint: "+exc.Name)
				}
			}
			for i := range prepared {
				for j := 0; j < i; j++ {
					if exclusionPairConflicts(tcols, exc, prepared[i].row, prepared[j].row) {
						return nil, newError(ExclusionViolation,
							"conflicting key value violates exclusion constraint: "+exc.Name)
					}
				}
			}
		}
	}

	// Charge + enforce the ceiling BEFORE phase 2 writes anything (all-or-nothing).
	meter.Charge(costs.ValueCompress * cunits)
	if err := meter.Guard(); err != nil {
		return nil, err
	}

	// The RETURNING projection (grammar.md §32, cost.md §3): evaluate over the validated
	// rows — every check has passed, nothing is written yet, so subqueries in the list read
	// the pre-statement snapshot and a 54P01 here leaves the store untouched.
	var returned [][]Value
	if returning != nil {
		prows := make([]storedRow, len(prepared))
		for i := range prepared {
			prows[i] = prepared[i].row
		}
		var err error
		if returned, err = db.projectReturning(returning, prows, nil, params, ctes, meter); err != nil {
			return nil, err
		}
	}

	// Phase 2 — every row validated, so each insert is guaranteed to succeed. A synthetic
	// rowid is allocated here, in row order, so a failed validation pass burns none
	// (spec/fileformat/format.md, spec/design/grammar.md §12). Each stored row's
	// secondary-index entries are computed against its final key (the rowid included) and
	// written after the rows (indexes.md §4 — an index write cannot fail, so
	// all-or-nothing is unchanged).
	indexInserts := make([][][]byte, len(table.Indexes))
	for _, pr := range prepared {
		key := pr.key
		if key == nil {
			key = encodeInt(scalarInt64, store.AllocRowid())
		}
		for k, def := range table.Indexes {
			eks, err := indexEntryKeys(table.Columns, colls, def, key, pr.row)
			if err != nil {
				return nil, err
			}
			indexInserts[k] = append(indexInserts[k], eks...)
		}
		ok, err := store.Insert(key, pr.row)
		if err != nil {
			return nil, err
		}
		if !ok {
			// A collision here can only happen under the writable-CTE read pin (writable-cte.md §7):
			// an EARLIER data-modifying sub-statement of the same WITH staged this key, which phase 1
			// (reading the pin) did not see. Matches PostgreSQL's unique violation; the whole statement
			// aborts all-or-nothing. For a single statement, phase 1 already caught every duplicate, so
			// this is never reached.
			return nil, newError(UniqueViolation,
				"duplicate key value violates unique constraint: "+strings.ToLower(table.Name)+"_pkey")
		}
	}
	for k, def := range table.Indexes {
		istore := db.writeIndexStoreScoped(dbScope, strings.ToLower(def.Name))
		for _, ek := range indexInserts[k] {
			inserted, err := istore.Insert(ek, nil)
			if err != nil {
				return nil, err
			}
			if !inserted {
				// A cross-sub-statement unique-index collision under the read pin (as above).
				return nil, newError(UniqueViolation,
					"duplicate key value violates unique constraint: "+def.Name)
			}
		}
	}
	return returned, nil
}

// foldConflictPlan folds globally-uncorrelated subqueries in a DO UPDATE's SET/WHERE once (their
// cost is added a single time — cost.md §3), exactly as UPDATE folds its assignment/filter.
func (db *engine) foldConflictPlan(plan *conflictPlan, bound []Value, accrued *int64) error {
	if plan == nil || !plan.doUpdate {
		return nil
	}
	for i := range plan.assignments {
		if err := db.foldUncorrelatedInRExpr(plan.assignments[i].source, bound, cteCtx{}, accrued); err != nil {
			return err
		}
	}
	if plan.filter != nil {
		if err := db.foldUncorrelatedInRExpr(plan.filter, bound, cteCtx{}, accrued); err != nil {
			return err
		}
	}
	return nil
}

// runInsertRows dispatches the validated candidate rows to the plain or the ON CONFLICT insert
// path, shared by both INSERT sources. Returns (rows affected, RETURNING rows): a plain insert
// affects every candidate row; an ON CONFLICT may insert, update, or skip (spec/design/upsert.md §3).
func (db *engine) runInsertRows(table *catTable, store *tableStore, dbScope *string, pk []int, checks []namedCheck, defaultExprs []*rExpr, rng *stmtRng, provided []int, rows [][]Value, conflict *conflictPlan, returning []*rExpr, params []Value, ctes cteCtx, meter *costMeter) (int64, [][]Value, error) {
	if conflict != nil {
		// ON CONFLICT is reached only for a reserved scope (an attachment target is 0A000 in
		// executeInsert), where the bare temp-first funnels resolve the store correctly, so the conflict
		// path takes no dbScope.
		return db.insertRowsOnConflict(table, store, pk, checks, defaultExprs, rng, provided, rows, conflict, returning, params, ctes, meter)
	}
	returned, err := db.insertRows(table, store, dbScope, pk, checks, defaultExprs, rng, provided, rows, returning, params, ctes, meter)
	if err != nil {
		return 0, nil, err
	}
	return int64(len(rows)), returned, nil
}

// insertRowsOnConflict runs phase 1 + phase 2 of an INSERT ... ON CONFLICT (spec/design/upsert.md
// §3), the UPSERT analogue of insertRows. Phase 1 walks the candidate rows in source order,
// classifying each as a planned INSERT, a planned UPDATE of an existing row, or a SKIP; the planned
// inserts + updates are then validated against the statement END STATE (PK / unique / CHECK / FK)
// before phase 2 writes anything (all-or-nothing). returning projects the AFFECTED rows (inserts
// with an all-NULL old side, updates with their pre-update existing row).
func (db *engine) insertRowsOnConflict(table *catTable, store *tableStore, pk []int, checks []namedCheck, defaultExprs []*rExpr, rng *stmtRng, provided []int, rows [][]Value, plan *conflictPlan, returning []*rExpr, params []Value, ctes cteCtx, meter *costMeter) (int64, [][]Value, error) {
	n := len(table.Columns)
	relation := table.Name
	// Per-column frozen collations for the collated text key form (§2.12), resolved before any
	// mutation; nil everywhere for a C-only / non-text table (the fast path).
	colls := db.columnCollations(table.Columns)
	// The unique-index positions in table.Indexes (for the no-target skip test + end-state pass).
	var uniqIdx []int
	for i, def := range table.Indexes {
		if def.Unique {
			uniqIdx = append(uniqIdx, i)
		}
	}

	type pendingUpdate struct {
		key    []byte
		newRow storedRow
		oldRow storedRow
	}
	var inserts []storedRow
	var updates []pendingUpdate
	// Arbiter keys this statement has already proposed (the §4 second-affect rule).
	proposedArb := make(map[string]struct{})
	// For the no-target DO NOTHING path: the planned inserts' keys/prefixes, so an in-batch
	// duplicate is skipped (the arbiter path uses proposedArb instead).
	insPk := make(map[string]struct{})
	insPrefixes := make([]map[string]struct{}, len(uniqIdx))
	for i := range insPrefixes {
		insPrefixes[i] = make(map[string]struct{})
	}

	for _, values := range rows {
		// Build + coerce the candidate row, then CHECK — the INSERT per-row order (NOT NULL
		// before CHECK before conflict; constraints.md §4.4).
		row := make(storedRow, n)
		for i, col := range table.Columns {
			var candidate Value
			if p := provided[i]; p >= 0 {
				candidate = values[p]
			} else {
				dv, err := db.evalDefault(col, defaultExprs[i], rng, meter)
				if err != nil {
					return 0, nil, err
				}
				candidate = dv
			}
			v, err := coerceForStore(candidate, store.colTypes[i], col.Decimal, col.VarcharLen, col.NotNull, col.Name)
			if err != nil {
				return 0, nil, err
			}
			row[i] = v
		}
		if len(checks) > 0 {
			if err := meter.Guard(); err != nil {
				return 0, nil, err
			}
			env := &evalEnv{exec: db, rng: rng}
			if err := evalChecks(checks, relation, row, env, meter); err != nil {
				return 0, nil, err
			}
		}

		if plan.arb == nil {
			// No-target DO NOTHING: skip on ANY uniqueness conflict (committed OR an earlier
			// planned insert); else insert (upsert.md §2/§3).
			var pkk []byte
			if len(pk) > 0 {
				k, err := encodePkKey(table, pk, colls, row)
				if err != nil {
					return 0, nil, err
				}
				pkk = k
			}
			committed, err := db.rowConflictsCommitted(store, table, pk, colls, row)
			if err != nil {
				return 0, nil, err
			}
			inBatch := false
			if pkk != nil {
				if _, dup := insPk[string(pkk)]; dup {
					inBatch = true
				}
			}
			if !inBatch {
				for u, ix := range uniqIdx {
					prefix, ok, err := indexPrefixKey(table.Columns, colls, table.Indexes[ix], row)
					if err != nil {
						return 0, nil, err
					}
					if ok {
						if _, dup := insPrefixes[u][string(prefix)]; dup {
							inBatch = true
							break
						}
					}
				}
			}
			if committed || inBatch {
				continue // skip
			}
			if pkk != nil {
				insPk[string(pkk)] = struct{}{}
			}
			for u, ix := range uniqIdx {
				prefix, ok, err := indexPrefixKey(table.Columns, colls, table.Indexes[ix], row)
				if err != nil {
					return 0, nil, err
				}
				if ok {
					insPrefixes[u][string(prefix)] = struct{}{}
				}
			}
			inserts = append(inserts, row)
			continue
		}

		// Arbiter present (DO UPDATE always; DO NOTHING with a target).
		ak, ok, err := arbiterKey(plan.arb, table, pk, colls, row)
		if err != nil {
			return 0, nil, err
		}
		if !ok {
			// A NULL-bearing arbiter key never conflicts (NULLS DISTINCT) — plain insert.
			inserts = append(inserts, row)
			continue
		}
		if _, dup := proposedArb[string(ak)]; dup {
			// A second proposed row with the same arbiter key (§4).
			if plan.doUpdate {
				return 0, nil, newError(CardinalityViolation,
					"ON CONFLICT DO UPDATE command cannot affect row a second time")
			}
			continue // DO NOTHING → skip
		}
		proposedArb[string(ak)] = struct{}{}
		existKey, existRow, found, err := db.arbiterExisting(plan.arb, store, table, ak)
		if err != nil {
			return 0, nil, err
		}
		if !found {
			// No committed conflict on the arbiter → insert (a non-arbiter conflict is caught
			// by the end-state validation below).
			inserts = append(inserts, row)
			continue
		}
		if !plan.doUpdate {
			continue // DO NOTHING → skip
		}
		// DO UPDATE: the combined eval row [existing | proposed] the §5 scope resolves against.
		combined := make(storedRow, 0, 2*n)
		combined = append(combined, existRow...)
		combined = append(combined, row...)
		env := &evalEnv{exec: db, params: params, rng: rng}
		// An optional WHERE that is not TRUE skips the update (existing row unchanged, not
		// returned) — but the arbiter key was already proposed, so a second row still trips §4.
		if plan.filter != nil {
			v, err := plan.filter.eval(combined, env, meter)
			if err != nil {
				return 0, nil, err
			}
			if !v.IsTrue() {
				continue
			}
		}
		newRow := make(storedRow, n)
		copy(newRow, existRow)
		for _, ap := range plan.assignments {
			raw, err := ap.source.eval(combined, env, meter)
			if err != nil {
				return 0, nil, err
			}
			checked, err := ap.check(raw)
			if err != nil {
				return 0, nil, err
			}
			newRow[ap.idx] = checked
		}
		if len(checks) > 0 {
			cenv := &evalEnv{exec: db, rng: rng}
			if err := evalChecks(checks, relation, newRow, cenv, meter); err != nil {
				return 0, nil, err
			}
		}
		updates = append(updates, pendingUpdate{key: existKey, newRow: newRow, oldRow: existRow})
	}

	// End-state validation (upsert.md §3), before any write. PRIMARY KEY: each insert's key must
	// be free in the committed store and distinct from the other inserts (updates never change
	// the key) — a collision is 23505 on <table>_pkey (a non-arbiter PK conflict).
	if len(pk) > 0 && len(inserts) > 0 {
		seen := make(map[string]struct{}, len(inserts))
		for _, row := range inserts {
			k, err := encodePkKey(table, pk, colls, row)
			if err != nil {
				return 0, nil, err
			}
			if _, exists, err := store.Get(k); err != nil {
				return 0, nil, err
			} else if exists {
				return 0, nil, newError(UniqueViolation,
					"duplicate key value violates unique constraint: "+strings.ToLower(relation)+"_pkey")
			}
			if _, dup := seen[string(k)]; dup {
				return 0, nil, newError(UniqueViolation,
					"duplicate key value violates unique constraint: "+strings.ToLower(relation)+"_pkey")
			}
			seen[string(k)] = struct{}{}
		}
	}

	// UNIQUE indexes: validate the END STATE over the updated NEW rows + the inserted rows
	// (indexes.md §8 — the same end-state model as UPDATE).
	if len(uniqIdx) > 0 && (len(inserts) > 0 || len(updates) > 0) {
		rewritten := make(map[string]struct{}, len(updates))
		for _, u := range updates {
			rewritten[string(u.key)] = struct{}{}
		}
		newRows := make([]storedRow, 0, len(updates)+len(inserts))
		for _, u := range updates {
			newRows = append(newRows, u.newRow)
		}
		newRows = append(newRows, inserts...)
		for _, ix := range uniqIdx {
			def := table.Indexes[ix]
			istore := db.lkpIndexStore(strings.ToLower(def.Name))
			batch := make(map[string]struct{})
			for _, newRow := range newRows {
				prefix, ok, err := indexPrefixKey(table.Columns, colls, def, newRow)
				if err != nil {
					return 0, nil, err
				}
				if !ok {
					continue
				}
				conflict := false
				if _, dup := batch[string(prefix)]; dup {
					conflict = true
				} else {
					entries, err := istore.RangeEntries(uniqueProbeBound(prefix))
					if err != nil {
						return 0, nil, err
					}
					for _, e := range entries {
						if _, own := rewritten[string(e.Key[len(prefix):])]; !own {
							conflict = true
							break
						}
					}
				}
				if conflict {
					return 0, nil, newError(UniqueViolation,
						"duplicate key value violates unique constraint: "+def.Name)
				}
				batch[string(prefix)] = struct{}{}
			}
		}
	}

	// FOREIGN KEY child-side (constraints.md §6.4): each inserted row, and each updated row that
	// assigned an FK local column, must reference an existing parent key — the committed parent
	// state plus (for a self-reference) the statement's end state.
	assigned := make(map[int]struct{})
	if plan.doUpdate {
		for _, ap := range plan.assignments {
			assigned[ap.idx] = struct{}{}
		}
	}
	for fki := range table.ForeignKeys {
		fk := &table.ForeignKeys[fki]
		parent, ok := db.Table(fk.RefTable)
		if !ok {
			continue
		}
		// The probe matches the parent's stored key, so a collated parent key column uses the
		// PARENT's collation (§2.12).
		parentColls := db.columnCollations(parent.Columns)
		checkUpdates := false
		for _, c := range fk.Columns {
			if _, ok := assigned[c]; ok {
				checkUpdates = true
				break
			}
		}
		// End-state referenced keys this statement supplies, for a self-reference.
		batch := make(map[string]struct{})
		if strings.EqualFold(fk.RefTable, relation) {
			for _, row := range inserts {
				probe, ok, err := buildFkProbe(fk, parent, parentColls, row, fk.RefColumns)
				if err != nil {
					return 0, nil, err
				}
				if ok {
					batch[string(probe.bytes)] = struct{}{}
				}
			}
			for _, u := range updates {
				probe, ok, err := buildFkProbe(fk, parent, parentColls, u.newRow, fk.RefColumns)
				if err != nil {
					return 0, nil, err
				}
				if ok {
					batch[string(probe.bytes)] = struct{}{}
				}
			}
		}
		toCheck := make([]storedRow, 0, len(inserts)+len(updates))
		toCheck = append(toCheck, inserts...)
		if checkUpdates {
			for _, u := range updates {
				toCheck = append(toCheck, u.newRow)
			}
		}
		for _, row := range toCheck {
			probe, ok, err := buildFkProbe(fk, parent, parentColls, row, fk.Columns)
			if err != nil {
				return 0, nil, err
			}
			if !ok {
				continue // a NULL local column → exempt (MATCH SIMPLE)
			}
			if _, inBatch := batch[string(probe.bytes)]; inBatch {
				continue
			}
			hit, err := db.fkProbeHits(probe, fk.RefTable)
			if err != nil {
				return 0, nil, err
			}
			if !hit {
				return 0, nil, newError(ForeignKeyViolation,
					"insert or update on table "+relation+" violates foreign key constraint "+fk.Name)
			}
		}
	}

	// FOREIGN KEY parent-side (constraints.md §6.5): an updated referenced row must not strand a
	// child (only a referenced UNIQUE column is at risk; inserts add rows, never strand a child).
	referencers := db.fkReferencers(relation)
	if len(referencers) > 0 && len(updates) > 0 {
		parent, _ := db.Table(relation)
		updatedKeys := make(map[string]struct{}, len(updates))
		for _, u := range updates {
			updatedKeys[string(u.key)] = struct{}{}
		}
		for ri := range referencers {
			r := &referencers[ri]
			// parent is the insert target itself, so its key columns use colls (§2.12).
			newPresent := make(map[string]struct{})
			for _, u := range updates {
				probe, ok, err := buildFkProbe(&r.fk, parent, colls, u.newRow, r.fk.RefColumns)
				if err != nil {
					return 0, nil, err
				}
				if ok {
					newPresent[string(probe.bytes)] = struct{}{}
				}
			}
			for _, u := range updates {
				oldProbe, ok, err := buildFkProbe(&r.fk, parent, colls, u.oldRow, r.fk.RefColumns)
				if err != nil {
					return 0, nil, err
				}
				if !ok {
					continue
				}
				newProbe, ok, err := buildFkProbe(&r.fk, parent, colls, u.newRow, r.fk.RefColumns)
				if err != nil {
					return 0, nil, err
				}
				if ok {
					if bytes.Equal(newProbe.bytes, oldProbe.bytes) {
						continue
					}
				}
				if _, present := newPresent[string(oldProbe.bytes)]; present {
					continue
				}
				referenced, err := db.fkChildReferences(r.childTable, &r.fk, parent, oldProbe.bytes, updatedKeys)
				if err != nil {
					return 0, nil, err
				}
				if referenced {
					return 0, nil, newError(ForeignKeyViolation,
						"update or delete on table "+parent.Name+" violates foreign key constraint "+r.fk.Name+" on table "+r.childTable)
				}
			}
		}
	}

	// Meter the disposition-plan compression attempts (value_compress, cost.md §3) for the
	// inserted + updated rows; enforce the ceiling BEFORE phase 2 writes (all-or-nothing).
	var cunits int64
	placeholder := make([]byte, 8)
	for _, row := range inserts {
		kb := placeholder
		if len(pk) > 0 {
			k, err := encodePkKey(table, pk, colls, row)
			if err != nil {
				return 0, nil, err
			}
			kb = k
		}
		cunits += int64(store.WriteCompressUnits(kb, row))
	}
	for _, u := range updates {
		cunits += int64(store.WriteCompressUnits(u.key, u.newRow))
	}
	meter.Charge(costs.ValueCompress * cunits)
	if err := meter.Guard(); err != nil {
		return 0, nil, err
	}

	// RETURNING (grammar.md §32): project the affected rows — inserts (old side all-NULL) then
	// updates (old side the pre-update existing row) — after all validation, before any write.
	var returned [][]Value
	if returning != nil {
		nullRow := make(storedRow, n)
		for i := range nullRow {
			nullRow[i] = NullValue()
		}
		prows := make([]storedRow, 0, len(inserts)+len(updates))
		olds := make([]storedRow, 0, len(inserts)+len(updates))
		for _, row := range inserts {
			prows = append(prows, row)
			olds = append(olds, nullRow)
		}
		for _, u := range updates {
			prows = append(prows, u.newRow)
			olds = append(olds, u.oldRow)
		}
		var err error
		if returned, err = db.projectReturning(returning, prows, olds, params, ctes, meter); err != nil {
			return 0, nil, err
		}
	}

	affected := int64(len(inserts) + len(updates))

	// Phase 2 — every row validated. Insert the new rows (rowid alloc for a no-PK table, index
	// entries added), then replace the updated rows (index entries moved).
	indexAdds := make([][][]byte, len(table.Indexes))
	for _, row := range inserts {
		var key []byte
		if len(pk) > 0 {
			k, err := encodePkKey(table, pk, colls, row)
			if err != nil {
				return 0, nil, err
			}
			key = k
		} else {
			key = encodeInt(scalarInt64, store.AllocRowid())
		}
		for k, def := range table.Indexes {
			eks, err := indexEntryKeys(table.Columns, colls, def, key, row)
			if err != nil {
				return 0, nil, err
			}
			indexAdds[k] = append(indexAdds[k], eks...)
		}
		ok, err := store.Insert(key, row)
		if err != nil {
			return 0, nil, err
		}
		if !ok {
			panic("pre-validated INSERT key must be unique")
		}
	}
	type indexMove struct{ removals, insertions [][]byte }
	indexMoves := make([][]indexMove, len(table.Indexes))
	for _, u := range updates {
		for k, def := range table.Indexes {
			oldEks, err := indexEntryKeys(table.Columns, colls, def, u.key, u.oldRow)
			if err != nil {
				return 0, nil, err
			}
			newEks, err := indexEntryKeys(table.Columns, colls, def, u.key, u.newRow)
			if err != nil {
				return 0, nil, err
			}
			removals := bytesDiff(oldEks, newEks)
			insertions := bytesDiff(newEks, oldEks)
			if len(removals) > 0 || len(insertions) > 0 {
				indexMoves[k] = append(indexMoves[k], indexMove{removals: removals, insertions: insertions})
			}
		}
	}
	for _, u := range updates {
		if err := store.Replace(u.key, u.newRow); err != nil {
			return 0, nil, err
		}
	}
	for k, def := range table.Indexes {
		istore := db.writeIndexStore(strings.ToLower(def.Name))
		for _, ek := range indexAdds[k] {
			inserted, err := istore.Insert(ek, nil)
			if err != nil {
				return 0, nil, err
			}
			if !inserted {
				panic("index entry keys are unique (storage-key suffix)")
			}
		}
		for _, mv := range indexMoves[k] {
			for _, oldEk := range mv.removals {
				if _, err := istore.Remove(oldEk); err != nil {
					return 0, nil, err
				}
			}
			for _, newEk := range mv.insertions {
				inserted, err := istore.Insert(newEk, nil)
				if err != nil {
					return 0, nil, err
				}
				if !inserted {
					panic("index entry keys are unique (storage-key suffix)")
				}
			}
		}
	}
	return affected, returned, nil
}

// defaultOrNull is the column's stored default value, or a NULL value when it has none —
// the candidate for an omitted column or a DEFAULT keyword slot (constraints.md §2).
func defaultOrNull(col catColumn) Value {
	if col.Default != nil {
		return *col.Default
	}
	return NullValue()
}

// resolveReturning resolves a RETURNING item list against the target table's one-relation
// scope (grammar.md §32): aggregates are 42803 (the non-collecting aggCtx), subqueries
// resolve (and may correlate against the returned row), output names follow §8. Returns the
// projection nodes and names; the item types have no consumer.
// The scope is the RETURNING scope (returningScope — the table at offset 0 plus the
// old/new qualifier-only pseudo-relations over the [base | other] projection row, with
// baseIsOld true for DELETE).
func (db *engine) resolveReturning(table *catTable, items selectItems, baseIsOld bool, ctes []*cteBinding, ptypes *paramTypes) ([]*rExpr, []string, []string, error) {
	s := returningScope(db, table, baseIsOld)
	s.ctes = ctes
	nodes, names, types, err := resolveProjections(s, items, &aggCtx{collecting: false}, ptypes)
	if err != nil {
		return nil, nil, nil, err
	}
	return nodes, names, typeNames(types), nil
}

// projectReturning evaluates a resolved RETURNING projection over the affected rows
// (grammar.md §32, cost.md §3): per returned row, guard the ceiling, charge one
// row_produced, then evaluate each item — metered expression work, exactly a SELECT's
// projection (a correlated subquery re-runs here, its outer reference reading the row being
// returned). Callers run this after all validation and BEFORE any write.
// The evaluation row is the concatenation [base | other] the RETURNING scope resolved
// against: others[i] is the row's opposite version (UPDATE's old rows), nil the all-NULL
// row (INSERT's old side, DELETE's new side).
func (db *engine) projectReturning(nodes []*rExpr, rows []storedRow, others []storedRow, params []Value, ctes cteCtx, meter *costMeter) ([][]Value, error) {
	env := &evalEnv{exec: db, params: params, rng: newStmtRng(), ctes: ctes}
	out := make([][]Value, 0, len(rows))
	for i, row := range rows {
		if err := meter.Guard(); err != nil {
			return nil, err
		}
		meter.Charge(costs.RowProduced)
		combined := make(storedRow, 0, 2*len(row))
		combined = append(combined, row...)
		if others != nil {
			combined = append(combined, others[i]...)
		} else {
			for range row {
				combined = append(combined, NullValue())
			}
		}
		vals := make([]Value, 0, len(nodes))
		for _, node := range nodes {
			v, err := node.eval(combined, env, meter)
			if err != nil {
				return nil, err
			}
			vals = append(vals, v)
		}
		out = append(out, vals)
	}
	return out, nil
}

// dmlOutcome wraps a DML statement's completion: a query result projecting the returned rows
// when a RETURNING clause was resolved (retNames non-nil — grammar.md §32; zero affected
// rows is an EMPTY query result, never a bare statement), else a bare statement result
// carrying the affected-row count (spec/design/api.md §4).
func dmlOutcome(retNames []string, retTypes []string, returned [][]Value, affected int64, cost int64) outcome {
	if retNames != nil {
		if returned == nil {
			returned = [][]Value{}
		}
		return outcome{Kind: outcomeQuery, ColumnNames: retNames, ColumnTypes: retTypes, Rows: returned, Cost: cost}
	}
	return outcome{Kind: outcomeStatement, Cost: cost, RowsAffected: affected, HasRowsAffected: true}
}

// executeDelete analyzes and runs a DELETE: resolve the table and optional predicate,
// collect the keys of matching rows (only a TRUE predicate matches — Kleene), then
// remove them. No WHERE deletes every row. Keys are collected before mutating so the
// map is not modified while iterating.
func (db *engine) executeDelete(del *deleteStmt, params []Value, ctx cteCtx) (outcome, error) {
	// A catalog relation is read-only (introspection.md §5): a DML target naming one is 42809,
	// checked by NAME before qualifier validation (the built-in resolves in every database).
	if err := checkCatalogRelWrite(del.Table); err != nil {
		return outcome{}, err
	}
	// A write to a READ-ONLY host attachment is 25006 before any I/O — checked BEFORE the qualifier
	// existence gate so a read-only attachment refuses the write deterministically (attached-databases.md §4).
	if err := db.checkAttachmentWritable(del.DB); err != nil {
		return outcome{}, err
	}
	if err := db.checkTableQualifier(del.DB, del.Table); err != nil { // attached-databases.md §3
		return outcome{}, err
	}
	table, ok := db.lkpTableScoped(del.DB, del.Table) // scope-aware temp-first (temp-tables.md §3)
	if !ok {
		return outcome{}, newError(UndefinedTable, "table does not exist: "+del.Table)
	}
	// Refuse the write if any collated key is version-skewed (slice 2d, collation.md §12, XX002): a
	// DELETE must locate + remove a stored key, which a skewed encoding cannot match.
	if err := db.ensureCollationsWritable(table.Columns); err != nil {
		return outcome{}, err
	}
	// Per-column frozen collations for the collated text key form (§2.12) — indexes both the FK
	// parent-side probe (parent is this table) and the index-entry path.
	colls := db.columnCollations(table.Columns)
	// DELETE is single-table; resolve its WHERE against a one-relation scope. The RETURNING
	// projection resolves after it (PostgreSQL's analysis order), against the same scope
	// (grammar.md §32). The statement's CTE bindings (writable-cte.md) are visible so a WHERE /
	// RETURNING sublink may reference an earlier CTE.
	s := singleScope(db, table)
	s.ctes = ctx.bindings
	ptypes := &paramTypes{}
	var filter *rExpr
	if del.Filter != nil {
		f, err := resolveBooleanFilter(s, del.Filter, ptypes)
		if err != nil {
			return outcome{}, err
		}
		filter = f
	}
	var retNodes []*rExpr
	var retNames []string
	var retTypes []string
	if del.Returning != nil {
		var rerr error
		if retNodes, retNames, retTypes, rerr = db.resolveReturning(table, *del.Returning, true, ctx.bindings, ptypes); rerr != nil {
			return outcome{}, rerr
		}
	}
	ptys, err := ptypes.finalize()
	if err != nil {
		return outcome{}, err
	}
	bound, err := bindParams(params, ptys)
	if err != nil {
		return outcome{}, err
	}

	// Fold globally-uncorrelated WHERE subqueries once (their cost is added a single time —
	// spec/design/grammar.md §26, cost.md §3); a correlated one stays and re-runs per row via the
	// per-row outer environment below (it pushes the current row, so `target.col` reads it). The
	// uncorrelated execution reads the pre-DELETE snapshot (keys are collected before mutating).
	// Each scanned row and each filter evaluation accrues cost (CLAUDE.md §13; cost.md §3).
	meter := db.session.newMeter()
	if filter != nil {
		if err := db.foldUncorrelatedInRExpr(filter, bound, ctx, &meter.Accrued); err != nil {
			return outcome{}, err
		}
	}
	// Uncorrelated subqueries in the RETURNING list fold once (cost.md §3), reading the
	// pre-statement snapshot (grammar.md §32).
	for _, node := range retNodes {
		if err := db.foldUncorrelatedInRExpr(node, bound, ctx, &meter.Accrued); err != nil {
			return outcome{}, err
		}
	}
	env := &evalEnv{exec: db, params: bound, rng: newStmtRng(), ctes: ctx}
	// The scan reads the pin (readSnap) — under the writable-CTE read pin (writable-cte.md §2) a
	// DELETE sees the PRE-statement rows, not an earlier sub-statement's table writes; phase 2 below
	// writes into working. readSnap == working for an ordinary DELETE, so the scan is unchanged there.
	store := db.lkpStoreScoped(del.DB, del.Table)
	writeStore := db.writeStoreScoped(del.DB, del.Table)
	// matched collects (key, row) pairs before mutating; the rows feed phase 2's
	// index-entry removal (indexed columns are fixed-width and always resident).
	type matchedRow struct {
		key []byte
		row storedRow
	}
	var matched []matchedRow
	// DELETE's touched set (cost.md §3): the filter's columns plus the RETURNING items'
	// OLD-side references — a returned old value is a logical read of the dropped row,
	// while a new.col is the constant NULL row and reads nothing. The RETURNING mask spans
	// the [base | other] projection row (2 x ncols); only the base (old) half maps back to
	// storage. A bare DELETE still charges no chain/decompress units at all.
	mask := make([]bool, len(table.Columns))
	collectTouched(filter, 0, mask)
	if retNodes != nil {
		retMask := make([]bool, 2*len(table.Columns))
		for _, node := range retNodes {
			collectTouched(node, 0, retMask)
		}
		for i := range mask {
			mask[i] = mask[i] || retMask[i]
		}
	}
	// A primary-key bound seeks/ranges instead of walking the whole B-tree (cost.md §3 "bounded
	// scan"); an empty bound deletes nothing. The whole WHERE stays the residual filter below.
	// page_read per visited node (block, before the rows), then storage_row_read per scanned row.
	var entries []entry
	var overlap, slabs int
	if isAttachmentScope(del.DB) {
		// A host-attached target full-scans this slice (attached-databases.md §8) — a bounded scan would
		// resolve its index store through the unscoped funnel. The whole WHERE stays the residual filter.
		if entries, overlap, slabs, err = store.ScanWithUnits(mask); err != nil {
			return outcome{}, err
		}
	} else if bp := db.pkBoundFor(table, filter); bp != nil {
		// Top-level statement: no enclosing query, so the bound never has a correlated source.
		kb, empty := db.buildKeyBound(bp, bound, nil, nil)
		if empty {
			// A provably-empty bound affects zero rows — with RETURNING that is still a
			// query result (empty rows), never a bare statement (grammar.md §32).
			return dmlOutcome(retNames, retTypes, nil, 0, meter.Accrued), nil
		}
		if entries, overlap, slabs, err = store.RangeScanWithUnits(kb, mask); err != nil {
			return outcome{}, err
		}
	} else if gb := detectGinBound(filter, table.Indexes, table.Columns, 0); gb != nil {
		// GIN-bounded delete (gin.md §6): when no PK bound applies, gather the candidate (key,row)
		// Entry pairs through the index; the predicate stays the residual filter, re-applied per
		// candidate below. GinEntry charged inside; the page_read/value_decompress block below.
		// readSnap()==working() during a mutation (tx open), so this reads the read-your-writes state.
		var query *rExpr
		if _, q, ok := ginMatch(filter, gb.colGlobal); ok {
			query = q
		}
		if entries, overlap, slabs, err = db.ginBoundRows(del.Table, gb, query, env, meter, mask); err != nil {
			return outcome{}, err
		}
	} else if gb := detectGistBound(filter, table.Indexes, table.Columns, 0); gb != nil {
		// GiST-bounded delete (gist.md §5): gather candidates by descending the resident R-tree; the
		// &&/@> predicate stays the residual filter re-applied per candidate below.
		var query *rExpr
		if q, ok := gistQueryOperand(filter, gb); ok {
			query = q
		}
		if entries, overlap, slabs, err = db.gistBoundRows(del.Table, gb, query, env, meter, mask); err != nil {
			return outcome{}, err
		}
	} else if ks := db.pkSetFor(table, filter); ks != nil {
		// Merged PK point-set delete (cost.md §3 "OR / IN-list"): a union of point probes over the
		// distinct sorted keys; whole rows so index entries can be removed. The predicate stays the
		// residual filter below.
		if entries, overlap, slabs, err = db.pkKeySetRows(store, ks, bound, nil, mask, nil, false); err != nil {
			return outcome{}, err
		}
	} else {
		if entries, overlap, slabs, err = store.ScanWithUnits(mask); err != nil {
			return outcome{}, err
		}
	}
	meter.Charge(costs.PageRead*int64(overlap) + costs.ValueDecompress*int64(slabs))
	for _, e := range entries {
		if err := meter.Guard(); err != nil { // enforce the cost ceiling per scanned row (CLAUDE.md §13)
			return outcome{}, err
		}
		meter.Charge(costs.StorageRowRead)
		// Materialize the filter's columns if the lazy load left them unfetched — exactly the
		// touched set the block above charged (large-values.md §14).
		row, err := store.resolveColumns(e.Row, mask)
		if err != nil {
			return outcome{}, err
		}
		keep := true
		if filter != nil {
			v, err := filter.eval(row, env, meter)
			if err != nil {
				return outcome{}, err
			}
			keep = v.IsTrue()
		}
		if keep {
			// The FK parent-side probe + index-entry removal below read this row's key/index columns
			// directly; resolve its inline-deferred values (lazy-record.md §5b — a key column is
			// always inline, so cost-free) so those paths see resident values.
			row, err = store.resolveInlineColumns(row)
			if err != nil {
				return outcome{}, err
			}
			matched = append(matched, matchedRow{key: e.Key, row: row})
		}
	}

	// FOREIGN KEY parent-side (constraints.md §6.5): a DELETE must not strand a child. For each
	// inbound FK, every deleted row's referenced tuple disappears (the referenced columns are
	// unique, so each is unique to its row); if a child still references it → 23503. Unmetered,
	// before phase 2 (all-or-nothing). For a self-reference the child IS this table, whose end
	// state excludes the rows being deleted.
	referencers := db.fkReferencers(del.Table)
	if len(referencers) > 0 {
		parent, _ := db.Table(del.Table)
		deletedKeys := make(map[string]struct{}, len(matched))
		for _, m := range matched {
			deletedKeys[string(m.key)] = struct{}{}
		}
		empty := map[string]struct{}{}
		for ri := range referencers {
			r := &referencers[ri]
			exclude := empty
			if strings.EqualFold(r.childTable, del.Table) {
				exclude = deletedKeys
			}
			for _, m := range matched {
				// parent is the delete target itself, so its key columns use colls (§2.12).
				probe, ok, err := buildFkProbe(&r.fk, parent, colls, m.row, r.fk.RefColumns)
				if err != nil {
					return outcome{}, err
				}
				if !ok {
					continue // a NULL referenced value cannot be referenced (MATCH SIMPLE)
				}
				referenced, err := db.fkChildReferences(r.childTable, &r.fk, parent, probe.bytes, exclude)
				if err != nil {
					return outcome{}, err
				}
				if referenced {
					return outcome{}, newError(ForeignKeyViolation,
						"update or delete on table "+parent.Name+" violates foreign key constraint "+r.fk.Name+" on table "+r.childTable)
				}
			}
		}
	}

	// The RETURNING projection (grammar.md §32, cost.md §3): evaluate over the matched rows'
	// OLD values before anything is removed — subqueries in the list read the pre-statement
	// snapshot, and a 54P01 here deletes nothing (all-or-nothing).
	var returned [][]Value
	if retNodes != nil {
		prows := make([]storedRow, len(matched))
		for i := range matched {
			prows[i] = matched[i].row
		}
		if returned, err = db.projectReturning(retNodes, prows, nil, bound, ctx, meter); err != nil {
			return outcome{}, err
		}
	}
	// Phase 2: remove the rows, then their secondary-index entries (indexes.md §4 —
	// unmetered write work; an index removal cannot fail). Writes land in working (writeStore), even
	// when the scan above read the pin.
	for _, m := range matched {
		if _, err := writeStore.Remove(m.key); err != nil {
			return outcome{}, err
		}
	}
	for _, def := range table.Indexes {
		istore := db.writeIndexStoreScoped(del.DB, strings.ToLower(def.Name))
		for _, m := range matched {
			eks, err := indexEntryKeys(table.Columns, colls, def, m.key, m.row)
			if err != nil {
				return outcome{}, err
			}
			for _, ek := range eks {
				if _, err := istore.Remove(ek); err != nil {
					return outcome{}, err
				}
			}
		}
	}
	return dmlOutcome(retNames, retTypes, returned, int64(len(matched)), meter.Accrued), nil
}

// executeUpdate analyzes and runs an UPDATE. Two-phase / all-or-nothing: phase 1
// builds and type-checks every matching row's new values (assignments evaluate
// against the old row, so `SET a = b, b = a` swaps); a 22003/23502 aborts with no
// writes. Phase 2 applies. Assigning a PRIMARY KEY column traps 0A000 (the storage
// key must not change this slice); a duplicate target column traps 42701. No WHERE
// updates every row.
func (db *engine) executeUpdate(upd *update, params []Value, ctx cteCtx) (outcome, error) {
	// A catalog relation is read-only (introspection.md §5): a DML target naming one is 42809,
	// checked by NAME before qualifier validation (the built-in resolves in every database).
	if err := checkCatalogRelWrite(upd.Table); err != nil {
		return outcome{}, err
	}
	// A write to a READ-ONLY host attachment is 25006 before any I/O — checked BEFORE the qualifier
	// existence gate so a read-only attachment refuses the write deterministically (attached-databases.md §4).
	if err := db.checkAttachmentWritable(upd.DB); err != nil {
		return outcome{}, err
	}
	if err := db.checkTableQualifier(upd.DB, upd.Table); err != nil { // attached-databases.md §3
		return outcome{}, err
	}
	table, ok := db.lkpTableScoped(upd.DB, upd.Table) // scope-aware temp-first (temp-tables.md §3)
	if !ok {
		return outcome{}, newError(UndefinedTable, "table does not exist: "+upd.Table)
	}
	// Refuse the write if any collated key is version-skewed (slice 2d, collation.md §12, XX002): an
	// UPDATE re-encodes + re-places keys, which a skewed encoding would corrupt.
	if err := db.ensureCollationsWritable(table.Columns); err != nil {
		return outcome{}, err
	}
	// Per-column frozen collations for the collated text key form (§2.12) — indexes both the FK
	// probe and the index-entry move path.
	colls := db.columnCollations(table.Columns)
	// UPDATE is single-table; the RHS / WHERE resolve against a one-relation scope so the
	// shared resolver serves it too (a qualified `WHERE t.a` against the sole table is fine). The
	// statement's CTE bindings (writable-cte.md) are visible so a SET / WHERE / RETURNING sublink may
	// reference an earlier CTE.
	s := singleScope(db, table)
	s.ctes = ctx.bindings
	ptypes := &paramTypes{}

	// Resolve assignments up front (fail fast, deterministic). Assigning a key member is
	// allowed and re-keys the row — the storage key is derived from the PK (constraints.md §3),
	// so a new key is recomputed and the row is moved in phase 2.
	pkMembers := table.PKIndices()
	plans := make([]assignPlan, 0, len(upd.Assignments))
	for _, a := range upd.Assignments {
		idx := table.ColumnIndex(a.Column)
		if idx < 0 {
			return outcome{}, newError(UndefinedColumn, "column does not exist: "+a.Column)
		}
		// A GENERATED ALWAYS identity column can only be set to DEFAULT (sequences.md §13.4); jed's
		// UPDATE has no `= DEFAULT` form, so any assignment is 428C9. Ordered before the PK-narrowing
		// 0A000 so an ALWAYS identity PRIMARY KEY reports 428C9 (PG's code).
		if c := table.Columns[idx].Identity; c != nil && *c == identityAlways {
			return outcome{}, newError(GeneratedAlways,
				fmt.Sprintf("column %s can only be updated to DEFAULT", a.Column))
		}
		for _, p := range plans {
			if p.idx == idx {
				return outcome{}, newError(DuplicateColumn,
					"column "+a.Column+" assigned more than once")
			}
		}
		col := table.Columns[idx]
		// Updating a composite-typed column lands in a later slice (anonymous-record → named-composite
		// assignment coercion — composite.md §12); reject it for now (0A000). Range and array columns
		// ARE updatable (ranges.md §4 / array.md §4) through the container path below.
		if col.Type.IsComposite() {
			return outcome{}, newError(FeatureNotSupported,
				"updating composite column "+a.Column+" is not supported yet")
		}
		if scalar, ok := col.Type.AsScalar(); ok {
			// The RHS is a general expression evaluated against the *old* row; a literal operand
			// adapts to the target column's type. The result must be assignable to the column's
			// family (integer/decimal/text or NULL; never boolean; decimal→int is explicit only).
			colScalar := scalar
			src, ty, err := resolve(s, a.Value, &colScalar, &aggCtx{collecting: false}, ptypes)
			if err != nil {
				return outcome{}, err
			}
			if err := requireAssignable(ty, colScalar, a.Column); err != nil {
				return outcome{}, err
			}
			plans = append(plans, assignPlan{
				idx: idx, name: col.Name, target: colScalar, decimal: col.Decimal, varcharLen: col.VarcharLen, notNull: col.NotNull, source: src,
			})
		} else {
			// A range or array column: the RHS adapts (a bare string literal via range_in/array_in,
			// a bare NULL to the typed NULL) or must resolve to the SAME container type. Stored
			// through coerceForStore (carried on the plan as colType).
			src, err := resolveContainerAssign(s, col, a.Value, &aggCtx{collecting: false}, ptypes)
			if err != nil {
				return outcome{}, err
			}
			ct := resolveColType(col.Type, s.catalog.readSnap().types)
			plans = append(plans, assignPlan{
				idx: idx, name: col.Name, notNull: col.NotNull, source: src, colType: &ct,
			})
		}
	}
	// A re-keying UPDATE assigns at least one key member: each matched row's storage key is
	// recomputed (phase 1) and the row is moved (phase 2). An UPDATE that touches no key member
	// keeps every storage key in place — the in-place fast path (writeStore.Replace).
	pkChanged := len(pkMembers) > 0 && slices.ContainsFunc(plans, func(p assignPlan) bool {
		return slices.Contains(pkMembers, p.idx)
	})

	var filter *rExpr
	if upd.Filter != nil {
		f, err := resolveBooleanFilter(s, upd.Filter, ptypes)
		if err != nil {
			return outcome{}, err
		}
		filter = f
	}
	// The RETURNING projection resolves last (PostgreSQL's analysis order), against the same
	// one-relation scope; it evaluates each matched row's NEW values (grammar.md §32).
	var retNodes []*rExpr
	var retNames []string
	var retTypes []string
	if upd.Returning != nil {
		var rerr error
		if retNodes, retNames, retTypes, rerr = db.resolveReturning(table, *upd.Returning, false, ctx.bindings, ptypes); rerr != nil {
			return outcome{}, rerr
		}
	}
	// The CHECK constraints, resolved once per statement in evaluation (name) order;
	// phase 1 evaluates them on each post-assignment row (constraints.md §4.4).
	checks, err := db.resolveChecks(table)
	if err != nil {
		return outcome{}, err
	}
	// All assignment RHSs + the WHERE + the RETURNING are resolved: finalize + bind before
	// any scan.
	ptys, err := ptypes.finalize()
	if err != nil {
		return outcome{}, err
	}
	bound, err := bindParams(params, ptys)
	if err != nil {
		return outcome{}, err
	}

	// Fold globally-uncorrelated subqueries (in any assignment RHS or the WHERE) once — their
	// cost is added a single time (grammar.md §26, cost.md §3); a correlated one stays and re-runs
	// per row via the outer environment (which pushes the current OLD row). The uncorrelated
	// execution reads the pre-UPDATE snapshot (phase 1 only reads; phase 2 writes).
	//
	// Phase 1: build + validate every matching row's new values; no writes yet. Each scanned row,
	// the filter, and each assignment RHS accrue cost (the phase-2 writes do not — cost.md §3).
	meter := db.session.newMeter()
	for i := range plans {
		if err := db.foldUncorrelatedInRExpr(plans[i].source, bound, ctx, &meter.Accrued); err != nil {
			return outcome{}, err
		}
	}
	if filter != nil {
		if err := db.foldUncorrelatedInRExpr(filter, bound, ctx, &meter.Accrued); err != nil {
			return outcome{}, err
		}
	}
	for _, node := range retNodes {
		if err := db.foldUncorrelatedInRExpr(node, bound, ctx, &meter.Accrued); err != nil {
			return outcome{}, err
		}
	}
	env := &evalEnv{exec: db, params: bound, rng: newStmtRng(), ctes: ctx}
	// The scan + per-row column resolution read the pin (readSnap) — under the writable-CTE read pin
	// (writable-cte.md §2) an UPDATE sees the PRE-statement rows; phase 2 below writes into working.
	// readSnap == working for an ordinary UPDATE, so this is unchanged there.
	store := db.lkpStoreScoped(upd.DB, upd.Table)
	writeStore := db.writeStoreScoped(upd.DB, upd.Table)
	// Each entry is (old key, new key, new row, OLD row) — the old row feeds the index
	// maintenance and the new key the re-keying; for a non-PK UPDATE the new key equals the old.
	type pending struct {
		key    []byte
		newKey []byte
		row    storedRow
		oldRow storedRow
	}
	var updates []pending
	// UPDATE's touched set (cost.md §3): the filter's columns, every assignment SOURCE's, and
	// the RETURNING items' MINUS the assigned columns — an assigned column's returned value is
	// the freshly computed one, not a storage read. The rewrite re-stores an untouched spilled
	// value without logically re-reading it (large-values.md §14).
	mask := make([]bool, len(table.Columns))
	collectTouched(filter, 0, mask)
	for i := range plans {
		collectTouched(plans[i].source, 0, mask)
	}
	// The RETURNING mask spans the [base | other] projection row (new at 0, old at ncols):
	// the NEW side joins minus the assigned columns (an assigned column's returned value is
	// the freshly computed one, not a storage read); the OLD side joins unconditionally
	// (old.col is always a storage read, assigned or not).
	if retNodes != nil {
		ncols := len(table.Columns)
		retMask := make([]bool, 2*ncols)
		for _, node := range retNodes {
			collectTouched(node, 0, retMask)
		}
		for i := range mask {
			if retMask[i] && !slices.ContainsFunc(plans, func(p assignPlan) bool { return p.idx == i }) {
				mask[i] = true // new side
			}
			if retMask[ncols+i] {
				mask[i] = true // old side — always a storage read
			}
		}
	}
	// A primary-key bound seeks/ranges instead of walking the whole B-tree (cost.md §3 "bounded
	// scan"); an empty bound updates nothing. The whole WHERE stays the residual filter below.
	// page_read per visited node (block, before the rows), then storage_row_read per scanned row.
	var entries []entry
	var overlap, slabs int
	if isAttachmentScope(upd.DB) {
		// A host-attached target full-scans this slice (attached-databases.md §8) — a bounded scan would
		// resolve its index store through the unscoped funnel. The whole WHERE stays the residual filter.
		if entries, overlap, slabs, err = store.ScanWithUnits(mask); err != nil {
			return outcome{}, err
		}
	} else if bp := db.pkBoundFor(table, filter); bp != nil {
		// Top-level statement: no enclosing query, so the bound never has a correlated source.
		kb, empty := db.buildKeyBound(bp, bound, nil, nil)
		if empty {
			// A provably-empty bound affects zero rows — with RETURNING that is still a
			// query result (empty rows), never a bare statement (grammar.md §32).
			return dmlOutcome(retNames, retTypes, nil, 0, meter.Accrued), nil
		}
		if entries, overlap, slabs, err = store.RangeScanWithUnits(kb, mask); err != nil {
			return outcome{}, err
		}
	} else if gb := detectGinBound(filter, table.Indexes, table.Columns, 0); gb != nil {
		// GIN-bounded update (gin.md §6): when no PK bound applies, gather the candidate (key,row)
		// Entry pairs through the index over the PRE-update state; the predicate stays the residual
		// filter (re-applied per candidate below). GinEntry charged inside; the block below.
		var query *rExpr
		if _, q, ok := ginMatch(filter, gb.colGlobal); ok {
			query = q
		}
		if entries, overlap, slabs, err = db.ginBoundRows(upd.Table, gb, query, env, meter, mask); err != nil {
			return outcome{}, err
		}
	} else if gb := detectGistBound(filter, table.Indexes, table.Columns, 0); gb != nil {
		// GiST-bounded update (gist.md §5): gather candidates by descending the resident R-tree over
		// the PRE-update state; the &&/@> predicate stays the residual filter re-applied per candidate.
		var query *rExpr
		if q, ok := gistQueryOperand(filter, gb); ok {
			query = q
		}
		if entries, overlap, slabs, err = db.gistBoundRows(upd.Table, gb, query, env, meter, mask); err != nil {
			return outcome{}, err
		}
	} else if ks := db.pkSetFor(table, filter); ks != nil {
		// Merged PK point-set update (cost.md §3 "OR / IN-list"): a union of point probes over the
		// distinct sorted keys of the PRE-update state; whole rows. The predicate stays the residual
		// filter below.
		if entries, overlap, slabs, err = db.pkKeySetRows(store, ks, bound, nil, mask, nil, false); err != nil {
			return outcome{}, err
		}
	} else {
		if entries, overlap, slabs, err = store.ScanWithUnits(mask); err != nil {
			return outcome{}, err
		}
	}
	meter.Charge(costs.PageRead*int64(overlap) + costs.ValueDecompress*int64(slabs))
	for _, e := range entries {
		if err := meter.Guard(); err != nil { // enforce the cost ceiling per scanned row (CLAUDE.md §13)
			return outcome{}, err
		}
		meter.Charge(costs.StorageRowRead)
		// Materialize the filter's + assignment sources' columns if the lazy load left them
		// unfetched — exactly the touched set the block above charged (large-values.md §14).
		row, err := store.resolveColumns(e.Row, mask)
		if err != nil {
			return outcome{}, err
		}
		if filter != nil {
			v, err := filter.eval(row, env, meter)
			if err != nil {
				return outcome{}, err
			}
			if !v.IsTrue() {
				continue
			}
		}
		// The OLD row is retained for index-entry removal (its key/index columns are read directly
		// below); resolve its inline-deferred values (lazy-record.md §5b — a key column is always
		// inline, so cost-free) so that maintenance sees resident values.
		if row, err = store.resolveInlineColumns(row); err != nil {
			return outcome{}, err
		}
		newRow := make(storedRow, len(row))
		copy(newRow, row)
		for _, p := range plans {
			raw, err := p.source.eval(row, env, meter)
			if err != nil {
				return outcome{}, err
			}
			checked, err := p.check(raw)
			if err != nil {
				return outcome{}, err
			}
			newRow[p.idx] = checked
		}
		// The rewritten row is stored fully resident: resolve any still-unfetched (untouched)
		// columns so its weight/disposition re-plan exactly as an eager writer's would —
		// unmetered, part of the rewrite like commit work (large-values.md §14).
		if newRow, err = store.resolveAll(newRow); err != nil {
			return outcome{}, err
		}
		// CHECK constraints, in name order, on the post-assignment row — after the
		// assignments coerced (22003/23502 in p.check above), on the fully-resident row
		// (constraints.md §4.4). Every check evaluates (not only those mentioning assigned
		// columns); TRUE and NULL pass, the first FALSE aborts the statement (phase 1 —
		// nothing has been written).
		if err := evalChecks(checks, table.Name, newRow, env, meter); err != nil {
			return outcome{}, err
		}
		// The row's NEW storage key: recomputed from the post-assignment row when a key member
		// was assigned (re-keying), else the unchanged old key.
		newKey := e.Key
		if pkChanged {
			if newKey, err = encodePkKey(table, pkMembers, colls, newRow); err != nil {
				return outcome{}, err
			}
		}
		updates = append(updates, pending{key: e.Key, newKey: newKey, row: newRow, oldRow: row})
	}

	// PRIMARY KEY end-state validation for a re-keying UPDATE (the storage key changed): like
	// UNIQUE (indexes.md §8) this is an END-STATE check — the new keys must be distinct from each
	// other (in-batch) and from every NON-rewritten stored key (a rewritten row's old key is
	// vacated by this statement, so a row landing on it is fine). A collision traps 23505 on the
	// PK's derived <table>_pkey name, reported BEFORE the secondary UNIQUE probes (PG reports the
	// PK first). Unmetered, phase 1.
	if pkChanged {
		rewritten := make(map[string]struct{}, len(updates))
		for _, u := range updates {
			rewritten[string(u.key)] = struct{}{}
		}
		batch := make(map[string]struct{}, len(updates))
		for _, u := range updates {
			collides := false
			if _, dup := batch[string(u.newKey)]; dup {
				collides = true
			} else if _, exists, gerr := store.Get(u.newKey); gerr != nil {
				return outcome{}, gerr
			} else if _, own := rewritten[string(u.newKey)]; exists && !own {
				collides = true
			}
			if collides {
				return outcome{}, newError(UniqueViolation,
					"duplicate key value violates unique constraint: "+strings.ToLower(table.Name)+"_pkey")
			}
			batch[string(u.newKey)] = struct{}{}
		}
	}

	// UNIQUE validation against the statement's END STATE (indexes.md §8 — a documented
	// PG divergence: PG checks per-row in heap order, so a transient collision like
	// `SET v = v + 1` fails there and succeeds here). Per unique index in catalog (name)
	// order, over the rewritten rows in scan (storage-key) order: the new prefixes must
	// not collide with each other (in-batch), nor with an existing entry whose suffix is
	// NOT a rewritten row's key (a rewritten row's old entry is being replaced, so it
	// cannot conflict). Unmetered validation, phase 1.
	if len(updates) > 0 {
		rewritten := make(map[string]struct{}, len(updates))
		for _, u := range updates {
			rewritten[string(u.key)] = struct{}{}
		}
		for _, def := range table.Indexes {
			if !def.Unique {
				continue
			}
			istore := db.lkpIndexStoreScoped(upd.DB, strings.ToLower(def.Name))
			batch := make(map[string]struct{})
			for _, u := range updates {
				prefix, ok, err := indexPrefixKey(table.Columns, colls, def, u.row)
				if err != nil {
					return outcome{}, err
				}
				if !ok {
					continue
				}
				conflict := false
				if _, dup := batch[string(prefix)]; dup {
					conflict = true
				} else {
					entries, err := istore.RangeEntries(uniqueProbeBound(prefix))
					if err != nil {
						return outcome{}, err
					}
					for _, e := range entries {
						if _, own := rewritten[string(e.Key[len(prefix):])]; !own {
							conflict = true
							break
						}
					}
				}
				if conflict {
					return outcome{}, newError(UniqueViolation,
						"duplicate key value violates unique constraint: "+def.Name)
				}
				batch[string(prefix)] = struct{}{}
			}
		}
	}

	// EXCLUDE end-state validation (spec/design/gist.md §7), mirroring UNIQUE's: each updated NEW row
	// must conflict with no OTHER row in the statement's END STATE — neither a STORED row that is NOT
	// being updated (probe the backing GiST tree, drop a hit whose storage key is a rewritten OLD key
	// — that row is vacated) nor another updated NEW row (pairwise). The NULL rule / empty-range
	// exempt a row. An end-state-valid swap thus succeeds where PG fails the per-row transient (the
	// documented UNIQUE end-state divergence). Unmetered, phase 1, before any write.
	if len(table.Exclusions) > 0 && len(updates) > 0 {
		rewritten := make(map[string]struct{}, len(updates))
		for _, u := range updates {
			rewritten[string(u.key)] = struct{}{}
		}
		for _, exc := range table.Exclusions {
			ikey := strings.ToLower(exc.Index)
			for _, u := range updates {
				q, strats, ok := exclusionProbeQuery(table.Columns, exc, u.row)
				if !ok {
					continue
				}
				conflict := false
				if tree := db.readSnap().gistTreeFor(ikey); tree != nil {
					hits, _, _ := tree.search(q, strats)
					for _, h := range hits {
						if _, own := rewritten[string(h)]; !own {
							conflict = true
							break
						}
					}
				}
				if conflict {
					return outcome{}, newError(ExclusionViolation,
						"conflicting key value violates exclusion constraint: "+exc.Name)
				}
			}
			for i := range updates {
				for j := 0; j < i; j++ {
					if exclusionPairConflicts(table.Columns, exc, updates[i].row, updates[j].row) {
						return outcome{}, newError(ExclusionViolation,
							"conflicting key value violates exclusion constraint: "+exc.Name)
					}
				}
			}
		}
	}

	// FOREIGN KEY child-side (constraints.md §6.4): re-validate an FK only when the statement
	// assigns one of its local columns (an unchanged value stays valid). Each updated NEW row must
	// reference an existing parent key — committed parent state, plus (for a self-reference) the
	// updated rows' new referenced values, so a row may reference a value another updated row now
	// supplies. Unmetered, phase 1, before any write.
	relation := table.Name
	assigned := make(map[int]struct{}, len(plans))
	for _, p := range plans {
		assigned[p.idx] = struct{}{}
	}
	for fki := range table.ForeignKeys {
		fk := &table.ForeignKeys[fki]
		touched := false
		for _, c := range fk.Columns {
			if _, ok := assigned[c]; ok {
				touched = true
				break
			}
		}
		if !touched {
			continue // this FK's local columns were not assigned
		}
		parent, ok := db.Table(fk.RefTable)
		if !ok {
			continue
		}
		// The probe matches the parent's stored key, so a collated parent key column uses the
		// PARENT's collation (§2.12).
		parentColls := db.columnCollations(parent.Columns)
		batch := make(map[string]struct{})
		if strings.EqualFold(fk.RefTable, relation) {
			for _, u := range updates {
				probe, ok, err := buildFkProbe(fk, parent, parentColls, u.row, fk.RefColumns)
				if err != nil {
					return outcome{}, err
				}
				if ok {
					batch[string(probe.bytes)] = struct{}{}
				}
			}
		}
		for _, u := range updates {
			probe, ok, err := buildFkProbe(fk, parent, parentColls, u.row, fk.Columns)
			if err != nil {
				return outcome{}, err
			}
			if !ok {
				continue // a NULL local column → exempt (MATCH SIMPLE)
			}
			if _, inBatch := batch[string(probe.bytes)]; inBatch {
				continue
			}
			hit, err := db.fkProbeHits(probe, fk.RefTable)
			if err != nil {
				return outcome{}, err
			}
			if !hit {
				return outcome{}, newError(ForeignKeyViolation,
					"insert or update on table "+relation+" violates foreign key constraint "+fk.Name)
			}
		}
	}

	// FOREIGN KEY parent-side (constraints.md §6.5): an UPDATE of a referenced row must not strand
	// a child. A referenced column — PRIMARY KEY (now re-keyable) or UNIQUE — may change. For each
	// inbound FK, a referenced tuple DISAPPEARS when an updated row's old value is absent from the
	// statement's new end state (old − new over the updated rows); if a child still references a
	// disappearing tuple → 23503. Unmetered, phase 1. A self-reference's child IS this table: the
	// committed scan excludes the rows being updated (their NEW references are checked separately,
	// newChildRefs, since a re-key can leave an updated row pointing at its own now-vacated value —
	// the child-side probe reads the pre-update parent, so it cannot see that).
	referencers := db.fkReferencers(upd.Table)
	if len(referencers) > 0 {
		parent, _ := db.Table(upd.Table)
		updatedKeys := make(map[string]struct{}, len(updates))
		for _, u := range updates {
			updatedKeys[string(u.key)] = struct{}{}
		}
		empty := map[string]struct{}{}
		for ri := range referencers {
			r := &referencers[ri]
			selfRef := strings.EqualFold(r.childTable, upd.Table)
			// parent is the update target itself, so its key columns use colls (§2.12).
			// The referenced tuples the updated rows now supply (so a swap re-supplies one).
			newPresent := make(map[string]struct{})
			for _, u := range updates {
				probe, ok, err := buildFkProbe(&r.fk, parent, colls, u.row, r.fk.RefColumns)
				if err != nil {
					return outcome{}, err
				}
				if ok {
					newPresent[string(probe.bytes)] = struct{}{}
				}
			}
			// For a self-reference, the FK tuples the updated rows now POINT AT (their new
			// local-column values): an updated row referencing a disappearing tuple dangles.
			newChildRefs := make(map[string]struct{})
			if selfRef {
				for _, u := range updates {
					probe, ok, err := buildFkProbe(&r.fk, parent, colls, u.row, r.fk.Columns)
					if err != nil {
						return outcome{}, err
					}
					if ok {
						newChildRefs[string(probe.bytes)] = struct{}{}
					}
				}
			}
			exclude := empty
			if selfRef {
				exclude = updatedKeys
			}
			for _, u := range updates {
				oldProbe, ok, err := buildFkProbe(&r.fk, parent, colls, u.oldRow, r.fk.RefColumns)
				if err != nil {
					return outcome{}, err
				}
				if !ok {
					continue // a NULL old referenced value was referenced by nothing
				}
				// Unchanged tuples (incl. a NULL → already skipped) do not disappear.
				newProbe, ok, err := buildFkProbe(&r.fk, parent, colls, u.row, r.fk.RefColumns)
				if err != nil {
					return outcome{}, err
				}
				if ok {
					if bytes.Equal(newProbe.bytes, oldProbe.bytes) {
						continue
					}
				}
				// Re-supplied by another updated row (e.g. a value swap) → not disappearing.
				if _, present := newPresent[string(oldProbe.bytes)]; present {
					continue
				}
				// Stranded if a committed (non-updated) child OR an updated row's NEW reference
				// still points at the disappearing tuple.
				referenced, err := db.fkChildReferences(r.childTable, &r.fk, parent, oldProbe.bytes, exclude)
				if err != nil {
					return outcome{}, err
				}
				if _, dangles := newChildRefs[string(oldProbe.bytes)]; referenced || dangles {
					return outcome{}, newError(ForeignKeyViolation,
						"update or delete on table "+parent.Name+" violates foreign key constraint "+r.fk.Name+" on table "+r.childTable)
				}
			}
		}
	}

	// Each rewritten row's disposition plan may attempt compression (a record over RECORD_MAX)
	// — meter the attempts (value_compress, cost.md §3) and enforce the ceiling BEFORE phase 2
	// writes anything, preserving all-or-nothing.
	var cunits int64
	for _, u := range updates {
		cunits += int64(store.WriteCompressUnits(u.newKey, u.row))
	}
	meter.Charge(costs.ValueCompress * cunits)
	if err := meter.Guard(); err != nil {
		return outcome{}, err
	}

	// The RETURNING projection (grammar.md §32, cost.md §3): evaluate over the matched rows'
	// NEW (post-assignment, fully resident) values — all validation has passed, nothing is
	// written yet, so subqueries in the list read the pre-statement snapshot and a 54P01 here
	// writes nothing (all-or-nothing).
	var returned [][]Value
	if retNodes != nil {
		prows := make([]storedRow, len(updates))
		olds := make([]storedRow, len(updates))
		for i := range updates {
			prows[i] = updates[i].row
			olds[i] = updates[i].oldRow
		}
		if returned, err = db.projectReturning(retNodes, prows, olds, bound, ctx, meter); err != nil {
			return outcome{}, err
		}
	}

	// Index maintenance (indexes.md §4): an entry moves only when its key CHANGED — equal
	// old/new keys leave the index tree untouched (part of the contract: it keeps the
	// copy-on-write dirty set, and so the commit's written pages, byte-identical across
	// cores). An entry key is `indexed-cols || storage-key`, so a re-keyed row moves EVERY
	// one of its entries (the suffix changed); a non-PK UPDATE keeps the suffix and moves
	// only entries whose indexed columns changed.
	type indexMove struct{ removals, insertions [][]byte }
	indexMoves := make([][]indexMove, len(table.Indexes))
	for _, u := range updates {
		for k, def := range table.Indexes {
			// The row's old and new entry SETS (one entry for an ordered index, one per term for
			// GIN — gin.md §5). Remove old−new, insert new−old: a shared entry is left untouched,
			// keeping the copy-on-write dirty set byte-identical across cores.
			oldEks, err := indexEntryKeys(table.Columns, colls, def, u.key, u.oldRow)
			if err != nil {
				return outcome{}, err
			}
			newEks, err := indexEntryKeys(table.Columns, colls, def, u.newKey, u.row)
			if err != nil {
				return outcome{}, err
			}
			removals := bytesDiff(oldEks, newEks)
			insertions := bytesDiff(newEks, oldEks)
			if len(removals) > 0 || len(insertions) > 0 {
				indexMoves[k] = append(indexMoves[k], indexMove{removals: removals, insertions: insertions})
			}
		}
	}

	// Phase 2: write the validated rows, then move the changed index entries (unmetered write
	// work). Writes land in working (writeStore), even when the scan above read the pin. A non-PK
	// UPDATE replaces each row in place (the fast path). A re-keying UPDATE vacates every OLD key
	// first and then places each row at its NEW key — a two-pass so a chain or swap of keys among
	// the updated rows never transiently collides (the end state is collision-free, validated
	// above). The index entries move the same way (all removals across rows, then all insertions),
	// since a moved row's new entry can equal another moved row's not-yet-removed old entry.
	if pkChanged {
		for _, u := range updates {
			if _, err := writeStore.Remove(u.key); err != nil {
				return outcome{}, err
			}
		}
		for _, u := range updates {
			inserted, err := writeStore.Insert(u.newKey, u.row)
			if err != nil {
				return outcome{}, err
			}
			if !inserted {
				// Reachable only under the writable-CTE read pin (writable-cte.md §7): an earlier
				// sub-statement staged this key, unseen by phase 1. Aborts all-or-nothing, matching
				// INSERT. For a single statement, phase 1's end-state check caught every duplicate.
				return outcome{}, newError(UniqueViolation,
					"duplicate key value violates unique constraint: "+strings.ToLower(table.Name)+"_pkey")
			}
		}
		for k, def := range table.Indexes {
			istore := db.writeIndexStoreScoped(upd.DB, strings.ToLower(def.Name))
			for _, mv := range indexMoves[k] {
				for _, oldEk := range mv.removals {
					if _, err := istore.Remove(oldEk); err != nil {
						return outcome{}, err
					}
				}
			}
			for _, mv := range indexMoves[k] {
				for _, newEk := range mv.insertions {
					inserted, err := istore.Insert(newEk, nil)
					if err != nil {
						return outcome{}, err
					}
					if !inserted {
						// A cross-sub-statement collision under the read pin (as above).
						return outcome{}, newError(UniqueViolation,
							"duplicate key value violates unique constraint: "+def.Name)
					}
				}
			}
		}
	} else {
		for _, u := range updates {
			if err := writeStore.Replace(u.key, u.row); err != nil {
				return outcome{}, err
			}
		}
		for k, def := range table.Indexes {
			istore := db.writeIndexStoreScoped(upd.DB, strings.ToLower(def.Name))
			for _, mv := range indexMoves[k] {
				for _, oldEk := range mv.removals {
					if _, err := istore.Remove(oldEk); err != nil {
						return outcome{}, err
					}
				}
				for _, newEk := range mv.insertions {
					inserted, err := istore.Insert(newEk, nil)
					if err != nil {
						return outcome{}, err
					}
					if !inserted {
						panic("index entry keys are unique (storage-key suffix)")
					}
				}
			}
		}
	}
	return dmlOutcome(retNames, retTypes, returned, int64(len(updates)), meter.Accrued), nil
}

// RowsInKeyOrder returns a table's rows in primary-key (encoded byte) order in the visible snapshot,
// or nil if the table does not exist. A test/debug convenience — the SELECT path scans through
// IterInKeyOrder directly (propagating fault errors); these callers are in-memory, where a scan never
// faults, so the error is inert and panicking on it surfaces a genuine bug rather than hiding it.
func (db *engine) RowsInKeyOrder(name string) []storedRow {
	snap := db.readSnap()
	if db.isTempTable(name) { // temp tables live in the session temp snapshot (temp-tables.md §2)
		snap = db.tempSnap()
	}
	store, ok := snap.stores[strings.ToLower(name)]
	if !ok {
		return nil
	}
	rows, err := store.IterInKeyOrder()
	if err != nil {
		panic(err)
	}
	// Fully materialize every value — the helper's callers compare whole rows, so no
	// unfetched reference may escape (large-values.md §14).
	for i := range rows {
		if rows[i], err = store.resolveAll(rows[i]); err != nil {
			panic(err)
		}
	}
	return rows
}

// selectResult is the full result of running a SELECT (runSelect): the output column names and
// their resolved types, the rows in result order, and the accrued cost. Internal to the
// executor — executeSelect drops the types into the public outcome, while INSERT ... SELECT uses
// the types to gate assignability up front (spec/design/grammar.md §24).
type selectResult struct {
	columnNames []string
	columnTypes []resolvedType
	rows        [][]Value
	cost        int64
}

// executeSelect runs a SELECT as a top-level statement: runSelect, then wrap as a query outcome
// (the projection types are internal — only INSERT ... SELECT consumes them).
func (db *engine) executeSelect(sel *selectStmt, params []Value) (outcome, error) {
	r, err := db.runSelect(sel, params)
	if err != nil {
		return outcome{}, err
	}
	return outcome{Kind: outcomeQuery, ColumnNames: r.columnNames, ColumnTypes: typeNames(r.columnTypes), Rows: r.rows, Cost: r.cost}, nil
}

// executeSetOp runs a set operation as a top-level statement: runSetOp, then wrap as a query
// outcome. Cost is lhs.cost + rhs.cost — the combine, sort, and window are unmetered (cost.md §3).
func (db *engine) executeSetOp(so *setOp, params []Value) (outcome, error) {
	r, err := db.runSetOp(so, params)
	if err != nil {
		return outcome{}, err
	}
	return outcome{Kind: outcomeQuery, ColumnNames: r.columnNames, ColumnTypes: typeNames(r.columnTypes), Rows: r.rows, Cost: r.cost}, nil
}

// executeWith runs a WITH query (spec/design/cte.md) — the host-API entry point; runWith does the
// CTE orchestration.
func (db *engine) executeWith(wq *withQuery, params []Value) (outcome, error) {
	// A WITH containing any data-modifying part (a data-modifying CTE or a data-modifying primary)
	// runs through the writable-CTE orchestrator (spec/design/writable-cte.md): it pins the
	// pre-statement snapshot and runs the parts in lexical order, all-or-nothing. A pure-query WITH
	// keeps the existing read-only path (cte.md) unchanged.
	if withHasDml(wq) {
		return db.executeWithDml(wq, params)
	}
	r, err := db.runWith(wq, params)
	if err != nil {
		return outcome{}, err
	}
	return outcome{Kind: outcomeQuery, ColumnNames: r.columnNames, ColumnTypes: typeNames(r.columnTypes), Rows: r.rows, Cost: r.cost}, nil
}

// planCteBindings plans every CTE in a WITH list into bindings (spec/design/cte.md §2,
// writable-cte.md). Each body is planned against the prefix of EARLIER bindings (parent = nil — a
// body is an independent query, NOT correlated to a reference site). Under WITH RECURSIVE a query CTE
// that references its own name is the recursive shape (its binding is pushed BEFORE planning the
// recursive term, so the self-reference resolves to it). A data-modifying CTE body resolves only its
// RETURNING schema here (its effect runs later, in the orchestrator) — a data-modifying body is never
// the recursive UNION shape, so it is always non-recursive. The refs counters are bumped as later
// query bodies / a query primary reference each binding (a data-modifying part's references are
// static-counted by the orchestrator, since it is not planned here).
func (db *engine) planCteBindings(ctes []cte, recursive bool, ptypes *paramTypes) ([]*cteBinding, error) {
	bindings := make([]*cteBinding, 0, len(ctes))
	for i := range ctes {
		cte := &ctes[i]
		lname := strings.ToLower(cte.Name)
		for _, b := range bindings {
			if b.name == lname {
				return nil, newError(DuplicateAlias,
					"WITH query name "+lname+" specified more than once")
			}
		}
		isRecursive, unionAll := false, false
		if recursive {
			if q := cte.Body.AsQuery(); q != nil {
				rec, ua, err := analyzeRecursiveCte(lname, *q)
				if err != nil {
					return nil, err
				}
				isRecursive, unionAll = rec, ua
			}
		}
		if isRecursive {
			// The body is `anchor UNION[ALL] recursive_term` (analyzeRecursiveCte verified).
			so := cte.Body.AsQuery().SetOp
			anchorPlan, err := db.planQuery(so.Lhs, nil, bindings, ptypes)
			if err != nil {
				return nil, err
			}
			table, err := cteSyntheticTable(lname, &anchorPlan, cte.Columns)
			if err != nil {
				return nil, err
			}
			bindings = append(bindings, &cteBinding{
				name: lname, table: table, plan: anchorPlan, hint: cte.Materialized,
			})
			bi := len(bindings) - 1
			rhsPlan, err := db.planQuery(so.Rhs, nil, bindings, ptypes)
			if err != nil {
				return nil, err
			}
			if err := checkRecursiveColumnTypes(&bindings[bi].plan, &rhsPlan, lname); err != nil {
				return nil, err
			}
			bindings[bi].recursive = &recursiveTerm{plan: rhsPlan, unionAll: unionAll}
			continue
		}
		if q := cte.Body.AsQuery(); q != nil {
			plan, err := db.planQuery(*q, nil, bindings, ptypes)
			if err != nil {
				return nil, err
			}
			table, err := cteSyntheticTable(lname, &plan, cte.Columns)
			if err != nil {
				return nil, err
			}
			bindings = append(bindings, &cteBinding{
				name: lname, table: table, plan: plan, hint: cte.Materialized,
			})
			continue
		}
		// A data-modifying CTE (writable-cte.md): resolve its RETURNING schema for the synthetic
		// relation + capture the statement to run later.
		table, dm, err := db.planDmCte(lname, &cte.Body, bindings, cte.Columns, ptypes)
		if err != nil {
			return nil, err
		}
		bindings = append(bindings, &cteBinding{
			name: lname, table: table, dm: dm, hint: cte.Materialized,
		})
	}
	return bindings, nil
}

// planDmCte plans a data-modifying CTE body (spec/design/writable-cte.md): resolve its RETURNING
// schema (against the EARLIER bindings, so a RETURNING sublink may reference an earlier CTE) to build
// the synthetic relation, and capture the statement to execute later. A body with no RETURNING yields
// a zero-column relation flagged noReturning (a FROM reference to it is 0A000, §5). The target must
// be a base table — a CTE name / missing table is 42P01 (§1).
func (db *engine) planDmCte(lname string, body *cteBody, bindings []*cteBinding, rename []string, ptypes *paramTypes) (*catTable, *dmCte, error) {
	var tableName string
	var returning *selectItems
	var baseIsOld bool
	dm := &dmCte{}
	switch {
	case body.Insert != nil:
		tableName, returning, baseIsOld = body.Insert.Table, body.Insert.Returning, false
		dm.insert = body.Insert
	case body.Update != nil:
		tableName, returning, baseIsOld = body.Update.Table, body.Update.Returning, false
		dm.update = body.Update
	default:
		tableName, returning, baseIsOld = body.Delete.Table, body.Delete.Returning, true
		dm.delete = body.Delete
	}
	tdef, ok := db.lkpTable(tableName) // temp-first (temp-tables.md §3)
	if !ok {
		return nil, nil, newError(UndefinedTable, "table does not exist: "+tableName)
	}
	if returning == nil {
		dm.noReturning = true
		table, err := cteSyntheticTableCols(lname, nil, nil, rename)
		if err != nil {
			return nil, nil, err
		}
		return table, dm, nil
	}
	s := returningScope(db, tdef, baseIsOld)
	s.ctes = bindings
	_, names, types, err := resolveProjections(s, *returning, &aggCtx{collecting: false}, ptypes)
	if err != nil {
		return nil, nil, err
	}
	table, err := cteSyntheticTableCols(lname, names, types, rename)
	if err != nil {
		return nil, nil, err
	}
	return table, dm, nil
}

// runWith runs a pure-query WITH (spec/design/cte.md) — the path for a WITH with no data-modifying
// part (a data-modifying WITH goes through executeWithDml). (1) PLAN every CTE binding against the
// prefix; (2) plan the main body with all bindings visible; (3) decide each CTE's mode from its
// reference count + [NOT] MATERIALIZED hint; (4) MATERIALIZE each referenced materialized CTE once,
// in list order (a later body sees the earlier buffers); (5) fold + EXECUTE the main body with the
// CTE context. Cost composes like set operations — a sum of the parts.
func (db *engine) runWith(wq *withQuery, params []Value) (selectResult, error) {
	ptypes := &paramTypes{}
	bindings, err := db.planCteBindings(wq.Ctes, wq.Recursive, ptypes)
	if err != nil {
		return selectResult{}, err
	}
	// (2) Plan the main body with all bindings visible (the pure-query path always has a query primary
	//     — a data-modifying primary routes to executeWithDml).
	bodyQ := wq.Body.AsQuery()
	plan, err := db.planQuery(*bodyQ, nil, bindings, ptypes)
	if err != nil {
		return selectResult{}, err
	}
	ptys, err := ptypes.finalize()
	if err != nil {
		return selectResult{}, err
	}
	bound, err := bindParams(params, ptys)
	if err != nil {
		return selectResult{}, err
	}
	modes := cteModes(bindings)
	buffers, totalCost, err := db.materializeCtes(bindings, modes, bound)
	if err != nil {
		return selectResult{}, err
	}

	// (5) Fold + execute the main body against the full CTE context.
	ctx := cteCtx{modes: modes, bindings: bindings, buffers: buffers}
	var subqueryCost int64
	if err := db.foldUncorrelatedInPlan(&plan, bound, ctx, &subqueryCost); err != nil {
		return selectResult{}, err
	}
	r, err := db.execQueryPlan(&plan, nil, bound, ctx)
	if err != nil {
		return selectResult{}, err
	}
	r.cost += subqueryCost + totalCost
	return r, nil
}

// materializeCtes materializes each CTE once, in list order (spec/design/cte.md §3) — the shared loop
// for the pure-query and writable-CTE paths' query/recursive CTEs. A data-modifying CTE is NOT run
// here (the orchestrator runs it for its effect — executeWithDml); its buffer slot is left empty for
// the orchestrator to fill. Returns the filled buffers + the accrued materialization cost (a later
// body sees the earlier buffers).
func (db *engine) materializeCtes(bindings []*cteBinding, modes []cteMode, bound []Value) ([][]storedRow, int64, error) {
	var totalCost int64
	buffers := make([][]storedRow, 0, len(bindings))
	for i := range bindings {
		var buf []storedRow
		switch {
		case bindings[i].recursive != nil:
			b, err := db.materializeRecursive(i, bindings[i].recursive, modes, bindings, buffers, bound, &totalCost)
			if err != nil {
				return nil, 0, err
			}
			buf = b
		case bindings[i].isDml():
			// A data-modifying CTE's buffer is filled by the orchestrator, not here.
		case modes[i] == cteMaterialize:
			ctx := cteCtx{modes: modes[:i], bindings: bindings[:i], buffers: buffers}
			cplan := bindings[i].plan
			r, err := db.execQueryPlan(&cplan, nil, bound, ctx)
			if err != nil {
				return nil, 0, err
			}
			totalCost += r.cost
			buf = rowsFromValues(r.rows)
		}
		buffers = append(buffers, buf)
	}
	return buffers, totalCost, nil
}

// materializeRecursive materializes a RECURSIVE CTE by iterating to a fixpoint — the PostgreSQL
// working-table method (spec/design/recursive-cte.md §4). rt is the recursive term (which references
// this CTE, index ci); the anchor is bindings[ci].plan. priorBuffers are the earlier CTEs'
// materialized rows (visible to both terms). totalCost accrues every term evaluation's cost and gates
// the per-statement ceiling between iterations, so a non-terminating recursion of cheap iterations
// still aborts 54P01 at the identical accrued cost in every core (recursive-cte.md §5).
func (db *engine) materializeRecursive(ci int, rt *recursiveTerm,
	modes []cteMode, bindings []*cteBinding, priorBuffers [][]storedRow, params []Value, totalCost *int64,
) ([]storedRow, error) {
	anchorPlan := &bindings[ci].plan
	maxCost := db.session.maxCost
	guard := func(total int64) error {
		if maxCost > 0 && total >= maxCost {
			return newError(CostLimitExceeded, fmt.Sprintf(
				"query exceeded the cost limit of %d (accrued %d)", maxCost, total,
			))
		}
		return nil
	}
	anchorTypes := anchorPlan.columnTypes()
	rhsTypes := rt.plan.columnTypes()

	// Evaluate the anchor: its rows seed both the result and the first working table.
	ctx0 := cteCtx{modes: modes[:ci], bindings: bindings[:ci], buffers: priorBuffers}
	ar, err := db.execQueryPlan(anchorPlan, nil, params, ctx0)
	if err != nil {
		return nil, err
	}
	*totalCost += ar.cost
	if err := guard(*totalCost); err != nil {
		return nil, err
	}

	// For UNION (distinct) a seen set drops rows duplicating any already-emitted row, keyed by the
	// NULL-safe distinctRowKey the set operators use.
	seen := map[string]bool{}
	keep := func(row storedRow) bool {
		if rt.unionAll {
			return true
		}
		k := distinctRowKey(row)
		if seen[k] {
			return false
		}
		seen[k] = true
		return true
	}
	var result, working []storedRow
	for _, row := range ar.rows {
		if keep(row) {
			result = append(result, row)
			working = append(working, row)
		}
	}

	// The recursive term scans the WORKING table through the CTE's own buffer slot (ci); the earlier
	// CTEs keep their full buffers. Build the buffer vec once and swap slot ci per iteration.
	rhsBuffers := make([][]storedRow, ci+1)
	copy(rhsBuffers, priorBuffers)

	for len(working) > 0 {
		rhsBuffers[ci] = working
		working = nil
		ctx := cteCtx{modes: modes[:ci+1], bindings: bindings[:ci+1], buffers: rhsBuffers}
		cplan := rt.plan
		rr, err := db.execQueryPlan(&cplan, nil, params, ctx)
		if err != nil {
			return nil, err
		}
		*totalCost += rr.cost
		if err := guard(*totalCost); err != nil {
			return nil, err
		}
		coerceSetopRows(rr.rows, rhsTypes, anchorTypes)
		for _, vrow := range rr.rows {
			row := storedRow(vrow)
			if keep(row) {
				result = append(result, row)
				working = append(working, row)
			}
		}
	}
	return result, nil
}

// executeWithDml runs a data-modifying WITH statement (spec/design/writable-cte.md): a WITH
// containing a data-modifying CTE and/or a data-modifying primary. It PINS the pre-statement snapshot
// for every sub-statement's reads (§2 — so the parts cannot see each other's table writes; data
// crosses only via a CTE's RETURNING buffer), runs the parts in lexical order, and returns the
// primary's result. The whole statement is one all-or-nothing transaction — the autocommit (or block)
// wrapper publishes the accumulated working only if this returns nil error (§6).
func (db *engine) executeWithDml(wq *withQuery, params []Value) (outcome, error) {
	// Pin the pre-statement snapshot. A write statement runs with a transaction open (autocommit
	// opened one), and nothing is written yet, so the pin equals working == committed. Cleared on
	// every exit path so the next statement reads normally.
	db.session.readPin = db.readSnap().clone()
	out, err := db.runWithDml(wq, params)
	db.session.readPin = nil
	return out, err
}

// runWithDml is the body of executeWithDml, run under the read pin. Plans every CTE binding + the
// query primary, runs the data-modifying CTEs / materialized query CTEs in list order, then the
// primary — every read against the pin, every write into the transaction's working.
func (db *engine) runWithDml(wq *withQuery, params []Value) (outcome, error) {
	ptypes := &paramTypes{}
	// (1) Plan every CTE binding (query plans + data-modifying RETURNING schemas).
	bindings, err := db.planCteBindings(wq.Ctes, wq.Recursive, ptypes)
	if err != nil {
		return outcome{}, err
	}
	// (2) Plan a query primary now (to bump refs + surface resolution errors, incl. a 0A000 FROM
	//     reference to a no-RETURNING data-modifying CTE). A data-modifying primary is resolved and
	//     run later (it sees the bindings via the threaded context); its references are static-counted
	//     in (2b).
	var primaryPlan *queryPlan
	if q := wq.Body.AsQuery(); q != nil {
		p, perr := db.planQuery(*q, nil, bindings, ptypes)
		if perr != nil {
			return outcome{}, perr
		}
		primaryPlan = &p
	}
	// (2b) Add the references each NON-planned data-modifying part (a data-modifying CTE body, or a
	//      data-modifying primary) contributes to each binding, so the inline-vs-materialize decision
	//      is correct for a query CTE referenced only by a data-modifying part (§3). Query bodies / a
	//      query primary were already plan-counted in (1)/(2).
	for i := range wq.Ctes {
		if wq.Ctes[i].Body.IsDataModifying() {
			for _, b := range bindings {
				b.refs += countCteRefsDml(&wq.Ctes[i].Body, b.name)
			}
		}
	}
	if wq.Body.IsDataModifying() {
		for _, b := range bindings {
			b.refs += countCteRefsDml(&wq.Body, b.name)
		}
	}
	ptys, err := ptypes.finalize()
	if err != nil {
		return outcome{}, err
	}
	bound, err := bindParams(params, ptys)
	if err != nil {
		return outcome{}, err
	}
	modes := cteModes(bindings)

	// (3) Run each CTE in list order, filling its buffer. A data-modifying CTE executes for its effect
	//     + RETURNING buffer; the query/recursive CTEs use the shared materialize loop's logic.
	var totalCost int64
	buffers := make([][]storedRow, 0, len(bindings))
	for i := range bindings {
		var buf []storedRow
		switch {
		case bindings[i].recursive != nil:
			b, rerr := db.materializeRecursive(i, bindings[i].recursive, modes, bindings, buffers, bound, &totalCost)
			if rerr != nil {
				return outcome{}, rerr
			}
			buf = b
		case bindings[i].isDml():
			ctx := cteCtx{modes: modes[:i], bindings: bindings[:i], buffers: buffers}
			rows, cost, derr := db.execDmCte(i, bindings, bound, ctx)
			if derr != nil {
				return outcome{}, derr
			}
			totalCost += cost
			buf = rows
		case modes[i] == cteMaterialize:
			ctx := cteCtx{modes: modes[:i], bindings: bindings[:i], buffers: buffers}
			cplan := bindings[i].plan
			r, rerr := db.execQueryPlan(&cplan, nil, bound, ctx)
			if rerr != nil {
				return outcome{}, rerr
			}
			totalCost += r.cost
			buf = rowsFromValues(r.rows)
		}
		buffers = append(buffers, buf)
	}

	// (4) Execute the primary against the full CTE context, adding the materialization cost.
	ctx := cteCtx{modes: modes, bindings: bindings, buffers: buffers}
	var out outcome
	switch {
	case wq.Body.AsQuery() != nil:
		var subqueryCost int64
		if err := db.foldUncorrelatedInPlan(primaryPlan, bound, ctx, &subqueryCost); err != nil {
			return outcome{}, err
		}
		r, rerr := db.execQueryPlan(primaryPlan, nil, bound, ctx)
		if rerr != nil {
			return outcome{}, rerr
		}
		out = outcome{
			Kind:        outcomeQuery,
			ColumnNames: r.columnNames,
			ColumnTypes: typeNames(r.columnTypes),
			Rows:        r.rows,
			Cost:        r.cost + subqueryCost,
		}
	case wq.Body.Insert != nil:
		out, err = db.executeInsert(wq.Body.Insert, params, ctx)
	case wq.Body.Update != nil:
		out, err = db.executeUpdate(wq.Body.Update, params, ctx)
	default:
		out, err = db.executeDelete(wq.Body.Delete, params, ctx)
	}
	if err != nil {
		return outcome{}, err
	}
	return addOutcomeCost(out, totalCost), nil
}

// execDmCte executes a data-modifying CTE (spec/design/writable-cte.md §3): run the INSERT/UPDATE/
// DELETE at binding i for its effect, with the earlier bindings/buffers in scope (so its inner
// queries may reference an earlier CTE), and return its RETURNING rows (the buffer the later parts
// scan) + its cost. A body with no RETURNING runs for its effect and buffers no rows.
func (db *engine) execDmCte(i int, bindings []*cteBinding, params []Value, ctx cteCtx) ([]storedRow, int64, error) {
	dm := bindings[i].dm
	var out outcome
	var err error
	switch {
	case dm.insert != nil:
		out, err = db.executeInsert(dm.insert, params, ctx)
	case dm.update != nil:
		out, err = db.executeUpdate(dm.update, params, ctx)
	default:
		out, err = db.executeDelete(dm.delete, params, ctx)
	}
	if err != nil {
		return nil, 0, err
	}
	if out.Kind == outcomeQuery {
		return rowsFromValues(out.Rows), out.Cost, nil
	}
	return nil, out.Cost, nil
}

// === WITH RECURSIVE analysis (spec/design/recursive-cte.md) ==========================
//
// A WITH RECURSIVE CTE is recursive iff its body references its own name (anywhere, deep). A
// recursive CTE must take the well-formed shape `non_recursive_term UNION [ALL] recursive_term`
// with the self-reference appearing exactly once, as a direct FROM/JOIN relation of the recursive
// term. These structural checks mirror PostgreSQL's checkWellFormedRecursion, run on the parsed AST
// before planning; the error surface is recursive-cte.md §6.

// analyzeRecursiveCte classifies a CTE body for WITH RECURSIVE (recursive-cte.md §6). It returns
// (false, _, nil) when the body does not reference name (an ordinary CTE, even under RECURSIVE);
// otherwise it validates the recursive shape and returns (true, unionAll, nil), or an error (42P19
// for a malformed recursion, 0A000 for a deferred shape).
func analyzeRecursiveCte(name string, body queryExpr) (bool, bool, error) {
	if countSelfRefsQuery(body, name) == 0 {
		return false, false, nil
	}
	so := body.SetOp
	if so == nil || so.Op != setOpUnion {
		return false, false, newError(InvalidRecursion, fmt.Sprintf(
			"recursive query %q does not have the form non-recursive-term UNION [ALL] recursive-term", name,
		))
	}
	if len(so.OrderBy) > 0 {
		return false, false, newError(FeatureNotSupported, "ORDER BY in a recursive query is not implemented")
	}
	if so.Limit != nil || so.Offset != nil {
		return false, false, newError(FeatureNotSupported, "LIMIT in a recursive query is not implemented")
	}
	if countSelfRefsQuery(so.Lhs, name) > 0 {
		return false, false, newError(InvalidRecursion, fmt.Sprintf(
			"recursive reference to query %q must not appear within its non-recursive term", name,
		))
	}
	if so.Rhs.With != nil {
		return false, false, newError(FeatureNotSupported,
			"a nested WITH in the recursive term of a recursive query is not supported yet")
	}
	if so.Rhs.Select == nil {
		return false, false, newError(FeatureNotSupported,
			"a set operation in the recursive term of a recursive query is not supported yet")
	}
	if err := validateRecursiveTerm(name, so.Rhs.Select); err != nil {
		return false, false, err
	}
	return true, so.All, nil
}

// validateRecursiveTerm validates the recursive term (the UNION's right SELECT) of a recursive CTE
// (recursive-cte.md §6). The self-reference must appear exactly once, as a direct FROM/JOIN
// relation, not on the nullable side of an outer join; the term must contain no aggregate. The
// checks fire in PostgreSQL's order — a self-reference in a bad CONTEXT (a sublink, an outer join)
// is reported as that context even when a valid FROM reference also exists.
func validateRecursiveTerm(name string, sel *selectStmt) error {
	if countSublinkSelfRefs(sel, name) >= 1 {
		return newError(InvalidRecursion, fmt.Sprintf(
			"recursive reference to query %q must not appear within a subquery", name,
		))
	}
	if countFromSubquerySelfRefs(sel, name) >= 1 {
		return newError(FeatureNotSupported, fmt.Sprintf(
			"recursive reference to query %q inside a FROM subquery is not supported yet", name,
		))
	}
	direct := countDirectFromSelfRefs(sel, name)
	if direct > 1 {
		return newError(InvalidRecursion, fmt.Sprintf(
			"recursive reference to query %q must not appear more than once", name,
		))
	}
	if itemsHaveAggregate(sel.Items) || (sel.Having != nil && exprHasAggregate(*sel.Having)) {
		return newError(InvalidRecursion,
			"aggregate functions are not allowed in a recursive query's recursive term")
	}
	if direct == 1 && directSelfRefOnNullableSide(sel, name) {
		return newError(InvalidRecursion, fmt.Sprintf(
			"recursive reference to query %q must not appear within an outer join", name,
		))
	}
	return nil
}

// countSelfRefsQuery counts self-references to name anywhere in a query expression (deep — FROM
// relations at every nesting level plus expression sublinks).
func countSelfRefsQuery(qe queryExpr, name string) int {
	if qe.Select != nil {
		return countSelfRefsSelect(qe.Select, name)
	}
	if qe.SetOp != nil {
		return countSelfRefsQuery(qe.SetOp.Lhs, name) + countSelfRefsQuery(qe.SetOp.Rhs, name)
	}
	return 0
}

// countSelfRefsSelect counts self-references in a SELECT: its FROM relations (deep) plus all of its
// expressions' sublinks.
func countSelfRefsSelect(s *selectStmt, name string) int {
	n := 0
	for _, tref := range fromRelations(s) {
		n += countSelfRefsTableref(tref, name)
	}
	for _, e := range selectExprs(s) {
		n += countSelfRefsExpr(e, name)
	}
	return n
}

// countSelfRefsTableref counts self-references reachable through one FROM relation: a plain table
// reference with the matching name (+1), a derived-table subquery (recurse), or a table-function's
// / VALUES' argument exprs.
func countSelfRefsTableref(tref *tableRef, name string) int {
	if isPlainRelation(tref) {
		if strings.EqualFold(tref.Name, name) {
			return 1
		}
		return 0
	}
	n := 0
	if tref.Subquery != nil {
		n += countSelfRefsQuery(*tref.Subquery, name)
	}
	for _, a := range tref.Args {
		n += countSelfRefsExpr(*a, name)
	}
	for _, row := range tref.Values {
		for _, e := range row {
			n += countSelfRefsExpr(*e, name)
		}
	}
	return n
}

// countSelfRefsExpr counts self-references inside an expression — only reachable through a sublink
// (a subquery is an independent query whose own FROM may reference the CTE). The walk is exhaustive
// (like exprHasAggregate).
func countSelfRefsExpr(e exprNode, name string) int {
	switch e.Kind {
	case exprScalarSubquery, exprExists:
		return countSelfRefsQuery(*e.Subquery, name)
	case exprInSubquery:
		return countSelfRefsExpr(e.InSubquery.Lhs, name) + countSelfRefsQuery(e.InSubquery.Query, name)
	case exprQuantifiedSubquery:
		return countSelfRefsExpr(e.QuantifiedSubquery.Lhs, name) + countSelfRefsQuery(e.QuantifiedSubquery.Query, name)
	case exprCast:
		return countSelfRefsExpr(e.Cast.Inner, name)
	case exprExtract:
		return countSelfRefsExpr(e.Extract.Source, name)
	case exprCollate:
		return countSelfRefsExpr(e.Collate.Inner, name)
	case exprUnary:
		return countSelfRefsExpr(e.Unary.Operand, name)
	case exprIsNull:
		return countSelfRefsExpr(e.IsNullOf.Operand, name)
	case exprIsJson:
		return countSelfRefsExpr(e.IsJsonOf.Operand, name)
	case exprJsonCtor:
		return countSelfRefsExpr(e.JsonCtorOf.Operand, name)
	case exprJsonExists:
		return countSelfRefsExpr(e.JsonExists.Ctx, name) + countSelfRefsExpr(e.JsonExists.Path, name)
	case exprJsonValue:
		return countSelfRefsExpr(e.JsonValue.Ctx, name) + countSelfRefsExpr(e.JsonValue.Path, name)
	case exprJsonQuery:
		return countSelfRefsExpr(e.JsonQuery.Ctx, name) + countSelfRefsExpr(e.JsonQuery.Path, name)
	case exprBinary:
		return countSelfRefsExpr(e.Binary.Lhs, name) + countSelfRefsExpr(e.Binary.Rhs, name)
	case exprIsDistinct:
		return countSelfRefsExpr(e.IsDistinct.Lhs, name) + countSelfRefsExpr(e.IsDistinct.Rhs, name)
	case exprIn:
		n := countSelfRefsExpr(e.In.Lhs, name)
		for _, x := range e.In.List {
			n += countSelfRefsExpr(x, name)
		}
		return n
	case exprBetween:
		return countSelfRefsExpr(e.Between.Lhs, name) + countSelfRefsExpr(e.Between.Lo, name) + countSelfRefsExpr(e.Between.Hi, name)
	case exprLike:
		return countSelfRefsExpr(e.Like.Lhs, name) + countSelfRefsExpr(e.Like.Rhs, name)
	case exprRegex:
		return countSelfRefsExpr(e.Regex.Lhs, name) + countSelfRefsExpr(e.Regex.Rhs, name)
	case exprCase:
		n := 0
		if e.Case.Operand != nil {
			n += countSelfRefsExpr(*e.Case.Operand, name)
		}
		for _, w := range e.Case.Whens {
			n += countSelfRefsExpr(w.Cond, name) + countSelfRefsExpr(w.Result, name)
		}
		if e.Case.Els != nil {
			n += countSelfRefsExpr(*e.Case.Els, name)
		}
		return n
	case exprFuncCall:
		n := 0
		for _, a := range e.FuncCall.Args {
			n += countSelfRefsExpr(*a, name)
		}
		return n
	case exprFieldAccess, exprFieldStar:
		return countSelfRefsExpr(*e.Base, name)
	case exprQualifiedStar:
		return 0 // a leaf relation reference — no sublink to recurse into

	case exprSubscript:
		n := countSelfRefsExpr(*e.Base, name)
		for _, sp := range e.Subscripts {
			for _, x := range subscriptSpecExprs(sp) {
				n += countSelfRefsExpr(*x, name)
			}
		}
		return n
	case exprRow, exprArray:
		n := 0
		for _, it := range e.RowItems {
			n += countSelfRefsExpr(it, name)
		}
		return n
	case exprQuantified:
		return countSelfRefsExpr(e.Quantified.Lhs, name) + countSelfRefsExpr(e.Quantified.Array, name)
	default:
		return 0
	}
}

// withHasDml reports whether a WITH statement contains any data-modifying part — a data-modifying
// CTE body or a data-modifying primary (spec/design/writable-cte.md). Such a statement runs through
// the writable-CTE orchestrator (the read pin + lexical-order, all-or-nothing execution); a
// pure-query WITH keeps the runWith path.
func withHasDml(wq *withQuery) bool {
	if wq.Body.IsDataModifying() {
		return true
	}
	for i := range wq.Ctes {
		if wq.Ctes[i].Body.IsDataModifying() {
			return true
		}
	}
	return false
}

// cteModes returns each CTE binding's evaluation mode (spec/design/cte.md §3, writable-cte.md §3): a
// RECURSIVE or data-modifying CTE is ALWAYS materialized; otherwise a MATERIALIZED hint or ≥2
// references → Materialize, else Inline.
func cteModes(bindings []*cteBinding) []cteMode {
	modes := make([]cteMode, len(bindings))
	for i, b := range bindings {
		switch {
		case b.recursive != nil || b.isDml():
			modes[i] = cteMaterialize
		case b.hint != nil && *b.hint:
			modes[i] = cteMaterialize
		case b.hint != nil && !*b.hint:
			modes[i] = cteInline
		case b.refs >= 2:
			modes[i] = cteMaterialize
		default:
			modes[i] = cteInline
		}
	}
	return modes
}

// addOutcomeCost adds extra cost to an outcome (the writable-CTE orchestrator folds the
// materialization cost of the data-modifying / query CTEs into the primary's result —
// spec/design/writable-cte.md §8).
func addOutcomeCost(outcome outcome, extra int64) outcome {
	outcome.Cost += extra
	return outcome
}

// countCteRefsDml counts references to CTE name reachable through a cte_body's inner queries — the
// writable-CTE analogue of countSelfRefsQuery (spec/design/writable-cte.md §3). A query body
// delegates to the query counter; a data-modifying body counts the references in its source query /
// WHERE / SET RHSs / ON CONFLICT / RETURNING sublinks. Used by the orchestrator to count the
// references a NON-planned data-modifying part contributes to the inline-vs-materialize decision.
func countCteRefsDml(body *cteBody, name string) int {
	switch {
	case body.Query != nil:
		return countSelfRefsQuery(*body.Query, name)
	case body.Insert != nil:
		ins := body.Insert
		n := 0
		// VALUES slots hold literals / params / ROW / ARRAY (no sublinks this slice); only a SELECT
		// source can reference a CTE.
		if ins.Select != nil {
			n += countSelfRefsSelect(ins.Select, name)
		}
		if ins.OnConflict != nil && ins.OnConflict.DoUpdate {
			for i := range ins.OnConflict.Assignments {
				n += countSelfRefsExpr(ins.OnConflict.Assignments[i].Value, name)
			}
			if ins.OnConflict.Filter != nil {
				n += countSelfRefsExpr(*ins.OnConflict.Filter, name)
			}
		}
		return n + countReturningRefs(ins.Returning, name)
	case body.Update != nil:
		upd := body.Update
		n := 0
		for i := range upd.Assignments {
			n += countSelfRefsExpr(upd.Assignments[i].Value, name)
		}
		if upd.Filter != nil {
			n += countSelfRefsExpr(*upd.Filter, name)
		}
		return n + countReturningRefs(upd.Returning, name)
	default:
		del := body.Delete
		n := 0
		if del.Filter != nil {
			n += countSelfRefsExpr(*del.Filter, name)
		}
		return n + countReturningRefs(del.Returning, name)
	}
}

// countReturningRefs counts references to CTE name in a RETURNING item list's sublinks (the star
// form RETURNING * has no expressions, so it contributes none).
func countReturningRefs(returning *selectItems, name string) int {
	if returning == nil || returning.All {
		return 0
	}
	n := 0
	for i := range returning.Items {
		n += countSelfRefsExpr(returning.Items[i].Expr, name)
	}
	return n
}

// countDirectFromSelfRefs counts self-references that are DIRECT FROM/JOIN relations of this SELECT
// (a plain table ref matching the name). This is the only valid position for a recursive reference.
func countDirectFromSelfRefs(s *selectStmt, name string) int {
	n := 0
	for _, tref := range fromRelations(s) {
		if isPlainRelation(tref) && strings.EqualFold(tref.Name, name) {
			n++
		}
	}
	return n
}

// countFromSubquerySelfRefs counts self-references nested inside a FROM-position subquery /
// table-function args / VALUES of this SELECT (the deferred 0A000 shape).
func countFromSubquerySelfRefs(s *selectStmt, name string) int {
	n := 0
	for _, tref := range fromRelations(s) {
		if !isPlainRelation(tref) {
			n += countSelfRefsTableref(tref, name)
		}
	}
	return n
}

// countSublinkSelfRefs counts self-references reachable only through an expression sublink in this
// SELECT's top-level expressions — the `within a subquery` position.
func countSublinkSelfRefs(s *selectStmt, name string) int {
	n := 0
	for _, e := range selectExprs(s) {
		n += countSelfRefsExpr(e, name)
	}
	return n
}

// directSelfRefOnNullableSide reports whether the SELECT's single direct self-reference sits on the
// NULLABLE side of an outer join — the position PostgreSQL rejects. The FROM is a left-deep chain:
// relation 0 is From, relation i+1 is Joins[i].Table, combined by Joins[i].Kind. A LEFT/FULL join
// makes its right operand nullable; a RIGHT/FULL join makes the whole accumulated left nullable.
func directSelfRefOnNullableSide(s *selectStmt, name string) bool {
	rels := fromRelations(s)
	nullable := make([]bool, len(rels))
	for j := range s.Joins {
		right := j + 1
		switch s.Joins[j].Kind {
		case joinLeft:
			nullable[right] = true
		case joinRight:
			for i := 0; i <= j; i++ {
				nullable[i] = true
			}
		case joinFull:
			for i := 0; i <= right; i++ {
				nullable[i] = true
			}
		}
	}
	for i, tref := range rels {
		if isPlainRelation(tref) && strings.EqualFold(tref.Name, name) && nullable[i] {
			return true
		}
	}
	return false
}

// isPlainRelation reports whether a FROM relation is a plain table NAME — not a derived-table
// subquery, a table function, or a VALUES body. Only a plain relation can resolve to a CTE.
func isPlainRelation(tref *tableRef) bool {
	return !tref.IsFunc && tref.Subquery == nil && tref.Values == nil
}

// fromRelations returns the FROM relations of a SELECT in left-deep order: From (if present) then
// each join's table.
func fromRelations(s *selectStmt) []*tableRef {
	rels := make([]*tableRef, 0, 1+len(s.Joins))
	if s.From != nil {
		rels = append(rels, s.From)
	}
	for i := range s.Joins {
		rels = append(rels, &s.Joins[i].Table)
	}
	return rels
}

// selectExprs returns every top-level expression of a SELECT that can hold a sublink (select items,
// WHERE, GROUP BY, HAVING, join ON conditions). ORDER BY keys are bare/qualified column references
// (never expressions), so they carry no sublink.
func selectExprs(s *selectStmt) []exprNode {
	var v []exprNode
	for _, it := range s.Items.Items {
		v = append(v, it.Expr)
	}
	if s.Filter != nil {
		v = append(v, *s.Filter)
	}
	for i := range s.GroupBy {
		s.GroupBy[i].forEachExpr(func(e *exprNode) {
			v = append(v, *e)
		})
	}
	if s.Having != nil {
		v = append(v, *s.Having)
	}
	for i := range s.Joins {
		if s.Joins[i].On != nil {
			v = append(v, *s.Joins[i].On)
		}
	}
	return v
}

// checkRecursiveColumnTypes checks a recursive CTE's column types (recursive-cte.md §2): the output
// types are FIXED by the non-recursive (anchor) term, and the recursive term's columns must be
// assignable to them — a literal adapts, an equal type passes, a WIDER type is 42804 (matching
// PostgreSQL). Mechanically the would-be UNION unified type must EQUAL the anchor type; any widening
// of the anchor is the error. An arity mismatch is 42601, like a plain UNION.
func checkRecursiveColumnTypes(anchor, recursive *queryPlan, name string) error {
	a := anchor.columnTypes()
	r := recursive.columnTypes()
	if len(a) != len(r) {
		return newError(SyntaxError, "each UNION query must have the same number of columns")
	}
	for i := range a {
		unified, err := unifySetopColumn(a[i], r[i], setOpUnion)
		if err != nil {
			return err
		}
		if rtName(unified) != rtName(a[i]) {
			return newError(DatatypeMismatch, fmt.Sprintf(
				"recursive query %q column %d has type %s in non-recursive term but type %s overall",
				name, i+1, rtName(a[i]), rtName(unified),
			))
		}
	}
	return nil
}

// cteSyntheticTable builds the synthetic relation a CTE reference resolves against
// (spec/design/cte.md §2): one column per body output, named by the rename list (a count mismatch is
// 42P10) or the body's own output names, typed from the planned body. The relation has no primary
// key / constraints — it is read-only and its rows come from the CTE context, never a store.
func cteSyntheticTable(name string, plan *queryPlan, rename []string) (*catTable, error) {
	return cteSyntheticTableCols(name, plan.columnNames(), plan.columnTypes(), rename)
}

// cteSyntheticTableCols is the shared core of cteSyntheticTable, over explicit body column names +
// types — so a data-modifying CTE (whose "body output" is its RETURNING projection, not a queryPlan)
// builds its synthetic relation the same way (spec/design/writable-cte.md §1).
func cteSyntheticTableCols(name string, bodyNames []string, bodyTypes []resolvedType, rename []string) (*catTable, error) {
	var colNames []string
	if rename != nil {
		// PostgreSQL allows FEWER aliases than the body has columns — the first len(rename) columns
		// take the aliases, the rest keep their body output names (a partial rename). Only MORE
		// aliases than columns is an error (42P10).
		if len(rename) > len(bodyTypes) {
			return nil, newError(InvalidColumnReference, fmt.Sprintf(
				"WITH query \"%s\" has %d columns available but %d columns specified",
				name, len(bodyTypes), len(rename),
			))
		}
		colNames = make([]string, len(bodyTypes))
		for i := range bodyTypes {
			if i < len(rename) {
				colNames[i] = rename[i]
			} else {
				colNames[i] = bodyNames[i]
			}
		}
	} else {
		colNames = append([]string(nil), bodyNames...)
	}
	columns := make([]catColumn, len(colNames))
	for i, n := range colNames {
		ty, err := typeFromResolved(bodyTypes[i])
		if err != nil {
			return nil, err
		}
		columns[i] = catColumn{Name: n, Type: ty}
	}
	return &catTable{Name: name, Columns: columns}, nil
}

// typeFromResolved is the catalog Type for a resolved expression type — used to give a CTE's
// synthetic columns a Type (spec/design/cte.md). An untyped NULL column maps to text (PostgreSQL's
// unknown -> text rule). A decimal's per-column typmod is irrelevant for a read-only CTE column
// (values flow through unchanged), so it is dropped. An anonymous ROW(...) composite has no catalog
// type to name — deferred (0A000), a corner not reached by the corpus.
func typeFromResolved(rt resolvedType) (dataType, error) {
	switch rt.kind {
	case rtInt:
		return scalarT(rt.intTy), nil
	case rtFloat32:
		return scalarT(scalarFloat32), nil
	case rtFloat64:
		return scalarT(scalarFloat64), nil
	case rtBool:
		return scalarT(scalarBool), nil
	case rtText, rtNull:
		return scalarT(scalarText), nil
	case rtDecimal:
		return scalarT(scalarDecimal), nil
	case rtBytea:
		return scalarT(scalarBytea), nil
	case rtUuid:
		return scalarT(scalarUuid), nil
	case rtTimestamp:
		return scalarT(scalarTimestamp), nil
	case rtTimestamptz:
		return scalarT(scalarTimestamptz), nil
	case rtDate:
		return scalarT(scalarDate), nil
	case rtInterval:
		return scalarT(scalarInterval), nil
	case rtComposite:
		if rt.comp != nil && rt.comp.named {
			return compositeT(rt.comp.name), nil
		}
		return dataType{}, newError(FeatureNotSupported,
			"an anonymous composite column in a CTE is not supported yet")
	case rtArray:
		elem, err := typeFromResolved(*rt.elem)
		if err != nil {
			return dataType{}, err
		}
		return arrayT(elem), nil
	default:
		return dataType{}, newError(FeatureNotSupported, "unsupported CTE column type")
	}
}

// runQueryExpr runs a query expression to a selectResult — a lone SELECT via runSelect, or a set
// operation via runSetOp (recursively, so a chain `a UNION b INTERSECT c` evaluates as the parsed
// precedence tree).
// runQueryExpr is the top-level orchestrator (spec/design/grammar.md §26): PLAN the whole
// expression tree once against an empty scope chain (threading one paramTypes so $N inference is
// statement-wide), bind the parameters, then the foldUncorrelated pass executes each
// globally-uncorrelated subquery once and folds it to a constant (preserving the once-only cost),
// and finally EXECUTE against an empty outer-row environment. Correlated subqueries that survive
// the fold are re-executed per outer row by the evaluator.
func (db *engine) runQueryExpr(qe queryExpr, params []Value) (selectResult, error) {
	ptypes := &paramTypes{}
	plan, err := db.planQuery(qe, nil, nil, ptypes)
	if err != nil {
		return selectResult{}, err
	}
	ptys, err := ptypes.finalize()
	if err != nil {
		return selectResult{}, err
	}
	bound, err := bindParams(params, ptys)
	if err != nil {
		return selectResult{}, err
	}
	var subqueryCost int64
	if err := db.foldUncorrelatedInPlan(&plan, bound, cteCtx{}, &subqueryCost); err != nil {
		return selectResult{}, err
	}
	r, err := db.execQueryPlan(&plan, nil, bound, cteCtx{})
	if err != nil {
		return selectResult{}, err
	}
	r.cost += subqueryCost
	return r, nil
}

// runSelect runs a lone SELECT — the entry point executeSelect and INSERT ... SELECT use.
func (db *engine) runSelect(sel *selectStmt, params []Value) (selectResult, error) {
	return db.runQueryExpr(queryExpr{Select: sel}, params)
}

// runSetOp runs a set operation as a top-level statement.
func (db *engine) runSetOp(so *setOp, params []Value) (selectResult, error) {
	return db.runQueryExpr(queryExpr{SetOp: so}, params)
}

// planQuery resolves a query expression into an owned queryPlan against the scope chain (parent
// = the enclosing query's scope, nil at top level). ctes are the statement's CTE bindings visible
// here (spec/design/cte.md §2), empty for a non-WITH statement. A subquery is planned here, once
// (§26).
func (db *engine) planQuery(qe queryExpr, parent *scope, ctes []*cteBinding, ptypes *paramTypes) (queryPlan, error) {
	if qe.Select != nil {
		sp, err := db.planSelect(qe.Select, parent, ctes, ptypes)
		if err != nil {
			return queryPlan{}, err
		}
		return queryPlan{sel: sp}, nil
	}
	if qe.With != nil {
		wp, err := db.planWithExpr(qe.With, parent, ptypes)
		if err != nil {
			return queryPlan{}, err
		}
		return queryPlan{with: wp}, nil
	}
	sop, err := db.planSetOp(qe.SetOp, parent, ctes, ptypes)
	if err != nil {
		return queryPlan{}, err
	}
	return queryPlan{setop: sop}, nil
}

// planWithExpr plans a nested `WITH … query_expr` (spec/design/cte.md §7) into a withPlan. The
// nested CTEs establish their OWN scope: the bodies and the inner main query see ONLY these CTEs
// (and the catalog) — the enclosing statement's CTE bindings are NOT inherited (a documented
// narrowing, cte.md §7), so planCteBindings and the body are planned without the outer ctes. The
// inner main query keeps the enclosing parent (so a LATERAL derived-table body still correlates to
// its left siblings), while the CTE bodies stay independent (parent=nil, inside planCteBindings). A
// data-modifying CTE here is rejected 0A000 — PostgreSQL restricts a DML-WITH to the top level.
func (db *engine) planWithExpr(we *withExpr, parent *scope, ptypes *paramTypes) (*withPlan, error) {
	for i := range we.Ctes {
		if we.Ctes[i].Body.IsDataModifying() {
			return nil, newError(FeatureNotSupported,
				fmt.Sprintf("WITH clause containing a data-modifying statement (%s) is only supported at the top level", we.Ctes[i].Name))
		}
	}
	bindings, err := db.planCteBindings(we.Ctes, we.Recursive, ptypes)
	if err != nil {
		return nil, err
	}
	body, err := db.planQuery(*we.Body, parent, bindings, ptypes)
	if err != nil {
		return nil, err
	}
	return &withPlan{bindings: bindings, modes: cteModes(bindings), body: body}, nil
}

// execQueryPlan executes a resolved plan against an outer-row environment (outer = the enclosing
// rows, innermost last; nil at top level) and the bound parameters. ctes is the per-statement CTE
// execution context (spec/design/cte.md §5), the zero cteCtx for a non-WITH statement.
func (db *engine) execQueryPlan(plan *queryPlan, outer []storedRow, params []Value, ctes cteCtx) (selectResult, error) {
	if plan.sel != nil {
		return db.execSelectPlan(plan.sel, outer, params, ctes)
	}
	if plan.values != nil {
		return db.execValuesPlan(plan.values, outer, params, ctes)
	}
	if plan.with != nil {
		return db.execWithPlan(plan.with, outer, params)
	}
	return db.execSetOpPlan(plan.setop, outer, params, ctes)
}

// execWithPlan executes a nested WITH plan (spec/design/cte.md §7): materialize its CTE bindings
// once (in list order, charging their cost), build a FRESH CTE context over them (the nested CTEs
// establish their own scope — the enclosing context is NOT chained in, the documented narrowing
// §7), and run the inner body against it. The body still sees the outer row environment (so a
// LATERAL nested-WITH derived-table body correlates to its left siblings). The materialization cost
// folds into the body's cost — the same shape as the top-level runWith (cte.md §3).
func (db *engine) execWithPlan(wp *withPlan, outer []storedRow, params []Value) (selectResult, error) {
	buffers, totalCost, err := db.materializeCtes(wp.bindings, wp.modes, params)
	if err != nil {
		return selectResult{}, err
	}
	ctx := cteCtx{modes: wp.modes, bindings: wp.bindings, buffers: buffers}
	r, err := db.execQueryPlan(&wp.body, outer, params, ctx)
	if err != nil {
		return selectResult{}, err
	}
	r.cost += totalCost
	return r, nil
}

// planSetOp plans a set operation (spec/design/grammar.md §25): plan both operands with the same
// parent scope, check arity + unify column types up front (so the 42601/42804 fire even over
// empty operands), and resolve the trailing ORDER BY by output column name.
func (db *engine) planSetOp(so *setOp, parent *scope, ctes []*cteBinding, ptypes *paramTypes) (*setOpPlan, error) {
	lhs, err := db.planQuery(so.Lhs, parent, ctes, ptypes)
	if err != nil {
		return nil, err
	}
	rhs, err := db.planQuery(so.Rhs, parent, ctes, ptypes)
	if err != nil {
		return nil, err
	}

	if len(lhs.columnTypes()) != len(rhs.columnTypes()) {
		return nil, newError(SyntaxError, fmt.Sprintf(
			"each %s query must have the same number of columns", setopName(so.Op),
		))
	}
	columnTypes := make([]resolvedType, len(lhs.columnTypes()))
	for i := range columnTypes {
		t, err := unifySetopColumn(lhs.columnTypes()[i], rhs.columnTypes()[i], so.Op)
		if err != nil {
			return nil, err
		}
		columnTypes[i] = t
	}
	columnNames := lhs.columnNames()

	order := make([]orderSlot, 0, len(so.OrderBy))
	for i := range so.OrderBy {
		key := &so.OrderBy[i]
		idx, err := resolveSetopOrderKey(key, columnNames)
		if err != nil {
			return nil, err
		}
		// An explicit COLLATE on a set-operation ORDER BY key (spec/design/collation.md §1): the
		// output column must be text (42804); the name resolves ("C", else loaded or 42704).
		var coll *Collation
		if key.Collation != "" {
			if columnTypes[idx].kind != rtText {
				return nil, typeError("collations are not supported by this column's type")
			}
			if coll, err = resolveCollationName(db, key.Collation); err != nil {
				return nil, err
			}
		}
		order = append(order, orderSlot{idx: idx, descending: key.Descending, nullsFirst: key.NullsFirst, collation: coll})
	}

	return &setOpPlan{
		op: so.Op, all: so.All, lhs: lhs, rhs: rhs,
		columnNames: columnNames, columnTypes: columnTypes,
		order: order, limit: so.Limit, offset: so.Offset,
	}, nil
}

// execSetOpPlan executes a resolved set operation: run both operands against the outer
// environment, coerce to the unified types, combine, then sort + window. Cost is lhs.cost +
// rhs.cost — the combine, sort, and window are unmetered (cost.md §3).
func (db *engine) execSetOpPlan(plan *setOpPlan, outer []storedRow, params []Value, ctes cteCtx) (selectResult, error) {
	left, err := db.execQueryPlan(&plan.lhs, outer, params, ctes)
	if err != nil {
		return selectResult{}, err
	}
	right, err := db.execQueryPlan(&plan.rhs, outer, params, ctes)
	if err != nil {
		return selectResult{}, err
	}

	coerceSetopRows(left.rows, left.columnTypes, plan.columnTypes)
	coerceSetopRows(right.rows, right.columnTypes, plan.columnTypes)

	rows := combineSetop(plan.op, plan.all, left.rows, right.rows)
	cost := left.cost + right.cost

	if len(plan.order) > 0 {
		if err := sortRows(rows, plan.order); err != nil {
			return selectResult{}, err
		}
	}

	n := int64(len(rows))
	start := int64(0)
	if plan.offset != nil && *plan.offset < n {
		start = *plan.offset
	} else if plan.offset != nil {
		start = n
	}
	end := n
	if plan.limit != nil && *plan.limit < n-start {
		end = start + *plan.limit
	}
	rows = rows[start:end]

	return selectResult{columnNames: plan.columnNames, columnTypes: plan.columnTypes, rows: rows, cost: cost}, nil
}

// planValues resolves a VALUES-body relation into a *valuesPlan (spec/design/grammar.md §42) — the
// body of a FROM (VALUES …) derived table. Each value resolves as a CONSTANT against an EMPTY scope
// with parent=nil: the body is non-LATERAL, so a column reference is unresolved (42703/42P01) and an
// aggregate is 42803; it still sees the statement's CTE bindings (an uncorrelated subquery inside a
// value resolves like anywhere). Every row must have the same arity (42601); the columns' types
// unify across rows like a set operation (42804 on a mismatch). A bind parameter is then noted at
// its column's unified type (so VALUES (1),($1) types $1 as int); a column with no concrete type —
// all NULL/param — leaves its $N untyped, surfacing 42P18 at finalize (jed's no-cross-context
// inference posture, §26).
func (db *engine) planValues(rows [][]*exprNode, parent *scope, ctes []*cteBinding, ptypes *paramTypes) (*valuesPlan, error) {
	arity := len(rows[0]) // the parser guarantees at least one row, each with at least one value
	// A constant scope: no local relations. With parent==nil (the usual case) any column reference is
	// unresolved (the non-LATERAL rule, §42); with a parent (a LATERAL VALUES body, §44) a column
	// reference correlates to the earlier FROM relations instead. CTE bindings stay visible and
	// subqueries are allowed (an uncorrelated one folds before the rows run).
	s := &scope{parent: parent, catalog: db, allowSubquery: true, ctes: ctes}
	resolvedRows := make([][]*rExpr, len(rows))
	colTypes := make([]resolvedType, arity)
	// Per column: the 0-based bind-parameter slots appearing in it, typed in a second pass from the
	// unified column type (a $N takes its column's type, like a set-operation operand).
	colParams := make([][]int, arity)
	for ri, row := range rows {
		if len(row) != arity {
			return nil, newError(SyntaxError, "VALUES lists must all be the same length")
		}
		resolvedRow := make([]*rExpr, arity)
		for ci, val := range row {
			node, ty, err := resolve(s, *val, nil, &aggCtx{}, ptypes) // forbidden: an aggregate is 42803
			if err != nil {
				return nil, err
			}
			if node.kind == reParam {
				colParams[ci] = append(colParams[ci], node.index)
			}
			if ri == 0 {
				colTypes[ci] = ty
			} else {
				u, err := unifyValuesColumn(colTypes[ci], ty)
				if err != nil {
					return nil, err
				}
				colTypes[ci] = u
			}
			resolvedRow[ci] = node
		}
		resolvedRows[ri] = resolvedRow
	}
	// Second pass: note each column's bind parameters at the unified column type. A column with no
	// scalar type (all NULL/param) passes nil — the parameter stays untyped (42P18).
	for ci := range colParams {
		hint := scalarForParamHint(colTypes[ci])
		for _, idx0 := range colParams[ci] {
			if err := ptypes.note(idx0, hint); err != nil {
				return nil, err
			}
		}
	}
	// PostgreSQL names a VALUES relation's columns column1, column2, … ; the derived table's optional
	// column-rename list overrides them at the synthetic relation (cteSyntheticTable).
	colNames := make([]string, arity)
	for i := range colNames {
		colNames[i] = fmt.Sprintf("column%d", i+1)
	}
	return &valuesPlan{rows: resolvedRows, columnTypes: colTypes, columnNames: colNames}, nil
}

// execValuesPlan executes a resolved VALUES-body relation (spec/design/grammar.md §42): evaluate
// each row's values as constants over an EMPTY environment (no local row, no outer row —
// non-LATERAL), coerce each to the unified column type (the only runtime change is int -> decimal,
// the set-operation rule), and emit the rows. Charges row_produced per row plus each value's
// operator_eval (the evaluator) — the derived table's intrinsic cost (cost.md §3), folded into the
// caller's meter via execQueryPlan.
func (db *engine) execValuesPlan(plan *valuesPlan, outer []storedRow, params []Value, ctes cteCtx) (selectResult, error) {
	env := &evalEnv{exec: db, params: params, outer: outer, rng: newStmtRng(), ctes: ctes}
	meter := db.session.newMeter()
	rows := make([][]Value, 0, len(plan.rows))
	for _, row := range plan.rows {
		if err := meter.Guard(); err != nil { // enforce the cost ceiling per produced row (CLAUDE.md §13)
			return selectResult{}, err
		}
		meter.Charge(costs.RowProduced)
		out := make([]Value, len(plan.columnTypes))
		for ci, e := range row {
			v, err := e.eval(nil, env, meter)
			if err != nil {
				return selectResult{}, err
			}
			// Int -> decimal where the column unified to decimal (the set-operation rule); every
			// other unified type is a value no-op (int-width promotion is free — all ints are i64).
			if plan.columnTypes[ci].kind == rtDecimal && v.Kind == ValInt {
				v = DecimalValue(decimalFromInt64(v.Int))
			}
			out[ci] = v
		}
		rows = append(rows, out)
	}
	return selectResult{columnNames: plan.columnNames, columnTypes: plan.columnTypes, rows: rows, cost: meter.Accrued}, nil
}

// setopName is the operator's name for an error message (PostgreSQL phrasing).
func setopName(op setOpKind) string {
	switch op {
	case setOpUnion:
		return "UNION"
	case setOpIntersect:
		return "INTERSECT"
	default:
		return "EXCEPT"
	}
}

// unifySetopColumn unifies one output column's type across the two operands of a set operation
// (spec/design/grammar.md §25, types.md §4): integer widths promote to the widest; integer with
// decimal -> decimal; a NULL-typed operand takes the other's type (an all-NULL column stays NULL —
// PostgreSQL would call a top-level one text, but the type is never observed in output); a
// same-family non-numeric pair gives that type; anything else is 42804. The set of unifiable pairs
// mirrors the comparability matrix (compare.toml).
func unifySetopColumn(a, b resolvedType, op setOpKind) (resolvedType, error) {
	switch {
	case a.kind == rtNull && b.kind == rtNull:
		return resolvedType{kind: rtNull}, nil
	case a.kind == rtNull:
		return b, nil
	case b.kind == rtNull:
		return a, nil
	case a.kind == rtInt && b.kind == rtInt:
		return resolvedType{kind: rtInt, intTy: promote(a, b)}, nil
	case (a.kind == rtInt || a.kind == rtDecimal) && (b.kind == rtInt || b.kind == rtDecimal):
		// at least one decimal (both-int handled above) -> decimal
		return resolvedType{kind: rtDecimal}, nil
	case a.kind == b.kind:
		return a, nil
	default:
		return resolvedType{}, newError(DatatypeMismatch, fmt.Sprintf(
			"%s types %s and %s cannot be matched", setopName(op), rtName(a), rtName(b),
		))
	}
}

// unifyValuesColumn unifies two row value types for the SAME VALUES-body column
// (spec/design/grammar.md §42), the set-operation rule (§25): integer widths widen, int+decimal ->
// decimal, anything + NULL keeps the other, and a same-type scalar pair (text, bool, bytea, uuid, a
// timestamp / timestamptz, an interval, a same-width float) unifies to itself; any other pair —
// including a composite or array column across rows (a deferred edge) — is 42804. Enumerated
// EXPLICITLY (not a generic same-kind passthrough) so all three cores compute byte-identical
// results (CLAUDE.md §8).
func unifyValuesColumn(a, b resolvedType) (resolvedType, error) {
	switch {
	case a.kind == rtNull && b.kind == rtNull:
		return resolvedType{kind: rtNull}, nil
	case a.kind == rtNull:
		return b, nil
	case b.kind == rtNull:
		return a, nil
	case a.kind == rtInt && b.kind == rtInt:
		return resolvedType{kind: rtInt, intTy: promote(a, b)}, nil
	case (a.kind == rtInt || a.kind == rtDecimal) && (b.kind == rtInt || b.kind == rtDecimal):
		return resolvedType{kind: rtDecimal}, nil
	case a.kind == rtText && b.kind == rtText,
		a.kind == rtBool && b.kind == rtBool,
		a.kind == rtBytea && b.kind == rtBytea,
		a.kind == rtUuid && b.kind == rtUuid,
		a.kind == rtTimestamp && b.kind == rtTimestamp,
		a.kind == rtTimestamptz && b.kind == rtTimestamptz,
		a.kind == rtDate && b.kind == rtDate,
		a.kind == rtInterval && b.kind == rtInterval,
		a.kind == rtFloat32 && b.kind == rtFloat32,
		a.kind == rtFloat64 && b.kind == rtFloat64:
		return a, nil
	default:
		return resolvedType{}, newError(DatatypeMismatch, fmt.Sprintf(
			"VALUES types %s and %s cannot be matched", rtName(a), rtName(b),
		))
	}
}

// scalarForParamHint is the scalar type to note a bind parameter at, given its VALUES column's
// unified type (spec/design/grammar.md §42). A scalar type flows through; a NULL / composite / array
// column has no scalar parameter type, so nil is returned and the parameter stays untyped (42P18 at
// finalize).
func scalarForParamHint(rt resolvedType) *scalarType {
	switch rt.kind {
	case rtInt:
		t := rt.intTy // rtInt carries its width in intTy
		return &t
	case rtFloat32:
		t := scalarFloat32
		return &t
	case rtFloat64:
		t := scalarFloat64
		return &t
	case rtBool:
		t := scalarBool
		return &t
	case rtText:
		t := scalarText
		return &t
	case rtDecimal:
		t := scalarDecimal
		return &t
	case rtBytea:
		t := scalarBytea
		return &t
	case rtUuid:
		t := scalarUuid
		return &t
	case rtTimestamp:
		t := scalarTimestamp
		return &t
	case rtTimestamptz:
		t := scalarTimestamptz
		return &t
	case rtDate:
		t := scalarDate
		return &t
	case rtInterval:
		t := scalarInterval
		return &t
	case rtJson:
		t := scalarJson
		return &t
	case rtJsonb:
		t := scalarJsonb
		return &t
	case rtJsonPath:
		t := scalarJsonPath
		return &t
	default:
		return nil
	}
}

// coerceSetopRows converts each row's values in place to the unified set-operation column types —
// the only runtime change is integer -> decimal (a NULL stays NULL; integer-width promotion is a
// value no-op since every integer is i64). Same conversion coerceCase uses for CASE.
func coerceSetopRows(rows [][]Value, from, to []resolvedType) {
	for i := range to {
		if from[i].kind == rtInt && to[i].kind == rtDecimal {
			for r := range rows {
				if rows[r][i].Kind == ValInt {
					rows[r][i] = DecimalValue(decimalFromInt64(rows[r][i].Int))
				}
			}
		}
	}
}

// combineSetop combines the operands' rows per the set operator + ALL flag (spec/design/grammar.md
// §25). Rows match by the NULL-safe, value-canonical distinctRowKey (two NULLs match, 1.5 == 1.50,
// and a converted int matches the decimal). The emitted representative for a matched / deduplicated
// key is its FIRST occurrence scanning the LEFT operand then the right, and emitted rows keep that
// left-then-right scan order — deterministic and identical across cores. (A later ORDER BY
// re-sorts; without one, output order is unspecified and the corpus compares rowsort.)
func combineSetop(op setOpKind, all bool, left, right [][]Value) [][]Value {
	switch {
	case op == setOpUnion && all:
		out := make([][]Value, 0, len(left)+len(right))
		out = append(out, left...)
		out = append(out, right...)
		return out
	case op == setOpUnion:
		seen := make(map[string]bool)
		out := make([][]Value, 0)
		for _, row := range left {
			if k := distinctRowKey(row); !seen[k] {
				seen[k] = true
				out = append(out, row)
			}
		}
		for _, row := range right {
			if k := distinctRowKey(row); !seen[k] {
				seen[k] = true
				out = append(out, row)
			}
		}
		return out
	case op == setOpIntersect && all:
		counts := make(map[string]int)
		for _, row := range right {
			counts[distinctRowKey(row)]++
		}
		out := make([][]Value, 0)
		for _, row := range left {
			k := distinctRowKey(row)
			if counts[k] > 0 {
				counts[k]--
				out = append(out, row)
			}
		}
		return out
	case op == setOpIntersect:
		rightSet := make(map[string]bool)
		for _, row := range right {
			rightSet[distinctRowKey(row)] = true
		}
		emitted := make(map[string]bool)
		out := make([][]Value, 0)
		for _, row := range left {
			k := distinctRowKey(row)
			if rightSet[k] && !emitted[k] {
				emitted[k] = true
				out = append(out, row)
			}
		}
		return out
	case op == setOpExcept && all:
		counts := make(map[string]int)
		for _, row := range right {
			counts[distinctRowKey(row)]++
		}
		out := make([][]Value, 0)
		for _, row := range left {
			k := distinctRowKey(row)
			if counts[k] > 0 {
				counts[k]--
			} else {
				out = append(out, row)
			}
		}
		return out
	default: // EXCEPT, distinct
		rightSet := make(map[string]bool)
		for _, row := range right {
			rightSet[distinctRowKey(row)] = true
		}
		emitted := make(map[string]bool)
		out := make([][]Value, 0)
		for _, row := range left {
			k := distinctRowKey(row)
			if !rightSet[k] && !emitted[k] {
				emitted[k] = true
				out = append(out, row)
			}
		}
		return out
	}
}

// resolveSetopOrderKey resolves a trailing ORDER BY key for a set operation against the OUTPUT
// column names (the left operand's). A qualified key is 42P01 (no relation scope after a set
// operation); an unknown name is 42703. Returns the output column index.
func resolveSetopOrderKey(key *orderKey, names []string) (int, error) {
	// A set-operation ORDER BY accepts only an output column name or ordinal — a general expression key
	// (after the inputs are unified) is 0A000, matching PostgreSQL's "invalid UNION/INTERSECT/EXCEPT
	// ORDER BY clause" (grammar.md §10).
	if key.Expr != nil {
		return 0, newError(FeatureNotSupported, "invalid UNION/INTERSECT/EXCEPT ORDER BY clause")
	}
	// An output-column ordinal (`... ORDER BY 1`) resolves by position into the output columns; out
	// of [1, ncols] is 42P10 (grammar.md §10). It precedes the name path (an ordinal has no column).
	if key.Ordinal != nil {
		ord := *key.Ordinal
		if ord < 1 || ord > int64(len(names)) {
			return 0, newError(InvalidColumnReference,
				fmt.Sprintf("ORDER BY position %d is not in select list", ord))
		}
		return int(ord - 1), nil
	}
	if key.Qualifier != "" {
		return 0, newError(UndefinedTable, "missing FROM-clause entry for table "+key.Qualifier)
	}
	for i, n := range names {
		if strings.EqualFold(n, key.Column) {
			return i, nil
		}
	}
	return 0, newError(UndefinedColumn, "column "+key.Column+" does not exist")
}

// runSelect analyzes and runs a SELECT: resolve projected columns and the WHERE/ORDER BY columns
// against the catalog, scan the table in primary-key order, filter by the predicate (three-valued
// — only TRUE keeps a row), optionally re-sort by ORDER BY, then project. Rows are produced
// deterministically (CLAUDE.md §10). Returns the rows with each output column's NAME and resolved
// TYPE (the types let INSERT ... SELECT gate assignability up front — §24) and the accrued cost.
// planSelect resolves a SELECT into a *selectPlan against the scope chain (parent = the enclosing
// query's scope, for correlated references — grammar.md §26). The resolve half of the old
// runSelect: build the FROM scope, resolve every clause, infer $N types into ptypes. No row is
// touched and no parameter is bound here (runQueryExpr binds once, after the whole tree is planned).
func (db *engine) planSelect(sel *selectStmt, parent *scope, ctes []*cteBinding, ptypes *paramTypes) (*selectPlan, error) {
	// Build the FROM scope: resolve each table reference (42P01 if unknown), compute each
	// relation's flat column offset in FROM order, and reject a duplicate label — a self-join
	// without distinct aliases is 42712 (spec/design/grammar.md §15). A FROM-less SELECT
	// (sel.From == nil) builds an EMPTY scope: nothing local resolves, so bare columns fall
	// through to `parent` (the correlated-subquery rule) or 42703 at top level
	// (spec/design/grammar.md §34). The scope links to `parent` (correlation) + the catalog
	// (so a subquery resolves its own FROM); allowSubquery is true.
	tableRefs := make([]tableRef, 0, 1+len(sel.Joins))
	if sel.From != nil {
		tableRefs = append(tableRefs, *sel.From)
	}
	for _, j := range sel.Joins {
		tableRefs = append(tableRefs, j.Table)
	}
	// A FROM item is a base table, a set-returning function (grammar.md §35), or a derived table
	// (§42). For a LATERAL item (§44) the body / SRF args resolve against the PREFIX of relations to
	// its left (a dependent join), so the build runs in FROM order and a prefix scope over the
	// already-resolved rels is handed to the body.
	var rels []scopeRel
	srfPlans := make([]*srfPlan, len(tableRefs))       // aligned with rels; nil = a base table
	derivedPlans := make([]*queryPlan, len(tableRefs)) // aligned with rels; non-nil = a derived table
	// lateralFlags[i] is true when FROM item i is a CORRELATED lateral relation (§44) — its body /
	// SRF args reference an earlier sibling (or an enclosing query), so the executor re-materializes
	// it per combined left-hand row. A non-correlated item (or the first item) is materialized once.
	lateralFlags := make([]bool, len(tableRefs))
	seenLabels := make(map[string]bool)
	offset := 0
	for i, tref := range tableRefs {
		var t *catTable
		var cteIdx *int
		isDerived := tref.Subquery != nil || tref.Values != nil
		// A FROM item is lateral-ELIGIBLE when it can see earlier siblings: a derived table / VALUES
		// body explicitly marked LATERAL, or ANY table function (implicitly lateral — §44). The first
		// item (i == 0) has no earlier sibling, so it is never lateral; an SRF there resolves against
		// `parent` (the enclosing query) exactly as before.
		lateralEligible := i > 0 && ((isDerived && tref.Lateral) || tref.IsFunc || tref.JsonTable != nil)
		// The prefix scope a LATERAL item resolves against: the relations to its left, chained to the
		// enclosing query's parent (so a sibling column correlates as Outer{level=1}, an enclosing one
		// deeper). nil when not lateral-eligible.
		var lateralParent *scope
		if lateralEligible {
			lateralParent = &scope{rels: rels, parent: parent, catalog: db, allowSubquery: true, ctes: ctes}
		}
		if isDerived {
			// Plan the body. LATERAL → parent is the prefix scope (a sibling/outer column correlates);
			// otherwise an INDEPENDENT query (parent=nil, §42). A LATERAL VALUES body resolves its
			// values against the prefix too (a column ref then correlates instead of 42703).
			bodyParent := (*scope)(nil)
			if lateralEligible {
				bodyParent = lateralParent
			}
			var plan queryPlan
			if tref.Subquery != nil {
				p, perr := db.planQuery(*tref.Subquery, bodyParent, ctes, ptypes)
				if perr != nil {
					return nil, perr
				}
				plan = p
			} else {
				vp, verr := db.planValues(tref.Values, bodyParent, ctes, ptypes)
				if verr != nil {
					return nil, verr
				}
				plan = queryPlan{values: vp}
			}
			lateralFlags[i] = lateralEligible && queryPlanReferencesOuter(&plan, 0)
			label := ""
			if tref.Alias != nil {
				label = strings.ToLower(*tref.Alias)
			}
			tbl, terr := cteSyntheticTable(label, &plan, tref.ColumnAliases)
			if terr != nil {
				return nil, terr
			}
			t = tbl
			derivedPlans[i] = &plan
		} else if tref.IsFunc {
			// A table function (SRF) — implicitly lateral. At i>0 its args resolve against the prefix
			// scope (a sibling column then correlates); at i==0 against `parent` (the enclosing query
			// / params), unchanged (functions.md §10).
			srfParent := parent
			if lateralEligible {
				srfParent = lateralParent
			}
			tbl, sp, serr := db.resolveSRF(tref.Name, tref.Args, tref.Alias, tref.ColumnDefs, srfParent, ctes, ptypes)
			if serr != nil {
				return nil, serr
			}
			t = tbl
			srfPlans[i] = sp
			if lateralEligible {
				for _, a := range sp.args {
					if rexprReferencesOuter(a, 0) {
						lateralFlags[i] = true
						break
					}
				}
			}
		} else if tref.JsonTable != nil {
			// A JSON_TABLE source (T1, json-table.md §3) — implicitly lateral like an SRF; its ctx
			// resolves against the prefix scope (so `JSON_TABLE(sibling.doc, …)` works), or `parent` at
			// i==0.
			jtParent := parent
			if lateralEligible {
				jtParent = lateralParent
			}
			tbl, sp, jerr := db.resolveJSONTable(tref.JsonTable, tref.Alias, jtParent, ctes, ptypes)
			if jerr != nil {
				return nil, jerr
			}
			t = tbl
			srfPlans[i] = sp
			if lateralEligible {
				for _, a := range sp.args {
					if rexprReferencesOuter(a, 0) {
						lateralFlags[i] = true
						break
					}
				}
			}
		} else if tref.DB != nil {
			// A database-QUALIFIED name reaches an attachment's table directly (attached-databases.md
			// §3): it never resolves to a CTE (a CTE has no database qualifier, so `main.x`/`temp.x`
			// cannot name one) and the qualifier fixes the scope (no temp-vs-persistent shadow).
			// A built-in catalog relation resolves in EVERY database's relation namespace
			// (temp.jed_tables, reports.jed_tables — introspection.md §5), before the user catalog;
			// only the qualifier itself needs validating.
			if kind, ok := catalogRelKind(tref.Name); ok {
				scope, serr := db.resolveCatalogScope(tref.DB)
				if serr != nil {
					return nil, serr
				}
				t = catalogRelTable(kind)
				srfPlans[i] = &srfPlan{kind: kind, introspectScope: scope}
			} else {
				// Validate the qualifier against the implicit scope, then resolve through the temp-first
				// funnel (which, by preclude-overlaps, lands in the validated scope).
				if err := db.checkTableQualifier(tref.DB, tref.Name); err != nil {
					return nil, err
				}
				// Route to the qualified database's catalog (attached-databases.md §3): main/temp fall through
				// to the temp-first funnel (preclude-overlaps lands them in the validated scope), a host
				// attachment resolves in its own snapshot — where its table lives ONLY.
				tbl, ok := db.lkpTableScoped(tref.DB, tref.Name)
				if !ok {
					return nil, newError(UndefinedTable, "table does not exist: "+*tref.DB+"."+tref.Name)
				}
				t = tbl
			}
		} else {
			// A plain FROM name (not an SRF call) may resolve to a CTE, which SHADOWS a catalog
			// table of the same name (cte.md §2); lookup is case-insensitive. A hit bumps the
			// binding's reference count (the inline-vs-materialize decision — cost.md §3).
			lname := strings.ToLower(tref.Name)
			ci := -1
			for j, b := range ctes {
				if b.name == lname {
					ci = j
					break
				}
			}
			if ci >= 0 {
				// A data-modifying CTE with no RETURNING produces no columns, so a FROM reference to
				// it is 0A000 (writable-cte.md §5; PostgreSQL's addRangeTableEntryForCTE check), raised
				// at resolution before any execution.
				if ctes[ci].dm != nil && ctes[ci].dm.noReturning {
					return nil, newError(FeatureNotSupported,
						"WITH query "+lname+" does not have a RETURNING clause")
				}
				ctes[ci].refs++
				idx := ci
				cteIdx = &idx
				t = ctes[ci].table
			} else if kind, ok := catalogRelKind(tref.Name); ok {
				// A built-in catalog relation (introspection.md §5), checked AFTER a CTE (a CTE
				// shadows it — PG-matching) and BEFORE the user catalog. Unqualified = the implicit
				// scope (main).
				t = catalogRelTable(kind)
				srfPlans[i] = &srfPlan{kind: kind, introspectScope: "main"}
			} else {
				tbl, ok := db.lkpTable(tref.Name) // temp-first (temp-tables.md §3)
				if !ok {
					return nil, newError(UndefinedTable, "table does not exist: "+tref.Name)
				}
				t = tbl
			}
		}
		// RIGHT/FULL JOIN to a CORRELATED lateral item is rejected (§44): the right side cannot be both
		// kept whole and evaluated per left row. (i ≥ 1 here, so the item carries a join kind.)
		if lateralFlags[i] && (sel.Joins[i-1].Kind == joinRight || sel.Joins[i-1].Kind == joinFull) {
			return nil, newError(InvalidColumnReference,
				"invalid reference to FROM-clause entry for a LATERAL item: the combining JOIN type must be INNER or LEFT")
		}
		label := strings.ToLower(t.Name)
		if tref.Alias != nil {
			label = strings.ToLower(*tref.Alias)
		}
		// An unaliased derived table (grammar.md §42, PG 18) has an EMPTY label — it has no
		// qualifier, so two of them never collide and the duplicate-label check is skipped (its bare
		// columns still resolve, and stay ambiguous via resolveBare). Every other relation has a
		// non-empty label (a table/function name or an explicit alias).
		if label != "" {
			if seenLabels[label] {
				return nil, newError(DuplicateAlias, "table name "+label+" specified more than once")
			}
			seenLabels[label] = true
		}
		rels = append(rels, scopeRel{label: label, table: t, offset: offset, cte: cteIdx, db: tref.DB})
		offset += len(t.Columns)
	}

	// USING/NATURAL merged columns + every join's resolved predicate (grammar.md §15) — computed
	// BEFORE the scope so GROUP BY / DISTINCT / projection / WHERE all see the merge columns; a plain
	// ON join resolves here too. Joins are processed left-to-right so a later join's left side sees
	// the merges introduced by earlier ones (a USING chain). For each USING column the synthesized
	// predicate is `left.col = right.col` (3-valued, like any ON); the SURVIVING side becomes the
	// single merge column — the left for INNER/LEFT, the right for RIGHT (FULL JOIN USING, a COALESCE,
	// is 0A000). Both copies are hidden from `*`. Merges/predicates respect the comma SEGMENT (commit 1).
	var merges []mergeCol
	var hidden []int
	joinPreds := make([]*rExpr, len(sel.Joins))
	for k := range sel.Joins {
		j := &sel.Joins[k]
		seg := k + 1
		for seg >= 1 && !sel.Joins[seg-1].Comma {
			seg--
		}
		segOff := rels[seg].offset
		var segMerges []mergeCol
		for _, m := range merges {
			if m.index >= segOff {
				segMerges = append(segMerges, m)
			}
		}
		var segHidden []int
		for _, i := range hidden {
			if i >= segOff {
				segHidden = append(segHidden, i)
			}
		}
		// A NATURAL join (grammar.md §15) derives its USING list as the column names common to both
		// sides (left order); an explicit USING uses its written list. A NATURAL join with NO common
		// column degenerates to a CROSS join (an empty list → no predicate, no merge).
		var usingCols []string
		if j.Using != nil {
			usingCols = j.Using
		} else if j.Natural {
			usingCols = naturalCommonCols(rels, seg, k)
		}
		switch {
		case len(usingCols) > 0:
			if j.Kind == joinFull {
				return nil, newError(FeatureNotSupported, "FULL JOIN with a merged (USING/NATURAL) condition is not supported yet")
			}
			left := &scope{rels: rels[seg : k+1], parent: parent, catalog: db, allowSubquery: true, ctes: ctes, merges: segMerges, hidden: segHidden}
			var predAST *exprNode
			for _, name := range usingCols {
				lr, lerr := left.resolveBare(name)
				if lerr != nil || lr.level != 0 {
					return nil, newError(UndefinedColumn, "column \""+name+"\" specified in USING clause does not exist in left table")
				}
				li := lr.index
				llabel, lname := relOfIndex(rels, li)
				rightRel := &rels[k+1]
				rl := rightRel.table.ColumnIndex(name)
				if rl < 0 {
					return nil, newError(UndefinedColumn, "column \""+name+"\" specified in USING clause does not exist in right table")
				}
				ri := rightRel.offset + rl
				eq := newBinaryExpr(opEq,
					exprNode{Kind: exprQualifiedColumn, Qualifier: llabel, Column: lname},
					exprNode{Kind: exprQualifiedColumn, Qualifier: rightRel.label, Column: name})
				if predAST == nil {
					predAST = &eq
				} else {
					a := newBinaryExpr(opAnd, *predAST, eq)
					predAST = &a
				}
				mi := li
				if j.Kind == joinRight {
					mi = ri
				}
				merges = slices.DeleteFunc(merges, func(m mergeCol) bool { return strings.EqualFold(m.name, name) })
				merges = append(merges, mergeCol{name: strings.ToLower(name), index: mi})
				hidden = append(hidden, li, ri)
			}
			partial := &scope{rels: rels[seg : k+2], parent: parent, catalog: db, allowSubquery: true, ctes: ctes, merges: segMerges, hidden: segHidden}
			pred, perr := resolveBooleanFilter(partial, predAST, ptypes)
			if perr != nil {
				return nil, perr
			}
			joinPreds[k] = pred
		case j.On != nil:
			partial := &scope{rels: rels[seg : k+2], parent: parent, catalog: db, allowSubquery: true, ctes: ctes, merges: segMerges, hidden: segHidden}
			pred, perr := resolveBooleanFilter(partial, j.On, ptypes)
			if perr != nil {
				return nil, perr
			}
			joinPreds[k] = pred
		}
	}

	s := &scope{rels: rels, parent: parent, catalog: db, allowSubquery: true, ctes: ctes, merges: merges, hidden: hidden}

	// Resolve projections (paired with output names — §8), the optional WHERE (must be
	// boolean), and the ORDER BY keys against the full scope. A bare key ambiguous across
	// relations is 42702; an unknown qualifier is 42P01 (§15).
	// Resolve GROUP BY keys to flat row indices (a key is a bare/qualified column — grammar.md
	// §18). An unknown column is 42703, an ambiguous bare key 42702.
	var err error
	// Expand GROUP BY (including ROLLUP / CUBE / GROUPING SETS) into a list of grouping sets, resolve
	// each set's columns to flat row indices, and build the master grouping-column list (groupKeys) —
	// the ordered union of every set's columns, i.e. the columns groupable in at least one set
	// (spec/design/aggregates.md §12). A plain GROUP BY a, b expands to a single set [a, b]; no GROUP
	// BY expands to a single empty set (the whole-table grand total). An unknown column is 42703.
	// Each grouping term is one of (aggregates.md §15): a bare/qualified COLUMN; a select-list ORDINAL
	// (a bare integer literal — `GROUP BY 1`); an output ALIAS (a bare name that is not an input
	// column — PG's input-column-first rule); or a general EXPRESSION (`GROUP BY a+b`). A column key
	// keeps its real row slot (groupKeys holds its flat index); an expression key is MATERIALIZED —
	// its node collected into groupExprs and evaluated per row into a synthetic column inputWidth+k
	// whose index is the master key. groupKeyExprs records each master key's canonical AST (set for
	// expression keys) so a matching projection / HAVING / ORDER BY expression resolves to its
	// synthetic slot. The whole-row equality bucket machinery (resolvedSets, GROUPING SETS) is
	// unchanged — it works on master key indices.
	expanded, err := expandGroupBy(sel.GroupBy)
	if err != nil {
		return nil, err
	}
	inputWidth := s.width()
	groupKeys := make([]int, 0)
	groupKeyExprs := make([]*groupKeyExpr, 0)
	groupExprs := make([]*rExpr, 0)
	resolvedSets := make([][]int, 0, len(expanded))
	for _, set := range expanded {
		idxs := make([]int, 0, len(set))
		for _, key := range set {
			gr, gerr := resolveGroupTerm(s, *key, sel.Items, ptypes)
			if gerr != nil {
				return nil, gerr
			}
			var idx int
			if gr.isColumn {
				// `json` has no equality operator (PG ships no hash/btree opclass — spec/design/json.md
				// §5), so GROUP BY a json column is 42883. jsonb IS groupable.
				if s.columnAt(gr.index).Type.IsJson() {
					return nil, newError(UndefinedFunction, "could not identify an equality operator for type json")
				}
				idx = gr.index
				found := false
				for _, gk := range groupKeys {
					if gk == idx {
						found = true
						break
					}
				}
				if !found {
					groupKeys = append(groupKeys, idx)
					groupKeyExprs = append(groupKeyExprs, nil)
				}
			} else {
				if gr.ty.kind == rtJson {
					return nil, newError(UndefinedFunction, "could not identify an equality operator for type json")
				}
				// Reuse an identical expression key already registered (`GROUP BY a+b, a+b`).
				pos := -1
				for p, gk := range groupKeyExprs {
					if gk != nil && exprEqual(gk.canon, gr.canon) {
						pos = p
						break
					}
				}
				if pos >= 0 {
					idx = groupKeys[pos]
				} else {
					synth := inputWidth + len(groupExprs)
					groupExprs = append(groupExprs, gr.node)
					groupKeys = append(groupKeys, synth)
					groupKeyExprs = append(groupKeyExprs, &groupKeyExpr{canon: gr.canon, ty: gr.ty})
					idx = synth
				}
			}
			idxs = append(idxs, idx)
		}
		resolvedSets = append(resolvedSets, idxs)
	}

	// Functional-dependency grouping (aggregates.md §16, PG): when there is a SINGLE grouping set
	// that contains every primary-key column of a base table T, T's PK functionally determines every
	// column of T, so any T column (and expressions over them) may appear ungrouped. Make them
	// groupable by adding T's remaining columns as extra master grouping keys — the grouping is
	// UNCHANGED (each is constant within a group, so bucketing by [pk…, others…] yields the same
	// partition as by [pk…] alone, even across a join). Restricted to a single set: PG rejects the
	// dependency when a grouping set omits the PK. A CTE / derived table / SRF has an empty PK (a
	// synthetic key), so only base tables with a real PK contribute.
	if len(resolvedSets) == 1 {
		var extra []int
		for ri := range s.rels {
			rel := &s.rels[ri]
			if rel.qualifierOnly || rel.cte != nil || len(rel.table.PK) == 0 {
				continue
			}
			pkGrouped := true
			for _, ord := range rel.table.PK {
				if !slices.Contains(groupKeys, rel.offset+ord) {
					pkGrouped = false
					break
				}
			}
			if !pkGrouped {
				continue
			}
			for c := range rel.table.Columns {
				idx := rel.offset + c
				if !slices.Contains(groupKeys, idx) && !slices.Contains(extra, idx) {
					extra = append(extra, idx)
				}
			}
		}
		for _, idx := range extra {
			groupKeys = append(groupKeys, idx)
			groupKeyExprs = append(groupKeyExprs, nil)
			resolvedSets[0] = append(resolvedSets[0], idx)
		}
	}

	// An aggregate query has a GROUP BY or an aggregate in the select list. Its projection
	// resolves in collect mode — aggregates collect into synthetic slots and a non-grouped
	// column is 42803 (spec/design/aggregates.md §4/§6); a plain query resolves in Forbidden
	// mode (columns normal). Output names per grammar.md §8.
	// GROUP BY, an aggregate in the select list, OR a HAVING clause all make this an aggregate
	// query (HAVING alone groups the whole table — grammar.md §19). An aggregate inside a window
	// definition's keys also does — inline (`OVER (ORDER BY sum(x))`, caught by itemsHaveAggregate)
	// or in a WINDOW-clause entry (`WINDOW w AS (ORDER BY sum(x))`, scanned here before the desugar).
	// Note len(sel.GroupBy) (not groupKeys): GROUP BY GROUPING SETS (()) has an empty master list yet
	// is still an aggregate query (the whole-table grand total).
	isAgg := len(sel.GroupBy) > 0 || itemsHaveAggregate(sel.Items) || sel.Having != nil ||
		windowsHaveAggregate(sel.Windows)
	// A window query (a select-list OVER call) resolves its projection in window mode, where bare
	// columns read the input/grouped row and window calls collect into synthetic slots
	// (spec/design/window.md §5.1). A grouped query that ALSO windows is both collecting and
	// windowing (the window stage runs over the grouped rows — §2); a plain window query is only
	// windowing.
	// A window function may appear in the SELECT list OR in an ORDER BY key (grammar.md §10): either
	// sets up the window machinery so the key can be sorted by the computed window value.
	hasWindowSyntax := itemsHaveWindow(sel.Items) || orderByHasWindow(sel.OrderBy)
	projAgg := &aggCtx{collecting: isAgg, groupKeys: groupKeys, groupKeyExprs: groupKeyExprs}
	if hasWindowSyntax {
		projAgg.windowing = true
		// Window results land AFTER the materialized window keys, and (for a grouped query) after
		// every aggregate — neither final count is known until resolution finishes (an aggregate may
		// be nested in a later window argument or in HAVING). So a window result carries the
		// PLACEHOLDER base windowResultBase, rebased afterwards to inputWidth+len(windowKeys)+w
		// (window.md §5.1). A materialized window key carries windowKeyBase+k, rebased to inputWidth+k.
		projAgg.windowBase = windowResultBase
	}
	// Resolve the WINDOW clause: an entry may extend an earlier entry (`w2 AS (w ORDER BY …)` —
	// window.md §5), so each is merged against the already-resolved earlier entries (a missing/
	// forward/self base is 42704; PARTITION/ORDER overrides and a framed base are 42P20). Every
	// entry is resolved, even unreferenced ones, matching PostgreSQL. The result is all-inline
	// (Base == "") definitions the desugar pass copies/extends from.
	windowsResolved := sel.Windows
	if len(sel.Windows) > 0 {
		windowsResolved, err = resolveWindowClause(sel.Windows)
		if err != nil {
			return nil, err
		}
	}
	// Desugar `OVER name` / `OVER (base …)` references to their WINDOW-clause definitions before
	// resolution (window.md §5). The projection resolves against the desugared items; a reference to
	// an undefined window is 42704. A plain query with no window clause/refs uses sel.Items unchanged.
	items := sel.Items
	if hasWindowSyntax {
		items, err = desugarItems(sel.Items, windowsResolved)
		if err != nil {
			return nil, err
		}
	}
	projections, columnNames, columnTypes, err := resolveProjections(s, items, projAgg, ptypes)
	if err != nil {
		return nil, err
	}
	aggSpecs := projAgg.specs
	windowSpecs := projAgg.windowSpecs
	windowKeys := projAgg.windowKeys
	groupingSpecs := projAgg.groupingSpecs
	hasWindow := len(windowSpecs) > 0
	// SELECT DISTINCT dedups the projected rows by equality, but `json` has no equality operator
	// (PG ships no opclass — spec/design/json.md §5), so a json output column under DISTINCT is
	// 42883. jsonb IS distinguishable (its btree equality, §5).
	if sel.Distinct {
		for _, t := range columnTypes {
			if t.kind == rtJson {
				return nil, newError(UndefinedFunction, "could not identify an equality operator for type json")
			}
		}
	}
	// HAVING resolves in collect mode with window functions FORBIDDEN (42P20 — HAVING runs BEFORE the
	// window stage, window.md §7), continuing the aggregate specs (and GROUPING() calls) so they slot
	// after the projection's. It must be boolean (42804). A HAVING aggregate, like a projection one, is
	// part of the grouped row, so the window slots that follow are rebased over the final aggregate count.
	var having *rExpr
	if sel.Having != nil {
		hctx := &aggCtx{collecting: true, groupKeys: groupKeys, groupKeyExprs: groupKeyExprs, specs: aggSpecs, groupingSpecs: groupingSpecs}
		node, ty, herr := resolve(s, *sel.Having, nil, hctx, ptypes)
		if herr != nil {
			return nil, herr
		}
		if ty.kind != rtBool && ty.kind != rtNull {
			return nil, typeError("argument of HAVING must be boolean")
		}
		having = node
		aggSpecs = hctx.specs
		groupingSpecs = hctx.groupingSpecs
	}
	// (The window / GROUPING() placeholder rebases run AFTER the ORDER BY resolution below, because an
	// ORDER BY key may itself introduce a window function / aggregate / GROUPING() — so the final spec
	// counts, and thus every placeholder's real slot, are not known until ORDER BY is resolved.)
	// Build the grouping sets (spec/design/aggregates.md §12). For an aggregate query with no GROUP BY
	// this is the single empty (whole-table) set; otherwise one entry per resolved set, each recording
	// its bucket key columns, the per-master-slot value source (or -1 = NULL), and the GROUPING() mask.
	var groupSets []groupSetPlan
	if isAgg {
		groupSets = make([]groupSetPlan, 0, len(resolvedSets))
		for _, set := range resolvedSets {
			slotSrc := make([]int, len(groupKeys))
			for p := range slotSrc {
				slotSrc[p] = -1
			}
			for j, fidx := range set {
				for p, gk := range groupKeys {
					if gk == fidx {
						slotSrc[p] = j
						break
					}
				}
			}
			var mask int64
			for p, src := range slotSrc {
				if src < 0 {
					mask |= int64(1) << uint(p)
				}
			}
			keyCols := make([]int, len(set))
			copy(keyCols, set)
			groupSets = append(groupSets, groupSetPlan{keyCols: keyCols, slotSrc: slotSrc, mask: mask})
		}
	}
	// (The GROUPING SETS/window mutual-exclusion check and the GROUPING() placeholder rebase also run
	// after the ORDER BY resolution below — an ORDER BY GROUPING() grows groupingSpecs.)
	// SELECT DISTINCT over an aggregate query's output (output-row dedup) dedups the projected
	// group rows by equality, keeping the first occurrence, then LIMIT/OFFSET (aggregates.md §10) —
	// the same project->dedup->window pipeline as the non-aggregate DISTINCT path. The ORDER BY
	// restriction (each key must be a select-list item) is enforced once for both at the §11 block.
	var filter *rExpr
	if sel.Filter != nil {
		filter, err = resolveBooleanFilter(s, sel.Filter, ptypes)
		if err != nil {
			return nil, err
		}
	}
	// Scan-bound pushdown, per base relation: detect WHERE conjuncts that bound that relation's
	// scan — a PK range, else a secondary-index equality — so it seeks/ranges instead of walking
	// the whole B-tree (cost.md §3 "bounded scan" / "index-bounded scan"; indexes.md §5). The
	// filter is resolved against the full FROM scope, so a relation's column is the GLOBAL index
	// rel.offset+local; isConstSource only accepts a literal/param/outer const (never a sibling
	// column), so a JOIN base table is bounded only by a CONSTANT predicate on its own columns —
	// `b.pk = a.x` (index-nested-loop) stays a full scan, a follow-on. Sound for outer joins too:
	// a non-NULL conjunct in WHERE eliminates that relation's NULL-extended rows, so bounding it
	// cannot drop a surviving row.
	relBounds := make([]*scanBound, len(rels))
	if filter != nil {
		for i, rel := range rels {
			// A set-returning relation or a derived table is a computed row source with no
			// PK/index — it never bounds (functions.md §10, §42), so skip detection for it.
			if srfPlans[i] != nil || derivedPlans[i] != nil {
				continue
			}
			relBounds[i] = detectScanBound(filter, rel, db)
		}
	}
	// Index-nested-loop pushdown (cost.md §3 "JOIN"): a join inner relation whose primary key /
	// indexed column is compared to a SIBLING column of an earlier relation (`a JOIN b ON b.pk = a.x`)
	// is re-materialized per outer row, seeking instead of full-scanning — O(N·M) → O(N·log M).
	// Detected from the join's ON and the WHERE. Gated to a base table (an SRF / derived table / CTE /
	// lateral item has no store to seek) that is the RIGHT/nullable side of an INNER/CROSS/LEFT join
	// (a RIGHT/FULL preserved side cannot be bounded per outer row). rels[0] has no earlier relation;
	// its join is sel.Joins[i-1]. A non-nil entry takes precedence over the once-materialized relBounds.
	relINLBounds := make([]*scanBound, len(rels))
	for i, rel := range rels {
		if i == 0 || srfPlans[i] != nil || derivedPlans[i] != nil || rel.cte != nil || lateralFlags[i] {
			continue
		}
		if k := sel.Joins[i-1].Kind; k != joinInner && k != joinCross && k != joinLeft {
			continue
		}
		relINLBounds[i] = detectINLBound(joinPreds[i-1], filter, rel, db)
	}
	// ORDER BY resolution. In an aggregate query a key resolves against the GROUP KEYS — a
	// grouping column gives its synthetic-row slot, a non-grouping column is 42803 (the
	// grouping-error rule, grammar.md §18); the sort runs on the group rows. In a plain query
	// keys resolve against the FROM scope (a flat row index). An outer (correlated) ORDER BY key
	// — ordering by an enclosing-query constant — is degenerate and 0A000 (§26).
	// ORDER BY resolution (spec/design/grammar.md §10). Each key is one of three modes (set at parse):
	// an output-column ORDINAL, a COLUMN reference, or a general EXPRESSION. A column / ordinal-to-column
	// key resolves to a real row slot (against the GROUP KEYS in an aggregate query — a grouping column
	// gives its synthetic slot, a non-grouping column is 42803; else against the FROM scope). A general-
	// expression key (and an ordinal pointing at a COMPUTED select-list item) is MATERIALIZED: its
	// expression is resolved here (introducing a new aggregate in a grouped query if it names one),
	// collected into orderExprs, and given a placeholder sort slot orderExprBase+k rebased to
	// final_width+k below — the window-key precedent (window.md §5.1).
	order := make([]orderSlot, 0, len(sel.OrderBy))
	var orderExprs []*rExpr
	for _, key := range sel.OrderBy {
		// Classify the key into a row slot (a column / ordinal-to-column) or a source expression (a
		// general expression, or an ordinal pointing at a computed projection).
		var slotRes resolved
		var orderExpr *exprNode
		if key.Ordinal != nil {
			ord := *key.Ordinal
			var ncols int64
			if items.All {
				ncols = int64(s.width())
			} else {
				ncols = int64(len(items.Items))
			}
			if ord < 1 || ord > ncols {
				return nil, newError(InvalidColumnReference,
					fmt.Sprintf("ORDER BY position %d is not in select list", ord))
			}
			pos := int(ord - 1)
			if items.All {
				slotRes = resolved{level: 0, index: pos}
			} else {
				switch e := items.Items[pos].Expr; e.Kind {
				case exprColumn:
					if slotRes, err = s.resolveBare(e.Column); err != nil {
						return nil, err
					}
				case exprQualifiedColumn:
					if slotRes, err = s.resolveQualified(e.Qualifier, e.Column); err != nil {
						return nil, err
					}
				default:
					orderExpr = &items.Items[pos].Expr
				}
			}
		} else if key.Expr != nil {
			orderExpr = key.Expr
		} else if key.Qualifier != "" {
			// A qualified key (`t.a`) is always an input column — never an output alias (PG; §10).
			if slotRes, err = s.resolveQualified(key.Qualifier, key.Column); err != nil {
				return nil, err
			}
		} else {
			// A bare name resolves an OUTPUT column (an AS alias or item's derived name) BEFORE an input
			// column — PostgreSQL's SQL92 rule (grammar.md §10). A match routes the item EXACTLY like the
			// same ORDER BY ordinal; no match falls through to the FROM scope (the prior behavior).
			matched, merr := orderAliasMatch(items, key.Column, s)
			if merr != nil {
				return nil, merr
			}
			switch {
			case matched == nil:
				if slotRes, err = s.resolveBare(key.Column); err != nil {
					return nil, err
				}
			case matched.Kind == exprColumn:
				if slotRes, err = s.resolveBare(matched.Column); err != nil {
					return nil, err
				}
			case matched.Kind == exprQualifiedColumn:
				if slotRes, err = s.resolveQualified(matched.Qualifier, matched.Column); err != nil {
					return nil, err
				}
			default:
				orderExpr = matched
			}
		}

		if orderExpr == nil {
			// A column / ordinal-to-column key resolves to a real row slot.
			r := slotRes
			if r.level != 0 {
				// A correlated (outer) column ORDER BY key — the local sort row has no slot for an
				// enclosing-query column, so materialize it as an OuterColumn expression evaluated per row
				// against the outer-row environment (query.order_by_correlated), exactly like a general-
				// expression key. PostgreSQL accepts it (a degenerate constant leading key).
				rexpr, ty, rerr := resolveColumnRef(s, &aggCtx{}, r, key.Column)
				if rerr != nil {
					return nil, rerr
				}
				if ty.kind == rtJson {
					return nil, newError(UndefinedFunction, "could not identify an ordering operator for type json")
				}
				var coll *Collation
				if key.Collation != "" {
					if ty.kind != rtText && ty.kind != rtNull {
						return nil, typeError(fmt.Sprintf("collations are not supported by type %s", rtName(ty)))
					}
					if coll, err = resolveCollationName(s.catalog, key.Collation); err != nil {
						return nil, err
					}
				} else if cn := s.columnOf(r).Collation; cn != "" {
					if coll, err = resolveCollationName(s.catalog, cn); err != nil {
						return nil, err
					}
				}
				k := len(orderExprs)
				orderExprs = append(orderExprs, rexpr)
				order = append(order, orderSlot{idx: orderExprBase + k, descending: key.Descending, nullsFirst: key.NullsFirst, collation: coll})
				continue
			}
			// `json` has no ordering operator (PG ships no btree opclass — spec/design/json.md §5):
			// ORDER BY a json column is 42883. jsonb IS orderable (its btree total order, §5).
			if s.columnOf(r).Type.IsJson() {
				return nil, newError(UndefinedFunction, "could not identify an ordering operator for type json")
			}
			idx := r.index
			// The sort key's collation (spec/design/collation.md §1/§7). An explicit COLLATE must be on a
			// text column (42804) and name a loaded collation ("C" → byte order, else 42704); absent a
			// clause, the key inherits the column's frozen (implicit) collation.
			var coll *Collation
			if key.Collation != "" {
				if !s.columnOf(r).Type.IsText() {
					return nil, typeError(fmt.Sprintf(
						"collations are not supported by type %s", s.columnOf(r).Type.CanonicalName(),
					))
				}
				if coll, err = resolveCollationName(s.catalog, key.Collation); err != nil {
					return nil, err
				}
			} else if cn := s.columnOf(r).Collation; cn != "" {
				if coll, err = resolveCollationName(s.catalog, cn); err != nil {
					return nil, err
				}
			}
			slot := idx
			if isAgg {
				slot = -1
				for pos, gk := range groupKeys {
					if gk == idx {
						slot = pos
						break
					}
				}
				if slot < 0 {
					return nil, groupingErrorColumn(key.Column)
				}
			}
			order = append(order, orderSlot{idx: slot, descending: key.Descending, nullsFirst: key.NullsFirst, collation: coll})
			continue
		}

		// Resolve the key expression in the SAME context the projection used, so a window function /
		// GROUPING() / aggregate it contains collects into the shared specs and references the same
		// placeholders (rebased together after this loop — grammar.md §10): a grouped query collects over
		// the group keys + aggregates + GROUPING() calls (a new aggregate or GROUPING() the select list
		// lacks is allowed); a window query collects window specs/keys; a grouped+window query does both
		// (query.order_by_grouped_window); a plain query forbids aggregates (42803) and window functions
		// (42P20).
		var rexpr *rExpr
		var ty resolvedType
		octx := &aggCtx{collecting: isAgg, groupKeys: groupKeys, groupKeyExprs: groupKeyExprs, specs: aggSpecs, groupingSpecs: groupingSpecs}
		if hasWindowSyntax {
			octx.windowing = true
			octx.windowBase = windowResultBase
			octx.windowSpecs = windowSpecs
			octx.windowKeys = windowKeys
		}
		rexpr, ty, err = resolve(s, *orderExpr, nil, octx, ptypes)
		if err != nil {
			return nil, err
		}
		aggSpecs = octx.specs
		groupingSpecs = octx.groupingSpecs
		windowSpecs = octx.windowSpecs
		windowKeys = octx.windowKeys
		// A correlated ORDER BY expression (one referencing an enclosing query) is allowed
		// (query.order_by_correlated): the outer column is a per-evaluation constant of the enclosing
		// row, evaluated against the outer-row environment still in scope when materializeOrderExprs
		// runs. PostgreSQL accepts it; it is a degenerate (constant) leading key.
		// A non-orderable result type — json (no btree opclass) — is 42883; jsonb orders.
		if ty.kind == rtJson {
			return nil, newError(UndefinedFunction, "could not identify an ordering operator for type json")
		}
		// The collation of an expression key (collation.md §1): an explicit trailing COLLATE (rare —
		// parseExpr usually absorbs one into the key) must be on a text key (42804); otherwise it is
		// DERIVED from the key expression.
		var coll *Collation
		if key.Collation != "" {
			if ty.kind != rtText && ty.kind != rtNull {
				return nil, typeError(fmt.Sprintf("collations are not supported by type %s", rtName(ty)))
			}
			if coll, err = resolveCollationName(s.catalog, key.Collation); err != nil {
				return nil, err
			}
		} else {
			d, derr := deriveCollation(s, *orderExpr)
			if derr != nil {
				return nil, derr
			}
			if coll, err = resolveDeriv(s.catalog, d); err != nil {
				return nil, err
			}
		}
		k := len(orderExprs)
		orderExprs = append(orderExprs, rexpr)
		order = append(order, orderSlot{idx: orderExprBase + k, descending: key.Descending, nullsFirst: key.NullsFirst, collation: coll})
	}
	// All specs are now final (an ORDER BY key may have introduced a window function / aggregate /
	// GROUPING()). Recompute hasWindow and rebase every placeholder — in the projections, HAVING, AND
	// the materialized ORDER BY expressions — to its real trailing slot (window.md §5.1). The window
	// stage's row is [input… , materialized window keys… , window results…]; inputWidth is the grouped
	// row's width (group keys + every aggregate) for a grouped+window query, else the FROM scope width.
	hasWindow = len(windowSpecs) > 0
	if hasWindow {
		// The grouped row the window stage extends is [master cols…, agg results…, GROUPING results…]
		// (the GROUPING columns precede the window columns — aggregates.md §21), so a grouped+window
		// query's window input width includes the GROUPING() results.
		inputWidth := 0
		if isAgg {
			inputWidth = len(groupKeys) + len(aggSpecs) + len(groupingSpecs)
		} else {
			inputWidth = s.width()
		}
		keyBase := inputWidth
		resultBase := inputWidth + len(windowKeys)
		// Bound to [windowKeyBase, 2·windowKeyBase) so a GROUPING() placeholder (the higher
		// groupingGsBase) in a window key is not clobbered here (it rebases below — §21).
		for i := range windowSpecs {
			for j, pk := range windowSpecs[i].partition {
				if pk >= windowKeyBase && pk < windowKeyBase*2 {
					windowSpecs[i].partition[j] = keyBase + (pk - windowKeyBase)
				}
			}
			for j := range windowSpecs[i].order {
				if windowSpecs[i].order[j].idx >= windowKeyBase && windowSpecs[i].order[j].idx < windowKeyBase*2 {
					windowSpecs[i].order[j].idx = keyBase + (windowSpecs[i].order[j].idx - windowKeyBase)
				}
			}
		}
		for _, p := range projections {
			rebasePlaceholderCols(p, windowResultBase, resultBase)
		}
		for _, oe := range orderExprs {
			rebasePlaceholderCols(oe, windowResultBase, resultBase)
		}
	}
	// GROUPING SETS / GROUPING() combined with window functions (aggregates.md §21): the window stage
	// runs over the unioned grouping-set rows. The grouped row is [master cols…, agg results…, GROUPING
	// results…] and the window stage appends [window keys…, window results…] after, so the two no longer
	// collide — GROUPING rebases below the window bases.
	// Rebase the GROUPING() placeholder slots to their real trailing synthetic slots
	// len(groupKeys)+len(aggSpecs)+g (the GROUPING results follow the master columns and aggregate
	// results — §12), in the projections, HAVING, and the materialized ORDER BY expressions.
	if len(groupingSpecs) > 0 {
		gbase := len(groupKeys) + len(aggSpecs)
		for _, p := range projections {
			rebasePlaceholderCols(p, groupingGsBase, gbase)
		}
		if having != nil {
			rebasePlaceholderCols(having, groupingGsBase, gbase)
		}
		for _, oe := range orderExprs {
			rebasePlaceholderCols(oe, groupingGsBase, gbase)
		}
	}
	// Rebase each materialized expression-key slot to its real trailing position now that the row layout
	// is final. The materialized order values are appended AFTER the input / window / grouped columns
	// (grammar.md §10): for a grouped+window query the grouped row is first extended by the window stage,
	// so the order values follow the window results.
	var orderValueBase int
	switch {
	case isAgg && hasWindow:
		orderValueBase = len(groupKeys) + len(aggSpecs) + len(groupingSpecs) + len(windowKeys) + len(windowSpecs)
	case isAgg:
		orderValueBase = len(groupKeys) + len(aggSpecs) + len(groupingSpecs)
	case hasWindow:
		orderValueBase = s.width() + len(windowKeys) + len(windowSpecs)
	default:
		orderValueBase = s.width()
	}
	for i := range order {
		if order[i].idx >= orderExprBase {
			order[i].idx = orderValueBase + (order[i].idx - orderExprBase)
		}
	}

	// SELECT DISTINCT restriction (spec/design/grammar.md §11): once duplicates collapse, an ORDER BY
	// key must have a per-row value in the projected output — a bare/qualified column that is projected,
	// an ordinal (which names a select-list item by position), or a general expression that STRUCTURALLY
	// matches a select-list item. Otherwise 42P10 (matching PostgreSQL). Aliases are invisible to ORDER
	// BY (§8); a SELECT DISTINCT * projects every column, so the restriction never bites.
	if sel.Distinct && len(sel.OrderBy) > 0 && !items.All {
		projected := make(map[int]bool)
		for _, it := range items.Items {
			switch it.Expr.Kind {
			case exprColumn:
				if r, e := s.resolveBare(it.Expr.Column); e == nil && r.level == 0 {
					projected[r.index] = true
				}
			case exprQualifiedColumn:
				if r, e := s.resolveQualified(it.Expr.Qualifier, it.Expr.Column); e == nil && r.level == 0 {
					projected[r.index] = true
				}
			}
		}
		for i := range sel.OrderBy {
			key := &sel.OrderBy[i]
			inList := false
			switch {
			case key.Ordinal != nil:
				inList = true
			case key.Expr != nil:
				for j := range items.Items {
					if exprEqual(*key.Expr, items.Items[j].Expr) {
						inList = true
						break
					}
				}
			default:
				// A bare name that binds an output column (alias/derived name) names a select-list
				// item, so it is projected (the alias form, §10). Ambiguity was already raised above.
				if key.Qualifier == "" {
					if m, _ := orderAliasMatch(items, key.Column, s); m != nil {
						inList = true
						break
					}
				}
				var r resolved
				var e error
				if key.Qualifier != "" {
					r, e = s.resolveQualified(key.Qualifier, key.Column)
				} else {
					r, e = s.resolveBare(key.Column)
				}
				inList = e == nil && r.level == 0 && projected[r.index]
			}
			if !inList {
				return nil, newError(InvalidColumnReference,
					"for SELECT DISTINCT, ORDER BY expressions must appear in select list")
			}
		}
	}

	// The join predicates were resolved above (alongside the USING/NATURAL merges, which the scope
	// now carries). Pair each with its join kind — the kind only changes how unmatched rows are
	// handled in the executor loop, not the predicate (grammar.md §15).
	joins := make([]planJoin, len(sel.Joins))
	for k, j := range sel.Joins {
		joins[k] = planJoin{kind: j.Kind, on: joinPreds[k]}
	}

	// Assemble the owned plan (table NAMES + offsets/widths replace the scope's *Table, so the
	// plan outlives the scope and a correlated subquery can re-execute it per row).
	planRels := make([]planRel, len(s.rels))
	for i, rel := range s.rels {
		planRels[i] = planRel{tableName: rel.table.Name, db: rel.db, offset: rel.offset, colCount: len(rel.table.Columns), srf: srfPlans[i], cte: rel.cte, derived: derivedPlans[i], lateral: lateralFlags[i]}
	}
	// The touched set per relation (cost.md §3 "The touched set"; large-values.md §14): the
	// columns this query statically references, collected depth-aware so a correlated
	// subquery's outer reference back into this scope counts. An aggregate query's projections
	// / HAVING / ORDER BY index the synthetic group row, whose inputs are exactly the group
	// keys + aggregate arguments collected here; a plain query's projections and ORDER BY keys
	// index the combined row directly.
	totalCols := 0
	for _, rel := range planRels {
		totalCols += rel.colCount
	}
	touched := make([]bool, totalCols)
	collectTouched(filter, 0, touched)
	for k := range joins {
		collectTouched(joins[k].on, 0, touched)
	}
	if isAgg {
		// A column grouping key is a real input column (mark it); an expression grouping key has a
		// SYNTHETIC index (inputWidth+k, out of touched's range) — its real input columns are reached
		// through its materialized groupExprs node instead (aggregates.md §15).
		for _, gk := range groupKeys {
			if gk < totalCols {
				touched[gk] = true
			}
		}
		for _, ge := range groupExprs {
			collectTouched(ge, 0, touched)
		}
		for i := range aggSpecs {
			collectTouched(aggSpecs[i].operand, 0, touched)
			// An aggregate reads real input columns beyond its operand: the FILTER predicate
			// (agg(x) FILTER (WHERE cond) — aggregates.md §11), an ordered-set direct argument, and a
			// hypothetical-set's WITHIN GROUP key operands / direct args (aggregates.md §13/§19). Without
			// these the referenced column is left unfetched by the lazy/masked scan (large-values.md §14)
			// and folds as NULL — a memory-vs-disk divergence (count(*) FILTER, rank() WITHIN GROUP).
			collectTouched(aggSpecs[i].filter, 0, touched)
			collectTouched(aggSpecs[i].osaFrac, 0, touched)
			if aggSpecs[i].hypo != nil {
				for _, k := range aggSpecs[i].hypo.keys {
					collectTouched(k, 0, touched)
				}
				for _, a := range aggSpecs[i].hypo.args {
					collectTouched(a, 0, touched)
				}
			}
		}
	} else {
		for _, p := range projections {
			collectTouched(p, 0, touched)
		}
		// A column-key ORDER BY slot is a real input column (< totalCols) — mark it; a materialized
		// expression-key slot is synthetic (>= totalCols, after rebase) whose input columns are reached
		// through its orderExprs expression instead (collected below).
		for _, o := range order {
			if o.idx < totalCols {
				touched[o.idx] = true
			}
		}
		// Each materialized ORDER BY expression key reads real input columns (a plain query resolves it
		// against the FROM scope; a grouped query reaches them through its group keys / aggregate
		// arguments, already marked above).
		for _, oe := range orderExprs {
			collectTouched(oe, 0, touched)
		}
		// A window query also reads each window function's PARTITION BY + ORDER BY keys, beyond what
		// the projection's window-result slots reference. A bare-column key is a real input slot
		// (< totalCols) — mark it; a materialized expression key is a synthetic slot (>= totalCols,
		// after rebase) whose input columns are reached through its windowKeys expression (below).
		for _, spec := range windowSpecs {
			for _, pk := range spec.partition {
				if pk < totalCols {
					touched[pk] = true
				}
			}
			for _, o := range spec.order {
				if o.idx < totalCols {
					touched[o.idx] = true
				}
			}
			// The window function's ARGUMENT operands (sum(amount)'s amount, lag(v, off, def)'s
			// value/offset/default) and its FILTER read real input columns too — the row-based
			// window stage evaluates them per frame row (window.md §5.2). Without this the operand
			// column is left unfetched by the lazy/masked scan (large-values.md §14) and folds as
			// NULL. Mirrors the aggregate branch's collectTouched(aggSpecs[i].operand, …) above.
			for _, a := range spec.args {
				collectTouched(a, 0, touched)
			}
			collectTouched(spec.filter, 0, touched)
		}
		// Each materialized window-key expression reads real input columns (a plain window query
		// resolves its keys against the FROM scope).
		for _, ke := range windowKeys {
			collectTouched(ke, 0, touched)
		}
	}
	// A set-returning relation's arguments and a LATERAL derived table's body read real input columns
	// too — an implicitly-lateral SRF arg / lateral body sees an earlier sibling relation (functions.md
	// §10, grammar.md §44). Applies to aggregate and plain queries alike (an aggregate query can carry a
	// lateral SRF). Without this the referenced column is left unfetched by the lazy/masked scan
	// (large-values.md §14) and the SRF/body reads NULL — a memory-vs-disk divergence.
	for i := range planRels {
		if planRels[i].srf != nil {
			// A LATERAL SRF (any SRF at position i>0) resolves its sibling columns as reOuterColumn at
			// level 1 (resolveSRF's lateralParent, the same frame the runtime pushes) — so collect at
			// depth 1, not 0. An i==0 SRF has no sibling correlation (constant/param args), so depth 1
			// marks nothing there. functions.md §10, grammar.md §44.
			for _, a := range planRels[i].srf.args {
				collectTouched(a, 1, touched)
			}
		}
		if planRels[i].derived != nil {
			collectTouchedPlan(planRels[i].derived, 1, touched)
		}
	}
	relMasks := make([][]bool, len(planRels))
	for i, rel := range planRels {
		relMasks[i] = touched[rel.offset : rel.offset+rel.colCount]
	}
	// ORDER BY satisfied by primary-key scan order (spec/design/cost.md §3): a single base table,
	// non-aggregate, non-DISTINCT SELECT whose ORDER BY keys are a prefix of the relation's PRIMARY
	// KEY columns — collation-matching the column's stored key form, all in one direction (ASC ⇒
	// forward scan, DESC ⇒ a reverse scan over the full PK) — needs no sort, since the table scan
	// already yields rows in that order. The streaming scan then elides the sort (and, with a LIMIT,
	// short-circuits a top-N).
	// (DISTINCT is allowed: when the scan already yields ORDER BY order, the dedup runs streaming —
	// keeping first occurrence in scan order — and the sort is elided, cost.md §3 "DISTINCT".)
	pkOrdered, pkReverse := false, false
	if !isAgg && len(order) > 0 && len(orderExprs) == 0 && len(planRels) == 1 &&
		planRels[0].srf == nil && planRels[0].cte == nil && planRels[0].derived == nil {
		pkOrdered, pkReverse = db.orderSatisfiedByPK(s.rels[0].table, planRels[0].offset, order)
	}
	// ORDER BY satisfied by SECONDARY-INDEX scan order (cost.md §3): when the PK scan does NOT
	// satisfy the order but a B-tree index's columns do, and there is a LIMIT, walk that index and
	// point-look-up each row — a top-N that avoids the blocking sort. Gated to a LIMIT and to no
	// WHERE pushdown bound (combining them is a follow-on); mutually exclusive with pkOrdered.
	var indexOrder *indexOrderPlan
	if !isAgg && !hasWindow && !sel.Distinct && !pkOrdered && sel.Limit != nil && len(order) > 0 &&
		len(orderExprs) == 0 && len(planRels) == 1 && planRels[0].srf == nil && planRels[0].cte == nil &&
		planRels[0].derived == nil && relBounds[0] == nil {
		indexOrder = db.orderSatisfiedByIndex(s.rels[0].table, planRels[0].offset, order)
	}
	// ORDER BY satisfied by the OUTER relation's PK scan order in a two-table INNER/CROSS join
	// (cost.md §3 "JOIN"): the nested loop drives the outer (rels[0]) in PK order, so the join output
	// is already in (outer PK, inner key) order — the sort is elided, and with a LIMIT the loop
	// short-circuits a top-N. Gated to exactly two non-lateral base relations, an INNER/CROSS join, a
	// LIMIT, and a FORWARD outer-PK order with NO key beyond the outer PK (an extra key is a real
	// tie-break the outer scan order does not satisfy — the outer PK is not unique over the join
	// output). The outer must carry no non-PK bound (a PK bound / no bound keeps it in PK order).
	joinPkOrdered := false
	if !isAgg && !hasWindow && !sel.Distinct && len(order) > 0 && len(orderExprs) == 0 && sel.Limit != nil &&
		len(planRels) == 2 && len(joins) == 1 && (joins[0].kind == joinInner || joins[0].kind == joinCross) &&
		!planRels[0].lateral && planRels[0].srf == nil && planRels[0].cte == nil && planRels[0].derived == nil &&
		!planRels[1].lateral && planRels[1].srf == nil && planRels[1].cte == nil && planRels[1].derived == nil &&
		!relBounds[0].needsEagerScan() &&
		relINLBounds[0] == nil && relINLBounds[1] == nil &&
		len(order) <= len(s.rels[0].table.PKIndices()) {
		ok, reverse := db.orderSatisfiedByPK(s.rels[0].table, planRels[0].offset, order)
		joinPkOrdered = ok && !reverse
	}
	return &selectPlan{
		rels: planRels, joins: joins, filter: filter, isAgg: isAgg, groupKeys: groupKeys,
		groupExprs: groupExprs,
		groupSets:  groupSets, groupingSpecs: groupingSpecs,
		aggSpecs: aggSpecs, hasWindow: hasWindow, windowSpecs: windowSpecs, windowKeys: windowKeys, having: having,
		order: order, orderExprs: orderExprs, projections: projections,
		columnNames: columnNames, columnTypes: columnTypes, distinct: sel.Distinct,
		limit: sel.Limit, offset: sel.Offset, pkOrdered: pkOrdered, pkReverse: pkReverse, indexOrder: indexOrder, joinPkOrdered: joinPkOrdered, relBounds: relBounds, relINLBounds: relINLBounds, relMasks: relMasks,
	}, nil
}

// orderSatisfiedByPK reports whether a single base relation's ORDER BY is satisfied by its
// PRIMARY-KEY scan order (spec/design/cost.md §3), and in which DIRECTION: it returns
// (satisfied, reverse) where reverse=true means the order is all-DESC over the full PK, served by a
// REVERSE scan, and reverse=false means all-ASC (forward). The direction comes from the first ORDER
// BY key; every PK-prefix key must share it (no mixed ASC/DESC). Two asymmetric coverage rules,
// both grounded in the eager sort being a STABLE sort that breaks ties in input = PK-ascending
// order: forward (ASC) allows a strict PREFIX of the PK (the remaining columns tie-break ascending,
// exactly the input order the stable sort preserves); reverse (DESC) requires the FULL PK
// (len(order) >= len(pk)) because a strict DESC prefix of a composite PK would have the eager sort
// break ties in PK-ascending input order, which a reverse scan inverts — so reverse is restricted
// to the unique full key, where no ties remain.
func (db *engine) orderSatisfiedByPK(table *catTable, offset int, order []orderSlot) (bool, bool) {
	pk := table.PKIndices()
	if len(pk) == 0 {
		return false, false // no PK (synthetic rowid order is not a user-visible column)
	}
	reverse := order[0].descending // direction comes from the first ORDER BY key
	if reverse && len(order) < len(pk) {
		return false, false // a reverse scan needs the full (unique) PK so no ties remain
	}
	m := len(order)
	if len(pk) < m {
		m = len(pk)
	}
	for i := 0; i < m; i++ {
		o := order[i]
		if o.descending != reverse {
			return false, false // every PK-prefix key must share the scan direction (no mixed ASC/DESC)
		}
		if o.idx != offset+pk[i] {
			return false, false // must be the i-th PK column, in key order
		}
		// The ORDER BY key must sort by the SAME order the stored PK key realizes. A raw-byte
		// (C/non-text) key matches a key with no collation; a Full-collated key matches the SAME
		// collation; a Skewed/unresolvable collation never matches (its stored keys are at the
		// file's pinned version, so the scan order would be wrong for the loaded one — §12).
		coll, push := db.keyCollationCtx(table.Columns[pk[i]])
		if !push {
			return false, false // Skewed / unresolvable
		}
		if coll == nil {
			if o.collation != nil {
				return false, false // raw-byte key, but the ORDER BY key carries a collation
			}
		} else {
			if o.collation == nil || o.collation.Name != coll.Name {
				return false, false
			}
		}
	}
	return true, reverse
}

// pkStorageWidth returns the fixed byte width of a table's stored primary key (encodePKKey = the
// bare per-column order-preserving keys concatenated, no NULL tags — a PK is NOT NULL) and true, or
// (0, false) when ANY PK column is variable-width (text/decimal/bytea/interval) or non-scalar
// (range/composite), or the table has no PK. Used by the secondary-index-order scan to peel the PK
// suffix off the END of each index entry key (the "key-suffix skip", cost.md §3) — sound only when
// that suffix is a known fixed length.
func pkStorageWidth(table *catTable) (int, bool) {
	pk := table.PKIndices()
	if len(pk) == 0 {
		return 0, false // a no-PK table keys on a synthetic rowid — not handled this slice
	}
	w := 0
	for _, ci := range pk {
		s, ok := table.Columns[ci].Type.AsScalar()
		if !ok || !s.IsFixedWidth() {
			return 0, false // a non-scalar / variable-width PK suffix is not a fixed peel
		}
		w += s.WidthBytes()
	}
	return w, true
}

// indexOrderPlan is the secondary-index-order plan: walk a B-tree index in key order to satisfy an
// ORDER BY without a sort, point-looking-up each row by its primary key (cost.md §3).
type indexOrderPlan struct {
	nameKey string // the index store's key — the lowercased index name
	pkWidth int    // the fixed PK-suffix byte width to peel off the END of each index entry key
}

// orderSatisfiedByIndex reports whether a single base relation's ORDER BY is satisfied by walking one
// of its B-tree SECONDARY indexes in key order (cost.md §3 "secondary-index order"), and which index.
// The index store holds its entries in (indexed columns, storage key) order, so a forward walk
// delivers rows in ORDER BY <indexed columns> ASC NULLS LAST order, ties broken by the PK — exactly
// the eager stable sort's tie-break. Returns non-nil iff the ORDER BY keys are EXACTLY a B-tree
// index's columns (same count, same columns in key order), each ASC with default NULLS LAST (the
// index stores NULL as 0x01 after a present 0x00 → NULLS LAST; an explicit NULLS FIRST does not
// match) and sorting by the column's stored key collation (Skewed/unresolvable → refuse, §12), AND
// the table's PK is fixed-width. The exact-match requirement is load-bearing: a strict prefix of a
// multi-column index would tie-break by the remaining index columns, not the PK.
func (db *engine) orderSatisfiedByIndex(table *catTable, offset int, order []orderSlot) *indexOrderPlan {
	pkWidth, ok := pkStorageWidth(table)
	if !ok {
		return nil
	}
	for _, idx := range table.Indexes {
		if idx.Kind != indexBtree {
			continue // only an ordered B-tree realizes the column order (GIN/GiST do not)
		}
		if len(order) != len(idx.Columns) {
			continue // the ORDER BY must be EXACTLY the index columns (see the doc — tie-break)
		}
		matches := true
		for i, o := range order {
			if o.descending || o.nullsFirst {
				matches = false // ASC + NULLS LAST only — the order a forward index walk realizes
				break
			}
			if o.idx != offset+idx.Columns[i] {
				matches = false
				break
			}
			coll, push := db.keyCollationCtx(table.Columns[idx.Columns[i]])
			if !push { // Skewed / unresolvable — never walked for order (§12)
				matches = false
				break
			}
			if coll == nil {
				if o.collation != nil {
					matches = false
					break
				}
			} else if o.collation == nil || o.collation.Name != coll.Name {
				matches = false
				break
			}
		}
		if matches {
			return &indexOrderPlan{nameKey: strings.ToLower(idx.Name), pkWidth: pkWidth}
		}
	}
	return nil
}

// resolveSRF resolves a FROM-clause set-returning function call (generate_series(...)) into a
// SYNTHETIC one-column relation plus its resolved argument expressions (spec/design/functions.md
// §10). Only generate_series exists this slice (any other name → 42883), with 2 or 3 integer
// args (a wrong arity/type → 42883). Non-LATERAL: the args resolve against an EMPTY-local-rels
// scope whose parent is the enclosing query, so $N and correlated outer columns resolve while a
// sibling FROM table does not (42703/42P01). The produced column is typed at the PROMOTED integer
// type of the args (PG); a NULL-typed arg contributes no width. Its NAME follows PostgreSQL's
// single-column function-alias rule: the table alias when one is given (generate_series(1,5) AS g
// ⇒ column g), else the function name generate_series.
func (db *engine) resolveSRF(name string, args []*exprNode, alias *string, columnDefs []typeFieldDef, parent *scope, ctes []*cteBinding, ptypes *paramTypes) (*catTable, *srfPlan, error) {
	// The args see only params/outer — never sibling FROM tables (non-LATERAL); CTE bindings are
	// inherited so an arg subquery can reference a CTE (cte.md §2).
	argScope := &scope{rels: nil, parent: parent, catalog: db, allowSubquery: true, ctes: ctes}
	lname := strings.ToLower(name)
	// Record-returning functions (R1, json-table.md §2): json[b]_to_record → one record row,
	// json[b]_to_recordset → setof record. They take their column shape from the C0 col-def list
	// `AS t(col type, …)`. Dispatched first, before the col-def-list guard below.
	switch lname {
	case "json_to_record", "jsonb_to_record", "json_to_recordset", "jsonb_to_recordset":
		jsonb := strings.HasPrefix(lname, "jsonb")
		set := strings.HasSuffix(lname, "set")
		return db.resolveJSONRecord(lname, jsonb, set, args, alias, columnDefs, argScope, ptypes)
	// json[b]_populate_record(set) (R2, json-table.md §2): like json[b]_to_record(set) but the
	// column shape comes from the COMPOSITE TYPE of the (typically NULL) first argument.
	case "json_populate_record", "jsonb_populate_record", "json_populate_recordset", "jsonb_populate_recordset":
		jsonb := strings.HasPrefix(lname, "jsonb")
		set := strings.HasSuffix(lname, "set")
		return db.resolveJSONPopulate(lname, jsonb, set, args, alias, argScope, ptypes)
	}
	// A column-definition list is valid ONLY on a record-returning function (PG).
	if columnDefs != nil {
		return nil, nil, newError(SyntaxError,
			"a column definition list is only allowed for a record-returning function, not "+name)
	}
	switch {
	case strings.EqualFold(name, "generate_series"):
		return db.resolveGenerateSeries(args, alias, argScope, ptypes)
	case strings.EqualFold(name, "unnest"):
		return db.resolveUnnest(args, alias, argScope, ptypes)
	}
	// json/jsonb two-column SRFs (B3, json-sql-functions.md §3): jsonb_each → (key text, value
	// jsonb), jsonb_each_text → (key text, value text). The json variants (verbatim sub-text,
	// json.md §4) are a deferred 0A000 follow-on. Built on the C0 multi-column synthetic table.
	switch lname {
	case "jsonb_each":
		return db.resolveJSONEach(lname, srfJsonbEach, scalarT(scalarJsonb), args, alias, argScope, ptypes)
	case "jsonb_each_text":
		return db.resolveJSONEach(lname, srfJsonbEachText, scalarT(scalarText), args, alias, argScope, ptypes)
	case "json_each", "json_each_text":
		return nil, nil, newError(FeatureNotSupported, lname+" is not supported yet; use the jsonb variant")
	}
	// json/jsonb single-column SRFs (B2, json-sql-functions.md §3). The json `array_elements`
	// variants preserve the verbatim sub-text (json.md §4) and are a deferred 0A000 follow-on, like
	// the json accessor operators; the jsonb variants + `json_object_keys` ship here.
	switch lname {
	case "jsonb_array_elements":
		return db.resolveJSONSrf(lname, srfJsonbArrayElements, scalarT(scalarJsonb), true, args, alias, argScope, ptypes)
	case "jsonb_array_elements_text":
		return db.resolveJSONSrf(lname, srfJsonbArrayElementsText, scalarT(scalarText), true, args, alias, argScope, ptypes)
	case "jsonb_object_keys":
		return db.resolveJSONSrf(lname, srfJsonbObjectKeys, scalarT(scalarText), true, args, alias, argScope, ptypes)
	case "json_object_keys":
		return db.resolveJSONSrf(lname, srfJsonObjectKeys, scalarT(scalarText), false, args, alias, argScope, ptypes)
	case "json_array_elements", "json_array_elements_text":
		return nil, nil, newError(FeatureNotSupported, lname+" is not supported yet; use the jsonb variant")
	}
	// jsonb_path_query(jsonb, jsonpath) (P2, jsonpath.md §5.2): one `jsonb` row per item of the path's
	// evaluation sequence over the context document. A bare string literal adapts (the ctx to jsonb,
	// the path to a compiled jsonpath). STRICT in the args; a NULL ctx/path → zero rows at exec.
	if lname == "jsonb_path_query" {
		forbidden := &aggCtx{}
		ctx, path, err := resolveJsonpathArgs(argScope, lname, args, forbidden, ptypes)
		if err != nil {
			return nil, nil, err
		}
		return srfTable(lname, alias, scalarT(scalarJsonb)), &srfPlan{kind: srfJsonbPathQuery, args: []*rExpr{ctx, path}}, nil
	}
	return nil, nil, newError(UndefinedFunction, "function does not exist: "+name)
}

// resolveJSONSrf resolves a json/jsonb single-column SRF (B2, json-sql-functions.md §3): the one
// argument is a json/jsonb value (a bare string literal adapts to the expected document type). The
// synthetic column's type is fixed (`jsonb`/`text`). A NULL argument yields zero rows at exec.
func (db *engine) resolveJSONSrf(name string, kind srfKind, colTy dataType, jsonb bool, args []*exprNode, alias *string, argScope *scope, ptypes *paramTypes) (*catTable, *srfPlan, error) {
	if len(args) != 1 {
		return nil, nil, noFuncOverload(name)
	}
	want := scalarJson
	if jsonb {
		want = scalarJsonb
	}
	forbidden := &aggCtx{}
	r, t, err := resolve(argScope, *args[0], &want, forbidden, ptypes)
	if err != nil {
		return nil, nil, err
	}
	ok := t.kind == rtNull || (jsonb && t.kind == rtJsonb) || (!jsonb && t.kind == rtJson)
	if !ok {
		return nil, nil, noFuncOverload(name)
	}
	return srfTable(name, alias, colTy), &srfPlan{kind: kind, args: []*rExpr{r}}, nil
}

// resolveJSONEach resolves a json/jsonb TWO-column SRF (B3 — jsonb_each / jsonb_each_text,
// json-sql-functions.md §3): the one argument is a jsonb value (a bare string literal adapts). The
// synthetic relation has the fixed columns `key text` and `value <valueTy>` (the C0 multi-column
// synthetic table). A non-object argument → 22023 at exec; a NULL → zero rows.
func (db *engine) resolveJSONEach(name string, kind srfKind, valueTy dataType, args []*exprNode, alias *string, argScope *scope, ptypes *paramTypes) (*catTable, *srfPlan, error) {
	if len(args) != 1 {
		return nil, nil, noFuncOverload(name)
	}
	want := scalarJsonb
	forbidden := &aggCtx{}
	r, t, err := resolve(argScope, *args[0], &want, forbidden, ptypes)
	if err != nil {
		return nil, nil, err
	}
	if t.kind != rtJsonb && t.kind != rtNull {
		return nil, nil, noFuncOverload(name)
	}
	table := srfTableCols(name, alias, []srfCol{{"key", scalarT(scalarText)}, {"value", valueTy}})
	return table, &srfPlan{kind: kind, args: []*rExpr{r}}, nil
}

// resolveJSONRecord resolves a json/jsonb RECORD-returning SRF (R1 — json[b]_to_record /
// json[b]_to_recordset, json-table.md §2): the one argument is a json/jsonb document; the output
// columns come from the C0 col-def list `AS t(col type, …)` (required — else 42601). The synthetic
// table's columns are the declared types (a composite/array column type is a deferred 0A000), and
// the srfPlan carries them as recordCols so the row generator can map members → columns by name.
func (db *engine) resolveJSONRecord(name string, jsonb, set bool, args []*exprNode, alias *string, columnDefs []typeFieldDef, argScope *scope, ptypes *paramTypes) (*catTable, *srfPlan, error) {
	if len(args) != 1 {
		return nil, nil, noFuncOverload(name)
	}
	want := scalarJson
	if jsonb {
		want = scalarJsonb
	}
	forbidden := &aggCtx{}
	r, t, err := resolve(argScope, *args[0], &want, forbidden, ptypes)
	if err != nil {
		return nil, nil, err
	}
	ok := t.kind == rtNull || (jsonb && t.kind == rtJsonb) || (!jsonb && t.kind == rtJson)
	if !ok {
		return nil, nil, noFuncOverload(name)
	}
	if columnDefs == nil {
		return nil, nil, newError(SyntaxError,
			"a column definition list is required for function "+name)
	}
	columns := make([]catColumn, 0, len(columnDefs))
	for _, d := range columnDefs {
		// A composite/array column type in the col-def list is a deferred 0A000 follow-on.
		if strings.HasSuffix(d.TypeName, "[]") || db.CompositeType(d.TypeName) != nil {
			return nil, nil, newError(FeatureNotSupported,
				"a composite/array column in a record column-definition list is not supported yet")
		}
		st, decimal, varcharLen, err := resolveTypeAndTypmod(d.TypeName, d.TypeMod)
		if err != nil {
			return nil, nil, err
		}
		columns = append(columns, catColumn{Name: d.Name, Type: scalarT(st), Decimal: decimal, VarcharLen: varcharLen})
	}
	tname := name
	if alias != nil {
		tname = *alias
	}
	table := &catTable{Name: tname, Columns: columns}
	kind := srfJSONRecord
	if set {
		kind = srfJSONRecordset
	}
	return table, &srfPlan{kind: kind, args: []*rExpr{r}, recordCols: columns}, nil
}

// resolveJSONPopulate resolves a json/jsonb POPULATE-RECORD SRF (R2 — json[b]_populate_record(set),
// json-table.md §2): the FIRST argument is a (typically NULL) value whose COMPOSITE TYPE supplies
// the output column shape; the SECOND is the json/jsonb document. Reuses the R1 row machinery
// (srfJSONRecord(set)) — only the column source differs (a composite type vs a col-def list). A
// non-composite first argument → 42804; an anonymous record base → 0A000.
func (db *engine) resolveJSONPopulate(name string, jsonb, set bool, args []*exprNode, alias *string, argScope *scope, ptypes *paramTypes) (*catTable, *srfPlan, error) {
	if len(args) != 2 {
		return nil, nil, noFuncOverload(name)
	}
	forbidden := &aggCtx{}
	// The base argument's COMPOSITE type fixes the columns (its value is unused — usually NULL).
	_, bt, err := resolve(argScope, *args[0], nil, forbidden, ptypes)
	if err != nil {
		return nil, nil, err
	}
	if bt.kind != rtComposite {
		return nil, nil, newError(DatatypeMismatch,
			"the first argument of "+name+" must be a composite type")
	}
	// A named composite supplies the columns; an anonymous record base is 0A000.
	if !bt.comp.named {
		return nil, nil, newError(FeatureNotSupported, "an anonymous record base is not supported yet")
	}
	ctype := db.CompositeType(bt.comp.name)
	if ctype == nil {
		return nil, nil, newError(UndefinedObject, "composite type no longer exists")
	}
	columns := make([]catColumn, 0, len(ctype.Fields))
	for _, f := range ctype.Fields {
		columns = append(columns, catColumn{Name: f.Name, Type: f.Type, Decimal: f.Decimal, VarcharLen: f.VarcharLen})
	}
	// The SECOND argument is the json/jsonb document.
	want := scalarJson
	if jsonb {
		want = scalarJsonb
	}
	r, dt, err := resolve(argScope, *args[1], &want, forbidden, ptypes)
	if err != nil {
		return nil, nil, err
	}
	ok := dt.kind == rtNull || (jsonb && dt.kind == rtJsonb) || (!jsonb && dt.kind == rtJson)
	if !ok {
		return nil, nil, noFuncOverload(name)
	}
	tname := name
	if alias != nil {
		tname = *alias
	}
	table := &catTable{Name: tname, Columns: columns}
	kind := srfJSONRecord
	if set {
		kind = srfJSONRecordset
	}
	// The SRF arg is the json DOCUMENT (the base value is unused); reuse the R1 row generator.
	return table, &srfPlan{kind: kind, args: []*rExpr{r}, recordCols: columns}, nil
}

// resolveJSONTable resolves a JSON_TABLE(ctx, path COLUMNS (…)) source (T1, json-table.md §3) → its
// synthetic relation (the flattened columns), the `[ctx]` arg, and the resolved jtPlan. The ctx /
// root path see only params + the lateral prefix (never sibling columns of THIS relation) — an
// empty-local-rels scope chained to `parent`, exactly like an SRF (grammar.md §44).
func (db *engine) resolveJSONTable(jt *jsonTable, alias *string, parent *scope, ctes []*cteBinding, ptypes *paramTypes) (*catTable, *srfPlan, error) {
	argScope := &scope{rels: nil, parent: parent, catalog: db, allowSubquery: true, ctes: ctes}
	forbidden := &aggCtx{}
	// The context item (json / jsonb / text, coerced to a jsonb document at eval).
	jsonbHint := scalarJsonb
	rctx, ctxTy, err := resolve(argScope, *jt.Ctx, &jsonbHint, forbidden, ptypes)
	if err != nil {
		return nil, nil, err
	}
	switch ctxTy.kind {
	case rtJsonb, rtJson, rtText, rtNull:
		// ok
	default:
		return nil, nil, newError(DatatypeMismatch,
			fmt.Sprintf("the context item of JSON_TABLE must be json/jsonb/text, not %s", rtName(ctxTy)))
	}
	// The root path — a constant jsonpath (a string literal compiles to a reConstJsonPath node).
	pathHint := scalarJsonPath
	rpath, pathTy, err := resolve(argScope, *jt.Path, &pathHint, forbidden, ptypes)
	if err != nil {
		return nil, nil, err
	}
	if pathTy.kind != rtJsonPath {
		return nil, nil, newError(DatatypeMismatch, "the path of JSON_TABLE must be a constant jsonpath")
	}
	if rpath.kind != reConstJsonPath {
		return nil, nil, newError(FeatureNotSupported, "a non-constant JSON_TABLE path is not supported")
	}
	rootPath := rpath.cText
	var outColumns []catColumn
	columns, err := db.resolveJtColumns(jt.Columns, &outColumns)
	if err != nil {
		return nil, nil, err
	}
	tname := "json_table"
	if alias != nil {
		tname = *alias
	}
	table := &catTable{Name: tname, Columns: outColumns}
	return table, &srfPlan{
		kind:      srfJsonTable,
		args:      []*rExpr{rctx},
		jsonTable: &jtPlan{rootPath: rootPath, width: len(outColumns), columns: columns},
	}, nil
}

// resolveJtColumns recursively resolves a JSON_TABLE COLUMNS tree, flattening the leaf columns into
// `outColumns` (pre-order, declaration order) and assigning each its flat output index.
func (db *engine) resolveJtColumns(cols []jtColumn, outColumns *[]catColumn) ([]jtCol, error) {
	resolved := make([]jtCol, 0, len(cols))
	for _, col := range cols {
		switch c := col.(type) {
		case *jtColumnOrdinality:
			idx := len(*outColumns)
			*outColumns = append(*outColumns, newJtColumn(c.Name, scalarInt32, nil))
			resolved = append(resolved, &jtColOrdinality{idx: idx})
		case *jtColumnRegular:
			if c.Array {
				return nil, newError(FeatureNotSupported, "an array JSON_TABLE column is not supported yet")
			}
			st, decimal, err := jtScalarType(db, c.TypeName)
			if err != nil {
				return nil, err
			}
			if !c.KeepQuotes {
				return nil, newError(FeatureNotSupported, "JSON_TABLE OMIT QUOTES is not supported yet")
			}
			query := st == scalarJson || st == scalarJsonb
			if !query && c.Wrapper != jWWithout {
				return nil, newError(FeatureNotSupported, "a WRAPPER on a scalar JSON_TABLE column is not supported yet")
			}
			compiled, err := jtCompilePath(c.Path, c.Name)
			if err != nil {
				return nil, err
			}
			idx := len(*outColumns)
			*outColumns = append(*outColumns, newJtColumn(c.Name, st, decimal))
			resolved = append(resolved, &jtColRegular{
				idx:       idx,
				returning: st,
				decimal:   decimal,
				path:      compiled,
				query:     query,
				wrapper:   c.Wrapper,
				onEmpty:   jtBehavior(c.OnEmpty, jOBNull),
				onError:   jtBehavior(c.OnError, jOBNull),
			})
		case *jtColumnExists:
			st, _, err := jtScalarType(db, c.TypeName)
			if err != nil {
				return nil, err
			}
			compiled, err := jtCompilePath(c.Path, c.Name)
			if err != nil {
				return nil, err
			}
			idx := len(*outColumns)
			*outColumns = append(*outColumns, newJtColumn(c.Name, st, nil))
			resolved = append(resolved, &jtColExists{
				idx:       idx,
				returning: st,
				path:      compiled,
				onError:   jtBehavior(c.OnError, jOBFalse),
			})
		case *jtColumnNested:
			compiled, err := compile(c.Path)
			if err != nil {
				return nil, err
			}
			nested, err := db.resolveJtColumns(c.Columns, outColumns)
			if err != nil {
				return nil, err
			}
			resolved = append(resolved, &jtColNested{path: compiled.Render(), columns: nested})
		default:
			panic("resolveJtColumns: unknown JtColumn kind")
		}
	}
	return resolved, nil
}

// resolveGenerateSeries resolves generate_series(start, stop[, step]) (spec/design/functions.md
// §10): 2 or 3 integer args (a wrong arity/type → 42883). The produced column is typed at the
// PROMOTED integer type of the args (PG); a NULL-typed arg contributes no width. All-NULL defaults
// i64.
func (db *engine) resolveGenerateSeries(args []*exprNode, alias *string, argScope *scope, ptypes *paramTypes) (*catTable, *srfPlan, error) {
	if len(args) != 2 && len(args) != 3 {
		return nil, nil, noFuncOverload("generate_series")
	}
	int64Ctx := scalarInt64
	forbidden := &aggCtx{}
	rargs := make([]*rExpr, 0, len(args))
	var result scalarType
	haveResult := false
	for _, a := range args {
		r, t, err := resolve(argScope, *a, &int64Ctx, forbidden, ptypes)
		if err != nil {
			return nil, nil, err
		}
		switch t.kind {
		case rtInt:
			if !haveResult || t.intTy.Rank() > result.Rank() {
				result = t.intTy
				haveResult = true
			}
		case rtNull:
			// An untyped NULL/param adapts and contributes no width.
		default:
			return nil, nil, noFuncOverload("generate_series")
		}
		rargs = append(rargs, r)
	}
	if !haveResult {
		result = scalarInt64
	}
	return srfTable("generate_series", alias, scalarT(result)), &srfPlan{kind: srfGenerateSeries, args: rargs}, nil
}

// resolveUnnest resolves unnest(anyarray) (spec/design/array-functions.md §9, §13): the single
// argument must be an array (binding ELEM := its element type, the produced column's type), else
// 42883 (a non-array, e.g. unnest(5)). A bare untyped NULL argument leaves ELEM undeterminable →
// 42P18 (jed's polymorphic posture, like array_append(NULL, NULL)); a typed NULL array
// (NULL::i32[]) resolves and yields zero rows at exec. ELEM may be a scalar OR a composite (AF7 —
// unnest(composite[])): the synthetic column is typed at the bound element type directly
// (typeFromResolved), so a composite array produces composite rows (an anonymous-composite element
// has no catalog name → 0A000, not reachable from a typed array).
func (db *engine) resolveUnnest(args []*exprNode, alias *string, argScope *scope, ptypes *paramTypes) (*catTable, *srfPlan, error) {
	if len(args) != 1 {
		return nil, nil, noFuncOverload("unnest")
	}
	forbidden := &aggCtx{}
	r, t, err := resolve(argScope, *args[0], nil, forbidden, ptypes)
	if err != nil {
		return nil, nil, err
	}
	switch t.kind {
	case rtArray:
		elemTy, err := typeFromResolved(*t.elem)
		if err != nil {
			return nil, nil, err
		}
		return srfTable("unnest", alias, elemTy), &srfPlan{kind: srfUnnest, args: []*rExpr{r}}, nil
	case rtNull:
		return nil, nil, indeterminatePoly()
	default:
		return nil, nil, noFuncOverload("unnest")
	}
}

// srfTable builds a set-returning function's SYNTHETIC one-column relation (spec/design/functions.md
// §10). The table's Name is the function name (the un-aliased label fallback); the lone column's
// NAME follows PostgreSQL's single-column function-alias rule — the table alias when one is given,
// else the function name — and its TYPE is colTy (the promoted integer for generate_series, the
// bound element type for unnest).
func srfTable(funcName string, alias *string, colTy dataType) *catTable {
	colName := funcName
	if alias != nil {
		colName = *alias
	}
	return &catTable{
		Name:    funcName,
		Columns: []catColumn{{Name: colName, Type: colTy}},
	}
}

// srfCol is one fixed column of a multi-column SRF synthetic table (its name + type).
type srfCol struct {
	name string
	ty   dataType
}

// srfTableCols builds a MULTI-COLUMN synthetic table for a set-returning function (C0,
// json-table.md §1) — the generalization of srfTable to N named/typed columns. The column NAMES are
// fixed by the function (e.g. jsonb_each → key, value); the FROM alias renames the RELATION (the
// table Name), not its columns. Used by json[b]_each[_text] (and, with a col-def list, the record
// functions).
func srfTableCols(funcName string, alias *string, cols []srfCol) *catTable {
	name := funcName
	if alias != nil {
		name = *alias
	}
	columns := make([]catColumn, len(cols))
	for i, c := range cols {
		columns[i] = catColumn{Name: c.name, Type: c.ty}
	}
	return &catTable{Name: name, Columns: columns}
}

// srfKindName is the catalog name of a json two-column SRF, for its non-object error message.
func srfKindName(kind srfKind) string {
	switch kind {
	case srfJsonbEach:
		return "jsonb_each"
	case srfJsonbEachText:
		return "jsonb_each_text"
	default:
		panic("srfKindName is only for the json two-column SRFs")
	}
}

// catalogRelKind classifies a relation name as a built-in catalog relation (introspection.md §5):
// jed_tables / jed_columns, case-insensitively (identifier resolution folds case; grammar.md §3
// leaves no quoted escape). Built-in names resolve in every database's relation namespace, checked
// AFTER a statement-local CTE (a CTE shadows a catalog relation — PG-matching, oracle-checked) and
// BEFORE the user catalog (post-I0 the two can never collide; for a pre-reservation legacy file
// the built-in wins and the user relation is unreachable by name — §5).
func catalogRelKind(name string) (srfKind, bool) {
	switch strings.ToLower(name) {
	case "jed_tables":
		return srfJedTables, true
	case "jed_columns":
		return srfJedColumns, true
	case "jed_indexes":
		return srfJedIndexes, true
	case "jed_constraints":
		return srfJedConstraints, true
	}
	return 0, false
}

// indexMethodName is the access-method name rendered by jed_indexes.method (introspection.md §5.1):
// the PostgreSQL amname spelling of the index kind.
func indexMethodName(kind indexKind) string {
	switch kind {
	case indexGin:
		return "gin"
	case indexGist:
		return "gist"
	default:
		return "btree"
	}
}

// isCatalogRelName reports whether name is a built-in catalog relation (jed_tables / jed_columns).
// The write paths use it to reject a catalog relation as a mutation/DDL target (42809 — a catalog
// relation is read-only, introspection.md §5); the privilege gate uses it so a built-in is
// SELECT-gated exactly like a user table under an explicit-grant session envelope.
func isCatalogRelName(name string) bool { _, ok := catalogRelKind(name); return ok }

// checkCatalogRelWrite rejects a mutation target (INSERT / UPDATE / DELETE / CREATE INDEX ON)
// naming a built-in catalog relation: 42809 wrong_object_type, `cannot modify system relation`
// (introspection.md §5 — the relations are read-only computed views of the catalog). Checked by
// NAME, before qualifier validation: the built-in resolves in every database's namespace, so the
// rejection is scope-independent.
func checkCatalogRelWrite(name string) error {
	if isCatalogRelName(name) {
		return newError(WrongObjectType,
			`cannot modify system relation "`+strings.ToLower(name)+`"`)
	}
	return nil
}

// catalogRelTable builds the FIXED synthetic schema of a catalog relation (introspection.md §5).
// Unlike an SRF's single-column alias rule, a FROM alias renames the RELATION only — the column
// names are part of the introspection surface. Growth is by ADDING columns (consumers select by
// name, not position — §5).
func catalogRelTable(kind srfKind) *catTable {
	textArr := arrayT(scalarT(scalarText)) // a text[] member-list column (introspection.md §5.1)
	switch kind {
	case srfJedTables:
		return &catTable{Name: "jed_tables", Columns: []catColumn{
			{Name: "name", Type: scalarT(scalarText), NotNull: true},
		}}
	case srfJedColumns:
		return &catTable{Name: "jed_columns", Columns: []catColumn{
			{Name: "table_name", Type: scalarT(scalarText), NotNull: true},
			{Name: "name", Type: scalarT(scalarText), NotNull: true},
			{Name: "ordinal", Type: scalarT(scalarInt32), NotNull: true},
			{Name: "type", Type: scalarT(scalarText), NotNull: true},
			{Name: "not_null", Type: scalarT(scalarBool), NotNull: true},
			{Name: "pk_ordinal", Type: scalarT(scalarInt32)},
		}}
	case srfJedIndexes:
		return &catTable{Name: "jed_indexes", Columns: []catColumn{
			{Name: "name", Type: scalarT(scalarText), NotNull: true},
			{Name: "table_name", Type: scalarT(scalarText), NotNull: true},
			{Name: "columns", Type: textArr, NotNull: true},
			{Name: "is_unique", Type: scalarT(scalarBool), NotNull: true},
			{Name: "method", Type: scalarT(scalarText), NotNull: true},
		}}
	default: // srfJedConstraints
		return &catTable{Name: "jed_constraints", Columns: []catColumn{
			{Name: "name", Type: scalarT(scalarText), NotNull: true},
			{Name: "table_name", Type: scalarT(scalarText), NotNull: true},
			{Name: "type", Type: scalarT(scalarText), NotNull: true},
			{Name: "columns", Type: textArr},
			{Name: "expression", Type: scalarT(scalarText)},
			{Name: "ref_table", Type: scalarT(scalarText)},
			{Name: "ref_columns", Type: textArr},
		}}
	}
}

// resolveCatalogScope validates a catalog relation's database qualifier and returns the scope
// string snapForScope resolves at exec (introspection.md §5): nil (unqualified) ⇒ "main" (the
// implicit scope); "main"/"temp" pass; any other qualifier must name a host attachment (else
// 42P01, the checkTableQualifier wording). Unlike a user table there is no per-table existence
// half — the relation exists in EVERY valid scope, so only the scope itself is validated.
func (db *engine) resolveCatalogScope(qualifier *string) (string, error) {
	if qualifier == nil {
		return "main", nil
	}
	q := strings.ToLower(*qualifier)
	if q == "main" || q == "temp" {
		return q, nil
	}
	if db.attachReadSnap(q) == nil {
		return "", newError(UndefinedTable, `database "`+*qualifier+`" is not attached`)
	}
	return q, nil
}

// catalogTypeText renders a column's declared type in the CANONICAL introspection form
// (introspection.md §5): the scalar's canonical name with its typmod applied at the leaf
// (varchar(10), decimal(8,2)), a composite's name as created, a range's canonical id (i32range,
// numrange, …), and `[]` appended for an array (the typmod applies to the element: varchar(5)[]).
// This text is a compatibility surface the moment it ships — pinned by the corpus.
func catalogTypeText(ty dataType, dec *decimalTypmod, vlen *uint32) string {
	if ty.Array != nil {
		return catalogTypeText(*ty.Array, dec, vlen) + "[]"
	}
	if ty.Range != nil {
		desc, _ := rangeForElement(ty.Range.ScalarTy())
		return desc.ID
	}
	if ty.Comp != nil {
		return ty.Comp.Name
	}
	if ty.Scalar == scalarText && vlen != nil {
		return fmt.Sprintf("varchar(%d)", *vlen)
	}
	if ty.Scalar == scalarDecimal && dec != nil {
		return fmt.Sprintf("decimal(%d,%d)", dec.Precision, dec.Scale)
	}
	return ty.Scalar.CanonicalName()
}

// jedTablesRows generates the rows of the jed_tables catalog relation (introspection.md §5): one
// row per USER table of the scope's pinned catalog snapshot — the canonical (CREATE TABLE-spelled)
// name — in ascending lowercased-name order (deterministic, no map-iteration leak; the multiset is
// the contract, order without ORDER BY stays unspecified — CLAUDE.md §8). Derived entirely from
// the resident catalog: zero page_read / storage_row_read; each produced row charges one
// generated_row AT THE SOURCE, guarded so a max_cost ceiling aborts deterministically (§13).
func (db *engine) jedTablesRows(sp *srfPlan, m *costMeter) ([]storedRow, error) {
	snap := db.snapForScope(sp.introspectScope)
	if snap == nil {
		// The attachment was valid at plan time but is gone at exec (a detached-then-reused plan).
		return nil, newError(UndefinedTable, `database "`+sp.introspectScope+`" is not attached`)
	}
	var out []storedRow
	for _, t := range snap.tablesSorted() {
		if err := m.Guard(); err != nil {
			return nil, err
		}
		m.Charge(costs.GeneratedRow)
		out = append(out, storedRow{TextValue(t.Name)})
	}
	return out, nil
}

// jedColumnsRows generates the rows of the jed_columns catalog relation (introspection.md §5): one
// row per column of every user table of the scope's snapshot, in (lowercased table name, ordinal)
// order. ordinal is 1-based CREATE TABLE order; type is the canonical type text (catalogTypeText);
// not_null covers a declared NOT NULL and PRIMARY KEY membership; pk_ordinal is the 1-based
// position in the PRIMARY KEY in KEY order (which may differ from declaration order —
// constraints.md §3), NULL for a non-member. Cost mirrors jedTablesRows.
func (db *engine) jedColumnsRows(sp *srfPlan, m *costMeter) ([]storedRow, error) {
	snap := db.snapForScope(sp.introspectScope)
	if snap == nil {
		return nil, newError(UndefinedTable, `database "`+sp.introspectScope+`" is not attached`)
	}
	var out []storedRow
	for _, t := range snap.tablesSorted() {
		for i, c := range t.Columns {
			if err := m.Guard(); err != nil {
				return nil, err
			}
			m.Charge(costs.GeneratedRow)
			pkOrdinal := NullValue()
			for k, ord := range t.PK {
				if ord == i {
					pkOrdinal = IntValue(int64(k + 1))
					break
				}
			}
			out = append(out, storedRow{
				TextValue(t.Name),
				TextValue(c.Name),
				IntValue(int64(i + 1)),
				TextValue(catalogTypeText(c.Type, c.Decimal, c.VarcharLen)),
				BoolValue(c.NotNull || c.PrimaryKey),
				pkOrdinal,
			})
		}
	}
	return out, nil
}

// jedIndexesRows generates the rows of the jed_indexes catalog relation (introspection.md §5.1):
// one row per secondary index of every user table of the scope's snapshot, in (lowercased table
// name, then the catalog's ascending index-name order) order. columns is the text[] of indexed
// column names in index-key order (duplicates included); is_unique the catalog flag; method the
// access-method name (btree/gin/gist). Cost mirrors jedTablesRows.
func (db *engine) jedIndexesRows(sp *srfPlan, m *costMeter) ([]storedRow, error) {
	snap := db.snapForScope(sp.introspectScope)
	if snap == nil {
		return nil, newError(UndefinedTable, `database "`+sp.introspectScope+`" is not attached`)
	}
	var out []storedRow
	for _, t := range snap.tablesSorted() {
		for _, idx := range t.Indexes {
			if err := m.Guard(); err != nil {
				return nil, err
			}
			m.Charge(costs.GeneratedRow)
			cols := make([]Value, len(idx.Columns))
			for j, ord := range idx.Columns {
				cols[j] = TextValue(t.Columns[ord].Name)
			}
			out = append(out, storedRow{
				TextValue(idx.Name),
				TextValue(t.Name),
				ArrayValue(cols),
				BoolValue(idx.Unique),
				TextValue(indexMethodName(idx.Kind)),
			})
		}
	}
	return out, nil
}

// jedConstraintsRows generates the rows of the jed_constraints catalog relation (introspection.md
// §5.1): one row per CHECK / UNIQUE / FK / EXCLUDE constraint of every user table of the scope's
// snapshot, in (lowercased table name, then a fixed KIND order — check, unique, foreign_key,
// exclude — each already held in ascending lowercased-name order). PRIMARY KEY / NOT NULL are
// deliberately absent (they own no named object and are described by jed_columns). A UNIQUE
// constraint IS its backing unique b-tree index (constraints.md §5), so type='unique' lists every
// unique index; expression is the persisted canonical CHECK text (constraints.md §4.5). Cost
// mirrors jedTablesRows.
func (db *engine) jedConstraintsRows(sp *srfPlan, m *costMeter) ([]storedRow, error) {
	snap := db.snapForScope(sp.introspectScope)
	if snap == nil {
		return nil, newError(UndefinedTable, `database "`+sp.introspectScope+`" is not attached`)
	}
	textArr := func(names []string) Value {
		vals := make([]Value, len(names))
		for i, n := range names {
			vals[i] = TextValue(n)
		}
		return ArrayValue(vals)
	}
	var out []storedRow
	for _, t := range snap.tablesSorted() {
		// CHECK: name / table / 'check' / NULL columns / expression text / NULL ref_*.
		for _, ck := range t.Checks {
			if err := m.Guard(); err != nil {
				return nil, err
			}
			m.Charge(costs.GeneratedRow)
			out = append(out, storedRow{
				TextValue(ck.Name),
				TextValue(t.Name),
				TextValue("check"),
				NullValue(),
				TextValue(ck.ExprText),
				NullValue(),
				NullValue(),
			})
		}
		// UNIQUE: every unique b-tree index (a UNIQUE constraint IS its unique index).
		for _, idx := range t.Indexes {
			if !idx.Unique {
				continue
			}
			if err := m.Guard(); err != nil {
				return nil, err
			}
			m.Charge(costs.GeneratedRow)
			cols := make([]string, len(idx.Columns))
			for j, ord := range idx.Columns {
				cols[j] = t.Columns[ord].Name
			}
			out = append(out, storedRow{
				TextValue(idx.Name),
				TextValue(t.Name),
				TextValue("unique"),
				textArr(cols),
				NullValue(),
				NullValue(),
				NullValue(),
			})
		}
		// FOREIGN KEY: local columns / referenced (parent) table + columns (rendered from the
		// parent's canonical names — the parent always exists, it cannot be dropped while referenced,
		// constraints.md §6.10).
		for _, fk := range t.ForeignKeys {
			if err := m.Guard(); err != nil {
				return nil, err
			}
			m.Charge(costs.GeneratedRow)
			local := make([]string, len(fk.Columns))
			for j, ord := range fk.Columns {
				local[j] = t.Columns[ord].Name
			}
			parent, _ := snap.table(fk.RefTable)
			refTable := fk.RefTable
			if parent != nil {
				refTable = parent.Name
			}
			refCols := make([]string, len(fk.RefColumns))
			for j, ord := range fk.RefColumns {
				if parent != nil && ord < len(parent.Columns) {
					refCols[j] = parent.Columns[ord].Name
				}
			}
			out = append(out, storedRow{
				TextValue(fk.Name),
				TextValue(t.Name),
				TextValue("foreign_key"),
				textArr(local),
				NullValue(),
				TextValue(refTable),
				textArr(refCols),
			})
		}
		// EXCLUDE: the excluded columns in element order (the &&/= operators are a deferred column
		// addition — introspection.md §5.1).
		for _, exc := range t.Exclusions {
			if err := m.Guard(); err != nil {
				return nil, err
			}
			m.Charge(costs.GeneratedRow)
			cols := make([]string, len(exc.Elements))
			for j, el := range exc.Elements {
				cols[j] = t.Columns[el.Column].Name
			}
			out = append(out, storedRow{
				TextValue(exc.Name),
				TextValue(t.Name),
				TextValue("exclude"),
				textArr(cols),
				NullValue(),
				NullValue(),
				NullValue(),
			})
		}
	}
	return out, nil
}

// generateSeriesRows generates the rows of a generate_series(start, stop[, step]) FROM-clause
// source (spec/design/functions.md §10), as one-column rows. The args evaluate ONCE against the
// outer environment with no local row (non-LATERAL). PostgreSQL semantics: any NULL arg → zero
// rows; a step of zero → 22023; start > stop with a positive step (or the reverse) → zero rows;
// an i64 overflow while stepping STOPS the series cleanly (no trap). Each generated element
// charges one generated_row AT THE SOURCE, guarded so a max_cost ceiling aborts a runaway series
// (54P01) mid-generation before the whole thing materializes (CLAUDE.md §13).
func (db *engine) generateSeriesRows(sp *srfPlan, env *evalEnv, m *costMeter) ([]storedRow, error) {
	evalInt := func(e *rExpr) (int64, bool, error) {
		v, err := e.eval(nil, env, m)
		if err != nil {
			return 0, false, err
		}
		switch v.Kind {
		case ValInt:
			return v.Int, true, nil
		case ValNull:
			return 0, false, nil
		default:
			panic("the resolver restricts generate_series args to integers")
		}
	}
	start, okStart, err := evalInt(sp.args[0])
	if err != nil {
		return nil, err
	}
	stop, okStop, err := evalInt(sp.args[1])
	if err != nil {
		return nil, err
	}
	step, okStep := int64(1), true
	if len(sp.args) == 3 {
		step, okStep, err = evalInt(sp.args[2])
		if err != nil {
			return nil, err
		}
	}
	// Any NULL argument yields zero rows (PG).
	if !okStart || !okStop || !okStep {
		return nil, nil
	}
	if step == 0 {
		return nil, newError(InvalidParameterValue, "step size cannot be equal to zero")
	}
	var out []storedRow
	cur := start
	for {
		inRange := false
		if step > 0 {
			inRange = cur <= stop
		} else {
			inRange = cur >= stop
		}
		if !inRange {
			break
		}
		if err := m.Guard(); err != nil {
			return nil, err
		}
		m.Charge(costs.GeneratedRow)
		out = append(out, storedRow{IntValue(cur)})
		// i64 overflow while stepping ends the series cleanly, matching PostgreSQL.
		next := cur + step
		if (step > 0 && next < cur) || (step < 0 && next > cur) {
			break
		}
		cur = next
	}
	return out, nil
}

// jsonSrfRows generates the rows of a json/jsonb single-column SRF (B2, json-sql-functions.md §3). A
// NULL argument yields zero rows (empty_on_null). array_elements[_text] over a non-array, or
// object_keys over a non-object, is 22023. Each produced row charges one generated_row.
func (db *engine) jsonSrfRows(sp *srfPlan, env *evalEnv, m *costMeter) ([]storedRow, error) {
	arg, err := sp.args[0].eval(nil, env, m)
	if err != nil {
		return nil, err
	}
	if arg.Kind == ValNull {
		return nil, nil
	}
	node, err := jsonArgNode(arg)
	if err != nil {
		return nil, err
	}
	var out []storedRow
	switch sp.kind {
	case srfJsonbArrayElements, srfJsonbArrayElementsText:
		if node.Kind != JArray {
			return nil, newError(InvalidParameterValue, "cannot extract elements from a scalar")
		}
		for i := range node.Arr {
			if err := m.Guard(); err != nil {
				return nil, err
			}
			m.Charge(costs.GeneratedRow)
			e := node.Arr[i]
			var v Value
			if sp.kind == srfJsonbArrayElementsText {
				if s, ok := jsonNodeToText(&e); ok {
					v = TextValue(s)
				} else {
					v = NullValue()
				}
			} else {
				v = JsonbValue(e)
			}
			out = append(out, storedRow{v})
		}
	case srfJsonbObjectKeys, srfJsonObjectKeys:
		if node.Kind != JObject {
			return nil, newError(InvalidParameterValue, "cannot call jsonb_object_keys on a non-object")
		}
		for i := range node.Obj {
			if err := m.Guard(); err != nil {
				return nil, err
			}
			m.Charge(costs.GeneratedRow)
			out = append(out, storedRow{TextValue(node.Obj[i].Key)})
		}
	case srfJsonbEach, srfJsonbEachText:
		if node.Kind != JObject {
			return nil, newError(InvalidParameterValue, "cannot call "+srfKindName(sp.kind)+" on a non-object")
		}
		for i := range node.Obj {
			if err := m.Guard(); err != nil {
				return nil, err
			}
			m.Charge(costs.GeneratedRow)
			// (key text, value): jsonb_each keeps the value node; _text renders ->>-style
			// (a string member's raw content, a JSON null → SQL NULL, else canonical).
			var value Value
			if sp.kind == srfJsonbEachText {
				if s, ok := jsonNodeToText(&node.Obj[i].Val); ok {
					value = TextValue(s)
				} else {
					value = NullValue()
				}
			} else {
				value = JsonbValue(node.Obj[i].Val)
			}
			out = append(out, storedRow{TextValue(node.Obj[i].Key), value})
		}
	case srfJSONRecord:
		// json[b]_to_record (R1): one record row, mapping members → the col-def columns by name.
		if err := m.Guard(); err != nil {
			return nil, err
		}
		m.Charge(costs.GeneratedRow)
		row, err := jsonRecordRow(&node, sp.recordCols, env, m)
		if err != nil {
			return nil, err
		}
		out = append(out, row)
	case srfJSONRecordset:
		// json[b]_to_recordset (R1): one record row per element of a top-level array (preserving
		// order); a non-array document → 22023.
		if node.Kind != JArray {
			return nil, newError(InvalidParameterValue, "cannot call json_to_recordset on a non-array")
		}
		for i := range node.Arr {
			if err := m.Guard(); err != nil {
				return nil, err
			}
			m.Charge(costs.GeneratedRow)
			row, err := jsonRecordRow(&node.Arr[i], sp.recordCols, env, m)
			if err != nil {
				return nil, err
			}
			out = append(out, row)
		}
	case srfJsonbPathQuery:
		// jsonb_path_query (P2, jsonpath.md §5.2): one jsonb row per path-evaluation-sequence item.
		// The context node is already parsed above (`node`); evaluate the path (a NULL path → zero
		// rows). The resolver restricts the path argument to jsonpath (its canonical text in Str).
		path, err := sp.args[1].eval(nil, env, m)
		if err != nil {
			return nil, err
		}
		if path.Kind == ValNull {
			return nil, nil
		}
		compiled, err := compile(path.str())
		if err != nil {
			return nil, err
		}
		seq, err := compiled.Eval(node)
		if err != nil {
			return nil, err
		}
		for i := range seq {
			if err := m.Guard(); err != nil {
				return nil, err
			}
			m.Charge(costs.GeneratedRow)
			out = append(out, storedRow{JsonbValue(seq[i])})
		}
	default:
		panic("jsonSrfRows only handles the json SRF kinds")
	}
	return out, nil
}

// jsonRecordRow builds one output row for json[b]_to_record(set) (R1): map each declared column to
// the JSON object's member of that name, coercing it to the column type. A missing member or a JSON
// null → SQL NULL; a non-object node → 22023. (json-table.md §2)
func jsonRecordRow(node *JsonNode, cols []catColumn, env *evalEnv, m *costMeter) (storedRow, error) {
	if node.Kind != JObject {
		return nil, newError(InvalidParameterValue, "argument of json_to_record must be a JSON object")
	}
	row := make(storedRow, 0, len(cols))
	for ci := range cols {
		col := &cols[ci]
		var member *JsonNode
		for mi := range node.Obj {
			if node.Obj[mi].Key == col.Name {
				member = &node.Obj[mi].Val
				break
			}
		}
		// A missing member or a JSON null member → SQL NULL.
		if member == nil || member.Kind == JNull {
			row = append(row, NullValue())
			continue
		}
		v, err := coerceJSONMember(member, col.Type, col.Decimal, env, m)
		if err != nil {
			return nil, err
		}
		row = append(row, v)
	}
	return row, nil
}

// coerceJSONMember coerces a JSON member node to a record column's type (R1, the JSON_VALUE scalar
// path): a `jsonb` column embeds the node, a `json` column its canonical text, every other scalar
// coerces the node's `->>`-style text through the cast machinery (so `"42"` / `42` → an `int`
// column, etc.). A composite/array column type is a deferred 0A000.
func coerceJSONMember(node *JsonNode, colTy dataType, decimal *decimalTypmod, env *evalEnv, m *costMeter) (Value, error) {
	// A composite / array / range field type is a deferred 0A000 (only scalar / json / jsonb coerce
	// this slice). R1's col-def list rejects these at resolve; R2's composite fields can carry one.
	if _, ok := colTy.AsScalar(); !ok {
		return Value{}, newError(FeatureNotSupported, "a composite/array record column is not supported yet")
	}
	st := colTy.ScalarTy()
	switch {
	case st == scalarJsonb:
		return JsonbValue(*node), nil
	case st == scalarJson:
		return JsonValue(jsonbOut(node)), nil
	default:
		text, ok := jsonNodeToText(node)
		if !ok {
			return NullValue(), nil
		}
		rexpr, _, err := coerceStringLiteral(text, st, decimal, nil)
		if err != nil {
			return Value{}, err
		}
		return rexpr.eval(nil, env, m)
	}
}

// isSQLJSONError reports whether an error is a SQL/JSON error caught by a query function's `ON ERROR`
// clause: a data exception (class `22`). Resource / cost aborts (class `53`/`54`) propagate
// unconditionally.
func isSQLJSONError(err error) bool {
	if ee, ok := err.(*EngineError); ok {
		return strings.HasPrefix(ee.Code(), "22")
	}
	return false
}

// applyJSONBehavior applies a constant `ON ERROR` / `ON EMPTY` behavior → a value of the RETURNING
// type. underlying is the SQL/JSON error this behavior replaces (raised verbatim by `ERROR`).
func applyJSONBehavior(behavior jsonOnBehavior, underlying error, returning scalarType, env *evalEnv, m *costMeter) (Value, error) {
	switch behavior {
	case jOBError:
		return Value{}, underlying
	case jOBNull:
		return NullValue(), nil
	case jOBTrue:
		return BoolValue(true), nil
	case jOBFalse:
		return BoolValue(false), nil
	case jOBUnknown:
		return NullValue(), nil
	case jOBEmptyArray:
		return jsonNodeAsReturning(JsonNode{Kind: JArray}, returning, env, m)
	default: // JOBEmptyObject
		return jsonNodeAsReturning(JsonNode{Kind: JObject}, returning, env, m)
	}
}

// jsonNodeAsReturning renders a json result node as the RETURNING type: `jsonb` embeds, `json` its
// canonical text, any other scalar coerces the node's `->>`-style text through the cast machinery.
func jsonNodeAsReturning(node JsonNode, returning scalarType, env *evalEnv, m *costMeter) (Value, error) {
	return coerceJSONMember(&node, scalarT(returning), nil, env, m)
}

// evalJSONSqlResult applies the SQL/JSON query-function semantics (JSON_VALUE / JSON_QUERY) to an
// evaluated sequence. (JSON_EXISTS is handled inline — non-empty → true.)
func evalJSONSqlResult(kind jsonSqlKind, seq []JsonNode, returning scalarType, wrapper jsonWrapper, onEmpty, onError jsonOnBehavior, env *evalEnv, m *costMeter) (Value, error) {
	switch kind {
	case jsExists:
		return BoolValue(len(seq) > 0), nil
	case jsValue:
		if len(seq) == 0 {
			return applyJSONBehavior(onEmpty, newError(NoSqlJsonItem, "no SQL/JSON item"), returning, env, m)
		}
		if len(seq) > 1 {
			return applyJSONBehavior(onError,
				newError(MoreThanOneSqlJsonItem, "JSON path expression in JSON_VALUE should return singleton scalar item"),
				returning, env, m)
		}
		item := seq[0]
		// JSON_VALUE requires a SCALAR item (PG 2203F otherwise).
		if item.Kind == JArray || item.Kind == JObject {
			return applyJSONBehavior(onError,
				newError(SqlJsonMemberNotFound, "JSON path expression in JSON_VALUE should return singleton scalar item"),
				returning, env, m)
		}
		// Coerce the scalar to the RETURNING type (a JSON null → SQL NULL). A coercion failure is a
		// SQL/JSON error honored by ON ERROR.
		v, err := coerceJSONMember(&item, scalarT(returning), nil, env, m)
		if err != nil {
			if isSQLJSONError(err) {
				return applyJSONBehavior(onError, err, returning, env, m)
			}
			return Value{}, err
		}
		return v, nil
	default: // jsQuery
		var node JsonNode
		switch wrapper {
		case jWUnconditional:
			node = JsonNode{Kind: JArray, Arr: seq}
		case jWConditional:
			if len(seq) == 1 {
				node = seq[0]
			} else {
				node = JsonNode{Kind: JArray, Arr: seq}
			}
		default: // JWWithout
			if len(seq) == 0 {
				return applyJSONBehavior(onEmpty, newError(NoSqlJsonItem, "no SQL/JSON item"), returning, env, m)
			}
			if len(seq) > 1 {
				return applyJSONBehavior(onError,
					newError(MoreThanOneSqlJsonItem, "JSON path expression in JSON_QUERY should return singleton item without wrapper"),
					returning, env, m)
			}
			node = seq[0]
		}
		return jsonNodeAsReturning(node, returning, env, m)
	}
}

// ----------------------------------------------------------------------------------------------
// JSON_TABLE (T1, json-table.md §3)
// ----------------------------------------------------------------------------------------------

// jtAssign is a sparse assignment of a JSON_TABLE row — `(flat column index, value)` pairs;
// unassigned columns are NULL (the LEFT-OUTER / sibling-UNION fill).
type jtAssign struct {
	idx int
	v   Value
}

// jsonTableRows generates the rows of a JSON_TABLE SRF (T1, json-table.md §3) — the default-plan
// recursive expansion (parent→child LEFT OUTER, sibling NESTED paths UNIONed). A NULL ctx → zero
// rows; a structural error evaluating the root path → zero rows.
func (db *engine) jsonTableRows(sp *srfPlan, env *evalEnv, m *costMeter) ([]storedRow, error) {
	plan := sp.jsonTable
	ctx, err := sp.args[0].eval(nil, env, m)
	if err != nil {
		return nil, err
	}
	if ctx.Kind == ValNull {
		return nil, nil
	}
	node, err := jsonArgNode(ctx)
	if err != nil {
		return nil, err
	}
	// The root path → the sequence of row items (a structural error here yields no rows).
	root, err := compile(plan.rootPath)
	if err != nil {
		return nil, err
	}
	items, err := root.Eval(node)
	if err != nil {
		if isSQLJSONError(err) {
			return nil, nil
		}
		return nil, err
	}
	// Expand the column tree over the root sequence → sparse rows, then materialize.
	sparse, err := expandJtLevel(plan.columns, items, env, m)
	if err != nil {
		return nil, err
	}
	out := make([]storedRow, 0, len(sparse))
	for _, assignment := range sparse {
		if err := m.Guard(); err != nil {
			return nil, err
		}
		m.Charge(costs.GeneratedRow)
		row := make(storedRow, plan.width)
		for i := range row {
			row[i] = NullValue()
		}
		for _, a := range assignment {
			row[a.idx] = a.v
		}
		out = append(out, row)
	}
	return out, nil
}

// jtColumn builds a synthetic JSON_TABLE output column.
func newJtColumn(name string, ty scalarType, decimal *decimalTypmod) catColumn {
	return catColumn{Name: name, Type: scalarT(ty), Decimal: decimal}
}

// jtBehavior resolves an optional ON EMPTY / ON ERROR behavior to its value, falling back to def.
func jtBehavior(b *jsonOnBehavior, def jsonOnBehavior) jsonOnBehavior {
	if b != nil {
		return *b
	}
	return def
}

// jtScalarType resolves a JSON_TABLE column type name → its scalar type + decimal typmod (a composite
// → 0A000, an unknown name → 42704).
func jtScalarType(db *engine, typeName string) (scalarType, *decimalTypmod, error) {
	if st, ok := scalarTypeFromName(typeName); ok {
		return st, nil, nil
	}
	if db.CompositeType(typeName) != nil {
		return 0, nil, newError(FeatureNotSupported, "a composite JSON_TABLE column is not supported yet")
	}
	return 0, nil, newError(UndefinedObject, fmt.Sprintf("type \"%s\" does not exist", typeName))
}

// jtCompilePath compiles a JSON_TABLE column path — the explicit `PATH p`, or the default
// `$.<column_name>` — to its canonical rendered form (validating; malformed → 42601).
func jtCompilePath(path *string, name string) (string, error) {
	src := "$." + name
	if path != nil {
		src = *path
	}
	compiled, err := compile(src)
	if err != nil {
		return "", err
	}
	return compiled.Render(), nil
}

// expandJtLevel expands a JSON_TABLE COLUMNS level over a sequence of row items → the sparse rows
// (the parent→child LEFT OUTER product with sibling NESTED paths UNIONed, json-table.md §3.3).
func expandJtLevel(cols []jtCol, items []JsonNode, env *evalEnv, m *costMeter) ([][]jtAssign, error) {
	var rows [][]jtAssign
	for i := range items {
		if err := m.Guard(); err != nil {
			return nil, err
		}
		ord := int64(i + 1)
		item := &items[i]
		// This level's non-nested columns (regular / exists / ordinality).
		var local []jtAssign
		for _, col := range cols {
			switch c := col.(type) {
			case *jtColOrdinality:
				local = append(local, jtAssign{idx: c.idx, v: IntValue(ord)})
			case *jtColRegular:
				v, err := evalJtRegular(item, c, env, m)
				if err != nil {
					return nil, err
				}
				local = append(local, jtAssign{idx: c.idx, v: v})
			case *jtColExists:
				v, err := evalJtExists(item, c)
				if err != nil {
					return nil, err
				}
				local = append(local, jtAssign{idx: c.idx, v: v})
			case *jtColNested:
				// handled below
			}
		}
		// The NESTED siblings, expanded over this item (UNIONed + LEFT OUTER fill).
		var nested []*jtColNested
		for _, col := range cols {
			if n, ok := col.(*jtColNested); ok {
				nested = append(nested, n)
			}
		}
		nestedRows, err := expandJtNested(nested, item, env, m)
		if err != nil {
			return nil, err
		}
		for _, nr := range nestedRows {
			row := make([]jtAssign, 0, len(local)+len(nr))
			row = append(row, local...)
			row = append(row, nr...)
			rows = append(rows, row)
		}
	}
	return rows, nil
}

// expandJtNested expands the NESTED siblings of a level over one parent item — the default-plan
// UNION of the siblings (each row fills only its own subtree), with the parent→child LEFT OUTER fill
// (no child rows at all → one all-NULL nested row).
func expandJtNested(children []*jtColNested, item *JsonNode, env *evalEnv, m *costMeter) ([][]jtAssign, error) {
	if len(children) == 0 {
		return [][]jtAssign{nil}, nil
	}
	var union [][]jtAssign
	for _, child := range children {
		p, err := compile(child.path)
		if err != nil {
			return nil, err
		}
		childSeq, err := p.Eval(*item)
		if err != nil {
			if isSQLJSONError(err) {
				childSeq = nil
			} else {
				return nil, err
			}
		}
		rows, err := expandJtLevel(child.columns, childSeq, env, m)
		if err != nil {
			return nil, err
		}
		union = append(union, rows...)
	}
	if len(union) == 0 {
		union = append(union, nil)
	}
	return union, nil
}

// evalJtRegular evaluates a regular JSON_TABLE column over a row item — JSON_VALUE (scalar) /
// JSON_QUERY (json/jsonb) semantics, with the column's wrapper / ON EMPTY / ON ERROR.
func evalJtRegular(item *JsonNode, c *jtColRegular, env *evalEnv, m *costMeter) (Value, error) {
	p, err := compile(c.path)
	if err != nil {
		return Value{}, err
	}
	seq, err := p.Eval(*item)
	if err != nil {
		if isSQLJSONError(err) {
			return applyJSONBehavior(c.onError, err, c.returning, env, m)
		}
		return Value{}, err
	}
	kind := jsValue
	if c.query {
		kind = jsQuery
	}
	return evalJSONSqlResult(kind, seq, c.returning, c.wrapper, c.onEmpty, c.onError, env, m)
}

// evalJtExists evaluates an EXISTS JSON_TABLE column over a row item — JSON_EXISTS, coerced to the
// column type (a NON-empty sequence is true; a structural error honors ON ERROR, default FALSE).
func evalJtExists(item *JsonNode, c *jtColExists) (Value, error) {
	p, err := compile(c.path)
	if err != nil {
		return Value{}, err
	}
	var exists bool
	seq, err := p.Eval(*item)
	if err != nil {
		if isSQLJSONError(err) {
			switch c.onError {
			case jOBError:
				return Value{}, err
			case jOBTrue:
				exists = true
			case jOBUnknown:
				return NullValue(), nil
			default:
				exists = false
			}
		} else {
			return Value{}, err
		}
	} else {
		exists = len(seq) > 0
	}
	// Coerce the boolean to the column type (a `boolean` column → bool; an integer column → 1/0).
	switch {
	case c.returning.IsBool():
		return BoolValue(exists), nil
	case c.returning.IsInteger():
		if exists {
			return IntValue(1), nil
		}
		return IntValue(0), nil
	default:
		return Value{}, newError(FeatureNotSupported, "an EXISTS JSON_TABLE column must be boolean or integer this slice")
	}
}

// unnestRows generates the rows of an unnest(anyarray) FROM-clause source (spec/design/array-functions.md
// §9), as one-column rows. The single array argument evaluates ONCE against the outer environment with
// no local row (non-LATERAL). PostgreSQL semantics: a NULL array yields zero rows; the empty array {}
// yields zero rows; otherwise one row per element in flattened row-major order (a multidimensional array
// flattens; a NULL element is produced as a NULL row). Each produced element charges one generated_row AT
// THE SOURCE, guarded so a max_cost ceiling aborts a runaway unnest (54P01) mid-generation, exactly like
// generate_series (CLAUDE.md §13).
func (db *engine) unnestRows(sp *srfPlan, env *evalEnv, m *costMeter) ([]storedRow, error) {
	v, err := sp.args[0].eval(nil, env, m)
	if err != nil {
		return nil, err
	}
	switch v.Kind {
	case ValNull:
		// A NULL array → zero rows (PG; the empty_on_null discipline).
		return nil, nil
	case ValArray:
		out := make([]storedRow, 0, len(v.arrayVal().Elements))
		for _, e := range v.arrayVal().Elements {
			if err := m.Guard(); err != nil {
				return nil, err
			}
			m.Charge(costs.GeneratedRow)
			out = append(out, storedRow{e})
		}
		return out, nil
	default:
		panic("the resolver restricts unnest's argument to an array")
	}
}

// rowSource is a pull-based row cursor (Volcano-style): each next() yields one row, or
// (nil, false, nil) at end of stream. The evaluation environment and the cost meter are
// threaded IN per call rather than stored as fields, so a source owns no borrow and the one
// meter is charged down a single call path with no aliasing (the discipline that keeps the
// Rust mirror free of lifetime entanglement — CLAUDE.md §2). This is the seam the streaming +
// point-lookup work (TODO Phase 6) builds on; today only scanSource exists and feeds the
// existing materialize-then-join pipeline unchanged, so results and cost are byte-identical.
type rowSource interface {
	next(env *evalEnv, m *costMeter) (storedRow, bool, error)
}

// scanSource streams a base table's rows in primary-key order. It charges the page_read block
// (one per B-tree node — spec/design/cost.md §3 "page_read") once, before the first row, then
// storage_row_read per row yielded: the same units in the same order as the inline scan loop it
// replaced. rows is the in-key-order materialization (eager today, via IterInKeyOrder; a lazy
// leaf walk later) — the charge accounting is identical either way because cost is the logical
// node/row count, not a physical leaf fetch (pager.md §5). The block fires on the first next()
// even for an empty table (nodeCount 0 ⇒ a no-op charge), so the accrued total never moves.
type scanSource struct {
	rows         []storedRow
	i            int
	nodeCount    int
	chargedBlock bool
}

func (s *scanSource) next(env *evalEnv, m *costMeter) (storedRow, bool, error) {
	// Enforce the cost ceiling before pulling the next row (CLAUDE.md §13): a runaway scan (or a
	// JOIN/correlated re-scan built on this source) stops deterministically once accrued cost
	// reaches the limit. No-op when unlimited (spec/design/cost.md §6).
	if err := m.Guard(); err != nil {
		return nil, false, err
	}
	if !s.chargedBlock {
		m.Charge(costs.PageRead * int64(s.nodeCount))
		s.chargedBlock = true
	}
	if s.i >= len(s.rows) {
		return nil, false, nil
	}
	m.Charge(costs.StorageRowRead)
	row := s.rows[s.i]
	s.i++
	return row, true, nil
}

// ---- Primary-key predicate pushdown (spec/design/cost.md §3 "bounded scan / point lookup") ----
//
// A single-table WHERE on the primary key bounds the storage-key range a scan must visit. Detection
// is two-stage: detectPKBound runs at plan time (structural — which conjuncts are PK comparisons),
// buildKeyBound at exec time (the const values, and any $N, are known only then). The bound is a
// SUPERSET of the matching keys: the whole WHERE stays the residual filter (re-applied to each
// scanned row), so the result is always correct — the bound only narrows which rows are scanned, and
// the page_read/storage_row_read drop to what it touches. The unbounded case (nil pkBound) keeps the
// full scan, so its cost never moves.

// boundTerm is one resolved `pk <op> const-source` from a WHERE AND-chain, normalized so the PK is
// the LEFT side (a `5 < pk` flips to `pk > 5`). src is the constant/parameter operand.
type boundTerm struct {
	op  binaryOp
	src *rExpr
}

// pkBoundPlan is the plan-time result of PK analysis: the PK column's storage type + the bound
// terms. The concrete key range is built per execution by buildKeyBound.
type pkBoundPlan struct {
	pkType scalarType
	terms  []boundTerm
	// coll is the key column's resolved collation when it is collated AND Full (loaded version
	// matches the file pin) — the probe encodes via this collation's UCA sort key (encoding.md
	// §2.12), seeking the same key FORM the B-tree stores (spec/design/collation.md §8). nil for a
	// C (raw-byte) key. A Skewed collated key never produces a pkBoundPlan (keyCollationCtx refuses
	// the bound — collation.md §12).
	coll *Collation
}

// scanBound is a per-relation scan bound (cost.md §3): a primary-key range, a
// secondary-index equality (spec/design/indexes.md §5), a GIN-bounded scan over an
// array column (spec/design/gin.md §6), a GiST-bounded scan, or a MERGED point-set (an
// OR / IN-list of key equalities lowered to a union of point probes — cost.md §3 "OR /
// IN-list") — exactly one field is set. The PK bound wins when several apply (it is the
// row's own key — no second tree, range-capable, strictly cheaper); the ordered-index
// equality bound wins over GIN (gin.md §6). The point-set bounds (pkSet/indexSet) are a
// LAST-RESORT access path, chosen only when no contiguous PK/index/GIN/GiST bound applies,
// so they never displace an existing plan.
type scanBound struct {
	pk       *pkBoundPlan
	index    *indexBoundPlan
	gin      *ginBoundPlan
	gist     *gistBoundPlan
	pkSet    *pkKeySetPlan
	indexSet *indexKeySetPlan
}

// needsEagerScan reports whether a bound needs the general eager materialize path (materializeRel /
// the DML scan) rather than a single-contiguous-range fast path (streaming scan, columnar project,
// vectorized aggregate, streaming sort, join top-N). True for a second-tree gather (index / GIN /
// GiST) and for a merged OR/IN point-set (pkSet / indexSet — a union of probes, cost.md §3
// "OR / IN-list"); false for a nil bound or a plain PK contiguous bound (which every fast path
// handles via a single buildKeyBound). Every single-table fast-path gate consults this so the
// point-set bounds are interpreted in exactly ONE place (materializeRel), never silently dropped to
// a full scan by a fast path that only understands `pk`. Nil-safe (a nil bound is not eager).
func (sb *scanBound) needsEagerScan() bool {
	return sb != nil && (sb.index != nil || sb.gin != nil || sb.gist != nil || sb.pkSet != nil || sb.indexSet != nil)
}

// pkKeySetPlan is the plan-time result of an OR / IN-list disjunction of primary-key
// equalities (`pk = a OR pk = b OR …`, or the equivalent `pk IN (a, b, …)` which desugars
// to that OR-chain — cost.md §3 "OR / IN-list"). srcs is the equality const-sources, one
// per disjunct, in source order (a bind param, an outer/correlated column, or a literal —
// isConstSource of the PK type). At exec time each src encodes into the PK key space; the
// resulting keys are de-duplicated and sorted, and each becomes a point probe [k, k]. The
// whole WHERE stays the residual filter (the union is a superset), so the result is
// unchanged. coll is the PK's key collation (nil for a C key), as in pkBoundPlan.
type pkKeySetPlan struct {
	pkType scalarType
	coll   *Collation
	srcs   []*rExpr
}

// indexKeySetPlan is the pkKeySetPlan analog over a leading B-tree secondary-index column
// (indexes.md §5): each distinct encoded value becomes an index point probe (prefix scan +
// per-entry row lookup), and the rows are gathered in ascending value order. tailTypes is
// the remaining key components' types (as in indexBoundPlan) — the per-entry key-suffix
// skip.
type indexKeySetPlan struct {
	nameKey   string
	colType   scalarType
	coll      *Collation
	tailTypes []scalarType
	srcs      []*rExpr
}

// gistBoundPlan is the plan-time result of GiST analysis (spec/design/gist.md §5): the chosen GiST
// index (lowest lowercased name whose range column has a `col && const` / `col @> const` conjunct),
// the descent strategy, and the column's global scope index. Like ginBoundPlan, the constant query
// operand is NOT stored (re-found in plan.filter at exec time by gistMatch). No element type is
// carried — the gather descends the resident R-tree (gist.md §4.1), whose bounds are already decoded.
type gistBoundPlan struct {
	nameKey   string
	strategy  gistStrategy
	colGlobal int
	// scalarType is the GiST-indexed column's scalar type for the scalar `=` opclass (strategy
	// gistEqual, GX2): gistBoundRows encodes the equality constant to its order-preserving key bytes
	// with it. Unused for range_ops, whose &&/@> query is a range constant the R-tree compares directly.
	scalarType scalarType
}

// ginStrategy is which array operator a GIN bound accelerates (spec/design/gin.md §6): @>
// (contains, mode ALL → posting-list intersection), && (overlaps, mode ANY → union), = ANY
// (member — `c = ANY(col)`, the single-term @> reduction: one scalar term, its lone posting list),
// or array = (equal — `col = Q`, the @>-superset gather + residual =).
type ginStrategy int

const (
	ginContains ginStrategy = iota
	ginOverlaps
	// ginMember is `c = ANY(col)`: c is a constant SCALAR (not an array); its single term is
	// gathered like a one-element @>. The query operand recovered by ginMatch is the scalar c.
	ginMember
	// ginEqual is `col = Q`: exact array equality. The query operand is the constant array Q; its
	// distinct non-NULL elements gather the SAME candidate superset as `@> Q` (equal arrays have
	// identical element multisets, so col = Q ⟹ col @> Q), and the residual = filter makes it
	// exact. Unlike ginContains, a NULL ELEMENT of Q does not empty the bound; and a Q with no
	// non-NULL element ('{}'/all-NULL) falls back to the full scan, not a provably-empty bound.
	ginEqual
)

// ginBoundPlan is the plan-time result of GIN analysis (spec/design/gin.md §6): the chosen
// GIN index (lowest lowercased name whose array column has a `col @> const` / `col && const`
// conjunct), the array ELEMENT type (for encode(term) — the term bytes), the operator
// strategy, and the column's global scope index. The constant query Q is NOT stored; it is
// re-found in plan.filter at exec time by ginMatch and evaluated there.
type ginBoundPlan struct {
	nameKey   string
	elemType  scalarType
	strategy  ginStrategy
	colGlobal int
}

// indexEqCol is one column of an index access predicate's equality prefix (indexes.md §5.1):
// the column's storage type, its key collation (nil unless it is a Full-collated text column),
// and every equality const-source bound to it. At exec time the sources must agree on one value
// (else the bound is provably empty). A collated column encodes its probe via the UCA sort key
// (encoding.md §2.12) to match the index's stored key form (collation.md §8).
type indexEqCol struct {
	colType scalarType
	coll    *Collation
	srcs    []*rExpr
}

// indexBoundPlan is the plan-time result of index analysis (indexes.md §5.1): the chosen index
// (lowest lowercased name yielding a non-empty access predicate) and the predicate — a maximal
// EQUALITY PREFIX on the leading key columns (eqCols) plus an OPTIONAL RANGE on the next column
// (rangeTerms / rangeType). At exec time buildIndexBound turns these into a concrete index-key
// range: the equality prefix bytes P = concatenated present slots, then the range (if any)
// intersected relative to P. suffixTypes are the types of the index columns AFTER the equality
// prefix (columns[len(eqCols):]) — the range column (if any) plus every trailing column — each
// FIXED-WIDTH so an admitted entry's row-key suffix is recovered by width-skipping them past P.
type indexBoundPlan struct {
	nameKey     string // the index store's key — the lowercased index name
	eqCols      []indexEqCol
	rangeType   scalarType  // the range column's type (meaningful iff rangeTerms != nil)
	rangeTerms  []boundTerm // range conjuncts on the column after the equality prefix (nil ⇒ none)
	suffixTypes []scalarType
}

// buildIndexAccessPredicate constructs an index access predicate for idx over rel (indexes.md
// §5.1): a maximal EQUALITY PREFIX on the leading key columns plus an OPTIONAL RANGE on the next
// column. It walks the index's key columns in key order against the WHERE AND-chain, consuming a
// column with an agreed equality conjunct into the prefix and stopping at the first column that
// has no equality (taking its range conjuncts, if any, as the trailing range). Returns nil for a
// non-B-tree index, a Skewed collated bound column (whose stored keys are at the file's pinned
// version — collation.md §12), no bound at all, or an ineligible suffix (a column after the
// equality prefix that is not a fixed-width scalar — the width-based key-suffix skip needs it).
// siblingCutoff opens the index-nested-loop door (>= 0 admits a bare sibling reColumn as a bound
// source, resolved per outer row); -1 is the ordinary once-materialized bound.
func (db *engine) buildIndexAccessPredicate(filter *rExpr, rel scopeRel, idx indexDef, siblingCutoff int) *indexBoundPlan {
	if idx.Kind != indexBtree {
		return nil
	}
	var eqCols []indexEqCol
	var rangeType scalarType
	var rangeTerms []boundTerm
	for _, ci := range idx.Columns {
		// A non-scalar (range/array/composite) column cannot be seeked here — stop the prefix.
		ty, ok := rel.table.Columns[ci].Type.AsScalar()
		if !ok {
			break
		}
		// The column's key collation form (collation.md §8/§12): a Skewed collated column refuses the
		// bound (its stored keys are wrong for the loaded bundle) — stop the prefix here (the column
		// then falls into the fixed-width suffix check below and rejects the whole index if it is text).
		coll, push := db.keyCollationCtx(rel.table.Columns[ci])
		if !push {
			break
		}
		colColl := ""
		if coll != nil {
			colColl = coll.Name
		}
		var eqs []*rExpr
		var ranges []boundTerm
		var walk func(e *rExpr)
		walk = func(e *rExpr) {
			if e == nil {
				return
			}
			if e.kind == reAnd {
				walk(e.lhs)
				walk(e.rhs)
				return
			}
			if t, ok := asBoundTerm(e, rel.offset+ci, ty, colColl, siblingCutoff); ok {
				if t.op == opEq {
					eqs = append(eqs, t.src)
				} else {
					ranges = append(ranges, t)
				}
			}
		}
		walk(filter)
		if len(eqs) > 0 {
			eqCols = append(eqCols, indexEqCol{colType: ty, coll: coll, srcs: eqs})
			continue // extend the equality prefix
		}
		if len(ranges) > 0 {
			rangeType = ty
			rangeTerms = ranges
		}
		break // first non-equality column ends the prefix (with or without a trailing range)
	}
	if len(eqCols) == 0 && rangeTerms == nil {
		return nil // nothing bound
	}
	// Eligibility: every index column from the range column onward (columns[len(eqCols):]) is
	// width-skipped past the known equality prefix, so each must be a fixed-width scalar. The
	// equality-prefix columns may be any width — their slots are matched as the known prefix bytes.
	suffix := make([]scalarType, 0, len(idx.Columns)-len(eqCols))
	for _, c := range idx.Columns[len(eqCols):] {
		s, ok := rel.table.Columns[c].Type.AsScalar()
		if !ok || !s.IsFixedWidth() {
			return nil
		}
		suffix = append(suffix, s)
	}
	return &indexBoundPlan{
		nameKey: strings.ToLower(idx.Name), eqCols: eqCols,
		rangeType: rangeType, rangeTerms: rangeTerms, suffixTypes: suffix,
	}
}

// detectScanBound picks one relation's scan bound (cost.md §3; indexes.md §5): the
// single-column PK bound first; else, among the relation's indexes (held in ascending
// lowercased-name order — the deterministic tie-break), the first that yields a non-empty
// access predicate (buildIndexAccessPredicate); else nil (full scan).
func detectScanBound(filter *rExpr, rel scopeRel, db *engine) *scanBound {
	// A host-attached relation full-scans this slice (attached-databases.md §8): the bounded-scan exec
	// path resolves index stores unscoped, so no PK/index/GiST/GIN bound may apply to an attachment.
	if rel.isAttachment() {
		return nil
	}
	if pkLocal := rel.table.PrimaryKeyIndex(); pkLocal >= 0 {
		// Ordered-equality pushdown is scalar-only; a non-scalar (range) PK skips it (point-lookup
		// deferred for containers — ranges.md §10), falling through to a full scan + residual filter.
		if sty, ok := rel.table.Columns[pkLocal].Type.AsScalar(); ok {
			// The PK column's key collation form (collation.md §8/§12): push=false ⇒ collated but
			// Skewed ⇒ refuse pushdown (full heap-scan recompute — the read-safety rule §12); else
			// coll is nil (C, raw-byte key) or the Full-collated table (push via the sort key).
			if coll, push := db.keyCollationCtx(rel.table.Columns[pkLocal]); push {
				if bp := detectPKBound(filter, rel.offset+pkLocal, sty, coll); bp != nil {
					return &scanBound{pk: bp}
				}
			}
		}
	}
	for _, idx := range rel.table.Indexes {
		// An index access predicate (indexes.md §5.1): a maximal equality prefix + optional trailing
		// range over a B-tree index's leading key columns. Returns nil for a GIN/GiST index (handled
		// by the passes below), an ineligible tail, or no bound. Indexes are held in ascending
		// lowercased-name order, so the first non-nil wins — the deterministic tie-break.
		if ib := db.buildIndexAccessPredicate(filter, rel, idx, -1); ib != nil {
			return &scanBound{index: ib}
		}
	}
	// GiST bound (gist.md §5) — a `col && const` / `col @> const` over a range column; the ordered
	// loop above already skipped the GiST index (its leading column is a non-scalar range).
	if gb := detectGistBound(filter, rel.table.Indexes, rel.table.Columns, rel.offset); gb != nil {
		return &scanBound{gist: gb}
	}
	// GIN bound (gin.md §6) — after the PK and ordered-index equality bounds.
	if gb := detectGinBound(filter, rel.table.Indexes, rel.table.Columns, rel.offset); gb != nil {
		return &scanBound{gin: gb}
	}
	// LAST RESORT — an OR / IN-list of key equalities lowered to merged point probes
	// (cost.md §3 "OR / IN-list"). Reached only when no contiguous PK/index/GIN/GiST bound
	// applied above, so this never displaces an existing plan. The primary key wins over a
	// secondary index (its own key — no second tree), matching detectScanBound's ordering.
	if pkLocal := rel.table.PrimaryKeyIndex(); pkLocal >= 0 {
		if sty, ok := rel.table.Columns[pkLocal].Type.AsScalar(); ok {
			if coll, push := db.keyCollationCtx(rel.table.Columns[pkLocal]); push {
				if srcs := detectKeySet(filter, rel.offset+pkLocal, sty, coll); srcs != nil {
					return &scanBound{pkSet: &pkKeySetPlan{pkType: sty, coll: coll, srcs: srcs}}
				}
			}
		}
	}
	for _, idx := range rel.table.Indexes {
		if idx.Kind != indexBtree {
			continue
		}
		ci := idx.Columns[0]
		ty, ok := rel.table.Columns[ci].Type.AsScalar()
		if !ok {
			continue
		}
		unskippableTail := false
		for _, c := range idx.Columns[1:] {
			s, ok := rel.table.Columns[c].Type.AsScalar()
			if !ok || !s.IsFixedWidth() {
				unskippableTail = true
				break
			}
		}
		if unskippableTail {
			continue
		}
		coll, push := db.keyCollationCtx(rel.table.Columns[ci])
		if !push {
			continue
		}
		if srcs := detectKeySet(filter, rel.offset+ci, ty, coll); srcs != nil {
			tail := make([]scalarType, 0, len(idx.Columns)-1)
			for _, c := range idx.Columns[1:] {
				tail = append(tail, rel.table.Columns[c].Type.ScalarTy())
			}
			return &scanBound{indexSet: &indexKeySetPlan{
				nameKey: strings.ToLower(idx.Name), colType: ty, coll: coll, tailTypes: tail, srcs: srcs,
			}}
		}
	}
	return nil
}

// detectKeySet finds an OR / IN-list disjunction of equalities on ONE key column (at global
// index keyIdx) and returns its equality const-sources (one per disjunct), or nil if the
// filter has no such shape (cost.md §3 "OR / IN-list"). `x IN (a, b, c)` desugars to
// `x = a OR x = b OR x = c` at resolve time (grammar.md §20), so an IN-list and a hand-written
// OR-of-equalities present the identical tree — this handles both. The filter's top-level
// AND-chain is flattened; the FIRST conjunct that reduces to a pure disjunction of
// `keycol = const` equalities is used (the rest of the WHERE stays the residual filter). A
// conjunct reduces iff it is a `keycol = const`, or an OR of two reducing operands — an AND,
// a NOT, a range comparison, or an equality on a different column makes it non-reducing, so a
// mixed disjunction (`pk = 1 OR x = 2`) or a NOT IN (`NOT (pk = 1 OR …)`) correctly yields
// no bound. Conservative + sound: an unrecognized filter contributes no bound.
func detectKeySet(filter *rExpr, keyIdx int, keyType scalarType, coll *Collation) []*rExpr {
	if filter == nil {
		return nil
	}
	colColl := ""
	if coll != nil {
		colColl = coll.Name
	}
	var found []*rExpr
	var walkAnd func(e *rExpr)
	walkAnd = func(e *rExpr) {
		if found != nil || e == nil {
			return
		}
		if e.kind == reAnd {
			walkAnd(e.lhs)
			walkAnd(e.rhs)
			return
		}
		if srcs, ok := reduceKeySet(e, keyIdx, keyType, colColl); ok {
			found = srcs
		}
	}
	walkAnd(filter)
	return found
}

// reduceKeySet reduces one predicate to the set of equality const-sources it bounds keyIdx
// with, or (nil, false) if it is not a pure disjunction of `keycol = const` (detectKeySet).
// Descends OR nodes only; a single `keycol = const` leaf is the base case (reusing
// asBoundTerm, siblingCutoff -1 — no sibling references in a once-materialized bound).
func reduceKeySet(e *rExpr, keyIdx int, keyType scalarType, colColl string) ([]*rExpr, bool) {
	if e == nil {
		return nil, false
	}
	if e.kind == reOr {
		l, lok := reduceKeySet(e.lhs, keyIdx, keyType, colColl)
		if !lok {
			return nil, false
		}
		r, rok := reduceKeySet(e.rhs, keyIdx, keyType, colColl)
		if !rok {
			return nil, false
		}
		return append(l, r...), true
	}
	if t, ok := asBoundTerm(e, keyIdx, keyType, colColl, -1); ok && t.op == opEq {
		return []*rExpr{t.src}, true
	}
	return nil, false
}

// detectINLBound detects an index-nested-loop scan bound for a join inner relation rel (cost.md §3
// "JOIN"): a primary-key (or leading secondary-index column) comparison to a SIBLING column of an
// EARLIER join relation, taken from the join's `on` predicate OR the `whereFilter`. Unlike
// detectScanBound (constants only), this admits a bare sibling column (a reColumn whose global index
// is < rel.offset), resolved per outer row from the current combined left-hand row — the join analog
// of a correlated subquery's outer reference (query.correlated_pushdown). So the inner relation seeks
// per outer row instead of full-scanning for every outer row: O(N·M) → O(N·log M).
//
// Returns non-nil only when the resulting bound has >= 1 sibling term (a reColumn src); a
// constant-only bound is the ordinary once-materialized relBounds path. Constant terms on the same
// key ride along and tighten the per-outer-row seek. The whole on/where stays the residual filter (a
// superset), so the ROWS are unchanged; only the inner re-scan cost drops. Caller restricts this to
// a base table that is the right/nullable side of an INNER/CROSS/LEFT join.
func detectINLBound(on *rExpr, whereFilter *rExpr, rel scopeRel, db *engine) *scanBound {
	// A host-attached inner relation full-scans per outer row this slice (attached-databases.md §8):
	// the seek would resolve its index store unscoped. Index-nested-loop over an attachment is a
	// perf follow-on.
	if rel.isAttachment() {
		return nil
	}
	cutoff := rel.offset
	collect := func(keyIdx int, ty scalarType, coll *Collation) []boundTerm {
		colColl := ""
		if coll != nil {
			colColl = coll.Name
		}
		var terms []boundTerm
		var walk func(e *rExpr)
		walk = func(e *rExpr) {
			if e == nil {
				return
			}
			if e.kind == reAnd {
				walk(e.lhs)
				walk(e.rhs)
				return
			}
			if t, ok := asBoundTerm(e, keyIdx, ty, colColl, cutoff); ok {
				terms = append(terms, t)
			}
		}
		walk(on)
		walk(whereFilter)
		return terms
	}
	hasSibling := func(terms []boundTerm) bool {
		for _, t := range terms {
			if t.src.kind == reColumn {
				return true
			}
		}
		return false
	}
	// Primary-key bound first (the row's own key — range-capable, strictly cheaper).
	if pkLocal := rel.table.PrimaryKeyIndex(); pkLocal >= 0 {
		if sty, ok := rel.table.Columns[pkLocal].Type.AsScalar(); ok {
			if coll, push := db.keyCollationCtx(rel.table.Columns[pkLocal]); push {
				terms := collect(rel.offset+pkLocal, sty, coll)
				if hasSibling(terms) {
					return &scanBound{pk: &pkBoundPlan{pkType: sty, terms: terms, coll: coll}}
				}
			}
		}
	}
	// Else a leading secondary-index equality bound to a sibling (indexes in ascending lowercased
	// name order — the deterministic tie-break, matching detectScanBound).
	for _, idx := range rel.table.Indexes {
		if idx.Kind != indexBtree {
			continue
		}
		ci := idx.Columns[0]
		ty, ok := rel.table.Columns[ci].Type.AsScalar()
		if !ok {
			continue
		}
		unskippableTail := false
		for _, c := range idx.Columns[1:] {
			s, ok := rel.table.Columns[c].Type.AsScalar()
			if !ok || !s.IsFixedWidth() {
				unskippableTail = true
				break
			}
		}
		if unskippableTail {
			continue
		}
		coll, push := db.keyCollationCtx(rel.table.Columns[ci])
		if !push {
			continue
		}
		terms := collect(rel.offset+ci, ty, coll)
		var eqs []*rExpr
		siblingEq := false
		for _, t := range terms {
			if t.op == opEq {
				eqs = append(eqs, t.src)
				if t.src.kind == reColumn {
					siblingEq = true
				}
			}
		}
		if siblingEq {
			// This slice keeps the index-nested-loop bound single-column-equality (a leading key
			// column bound to a sibling); a multi-column / range INL bound is a follow-on (cost.md
			// §3 "index-nested-loop"). suffixTypes are the trailing columns (columns[1:], fixed-width
			// by the unskippableTail check above), width-skipped past the single equality slot.
			tail := make([]scalarType, 0, len(idx.Columns)-1)
			for _, c := range idx.Columns[1:] {
				tail = append(tail, rel.table.Columns[c].Type.ScalarTy())
			}
			return &scanBound{index: &indexBoundPlan{
				nameKey:     strings.ToLower(idx.Name),
				eqCols:      []indexEqCol{{colType: ty, coll: coll, srcs: eqs}},
				suffixTypes: tail,
			}}
		}
	}
	return nil
}

// keyCollationCtx reports the collation a key over col is STORED under, deciding whether — and how —
// a comparison bound may push down to that key (spec/design/collation.md §8/§12). Three outcomes:
//   - (nil, true)  — col is C (or non-text): the key is raw bytes (encoding.md §2.4), always
//     pushable, the unchanged fast path.
//   - (coll, true) — col is collated and the collation is Full (its file pin matches the loaded
//     bundle): the key is the UCA sort key (encoding.md §2.12), pushable using coll to encode the
//     probe in the same form.
//   - (nil, false) — col is collated but Skewed (its file pin differs from the loaded bundle): push
//     is REFUSED. The scan stays a full heap-scan that recomputes against the LOADED table (the
//     read-safety rule §12; seeking a loaded-version probe in a file-version B-tree would mis-match —
//     the tripwire suites/collation/skew.test stays green only because this refuses). An
//     unresolvable collation likewise refuses rather than mis-encoding.
func (db *engine) keyCollationCtx(col catColumn) (*Collation, bool) {
	if col.Collation == "" {
		return nil, true
	}
	snap := db.readSnap()
	if _, _, _, _, skewed := snap.collationSkew(col.Collation); skewed {
		return nil, false
	}
	if c := snap.resolveCollation(col.Collation); c != nil {
		return c, true
	}
	return nil, false
}

// detectGinBound detects a GIN-bounded scan over columns/indexes (gin.md §6): the lowest-named
// GIN index whose array column at offset+ci has a GIN-accelerable conjunct (`col @> const`,
// `col && const`, `const = ANY(col)`, or `col = const`). Factored out so the SELECT planner
// (detectScanBound) and the UPDATE/DELETE scan both use the identical detection — the mutations
// pass their own table's indexes/columns at offset 0.
func detectGinBound(filter *rExpr, indexes []indexDef, columns []catColumn, offset int) *ginBoundPlan {
	if filter == nil {
		return nil
	}
	for _, idx := range indexes {
		if idx.Kind != indexGin {
			continue
		}
		ci := idx.Columns[0]
		colGlobal := offset + ci
		at := columns[ci].Type
		if at.Array == nil {
			continue // a GIN column is always an array (the CREATE INDEX gate); defensive
		}
		if s, _, ok := ginMatch(filter, colGlobal); ok {
			return &ginBoundPlan{
				nameKey: strings.ToLower(idx.Name), elemType: at.Array.ScalarTy(), strategy: s, colGlobal: colGlobal,
			}
		}
	}
	return nil
}

// ginMatch finds the first WHERE AND-chain conjunct a GIN index on colGlobal accelerates
// (spec/design/gin.md §6): `col @> Q` (contains), `col && Q` (overlaps), `c = ANY(col)`
// (membership), or `col = Q` (exact array equality) where the query operand is a constant
// (references no column / outer / subquery). @> is asymmetric (the indexed column must be the LEFT
// operand — `Q @> col` is the non-accelerated <@); && and array = are symmetric; = ANY requires the
// column be ANY's array operand and c the scalar. Returns the strategy and the constant query
// operand (the scalar c for ginMember, the array Q otherwise). Used at plan time (strategy) and exec
// time (recover the operand from plan.filter), so the two agree on the same conjunct by construction.
func ginMatch(filter *rExpr, colGlobal int) (ginStrategy, *rExpr, bool) {
	if filter == nil {
		return 0, nil, false
	}
	if filter.kind == reAnd {
		if s, q, ok := ginMatch(filter.lhs, colGlobal); ok {
			return s, q, true
		}
		return ginMatch(filter.rhs, colGlobal)
	}
	if filter.kind == reArrayFunc && len(filter.sargs) == 2 {
		a, b := filter.sargs[0], filter.sargs[1]
		switch filter.afunc {
		case afContains:
			if isColumn(a, colGlobal) && rexprIsConstant(b) {
				return ginContains, b, true
			}
		case afOverlaps:
			if isColumn(a, colGlobal) && rexprIsConstant(b) {
				return ginOverlaps, b, true
			}
			if isColumn(b, colGlobal) && rexprIsConstant(a) {
				return ginOverlaps, a, true
			}
		}
	}
	// `col = Q` — exact array equality (gin.md §6). Commutative: the column may be either operand,
	// the constant array Q the other. Recovered operand is Q; ginBoundRows reads it via ginEqual
	// (the @>-superset gather + the residual =). <> is NOT matched (only OpEq). When the column is an
	// array, the other constant operand is necessarily an array too (resolve rejects array/scalar =).
	if filter.kind == reCompare && filter.op == opEq {
		if isColumn(filter.lhs, colGlobal) && rexprIsConstant(filter.rhs) {
			return ginEqual, filter.rhs, true
		}
		if isColumn(filter.rhs, colGlobal) && rexprIsConstant(filter.lhs) {
			return ginEqual, filter.lhs, true
		}
	}
	// `c = ANY(col)` — the array spelling of membership (gin.md §6): the GIN column must be ANY's
	// ARRAY operand (rhs) and c (the scalar lhs) a constant. Only = ANY (not = ALL, not any other
	// comparison/quantifier — those are not a single-term posting gather). The recovered query
	// operand is the scalar c; ginBoundRows reads it via ginMember.
	if filter.kind == reQuantified && filter.op == opEq && !filter.quantAll &&
		isColumn(filter.rhs, colGlobal) && rexprIsConstant(filter.lhs) {
		return ginMember, filter.lhs, true
	}
	return 0, nil, false
}

// detectGistBound detects a GiST-bounded scan over columns/indexes (spec/design/gist.md §5): the
// lowest-named GiST index whose range column at offset+ci has a `col && const` / `col @> const`
// conjunct. Factored out so the SELECT planner (detectScanBound) and the UPDATE/DELETE scan share
// the identical detection (the GIN precedent) — the mutations pass their indexes/columns at offset 0.
func detectGistBound(filter *rExpr, indexes []indexDef, columns []catColumn, offset int) *gistBoundPlan {
	for _, idx := range indexes {
		if idx.Kind != indexGist {
			continue
		}
		// The planner gather is single-operator: only a single-column GiST index accelerates a
		// `col && Q` / `col @> Q` / `col = Q` conjunct. A multi-column GiST index (an EXCLUDE backing
		// structure, gist.md §7) is probed only by the constraint, never the planner.
		if len(idx.Columns) != 1 {
			continue
		}
		ci := idx.Columns[0]
		colGlobal := offset + ci
		colTy := columns[ci].Type
		if colTy.IsRange() {
			// range_ops (GX1): a `col && Q` / `col @> Q` conjunct.
			if s, _, ok := gistMatch(filter, colGlobal); ok {
				return &gistBoundPlan{nameKey: strings.ToLower(idx.Name), strategy: s, colGlobal: colGlobal}
			}
		} else if isGistScalarType(colTy) {
			// scalar `=` opclass (GX2): a `col = Q` conjunct over a fixed-width keyable scalar.
			if _, _, ok := gistScalarMatch(filter, colGlobal); ok {
				return &gistBoundPlan{nameKey: strings.ToLower(idx.Name), strategy: gistEqual, colGlobal: colGlobal, scalarType: colTy.ScalarTy()}
			}
		}
	}
	return nil
}

// gistScalarMatch finds the first WHERE AND-chain conjunct a GiST scalar `=` opclass on colGlobal
// accelerates (spec/design/gist.md §6): `col = Q` where Q is a constant (re-evaluable per scan).
// Equality is commutative (the column may be either operand). <> and the inequalities are not
// accelerated (the `=` opclass has only the equal strategy). Returns the Equal strategy and the
// constant operand — used at plan time (strategy) and exec time (recover from plan.filter).
func gistScalarMatch(filter *rExpr, colGlobal int) (gistStrategy, *rExpr, bool) {
	if filter == nil {
		return 0, nil, false
	}
	if filter.kind == reAnd {
		if s, q, ok := gistScalarMatch(filter.lhs, colGlobal); ok {
			return s, q, true
		}
		return gistScalarMatch(filter.rhs, colGlobal)
	}
	if filter.kind == reCompare && filter.op == opEq {
		if isColumn(filter.lhs, colGlobal) && rexprIsConstant(filter.rhs) {
			return gistEqual, filter.rhs, true
		}
		if isColumn(filter.rhs, colGlobal) && rexprIsConstant(filter.lhs) {
			return gistEqual, filter.lhs, true
		}
	}
	return 0, nil, false
}

// gistQueryOperand recovers a GiST bound's constant query operand from the live filter at exec time
// — gistMatch for range_ops (&&/@>), gistScalarMatch for the scalar `=` opclass. Centralizes the
// strategy dispatch so every scan site (SELECT / UPDATE / DELETE) recovers the operand uniformly.
func gistQueryOperand(filter *rExpr, gb *gistBoundPlan) (*rExpr, bool) {
	if gb.strategy == gistEqual {
		_, q, ok := gistScalarMatch(filter, gb.colGlobal)
		return q, ok
	}
	_, q, ok := gistMatch(filter, gb.colGlobal)
	return q, ok
}

// gistMatch finds the first WHERE AND-chain conjunct a GiST range_ops index on colGlobal accelerates
// (spec/design/gist.md §5): `col && Q` (overlap — symmetric) or `col @> Q` (contains — asymmetric,
// the column must be the LEFT operand; `Q @> col` is the non-accelerated <@). Q must be a constant.
// The other range operators stay full-scan this slice. Returns the strategy and the constant query
// operand — used at plan time (strategy) and exec time (recover from plan.filter).
func gistMatch(filter *rExpr, colGlobal int) (gistStrategy, *rExpr, bool) {
	if filter == nil {
		return 0, nil, false
	}
	if filter.kind == reAnd {
		if s, q, ok := gistMatch(filter.lhs, colGlobal); ok {
			return s, q, true
		}
		return gistMatch(filter.rhs, colGlobal)
	}
	if filter.kind == reRangeOp && len(filter.sargs) == 2 {
		a, b := filter.sargs[0], filter.sargs[1]
		switch filter.rop {
		case roOverlaps: // && — symmetric in its operands
			if isColumn(a, colGlobal) && rexprIsConstant(b) {
				return gistOverlaps, b, true
			}
			if isColumn(b, colGlobal) && rexprIsConstant(a) {
				return gistOverlaps, a, true
			}
		case roContains: // @> — the indexed column must be the container (LEFT)
			if isColumn(a, colGlobal) && rexprIsConstant(b) {
				return gistContains, b, true
			}
		}
	}
	return 0, nil, false
}

// isColumn reports whether e is a reference to the column at global scope index colGlobal.
func isColumn(e *rExpr, colGlobal int) bool {
	return e != nil && e.kind == reColumn && e.index == colGlobal
}

// rexprIsConstant reports whether e is evaluable without a current/outer row (so its value is the
// same for every scanned row — computable once). False for any column, correlated outer column, or
// subquery; true for literals, params, and pure operations over them. Used to admit a GIN query
// operand Q (spec/design/gin.md §6: a constant query only this slice). Mirrors the traversal of
// rexprReferencesOuter.
func rexprIsConstant(e *rExpr) bool {
	if e == nil {
		return true
	}
	switch e.kind {
	case reColumn, reOuterColumn, reSubquery:
		return false
	}
	if e.operand != nil && !rexprIsConstant(e.operand) {
		return false
	}
	if e.lhs != nil && !rexprIsConstant(e.lhs) {
		return false
	}
	if e.rhs != nil && !rexprIsConstant(e.rhs) {
		return false
	}
	for _, arm := range e.caseArms {
		if !rexprIsConstant(arm.cond) || !rexprIsConstant(arm.result) {
			return false
		}
	}
	if e.caseEls != nil && !rexprIsConstant(e.caseEls) {
		return false
	}
	for _, a := range e.sargs {
		if !rexprIsConstant(a) {
			return false
		}
	}
	for _, s := range e.subs {
		if !rexprIsConstant(s.index) || !rexprIsConstant(s.lower) || !rexprIsConstant(s.upper) {
			return false
		}
	}
	return true
}

// indexBoundRows executes an index access-predicate bound (cost.md §3 "index-bounded scan",
// indexes.md §5.1): build the concrete index-key range from the equality prefix + optional
// trailing range, then fetch the rows it admits, in index-entry order (= key order, then
// storage-key order), with the scan's up-front units (pages, slabs) — the index-tree nodes
// overlapping the range plus, per admitted entry, the table-tree nodes of that row's point
// lookup and its touched-column decompress slabs. The caller feeds the rows through the same
// scanSource as any bounded scan. A provably empty bound (a NULL / contradictory equality, a
// NULL / contradictory range, an out-of-range integer) returns nothing and charges nothing.
func (db *engine) indexBoundRows(tableName string, ib *indexBoundPlan, params []Value, outer []storedRow, mask []bool, left storedRow) (rows []storedRow, pages, slabs int, err error) {
	b, prefixByteLen, empty := db.buildIndexBound(ib, params, outer, left)
	if empty {
		return nil, 0, 0, nil
	}
	return db.indexScanBound(tableName, ib.nameKey, ib.suffixTypes, b, prefixByteLen, mask)
}

// buildIndexBound turns an index access predicate into a concrete index-key range at exec time
// (indexes.md §5.1). It encodes the equality prefix into P (the concatenated present slots), then
// — if there is a range column — starts from [P, P‖0x01) (the upper endpoint stops before the
// range column's NULL slot, since a range is never true for NULL) and intersects each range term;
// otherwise the range is [P, byte-successor(P)) (every entry extending P). empty=true ⇒ the bound
// admits no key (a NULL / disagreeing prefix equality, a NULL range endpoint, or a contradictory
// range). prefixByteLen = len(P), the byte count the row-key suffix skip advances past the
// equality-prefix slots before width-skipping the remaining components.
func (db *engine) buildIndexBound(ib *indexBoundPlan, params []Value, outer []storedRow, left storedRow) (b keyBound, prefixByteLen int, empty bool) {
	var p []byte
	for _, ec := range ib.eqCols {
		// Every equality const-source on this column must encode to ONE agreed value: a NULL is
		// 3VL-never-true, a disagreement (`a = 1 AND a = 2`) is a contradiction, and an out-of-range
		// integer can equal no stored value — all provably empty.
		var agreed []byte
		for _, src := range ec.srcs {
			key, isNull, ok := encodeBoundKey(ec.colType, src, params, outer, ec.coll, left)
			if isNull || !ok {
				return keyBound{}, 0, true
			}
			if agreed == nil {
				agreed = key
			} else if !bytes.Equal(agreed, key) {
				return keyBound{}, 0, true
			}
		}
		p = append(p, 0x00)
		p = append(p, agreed...)
	}
	if ib.rangeTerms == nil {
		b = keyBound{lo: p, loInc: true, hi: prefixSuccessor(p), hiInc: false}
		return b, len(p), boundEmpty(b)
	}
	// Equality prefix P + a range on the next column. Base: [P, P‖0x01) — present values only
	// (the 0x01 NULL tag sorts after every 0x00 present slot at this position).
	b = keyBound{lo: append([]byte(nil), p...), loInc: true, hi: append(append([]byte(nil), p...), 0x01), hiInc: false}
	for _, t := range ib.rangeTerms {
		// The range column is fixed-width (indexes.md §5.1 eligibility), so it is never collated: the
		// probe encodes with a nil collation.
		key, isNull, ok := encodeBoundKey(ib.rangeType, t.src, params, outer, nil, left)
		if isNull {
			return keyBound{}, 0, true
		}
		if !ok {
			continue // out-of-range endpoint: drop this half-bound (a wider, still-sound scan)
		}
		ps := append(append([]byte(nil), p...), 0x00) // P ‖ 0x00 (present tag)
		ps = append(ps, key...)                       // P ‖ 0x00 ‖ encode(v)
		switch t.op {
		case opGe:
			b = intersectLo(b, ps, true)
		case opGt:
			b = intersectLo(b, prefixSuccessor(ps), true) // skip the whole c = v subtree
		case opLt:
			b = intersectHi(b, ps, false)
		case opLe:
			b = intersectHi(b, prefixSuccessor(ps), false)
		case opEq: // defensive — an equality never reaches rangeTerms, but treat it as [v, v]
			b = intersectLo(b, ps, true)
			b = intersectHi(b, prefixSuccessor(ps), false)
		}
	}
	return b, len(p), boundEmpty(b)
}

// indexScanBound range-scans the index B-tree over an already-built key bound and point-looks-up
// each admitted entry's row, returning them in index-entry order with the scan's up-front (pages,
// slabs) block — the index-tree nodes overlapping the range plus, per entry, the row's point
// lookup. prefixByteLen is the equality-prefix byte length skipped before the fixed-width
// suffix-skip that recovers each entry's row storage key (indexes.md §5.1). Shared by the
// access-predicate bound (indexBoundRows) and the OR / IN-list point-set (indexPointRows) so both
// drive the identical per-entry fetch — same cost by construction.
func (db *engine) indexScanBound(tableName, nameKey string, suffixTypes []scalarType, b keyBound, prefixByteLen int, mask []bool) (rows []storedRow, pages, slabs int, err error) {
	istore := db.lkpIndexStore(nameKey)
	// The index store has no payload columns, so its mask is empty and its fused scan contributes
	// only the index-tree page_read count (no spill/compress units).
	entries, pages, _, err := istore.RangeScanWithUnits(b, nil)
	if err != nil {
		return nil, 0, 0, err
	}
	store := db.lkpStore(tableName)
	for _, e := range entries {
		// Skip the equality prefix by its known byte length, then each remaining key component by
		// width (self-delimiting — a 0x01 NULL tag alone, or 0x00 + the fixed width, indexes.md §5.1);
		// the suffix after them is the row's storage key (indexes.md §3).
		at := prefixByteLen
		for _, ty := range suffixTypes {
			if at < len(e.Key) && e.Key[at] == 0x01 {
				at++
			} else {
				at += 1 + ty.WidthBytes()
			}
		}
		rowKey := e.Key[at:]
		row, ok, n, sl, err := store.GetWithUnits(rowKey, mask)
		if err != nil {
			return nil, 0, 0, err
		}
		pages += n
		slabs += sl
		if !ok {
			panic("an index entry references a stored row")
		}
		rows = append(rows, row)
	}
	return rows, pages, slabs, nil
}

// indexPointRows fetches the rows a SINGLE already-encoded leading-column index value admits — the
// equality prefix scan [0x00‖value, byte-successor) over the index B-tree plus, per admitted entry,
// the row's point lookup. Used by the OR / IN-list secondary-index point-set (indexKeySetRows),
// where each distinct list value is one such point probe. suffixTypes are the trailing key
// components (columns[1:], fixed-width), width-skipped past the single leading slot.
func (db *engine) indexPointRows(tableName, nameKey string, suffixTypes []scalarType, valueKey []byte, mask []bool) (rows []storedRow, pages, slabs int, err error) {
	prefix := append([]byte{0x00}, valueKey...)
	b := keyBound{lo: prefix, loInc: true, hi: prefixSuccessor(prefix), hiInc: false}
	return db.indexScanBound(tableName, nameKey, suffixTypes, b, len(prefix), mask)
}

// encodeKeySet encodes an OR / IN-list's equality const-sources into the key space and returns
// a SORTED, DE-DUPLICATED set of encoded keys — the distinct point probes a merged point-set
// bound visits (cost.md §3 "OR / IN-list"). A src that is NULL (3VL-never-true) or does not
// encode into the key domain (an out-of-range integer — no stored key can equal it) contributes
// no point and is skipped (sound: the union stays a superset, and the residual filter re-checks
// each admitted row). Byte-dedup == value-dedup and byte-sort == value-sort under the
// order-preserving key encoding (encoding.md §2), so probing the sorted distinct keys yields
// rows in ascending key order with no row visited twice. Shared by the PK and secondary-index
// point-set executors.
func encodeKeySet(keyType scalarType, srcs []*rExpr, params []Value, outer []storedRow, coll *Collation, left storedRow) [][]byte {
	keys := make([][]byte, 0, len(srcs))
	for _, src := range srcs {
		key, isNull, ok := encodeBoundKey(keyType, src, params, outer, coll, left)
		if isNull || !ok {
			continue
		}
		keys = append(keys, key)
	}
	sort.Slice(keys, func(i, j int) bool { return bytes.Compare(keys[i], keys[j]) < 0 })
	out := keys[:0]
	for i, k := range keys {
		if i == 0 || !bytes.Equal(k, keys[i-1]) {
			out = append(out, k)
		}
	}
	return out
}

// pkKeySetRows executes a primary-key OR / IN-list point-set bound (cost.md §3 "OR / IN-list"):
// each distinct encoded key is a point probe [k, k] over the row's own B-tree, and the admitted
// rows are concatenated in ascending key order (the same order a full scan would yield). The
// per-probe (pages, slabs) blocks sum, so the metered cost is the sum of the individual point
// lookups — a core that full-scans instead computes a different cost (the cross-core contract,
// §8). Returns the (key, row) entries so the mutation paths can Remove/Replace by key; SELECT
// discards the keys. Reconstruction is uniformly lazy since B4 (bplus-reshape.md) — the old
// masked/unmasked reconstruction variants are collapsed, and the (pages, slabs) cost stays driven
// by the shared mask (the cost touched set).
func (db *engine) pkKeySetRows(store *tableStore, ks *pkKeySetPlan, params []Value, outer []storedRow, mask []bool, left storedRow, masked bool) (entries []entry, pages, slabs int, err error) {
	for _, k := range encodeKeySet(ks.pkType, ks.srcs, params, outer, ks.coll, left) {
		b := keyBound{lo: k, loInc: true, hi: k, hiInc: true}
		es, p, sl, err := store.RangeScanWithUnits(b, mask)
		if err != nil {
			return nil, 0, 0, err
		}
		entries = append(entries, es...)
		pages += p
		slabs += sl
	}
	return entries, pages, slabs, nil
}

// indexKeySetRows executes a secondary-index OR / IN-list point-set bound (cost.md §3 "OR /
// IN-list"): each distinct encoded value is an index point probe (indexPointRows), and the rows
// are gathered in ascending value order. Because a row has exactly one value for the indexed
// column, distinct values probe disjoint row sets — no row is fetched twice. The per-probe
// (pages, slabs) blocks sum, matching the PK point-set cost contract.
func (db *engine) indexKeySetRows(tableName string, ks *indexKeySetPlan, params []Value, outer []storedRow, mask []bool, left storedRow) (rows []storedRow, pages, slabs int, err error) {
	for _, k := range encodeKeySet(ks.colType, ks.srcs, params, outer, ks.coll, left) {
		r, p, sl, err := db.indexPointRows(tableName, ks.nameKey, ks.tailTypes, k, mask)
		if err != nil {
			return nil, 0, 0, err
		}
		rows = append(rows, r...)
		pages += p
		slabs += sl
	}
	return rows, pages, slabs, nil
}

// ginBoundRows executes a GIN-bounded scan (spec/design/gin.md §6, cost.md §3). Evaluates the
// constant query operand, extracts its terms + mode via the array_ops opclass (an array for @>/&&/=;
// a single scalar term for = ANY — ginMember; the array's distinct non-NULL terms for = — ginEqual),
// gathers each term's posting list (a prefix range scan of the GIN entry tree), combines them by mode
// (@>, = ANY, and = → intersection, && → union) into the candidate storage-key set, and
// point-looks-up each candidate in storage-key order. The original predicate stays the residual WHERE
// filter (re-applied downstream), so the result is always correct. Returns the candidate rows + the
// scan's up-front (pages, slabs); gin_entry (per posting entry visited) is charged on meter directly.
// Degenerate constant queries (gin.md §6): a NULL Q, an @> whose Q holds a NULL element, an && with
// no non-NULL term, and a NULL = ANY scalar are provably empty; @> '{}' and array = with no non-NULL
// term fall back to the full scan.
// ginBoundRows gathers a GIN-bounded scan's candidate rows as (storage key, row) Entry pairs
// (the candidate set IS the storage keys), with the up-front (page_read nodes, value_decompress
// slabs) block. SELECT drops the keys; UPDATE/DELETE keep them to rewrite/remove the rows
// (gin.md §6). GinEntry is charged inside (during the gather); the caller charges the block.
func (db *engine) ginBoundRows(tableName string, gb *ginBoundPlan, query *rExpr, env *evalEnv, meter *costMeter, mask []bool) (out []entry, pages, slabs int, err error) {
	store := db.lkpStore(tableName)
	if query == nil {
		return nil, 0, 0, nil
	}
	// Extract the query's terms (extract_query_terms) — a pure planning step, NOT metered (cost.md
	// §3): evaluate Q on a scratch meter. Q is a constant, so the nil row suffices.
	qv, err := query.eval(nil, env, &costMeter{})
	if err != nil {
		return nil, 0, 0, err
	}
	// Each term is the element's order-preserving key encoding (gin.md §4) — the SAME bytes the
	// entries carry, so a term doubles as its posting-list prefix below. Encoding now lets us dedup
	// distinct terms by bytes (a bijection: byte-dedup == value-dedup, byte-sort == value-sort)
	// generically over every admitted element type.
	var terms [][]byte
	hasNull := false
	isEmpty := false
	if gb.strategy == ginMember {
		// `c = ANY(col)`: the query operand is a SCALAR, not an array. A NULL c can equal no element,
		// so the bound is provably empty (gin.md §6). c is in the element type's domain by resolution
		// (jed coerces c to the element type, rejecting an out-of-range integer constant 22003 before
		// exec); InRange is a defensive guard against silently truncating an out-of-range integer into
		// a wrong term.
		if qv.Kind == ValNull {
			return nil, 0, 0, nil
		}
		if qv.Kind == ValInt && !gb.elemType.InRange(qv.Int) {
			return nil, 0, 0, nil // out-of-range guard
		}
		// a GIN element is fixed-width (isGinElementType excludes text), so the term never collates / fails
		t, err := encodeKeyValue(gb.elemType, qv, nil)
		if err != nil {
			panic("a GIN element key is infallible (fixed-width, no collation)")
		}
		terms = append(terms, t)
	} else {
		if qv.Kind != ValArray {
			return nil, 0, 0, nil // a NULL whole-array (or non-array) query → provably empty
		}
		seen := make(map[string]bool)
		for _, el := range qv.arrayVal().Elements {
			if el.Kind == ValNull {
				hasNull = true // a NULL element carries no term
				continue
			}
			t, err := encodeKeyValue(gb.elemType, el, nil)
			if err != nil {
				panic("a GIN element key is infallible (fixed-width, no collation)")
			}
			if !seen[string(t)] {
				seen[string(t)] = true
				terms = append(terms, t)
			}
		}
		isEmpty = len(qv.arrayVal().Elements) == 0
		slices.SortFunc(terms, bytes.Compare)
	}

	switch gb.strategy {
	case ginContains:
		if isEmpty {
			// @> '{}': every non-NULL array contains the empty array — not derivable from the index;
			// fall back to the full scan (the residual filter keeps the right rows — gin.md §6).
			entries, p, sl, e := store.ScanWithUnits(mask)
			if e != nil {
				return nil, 0, 0, e
			}
			return entries, p, sl, nil
		}
		if hasNull {
			return nil, 0, 0, nil // @> a query with a NULL element is never TRUE
		}
	case ginEqual:
		if len(terms) == 0 {
			// col = Q with NO non-NULL term — '{}' (isEmpty) or an all-NULL Q (hasNull, no non-NULL
			// element). The rows it matches ({}, {NULL}, …) carry NO index terms, so the index cannot
			// enumerate them: fall back to the full scan and let the residual = keep them (gin.md §6).
			// NOT a provably-empty bound — and a Q with ≥1 non-NULL element is NOT caught here (it
			// gathers, even when it also has a NULL element).
			entries, p, sl, e := store.ScanWithUnits(mask)
			if e != nil {
				return nil, 0, 0, e
			}
			return entries, p, sl, nil
		}
	case ginOverlaps:
		if len(terms) == 0 {
			return nil, 0, 0, nil // && with no non-NULL term overlaps nothing
		}
	}

	// Gather each term's posting list: the entry range [encode(term), successor) of the GIN tree
	// (gin.md §4). The entry is encode(term) ‖ storage_key; the fixed-width term self-delimits, so
	// the storage key is the suffix after termWidth bytes.
	istore := db.lkpIndexStore(gb.nameKey)
	termWidth := gb.elemType.WidthBytes()
	entriesVisited := 0
	postings := make([][][]byte, 0, len(terms))
	for _, prefix := range terms {
		b := keyBound{lo: prefix, loInc: true, hi: prefixSuccessor(prefix), hiInc: false}
		es, p, _, e := istore.RangeScanWithUnits(b, nil)
		if e != nil {
			return nil, 0, 0, e
		}
		pages += p
		entriesVisited += len(es)
		keys := make([][]byte, len(es))
		for i := range es {
			keys[i] = es[i].Key[termWidth:]
		}
		postings = append(postings, keys)
	}
	meter.Charge(costs.GinEntry * int64(entriesVisited))

	// Combine into the candidate storage keys, ascending byte (= storage-key) order, so the point
	// lookups and emitted rows follow storage order exactly as a full scan (gin.md §6/§8).
	// @> ALL → intersection; = ANY (ginMember) is a single term, so its intersection is that lone
	// posting list; array = (ginEqual) gathers the same superset as @> over Q's distinct non-NULL
	// terms (the residual = makes it exact downstream). && ANY → union.
	var cand [][]byte
	if gb.strategy == ginOverlaps {
		cand = unionPostings(postings)
	} else {
		cand = intersectPostings(postings)
	}

	for _, key := range cand {
		row, ok, n, sl, e := store.GetWithUnits(key, mask)
		if e != nil {
			return nil, 0, 0, e
		}
		pages += n
		slabs += sl
		if !ok {
			panic("a GIN entry references a stored row")
		}
		out = append(out, entry{Key: key, Row: row})
	}
	return out, pages, slabs, nil
}

// gistBoundRows gathers a GiST-bounded scan's candidate rows (spec/design/gist.md §5). Evaluates the
// constant query operand, then DESCENDS the index's resident R-tree visiting only children
// consistent with the query, collecting candidate storage keys at the leaves; each candidate row is
// point-looked-up in storage-key order. The original &&/@> predicate stays the residual WHERE filter
// (always-recheck), so the result is exactly the full-scan result — the bound only narrows which rows
// are fetched. Returns the candidate (key, row) Entry pairs + the up-front (page_read, value_decompress)
// block (tree nodes visited + each candidate's lookup); gist_descent (per interior) is charged on meter
// directly. Degenerate constant queries (gist.md §5): a NULL Q and an empty && query match nothing; an
// empty @> query (col @> 'empty') matches every row → full-scan fallback (the empty bound is invisible
// to the overlap-descend).
func (db *engine) gistBoundRows(tableName string, gb *gistBoundPlan, query *rExpr, env *evalEnv, meter *costMeter, mask []bool) (out []entry, pages, slabs int, err error) {
	store := db.lkpStore(tableName)
	if query == nil {
		return nil, 0, 0, nil
	}
	// The query operand is a constant; evaluating it (extract query) is a planning step, NOT metered.
	qv, err := query.eval(nil, env, &costMeter{})
	if err != nil {
		return nil, 0, 0, err
	}
	// Form the resident-tree search query from the constant, handling strategy-specific degenerate
	// cases. A NULL query is never TRUE for any row (all strategies).
	var gq gistQuery
	if gb.strategy == gistEqual {
		// scalar `=` (gist.md §6): encode the constant to its order-preserving key bytes.
		if qv.Kind == ValNull {
			return nil, 0, 0, nil
		}
		k, e := encodeKeyValue(gb.scalarType, qv, nil)
		if e != nil {
			panic("a fixed-width GiST scalar key is infallible (no collation)")
		}
		gq = gistQuery{skey: k}
	} else {
		if qv.Kind != ValRange {
			return nil, 0, 0, nil // a NULL (or non-range) query is never TRUE (both && and @>)
		}
		qr := qv.rangeVal()
		if qr.Empty {
			switch gb.strategy {
			case gistContains:
				// col @> 'empty' is TRUE for every row, but an empty bound is absorbed by the union, so
				// it is invisible to the overlap-descend (a false-negative trap, gist.md §5). Fall back
				// to the full scan; the residual @> keeps every row.
				entries, p, sl, e := store.ScanWithUnits(mask)
				return entries, p, sl, e
			default:
				return nil, 0, 0, nil // col && 'empty' overlaps nothing
			}
		}
		gq = gistQuery{rng: qr}
	}
	// Descend the resident R-tree (rebuilt at each mutating statement, gist.md §3/§4.1) — no per-query
	// build. page_read per node touched + gist_descent per interior node.
	var skeys [][]byte
	if tree := db.readSnap().gistTreeFor(gb.nameKey); tree != nil {
		nodes, interior := 0, 0
		skeys, nodes, interior = tree.search([]gistQuery{gq}, []gistStrategy{gb.strategy})
		pages += nodes
		meter.Charge(costs.GistDescent * int64(interior))
	}
	// Point-look-up each candidate in storage-key order (the candidates ARE storage keys).
	slices.SortFunc(skeys, bytes.Compare)
	skeys = slices.CompactFunc(skeys, bytes.Equal)
	for _, key := range skeys {
		row, ok, n, sl, e := store.GetWithUnits(key, mask)
		if e != nil {
			return nil, 0, 0, e
		}
		pages += n
		slabs += sl
		if !ok {
			panic("a GiST entry references a stored row")
		}
		out = append(out, entry{Key: key, Row: row})
	}
	return out, pages, slabs, nil
}

// intersectPostings returns the storage keys present in EVERY posting list (the @> mode-ALL
// combine), sorted ascending. Each posting list holds distinct keys (one (term,row) entry per
// row), so a per-list count == the number of lists means the key is in all of them.
func intersectPostings(postings [][][]byte) [][]byte {
	if len(postings) == 0 {
		return nil
	}
	count := make(map[string]int)
	for _, list := range postings {
		for _, k := range list {
			count[string(k)]++
		}
	}
	need := len(postings)
	var out [][]byte
	for _, k := range postings[0] {
		if count[string(k)] == need {
			out = append(out, k)
		}
	}
	slices.SortFunc(out, bytes.Compare)
	return out
}

// unionPostings returns the storage keys present in ANY posting list (the && mode-ANY combine),
// deduplicated and sorted ascending.
func unionPostings(postings [][][]byte) [][]byte {
	seen := make(map[string]bool)
	var out [][]byte
	for _, list := range postings {
		for _, k := range list {
			if !seen[string(k)] {
				seen[string(k)] = true
				out = append(out, k)
			}
		}
	}
	slices.SortFunc(out, bytes.Compare)
	return out
}

// prefixSuccessor is the byte-successor of a prefix: the smallest byte string greater
// than every string that extends p. Increment the last non-0xFF byte and truncate after
// it; an all-0xFF prefix has no successor (nil ⇒ unbounded high end).
func prefixSuccessor(p []byte) []byte {
	s := append([]byte(nil), p...)
	for len(s) > 0 {
		if s[len(s)-1] == 0xFF {
			s = s[:len(s)-1]
		} else {
			s[len(s)-1]++
			return s
		}
	}
	return nil
}

// pkBoundFor detects a single-table mutation's (UPDATE/DELETE) PK pushdown bound; nil ⇒ full scan.
func (db *engine) pkBoundFor(table *catTable, filter *rExpr) *pkBoundPlan {
	if filter == nil {
		return nil
	}
	pkIdx := table.PrimaryKeyIndex()
	if pkIdx < 0 {
		return nil
	}
	// Point-lookup pushdown is scalar-only; a non-scalar (range) PK skips it (deferred — ranges.md
	// §10), so a range PK WHERE k = … full-scans + residual-filters.
	sty, ok := table.Columns[pkIdx].Type.AsScalar()
	if !ok {
		return nil
	}
	// A collated Skewed PK refuses pushdown (push=false) — though a skewed table's write is already
	// refused XX002 upstream (ensureCollationsWritable), so this is reached only for a C or
	// Full-collated PK (collation.md §8/§12).
	coll, push := db.keyCollationCtx(table.Columns[pkIdx])
	if !push {
		return nil
	}
	return detectPKBound(filter, pkIdx, sty, coll)
}

// pkSetFor is the pkBoundFor analog for an OR / IN-list of primary-key equalities — a merged
// PK point-set bound for the UPDATE/DELETE scan (cost.md §3 "OR / IN-list"). Like pkBoundFor it
// applies only to a scalar, non-Skewed-collated PK. A secondary-index point-set for DML is the
// separate index-scans-for-DML follow-on, so mutations bound only by the primary key here.
func (db *engine) pkSetFor(table *catTable, filter *rExpr) *pkKeySetPlan {
	if filter == nil {
		return nil
	}
	pkIdx := table.PrimaryKeyIndex()
	if pkIdx < 0 {
		return nil
	}
	sty, ok := table.Columns[pkIdx].Type.AsScalar()
	if !ok {
		return nil
	}
	coll, push := db.keyCollationCtx(table.Columns[pkIdx])
	if !push {
		return nil
	}
	if srcs := detectKeySet(filter, pkIdx, sty, coll); srcs != nil {
		return &pkKeySetPlan{pkType: sty, coll: coll, srcs: srcs}
	}
	return nil
}

// detectPKBound flattens the WHERE's top-level AND-chain (an OR is never descended — a disjunction
// is not a contiguous range) and collects every `pk <cmp> const-source` conjunct. Returns nil when
// none exist (⇒ full scan). Conservative + sound: an unrecognized conjunct contributes no bound and
// stays in the residual filter. coll is the key column's collation when collated AND Full (nil for a
// C key); a comparison's own collation must match it for the comparison to seed a bound.
func detectPKBound(filter *rExpr, pkIdx int, pkType scalarType, coll *Collation) *pkBoundPlan {
	colColl := ""
	if coll != nil {
		colColl = coll.Name
	}
	var terms []boundTerm
	var walk func(e *rExpr)
	walk = func(e *rExpr) {
		if e.kind == reAnd {
			walk(e.lhs)
			walk(e.rhs)
			return
		}
		if t, ok := asBoundTerm(e, pkIdx, pkType, colColl, -1); ok {
			terms = append(terms, t)
		}
	}
	walk(filter)
	if len(terms) == 0 {
		return nil
	}
	return &pkBoundPlan{pkType: pkType, terms: terms, coll: coll}
}

// asBoundTerm recognizes a single PK comparison conjunct: a comparison (=,<,<=,>,>=) with the bare
// LOCAL PK column (reColumn at pkIdx — a correlated reOuterColumn is a different kind, so it never
// matches) on one side and a const-source of the PK's own type on the other (a promoted comparison
// — e.g. intpk = 2.5 → a reConstDecimal — does not match, so it stays residual). The op is flipped
// when the PK is on the right.
func asBoundTerm(e *rExpr, pkIdx int, pkType scalarType, colColl string, siblingCutoff int) (boundTerm, bool) {
	if e.kind != reCompare {
		return boundTerm{}, false
	}
	// A comparison bounds the key only when ITS resolved collation matches the key column's frozen
	// collation (colColl) — so the comparison orders text the SAME way the B-tree is keyed
	// (spec/design/collation.md §8). C key ⇔ a C/byte comparison (both empty); a collated key ⇔ a
	// comparison under the SAME collation (the column's implicit collation, or an explicit
	// COLLATE "<that name>"). A comparison under a DIFFERENT collation — name COLLATE "C" over a
	// unicode column, COLLATE "de" over unicode — does NOT match: its order disagrees with the
	// stored keys, so it stays a full scan + residual filter. (A *skewed* collated key never reaches
	// here — keyCollationCtx refuses the whole bound, §12.) The probe is then encoded in the key
	// column's form (sort key for a Full-collated column — buildKeyBound/indexBoundRows).
	cmpColl := ""
	if e.collation != nil {
		cmpColl = e.collation.Name
	}
	if cmpColl != colColl {
		return boundTerm{}, false
	}
	switch e.op {
	case opEq, opLt, opLe, opGt, opGe:
	default:
		return boundTerm{}, false
	}
	isPK := func(x *rExpr) bool { return x.kind == reColumn && x.index == pkIdx }
	switch {
	case isPK(e.lhs) && isConstSource(e.rhs, pkType, siblingCutoff):
		return boundTerm{op: e.op, src: e.rhs}, true
	case isPK(e.rhs) && isConstSource(e.lhs, pkType, siblingCutoff):
		return boundTerm{op: flipCompare(e.op), src: e.lhs}, true
	}
	return boundTerm{}, false
}

// isConstSource reports whether e is constant for the whole scan (no per-row input) AND of a type
// that encodes into the PK key space: a same-family const literal, a NULL literal (⇒ a provably
// empty range), a bind parameter $N (its inferred type matched the PK via the comparison; a value
// that does not fit is caught at buildKeyBound), or a bare correlated reOuterColumn — its value is a
// runtime constant for a given outer row, so the inner subquery's PK is bounded by the current outer
// row's column and seeks instead of re-scanning the whole inner table per outer row (cost.md §3
// "bounded scan", grammar.md §26). A type-mismatched outer reference is wrapped in a cast by the
// resolver (as for a const literal), so it never arrives here bare — the type check stays implicit.
//
// siblingCutoff opens the index-nested-loop door (cost.md §3 "JOIN"): when >= 0, a bare reColumn
// whose GLOBAL index is < siblingCutoff — a column of an EARLIER join relation — is a valid bound
// source, resolved per outer row from the combined left-hand row (like reOuterColumn, a bare sibling
// column implies a type match — a mismatch is a cast, never bare). -1 (the ordinary once-materialized
// bound) accepts only literals/params/outer references.
func isConstSource(e *rExpr, pkType scalarType, siblingCutoff int) bool {
	switch e.kind {
	case reParam, reConstNull, reOuterColumn:
		return true
	case reColumn:
		return siblingCutoff >= 0 && e.index < siblingCutoff
	case reConstInt:
		return pkType.IsInteger()
	case reConstBool:
		return pkType.IsBool()
	case reConstUuid:
		return pkType.IsUuid()
	case reConstTimestamp:
		return pkType.IsTimestamp()
	case reConstTimestamptz:
		return pkType.IsTimestamptz()
	case reConstDate:
		return pkType.IsDate()
	case reConstText:
		return pkType.IsText()
	case reConstBytea:
		return pkType.IsBytea()
	case reConstDecimal:
		return pkType.IsDecimal()
	case reConstInterval:
		return pkType.IsInterval()
	}
	return false
}

// flipCompare swaps a comparison's sense (for `const <op> pk` ⇒ `pk <flipped> const`). Eq is
// symmetric.
func flipCompare(op binaryOp) binaryOp {
	switch op {
	case opLt:
		return opGt
	case opLe:
		return opGe
	case opGt:
		return opLt
	case opGe:
		return opLe
	default:
		return op
	}
}

// buildKeyBound turns the plan-time terms into a concrete key range at exec time: encode each
// const-source in the PK key space and intersect the half-bounds. empty=true ⇒ the range admits no
// key (a NULL const — 3VL — or contradictory bounds like pk>5 AND pk<5), so the scan reads nothing
// and charges nothing. An out-of-range integer const drops only its own half-bound (a wider, still
// sound, scan), never a wrong key.
// outer carries the enclosing rows (innermost last) so a correlated reOuterColumn source resolves to
// the current outer row's value; it is nil for a top-level statement.
func (db *engine) buildKeyBound(bp *pkBoundPlan, params []Value, outer []storedRow, left storedRow) (keyBound, bool) {
	b := unboundedBound()
	for _, t := range bp.terms {
		key, isNull, ok := encodeBoundKey(bp.pkType, t.src, params, outer, bp.coll, left)
		if isNull {
			return keyBound{}, true
		}
		if !ok {
			continue
		}
		switch t.op {
		case opEq:
			b = intersectLo(b, key, true)
			b = intersectHi(b, key, true)
		case opGt:
			b = intersectLo(b, key, false)
		case opGe:
			b = intersectLo(b, key, true)
		case opLt:
			b = intersectHi(b, key, false)
		case opLe:
			b = intersectHi(b, key, true)
		}
	}
	if boundEmpty(b) {
		return keyBound{}, true
	}
	return b, false
}

// encodeBoundKey encodes a const-source's value into the PK's storage key (the same codec INSERT
// uses — EncodeInt for integer/timestamp widths, the raw 16 bytes for uuid, the 1-byte bool-byte
// for boolean). isNull ⇒ the value is NULL; ok=false (not null) ⇒ an integer value outside the PK
// type's range (no key can equal it), so the caller drops this bound. reParam/reOuterColumn resolve
// to a runtime Value first (the param table / the enclosing outer row) and then encode through the
// shared path.
func encodeBoundKey(pkType scalarType, src *rExpr, params []Value, outer []storedRow, coll *Collation, left storedRow) (key []byte, isNull bool, ok bool) {
	switch src.kind {
	case reConstNull:
		return nil, true, false
	case reConstInt:
		if !pkType.InRange(src.cInt) {
			return nil, false, false
		}
		return encodeInt(pkType, src.cInt), false, true
	case reConstBool:
		return encodeBool(src.cBool), false, true
	case reConstUuid:
		return src.cBytea, false, true
	case reConstTimestamp, reConstTimestamptz:
		return encodeInt(pkType, src.cInt), false, true
	case reConstText:
		return encodeTextBound(src.cText, coll)
	case reConstBytea:
		return encodeTerminated(src.cBytea), false, true
	case reConstDecimal:
		return src.cDec.EncodeKey(), false, true
	case reConstInterval:
		return src.cIv.EncodeKey(), false, true
	case reParam:
		return encodeValueKey(pkType, params[src.index], coll)
	case reOuterColumn:
		// A correlated reference: column index of the enclosing row level hops out — the same
		// indexing the evaluator uses for reOuterColumn (innermost outer row is last).
		return encodeValueKey(pkType, outer[len(outer)-src.level][src.index], coll)
	case reColumn:
		// Index-nested-loop: the GLOBAL column index of an earlier join relation, read from the
		// current combined left-hand row (cost.md §3 "JOIN"). The join loop always passes a `left`
		// wide enough (the running row spans columns [0, rel.offset), and a sibling index is <
		// rel.offset); a stray out-of-range index widens to a full scan rather than panic.
		if src.index >= len(left) {
			return nil, false, false
		}
		return encodeValueKey(pkType, left[src.index], coll)
	}
	return nil, false, false
}

// encodeTextBound encodes a text probe into a key bound: the raw text-terminated-escape bytes for a
// C key (coll == nil, the fast path, encoding.md §2.4), or the collation's UCA sort key
// (text-collated-sortkey, §2.12) for a Full-collated key. A sort-key build that fails on an unmapped
// code point (the 0A000 the write/compare path raises, collation.md §6) yields ok=false here: the
// probe matches no stored (always-mapped) key, so the term contributes no bound and the scan widens
// to a full scan + residual filter — which reproduces the exact non-pushdown answer (empty for =,
// since equality is byte-identity §7; the 0A000 for an ordering compare iff any row is scanned).
// Identical across cores (mirrors Rust encode_text_bound / TS encodeTextBound).
func encodeTextBound(s string, coll *Collation) (key []byte, isNull bool, ok bool) {
	if coll == nil {
		return encodeTerminated([]byte(s)), false, true
	}
	k, err := sortKey(coll, s)
	if err != nil {
		return nil, false, false
	}
	return k, false, true
}

// encodeValueKey encodes a runtime Value (a bound param or a resolved outer column) into the PK's
// storage key. isNull ⇒ the value is NULL (a 3VL-empty range); ok=false (not null) ⇒ an integer
// outside the PK width, so the caller drops this half-bound (a wider, still sound, scan). coll
// selects a text value's key form (collated sort key vs raw bytes — encodeTextBound).
func encodeValueKey(pkType scalarType, v Value, coll *Collation) (key []byte, isNull bool, ok bool) {
	if v.IsNull() {
		return nil, true, false
	}
	switch {
	case pkType.IsBool():
		return encodeBool(v.boolVal()), false, true
	case pkType.IsUuid():
		return []byte(v.str()), false, true
	case pkType.IsText():
		return encodeTextBound(v.str(), coll)
	case pkType.IsBytea():
		return encodeTerminated([]byte(v.str())), false, true
	case pkType.IsDecimal():
		if v.Kind != ValDecimal {
			return nil, false, false // mismatched param kind: drop this half-bound (sound widening)
		}
		return v.decimal().EncodeKey(), false, true
	case pkType.IsInterval():
		if v.Kind != ValInterval {
			return nil, false, false // mismatched param kind: drop this half-bound (sound widening)
		}
		return v.interval().EncodeKey(), false, true
	case pkType.IsFloat():
		// A float PK does NOT push down this slice (full-scan + residual filter, like the container
		// keys) — drop the half-bound (sound widening), matching Rust encode_value_key's OutOfRange.
		return nil, false, false
	case pkType.IsInteger():
		if !pkType.InRange(v.Int) {
			return nil, false, false
		}
		return encodeInt(pkType, v.Int), false, true
	default: // timestamp / timestamptz / date
		return encodeInt(pkType, v.Int), false, true
	}
}

// intersectLo tightens b's lower bound to the more restrictive of (current, key); at an equal key an
// exclusive bound (inc=false) wins.
func intersectLo(b keyBound, key []byte, inc bool) keyBound {
	if b.lo == nil {
		b.lo, b.loInc = key, inc
		return b
	}
	if c := bytes.Compare(key, b.lo); c > 0 || (c == 0 && !inc) {
		b.lo, b.loInc = key, inc
	}
	return b
}

// intersectHi tightens b's upper bound to the more restrictive of (current, key); at an equal key an
// exclusive bound wins.
func intersectHi(b keyBound, key []byte, inc bool) keyBound {
	if b.hi == nil {
		b.hi, b.hiInc = key, inc
		return b
	}
	if c := bytes.Compare(key, b.hi); c < 0 || (c == 0 && !inc) {
		b.hi, b.hiInc = key, inc
	}
	return b
}

// boundEmpty reports whether the bound admits no key: lo above hi, or lo == hi with a non-inclusive
// endpoint.
func boundEmpty(b keyBound) bool {
	if b.lo == nil || b.hi == nil {
		return false
	}
	switch bytes.Compare(b.lo, b.hi) {
	case 1:
		return true
	case 0:
		return !(b.loInc && b.hiInc)
	}
	return false
}

// execSelectPlan executes a resolved SELECT against an outer-row environment (outer = the
// enclosing rows, innermost last; nil at top level) and the bound parameters. The execute half
// of the old runSelect: materialize, nested-loop join, WHERE, then aggregate / DISTINCT / window
// + project. The per-row evaluator gets an evalEnv carrying the engine + outer rows, so a
// correlated subquery in any clause re-executes against them (grammar.md §26).
// execStreamingScan executes the streaming primary-key-ordered scan path (spec/design/cost.md §3):
// a single-table, no-blocking-operator query whose output order is already the table's primary-key
// scan order — either no ORDER BY (the LIMIT short-circuit) or an ORDER BY satisfied by PK order
// (plan.pkOrdered, set by orderSatisfiedByPK) — streams scan→filter→project with NO sort, and (when
// there is a LIMIT) stops the scan the instant the LIMIT/OFFSET window is filled, charging
// storage_row_read only for the rows actually read. With no LIMIT it emits every survivor after
// OFFSET (the sort is simply elided — same rows, same cost as the eager/sort path). It is
// cost-equivalent to the eager path EXCEPT that a LIMIT reads (and filters) fewer rows, which is the
// deliberate cost change. page_read is the full block (the bound's node count) — it does not
// short-circuit; only the row reads do. Rows match the eager path exactly: the offset..offset+limit
// slice of the primary-key-ordered filtered rows (which, for a pkOrdered query, IS the ORDER BY's
// result — the stored PK order is the requested order).
// streamingScanEligible reports whether plan is the single-table, no-blocking-operator STREAMING SCAN
// shape (spec/design/cost.md §3, streaming.md §4) — a single relation, no join/aggregate/window, an
// output order the primary-key scan already yields (pkOrdered, or no ORDER BY with a LIMIT
// short-circuit), no index/GIN/GiST bound (those read the full admitted set eagerly), and a real
// table store (not an SRF / CTE / derived source). Both execSelectPlan (which routes to the eager
// execStreamingScan) and tryStreamingQuery (the lazy Query lane) gate on this ONE predicate, so the
// two never drift.
func streamingScanEligible(plan *selectPlan) bool {
	return len(plan.rels) == 1 && len(plan.joins) == 0 &&
		!plan.isAgg && !plan.hasWindow &&
		(plan.pkOrdered || (!plan.distinct && len(plan.order) == 0 && plan.limit != nil)) &&
		!plan.relBounds[0].needsEagerScan() &&
		plan.rels[0].srf == nil &&
		plan.rels[0].cte == nil &&
		plan.rels[0].derived == nil
}

// windowTopNEligible reports whether a plain (non-grouped) window query can serve its LIMIT with a
// TOP-N over the primary-key scan — reading only the first OFFSET+LIMIT rows instead of the whole
// table (spec/design/window.md §5.2 "windowed top-N", cost.md §3). It is the window analog of the
// streaming-scan LIMIT short-circuit above, sound only when every window value at scan position k
// depends solely on rows at positions <= k (a "backward" window over the scan order): then the first
// OFFSET+LIMIT scan rows determine the first OFFSET+LIMIT output rows exactly.
//
// The gate (all must hold): a single base-table full/PK-bounded scan (no join/SRF/CTE/derived, no
// index/GIN/GiST bound — those read the full admitted set), a plain window (`hasWindow && !isAgg`),
// not DISTINCT, a LIMIT present, and an outer ORDER BY the PK scan already satisfies (`pkOrdered`, so
// the scan order IS the output order and no post-window sort reorders rows). No compound
// (materialized) window key (windowKeys) and no general-expression ORDER BY (orderExprs) — those
// append synthetic columns; a bare PK-column window is the shape that streams. Finally EVERY window
// spec must be prefix-safe (windowSpecPrefixSafe). Rows are byte-identical to the eager path; only
// the accrued cost drops (fewer rows scanned/folded), the deliberate cost change (like the streaming
// LIMIT short-circuit — cross-core identical because every core caps at the same OFFSET+LIMIT).
func (db *engine) windowTopNEligible(plan *selectPlan) bool {
	if !plan.hasWindow || plan.isAgg || plan.distinct || plan.limit == nil || !plan.pkOrdered {
		return false
	}
	if len(plan.rels) != 1 || len(plan.joins) != 0 {
		return false
	}
	rel := plan.rels[0]
	if rel.srf != nil || rel.cte != nil || rel.derived != nil {
		return false
	}
	if plan.relBounds[0].needsEagerScan() {
		return false
	}
	if len(plan.windowKeys) != 0 || len(plan.orderExprs) != 0 {
		return false
	}
	table, ok := db.lkpTableScoped(rel.db, rel.tableName)
	if !ok {
		return false
	}
	for i := range plan.windowSpecs {
		if !db.windowSpecPrefixSafe(&plan.windowSpecs[i], plan, table, rel.offset) {
			return false
		}
	}
	return true
}

// windowSpecPrefixSafe reports whether one window function's value at scan position k depends solely
// on rows at positions <= k, so truncating the input to the first OFFSET+LIMIT rows is exact
// (spec/design/window.md §5.2). It requires: no PARTITION BY (the whole scan is one partition, so
// scan order = partition order); a window ORDER BY the PRIMARY KEY satisfies in the SAME direction as
// the outer pkOrdered scan (so the window's "preceding" is the scan's preceding — the sort is a
// no-op); and a backward plan/frame.
//
//   - row_number / rank / dense_rank / lag → backward (position, earlier-peer count, or a look-BACK
//     offset); never depend on later rows or the total partition size.
//   - an aggregate / first_value / last_value / nth_value window → backward iff its FRAME does not
//     look forward (frameBackwardSafe): the frame END must be UNBOUNDED PRECEDING / PRECEDING /
//     CURRENT ROW, and a RANGE/GROUPS CURRENT-ROW end (which spans the current PEER GROUP) is safe
//     only when the ordering key is unique (the full PK) so a peer group is a single row.
//   - percent_rank / cume_dist / ntile depend on the total partition size N; lead looks FORWARD —
//     all rejected.
func (db *engine) windowSpecPrefixSafe(spec *windowSpec, plan *selectPlan, table *catTable, offset int) bool {
	if len(spec.partition) != 0 || len(spec.order) == 0 {
		return false
	}
	ok, rev := db.orderSatisfiedByPK(table, offset, spec.order)
	if !ok || rev != plan.pkReverse {
		return false
	}
	unique := len(spec.order) >= len(table.PKIndices()) // order covers the full (unique) PK ⇒ singleton peer groups
	switch spec.plan {
	case planRowNumber, planRank, planDenseRank, planLag:
		return true
	case planAgg, planFirstValue, planLastValue, planNthValue:
		return frameBackwardSafe(spec.frame, unique)
	default:
		return false // planPercentRank, planCumeDist, planNtile (need N), planLead (looks forward)
	}
}

// frameBackwardSafe reports whether a frame folds only rows at or before the current row in the scan
// order (spec/design/window.md §5.2/§6). The frame END must not look forward; a RANGE/GROUPS
// CURRENT-ROW end spans the current peer group, which pulls in later rows unless the ordering key is
// unique. A ROWS frame uses physical position, so it never expands to peers. The default frame (nil,
// with a window ORDER BY) is RANGE UNBOUNDED PRECEDING TO CURRENT ROW — safe only when the key is
// unique.
func frameBackwardSafe(frame *resolvedFrame, unique bool) bool {
	if frame == nil {
		return unique
	}
	switch frame.end.kind {
	case boundUnboundedPreceding, boundPreceding:
		return true // strictly before the current peer group
	case boundCurrentRow:
		return frame.mode == frameRows || unique // ROWS = the physical row; RANGE/GROUPS = the peer group
	default:
		return false // boundFollowing / boundUnboundedFollowing look forward
	}
}

// snapshotEngine builds a frozen read-snapshot engine for a streaming cursor (spec/design/streaming.md
// §5): the VISIBLE main / session-temp snapshots captured (the snapshots are immutable
// copy-on-write, so sharing the pointers pins the roots cheaply and keeps them stable for the cursor's
// life, isolated from later writes on the live handle) with NO open transaction — so the engine's
// reads see exactly the captured frozen state — plus the session envelope the per-row eval / the cost
// meter read: the cost ceilings + the SHARED lifetime gauge (the *int64 pointer — so streaming cost
// still counts against LifetimeMaxCost), the cancel poll, the entropy/clock seam, session vars, the
// time zone, and the currval/lastval session state. The cursor evaluates its filter/projection against
// this engine, so the streaming Rows is self-contained (it does not reference the live handle, so it
// survives Database.queryValues's transient session, streaming.md §5).
func (db *engine) snapshotEngine() *engine {
	s := db.session // struct copy: shares the seam (func fields), the lifetime gauge (pointer), and the
	// read-only maps (vars/sessionSeq); reset the per-statement / transaction state below.
	s.tx = nil
	s.readPin = nil
	s.pendingSeq = map[string]*sequenceDef{}
	s.pendingCurrval = map[string]int64{}
	s.pendingLastName = ""
	s.tempCommitted = db.tempSnap()
	return &engine{
		committed: db.readSnap(),
		session:   s,
		pageSize:  db.pageSize,
		paging:    db.paging,
		readOnly:  db.readOnly,
		// The frozen read engine carries the same pinned attachment view so a streaming read of an
		// attached database (attached-databases.md §5) resolves through it; it never commits (read-only),
		// so it needs no core back-ref. tempCommitted above already froze the temp snapshot.
		attachedCommitted: db.attachReadView(),
	}
}

// scanCache is one immutable filled entry of a prepared statement's plan cache (stmtCache): the
// resolved scan plan + finalized param types, stamped with the Database (sharedCore) and committed
// catalog generation they were resolved against. Built once, published via stmtCache.p, and never
// mutated after — so a concurrent reader sees a complete entry or none.
type scanCache struct {
	// core identifies the Database the plan was resolved against. catGen is only monotonic within
	// one core — two Databases can share a generation number with different schemas — so a hit
	// requires the same core AND the same generation (spec/design/api.md §2.4).
	core   *sharedCore
	catGen uint64
	sp     *selectPlan
	ptys   []scalarType
}

// stmtCache memoizes a prepared statement's resolved scan plan + finalized param types so a repeated
// execute skips planning entirely (spec/design/api.md §2.4) — the biggest lever for the point-lookup
// / high-frequency class (planning is ~⅔ of a point lookup's latency and ~88% of its allocations, and
// the resulting GC inflates the tail). An entry is valid only for the Database it was resolved
// against and only while catGen still equals that core's committed catalog generation; any DDL bumps
// catGen and the next execute re-plans. Filled only from committed state and only for a reusable plan
// (planCacheable + !paramTypes.uncacheable), so reusing it is result/cost-identical to a fresh plan.
// Zero value is "empty". The slot is a lock-free atomic pointer: a prepared statement is a standalone
// value shared across sessions — and goroutines — so concurrent executes may race to fill it; the
// entry itself is immutable and last-writer-wins (both candidates are correct for their core+catGen;
// a statement bounced between databases or a pinned-vs-current session merely re-plans).
type stmtCache struct {
	p atomic.Pointer[scanCache]
}

// planCacheable reports whether a resolved scan plan may be memoized on a prepared statement. The
// subquery / precompiled-regex exclusion is tracked separately (paramTypes.uncacheable, set at the
// node's birth — a folded uncorrelated subquery bakes in one execution's params, and a precompiled
// regex carries a per-execution cost flag). Here the relations are vetted: a set-returning / CTE /
// derived relation carries a nested plan or generator we do not vet for reuse, and a temp table lives
// in a snapshot the cache key (committed.catGen) does not track — so a plan referencing any of those
// is never cached (a point lookup / plain join over persistent base tables has none).
func (db *engine) planCacheable(sp *selectPlan) bool {
	for i := range sp.rels {
		r := &sp.rels[i]
		if r.srf != nil || r.cte != nil || r.derived != nil {
			return false
		}
	}
	return !db.planTouchesTemp(sp)
}

// planTouchesTemp reports whether any of the plan's relations currently resolves to a SESSION-LOCAL
// temporary table in THIS session's visible temp domain. Checked at cache fill (a temp plan is never
// cached) and re-checked on every cache HIT: a statement is shared across sessions, and a plan cached
// where a name was persistent must not be served on a session whose temp table shadows that name —
// the temp domain is session-local, so the committed catGen the cache is keyed on cannot see it.
// Cheap: one map lookup per relation, against a usually-empty temp catalog.
func (db *engine) planTouchesTemp(sp *selectPlan) bool {
	for i := range sp.rels {
		if db.isTempTable(sp.rels[i].tableName) {
			return true
		}
	}
	return false
}

// tryScanQuery serves stmt as a lazy STREAMING or BUFFERED query (spec/design/streaming.md §3/§4),
// planning it EXACTLY ONCE and classifying streaming-vs-buffered from that single plan — the
// plan-once replacement for the old tryStreamingQuery + tryBufferedQuery pair, each of which
// re-planned the same statement. Returns (rows, true, nil) for a top-level read SELECT; (nil, false,
// nil) for a shape no scan lane covers (a non-SELECT, a write — incl. a nextval/setval SELECT,
// stmtIsWrite — or a top-level set-op / VALUES / WITH), so the caller falls through to the deferred /
// materialized paths. When sc is non-nil (a prepared statement) a repeated execute over an unchanged
// catalog reuses the cached plan and skips planning + the fold; ad-hoc callers pass nil and still
// plan exactly once. The conformance corpus drives this lazy lane for every read (the harness routes
// through queryValues), cross-checked to yield identical rows + total cost as the materialized drive
// under full drain (streaming.md §6).
func (db *engine) tryScanQuery(stmt statement, params []Value, sc *stmtCache) (*Rows, bool, error) {
	if stmt.Select == nil || stmtIsWrite(stmt) {
		return nil, false, nil
	}
	// Cache HIT: the plan was resolved against THIS Database (same core — catGen is only monotonic
	// within one core) and the read snapshot's catalog is unchanged since (its catGen still matches),
	// and no relation name is shadowed by a session-local temp table the committed generation cannot
	// see (planTouchesTemp — the statement may have been filled on a different session). Reuse the
	// resolved plan + finalized param types — no planQuery, no fold, no param-type walk. A cached plan
	// carries no subquery to fold (planCacheable rejected any), so the shared plan is never mutated;
	// params are still bound per execute inside buildScanRows.
	rsnap := db.readSnap()
	if sc != nil {
		if c := sc.p.Load(); c != nil && c.core == db.core && c.catGen == rsnap.catGen && !db.planTouchesTemp(c.sp) {
			return db.buildScanRows(c.sp, c.ptys, params, false)
		}
	}
	// MISS: plan once.
	ptypes := &paramTypes{}
	plan, err := db.planQuery(queryExpr{Select: stmt.Select}, nil, nil, ptypes)
	if err != nil {
		return nil, false, err
	}
	if plan.sel == nil {
		return nil, false, nil // set-op / VALUES / WITH — a scan lane does not cover it
	}
	sp := plan.sel
	ptys, err := ptypes.finalize()
	if err != nil {
		return nil, false, err
	}
	// Fill the cache only from committed state — so committed.catGen is strictly increasing over the
	// core's life and never aliases a rolled-back working generation, making the catGen equality on
	// a later HIT a sound "same catalog" identity check (a statement first executed inside an open
	// transaction re-plans until the tx commits) — and only for a reusable plan, and only when this
	// engine belongs to a core (the entry's identity key; a core-less engine never fills).
	if sc != nil && db.core != nil && rsnap == db.committed && !ptypes.uncacheable && db.planCacheable(sp) {
		sc.p.Store(&scanCache{core: db.core, catGen: rsnap.catGen, sp: sp, ptys: ptys})
	}
	return db.buildScanRows(sp, ptys, params, true)
}

// buildScanRows binds params, optionally folds uncorrelated subqueries to constants (doFold — done on
// a freshly-planned plan, skipped for a cached one which has no subquery), classifies the plan as
// streaming or buffered via streamingScanEligible, and returns the matching lazy cursor. When doFold
// is false, sp is a shared cached plan and stays strictly read-only. The streaming branch resolves
// the PK scan bound + the up-front cost block; the buffered branch runs its blocking part lazily on
// the first pull. Under full drain the rows + total cost are byte-identical to the eager path.
func (db *engine) buildScanRows(sp *selectPlan, ptys []scalarType, params []Value, doFold bool) (*Rows, bool, error) {
	bound, err := bindParams(params, ptys)
	if err != nil {
		return nil, false, err
	}
	// Fold globally-uncorrelated subqueries to constants (at top level every surviving subquery is
	// uncorrelated) so the per-row eval re-enters nothing — keeping the cursor self-contained. The
	// fold's own cost was already charged to the shared lifetime gauge by its sub-executions; it is
	// added to the cursor's reported cost below (mirroring runQueryExpr's r.cost += …). A cached plan
	// has no subquery, so the fold is skipped (it would be a no-op anyway) — cost stays identical.
	var subqueryCost int64
	if doFold {
		if err := db.foldUncorrelatedInSelect(sp, bound, cteCtx{}, &subqueryCost); err != nil {
			return nil, false, err
		}
	}

	if streamingScanEligible(sp) {
		// Resolve the scan bound (the PK pushdown, if any) and the up-front cost block. An empty bound
		// (e.g. pk = NULL) admits no row.
		b := unboundedBound()
		empty := false
		if sp.relBounds[0] != nil && sp.relBounds[0].pk != nil {
			b, empty = db.buildKeyBound(sp.relBounds[0].pk, bound, nil, nil)
		}
		snap := db.snapshotEngine()
		store := snap.lkpStoreScoped(sp.rels[0].db, sp.rels[0].tableName)
		overlap, slabs := 0, 0
		if !empty {
			if overlap, slabs, err = store.OverlapScanUnits(b, sp.relMasks[0]); err != nil {
				return nil, false, err
			}
		}
		meter := snap.session.newMeter()
		meter.Accrued = subqueryCost // the folded constant cost (lifetime already charged)
		meter.Charge(costs.PageRead*int64(overlap) + costs.ValueDecompress*int64(slabs))

		var offset int64
		if sp.offset != nil {
			offset = *sp.offset
		}
		cur := &streamingCursor{
			eng:      snap,
			plan:     sp,
			env:      &evalEnv{exec: snap, params: bound, outer: nil, rng: newStmtRng(), ctes: cteCtx{}},
			meter:    meter,
			offset:   offset,
			limit:    sp.limit,
			distinct: sp.distinct,
			seen:     make(map[string]bool),
			done:     empty || (sp.limit != nil && *sp.limit == 0),
		}
		if !cur.done {
			cur.scan = store.storeScan(b, sp.pkReverse)
		}
		return &Rows{columnNames: sp.columnNames, columnTypes: typeNames(sp.columnTypes), cursor: cur}, true, nil
	}

	// Blocking (buffered) shape: buffers its input but yields the output one row at a time via
	// bufferedScanCursor — bounding peak output memory and letting a caller's early exit skip the
	// projection of rows it never pulls (the top-N-over-the-buffer win, streaming.md §4).
	snap := db.snapshotEngine()
	meter := snap.session.newMeter()
	meter.Accrued = subqueryCost // the folded constant cost (lifetime already charged)
	c := &bufferedScanCursor{
		eng:    snap,
		plan:   sp,
		params: bound,
		rng:    newStmtRng(),
		meter:  meter,
	}
	return &Rows{columnNames: sp.columnNames, columnTypes: typeNames(sp.columnTypes), cursor: c}, true, nil
}

// streamingCursor is the lazy pull pipeline behind a streaming Rows cursor (spec/design/streaming.md
// §3/§4, S3): execStreamingScan's per-row loop turned inside out so the CALLER pulls each row. It
// holds a frozen snapshot engine (eval's exec — so the cursor is self-contained and outlives the
// handle, streaming.md §5), a pull storeScan over that snapshot (the scan pin), the resolved + folded
// plan, an evalEnv, and its own cost meter. Each nextRow runs scan → resolve touched columns → WHERE →
// project for ONE output row, accruing the identical cost units at the identical sites as the eager
// path — so a fully-drained streaming query observes the same rows + total cost (streaming.md §6),
// while a caller that stops early reads (and charges) less.
type streamingCursor struct {
	eng      *engine
	plan     *selectPlan
	env      *evalEnv
	meter    *costMeter
	scan     *storeScan
	offset   int64
	limit    *int64
	distinct bool
	seen     map[string]bool
	passed   int64 // survivors past the filter+dedup so far (OFFSET runs against this)
	produced int64 // output rows produced so far (the LIMIT short-circuit runs against this)
	done     bool  // scan exhausted, LIMIT window full, or empty bound — then nextRow is a no-op
}

func (c *streamingCursor) nextRow() ([]Value, bool, error) {
	if c.done {
		return nil, false, nil
	}
	// The LIMIT short-circuit: once the window is full, stop WITHOUT pulling another row — so no
	// further leaf is faulted (the streaming early-exit win; cost.md §3 "LIMIT short-circuit").
	if c.limit != nil && c.produced >= *c.limit {
		c.done = true
		return nil, false, nil
	}
	for {
		_, row, ok, err := c.scan.next()
		if err != nil {
			return nil, false, err
		}
		if !ok {
			c.done = true
			return nil, false, nil
		}
		if err := c.meter.Guard(); err != nil { // enforce the cost ceiling / cancellation per scanned row
			return nil, false, err
		}
		c.meter.Charge(costs.StorageRowRead)
		// Materialize the touched columns left unfetched by the lazy load (large-values.md §14); the
		// chain reads were already metered in the up-front block (cost.md §3).
		row, err = c.scan.resolveColumns(row, c.plan.relMasks[0])
		if err != nil {
			return nil, false, err
		}
		if c.plan.filter != nil {
			v, err := c.plan.filter.eval(row, c.env, c.meter)
			if err != nil {
				return nil, false, err
			}
			if !v.IsTrue() {
				continue
			}
		}
		if c.distinct {
			// DISTINCT (cost.md §3): project EVERY scanned filtered row (the dedup key, charged even
			// for a duplicate — the §3 asymmetry), drop a value already seen, then OFFSET/LIMIT window
			// the survivors — exactly execStreamingScan.
			projected := make([]Value, len(c.plan.projections))
			for i, p := range c.plan.projections {
				v, err := p.eval(row, c.env, c.meter)
				if err != nil {
					return nil, false, err
				}
				projected[i] = v
			}
			if key := distinctRowKey(projected); c.seen[key] {
				continue
			} else {
				c.seen[key] = true
			}
			c.passed++
			if c.passed <= c.offset {
				continue
			}
			c.meter.Charge(costs.RowProduced)
			c.produced++
			return projected, true, nil
		}
		c.passed++
		if c.passed <= c.offset {
			continue
		}
		c.meter.Charge(costs.RowProduced)
		projected := make([]Value, len(c.plan.projections))
		for i, p := range c.plan.projections {
			v, err := p.eval(row, c.env, c.meter)
			if err != nil {
				return nil, false, err
			}
			projected[i] = v
		}
		c.produced++
		return projected, true, nil
	}
}

func (c *streamingCursor) costAccrued() int64 { return c.meter.Accrued }

// close marks the cursor done; the pinned snapshot is owned by eng/scan and reclaimed by the GC, and
// the watermark deregister (if any) lives on the Rows (streaming.md §5). Idempotent.
func (c *streamingCursor) close() { c.done = true }

// tryBufferedQuery tries to serve stmt as a lazy BUFFERED query (spec/design/streaming.md §4, S4) — the
// bufferedScanCursor is the lazy BUFFERED pull pipeline behind a Query Rows cursor for a plan with a
// blocking operator (spec/design/streaming.md §4, S4) — the generalization of the spilling sorter's
// pull iterator to every blocking shape. It owns a frozen snapshot engine (eval's exec — so the cursor
// is self-contained and outlives the handle, streaming.md §5), the resolved + folded plan, bound
// params, a per-statement entropy cell, its own cost meter, and the lazy emission state. On its FIRST
// nextRow it runs the blocking part (execSelectEmit) to completion into an emitter — buffering the input
// (correctly: a sort/group/dedup/join must see it all) and charging the scan/sort/group/dedup cost —
// then yields its buffer ONE row at a time: an emitProject row is projected (and charges row_produced +
// projection) on emission, an emitSorted row is pulled from the `sorted` iterator and projected (the
// streaming-sort output, streaming.md §4/§7), an emitIdentity/emitFinal row is handed out (already
// projected). So peak output memory is one row, a caller's early exit skips the projection of the rows
// it never pulls, and a fully-drained query observes the same rows + total cost as the eager path
// (streaming.md §6).
type bufferedScanCursor struct {
	eng    *engine
	plan   *selectPlan
	params []Value
	rng    *stmtRng
	meter  *costMeter
	ran    bool    // has the blocking part run? (it runs on the first nextRow)
	em     emitter // the emission descriptor, valid once ran
	idx    int64   // next row index: [em.start, em.end) for buffer modes, [0, len) for emitFinal
	done   bool    // exhausted or closed — then nextRow is a no-op
}

func (c *bufferedScanCursor) nextRow() ([]Value, bool, error) {
	if c.done {
		return nil, false, nil
	}
	// Run the blocking part on the FIRST pull (streaming.md §4 — a blocking cursor runs the blocking
	// part then yields its buffer lazily). A mid-blocking cost abort / cancellation / trap surfaces HERE
	// (during iteration), not at Query time (streaming.md §6).
	if !c.ran {
		em, err := c.eng.execSelectEmit(c.plan, nil, c.params, cteCtx{}, c.rng, c.meter)
		if err != nil {
			return nil, false, err
		}
		c.em = em
		c.ran = true
		if em.mode != emitFinal {
			c.idx = em.start
		}
	}
	switch c.em.mode {
	case emitFinal:
		// Already projected + charged — hand the next row out (no further cost).
		if c.idx >= int64(len(c.em.final)) {
			c.done = true
			return nil, false, nil
		}
		row := c.em.final[c.idx]
		c.idx++
		return row, true, nil
	case emitSorted:
		// The streaming sort's lazy output: pull the next windowed row, charge row_produced, and
		// project it (streaming.md §4/§7). The output slice is never built; an early exit (close)
		// releases any undrained spill runs.
		if c.idx >= c.em.end {
			c.done = true
			c.em.sorted.close()
			return nil, false, nil
		}
		row, ok, err := c.em.sorted.next()
		if err != nil {
			return nil, false, err
		}
		if !ok {
			c.done = true
			c.em.sorted.close()
			return nil, false, nil
		}
		c.idx++
		if err := c.meter.Guard(); err != nil { // enforce the cost ceiling / cancellation per produced row
			return nil, false, err
		}
		c.meter.Charge(costs.RowProduced)
		env := &evalEnv{exec: c.eng, params: c.params, outer: nil, rng: c.rng, ctes: cteCtx{}}
		projected := make([]Value, len(c.plan.projections))
		for i, p := range c.plan.projections {
			v, perr := p.eval(row, env, c.meter)
			if perr != nil {
				return nil, false, perr
			}
			projected[i] = v
		}
		return projected, true, nil
	case emitIdentity:
		// Pre-projected (the DISTINCT dedup) — charge only row_produced per emitted row.
		if c.idx >= c.em.end {
			c.done = true
			return nil, false, nil
		}
		row := c.em.final[c.idx]
		c.idx++
		if err := c.meter.Guard(); err != nil { // enforce the cost ceiling / cancellation per produced row
			return nil, false, err
		}
		c.meter.Charge(costs.RowProduced)
		return row, true, nil
	case emitColumnar:
		// Columnar projection (packed-leaf.md §11 Track A2/A3): gather this row from the dense lanes — a
		// bare-column projection with no full-width row — charging only row_produced (a bare column ref
		// evaluates to a zero-cost slot read, so there is no projection operator_eval to charge). A non-nil
		// `sel` (the A3 filter's survivors) maps output row j to lane position sel[j].
		if c.idx >= c.em.end {
			c.done = true
			return nil, false, nil
		}
		j := c.idx
		c.idx++
		if err := c.meter.Guard(); err != nil { // enforce the cost ceiling / cancellation per produced row
			return nil, false, err
		}
		c.meter.Charge(costs.RowProduced)
		li := j
		if c.em.sel != nil {
			li = int64(c.em.sel[j])
		}
		projected := make([]Value, len(c.em.projCols))
		for k, cc := range c.em.projCols {
			projected[k] = c.em.cols[cc][li]
		}
		return projected, true, nil
	default: // emitProject — project the buffer row on emission (charging row_produced + projection)
		if c.idx >= c.em.end {
			c.done = true
			return nil, false, nil
		}
		row := c.em.src[c.idx]
		c.idx++
		if err := c.meter.Guard(); err != nil { // enforce the cost ceiling / cancellation per produced row
			return nil, false, err
		}
		c.meter.Charge(costs.RowProduced)
		env := &evalEnv{exec: c.eng, params: c.params, outer: nil, rng: c.rng, ctes: cteCtx{}}
		projected := make([]Value, len(c.plan.projections))
		for i, p := range c.plan.projections {
			v, perr := p.eval(row, env, c.meter)
			if perr != nil {
				return nil, false, perr
			}
			projected[i] = v
		}
		return projected, true, nil
	}
}

func (c *bufferedScanCursor) costAccrued() int64 { return c.meter.Accrued }

// close marks the cursor done; the pinned snapshot is owned by eng and reclaimed by the GC, and the
// watermark deregister (if any) lives on the Rows (streaming.md §5). A `Sorted` emitter additionally
// releases any undrained spill run files (Go has no destructor — streaming.md §5). Idempotent.
func (c *bufferedScanCursor) close() {
	c.done = true
	if c.ran && c.em.mode == emitSorted && c.em.sorted != nil {
		c.em.sorted.close()
	}
}

// tryDeferredQuery tries to serve stmt as a lazy DEFERRED query (spec/design/streaming.md §4/§7) — the
// Query path for a top-level set operation (UNION/INTERSECT/EXCEPT) or pure-query WITH. These are
// blocking shapes whose output is already projected AND charged (no per-row top-level projection to
// defer), so the only streaming win is lazy-yield (streaming.md §7): the cursor defers the whole
// runSetOp / runWith to its FIRST pull — so a 54P01 cost abort, a 54P02 lifetime abort, a canceled
// context, or an arithmetic trap surfaces during iteration, not at Query (§6) — then yields the
// buffered result one row at a time over a frozen snapshot (§5). Returns (nil,false,nil) for any
// non-set-op/WITH statement, or a write-classified one (a data-modifying WITH, a nextval/setval call —
// stmtIsWrite), which falls back to the materialized dispatch path. Under full drain the rows + total
// cost are byte-identical to the materialized drive (it drives the SAME runSetOp / runWith, §6), so the
// corpus — which drives the total queryValues seam — stays green by construction; per-core unit tests
// pin the lazy drive == the materialized drive.
func (db *engine) tryDeferredQuery(stmt statement, params []Value) (*Rows, bool, error) {
	// A write-classified statement (a data-modifying WITH, a sequence mutator) must take the write gate
	// and never streams (streaming.md §7 / sequences.md §4).
	if stmtIsWrite(stmt) {
		return nil, false, nil
	}
	var q deferredQuery
	switch {
	case stmt.SetOp != nil:
		q = deferredQuery{setop: stmt.SetOp}
	case stmt.With != nil:
		q = deferredQuery{with: stmt.With}
	default:
		return nil, false, nil
	}
	// Resolve the output column metadata up front (the Rows cursor exposes it before the first pull).
	// Planning is unmetered + deterministic, so the names/types read here are IDENTICAL to what the
	// deferred run produces (the run reuses runSetOp/runWith verbatim, so there is no rows/cost drift).
	// A planning error (42P01/42804/…) surfaces at Query, matching the eager path.
	names, types, err := db.deferredColumnMeta(stmt)
	if err != nil {
		return nil, false, err
	}
	c := &deferredCursor{eng: db.snapshotEngine(), query: q, params: params}
	return &Rows{columnNames: names, columnTypes: types, cursor: c}, true, nil
}

// deferredColumnMeta resolves the output column names + type names of a top-level set operation /
// pure-query WITH by planning only (no execution) — fills a deferredCursor's Rows metadata before its
// first pull (tryDeferredQuery). Mirrors the planning prefix of runSetOp / runWith exactly so the
// metadata matches the deferred run's. Bound params are not needed: column names/types never depend on
// bound values.
func (db *engine) deferredColumnMeta(stmt statement) ([]string, []string, error) {
	ptypes := &paramTypes{}
	var plan queryPlan
	var err error
	switch {
	case stmt.SetOp != nil:
		plan, err = db.planQuery(queryExpr{SetOp: stmt.SetOp}, nil, nil, ptypes)
	case stmt.With != nil:
		// The planning prefix of runWith (cte.md): plan the CTE bindings, then the body with them
		// visible. The body's columns are the WITH's output columns.
		var bindings []*cteBinding
		bindings, err = db.planCteBindings(stmt.With.Ctes, stmt.With.Recursive, ptypes)
		if err == nil {
			bodyQ := stmt.With.Body.AsQuery() // pure-query WITH (DML excluded by stmtIsWrite)
			plan, err = db.planQuery(*bodyQ, nil, bindings, ptypes)
		}
	default:
		return nil, nil, nil // unreachable: tryDeferredQuery only calls this for SetOp / With
	}
	if err != nil {
		return nil, nil, err
	}
	return plan.columnNames(), typeNames(plan.columnTypes()), nil
}

// deferredQuery is the deferred query payload for a top-level set operation / pure-query WITH (exactly
// one field is set). Run via the eager runSetOp / runWith verbatim so the rows + cost match Execute
// exactly (streaming.md §6).
type deferredQuery struct {
	setop *setOp
	with  *withQuery
}

// deferredCursor is the lazy DEFERRED pull pipeline behind a Query Rows cursor for a top-level set
// operation / pure-query WITH (spec/design/streaming.md §7). It owns a frozen snapshot engine (§5), the
// query AST, and the bound params; on its FIRST nextRow it runs the whole runSetOp / runWith to
// completion (so a cost abort / cancellation / trap surfaces during iteration, not at Query — §6),
// records the accrued cost, and yields the materialized result ONE row at a time. The input is still
// buffered (a set op dedups / a WITH materializes — it must), so the win here is only lazy-yield: the
// work is deferred to the first pull and the result rows are handed out incrementally rather than
// wrapped in an eager outcome. Under full drain the rows + total cost are byte-identical to the eager
// path (it drives the SAME runSetOp / runWith, §6).
type deferredCursor struct {
	eng    *engine
	query  deferredQuery
	params []Value
	ran    bool      // has the query run? (it runs on the first nextRow)
	rows   [][]Value // the materialized result, valid once ran
	idx    int
	cost   int64 // 0 until the first pull runs the query, then selectResult.cost (final)
	done   bool  // exhausted or closed — then nextRow is a no-op
}

func (c *deferredCursor) nextRow() ([]Value, bool, error) {
	if c.done {
		return nil, false, nil
	}
	// Run the whole set op / WITH on the FIRST pull (streaming.md §7), reusing the eager runSetOp /
	// runWith verbatim so the rows + cost match Execute exactly. A mid-run cost abort / cancellation /
	// arithmetic trap surfaces HERE (during iteration), not at Query (streaming.md §6).
	if !c.ran {
		var r selectResult
		var err error
		if c.query.setop != nil {
			r, err = c.eng.runSetOp(c.query.setop, c.params)
		} else {
			r, err = c.eng.runWith(c.query.with, c.params)
		}
		if err != nil {
			return nil, false, err
		}
		c.rows = r.rows
		c.cost = r.cost
		c.ran = true
	}
	if c.idx >= len(c.rows) {
		c.done = true
		return nil, false, nil
	}
	row := c.rows[c.idx]
	c.idx++
	return row, true, nil
}

func (c *deferredCursor) costAccrued() int64 { return c.cost }

// close marks the cursor done + drops any unread rows; the frozen snapshot is owned by eng
// (GC-reclaimed) and the watermark deregister lives on the Rows (streaming.md §5). Idempotent.
func (c *deferredCursor) close() {
	c.done = true
	c.rows = nil
}

func (db *engine) execStreamingScan(plan *selectPlan, env *evalEnv, meter *costMeter, params []Value) (selectResult, error) {
	store := db.lkpStoreScoped(plan.rels[0].db, plan.rels[0].tableName)

	// Resolve the scan bound (the PK pushdown, if any) and charge the page_read block. A correlated
	// bound resolves against env.outer (the enclosing rows).
	// This path is single-table (gated below), so the only relation is relBounds[0].
	// An INDEX bound never streams — the dispatch gate routes it to the eager path
	// (cost.md §3 "LIMIT short-circuit").
	b := unboundedBound()
	empty := false
	overlap, slabs := 0, 0
	if plan.relBounds[0] != nil && plan.relBounds[0].pk != nil {
		b, empty = db.buildKeyBound(plan.relBounds[0].pk, params, env.outer, nil)
	}
	if !empty {
		var err error
		if overlap, slabs, err = store.OverlapScanUnits(b, plan.relMasks[0]); err != nil {
			return selectResult{}, err
		}
	}
	meter.Charge(costs.PageRead*int64(overlap) + costs.ValueDecompress*int64(slabs))

	// limit is optional here: a pkOrdered query may have no LIMIT (it streams every survivor in
	// order, eliding the sort), while the LIMIT short-circuit always has one.
	var offset int64
	if plan.offset != nil {
		offset = *plan.offset
	}
	out := make([][]Value, 0)
	// DISTINCT (cost.md §3): when the scan already yields ORDER BY order, dedup runs streaming —
	// project EVERY scanned filtered row (the dedup key), drop a value already in `seen` keeping the
	// first (scan-order) occurrence, then the LIMIT/OFFSET window the DISTINCT rows. The sort is
	// elided; the projection is charged per scanned filtered row (the §3 asymmetry).
	seen := make(map[string]bool)
	// Skip the scan entirely for LIMIT 0 (no window to fill).
	if !empty && (plan.limit == nil || *plan.limit > 0) {
		var passed int64
		// A pkReverse plan (ORDER BY the full PK all-DESC) walks the tree backward; everything else
		// (forward pkOrdered, or the no-ORDER-BY LIMIT short-circuit) walks forward.
		visit := func(_ []byte, row storedRow) (bool, error) {
			if err := meter.Guard(); err != nil { // enforce the cost ceiling per scanned row (CLAUDE.md §13)
				return false, err
			}
			meter.Charge(costs.StorageRowRead)
			// Materialize the touched columns if the lazy load left them unfetched
			// (large-values.md §14) — a fresh copy only when needed (resolveColumns).
			row, err := store.resolveColumns(row, plan.relMasks[0])
			if err != nil {
				return false, err
			}
			if plan.filter != nil {
				v, err := plan.filter.eval(row, env, meter)
				if err != nil {
					return false, err
				}
				if !v.IsTrue() {
					return true, nil
				}
			}
			if plan.distinct {
				// Project per scanned filtered row (the dedup key) and drop duplicates by first
				// occurrence; the OFFSET/LIMIT then window the DISTINCT rows.
				projected := make([]Value, len(plan.projections))
				for i, p := range plan.projections {
					v, err := p.eval(row, env, meter)
					if err != nil {
						return false, err
					}
					projected[i] = v
				}
				if key := distinctRowKey(projected); seen[key] {
					return true, nil // a duplicate of an already-emitted/seen value
				} else {
					seen[key] = true
				}
				passed++
				if passed <= offset {
					return true, nil
				}
				meter.Charge(costs.RowProduced)
				out = append(out, projected)
			} else {
				passed++
				if passed <= offset {
					return true, nil
				}
				meter.Charge(costs.RowProduced)
				projected := make([]Value, len(plan.projections))
				for i, p := range plan.projections {
					v, err := p.eval(row, env, meter)
					if err != nil {
						return false, err
					}
					projected[i] = v
				}
				out = append(out, projected)
			}
			// Stop once a LIMIT window is filled; with no LIMIT, never stop early (emit every
			// survivor after OFFSET, in primary-key scan order).
			if plan.limit != nil {
				return int64(len(out)) < *plan.limit, nil
			}
			return true, nil
		}
		var err error
		if plan.pkReverse {
			err = store.ScanRangeRev(b, visit)
		} else {
			err = store.ScanRange(b, visit)
		}
		if err != nil {
			return selectResult{}, err
		}
	}
	return selectResult{columnNames: plan.columnNames, columnTypes: plan.columnTypes, rows: out, cost: meter.Accrued}, nil
}

// execWindowTopN serves a windowed top-N (spec/design/window.md §5.2, cost.md §3): a plain window
// query whose LIMIT is answerable from the first OFFSET+LIMIT primary-key-scan rows (the gate is
// windowTopNEligible). It streams the PK scan, applies WHERE, and collects survivors until it has
// OFFSET+LIMIT of them — then runs the ordinary window stage over that PREFIX and emits the
// OFFSET..OFFSET+LIMIT slice. Because every window value at scan position k depends only on rows at
// positions <= k (windowSpecPrefixSafe), and the outer ORDER BY is the PK scan order (pkOrdered) so
// no sort reorders rows, the rows are byte-identical to the eager whole-table path; only the accrued
// cost is lower (fewer rows scanned, filtered, and folded) — the deliberate short-circuit, mirroring
// execStreamingScan's LIMIT stop. page_read is the full block up front (only per-row work
// short-circuits, like the streaming scan).
func (db *engine) execWindowTopN(plan *selectPlan, env *evalEnv, meter *costMeter, params []Value) (emitter, error) {
	store := db.lkpStoreScoped(plan.rels[0].db, plan.rels[0].tableName)

	// The scan bound (the PK pushdown, if any) + its page_read block, exactly as execStreamingScan.
	b := unboundedBound()
	empty := false
	overlap, slabs := 0, 0
	if plan.relBounds[0] != nil && plan.relBounds[0].pk != nil {
		b, empty = db.buildKeyBound(plan.relBounds[0].pk, params, env.outer, nil)
	}
	if !empty {
		var err error
		if overlap, slabs, err = store.OverlapScanUnits(b, plan.relMasks[0]); err != nil {
			return emitter{}, err
		}
	}
	meter.Charge(costs.PageRead*int64(overlap) + costs.ValueDecompress*int64(slabs))

	limit := *plan.limit // non-nil (windowTopNEligible)
	var offset int64
	if plan.offset != nil {
		offset = *plan.offset
	}
	capN := offset + limit
	if capN < offset { // int64 overflow (offset+limit both enormous) ⇒ no effective cap, scan all
		capN = int64(1) << 62
	}

	// Collect the first `cap` surviving rows in PK scan order (respecting pkReverse), charging
	// storage_row_read per scanned row and the WHERE operator_evals — the streaming-scan feed, minus
	// the projection (the window stage runs before projection). Stop the instant `cap` survivors are in
	// hand: a genuine early-out, so the window fold sees only the prefix it needs.
	var rows []storedRow
	if !empty && limit > 0 {
		visit := func(_ []byte, row storedRow) (bool, error) {
			if err := meter.Guard(); err != nil { // enforce the cost ceiling per scanned row (CLAUDE.md §13)
				return false, err
			}
			meter.Charge(costs.StorageRowRead)
			row, err := store.resolveColumns(row, plan.relMasks[0])
			if err != nil {
				return false, err
			}
			if plan.filter != nil {
				v, err := plan.filter.eval(row, env, meter)
				if err != nil {
					return false, err
				}
				if !v.IsTrue() {
					return true, nil
				}
			}
			rows = append(rows, row)
			return int64(len(rows)) < capN, nil // stop once the OFFSET+LIMIT window is filled
		}
		var err error
		if plan.pkReverse {
			err = store.ScanRangeRev(b, visit)
		} else {
			err = store.ScanRange(b, visit)
		}
		if err != nil {
			return emitter{}, err
		}
	}

	// The window stage over the collected prefix — identical to the eager path (§5.2), just fewer rows.
	if err := applyWindowStage(rows, plan.windowSpecs, plan.windowKeys, env, meter); err != nil {
		return emitter{}, err
	}

	// The prefix is already in outer ORDER BY order (pkOrdered), so the sort is elided. Slice the
	// OFFSET..OFFSET+LIMIT window and project on emission — only an emitted row charges row_produced +
	// projection cost (the eager non-DISTINCT window path's emitProject, streaming.md §4).
	n := int64(len(rows))
	start := offset
	if start > n {
		start = n
	}
	end := n
	if limit < n-start {
		end = start + limit
	}
	return emitter{src: rows, start: start, end: end, mode: emitProject}, nil
}

// execIndexOrderScan is the streaming secondary-index-order scan (cost.md §3 "secondary-index
// order"): an ORDER BY the PK scan does NOT satisfy but a B-tree index does, with a LIMIT (the gate
// — plan.indexOrder non-nil). It walks the index store forward in key order, peels the fixed-width
// PK suffix off the END of each entry key (the "key-suffix skip"), point-looks-up the row, applies
// the residual filter, and STOPS once the LIMIT/OFFSET window is filled — a top-N that elides the
// blocking sort (and, for a collated index, the collate units). The index-tree page_read is charged
// up front as the full block (like the streaming PK scan — only the per-row work short-circuits);
// each scanned entry then charges its point-lookup's page_read/value_decompress + one
// storage_row_read, plus row_produced and projection operator_evals per produced row.
func (db *engine) execIndexOrderScan(plan *selectPlan, io *indexOrderPlan, env *evalEnv, meter *costMeter) (selectResult, error) {
	store := db.lkpStoreScoped(plan.rels[0].db, plan.rels[0].tableName)
	istore := db.lkpIndexStore(io.nameKey)
	// Up-front index-tree page_read (the full block; the index store has no payload, so no slabs).
	meter.Charge(costs.PageRead * int64(istore.NodeCount()))

	var offset int64
	if plan.offset != nil {
		offset = *plan.offset
	}
	out := make([][]Value, 0)
	if plan.limit == nil || *plan.limit > 0 {
		var passed int64
		err := istore.ScanRange(unboundedBound(), func(ekey []byte, _ storedRow) (bool, error) {
			if err := meter.Guard(); err != nil { // enforce the cost ceiling per scanned entry (CLAUDE.md §13)
				return false, err
			}
			// Peel the fixed-width PK suffix off the END of the index entry key (indexes.md §3):
			// the entry key is `<index columns> ‖ storage_key`, and storage_key is exactly
			// io.pkWidth bytes — so the suffix is the row's storage key with no prefix parse.
			rowKey := ekey[len(ekey)-io.pkWidth:]
			row, ok, n, sl, err := store.GetWithUnits(rowKey, plan.relMasks[0])
			if err != nil {
				return false, err
			}
			if !ok {
				panic("an index entry references a stored row")
			}
			meter.Charge(costs.PageRead*int64(n) + costs.ValueDecompress*int64(sl) + costs.StorageRowRead)
			row, err = store.resolveColumns(row, plan.relMasks[0])
			if err != nil {
				return false, err
			}
			if plan.filter != nil {
				v, err := plan.filter.eval(row, env, meter)
				if err != nil {
					return false, err
				}
				if !v.IsTrue() {
					return true, nil
				}
			}
			passed++
			if passed <= offset {
				return true, nil
			}
			meter.Charge(costs.RowProduced)
			projected := make([]Value, len(plan.projections))
			for i, p := range plan.projections {
				v, err := p.eval(row, env, meter)
				if err != nil {
					return false, err
				}
				projected[i] = v
			}
			out = append(out, projected)
			// Stop once a LIMIT window is filled (a top-N over the index order).
			if plan.limit != nil {
				return int64(len(out)) < *plan.limit, nil
			}
			return true, nil
		})
		if err != nil {
			return selectResult{}, err
		}
	}
	return selectResult{columnNames: plan.columnNames, columnTypes: plan.columnTypes, rows: out, cost: meter.Accrued}, nil
}

// execStreamingSort is the streaming external sort for a single-table ORDER BY (spec/design/spill.md
// §4/§5, streaming.md §4/§7). It streams scan→filter→sorter, so the input is never materialized in the
// executor heap; the sorter spills sorted runs to disk under workMem (file-backed databases) and
// k-way-merges them at finish. It runs the BLOCKING part (scan + sort + the OFFSET skip) and returns an
// emitSorted emitter holding the `sorted` pull iterator positioned at the first output row — so the
// window's row_produced + projection is charged LAZILY by the caller's emitter drive, one row per pull
// (the §4/§7 output-laziness follow-on: the output slice is never built and an early exit skips the rows
// it never pulls). Results + cost under full drain are byte-identical to the eager sort: the same
// page_read block, storage_row_read per scanned row, filter operator_eval, and row_produced per windowed
// row accrue — only the sort, which is unmetered (cost.md §3), now spills. Gated (by the caller) to a
// single table, no join, non-aggregate, non-DISTINCT, with an ORDER BY and no index bound.
func (db *engine) execStreamingSort(plan *selectPlan, env *evalEnv, meter *costMeter, params []Value) (emitter, error) {
	store := db.lkpStoreScoped(plan.rels[0].db, plan.rels[0].tableName)

	// Resolve the scan bound (the PK pushdown, if any) and charge the page_read + value_decompress
	// block up front — identical to the eager scan (cost.md §3). An INDEX bound never reaches here.
	b := unboundedBound()
	empty := false
	overlap, slabs := 0, 0
	if plan.relBounds[0] != nil && plan.relBounds[0].pk != nil {
		b, empty = db.buildKeyBound(plan.relBounds[0].pk, params, env.outer, nil)
	}
	if !empty {
		var err error
		if overlap, slabs, err = store.OverlapScanUnits(b, plan.relMasks[0]); err != nil {
			return emitter{}, err
		}
	}
	meter.Charge(costs.PageRead*int64(overlap) + costs.ValueDecompress*int64(slabs))

	// Build the sorted source in ORDER BY order, deferring the window's row_produced + projection to
	// the lazy emitter drive (the caller). Two ways to sort, both yielding a `sortedRows` pull iterator
	// over the survivors:
	//
	// A collated ORDER BY cannot use the C-ordered Sorter / spill (collated keys are slice 1e), and
	// collation is in-memory only this slice — so materialize the survivors and sort them with the
	// collation-aware decorate sorter (spec/design/collation.md §8), then wrap the sorted slice as an
	// in-memory `sortedRows`. The metered costs (storage_row_read per scanned row, row_produced per
	// windowed output) are identical to the Sorter path; the sort itself is unmetered like every sort
	// (cost.md §3).
	collated := false
	for _, k := range plan.order {
		if k.collation != nil {
			collated = true
			break
		}
	}
	var total int64
	var sorted *sortedRows
	if collated {
		var rows []storedRow
		if !empty {
			err := store.ScanRange(b, func(_ []byte, row storedRow) (bool, error) {
				if err := meter.Guard(); err != nil {
					return false, err
				}
				meter.Charge(costs.StorageRowRead)
				row, err := store.resolveColumns(row, plan.relMasks[0])
				if err != nil {
					return false, err
				}
				keep := true
				if plan.filter != nil {
					v, err := plan.filter.eval(row, env, meter)
					if err != nil {
						return false, err
					}
					keep = v.IsTrue()
				}
				if keep {
					rows = append(rows, row)
				}
				return true, nil
			})
			if err != nil {
				return emitter{}, err
			}
		}
		if err := sortRows(rows, plan.order); err != nil {
			return emitter{}, err
		}
		total = int64(len(rows))
		sorted = &sortedRows{mem: rows}
	} else {
		// Stream the scan → filter → sorter. ORDER BY is blocking, so the scan never short-circuits:
		// every in-range row is read (charging storage_row_read), its touched columns resolved
		// (large-values.md §14), the WHERE applied (charging operator_eval), and a survivor pushed into
		// the sorter, which spills when it exceeds the budget.
		s := db.newSorterFor(plan.order)
		if !empty {
			err := store.ScanRange(b, func(_ []byte, row storedRow) (bool, error) {
				if err := meter.Guard(); err != nil { // enforce the cost ceiling per scanned row (CLAUDE.md §13)
					return false, err
				}
				meter.Charge(costs.StorageRowRead)
				row, err := store.resolveColumns(row, plan.relMasks[0])
				if err != nil {
					return false, err
				}
				keep := true
				if plan.filter != nil {
					v, err := plan.filter.eval(row, env, meter)
					if err != nil {
						return false, err
					}
					keep = v.IsTrue()
				}
				if keep {
					if err := s.push(row); err != nil {
						return false, err
					}
				}
				return true, nil // never stop early — the sort must see every row
			})
			if err != nil {
				return emitter{}, err
			}
		}
		total = int64(s.total)
		var err error
		sorted, err = s.finish()
		if err != nil {
			return emitter{}, err
		}
	}

	// LIMIT / OFFSET window over the sort's total row count (known without materializing the output).
	// Clamp in the i64 domain (CLAUDE.md §8). The OFFSET skip is part of the blocking part (unwindowed —
	// no row_produced), done now so `sorted` is positioned at the first output row; the emitter drive
	// then yields exactly `end-start` rows, charging row_produced + projection per pull (streaming.md
	// §4/§7).
	var start int64
	if plan.offset != nil && *plan.offset < total {
		start = *plan.offset
	} else if plan.offset != nil {
		start = total
	}
	end := total
	if plan.limit != nil && *plan.limit < total-start {
		end = start + *plan.limit
	}
	for i := int64(0); i < start; i++ {
		if _, _, err := sorted.next(); err != nil { // skip the OFFSET rows (unwindowed)
			sorted.close()
			return emitter{}, err
		}
	}
	return emitter{sorted: sorted, start: 0, end: end - start, mode: emitSorted}, nil
}

// execStreamingJoin is a streaming two-table INNER/CROSS join whose ORDER BY is satisfied by the
// OUTER (first) relation's PK scan order (cost.md §3 "JOIN"). A left-deep nested loop produces
// combined rows in (outer PK, inner key) order — which IS the requested order — so the sort is
// elided, and with a LIMIT the loop STOPS once the window is filled. Both tables are still
// materialized in full (storage_row_read = the sum of cardinalities, the join contract); only the
// ON/WHERE operator_evals and row_produced short-circuit. Gated (by the caller / plan.joinPkOrdered)
// to exactly two non-lateral base relations, an INNER/CROSS join, a LIMIT, and a forward outer-PK
// ORDER BY.
func (db *engine) execStreamingJoin(plan *selectPlan, env *evalEnv, meter *costMeter, params []Value, outer []storedRow, rng *stmtRng) (selectResult, error) {
	leftRows, err := db.materializeRel(plan, 0, params, outer, nil, rng, env.ctes, meter)
	if err != nil {
		return selectResult{}, err
	}
	rightRows, err := db.materializeRel(plan, 1, params, outer, nil, rng, env.ctes, meter)
	if err != nil {
		return selectResult{}, err
	}
	on := plan.joins[0].on

	var offset int64
	if plan.offset != nil {
		offset = *plan.offset
	}
	out := make([][]Value, 0)
	if plan.limit == nil || *plan.limit > 0 {
		var passed int64
	outerLoop:
		for _, left := range leftRows {
			for _, right := range rightRows {
				combined := make(storedRow, 0, len(left)+len(right))
				combined = append(combined, left...)
				combined = append(combined, right...)
				// INNER: keep the pair iff its ON is TRUE (3VL); CROSS: keep every pair (no ON).
				if on != nil {
					v, err := on.eval(combined, env, meter)
					if err != nil {
						return selectResult{}, err
					}
					if !v.IsTrue() {
						continue
					}
				}
				// The residual WHERE over the combined row (per surviving pair).
				if plan.filter != nil {
					v, err := plan.filter.eval(combined, env, meter)
					if err != nil {
						return selectResult{}, err
					}
					if !v.IsTrue() {
						continue
					}
				}
				passed++
				if passed <= offset {
					continue
				}
				if err := meter.Guard(); err != nil { // enforce the cost ceiling per produced row (CLAUDE.md §13)
					return selectResult{}, err
				}
				meter.Charge(costs.RowProduced)
				projected := make([]Value, len(plan.projections))
				for j, p := range plan.projections {
					v, err := p.eval(combined, env, meter)
					if err != nil {
						return selectResult{}, err
					}
					projected[j] = v
				}
				out = append(out, projected)
				// Stop the whole nested loop once the LIMIT window is filled.
				if plan.limit != nil && int64(len(out)) >= *plan.limit {
					break outerLoop
				}
			}
		}
	}
	return selectResult{columnNames: plan.columnNames, columnTypes: plan.columnTypes, rows: out, cost: meter.Accrued}, nil
}

// newSorterFor builds a sorter for order, bounded by this handle's workMem. Spilling is enabled only
// for a file-backed database (an in-memory one has nowhere to spill — spill.md §2); spill runs live
// next to the database file (same filesystem, guaranteed writable).
func (db *engine) newSorterFor(order []orderSlot) *sorter {
	spillDir := ""
	if db.paging != nil {
		spillDir = filepath.Dir(db.path)
	}
	return newSorter(order, db.session.workMem, spillDir)
}

// rowsFromValues reinterprets a result-row slice ([][]Value) as a join-feed buffer ([]Row). Row is
// []Value, so each element converts directly; used where a CTE body's selectResult rows feed the
// join pipeline (spec/design/cte.md §5).
func rowsFromValues(in [][]Value) []storedRow {
	out := make([]storedRow, len(in))
	for i, r := range in {
		out[i] = storedRow(r)
	}
	return out
}

// materializeRel materializes one FROM relation ri into its rows, given the current outer-row stack
// `outer` (spec/design/grammar.md §15/§44). A base table is scanned (a PK/index bound may seek via
// outer); an SRF is generated; a CTE / derived table is delivered / run in place. For a CORRELATED
// LATERAL relation (§44) the caller passes outer EXTENDED with the combined left-hand row, so the
// body / SRF args read that row as their immediate outer; a non-lateral relation is passed the
// query's own outer and its parent=nil body simply ignores it (a parent=nil plan holds no
// outerColumn, so the two are observably identical).
func (db *engine) materializeRel(plan *selectPlan, ri int, params []Value, outer []storedRow, left storedRow, rng *stmtRng, ctes cteCtx, meter *costMeter) ([]storedRow, error) {
	rel := plan.rels[ri]
	env := &evalEnv{exec: db, params: params, outer: outer, rng: rng, ctes: ctes}
	// A set-returning relation is generated, not scanned (functions.md §10): produce its rows,
	// charging generated_row per element (its args read outer — implicitly lateral, §44).
	if rel.srf != nil {
		switch rel.srf.kind {
		case srfGenerateSeries:
			return db.generateSeriesRows(rel.srf, env, meter)
		case srfUnnest:
			return db.unnestRows(rel.srf, env, meter)
		case srfJsonbArrayElements, srfJsonbArrayElementsText, srfJsonbObjectKeys, srfJsonObjectKeys, srfJsonbEach, srfJsonbEachText, srfJSONRecord, srfJSONRecordset, srfJsonbPathQuery:
			return db.jsonSrfRows(rel.srf, env, meter)
		case srfJsonTable:
			return db.jsonTableRows(rel.srf, env, meter)
		case srfJedTables:
			return db.jedTablesRows(rel.srf, meter)
		case srfJedColumns:
			return db.jedColumnsRows(rel.srf, meter)
		case srfJedIndexes:
			return db.jedIndexesRows(rel.srf, meter)
		case srfJedConstraints:
			return db.jedConstraintsRows(rel.srf, meter)
		}
		return nil, nil
	}
	// A CTE reference delivers its rows from the per-statement context (cte.md §3/§5): a MATERIALIZED
	// CTE reads its buffer (charging cte_scan_row, guarded so a runaway scan aborts 54P01); an INLINE
	// CTE runs its body in place. (A CTE is never lateral.)
	if rel.cte != nil {
		ci := *rel.cte
		switch env.ctes.modes[ci] {
		case cteMaterialize:
			buf := env.ctes.buffers[ci]
			for range buf {
				if err := meter.Guard(); err != nil {
					return nil, err
				}
				meter.Charge(costs.CteScanRow)
			}
			return append([]storedRow(nil), buf...), nil
		case cteInline:
			// Only a plain (query) CTE is ever inlined; a data-modifying CTE is always materialized
			// (writable-cte.md §3), so its buffer was filled above.
			cplan := env.ctes.bindings[ci].plan
			r, err := db.execQueryPlan(&cplan, outer, params, env.ctes)
			if err != nil {
				return nil, err
			}
			meter.Charge(r.cost)
			return rowsFromValues(r.rows), nil
		}
		return nil, nil
	}
	// A DERIVED TABLE runs its body in place (grammar.md §42), charging its intrinsic cost — no
	// cte_scan_row. Non-lateral it was planned parent=nil and ignores outer; a LATERAL body (§44)
	// reads the left-hand row from outer.
	if rel.derived != nil {
		r, err := db.execQueryPlan(rel.derived, outer, params, env.ctes)
		if err != nil {
			return nil, err
		}
		meter.Charge(r.cost)
		return rowsFromValues(r.rows), nil
	}
	// A base table: scan in primary-key order via a scanSource (the page_read block + per-row
	// storage_row_read accrue inside next() — cost.md §3). A PK/index bound seeks/ranges instead of a
	// full walk; an empty bound reads nothing.
	// An index-nested-loop bound (per-outer-row seek) takes precedence over the once-materialized
	// bound and resolves its sibling source from the current left row (cost.md §3 "JOIN"); else the
	// once-materialized relBounds.
	store := db.lkpStoreScoped(rel.db, rel.tableName)
	sb := plan.relINLBounds[ri]
	if sb == nil {
		sb = plan.relBounds[ri]
	}
	var rows []storedRow
	var nodeCount, slabs int
	if sb != nil && sb.index != nil {
		var err error
		if rows, nodeCount, slabs, err = db.indexBoundRows(rel.tableName, sb.index, params, outer, plan.relMasks[ri], left); err != nil {
			return nil, err
		}
	} else if sb != nil && sb.gin != nil {
		// Re-find the constant query Q in the WHERE filter (the same conjunct plan-time ginMatch
		// chose — gin.md §6); the @>/&& predicate also stays the residual filter downstream.
		var query *rExpr
		if plan.filter != nil {
			if _, q, ok := ginMatch(plan.filter, sb.gin.colGlobal); ok {
				query = q
			}
		}
		entries, pages, sl, err := db.ginBoundRows(rel.tableName, sb.gin, query, env, meter, plan.relMasks[ri])
		if err != nil {
			return nil, err
		}
		// SELECT discards the storage keys (UPDATE/DELETE keep them — gin.md §6).
		rows = make([]storedRow, len(entries))
		for i := range entries {
			rows[i] = entries[i].Row
		}
		nodeCount, slabs = pages, sl
	} else if sb != nil && sb.gist != nil {
		// Re-find the constant query Q (the conjunct plan-time gistMatch chose — gist.md §5); the
		// &&/@> predicate also stays the residual filter downstream (always-recheck).
		var query *rExpr
		if plan.filter != nil {
			if q, ok := gistQueryOperand(plan.filter, sb.gist); ok {
				query = q
			}
		}
		entries, pages, sl, err := db.gistBoundRows(rel.tableName, sb.gist, query, env, meter, plan.relMasks[ri])
		if err != nil {
			return nil, err
		}
		rows = make([]storedRow, len(entries))
		for i := range entries {
			rows[i] = entries[i].Row
		}
		nodeCount, slabs = pages, sl
	} else if sb != nil && sb.pk != nil {
		b, empty := db.buildKeyBound(sb.pk, params, outer, left)
		if !empty {
			entries, pages, sl, err := store.RangeScanWithUnits(b, plan.relMasks[ri])
			if err != nil {
				return nil, err
			}
			rows = make([]storedRow, len(entries))
			for i := range entries {
				rows[i] = entries[i].Row
			}
			nodeCount, slabs = pages, sl
		}
	} else if sb != nil && sb.pkSet != nil {
		// Merged PK point-set (cost.md §3 "OR / IN-list"): a union of point probes over the
		// distinct sorted keys; the whole WHERE stays the residual filter downstream.
		entries, pages, sl, err := db.pkKeySetRows(store, sb.pkSet, params, outer, plan.relMasks[ri], left, true)
		if err != nil {
			return nil, err
		}
		rows = make([]storedRow, len(entries))
		for i := range entries {
			rows[i] = entries[i].Row
		}
		nodeCount, slabs = pages, sl
	} else if sb != nil && sb.indexSet != nil {
		// Merged secondary-index point-set (cost.md §3 "OR / IN-list").
		var err error
		if rows, nodeCount, slabs, err = db.indexKeySetRows(rel.tableName, sb.indexSet, params, outer, plan.relMasks[ri], left); err != nil {
			return nil, err
		}
	} else {
		entries, pages, sl, err := store.ScanWithUnits(plan.relMasks[ri])
		if err != nil {
			return nil, err
		}
		rows = make([]storedRow, len(entries))
		for i := range entries {
			rows[i] = entries[i].Row
		}
		nodeCount, slabs = pages, sl
	}
	// Materialize this relation's touched columns where the lazy load left unfetched references
	// (large-values.md §14) — exactly the static set the cost block charges.
	for i := range rows {
		var err error
		if rows[i], err = store.resolveColumns(rows[i], plan.relMasks[ri]); err != nil {
			return nil, err
		}
	}
	meter.Charge(costs.ValueDecompress * int64(slabs))
	src := &scanSource{rows: rows, nodeCount: nodeCount}
	var tableRows []storedRow
	for {
		row, ok, err := src.next(env, meter)
		if err != nil {
			return nil, err
		}
		if !ok {
			break
		}
		tableRows = append(tableRows, row)
	}
	return tableRows, nil
}

func (db *engine) execSelectPlan(plan *selectPlan, outer []storedRow, params []Value, ctes cteCtx) (selectResult, error) {
	// Run the blocking part to an emitter, then drive the emission EAGERLY into a slice (the
	// materialized drive). The lazy queryValues drive walks the SAME emitter row by row via
	// bufferedCursor (streaming.md §4); both charge the identical units at the identical sites, so the
	// totals agree (streaming.md §6).
	rng := newStmtRng()
	meter := db.session.newMeter()
	em, err := db.execSelectEmit(plan, outer, params, ctes, rng, meter)
	if err != nil {
		return selectResult{}, err
	}
	out, err := em.drainEager(db, plan, outer, params, ctes, rng, meter)
	if err != nil {
		return selectResult{}, err
	}
	return selectResult{columnNames: plan.columnNames, columnTypes: plan.columnTypes, rows: out, cost: meter.Accrued}, nil
}

// emitMode is how a bufferedCursor / drainEager emits an emitter's rows (spec/design/streaming.md §4).
type emitMode int

const (
	// emitProject: the buffer rows are unprojected — emission evaluates the projection list (charging
	// its operator_evals) plus row_produced per windowed row.
	emitProject emitMode = iota
	// emitIdentity: the buffer rows are already the projected output (the DISTINCT dedup projected
	// them up front — the §3 asymmetry), so emission charges only row_produced per windowed row.
	emitIdentity
	// emitFinal: the rows are a fully-formed result (the special input-streaming paths already
	// projected AND charged them) — emission hands them out with no further charge.
	emitFinal
	// emitSorted: the streaming external sort's output, yielded LAZILY from the `sorted` pull iterator
	// (positioned past the OFFSET) — emission pulls the next sorted row, charges row_produced, and
	// evaluates the projection list per windowed row, [0, end). So the output slice is never built and a
	// caller's early exit skips the projection (and row_produced) of the rows it never pulls
	// (streaming.md §4/§7).
	emitSorted
	// emitColumnar: the columnar projection fast path (batch.go projectColumnar, packed-leaf.md §11
	// Track A2/A3). `cols` holds the pre-gathered dense per-column lanes and `projCols` the projection's
	// column indices into them; emission builds output row j as [cols[projCols[0]][lane(j)], …] — a
	// bare-column projection with no full-width storedRow, charging row_produced per windowed row exactly
	// like emitProject (a bare column ref evaluates to row[index] with zero operator_eval, so the lane
	// read is cost-identical). `sel` is the optional A3 selection vector: when non-nil, output row j maps
	// to lane position sel[j] (a filtered scan's survivors); when nil, lane position j (all rows). Lazy
	// like emitProject: a caller's early exit skips the row_produced of the rows it never pulls.
	emitColumnar
)

// emitter describes how a selectPlan's output rows are emitted (spec/design/streaming.md §4, S4): a
// SELECT runs its blocking part (scan/join/WHERE/window/sort/GROUP BY/DISTINCT) into a buffer, then
// emits a row at a time. execSelectEmit returns this so the emission can be driven EAGERLY (the
// materialized drive — execSelectPlan's drainEager builds a slice) or LAZILY (the queryValues drive —
// bufferedCursor yields it row by row, bounding output memory and short-circuiting a caller's early
// exit). Both drives charge the identical units at the identical sites (streaming.md §6).
//   - emitProject: `src` holds the UNPROJECTED rows, windowed to [start, end) — emission evaluates the
//     projection list (charging its operator_evals) + row_produced per row.
//   - emitIdentity: `final` holds the already-projected rows (the DISTINCT dedup projected them up
//     front — the §3 asymmetry), windowed to [start, end) — emission charges only row_produced.
//   - emitFinal: `final` is a fully-formed result (the special input-streaming paths already projected
//     AND charged it) — emission hands it out with no further charge.
//   - emitSorted: `sorted` is the streaming-sort output pull iterator (positioned past the OFFSET),
//     [0, end) windowed — emission pulls + projects + charges row_produced per row (streaming.md §4/§7).
//   - emitColumnar: `cols` are the dense per-column lanes and `projCols` the projection's column indices
//     into them, [start, end) windowed — emission gathers output row j from the lanes (a bare-column
//     projection, no full-width row) at lane position sel[j] (or j when sel is nil) and charges
//     row_produced per row (packed-leaf.md §11 Track A2/A3).
type emitter struct {
	src      []storedRow // emitProject: unprojected rows
	final    [][]Value   // emitIdentity / emitFinal: already-projected rows
	sorted   *sortedRows // emitSorted: the streaming-sort output pull iterator (positioned past OFFSET)
	cols     [][]Value   // emitColumnar: the dense per-column lanes (indexed by table ordinal)
	projCols []int       // emitColumnar: projection column indices into cols (one per output column)
	sel      []int32     // emitColumnar: optional A3 selection vector — output row j → lane position sel[j]
	start    int64
	end      int64
	mode     emitMode
}

// drainEager builds the full output slice from the emitter — the materialized drive
// (spec/design/streaming.md §4). The lazy queryValues drive (bufferedCursor) emits the same rows one at
// a time instead; both charge the identical units in the identical order, so totals agree (§6).
func (em emitter) drainEager(db *engine, plan *selectPlan, outer []storedRow, params []Value, ctes cteCtx, rng *stmtRng, meter *costMeter) ([][]Value, error) {
	switch em.mode {
	case emitFinal:
		return em.final, nil
	case emitSorted:
		// The streaming sort's lazy output: pull every windowed row from the `sorted` iterator,
		// charging row_produced + the projection per row — exactly the eager window loop
		// execStreamingSort ran before its output went lazy (streaming.md §4/§7).
		defer em.sorted.close() // a LIMIT/error may stop the merge early — release any undrained runs
		env := &evalEnv{exec: db, params: params, outer: outer, rng: rng, ctes: ctes}
		out := make([][]Value, 0, em.end)
		for i := int64(0); i < em.end; i++ {
			row, ok, err := em.sorted.next()
			if err != nil {
				return nil, err
			}
			if !ok {
				break
			}
			if err := meter.Guard(); err != nil { // enforce the cost ceiling per produced row (CLAUDE.md §13)
				return nil, err
			}
			meter.Charge(costs.RowProduced)
			projected := make([]Value, len(plan.projections))
			for j, p := range plan.projections {
				v, perr := p.eval(row, env, meter)
				if perr != nil {
					return nil, perr
				}
				projected[j] = v
			}
			out = append(out, projected)
		}
		return out, nil
	case emitIdentity:
		out := make([][]Value, 0, em.end-em.start)
		for _, row := range em.final[em.start:em.end] {
			if err := meter.Guard(); err != nil { // enforce the cost ceiling per produced row (CLAUDE.md §13)
				return nil, err
			}
			meter.Charge(costs.RowProduced)
			out = append(out, row)
		}
		return out, nil
	case emitColumnar:
		// Columnar projection (packed-leaf.md §11 Track A2/A3): gather each windowed output row from the
		// dense lanes — a bare-column projection with no full-width row — charging row_produced per row,
		// exactly the emitProject drive over a bare-column projection (whose p.eval is a zero-cost slot
		// read). A non-nil `sel` (the A3 filter's survivors) maps output row j to lane position sel[j].
		out := make([][]Value, 0, em.end-em.start)
		for j := em.start; j < em.end; j++ {
			if err := meter.Guard(); err != nil { // enforce the cost ceiling per produced row (CLAUDE.md §13)
				return nil, err
			}
			meter.Charge(costs.RowProduced)
			li := j
			if em.sel != nil {
				li = int64(em.sel[j])
			}
			projected := make([]Value, len(em.projCols))
			for k, c := range em.projCols {
				projected[k] = em.cols[c][li]
			}
			out = append(out, projected)
		}
		return out, nil
	default: // emitProject
		env := &evalEnv{exec: db, params: params, outer: outer, rng: rng, ctes: ctes}
		out := make([][]Value, 0, em.end-em.start)
		for _, row := range em.src[em.start:em.end] {
			if err := meter.Guard(); err != nil { // enforce the cost ceiling per produced row (CLAUDE.md §13)
				return nil, err
			}
			meter.Charge(costs.RowProduced)
			projected := make([]Value, len(plan.projections))
			for i, p := range plan.projections {
				v, perr := p.eval(row, env, meter)
				if perr != nil {
					return nil, perr
				}
				projected[i] = v
			}
			out = append(out, projected)
		}
		return out, nil
	}
}

// execSelectEmit runs a selectPlan's blocking part and returns an emitter describing how to emit its
// output rows (spec/design/streaming.md §4, S4): the scan / join / WHERE / window / ORDER BY / GROUP
// BY / DISTINCT all run here (charging their cost into meter), producing either a windowed buffer
// (projected lazily on emission) or, for the special input-streaming paths, a fully-formed result. The
// caller drives the emission — eagerly (execSelectPlan, the materialized Execute path) or lazily
// (bufferedCursor, the Query path). The shared rng threads the per-statement entropy through both the
// blocking part and the (possibly deferred) projection (streaming.md §6).
func (db *engine) execSelectEmit(plan *selectPlan, outer []storedRow, params []Value, ctes cteCtx, rng *stmtRng, meter *costMeter) (emitter, error) {
	env := &evalEnv{exec: db, params: params, outer: outer, rng: rng, ctes: ctes}

	// Vectorized single-table aggregate (batch.go, the PAX/vectorization program's executor track): a
	// SUM/COUNT/MIN/MAX/AVG with no DISTINCT / FILTER / HAVING / window / ORDER BY, either whole-table
	// or grouped by a single integer column, folds columnar / int64-bucketed instead of the
	// row-at-a-time group machinery below. Gated to the unmetered lane so a metered query's
	// deterministic abort row stays the scalar path's; results and accrued cost are byte-identical
	// either way (the conformance corpus proves both). Ineligible / metered ⇒ this is skipped and the
	// general aggregate branch runs unchanged.
	if meter.unmetered() && db.vectorizedAggEligible(plan) {
		return db.execVectorizedAgg(plan, outer, params, ctes, rng, meter)
	}

	// Columnar projection fast path (batch.go, packed-leaf.md §11 Track A2/A3): a bare-column projection
	// over a single-table full/PK-bounded scan with no ORDER BY / LIMIT / OFFSET / blocking operator
	// gathers only its touched columns into dense lanes and emits from them — never the full-width
	// storedRow the materialize path below allocates per record (the allocation dividend on a wide table).
	// A WHERE predicate (A3) is applied over the lanes into a selection vector rather than forcing the row
	// path. Gated to the unmetered lane (so a metered query's per-eval Guards stay the row path's) and to
	// file-backed stores with no spillable touched column (projectColumnar declines otherwise, falling
	// through to the identical-cost row path). Cost-neutral by construction.
	if meter.unmetered() && db.vectorizedProjectEligible(plan) {
		em, ok, err := db.projectColumnar(plan, env, meter)
		if err != nil {
			return emitter{}, err
		}
		if ok {
			return em, nil
		}
	}

	// Streaming primary-key-ordered scan (spec/design/cost.md §3): a single-table query with no
	// blocking operator beyond an ORDER BY the scan already satisfies — either no ORDER BY with a
	// LIMIT (the LIMIT short-circuit), or an ORDER BY satisfied by the table's primary-key scan order
	// (plan.pkOrdered) — streams scan→filter→project with NO sort, and with a LIMIT STOPS the scan
	// once the window is filled, so storage_row_read counts only the rows actually read (a genuine
	// early-out, not a post-hoc truncation). A non-PK-ordered ORDER BY, DISTINCT, aggregate, or join
	// must see every row, so it keeps the sort/eager path below. page_read stays the full block (the
	// bound's node count); only row reads short-circuit.
	// An index-bounded scan does not stream (cost.md §3 "index-bounded scan"): it reads
	// the full admitted set via the eager path below.
	// A set-returning relation is generated, not scanned — it takes the eager path
	// (functions.md §10); the streaming reader assumes a table store.
	// A pkOrdered DISTINCT streams too: the dedup runs in scan order (the sort elided), so it
	// short-circuits a top-N like the non-DISTINCT case. A no-ORDER-BY DISTINCT keeps the eager path.
	if streamingScanEligible(plan) {
		res, err := db.execStreamingScan(plan, env, meter, params)
		if err != nil {
			return emitter{}, err
		}
		return emitter{final: res.rows, mode: emitFinal}, nil
	}

	// Streaming secondary-index-order scan (cost.md §3 "secondary-index order"): the planner set
	// indexOrder only for a single-table, non-aggregate/window/DISTINCT, no-bound, LIMITed query
	// whose ORDER BY a B-tree index satisfies (and the PK scan does not). Walk the index +
	// point-lookup; the eager sort is elided.
	if plan.indexOrder != nil {
		res, err := db.execIndexOrderScan(plan, plan.indexOrder, env, meter)
		if err != nil {
			return emitter{}, err
		}
		return emitter{final: res.rows, mode: emitFinal}, nil
	}

	// Streaming external sort (spec/design/spill.md §5): a single-table, no-join, non-aggregate,
	// non-DISTINCT query with an ORDER BY the scan does NOT already satisfy (!plan.pkOrdered — caught
	// above) streams scan→filter→sorter, so the input is never materialized in the executor heap and
	// the sort spills sorted runs to disk under workMem (file-backed databases). DISTINCT/aggregate/
	// join take the eager path below, and an index bound does not stream (like the LIMIT
	// short-circuit). Results + cost are identical to the eager sort (the sort is unmetered —
	// cost.md §3; spill.md §6).
	if len(plan.order) > 0 && !plan.pkOrdered && len(plan.orderExprs) == 0 && len(plan.rels) == 1 && len(plan.joins) == 0 &&
		!plan.isAgg && !plan.hasWindow && !plan.distinct &&
		!plan.relBounds[0].needsEagerScan() &&
		plan.rels[0].srf == nil &&
		// A CTE reference takes the eager path (cte.md §5).
		plan.rels[0].cte == nil &&
		// A derived table takes the eager path (grammar.md §42).
		plan.rels[0].derived == nil {
		// The streaming sort yields its output LAZILY (streaming.md §4/§7) — execStreamingSort runs the
		// scan + sort + OFFSET skip and returns an emitSorted emitter over the `sorted` pull iterator;
		// the window's row_produced + projection is charged by the emitter drive.
		return db.execStreamingSort(plan, env, meter, params)
	}

	// Streaming two-table join (cost.md §3 "JOIN"): the planner set joinPkOrdered only for a two-table
	// INNER/CROSS join whose ORDER BY the OUTER relation's PK scan order satisfies, with a LIMIT. The
	// nested loop drives the outer in PK order so the output is already ordered — the sort is elided
	// and the loop short-circuits a top-N.
	if plan.joinPkOrdered {
		res, err := db.execStreamingJoin(plan, env, meter, params, outer, env.rng)
		if err != nil {
			return emitter{}, err
		}
		return emitter{final: res.rows, mode: emitFinal}, nil
	}

	// Windowed top-N (spec/design/window.md §5.2, cost.md §3): a plain window query whose LIMIT is
	// answerable from the first OFFSET+LIMIT PK-scan rows (a backward window over the PK-ordered scan)
	// scans only that prefix instead of the whole table — the window analog of the streaming LIMIT
	// short-circuit. Ineligible window queries fall through to the eager whole-table materialize below.
	if db.windowTopNEligible(plan) {
		return db.execWindowTopN(plan, env, meter, params)
	}

	// Materialize each relation once, in primary-key order (base tables drain a scanSource — the
	// page_read block + per-row storage_row_read accrue inside next(), cost.md §3). The nested loop
	// re-reads from these in-memory buffers, which are not stores and charge nothing. A CORRELATED
	// LATERAL relation (§44) depends on the left-hand row, so it cannot be materialized up front — a
	// placeholder (nil) holds its slot and the join loop re-materializes it per combined left row.
	// An INDEX-NESTED-LOOP relation (cost.md §3 "JOIN") likewise depends on the left-hand row (its
	// bound seeks per outer row), so it is not materialized up front either — a placeholder (nil)
	// holds its slot and the join loop re-materializes it per left row.
	materialized := make([][]storedRow, len(plan.rels))
	for ri, rel := range plan.rels {
		if rel.lateral || plan.relINLBounds[ri] != nil {
			continue
		}
		rows, err := db.materializeRel(plan, ri, params, outer, nil, env.rng, env.ctes, meter)
		if err != nil {
			return emitter{}, err
		}
		materialized[ri] = rows
	}

	// Left-deep nested-loop join. `running` holds the combined rows over the relations joined
	// so far (starting with the first table's rows). For each join, concatenate every running
	// row with every right-table row; CROSS keeps all pairs, INNER keeps a pair iff its ON
	// predicate is TRUE (three-valued — a NULL join key never matches). LEFT/FULL additionally
	// emit each unmatched left row NULL-extended over the right side; RIGHT/FULL emit each
	// unmatched right row NULL-extended over the left side. The NULL-extension appends evaluate
	// no ON (no operator_eval — spec/design/cost.md §3). Output order is deterministic: running
	// order (outer) then right key order (inner), each unmatched left row after its (empty)
	// match run, all unmatched right rows last in right key order (CLAUDE.md §10).
	// A FROM-less SELECT has no relations: seed `running` with ONE virtual zero-column row
	// instead of a table's rows (grammar.md §34). No scan ran, so no scan cost accrued.
	running := []storedRow{{}}
	if len(plan.rels) > 0 {
		running = materialized[0]
	}
	for k := range plan.joins {
		on := plan.joins[k].on
		emitLeft := plan.joins[k].kind == joinLeft || plan.joins[k].kind == joinFull
		emitRight := plan.joins[k].kind == joinRight || plan.joins[k].kind == joinFull
		// NULL-pad widths come from the PLAN, never a sampled row, so they are correct even when
		// `running`/`rightRows` is empty: the right table begins at flat offset rels[k+1].offset
		// (= the width of every running row) and is that many columns wide.
		leftPad := plan.rels[k+1].offset
		rightPad := plan.rels[k+1].colCount
		var next []storedRow
		// A CORRELATED LATERAL relation (§44): re-materialize it ONCE PER combined left-hand row, with
		// that row pushed onto the outer-row stack as the body's immediate outer (the correlated-
		// subquery mechanism). The plan guarantees INNER/CROSS/LEFT here (RIGHT/FULL to a correlated
		// lateral is 42P10), so there is no unmatched-right emission.
		if plan.rels[k+1].lateral {
			for _, left := range running {
				latOuter := make([]storedRow, len(outer)+1)
				copy(latOuter, outer)
				latOuter[len(outer)] = left
				rightRows, err := db.materializeRel(plan, k+1, params, latOuter, nil, env.rng, env.ctes, meter)
				if err != nil {
					return emitter{}, err
				}
				leftMatched := false
				for _, right := range rightRows {
					combined := make(storedRow, 0, len(left)+len(right))
					combined = append(combined, left...)
					combined = append(combined, right...)
					keep := true
					if on != nil {
						v, err := on.eval(combined, env, meter)
						if err != nil {
							return emitter{}, err
						}
						keep = v.IsTrue()
					}
					if keep {
						next = append(next, combined)
						leftMatched = true
					}
				}
				if emitLeft && !leftMatched {
					combined := make(storedRow, 0, len(left)+rightPad)
					combined = append(combined, left...)
					for i := 0; i < rightPad; i++ {
						combined = append(combined, NullValue())
					}
					next = append(next, combined)
				}
			}
			running = next
			continue
		}
		// An INDEX-NESTED-LOOP inner relation (cost.md §3 "JOIN"): re-materialize it ONCE PER combined
		// left-hand row, its scan bounded per outer row by the SIBLING columns of that row (a
		// per-outer-row seek instead of a full scan). Detection restricts this to the RIGHT/nullable
		// side of an INNER/CROSS/LEFT join, so there is never an unmatched-RIGHT emission (RIGHT/FULL
		// are excluded — a preserved side cannot be bounded per outer row). The whole ON/WHERE stays
		// applied (the ON here, the WHERE below), so rows are unchanged.
		if plan.relINLBounds[k+1] != nil {
			for _, left := range running {
				rightRows, err := db.materializeRel(plan, k+1, params, outer, left, env.rng, env.ctes, meter)
				if err != nil {
					return emitter{}, err
				}
				leftMatched := false
				for _, right := range rightRows {
					combined := make(storedRow, 0, len(left)+len(right))
					combined = append(combined, left...)
					combined = append(combined, right...)
					keep := true
					if on != nil {
						v, err := on.eval(combined, env, meter)
						if err != nil {
							return emitter{}, err
						}
						keep = v.IsTrue()
					}
					if keep {
						next = append(next, combined)
						leftMatched = true
					}
				}
				if emitLeft && !leftMatched {
					combined := make(storedRow, 0, len(left)+rightPad)
					combined = append(combined, left...)
					for i := 0; i < rightPad; i++ {
						combined = append(combined, NullValue())
					}
					next = append(next, combined)
				}
			}
			running = next
			continue
		}
		rightRows := materialized[k+1]
		rightMatched := make([]bool, len(rightRows))
		for _, left := range running {
			leftMatched := false
			for ri, right := range rightRows {
				combined := make(storedRow, 0, len(left)+len(right))
				combined = append(combined, left...)
				combined = append(combined, right...)
				keep := true
				if on != nil {
					v, err := on.eval(combined, env, meter)
					if err != nil {
						return emitter{}, err
					}
					keep = v.IsTrue()
				}
				if keep {
					next = append(next, combined)
					leftMatched = true
					rightMatched[ri] = true
				}
			}
			if emitLeft && !leftMatched {
				combined := make(storedRow, 0, len(left)+rightPad)
				combined = append(combined, left...)
				for i := 0; i < rightPad; i++ {
					combined = append(combined, NullValue())
				}
				next = append(next, combined)
			}
		}
		if emitRight {
			for ri, right := range rightRows {
				if !rightMatched[ri] {
					combined := make(storedRow, 0, leftPad+len(right))
					for i := 0; i < leftPad; i++ {
						combined = append(combined, NullValue())
					}
					combined = append(combined, right...)
					next = append(next, combined)
				}
			}
		}
		running = next
	}

	// WHERE over the combined rows. A WHERE arithmetic can trap (22003/22012); each surviving
	// combined row's filter accrues operator_eval.
	var rows []storedRow
	for _, row := range running {
		keep := true
		if plan.filter != nil {
			v, err := plan.filter.eval(row, env, meter)
			if err != nil {
				return emitter{}, err
			}
			keep = v.IsTrue()
		}
		if keep {
			rows = append(rows, row)
		}
	}

	// WINDOW stage (spec/design/window.md §5.2): a blocking operator over the post-WHERE rows,
	// running BEFORE the query ORDER BY / DISTINCT / LIMIT. Each window function's per-row result is
	// APPENDED to its row (so the projection reads result i at flat slot input_width+i); the rows
	// keep their scan order, and the query ORDER BY below re-sorts the extended rows. A window query
	// never enters the streaming fast-paths above. A GROUPED window query (§2) runs the window stage
	// over the GROUPED rows instead, inside the aggregate branch below — so it is gated to plain
	// (non-aggregate) windows here.
	if plan.hasWindow && !plan.isAgg {
		if err := applyWindowStage(rows, plan.windowSpecs, plan.windowKeys, env, meter); err != nil {
			return emitter{}, err
		}
	}

	// ORDER BY: stable sort applying each key left to right — the first non-equal key decides,
	// and a full tie keeps the scan order (SliceStable). Each key's NULL placement is decoupled
	// from its value-direction flip (spec/design/grammar.md §10). Aggregate queries sort their
	// GROUP rows in the aggregate branch below — not these pre-aggregation rows — so this is
	// gated to plain queries.
	if !plan.isAgg && len(plan.order) > 0 {
		// Materialize each general-expression ORDER BY key (grammar.md §10): evaluate it against the
		// post-WHERE (post-window) row and append the value, so its sort slot final_width+k reads the
		// appended column and the slot-based sort below is unchanged — the window-key precedent. The
		// evaluation is metered per node (cost.md §3); a no-op for a column/ordinal-only ORDER BY.
		if err := materializeOrderExprs(rows, plan.orderExprs, env, meter); err != nil {
			return emitter{}, err
		}
		if err := sortRows(rows, plan.order); err != nil {
			return emitter{}, err
		}
	}

	// LIMIT / OFFSET window bounds over a result of n rows. Clamp in the i64 domain
	// against the row count before indexing — never truncate a huge count (CLAUDE.md §8;
	// spec/design/grammar.md §9). The counts are already non-negative (parser).
	windowBounds := func(n int64) (int64, int64) {
		start := int64(0)
		if plan.offset != nil && *plan.offset < n {
			start = *plan.offset
		} else if plan.offset != nil {
			start = n
		}
		end := n
		if plan.limit != nil && *plan.limit < n-start {
			end = start + *plan.limit
		}
		return start, end
	}

	// Build the emitter. The two paths differ in pipeline order (spec/design/grammar.md §11): without
	// DISTINCT the window slices the sorted source rows and ONLY the windowed rows are projected (on
	// emission); with DISTINCT every (sorted) filtered row is projected — dedup must see them all —
	// duplicates drop by first occurrence, and the window then slices the DISTINCT rows.
	var em emitter
	if plan.isAgg {
		// Aggregate query — group + accumulate (aggregates.md §5). Bucket the post-WHERE rows by
		// their group-key values; the bucket key is the value-canonical distinctRowKey (it
		// collapses 1.5/1.50 and groups NULL with NULL), and the map is only an index — output
		// order comes from the insertion-ordered `groups`, never map iteration (no map-order leak
		// — CLAUDE.md §8/§10). Whole-table aggregation (no GROUP BY) is one pre-created empty-key
		// group, so it emits ONE row even over zero input; GROUP BY over an empty table creates no
		// groups -> zero rows. Each (row × aggregate) charges aggregate_accumulate; the operand's
		// own operator_evals accrue via eval; the bucketing/finalize is unmetered (cost.md §3).
		// Per group: its key values, one acc per aggregate, and one DISTINCT dedup set per
		// DISTINCT aggregate (nil for a plain aggregate — COUNT(DISTINCT x), aggregates.md §5).
		// The set keys on distinctRowKey, the same value-canonical form as the group-key
		// bucketing (so 1.5 == 1.50 and -0.0 == 0.0).
		type group struct {
			keys []Value
			accs []*acc
			seen []map[string]bool
		}
		newSeen := func() []map[string]bool {
			s := make([]map[string]bool, len(plan.aggSpecs))
			for i, spec := range plan.aggSpecs {
				if spec.distinct {
					s[i] = make(map[string]bool)
				}
			}
			return s
		}
		newAccs := func() []*acc {
			a := make([]*acc, len(plan.aggSpecs))
			for i, spec := range plan.aggSpecs {
				a[i] = newAccFromSpec(spec)
			}
			return a
		}
		// One grouping set per plan.groupSets (spec/design/aggregates.md §12): a plain GROUP BY /
		// whole-table aggregate has exactly one; ROLLUP / CUBE / GROUPING SETS have several. Each set is
		// bucketed independently over the SAME post-WHERE rows and its groups projected into the shared
		// synthetic row [master_columns…, aggregate_results…, GROUPING_results…]: a column not grouped
		// in this set is NULL, and each GROUPING() value comes from this set's mask. The scan
		// (storage_row_read) is upstream and counted once; aggregate_accumulate + operand evals accrue
		// per (set × row × passing aggregate). The per-set bucket index is never iterated — output order
		// comes from the insertion-ordered groups then the set order (no map-order leak — §8/§10).
		// Materialize the general-expression GROUP BY keys (aggregates.md §15): evaluate each per
		// post-WHERE row ONCE (charging its operator_evals, like an aggregate operand) and append the
		// value at flat slot inputWidth+k, so a master grouping-key index pointing there reads it. Done
		// before the (possibly multi-) grouping-set loop so each row is extended once and the values are
		// shared across sets. A plain column GROUP BY appends nothing.
		if len(plan.groupExprs) > 0 {
			for ri := range rows {
				if err := meter.Guard(); err != nil {
					return emitter{}, err
				}
				for _, ge := range plan.groupExprs {
					v, gerr := ge.eval(rows[ri], env, meter)
					if gerr != nil {
						return emitter{}, gerr
					}
					rows[ri] = append(rows[ri], v)
				}
			}
		}
		var groupRows []storedRow
		for gsi := range plan.groupSets {
			gset := &plan.groupSets[gsi]
			index := make(map[string]int)
			var groups []group
			// An empty grouping set (the () / whole-table grand total) is one pre-created group, so it
			// emits ONE row even over zero input; a non-empty set over empty input emits nothing.
			if len(gset.keyCols) == 0 {
				groups = append(groups, group{keys: nil, accs: newAccs(), seen: newSeen()})
				index[""] = 0
			}
			for _, row := range rows {
				if err := meter.Guard(); err != nil { // enforce the cost ceiling per folded row (CLAUDE.md §13)
					return emitter{}, err
				}
				keys := make([]Value, len(gset.keyCols))
				for i, gk := range gset.keyCols {
					keys[i] = row[gk]
				}
				k := distinctRowKey(keys)
				gi, ok := index[k]
				if !ok {
					gi = len(groups)
					index[k] = gi
					groups = append(groups, group{keys: keys, accs: newAccs(), seen: newSeen()})
				}
				for i, spec := range plan.aggSpecs {
					// FILTER (WHERE cond): a row for which the filter is not TRUE (FALSE or NULL)
					// contributes nothing to THIS aggregate — its operand is not evaluated and it is not
					// accumulated (aggregates.md §11). The filter's own operator_evals are charged (it is
					// evaluated per row, like the operand); aggregate_accumulate is charged only for a row
					// that passes. The pass/fold decision is deterministic (scan order is cross-core
					// identical), so the metered cost is identical across cores.
					if spec.filter != nil {
						fv, ferr := spec.filter.eval(row, env, meter)
						if ferr != nil {
							return emitter{}, ferr
						}
						if !fv.IsTrue() {
							continue
						}
					}
					meter.Charge(costs.AggregateAccumulate)
					// A hypothetical-set aggregate (rank/dense_rank/… — aggregates.md §19) buffers the
					// row's WITHIN GROUP key TUPLE (no NULL-skip — every row counts, sorted by NULLS
					// FIRST/LAST). The hypothetical row itself is evaluated per group at finalize. No
					// DISTINCT (rejected at resolve).
					if spec.hypo != nil {
						tuple := make([]Value, len(spec.hypo.keys))
						for ki, k := range spec.hypo.keys {
							kv, kerr := k.eval(row, env, meter)
							if kerr != nil {
								return emitter{}, kerr
							}
							tuple[ki] = kv
						}
						a := groups[gi].accs[i]
						a.hypoRows = append(a.hypoRows, tuple)
						continue
					}
					v := NullValue() // COUNT(*) ignores the value
					if spec.operand != nil {
						var verr error
						if v, verr = spec.operand.eval(row, env, meter); verr != nil {
							return emitter{}, verr
						}
					}
					// DISTINCT: skip a NULL (never folded by any aggregate) and any value already
					// folded into this group — the FIRST occurrence in scan order wins, so the set of
					// folded values (and the decimal_work fold charges) is order-deterministic and
					// cross-core identical.
					if seen := groups[gi].seen[i]; seen != nil {
						if v.IsNull() {
							continue
						}
						dk := distinctRowKey([]Value{v})
						if seen[dk] {
							continue
						}
						seen[dk] = true
					}
					if ferr := groups[gi].accs[i].fold(v, meter); ferr != nil {
						return emitter{}, ferr
					}
				}
			}
			// Build one synthetic row per group of this set: each master grouping column's value (NULL
			// where this set doesn't group it), then the aggregate results, then each GROUPING() value
			// (computed from this set's mask — spec/design/aggregates.md §12).
			for _, g := range groups {
				srow := make([]Value, 0, len(plan.groupKeys)+len(plan.aggSpecs)+len(plan.groupingSpecs))
				for _, src := range gset.slotSrc {
					if src < 0 {
						srow = append(srow, NullValue())
					} else {
						srow = append(srow, g.keys[src])
					}
				}
				for si, a := range g.accs {
					// An ordered-set aggregate's percentile fraction (the direct argument) is
					// evaluated PER GROUP here, against the synthetic row's grouping-key values
					// (aggregates.md §13/§17) — so it may reference grouping columns. Unmetered
					// (the finalize step, like the sort), via a scratch meter. mode has none.
					if fe := plan.aggSpecs[si].osaFrac; fe != nil {
						fv, ferr := fe.eval(srow, env, &costMeter{})
						if ferr != nil {
							return emitter{}, ferr
						}
						a.osaFrac = &fv
					}
					// A hypothetical-set aggregate is finalized INLINE here (not via acc.finalize)
					// because it needs the spec's per-key sort specs: evaluate the hypothetical row's
					// direct args per group (against the synthetic row, like a fraction — unmetered
					// scratch meter), then count its rank among the buffered key tuples (aggregates.md §19).
					if hp := plan.aggSpecs[si].hypo; hp != nil {
						hyp := make([]Value, len(hp.args))
						for ai, arg := range hp.args {
							av, aerr := arg.eval(srow, env, &costMeter{})
							if aerr != nil {
								return emitter{}, aerr
							}
							hyp[ai] = av
						}
						v, ferr := finalizeHypothetical(a.plan, a.hypoRows, hyp, hp.sorts)
						if ferr != nil {
							return emitter{}, ferr
						}
						srow = append(srow, v)
						continue
					}
					v, ferr := a.finalize()
					if ferr != nil {
						return emitter{}, ferr
					}
					srow = append(srow, v)
				}
				for _, positions := range plan.groupingSpecs {
					srow = append(srow, IntValue(groupingValue(positions, gset.mask)))
				}
				groupRows = append(groupRows, srow)
			}
		}
		// HAVING: filter the grouped rows (after aggregation, before ORDER BY). The predicate is
		// evaluated against each group's synthetic row (charging its operator_evals per group);
		// only a TRUE result keeps the group. A dropped group charges no row_produced (§8).
		if plan.having != nil {
			kept := groupRows[:0:0]
			for _, srow := range groupRows {
				v, herr := plan.having.eval(srow, env, meter)
				if herr != nil {
					return emitter{}, herr
				}
				if v.IsTrue() {
					kept = append(kept, srow)
				}
			}
			groupRows = kept
		}
		// WINDOW stage over the grouped rows (spec/design/window.md §2): runs AFTER GROUP BY/HAVING
		// and BEFORE the query ORDER BY. It appends each window result to the grouped row
		// [group_keys…, agg_results…], so the projection reads window result w at slot
		// len(groupKeys)+len(aggSpecs)+w (the rebased slot — §5.1). The group-key slots the ORDER BY
		// below sorts on are unchanged (they precede the appended results).
		if plan.hasWindow {
			if err := applyWindowStage(groupRows, plan.windowSpecs, plan.windowKeys, env, meter); err != nil {
				return emitter{}, err
			}
		}
		// ORDER BY over the grouped output (a column/ordinal key is a synthetic group-key slot; an
		// expression key is materialized against the grouped row and appended — grammar.md §10).
		if len(plan.order) > 0 {
			if err := materializeOrderExprs(groupRows, plan.orderExprs, env, meter); err != nil {
				return emitter{}, err
			}
			if err := sortRows(groupRows, plan.order); err != nil {
				return emitter{}, err
			}
		}
		if plan.distinct {
			// SELECT DISTINCT: project EVERY grouped row (charging its projection operator_evals,
			// the §3 asymmetry — like the non-aggregate DISTINCT path below), dedup by equality
			// keeping the first occurrence in the (already ORDER-BY-sorted) order, then LIMIT/OFFSET.
			// `seen` is membership-only; output order comes from the deterministic group iteration /
			// sort, never map iteration (no map-order leak — CLAUDE.md §8/§10).
			seen := make(map[string]bool)
			var distinctRows [][]Value
			for _, srow := range groupRows {
				projected := make([]Value, len(plan.projections))
				for i, p := range plan.projections {
					v, perr := p.eval(srow, env, meter)
					if perr != nil {
						return emitter{}, perr
					}
					projected[i] = v
				}
				if key := distinctRowKey(projected); !seen[key] {
					seen[key] = true
					distinctRows = append(distinctRows, projected)
				}
			}
			// The dedup already projected every grouped row (the §3 asymmetry, charged above), so
			// emission is Identity — window + charge row_produced, deferred to the drive (streaming.md §4).
			start, end := windowBounds(int64(len(distinctRows)))
			em = emitter{final: distinctRows, start: start, end: end, mode: emitIdentity}
		} else {
			// Window then project on emission; only an emitted row charges row_produced + projection
			// cost. Deferred to the drive (streaming.md §4).
			start, end := windowBounds(int64(len(groupRows)))
			em = emitter{src: groupRows, start: start, end: end, mode: emitProject}
		}
	} else if plan.distinct {
		// Project every filtered row (charging projection cost per row, the §3 asymmetry),
		// keeping first occurrences. `seen` is membership-only: output order comes from the
		// deterministic source iteration, never from map iteration (no map-order leak —
		// CLAUDE.md §8/§10).
		seen := make(map[string]bool)
		var distinctRows [][]Value
		for _, row := range rows {
			projected := make([]Value, len(plan.projections))
			for i, p := range plan.projections {
				v, err := p.eval(row, env, meter)
				if err != nil {
					return emitter{}, err
				}
				projected[i] = v
			}
			if key := distinctRowKey(projected); !seen[key] {
				seen[key] = true
				distinctRows = append(distinctRows, projected)
			}
		}
		// LIMIT / OFFSET applies to the DISTINCT rows; only the emitted rows charge RowProduced
		// (spec/design/cost.md §3). The rows were already projected for their dedup key (the §3
		// asymmetry, charged above), so emission is Identity — deferred to the drive (streaming.md §4).
		start, end := windowBounds(int64(len(distinctRows)))
		em = emitter{final: distinctRows, start: start, end: end, mode: emitIdentity}
	} else {
		// Window the sorted rows BEFORE projection, so rows skipped by OFFSET or excluded by LIMIT
		// accrue no row_produced/projection cost (they were still scanned + filtered above). Producing
		// a row, and each projection-list evaluation, accrue cost on emission — deferred to the drive
		// (streaming.md §4). (ORDER BY's sort comparisons are not metered — spec/design/cost.md §3.)
		start, end := windowBounds(int64(len(rows)))
		em = emitter{src: rows, start: start, end: end, mode: emitProject}
	}
	return em, nil
}

// ---- Uncorrelated subquery folding (spec/design/grammar.md §26) ----------------------
//
// After the whole statement tree is planned + the parameters bound, this bottom-up pass walks
// every reSubquery node in the plan tree: it first folds within the node's own sub-plan, then —
// if the subquery references NO enclosing scope (a global constant, PG's "initplan") — executes
// it ONCE and replaces it with a constant (scalar -> its value; EXISTS -> a boolean; IN -> an
// reInValues over the result column), accruing the subquery's cost once (preserving the committed
// once-only cost — cost.md §3). A CORRELATED subquery is left in place; the evaluator re-executes
// it per outer row. So after this pass the only surviving reSubquery nodes are correlated.

func (db *engine) foldUncorrelatedInPlan(plan *queryPlan, bound []Value, ctes cteCtx, cost *int64) error {
	if plan.sel != nil {
		return db.foldUncorrelatedInSelect(plan.sel, bound, ctes, cost)
	}
	if plan.values != nil {
		// A VALUES-body value may itself hold an (uncorrelated) scalar subquery to fold once before
		// the rows are produced (grammar.md §42; the §26 fold).
		for r := range plan.values.rows {
			for c := range plan.values.rows[r] {
				if err := db.foldUncorrelatedInRExpr(plan.values.rows[r][c], bound, ctes, cost); err != nil {
					return err
				}
			}
		}
		return nil
	}
	if plan.with != nil {
		// A nested WITH body is not folded here against the enclosing ctes — its inner subqueries
		// reference the nested CTEs (a different scope, materialized only when the node runs), so
		// they are left to the evaluator, exactly like a derived table's body (spec/design/cte.md
		// §7). The whole nested-WITH subquery is itself folded by the caller if uncorrelated
		// (executed once via execWithPlan).
		return nil
	}
	if err := db.foldUncorrelatedInPlan(&plan.setop.lhs, bound, ctes, cost); err != nil {
		return err
	}
	return db.foldUncorrelatedInPlan(&plan.setop.rhs, bound, ctes, cost)
}

func (db *engine) foldUncorrelatedInSelect(sp *selectPlan, bound []Value, ctes cteCtx, cost *int64) error {
	for k := range sp.joins {
		if sp.joins[k].on != nil {
			if err := db.foldUncorrelatedInRExpr(sp.joins[k].on, bound, ctes, cost); err != nil {
				return err
			}
		}
	}
	if sp.filter != nil {
		if err := db.foldUncorrelatedInRExpr(sp.filter, bound, ctes, cost); err != nil {
			return err
		}
	}
	if sp.having != nil {
		if err := db.foldUncorrelatedInRExpr(sp.having, bound, ctes, cost); err != nil {
			return err
		}
	}
	for i := range sp.aggSpecs {
		if sp.aggSpecs[i].operand != nil {
			if err := db.foldUncorrelatedInRExpr(sp.aggSpecs[i].operand, bound, ctes, cost); err != nil {
				return err
			}
		}
	}
	for _, p := range sp.projections {
		if err := db.foldUncorrelatedInRExpr(p, bound, ctes, cost); err != nil {
			return err
		}
	}
	// A set-returning relation's arguments may themselves contain an (uncorrelated) subquery to
	// fold once before the generator runs (functions.md §10).
	for i := range sp.rels {
		if sp.rels[i].srf != nil {
			for _, a := range sp.rels[i].srf.args {
				if err := db.foldUncorrelatedInRExpr(a, bound, ctes, cost); err != nil {
					return err
				}
			}
		}
	}
	return nil
}

// foldUncorrelatedInRExpr folds this node if it is an uncorrelated reSubquery, else recurses into
// its children. A reSubquery is mutated IN PLACE (*e = ...) so every pointer to it sees the fold.
func (db *engine) foldUncorrelatedInRExpr(e *rExpr, bound []Value, ctes cteCtx, cost *int64) error {
	if e.kind == reSubquery {
		// Bottom-up: fold within this subquery's own sub-plan (and its IN lhs) first, so a
		// globally-uncorrelated subquery nested inside it is already a constant before we run it.
		if e.lhs != nil {
			if err := db.foldUncorrelatedInRExpr(e.lhs, bound, ctes, cost); err != nil {
				return err
			}
		}
		if err := db.foldUncorrelatedInPlan(e.subPlan, bound, ctes, cost); err != nil {
			return err
		}
		if queryPlanReferencesOuter(e.subPlan, 0) {
			return nil // correlated — re-executed per outer row at eval
		}
		// Uncorrelated: execute ONCE and fold to a constant / reInValues.
		r, err := db.execQueryPlan(e.subPlan, nil, bound, ctes)
		if err != nil {
			return err
		}
		*cost += r.cost
		switch e.subKind {
		case sqScalar:
			if len(r.rows) > 1 {
				return newError(CardinalityViolation, "more than one row returned by a subquery used as an expression")
			}
			val := NullValue()
			if len(r.rows) == 1 {
				val = r.rows[0][0]
			}
			*e = *valueToRExpr(val)
		case sqExists:
			*e = rExpr{kind: reConstBool, cBool: (len(r.rows) > 0) != e.negated}
		case sqQuantified:
			// An uncorrelated quantified subquery folds to a constant-array reQuantified
			// (array-functions.md §11.6): its single column becomes a 1-D array and the node reuses
			// the array form's 3VL fold — no per-row re-execution.
			elems := make([]Value, len(r.rows))
			for i, row := range r.rows {
				elems[i] = row[0]
			}
			arr := &rExpr{kind: reConstArray, cArray: oneDimArray(elems)}
			*e = rExpr{kind: reQuantified, op: e.op, quantAll: e.quantAll, lhs: e.lhs, rhs: arr}
		default: // sqIn
			list := make([]Value, len(r.rows))
			for i, row := range r.rows {
				list[i] = row[0]
			}
			*e = rExpr{kind: reInValues, lhs: e.lhs, list: list, negated: e.negated}
		}
		return nil
	}
	// Recurse into the children of every other node (a subquery may nest anywhere). The fields
	// are only set for the relevant node kinds, so this is exhaustive without a per-kind switch.
	if e.operand != nil {
		if err := db.foldUncorrelatedInRExpr(e.operand, bound, ctes, cost); err != nil {
			return err
		}
	}
	if e.lhs != nil {
		if err := db.foldUncorrelatedInRExpr(e.lhs, bound, ctes, cost); err != nil {
			return err
		}
	}
	if e.rhs != nil {
		if err := db.foldUncorrelatedInRExpr(e.rhs, bound, ctes, cost); err != nil {
			return err
		}
	}
	for _, arm := range e.caseArms {
		if err := db.foldUncorrelatedInRExpr(arm.cond, bound, ctes, cost); err != nil {
			return err
		}
		if err := db.foldUncorrelatedInRExpr(arm.result, bound, ctes, cost); err != nil {
			return err
		}
	}
	if e.caseEls != nil {
		if err := db.foldUncorrelatedInRExpr(e.caseEls, bound, ctes, cost); err != nil {
			return err
		}
	}
	for _, a := range e.sargs {
		if err := db.foldUncorrelatedInRExpr(a, bound, ctes, cost); err != nil {
			return err
		}
	}
	return nil
}

// queryPlanReferencesOuter reports whether a plan references any scope STRICTLY OUTSIDE itself —
// i.e. it is correlated (spec/design/grammar.md §26). depth is how many nested-subquery frames we
// have descended INTO this plan (0 = its own clauses); an reOuterColumn at level points above iff
// level > depth. The fold pass calls it with depth 0 on a subquery's sub-plan to fold (uncorrelated)
// or leave (correlated) it.
func queryPlanReferencesOuter(plan *queryPlan, depth int) bool {
	if plan.sel != nil {
		return selectPlanReferencesOuter(plan.sel, depth)
	}
	if plan.values != nil {
		// A VALUES body is planned parent=nil, so its values hold no outer reference of their own; a
		// folded-in subquery, however, may correlate to the target scope.
		for r := range plan.values.rows {
			for c := range plan.values.rows[r] {
				if rexprReferencesOuter(plan.values.rows[r][c], depth) {
					return true
				}
			}
		}
		return false
	}
	if plan.with != nil {
		// A nested WITH adds no correlation frame: its body is at the same depth, and the CTE bodies
		// are planned parent=nil (no outer reference), so only the body can correlate (cte.md §7).
		return queryPlanReferencesOuter(&plan.with.body, depth)
	}
	return queryPlanReferencesOuter(&plan.setop.lhs, depth) || queryPlanReferencesOuter(&plan.setop.rhs, depth)
}

func selectPlanReferencesOuter(sp *selectPlan, depth int) bool {
	for k := range sp.joins {
		if sp.joins[k].on != nil && rexprReferencesOuter(sp.joins[k].on, depth) {
			return true
		}
	}
	if sp.filter != nil && rexprReferencesOuter(sp.filter, depth) {
		return true
	}
	if sp.having != nil && rexprReferencesOuter(sp.having, depth) {
		return true
	}
	for i := range sp.aggSpecs {
		if sp.aggSpecs[i].operand != nil && rexprReferencesOuter(sp.aggSpecs[i].operand, depth) {
			return true
		}
	}
	for _, p := range sp.projections {
		if rexprReferencesOuter(p, depth) {
			return true
		}
	}
	// A materialized ORDER BY expression may itself carry a correlated reference (query.order_by_correlated):
	// a subquery whose ONLY outer reference is in its ORDER BY is still correlated and must re-execute per
	// outer row (else its OuterColumn reads an empty outer-row environment).
	for _, oe := range sp.orderExprs {
		if rexprReferencesOuter(oe, depth) {
			return true
		}
	}
	// A set-returning relation's arguments may carry a correlated reference (an implicitly-lateral
	// SRF arg sees params / outer / an earlier sibling — functions.md §10, grammar.md §44), making
	// the enclosing query correlated.
	for i := range sp.rels {
		if sp.rels[i].srf != nil {
			for _, a := range sp.rels[i].srf.args {
				if rexprReferencesOuter(a, depth) {
					return true
				}
			}
		}
		// A LATERAL derived table's body is one frame deeper; a reference in it back into this
		// query's outer counts here so the enclosing item is correctly flagged correlated (§44).
		if sp.rels[i].derived != nil && queryPlanReferencesOuter(sp.rels[i].derived, depth+1) {
			return true
		}
	}
	return false
}

func rexprReferencesOuter(e *rExpr, depth int) bool {
	switch e.kind {
	case reOuterColumn:
		return e.level > depth
	case reSubquery:
		// A nested subquery's own clauses are one frame deeper; its IN lhs is at this frame.
		if e.lhs != nil && rexprReferencesOuter(e.lhs, depth) {
			return true
		}
		return queryPlanReferencesOuter(e.subPlan, depth+1)
	case reInValues:
		return rexprReferencesOuter(e.lhs, depth)
	}
	if e.operand != nil && rexprReferencesOuter(e.operand, depth) {
		return true
	}
	if e.lhs != nil && rexprReferencesOuter(e.lhs, depth) {
		return true
	}
	if e.rhs != nil && rexprReferencesOuter(e.rhs, depth) {
		return true
	}
	for _, arm := range e.caseArms {
		if rexprReferencesOuter(arm.cond, depth) || rexprReferencesOuter(arm.result, depth) {
			return true
		}
	}
	if e.caseEls != nil && rexprReferencesOuter(e.caseEls, depth) {
		return true
	}
	for _, a := range e.sargs {
		if rexprReferencesOuter(a, depth) {
			return true
		}
	}
	return false
}

// collectTouched marks the combined-row columns an expression STATICALLY references — the
// touched set (cost.md §3 "The touched set"; large-values.md §14). Depth bookkeeping mirrors
// rexprReferencesOuter: walking the target plan's own clauses is depth 0 (a column touches);
// inside a nested subquery a column indexes the subquery's own row (ignored) and an outer
// column with level == depth is a correlated reference back into the target scope (touches).
// Purely syntactic — a never-taken CASE branch still touches — so the set is deterministic and
// cross-core identical (a §8 contract).
func collectTouched(e *rExpr, depth int, touched []bool) {
	if e == nil {
		return
	}
	switch e.kind {
	case reColumn:
		// A reColumn index beyond the real columns is a SYNTHETIC slot (an aggregate or window
		// result, spec/design/window.md §5.1), not a table column — it touches no stored data, so
		// the bound check skips it rather than panicking.
		if depth == 0 && e.index < len(touched) {
			touched[e.index] = true
		}
		return
	case reOuterColumn:
		// A correlated reference into the scope we are collecting for (its frame is `depth` levels up).
		// The index is a slot in that target scope's combined row; bounds-checked like the reColumn case
		// (a synthetic slot beyond the real columns touches no stored data). Callers collect at the depth
		// matching the reference's level — a correlated subquery at its nesting depth, a LATERAL SRF arg
		// at depth 1 (its sibling frame — functions.md §10).
		if e.level == depth && depth > 0 && e.index < len(touched) {
			touched[e.index] = true
		}
		return
	case reSubquery:
		collectTouched(e.lhs, depth, touched)
		collectTouchedPlan(e.subPlan, depth+1, touched)
		return
	case reInValues:
		collectTouched(e.lhs, depth, touched)
		return
	}
	collectTouched(e.operand, depth, touched)
	collectTouched(e.lhs, depth, touched)
	collectTouched(e.rhs, depth, touched)
	for _, arm := range e.caseArms {
		collectTouched(arm.cond, depth, touched)
		collectTouched(arm.result, depth, touched)
	}
	collectTouched(e.caseEls, depth, touched)
	for _, a := range e.sargs {
		collectTouched(a, depth, touched)
	}
}

// windowResultBase is the placeholder base a window query's window results carry until
// rebaseWindowResults rewrites them to inputWidth+len(windowKeys)+w (spec/design/window.md §5.1).
// Far above any real column/synthetic-slot count, and below 2^31 so it is valid on a 32-bit usize
// (the Rust wasm32 build) as well as f64-exact in the TS core. Kept identical across the three cores.
const windowResultBase = 1 << 28

// windowKeyBase is the placeholder base a materialized window-key expression (a non-column PARTITION
// BY / ORDER BY key — `PARTITION BY a + b`) carries until the rebase pass rewrites it to its real
// synthetic slot inputWidth+k (spec/design/window.md §5.1). Disjoint from windowResultBase's range,
// below 2^31 (32-bit-usize / wasm32 safe). A bare-column key is NOT materialized — it keeps its real row slot.
const windowKeyBase = 1 << 29

// groupingGsBase is the placeholder base a GROUPING(...) call carries until the rebase pass rewrites
// it to its real trailing synthetic slot len(groupKeys)+len(aggSpecs)+g (the GROUPING results follow
// the master columns + aggregate results — spec/design/aggregates.md §12). Disjoint from the window
// bases, below 2^31 (32-bit-usize / wasm32 safe). GROUPING is mutually exclusive with window functions, so its
// placeholders never coexist with the window ones in a projection.
const groupingGsBase = 1 << 30

// orderExprBase is the placeholder base a materialized ORDER BY EXPRESSION key's sort slot carries
// until it is rebased to its real trailing slot final_row_width+k (the materialized order values are
// appended after the input / window / grouped columns — grammar.md §10). Used only in the orderSlot
// idx field (a different namespace from the rExpr column bases above), but kept disjoint and below
// 2^31 (32-bit-usize / wasm32 safe) for the same reasons. A column / ordinal key keeps its real slot.
const orderExprBase = 1 << 27

// maxGroupingSets bounds a GROUP BY's total expansion (CUBE of n columns alone is 2^n). Beyond this
// the statement is aborted 54001 (statement_too_complex) — jed's structural-complexity gate (a
// deliberate divergence from PostgreSQL's per-construct "CUBE is limited to 12 elements" / 54011;
// jed bounds the total expansion instead). spec/design/aggregates.md §12.
const maxGroupingSets = 4096

// groupSetPlan is one resolved grouping set of a GROUP BY (spec/design/aggregates.md §12). A plain
// GROUP BY has exactly one; ROLLUP/CUBE/GROUPING SETS produce several. Each is bucketed independently
// over the post-WHERE rows and its groups projected into the shared synthetic row, whose first
// len(groupKeys) slots are the master grouping columns (the ordered union of all sets' columns).
type groupSetPlan struct {
	// keyCols are the flat input-row indices this set buckets on (its key, in key order). Empty = one
	// grand-total group (always emits one row, even over an empty input — the () / whole-table case).
	keyCols []int
	// slotSrc is per master grouping-column slot (length len(groupKeys)): >= 0 if this set includes
	// that column (its synthetic value is the bucket key's slotSrc[p]-th component), else -1, meaning
	// the column is not grouped in this set and its synthetic value is NULL.
	slotSrc []int
	// mask is the GROUPING() bitmask for rows from this set: bit p is set iff master slot p is NOT in
	// this set.
	mask int64
}

// groupItemSetCount is the number of grouping sets a single GROUP BY term expands to, saturating well
// below the int max so a huge CUBE cannot overflow the product before the maxGroupingSets check.
func groupItemSetCount(item *groupItem) int {
	switch item.Kind {
	case groupSet:
		return 1
	case groupRollup:
		return len(item.Groups) + 1
	case groupCube:
		if len(item.Groups) >= 20 {
			return maxGroupingSets + 1
		}
		return 1 << len(item.Groups)
	case groupGroupingSets:
		total := 0
		for i := range item.Elems {
			total += groupItemSetCount(&item.Elems[i])
			if total > maxGroupingSets {
				return maxGroupingSets + 1
			}
		}
		return total
	}
	return 1
}

// expandGroupItem expands a single GROUP BY term into its grouping sets, each a list of column Exprs
// (ROLLUP/CUBE/GROUPING SETS and nesting — spec/design/aggregates.md §12). The per-set column order
// is textual; the set order is deterministic and identical across cores (tests compare with rowsort).
func expandGroupItem(item *groupItem) [][]*exprNode {
	switch item.Kind {
	case groupSet:
		set := make([]*exprNode, len(item.Cols))
		for i := range item.Cols {
			set[i] = &item.Cols[i]
		}
		return [][]*exprNode{set}
	case groupRollup:
		// The prefixes longest-first down to the empty set — n+1 sets.
		out := make([][]*exprNode, 0, len(item.Groups)+1)
		for k := len(item.Groups); k >= 0; k-- {
			var set []*exprNode
			for i := 0; i < k; i++ {
				for j := range item.Groups[i] {
					set = append(set, &item.Groups[i][j])
				}
			}
			out = append(out, set)
		}
		return out
	case groupCube:
		// Every subset of the column groups — 2^n sets (bit i = include group i).
		n := len(item.Groups)
		out := make([][]*exprNode, 0, 1<<n)
		for mask := 0; mask < (1 << n); mask++ {
			var set []*exprNode
			for i := 0; i < n; i++ {
				if mask&(1<<i) != 0 {
					for j := range item.Groups[i] {
						set = append(set, &item.Groups[i][j])
					}
				}
			}
			out = append(out, set)
		}
		return out
	case groupGroupingSets:
		var out [][]*exprNode
		for i := range item.Elems {
			out = append(out, expandGroupItem(&item.Elems[i])...)
		}
		return out
	}
	return nil
}

// expandGroupBy expands a whole GROUP BY clause into its grouping sets: the cross-product of the
// top-level terms' expansions. An empty clause yields one empty set (the whole-table grand total).
// Aborts 54001 if the expansion exceeds maxGroupingSets (spec/design/aggregates.md §12).
func expandGroupBy(items []groupItem) ([][]*exprNode, error) {
	total := 1
	for i := range items {
		total *= groupItemSetCount(&items[i])
		if total > maxGroupingSets {
			return nil, newError(StatementTooComplex, fmt.Sprintf("too many grouping sets (the limit is %d)", maxGroupingSets))
		}
	}
	acc := [][]*exprNode{{}}
	for i := range items {
		exp := expandGroupItem(&items[i])
		next := make([][]*exprNode, 0, len(acc)*len(exp))
		for _, a := range acc {
			for _, s := range exp {
				combined := make([]*exprNode, 0, len(a)+len(s))
				combined = append(combined, a...)
				combined = append(combined, s...)
				next = append(next, combined)
			}
		}
		acc = next
	}
	return acc, nil
}

// groupKeyExpr records a general-expression GROUP BY key (`GROUP BY a + b`, aggregates.md §15): its
// canonical AST (so a matching projection / HAVING / ORDER BY expression resolves to its synthetic
// slot) and its resolved type.
type groupKeyExpr struct {
	canon exprNode
	ty    resolvedType
}

// groupKeyResolved is the resolution of one GROUP BY grouping term (aggregates.md §15): either an
// input COLUMN at a flat row index (isColumn, index), or a general EXPRESSION to materialize
// (node + ty + canonical AST). Mirrors Rust's GroupKeyResolved enum.
type groupKeyResolved struct {
	isColumn bool
	index    int    // valid when isColumn
	node     *rExpr // valid when !isColumn — the materialized expression
	ty       resolvedType
	canon    exprNode // valid when !isColumn — the canonical AST kept for projection matching
}

// resolveGroupTerm resolves one GROUP BY grouping term to a column or a materialized expression
// (aggregates.md §15). Classifies the term: a bare integer literal is a select-list ORDINAL (1-based;
// out of range 42P10) whose target select item is then resolved as a term; otherwise it is a column
// / alias / general expression (resolveGroupNamed).
func resolveGroupTerm(s *scope, term exprNode, items selectItems, params *paramTypes) (groupKeyResolved, error) {
	// Only a *bare* integer literal is an ordinal — `GROUP BY 1`; `GROUP BY 1 + 1` is a constant
	// expression (PG). The parser folds a unary minus into the value, so a negative is just out of
	// range. The select list fixes the position count: `*` expands to the scope width.
	if term.Kind == exprLiteral && term.Literal != nil && term.Literal.Kind == literalInt {
		n := term.Literal.Int
		var ncols int64
		if items.All {
			ncols = int64(s.width())
		} else {
			ncols = int64(len(items.Items))
		}
		if n < 1 || n > ncols {
			return groupKeyResolved{}, newError(InvalidColumnReference,
				fmt.Sprintf("GROUP BY position %d is not in select list", n))
		}
		pos := int(n - 1)
		if items.All {
			// `SELECT *` — the ordinal names the column at that scope position directly.
			return groupKeyResolved{isColumn: true, index: pos}, nil
		}
		return resolveGroupExpr(s, items.Items[pos].Expr, params)
	}
	return resolveGroupNamed(s, term, items, params)
}

// resolveGroupNamed resolves a non-ordinal grouping term: a bare/qualified column, an output alias,
// or a general expression (aggregates.md §15). A bare name resolves an INPUT column FIRST, then —
// only if there is no such column — an output alias (PG's rule, the opposite of ORDER BY's
// output-first rule).
func resolveGroupNamed(s *scope, term exprNode, items selectItems, params *paramTypes) (groupKeyResolved, error) {
	switch term.Kind {
	case exprColumn:
		r, err := s.resolveBare(term.Column)
		if err != nil {
			// No input column of this name: try an output alias (`SELECT a+b AS s … GROUP BY s`). If
			// none matches either, propagate the original 42703.
			if se, ok := err.(*EngineError); ok && se.State == UndefinedColumn {
				aexpr, aerr := orderAliasMatch(items, term.Column, s)
				if aerr != nil {
					return groupKeyResolved{}, aerr
				}
				if aexpr != nil {
					return resolveGroupExpr(s, *aexpr, params)
				}
			}
			return groupKeyResolved{}, err
		}
		if r.level != 0 {
			return groupKeyResolved{}, newError(FeatureNotSupported, "GROUP BY may not reference an outer query column")
		}
		return groupKeyResolved{isColumn: true, index: r.index}, nil
	case exprQualifiedColumn:
		r, err := s.resolveQualified(term.Qualifier, term.Column)
		if err != nil {
			return groupKeyResolved{}, err
		}
		if r.level != 0 {
			return groupKeyResolved{}, newError(FeatureNotSupported, "GROUP BY may not reference an outer query column")
		}
		return groupKeyResolved{isColumn: true, index: r.index}, nil
	default:
		return resolveGroupExpr(s, term, params)
	}
}

// resolveGroupExpr resolves a grouping expression (the target of an ordinal/alias, or a general
// `GROUP BY a+b`). A plain column expression stays a COLUMN key (so the projection's bare-column path
// matches it); anything else is MATERIALIZED — resolved against the input row with aggregates
// forbidden (an aggregate in GROUP BY is 42803), its canonical AST kept for projection matching
// (aggregates.md §15).
func resolveGroupExpr(s *scope, e exprNode, params *paramTypes) (groupKeyResolved, error) {
	switch e.Kind {
	case exprColumn:
		if r, err := s.resolveBare(e.Column); err == nil && r.level == 0 {
			return groupKeyResolved{isColumn: true, index: r.index}, nil
		}
	case exprQualifiedColumn:
		if r, err := s.resolveQualified(e.Qualifier, e.Column); err == nil && r.level == 0 {
			return groupKeyResolved{isColumn: true, index: r.index}, nil
		}
	}
	sub := &aggCtx{collecting: false}
	node, ty, err := resolve(s, e, nil, sub, params)
	if err != nil {
		return groupKeyResolved{}, err
	}
	return groupKeyResolved{node: node, ty: ty, canon: e}, nil
}

// matchGroupExpr reports whether e structurally matches a general-expression GROUP BY key in this
// aggregate context; if so it returns that group's synthetic key slot (its master position) and
// resolved type (aggregates.md §15). Only fires in a collecting context with groupKeyExprs; an
// aggregate operand / FILTER resolves under Forbidden (no groupKeyExprs), so a grouping expression
// there is correctly NOT remapped (it is a per-row value, not the group key).
func matchGroupExpr(ag *aggCtx, e exprNode) (int, resolvedType, bool) {
	if ag == nil {
		return 0, resolvedType{}, false
	}
	for p, gk := range ag.groupKeyExprs {
		if gk != nil && exprEqual(gk.canon, e) {
			return p, gk.ty, true
		}
	}
	return 0, resolvedType{}, false
}

// groupingValue computes a GROUPING(args) result for a group from the grouping set whose mask is
// given: bit (k-1-j) of the result is bit positions[j] of mask (spec/design/aggregates.md §12).
func groupingValue(positions []int, mask int64) int64 {
	k := len(positions)
	var r int64
	for j, p := range positions {
		bit := (mask >> uint(p)) & 1
		r |= bit << uint(k-1-j)
	}
	return r
}

// rebasePlaceholderCols rewrites placeholder column slots in [from, 2·from) — a window-result
// (windowResultBase+w), a materialized window-key (windowKeyBase+k), or a GROUPING() (groupingGsBase+g)
// placeholder — to their real synthetic slot target+(slot-from), once the grouped/windowed row layout
// is final (spec/design/window.md §5.1, aggregates.md §12/§21). During resolution a window result of
// index w is assigned the placeholder windowResultBase+w, because its real slot
// len(groupKeys)+len(aggSpecs)+w is unknown until every aggregate (including any nested in a later
// window argument or HAVING) has been collected. Each placeholder base is 2× the previous (1<<28,
// 1<<29, 1<<30) and a base's placeholder count is far below that gap, so bounding the rewrite to
// [from, 2·from) keeps the bases isolated — a window-result rebase no longer clobbers a GROUPING()
// placeholder (the two now COEXIST in a GROUPING SETS + window query — aggregates.md §21). It descends
// into a subquery's lhs (current row space) but NOT its plan (those columns index the subquery's own
// rows; a nested grouped+window plan was already rebased when it was built).
func rebasePlaceholderCols(e *rExpr, from, target int) {
	if e == nil {
		return
	}
	switch e.kind {
	case reColumn:
		if e.index >= from && e.index < from*2 {
			e.index = target + (e.index - from)
		}
		return
	case reOuterColumn:
		return
	case reSubquery:
		rebasePlaceholderCols(e.lhs, from, target) // current row space only; not subPlan
		return
	case reInValues:
		rebasePlaceholderCols(e.lhs, from, target)
		return
	}
	rebasePlaceholderCols(e.operand, from, target)
	rebasePlaceholderCols(e.lhs, from, target)
	rebasePlaceholderCols(e.rhs, from, target)
	for _, arm := range e.caseArms {
		rebasePlaceholderCols(arm.cond, from, target)
		rebasePlaceholderCols(arm.result, from, target)
	}
	rebasePlaceholderCols(e.caseEls, from, target)
	for _, a := range e.sargs {
		rebasePlaceholderCols(a, from, target)
	}
	for _, sub := range e.subs {
		rebasePlaceholderCols(sub.index, from, target)
		rebasePlaceholderCols(sub.lower, from, target)
		rebasePlaceholderCols(sub.upper, from, target)
	}
}

// collectTouchedPlan walks a nested plan's expression surfaces for outer references back into
// the target scope — the same five surfaces selectPlanReferencesOuter checks (slot lists like
// group keys / ORDER BY index the nested plan's own rows and can never reach outward).
func collectTouchedPlan(plan *queryPlan, depth int, touched []bool) {
	if plan == nil {
		return
	}
	if plan.sel != nil {
		sp := plan.sel
		for k := range sp.joins {
			collectTouched(sp.joins[k].on, depth, touched)
		}
		collectTouched(sp.filter, depth, touched)
		collectTouched(sp.having, depth, touched)
		for i := range sp.aggSpecs {
			collectTouched(sp.aggSpecs[i].operand, depth, touched)
		}
		for _, p := range sp.projections {
			collectTouched(p, depth, touched)
		}
		// A materialized ORDER BY expression and a set-returning relation's args / a LATERAL derived
		// body can each carry a correlated reference back into the target scope (the same surfaces
		// selectPlanReferencesOuter checks — query.order_by_correlated, functions.md §10, grammar.md
		// §44). collectTouchedPlan MUST cover every surface that function does, or an outer column read
		// only through one of them is left unfetched by the lazy/masked scan (large-values.md §14) and
		// the correlated subquery re-executes against NULL — a memory-vs-disk divergence.
		for _, oe := range sp.orderExprs {
			collectTouched(oe, depth, touched)
		}
		for i := range sp.rels {
			if sp.rels[i].srf != nil {
				for _, a := range sp.rels[i].srf.args {
					collectTouched(a, depth, touched)
				}
			}
			if sp.rels[i].derived != nil {
				collectTouchedPlan(sp.rels[i].derived, depth+1, touched)
			}
		}
	}
	if plan.values != nil {
		for r := range plan.values.rows {
			for c := range plan.values.rows[r] {
				collectTouched(plan.values.rows[r][c], depth, touched)
			}
		}
	}
	if plan.setop != nil {
		collectTouchedPlan(&plan.setop.lhs, depth, touched)
		collectTouchedPlan(&plan.setop.rhs, depth, touched)
	}
	if plan.with != nil {
		// A nested WITH's correlated references live in its body (the CTE bodies are parent=nil);
		// recurse into the body at the same depth (spec/design/cte.md §7).
		collectTouchedPlan(&plan.with.body, depth, touched)
	}
}

// inMembership is three-valued `lhs IN (list)` membership (spec/design/grammar.md §26), charging
// one operator_eval per element compared. An EMPTY list is `negated` (x IN () = FALSE, x NOT IN ()
// = TRUE) independent of lv. Otherwise: a positive match -> TRUE; else a NULL element (or NULL lv)
// -> NULL; else FALSE. NOT IN is the Kleene negation. Shared by reInValues and the correlated
// reSubquery/sqIn eval.
func inMembership(lv Value, list []Value, negated bool, m *costMeter) (Value, error) {
	if len(list) == 0 {
		return BoolValue(negated), nil
	}
	anyMatch := false
	anyNull := false
	for _, v := range list {
		m.Charge(costs.OperatorEval)
		// Each element comparison over a decimal pair charges its size-scaled decimal_work
		// (spec/design/cost.md §3 "decimal_work"), like a compare node.
		m.Charge(costs.DecimalWork * (decimalCmpWork(lv, v) - 1))
		if err := m.Guard(); err != nil {
			return Value{}, err
		}
		switch lv.Eq3(v) {
		case True:
			anyMatch = true
		case Unknown:
			anyNull = true
		}
	}
	var inVal Value
	switch {
	case anyMatch:
		inVal = BoolValue(true)
	case anyNull:
		inVal = NullValue()
	default:
		inVal = BoolValue(false)
	}
	if negated {
		return boolNot(inVal), nil
	}
	return inVal, nil
}

// quantifiedMembership is the three-valued membership fold for `lhs op ANY/ALL(array)`
// (array-functions.md §11), the generalization of inMembership to all five comparison operators and
// both quantifiers. A NULL array → NULL; otherwise, over the flattened elements, ANY/SOME (all=false)
// is the OR-fold (TRUE if any `lhs op e` is TRUE, else NULL if any is NULL, else FALSE; empty →
// FALSE) and ALL (all=true) the AND-fold (FALSE if any is FALSE, else NULL if any is NULL, else TRUE;
// empty → TRUE). Each element comparison charges one operator_eval (+ size-scaled decimal_work),
// exactly like inMembership, so max_cost bounds the walk (54P01).
func quantifiedMembership(op binaryOp, all bool, lv, av Value, m *costMeter) (Value, error) {
	if av.Kind == ValNull {
		return NullValue(), nil
	}
	anyNull := false
	for _, e := range av.arrayVal().Elements {
		m.Charge(costs.OperatorEval)
		m.Charge(costs.DecimalWork * (decimalCmpWork(lv, e) - 1))
		if err := m.Guard(); err != nil {
			return Value{}, err
		}
		switch quantifiedCmp3(op, lv, e) {
		case True:
			// ANY short-circuits TRUE; ALL keeps going (TRUE is its neutral element).
			if !all {
				return BoolValue(true), nil
			}
		case False:
			// ALL short-circuits FALSE; ANY keeps going (FALSE is its neutral element).
			if all {
				return BoolValue(false), nil
			}
		case Unknown:
			anyNull = true
		}
	}
	// Drained without a short-circuit: a NULL seen → UNKNOWN; else the quantifier's identity (ALL →
	// TRUE, ANY → FALSE — also the empty-array result).
	if anyNull {
		return NullValue(), nil
	}
	return BoolValue(all), nil
}

// quantifiedCmp3 is the per-element three-valued comparison `lhs op e` for a quantified node,
// normalizing a mixed-width float pair to f64 first (the resolver admits f32 vs f64,
// matching reCompare's promote — here the array elements are runtime values, so the widen happens per
// element). Bottoms out in the value module's Eq3/Lt3/Gt3 kernels.
//
// A composite operand pair routes through the composite TOTAL ORDER (valueCmp), NOT the bare-ROW 3VL
// Eq3/Lt3/Gt3 (array-functions.md §13): PostgreSQL's = ANY(addr[]) dispatches on the composite =
// operator = record_eq, which is DEFINITE with NULL fields comparable (ROW('a',NULL)::addr =
// ANY(ARRAY[ROW('a',NULL)::addr]) is TRUE), the same total order array_eq / @> already use for
// composite elements (array.md §5). A whole-element NULL is still UNKNOWN — the operator stays strict
// at the value level — so the resolver-guaranteed same-type pair is composite-vs-composite or
// composite-vs-NULL.
func quantifiedCmp3(op binaryOp, x, e Value) ThreeValued {
	if x.Kind == ValComposite || e.Kind == ValComposite {
		if x.Kind == ValNull || e.Kind == ValNull {
			return Unknown
		}
		ord := valueCmp(x, e)
		var matched bool
		switch op {
		case opEq:
			matched = ord == 0
		case opNe:
			matched = ord != 0
		case opLt:
			matched = ord < 0
		case opGt:
			matched = ord > 0
		case opLe:
			matched = ord <= 0
		default: // OpGe
			matched = ord >= 0
		}
		if matched {
			return True
		}
		return False
	}
	if x.Kind == ValFloat32 && e.Kind == ValFloat64 {
		x = Float64Value(float64(x.F32()))
	} else if x.Kind == ValFloat64 && e.Kind == ValFloat32 {
		e = Float64Value(float64(e.F32()))
	}
	switch op {
	case opEq:
		return x.Eq3(e)
	case opNe:
		return not3(x.Eq3(e))
	case opLt:
		return x.Lt3(e)
	case opGt:
		return x.Gt3(e)
	case opLe:
		return or3(x.Lt3(e), x.Eq3(e))
	default: // OpGe
		return or3(x.Gt3(e), x.Eq3(e))
	}
}

// valueToRExpr builds the constant rExpr for a folded subquery value (§26). The static type is
// carried separately (the node's Type), so a NULL value here is just reConstNull.
func valueToRExpr(v Value) *rExpr {
	switch v.Kind {
	case ValInt:
		return &rExpr{kind: reConstInt, cInt: v.Int}
	case ValBool:
		return &rExpr{kind: reConstBool, cBool: v.boolVal()}
	case ValText:
		return &rExpr{kind: reConstText, cText: v.str()}
	case ValDecimal:
		return &rExpr{kind: reConstDecimal, cDec: *v.decimal()}
	case ValBytea:
		return &rExpr{kind: reConstBytea, cBytea: []byte(v.str())}
	case ValUuid:
		return &rExpr{kind: reConstUuid, cBytea: []byte(v.str())}
	case ValTimestamp:
		return &rExpr{kind: reConstTimestamp, cInt: v.Int}
	case ValTimestamptz:
		return &rExpr{kind: reConstTimestamptz, cInt: v.Int}
	case ValDate:
		return &rExpr{kind: reConstDate, cInt: v.Int}
	case ValInterval:
		return &rExpr{kind: reConstInterval, cIv: v.interval()}
	case ValComposite:
		// A folded composite value rebuilds as a ROW(...) of its per-field constant nodes
		// (spec/design/composite.md), so the recursive structure round-trips.
		nodes := make([]*rExpr, len(*v.composite()))
		for i, f := range *v.composite() {
			nodes[i] = valueToRExpr(f)
		}
		return &rExpr{kind: reRow, sargs: nodes}
	case ValArray:
		// A folded array constant — preserve its full shape (dims/lbounds) in a const node.
		return &rExpr{kind: reConstArray, cArray: v.arrayVal()}
	case ValRange:
		// A folded range constant (already canonical).
		return &rExpr{kind: reConstRange, cRange: v.rangeVal()}
	case ValJson:
		return &rExpr{kind: reConstJson, cText: v.str()}
	case ValJsonPath:
		return &rExpr{kind: reConstJsonPath, cText: v.str()}
	case ValJsonb:
		return &rExpr{kind: reConstJsonb, cJsonb: v.jsonb()}
	default: // ValNull
		return &rExpr{kind: reConstNull}
	}
}

// distinctRowKey encodes a projected row into a collision-free string key for DISTINCT
// dedup. Each field carries a type tag (n/i/b) and a payload, joined by a separator that
// no field can contain, so e.g. (1,23) and (12,3) do not collide (spec/design/grammar.md
// §11). NULL == NULL falls out (both encode to "n"), matching the NULL-safe DISTINCT rule.
func distinctRowKey(row []Value) string {
	var b strings.Builder
	for i, v := range row {
		if i > 0 {
			b.WriteByte('|')
		}
		switch v.Kind {
		case ValNull:
			b.WriteByte('n')
		case ValInt:
			b.WriteByte('i')
			b.WriteString(strconv.FormatInt(v.Int, 10))
		case ValBool:
			b.WriteByte('b')
			if v.boolVal() {
				b.WriteByte('1')
			} else {
				b.WriteByte('0')
			}
		case ValText:
			// Length-prefix the content so the separator byte cannot be confused with a
			// text value that contains it (the value bytes are arbitrary UTF-8).
			b.WriteByte('t')
			b.WriteString(strconv.Itoa(len(v.str())))
			b.WriteByte(':')
			b.WriteString(v.str())
		case ValDecimal:
			// Value-canonical key so 1.5 and 1.50 collapse to one DISTINCT bucket
			// (spec/design/decimal.md §5).
			b.WriteByte('d')
			b.WriteString(v.decimal().CanonicalString())
		case ValBytea:
			// Length-prefix the raw bytes (held in Str; a distinct 'y' tag, so a bytea never
			// collides with a text value of the same bytes).
			b.WriteByte('y')
			b.WriteString(strconv.Itoa(len(v.str())))
			b.WriteByte(':')
			b.WriteString(v.str())
		case ValUuid:
			// The 16 raw bytes (held in Str), under a distinct 'u' tag so a uuid never collides
			// with a bytea/text of the same bytes. Fixed-width, but length-prefixed for symmetry.
			b.WriteByte('u')
			b.WriteString(strconv.Itoa(len(v.str())))
			b.WriteByte(':')
			b.WriteString(v.str())
		case ValTimestamp:
			// The i64 microsecond instant (held in Int), under a distinct 's' tag. Two literals
			// for the same instant (e.g. 12:00:00 and 12:00:00.0) share the int, so they bucket
			// together; the infinity sentinels are ordinary int values with their own buckets.
			b.WriteByte('s')
			b.WriteString(strconv.FormatInt(v.Int, 10))
		case ValTimestamptz:
			// The i64 UTC-instant micros (held in Int), under a distinct 'z' tag: offsets are
			// already normalized to UTC at parse, so +00 and +05-of-the-same-instant bucket together.
			b.WriteByte('z')
			b.WriteString(strconv.FormatInt(v.Int, 10))
		case ValDate:
			// The i32 day count (held in Int), under a distinct 'd' tag.
			b.WriteByte('d')
			b.WriteString(strconv.FormatInt(v.Int, 10))
		case ValInterval:
			// The canonical 128-bit span as a decimal string, under a distinct 'v' tag, so
			// span-equal intervals ('1 mon' / '30 days' / '720:00:00') collapse to one DISTINCT/
			// GROUP BY bucket while each value still renders its own fields (spec/design/interval.md §2).
			b.WriteByte('v')
			b.WriteString(v.interval().Span().String())
		case ValFloat32, ValFloat64:
			// Float DISTINCT / GROUP BY uses the §3 total order's equivalence classes: -0 → +0 and
			// ALL NaNs collapse to one bucket (spec/design/float.md §3). Key on the CANONICAL form —
			// a canonical NaN pattern, and +0 for ±0 — so -0/+0 and any two NaNs share a bucket. A
			// distinct 'f' tag (a column is one float width, so the width need not enter the key).
			b.WriteByte('f')
			b.WriteString(floatCanonicalKey(v.asF64()))
		case ValComposite:
			// A composite keys structurally (spec/design/composite.md §2/§5): the field count under a
			// distinct 'c' tag, then each field's own key recursively. NULL fields key as 'n' (the
			// value-level structural equality, like decimal/interval), so two composites with the same
			// field values share a DISTINCT/GROUP BY bucket; a nested composite recurses.
			b.WriteByte('c')
			b.WriteString(strconv.Itoa(len(*v.composite())))
			b.WriteByte(':')
			b.WriteString(distinctRowKey(*v.composite()))
		case ValArray:
			// An array keys structurally INCLUDING its shape (spec/design/array.md §5): the
			// dims and lower bounds (so [2:4]={1,2,3} and {1,2,3} bucket apart — array_eq considers
			// them), then each element's own key recursively. NULL elements key as 'n' (btree
			// equality — NULLs mutually equal), so structurally-equal arrays share a bucket.
			a := v.arrayVal()
			b.WriteByte('a')
			b.WriteString(strconv.Itoa(len(a.Dims)))
			for _, d := range a.Dims {
				b.WriteByte(':')
				b.WriteString(strconv.Itoa(d))
			}
			for _, lb := range a.Lbounds {
				b.WriteByte(';')
				b.WriteString(strconv.FormatInt(int64(lb), 10))
			}
			b.WriteByte('=')
			b.WriteString(distinctRowKey(a.Elements))
		case ValRange:
			// A range keys structurally over its CANONICAL form (PG range btree — spec/design/ranges.md
			// §6), under a distinct 'r' tag: the empty flag, then each bound's presence (infinite = '_'),
			// inclusivity, and the bound value's own key recursively. Because the stored form is canonical,
			// two equal ranges produce the identical key (rangeTotalCmp == 0 ⇔ same key), so they share a
			// DISTINCT/GROUP BY bucket. NULL ranges key as 'n' (the whole-value NULL above).
			rv := v.rangeVal()
			b.WriteByte('r')
			if rv.Empty {
				b.WriteByte('e')
				break
			}
			b.WriteByte('n') // non-empty marker
			writeRangeBoundKey(&b, rv.Lower, rv.LowerInc)
			writeRangeBoundKey(&b, rv.Upper, rv.UpperInc)
		case ValJson:
			// json keys on its verbatim text under a distinct 'J' tag (the value-level equality,
			// consistent with the structural derive). Length-prefixed (arbitrary UTF-8 content).
			// Never reached through SQL in J0 (json is non-comparable — 42883).
			b.WriteByte('J')
			b.WriteString(strconv.Itoa(len(v.str())))
			b.WriteByte(':')
			b.WriteString(v.str())
		case ValJsonb:
			// jsonb keys on its CANONICAL text under a distinct 'B' tag (the canonical form makes
			// structural equality the value equality, §5; jsonbOut is byte-identical for equal trees,
			// so equal jsonb values share a DISTINCT/GROUP BY bucket). Length-prefixed.
			s := jsonbOut(v.jsonb())
			b.WriteByte('B')
			b.WriteString(strconv.Itoa(len(s)))
			b.WriteByte(':')
			b.WriteString(s)
		}
	}
	return b.String()
}

// writeRangeBoundKey appends one canonical range bound to a distinctRowKey buffer: '_' for an
// infinite (nil) bound, else the inclusivity flag ('[' / '(') and the bound value's own recursive key.
func writeRangeBoundKey(b *strings.Builder, bound *Value, inc bool) {
	if bound == nil {
		b.WriteByte('_')
		return
	}
	if inc {
		b.WriteByte('[')
	} else {
		b.WriteByte('(')
	}
	b.WriteString(distinctRowKey([]Value{*bound}))
}

// floatCanonicalKey is a collision-free string of a float's total-order equivalence class
// (spec/design/float.md §3): every NaN → "nan", -0 → +0, otherwise the shortest round-trip
// decimal. So -0/+0 and any two NaNs key identically (they dedup to one DISTINCT/GROUP BY bucket).
func floatCanonicalKey(f float64) string {
	if math.IsNaN(f) {
		return "nan"
	}
	return renderFloat64(canonicalizeFloat64(f))
}

// ============================================================================
// Resolved expression layer (mirrors impl/rust executor.rs).
//
// Parse → Expr (names) → resolve → rExpr (column indices, known result types, folded
// constants) → eval per row → Value. The resolver is where all type-checking and the
// literal range-check live; the evaluator is a pure tree-walk.
// ============================================================================

// rtKind tags the static type of a resolved expression.
type rtKind int

const (
	rtNull rtKind = iota // an untyped NULL literal
	rtInt                // integer; intTy carries the ScalarType
	rtBool
	rtText        // text (one family, collation C); does not promote
	rtDecimal     // decimal (one family; the per-column typmod is carried separately)
	rtBytea       // bytea (one family, raw bytes); does not promote
	rtUuid        // uuid (one family, fixed 16 bytes); does not promote. First non-integer key.
	rtTimestamp   // timestamp (zoneless instant); does not compare/cast to timestamptz
	rtTimestamptz // timestamptz (UTC instant); does not compare/cast to timestamp
	rtDate        // date (calendar date, i32 days); strict island, no compare/cast to timestamp
	rtInterval    // interval (a span); compares only with itself, by the canonical span
	rtFloat32     // f32 / real (binary32); promotes to f64; a strict island vs int/decimal
	rtFloat64     // f64 / double precision (binary64)
	// rtComposite is a composite (row) type (spec/design/composite.md §5): comp carries the
	// (optional) name and resolved field list. A named catalog type's name drives the `# types:`
	// output; an anonymous ROW(...) result has a nil name (rendered "record").
	rtComposite
	// rtArray is an array type (spec/design/array.md §2): elem carries the resolved element type.
	// Two arrays are comparable iff their element types are comparable; assignable to an array
	// column of the same element type.
	rtArray
	// rtRange is a range type (spec/design/ranges.md §2): elem carries the resolved element
	// (subtype) type. Two ranges are comparable iff their elements are equal; the element is one of
	// the six scalar subtypes that have a range.
	rtRange
	// rtJson is the json family (verbatim text — spec/design/json.md §4). NOT comparable (PG ships no
	// btree/hash opclass — §5): a comparison/ORDER BY/DISTINCT on json resolves to 42883.
	rtJson
	// rtJsonb is the jsonb family (canonical binary — spec/design/json.md §2). Comparable with itself
	// by PG's total btree order (§5).
	rtJsonb
	// rtJsonPath is the jsonpath type (spec/design/jsonpath.md, P1a). NOT comparable (42883);
	// literal-only.
	rtJsonPath
)

// isFloatKind reports whether a resolvedType is one of the two float kinds.
func isFloatKind(k rtKind) bool { return k == rtFloat32 || k == rtFloat64 }

// promoteFloat is the float promotion tower (compare.toml max-rank): a mixed-width pair widens to
// f64; same-width stays. Caller guarantees both kinds are float.
func promoteFloat(a, b rtKind) scalarType {
	if a == rtFloat64 || b == rtFloat64 {
		return scalarFloat64
	}
	return scalarFloat32
}

type resolvedType struct {
	kind  rtKind
	intTy scalarType      // valid when kind == rtInt
	comp  *compositeRType // valid when kind == rtComposite
	elem  *resolvedType   // valid when kind == rtArray (the element type)
}

// compositeRType is the resolved shape of a composite type — its (optional) name and resolved field
// list (spec/design/composite.md §5). name is "" (named=false) for an anonymous ROW(...) result, set
// for a named catalog type. fields are the resolved (field-name, type) pairs in declaration order.
type compositeRType struct {
	named  bool
	name   string
	fields []compositeRField
}

// compositeRField is one resolved (name, type) field of a compositeRType.
type compositeRField struct {
	name string
	ty   resolvedType
}

func intType(t resolvedType) (scalarType, bool) {
	if t.kind == rtInt {
		return t.intTy, true
	}
	return 0, false
}

// resolvedOfColumn is the resolved type of a stored column of ty — the output type of a bare
// column projection (SELECT * / SELECT col). A column always has a concrete type, never rtNull.
func resolvedOfColumn(ty scalarType) resolvedType {
	if ty.IsInteger() {
		return resolvedType{kind: rtInt, intTy: ty}
	}
	switch {
	case ty.IsBool():
		return resolvedType{kind: rtBool}
	case ty.IsText():
		return resolvedType{kind: rtText}
	case ty.IsDecimal():
		return resolvedType{kind: rtDecimal}
	case ty.IsBytea():
		return resolvedType{kind: rtBytea}
	case ty.IsTimestamp():
		return resolvedType{kind: rtTimestamp}
	case ty.IsTimestamptz():
		return resolvedType{kind: rtTimestamptz}
	case ty.IsDate():
		return resolvedType{kind: rtDate}
	case ty.IsInterval():
		return resolvedType{kind: rtInterval}
	case ty.IsFloat32():
		return resolvedType{kind: rtFloat32}
	case ty.IsFloat64():
		return resolvedType{kind: rtFloat64}
	default: // uuid
		return resolvedType{kind: rtUuid}
	}
}

// assignableTo reports whether a projected value of type t is assignable to a colTy column for
// storage — the FAMILY-level gate INSERT ... SELECT applies up front (spec/design/grammar.md
// §24), before any row is produced (so it fires even over an empty source). It is the
// family-level subset of storeValue and MUST agree with it: an integer assigns to an integer
// or decimal column (int→decimal widens), a decimal only to a decimal column (decimal→int is
// explicit-CAST only), text to text/uuid/bytea/timestamp/timestamptz (the documented text
// adaptation — the per-row store then parses, trapping 22P02/22007 on malformed input),
// boolean→boolean, uuid→uuid, bytea→bytea, a timestamp only to a timestamp column and a
// timestamptz only to a timestamptz column (the two never cross — they do not even compare,
// timestamp.md), and a NULL-typed projection to any column (a NOT NULL target then traps 23502
// per row). A non-assignable pair is a 42804.
func assignableTo(t resolvedType, colTy scalarType) bool {
	switch t.kind {
	case rtNull:
		return true
	case rtInt:
		return colTy.IsInteger() || colTy.IsDecimal()
	case rtDecimal:
		return colTy.IsDecimal()
	case rtBool:
		return colTy.IsBool()
	case rtText:
		return colTy.IsText() || colTy.IsUuid() || colTy.IsBytea() ||
			colTy.IsTimestamp() || colTy.IsTimestamptz() || colTy.IsInterval() || colTy.IsDate()
	case rtBytea:
		return colTy.IsBytea()
	case rtUuid:
		return colTy.IsUuid()
	case rtTimestamp:
		return colTy.IsTimestamp()
	case rtTimestamptz:
		return colTy.IsTimestamptz()
	case rtDate:
		return colTy.IsDate()
	case rtInterval:
		return colTy.IsInterval()
	case rtFloat32:
		// f32 assigns to a f32 OR a f64 column (the implicit, lossless widen — §2).
		return colTy.IsFloat32() || colTy.IsFloat64()
	case rtFloat64:
		// f64 assigns only to a f64 column (f64→f32 is explicit-CAST only).
		return colTy.IsFloat64()
	default:
		return false
	}
}

// rtName is t's type name, for a 42804 assignability message (the integer width is exact).
// typeNames renders a projection's resolved types as their canonical names for the public
// outcome.ColumnTypes — the `# types:` directive's assertion surface (spec/design/conformance.md
// §7). Same names as the 42804 message (rtName): the exact integer width, the unconstrained
// "decimal".
func typeNames(ts []resolvedType) []string {
	out := make([]string, len(ts))
	for i, t := range ts {
		out[i] = rtName(t)
	}
	return out
}

func rtName(t resolvedType) string {
	switch t.kind {
	case rtInt:
		return t.intTy.CanonicalName()
	case rtBool:
		return "boolean"
	case rtText:
		return "text"
	case rtDecimal:
		return "decimal"
	case rtBytea:
		return "bytea"
	case rtUuid:
		return "uuid"
	case rtTimestamp:
		return "timestamp"
	case rtTimestamptz:
		return "timestamptz"
	case rtDate:
		return "date"
	case rtInterval:
		return "interval"
	case rtFloat32:
		return "f32"
	case rtFloat64:
		return "f64"
	case rtJson:
		return "json"
	case rtJsonb:
		return "jsonb"
	case rtJsonPath:
		return "jsonpath"
	case rtComposite:
		// A named composite is its type name; an anonymous ROW(...) is "record" (PG).
		if t.comp != nil && t.comp.named {
			return t.comp.name
		}
		return "record"
	case rtArray:
		if t.elem != nil {
			return rtName(*t.elem) + "[]"
		}
		return "array"
	case rtRange:
		// A range names itself by its element subtype (i32 → i32range — spec/design/ranges.md).
		if t.elem != nil {
			if s, ok := resolvedRangeElementScalar(t.elem); ok {
				if name, ok2 := rangeNameForElement(s); ok2 {
					return name
				}
			}
			return "range<" + rtName(*t.elem) + ">"
		}
		return "range"
	default:
		return "unknown"
	}
}

// resolvedRangeElementScalar returns the scalar element type of a resolved range element. A range's
// element is always one of the six scalar subtypes; ok is false for anything else (never a valid
// range). Used to name a range and to build its codec.
func resolvedRangeElementScalar(elem *resolvedType) (scalarType, bool) {
	switch elem.kind {
	case rtInt:
		return elem.intTy, true
	case rtDecimal:
		return scalarDecimal, true
	case rtTimestamp:
		return scalarTimestamp, true
	case rtTimestamptz:
		return scalarTimestamptz, true
	case rtDate:
		return scalarDate, true
	default:
		return 0, false
	}
}

// ctxOf returns the type a sibling operand offers an adaptable operand. For an integer literal
// this is the integer width it adopts; for a string literal, bytea/uuid/text (so it can decode
// the hex/uuid input); a bind parameter additionally adopts a decimal/boolean sibling (a literal
// ignores those — its arm keeps i64/text — so widening the mapping is safe). Only a bare NULL
// offers no context (spec/design/api.md §5).
func ctxOf(t resolvedType) *scalarType {
	switch t.kind {
	case rtInt:
		ty := t.intTy
		return &ty
	case rtBytea:
		ty := scalarBytea
		return &ty
	case rtUuid:
		ty := scalarUuid
		return &ty
	case rtText:
		ty := scalarText
		return &ty
	case rtBool:
		ty := scalarBool
		return &ty
	case rtDecimal:
		ty := scalarDecimal
		return &ty
	case rtTimestamp:
		ty := scalarTimestamp
		return &ty
	case rtTimestamptz:
		ty := scalarTimestamptz
		return &ty
	case rtDate:
		ty := scalarDate
		return &ty
	case rtInterval:
		ty := scalarInterval
		return &ty
	case rtFloat32:
		ty := scalarFloat32
		return &ty
	case rtFloat64:
		ty := scalarFloat64
		return &ty
	case rtJson:
		// A json/jsonb sibling offers its type so a string literal parses as that type.
		ty := scalarJson
		return &ty
	case rtJsonb:
		ty := scalarJsonb
		return &ty
	case rtJsonPath:
		ty := scalarJsonPath
		return &ty
	default:
		return nil
	}
}

// rExprKind tags a resolved expression node.
type rExprKind int

const (
	reColumn rExprKind = iota
	// reParam is a bind parameter, by 0-based index into the bound-values slice passed to eval.
	// Its static type was inferred from context at resolve (spec/design/api.md §5); the value is
	// supplied (and coerced) before evaluation.
	reParam
	reConstInt
	reConstBool
	reConstText
	reConstDecimal
	reConstBytea
	reConstUuid
	reConstTimestamp
	reConstTimestamptz
	reConstDate
	reConstInterval
	reConstFloat32
	reConstFloat64
	// reConstJson is a json constant — JSON text stored VERBATIM (spec/design/json.md §4), validated
	// well-formed at resolve. Held in cText (the verbatim text).
	reConstJson
	// reConstJsonb is a jsonb constant — the canonical tagged-node tree (spec/design/json.md §2),
	// parsed + canonicalized at resolve. Held in cJsonb.
	reConstJsonb
	// reConstJsonPath is a jsonpath constant — the canonical normalized source text
	// (spec/design/jsonpath.md, P1a), compiled + rendered at resolve. Held in cText.
	reConstJsonPath
	reConstNull
	reCast
	// reArrayCast is a cast that INVOLVES an array type (spec/design/array.md §7), none expressible
	// by the scalar reCast node (whose `result` is a ScalarType): runtime text → T[] (array_in per
	// row), array → text (array_out per row), and element-wise array → other-element-array (each
	// element through the scalar cast). `castElem` is the target element ColType for the two
	// array-producing casts and nil for array → text; the eval branches on the runtime value.
	reArrayCast
	reNeg
	reNot
	reArith
	reCompare
	reAnd
	reOr
	reIsNull
	// reIsJson is `operand IS [NOT] JSON …` (json-sql-functions.md §5): well-formedness + optional
	// kind / unique-keys test over a string / json / jsonb operand. A NULL operand → NULL; else a
	// definite boolean (NOT-negated when `negated`). `jpKind` selects the kind word; `jpUnique`
	// selects WITH UNIQUE KEYS.
	reIsJson
	// reJsonCtor is `JSON(text [(WITH|WITHOUT) UNIQUE [KEYS]])` (json-sql-functions.md §5): validate a
	// string as a `json` value (verbatim). The operand reuses `operand`; `jpUnique` carries WITH UNIQUE
	// KEYS. A NULL operand → NULL; a malformed string → 22P02; a duplicate key under jpUnique → 22030.
	reJsonCtor
	reDistinct
	reLike
	// reRegex is `lhs ~ rhs` / `~*` / `!~` / `!~*` — a regular-expression match (regex.md). Matched
	// by the hand-written Pike VM (regex.go); negated carries `!~`/`!~*`, insensitive carries
	// `~*`/`!~*` (both sides simple-lowercased like ILIKE). A constant pattern is precompiled once.
	reRegex
	// reCasing is upper(text)/lower(text) — Unicode case folding (collation.md §16). casingUpper
	// selects the direction; folds via the engine-global property table or the ASCII baseline.
	reCasing
	// reAtTimeZone is `value AT TIME ZONE zone` (grammar.md §49, timezones.md §6), desugared from the
	// operator and a bare timezone(zone, value) call. lhs is the zone (text), rhs the value;
	// atTzToTimestamptz selects the direction (false: timestamptz→timestamp; true: timestamp→
	// timestamptz). Reads the engine-global loaded zone set; unknown zone 22023, NULL propagates,
	// ±infinity passes through.
	reAtTimeZone
	// reDateTrunc is date_trunc(unit, value[, zone]) (timezones.md §9.1). sargs is [unit, value] or
	// [unit, value, zone]; for a timestamptz value the truncation is in zone (3-arg) or the session
	// zone (2-arg), charging the timezone unit. The result family is the value family.
	reDateTrunc
	// reExtract is EXTRACT(field FROM value) (timezones.md §9.2). cText is the lowercased field
	// (validated at resolve); operand is the value. For a timestamptz value every field but `epoch` is
	// computed in the session zone (charging timezone).
	reExtract
	// reDateConvert is a cross-family datetime cast (timezones.md §9.3): operand cast to `result`
	// (timestamp/timestamptz/date) from another datetime family. The casts crossing the timestamptz
	// boundary consult the session zone (charging timezone); ±infinity and NULL pass through.
	reDateConvert
	reCase
	// reScalarFunc is a scalar-function call (abs/round, spec/design/functions.md §9),
	// evaluated per row in any context.
	reScalarFunc
	// reArrayFunc is a polymorphic array-function call (spec/design/array-functions.md §3),
	// evaluated per row. Distinct from reScalarFunc: it resolves over anyarray/anyelement (§2) and
	// its builders return an array; NULL handling is per-kernel (the introspectors propagate, the
	// builders are non-strict), so there is no blanket NULL short-circuit at eval.
	reArrayFunc
	// reRangeFunc is a polymorphic range accessor call (spec/design/range-functions.md §1 — lower/
	// upper/isempty/lower_inc/upper_inc/lower_inf/upper_inf), evaluated per row. Like reArrayFunc it
	// resolves over a pseudo-family (anyrange, binding ELEM := the element type) and reuses `sargs`
	// for its single range argument; `rfunc` selects the kernel. All are STRICT (a NULL range → NULL,
	// handled in the kernel). The result type lives in the surrounding resolvedType.
	reRangeFunc
	// reRegexFunc is a regex scalar function call (spec/design/regex.md §8 — regexp_replace → text,
	// regexp_match → text[]). Like reArrayFunc the result type lives in the surrounding resolvedType;
	// `rxFunc` selects the kernel and its arg nodes reuse `sargs`. STRICT (a NULL arg → NULL). A
	// constant pattern is precompiled into rxProgram (regex.md §5), charged once via rxCompileCharged.
	reRegexFunc
	// reRangeCtor is a range CONSTRUCTOR call (spec/design/range-functions.md §2 — i32range(lo, hi[,
	// bounds]) and the five siblings), evaluated per row. `relem` is the range's element scalar (the
	// result range type is recovered from it, a bijection); its 2 bound nodes plus an optional
	// bounds-flags TEXT node reuse `sargs`. Non-strict (null = "none"): a NULL bound is an infinite
	// bound, handled in the kernel — there is no blanket NULL short-circuit. The kernel coerces each
	// bound to relem (assignment-style), reads the bounds flags, and finalizes (canonicalize /
	// order-check / empty-normalize).
	reRangeCtor
	// reRangeOp is a range BOOLEAN operator (spec/design/range-functions.md §3 — @> <@ && << >> &< &>
	// -|-), evaluated per row. Its two operand nodes reuse `sargs`; `rop` selects the kernel. STRICT —
	// a NULL operand → NULL (handled in the eval arm). `relem` carries the range's element scalar, used
	// only by the roContainsElem/roElemContainedBy element overloads to coerce the bare-element operand
	// to the range's element type at eval; unused (but carried) for the range-against-range operators.
	reRangeOp
	// reRangeSetOp is a range SET operator (spec/design/range-functions.md §4 — `+` union, `-`
	// difference, `*` intersection, and range_merge), evaluated per row. Its two range operand nodes
	// reuse `sargs`; `rsop` selects the kernel. STRICT — a NULL operand → NULL (handled in the eval
	// arm). Unlike reRangeOp it carries no element scalar — the kernels work off the self-describing
	// operand values, and the result range type is fixed at resolve. The kernels (rangeUnion/
	// rangeIntersect/rangeMinus) live in range.go; `+`/`-` raise 22000 on a non-contiguous result.
	reRangeSetOp
	// reVariadic is a VARIADIC argument-counting call (spec/design/array-functions.md §12 —
	// num_nulls/num_nonnulls). Non-strict (null = "none"): no blanket NULL short-circuit. Its
	// argument nodes reuse `sargs`; `variadicArray` records the call shape (false = the spread form,
	// counting sargs' null-ness directly; true = the VARIADIC-array form, one sargs operand whose
	// flattened elements are counted, a NULL whole-array → NULL). Result is always i32.
	reVariadic
	// reJsonBuild is a VARIADIC json/jsonb builder (json-sql-functions.md §2 — json[b]_build_array /
	// _object). Non-strict: a NULL argument is included as JSON null (array) or a value (object).
	// `jbKind` selects array vs object; `jbJson` selects the json (compact / PG builder-spacing) vs
	// jsonb (canonical) render; `variadicArray` records the VARIADIC-array call shape (the lone array
	// operand is spread; a NULL whole-array → NULL). Argument nodes reuse `sargs`. The result type
	// (json/jsonb) is fixed at resolve from the catalog.
	reJsonBuild
	// reJsonSetInsert is `jsonb_set` / `jsonb_insert` (json-sql-functions.md §2): a jsonb path
	// mutation. `sargs` is `[target jsonb, path text[], value jsonb, flag boolean]` — STRICT (any
	// NULL → SQL NULL, including a NULL path element). `psMode` selects replace-or-create (Set) vs
	// insert (Insert); the boolean flag is create_if_missing (Set) / insert_after (Insert).
	reJsonSetInsert
	// reJsonObject is `json_object` / `jsonb_object` (json-sql-functions.md §2): build an object from
	// text array(s). `sargs` is one `text[]` of alternating keys/values, or two `text[]` (keys,
	// values). Every VALUE becomes a JSON string (a NULL value → JSON null); a NULL key → 22004; an
	// odd one-array / mismatched two-array length → 2202E. STRICT in the whole array argument(s) (a
	// NULL array → SQL NULL). `jbJson` true ⇒ the json result (insertion order + dups + " : "
	// spacing); false ⇒ the jsonb result (canonical: last-wins dedup + sorted keys).
	reJsonObject
	// reJsonPathFn is a scalar jsonpath query function (P2, jsonpath.md §5): jsonb_path_exists /
	// jsonb_path_query_first / jsonb_path_query_array. `sargs` = [ctx jsonb, path jsonpath]; STRICT
	// (any NULL → SQL NULL). `jpFnKind` selects which function. The path is recompiled from its
	// canonical text at eval.
	reJsonPathFn
	// reJsonSqlFn is a SQL/JSON query function JSON_EXISTS / JSON_VALUE / JSON_QUERY (json-sql-functions.md
	// §5, S2). `sargs` = [ctx, path]: ctx produces the context jsonb (or json/text, coerced), path the
	// jsonpath; a NULL ctx/path → SQL NULL. `jsKind` selects which function; `result`/`typmod` the
	// RETURNING type; `jsWrapper`/`jsKeepQuotes`/`jsOnEmpty`/`jsOnError` drive the result. A SQL/JSON
	// (class-22) error honors ON ERROR; anything else propagates.
	reJsonSqlFn
	// reOuterColumn is a correlated column reference (spec/design/grammar.md §26): the column
	// `index` of the enclosing row `level` hops out (1 = immediate parent). A leaf.
	reOuterColumn
	// reSubquery is a CORRELATED subquery, re-executed per outer row at eval (uncorrelated ones
	// are folded to a constant / reInValues before exec).
	reSubquery
	// reInValues is a folded uncorrelated `IN (subquery)`: the subquery ran once yielding `list`;
	// per row it tests `lhs` for three-valued membership.
	reInValues
	// reQuantified is a quantified array comparison `lhs op ANY/ALL(array)`
	// (spec/design/array-functions.md §11) — the array spelling of IN. `lhs` is the scalar, `rhs`
	// the array node, `op` the comparison, `quantAll` true for ALL. At eval the three-valued fold
	// over the array's flattened elements reuses the IN-list membership semantics, charging per
	// element like reInValues.
	reQuantified
	// reRow is a ROW(...) composite constructor (spec/design/composite.md §1): its field nodes are
	// held in sargs (so the existing fold / references-outer / touched-set walks recurse into them
	// for free). Evaluates to a ValComposite; one operator_eval per node (cost.md §9).
	reRow
	// reArray is an ARRAY[...] constructor (spec/design/array.md §1): its element nodes are held in
	// sargs (so the fold / references-outer / touched-set walks recurse for free). Evaluates to a
	// ValArray; one operator_eval per node. `nested` stacks sub-arrays into a higher dimension (§4).
	reArray
	// reConstArray is a folded array constant (the value_to_rexpr equivalent), preserving its shape;
	// it evaluates to its ValArray directly (cArray).
	reConstArray
	// reConstRange is a folded range constant ('[1,5)'::i32range, already canonicalized at resolve);
	// it evaluates to its ValRange directly (cRange).
	reConstRange
	// reField is field selection `(composite).field` (spec/design/composite.md §S4): evaluate
	// `operand` (the base) to a composite value and return its `index`-th field (the field ordinal,
	// fixed at resolve). A whole-value-NULL composite yields NULL for any field. One operator_eval
	// per node (cost.md §9).
	reField
	// reSubscript is array element subscript `operand[sub]` (spec/design/array.md §6): evaluate
	// `operand` (the base array) and `sub` (the index) and return the 1-based element. A NULL array,
	// a NULL index, or an out-of-bounds index yields NULL (PG — never an error). One operator_eval
	// per node.
	reSubscript
	// reJsonGet is a jsonb accessor operator (`-> ->> #> #>>`, spec/design/json-sql-functions.md §1, J4).
	// `jgop` selects field/index vs path and text-vs-jsonb; `lhs` evaluates to a jsonb document; `rhs` is
	// the key (text), array index (integer), or path (`text[]`). The result is jsonb (`-> #>`) or text
	// (`->> #>>`), and is SQL NULL when the access misses (or when base/arg is NULL — the operators are
	// strict).
	reJsonGet
	// reJsonContains is `a @> b` jsonb deep containment (spec/design/json-sql-functions.md §1, J5):
	// does `a` contain `b`. `<@` resolves to this with the operands swapped (`lhs`=a, `rhs`=b).
	// Boolean; strict (a NULL operand → NULL).
	reJsonContains
	// reJsonHasKey is `jsonb ? text` / `?| text[]` / `?& text[]` key-existence
	// (spec/design/json-sql-functions.md §1, J5). `hasKey` selects one-key / any-key / all-keys;
	// `lhs` is the jsonb base, `rhs` the text key or text[] of keys. Boolean; strict.
	reJsonHasKey
	// reJsonConcat is `a || b` jsonb concatenate / shallow-merge (spec/design/json-sql-functions.md
	// §1, J6). `lhs`/`rhs` are the two jsonb operands. Result jsonb; strict (a NULL operand → NULL).
	reJsonConcat
	// reJsonDelete is `jsonb - text|int|text[]` (delete key/index/keys) and `jsonb #- text[]` (delete
	// at path) — the J6 mutation deletes (spec/design/json-sql-functions.md §1). `delKind` selects the
	// form; `lhs` is the jsonb document, `rhs` the key/index/key-array/path. Result jsonb; strict; a
	// delete from a scalar (or an integer index into an object) is `22023`.
	reJsonDelete
)

// jsonGetOp selects which jsonb accessor operator an reJsonGet node applies
// (spec/design/json-sql-functions.md §1).
type jsonGetOp int

const (
	jgArrow         jsonGetOp = iota // `->` — field by key (text arg) or element by index (integer arg); result jsonb.
	jgArrowText                      // `->>` — same access, rendered as text.
	jgHashArrow                      // `#>` — get at a `text[]` path; result jsonb.
	jgHashArrowText                  // `#>>` — get at a `text[]` path, rendered as text.
)

// hasKeyKind selects which jsonb key-existence operator an reJsonHasKey node applies
// (spec/design/json-sql-functions.md §1, J5).
type hasKeyKind int

const (
	hkOne hasKeyKind = iota // `?` — a single key (text) exists.
	hkAny                   // `?|` — any key of a `text[]` exists.
	hkAll                   // `?&` — all keys of a `text[]` exist.
)

// deleteKind selects which jsonb delete form an reJsonDelete node applies
// (spec/design/json-sql-functions.md §1, J6).
type deleteKind int

const (
	dkKey   deleteKind = iota // `jsonb - text` — delete a key (object) or matching string elements (array).
	dkIndex                   // `jsonb - int` — delete the array element at an index.
	dkKeys                    // `jsonb - text[]` — delete each key.
	dkPath                    // `jsonb #- text[]` — delete the element at a path.
)

// subqueryKind selects which subquery form an reSubquery node is (spec/design/grammar.md §26).
type subqueryKind int

const (
	sqScalar subqueryKind = iota
	sqExists
	sqIn
	// sqQuantified is `lhs op ANY/ALL(SELECT …)` (array-functions.md §11.6): the node carries the
	// comparison `op` and `quantAll`, and the body's single column folds through quantifiedMembership
	// exactly like the array form. Survives as an reSubquery node only when CORRELATED; an
	// uncorrelated one is folded to a constant-array reQuantified.
	sqQuantified
)

// scalarFunc selects a scalar function (kind = "function"). The overload (integer vs decimal)
// is recovered at eval from the argument's runtime value.
type scalarFunc int

const (
	sfAbs scalarFunc = iota
	sfRound
	// Float scalar functions (spec/design/float.md §8). The exact / correctly-rounded set
	// (in-contract): sfFloatAbs, sfCeil, sfFloor, sfTrunc, sfFloatRound (1- and 2-arg), sfSqrt.
	// The transcendental set (exempted, native libm): sfExp, sfLn, sfLog10, sfPow, sfSin, sfCos,
	// sfTan. The width of the call is recorded in `result` (Float32/Float64).
	sfFloatAbs
	sfCeil
	sfFloor
	sfTrunc
	sfFloatRound
	sfSqrt
	sfExp
	sfLn
	sfLog10
	sfPow
	// sfLog — base-10 (1-arg) / arbitrary-base (2-arg) logarithm over decimal (decimal.md §8).
	// Decimal-only (no float `log`); the EXACT-numeric kernel, IN-CONTRACT.
	sfLog
	sfSin
	sfCos
	sfTan
	// sfCbrt is the real cube root (float.md §8) — transcendental, exempted; no domain
	// restriction (cbrt of a negative is the negative real root).
	sfCbrt
	// sfPi is the constant π as f64 (float.md §8) — zero-arg, IN-CONTRACT (same f64 literal in
	// every core), not in the transcendental ledger.
	sfPi
	// sfRadians is degrees → radians (float.md §8): x · RADIANS_PER_DEGREE. A single
	// correctly-rounded IEEE multiply, IN-CONTRACT (not ledgered).
	sfRadians
	// sfDegrees is radians → degrees (float.md §8): x / RADIANS_PER_DEGREE. A single
	// correctly-rounded IEEE divide, IN-CONTRACT (not ledgered).
	sfDegrees
	// sfAsin is the inverse sine in radians (float.md §8) — transcendental, exempted; domain
	// [-1, 1], |x| > 1 (or ±Inf) → 22003, NaN propagates.
	sfAsin
	// sfAcos is the inverse cosine in radians (float.md §8) — transcendental, exempted; same
	// domain [-1, 1] as asin.
	sfAcos
	// sfAtan is the inverse tangent in radians (float.md §8) — transcendental, exempted; no
	// domain restriction (atan(±Inf) = ±π/2).
	sfAtan
	// sfAtan2 is the quadrant-aware inverse tangent of y/x (float.md §8) — transcendental,
	// exempted; two float operands (widened to f64), no domain trap.
	sfAtan2
	// sfCot is the cotangent, 1/tan(x) (float.md §8) — transcendental, exempted; cot(0) =
	// +Infinity (no trap).
	sfCot
	// Hyperbolic functions (float.md §8) — transcendental, exempted. sinh/cosh/tanh/asinh have no
	// domain trap (sinh/cosh overflow to ±Inf, PG-faithful); acosh traps below 1, atanh outside
	// [-1, 1] (atanh(±1) = ±Inf is admissible).
	sfSinh
	sfCosh
	sfTanh
	sfAsinh
	sfAcosh
	sfAtanh
	// sfSign is sign(x) → -1 / 0 / +1 (float.md §8). Decimal → numeric (scale 0), float → f64
	// (EXACT/in-contract; sign(NaN) = sign(±0) = 0, sign(±Inf) = ±1). Dispatches on the operand.
	sfSign
	// sfDiv is div(a, b) → numeric — the TRUNCATED (toward zero) integer quotient at scale 0
	// (PG div(numeric, numeric)). Computed exactly as (a − a%b)/b. Resolver-routed (the catalog
	// name "div" belongs to the `/` operator); integers promote; 22012 on a zero divisor.
	sfDiv
	// sfGcd is gcd(a, b) → the greatest common divisor (non-negative), EXACT/in-contract. Integer
	// operands → the promoted integer type (Euclid; an overflowing-magnitude result → 22003); a
	// decimal operand → numeric at scale max(sₐ, s_b). gcd(0, 0) = 0. Resolver-routed.
	sfGcd
	// sfLcm is lcm(a, b) → the least common multiple (non-negative), EXACT/in-contract, |a/gcd·b|.
	// lcm(_, 0) = 0. Integer → the promoted type (overflow → 22003); decimal → numeric. Resolver-routed.
	sfLcm
	// sfFactorial is factorial(n) → numeric — n! at scale 0 (PG factorial(bigint)). A negative
	// operand → 22003. The O(n) multiply loop is metered per step (decimal_work, guarded) so the
	// cost ceiling bounds a large factorial before its limb work runs (§13).
	sfFactorial
	// sfWidthBucket is width_bucket(op, low, high, count) → i32 — the equi-width histogram bucket.
	// Two overloads (numeric exact, float in f64); dispatches on the operand. 2201G on a bad count /
	// equal bounds (and, for float, a NaN operand / infinite bound); a result past int4 → 22003.
	sfWidthBucket
	// sfScale is scale(numeric) → i32 — the decimal's display (fractional-digit) scale (decimal.md).
	sfScale
	// sfMinScale is min_scale(numeric) → i32 — the smallest scale that represents the value exactly
	// (trailing fractional zeros dropped); zero has min_scale 0 (decimal.md).
	sfMinScale
	// sfTrimScale is trim_scale(numeric) → numeric — the value re-scaled down to its min_scale
	// (trailing zeros removed), value-identical (decimal.md).
	sfTrimScale
	// sfMakeInterval builds an interval from its (named/defaulted) integer components plus the
	// f64 secs (spec/design/functions.md §11). The one scalar function returning interval.
	sfMakeInterval
	// sfMakeTimestamp builds a zoneless timestamp from the named (un-defaulted) date/time fields
	// plus the f64 sec — the make_interval sibling (spec/design/functions.md §11).
	sfMakeTimestamp
	// sfMakeTimestamptz builds a timestamptz: as sfMakeTimestamp, then interprets the wall clock in
	// the session zone (6-arg) or the explicit timezone text (7-arg), charging one timezone unit (§11).
	sfMakeTimestamptz
	// uuid extractors (spec/design/functions.md §12): pure inspectors of a uuid's bits.
	// sfUuidExtractVersion → i16 (NULL off-RFC-variant); sfUuidExtractTimestamp → timestamptz
	// (the embedded instant for v1/v7, else NULL).
	sfUuidExtractVersion
	sfUuidExtractTimestamp
	// uuid generators (spec/design/entropy.md §3): volatile. sfUuidv4 → random; sfUuidv7 → ms
	// timestamp + monotonic counter + random, with an optional interval shift.
	sfUuidv4
	sfUuidv7
	// current-time functions (spec/design/entropy.md §5): sfNow → timestamptz, the statement clock
	// read ONCE and reused (STABLE; current_timestamp is parser sugar for it); sfClockTimestamp →
	// timestamptz, the clock seam read on EVERY call (VOLATILE).
	sfNow
	sfClockTimestamp
	// sequence value functions (spec/design/sequences.md §4/§6): sfNextval(text) → i64 advances
	// the named sequence and MUTATES the per-statement pending state (write path); sfCurrval(text)
	// → i64 is a pure session-state read. sfSetval(text, i64[, bool]) → i64 sets the counter
	// (also a write); sfLastval() → i64 reads the most-recent-nextval session value (pure read).
	sfNextval
	sfCurrval
	sfSetval
	sfLastval
	// session-variable read (spec/design/session.md §6.1): sfCurrentSetting(text[, bool]) → text reads
	// the named session variable from the session's variable map. STABLE; 42704 on an unset name unless
	// the two-arg missing_ok is true (→ NULL).
	sfCurrentSetting
	// json/jsonb processing functions (B1, spec/design/json-sql-functions.md §2). The json* and
	// jsonb* variants share a kernel; the only difference is the json overload parses the verbatim
	// text first. All propagate a SQL NULL input.
	// json[b]_typeof → the JSON type name (object/array/string/number/boolean/null).
	sfJsonbTypeof
	sfJsonTypeof
	// json[b]_array_length → the array element count; a non-array is 22023.
	sfJsonbArrayLength
	sfJsonArrayLength
	// json[b]_strip_nulls → recursively remove object members whose value is JSON null.
	sfJsonbStripNulls
	sfJsonStripNulls
	// jsonb_pretty → an indented multi-line render.
	sfJsonbPretty
	// to_jsonb(anyelement) → the JSON image of any value (the valueToNode kernel). STRICT.
	sfToJsonb
	// to_json(anyelement) → the JSON image as `json` (the valueToNode kernel rendered per elemJsonText:
	// a jsonb input canonical-spaced, a json input verbatim, everything else compact). STRICT.
	sfToJson
	// json_scalar(anyelement) → the value's JSON scalar as `json` (number/boolean/string). STRICT.
	// Other source types (date/timestamp/uuid/bytea/interval/float) are a deferred 0A000.
	sfJsonScalar
	// json_serialize(json|jsonb) → the value's text serialization (json verbatim, jsonb canonical).
	sfJsonSerialize
	// --- string / text functions (spec/design/string-functions.md). All STRICT (NULL propagates via
	// the generic scalarFunc short-circuit). Character functions count Unicode code points (Go strings
	// are UTF-8, so `range`/utf8.RuneCountInString); octet/bit functions count UTF-8 bytes.
	// length(text) → i32 — the number of characters (code points). length('héllo') = 5.
	sfLength
	// octet_length(text) → i32 — the number of UTF-8 bytes. octet_length('héllo') = 6.
	sfOctetLength
	// bit_length(text) → i32 — the number of UTF-8 bits = octet_length × 8. bit_length('héllo') = 48.
	sfBitLength
	// substr(text, start[, count]) → text — the function form of SUBSTRING (1-based, code-point
	// indexed). A negative count is 22011 (string-functions.md §3).
	sfSubstr
	// left(text, n) → text — the first n characters; a negative n drops the last |n| (§3).
	sfLeft
	// right(text, n) → text — the last n characters; a negative n drops the first |n| (§3).
	sfRight
	// lpad(text, length[, fill]) → text — left-pad to `length` chars with `fill` (default space);
	// a longer string truncates; an over-large length traps 54000 (§3).
	sfLpad
	// rpad(text, length[, fill]) → text — the right-hand mirror of lpad (§3).
	sfRpad
	// btrim(text[, chars]) → text — trim characters in the `chars` set from both ends (§3).
	sfBtrim
	// ltrim(text[, chars]) → text — trim the `chars` set from the LEADING end only (§3).
	sfLtrim
	// rtrim(text[, chars]) → text — trim the `chars` set from the TRAILING end only (§3).
	sfRtrim
	// replace(text, from, to) → text — replace every occurrence of substring `from` with `to` (§3).
	sfReplace
	// translate(text, from, to) → text — per-character map/delete by position in `from`/`to` (§3).
	sfTranslate
	// repeat(text, n) → text — the string concatenated n times; over-large result traps 54000 (§3).
	sfRepeat
	// reverse(text) → text — the code points in reverse order (§3).
	sfReverse
	// strpos(text, substring) → i32 — 1-based code-point position of the first match, else 0 (§3).
	sfStrpos
	// split_part(text, delimiter, n) → text — the n-th field of the split; n=0 traps 22023 (§3).
	sfSplitPart
	// starts_with(text, prefix) → boolean — true iff the string begins with `prefix` (§3).
	sfStartsWith
	// ascii(text) → i32 — the Unicode code point of the first character; empty → 0 (§3).
	sfAscii
	// chr(int) → text — the one-character string for a Unicode code point; bad point traps (§3).
	sfChr
	// initcap(text) → text — titlecase each word (ASCII word boundaries + ASCII case fold, §3).
	sfInitcap
	// to_hex(int) → text — lowercase hex of the value's 64-bit two's-complement pattern (§3).
	sfToHex
	// encode(bytea, format) → text — render bytes as hex / base64 / escape (§3).
	sfEncode
	// decode(text, format) → bytea — parse hex / base64 / escape back to binary (§3).
	sfDecode
	// quote_literal(text) → text — wrap as a SQL string literal (§3).
	sfQuoteLiteral
	// quote_ident(text) → text — wrap as a SQL identifier (§3).
	sfQuoteIdent
	// quote_nullable(text) → text — like quote_literal but NON-STRICT (NULL → 'NULL', §3).
	sfQuoteNullable
)

// arrayFunc selects a polymorphic array function (spec/design/array-functions.md §3). Each name is
// single-arity, so the name alone picks the kernel; the eval recovers everything else from the
// operand values (the array's own shape header).
type arrayFunc int

const (
	afNdims       arrayFunc = iota // array_ndims(anyarray) → i32; NULL for the empty array
	afLength                       // array_length(anyarray, integer) → i32; NULL if empty / out of range
	afLower                        // array_lower(anyarray, integer) → i32
	afUpper                        // array_upper(anyarray, integer) → i32
	afCardinality                  // cardinality(anyarray) → i32; 0 for the empty array
	afDims                         // array_dims(anyarray) → text; NULL for the empty array
	afAppend                       // array_append(anyarray, anyelement) → anyarray; non-strict; 22000 if multidim
	afPrepend                      // array_prepend(anyelement, anyarray) → anyarray
	afCat                          // array_cat(anyarray, anyarray) → anyarray; 2202E on incompatible dims
	afRemove                       // array_remove(anyarray, anyelement) → anyarray; 1-D/empty only (0A000); lb preserved
	afReplace                      // array_replace(anyarray, anyelement, anyelement) → anyarray; any dim, shape preserved
	afPosition                     // array_position(anyarray, anyelement[, integer]) → i32; 1-D/empty (0A000); NULL start 22004
	afPositions                    // array_positions(anyarray, anyelement) → i32[]; 1-D/empty only (0A000)
	afToJson                       // array_to_json(anyarray) → json; compact to_jsonb kernel; STRICT; multidim 0A000
	afContains                     // a @> b (anyarray, anyarray) → boolean; does a contain b; strict eq; any dim (§10)
	afContainedBy                  // a <@ b (anyarray, anyarray) → boolean; is a contained by b (b @> a) (§10)
	afOverlaps                     // a && b (anyarray, anyarray) → boolean; do a and b share an element; strict eq (§10)
)

// rangeFunc selects a polymorphic range accessor (spec/design/range-functions.md §1, RF1). Like
// arrayFunc each is single-arity, so the name alone picks the kernel; the eval recovers everything
// else from the operand range value (self-describing). All are STRICT (a NULL range → NULL).
type rangeFunc int

const (
	rfLower    rangeFunc = iota // lower(anyrange) → anyelement; NULL if empty / unbounded below
	rfUpper                     // upper(anyrange) → anyelement; NULL if empty / unbounded above
	rfIsEmpty                   // isempty(anyrange) → boolean
	rfLowerInc                  // lower_inc(anyrange) → boolean (false for empty / an infinite lower bound)
	rfUpperInc                  // upper_inc(anyrange) → boolean (false for empty / an infinite upper bound)
	rfLowerInf                  // lower_inf(anyrange) → boolean (false for the empty range)
	rfUpperInf                  // upper_inf(anyrange) → boolean (false for the empty range)
)

// regexFunc selects a regex scalar function kernel (spec/design/regex.md §8). Kernels in regex.go.
type regexFunc int

const (
	rxReplace regexFunc = iota // regexp_replace(source, pattern, replacement [, flags]) → text
	rxMatch                    // regexp_match(source, pattern [, flags]) → text[]
	rxLike                     // regexp_like(string, pattern [, flags]) → boolean (regex.md §8b)
	rxCount                    // regexp_count(string, pattern [, start [, flags]]) → integer
	rxSubstr                   // regexp_substr(string, pattern [, start [, N [, flags [, subexpr]]]]) → text
	rxInstr                    // regexp_instr(string, pattern [, start [, N [, endoption [, flags [, subexpr]]]]]) → integer
)

// rangeOp selects a range BOOLEAN operator (spec/design/range-functions.md §3, RF3). Each is a binary
// infix operator returning a definite boolean (a NULL operand short-circuits to NULL at eval, like the
// array containment operators). roContainsElem/roElemContainedBy are the element overloads of @>/<@
// (the other operand is a bare element coerced to the range's element type); the rest are
// range-against-range. The kernels live in range.go.
type rangeOp int

const (
	roContains        rangeOp = iota // a @> b — range a contains range b
	roContainsElem                   // r @> e — range r contains element e (the element overload of @>)
	roContainedBy                    // a <@ b — range a is contained by range b
	roElemContainedBy                // e <@ r — element e is contained by range r (the element overload of <@)
	roOverlaps                       // a && b — ranges a and b overlap
	roBefore                         // a << b — a is strictly left of b
	roAfter                          // a >> b — a is strictly right of b
	roOverleft                       // a &< b — a does not extend to the right of b
	roOverright                      // a &> b — a does not extend to the left of b
	roAdjacent                       // a -|- b — a and b are adjacent
)

// rangeSetOp selects a range SET operator (spec/design/range-functions.md §4, RF4). Each combines two
// ranges over a common element type into a new range. rsoUnion/rsoDifference raise 22000 on a
// non-contiguous result; rsoIntersect/rsoMerge never error. The kernels live in range.go.
type rangeSetOp int

const (
	rsoUnion      rangeSetOp = iota // a + b — union: the smallest single range covering both (22000 on a gap)
	rsoIntersect                    // a * b — intersection: the overlap (empty when the ranges are disjoint)
	rsoDifference                   // a - b — difference: the part of `a` not in `b` (22000 if `b` splits `a`)
	rsoMerge                        // range_merge(a, b) — like union but spans any gap silently (never errors)
)

// variadicFunc selects a VARIADIC argument-counting function (spec/design/array-functions.md §12).
// Both return i32; the call form (spread vs VARIADIC-array) lives on the rExpr node.
type variadicFunc int

const (
	vfNumNulls    variadicFunc = iota // num_nulls(VARIADIC "any") → i32 — count of NULL args/elements
	vfNumNonnulls                     // num_nonnulls(VARIADIC "any") → i32 — count of non-NULL args/elements
)

// jsonBuildKind selects which VARIADIC json/jsonb builder an reJsonBuild node is
// (json-sql-functions.md §2). The `json` flag (on the node) selects the json vs jsonb render.
type jsonBuildKind int

const (
	jbArray  jsonBuildKind = iota // json[b]_build_array — every argument is one array element (NULL → JSON null)
	jbObject                      // json[b]_build_object — alternating key/value args (odd count / NULL key → 22023)
)

// jsonSqlKind selects which SQL/JSON query function an reJsonSqlFn node is (json-sql-functions.md §5).
type jsonSqlKind int

const (
	// jsExists is JSON_EXISTS → boolean (non-empty sequence); errors honor ON ERROR (default FALSE).
	jsExists jsonSqlKind = iota
	// jsValue is JSON_VALUE → a single scalar coerced to the RETURNING type (default text).
	jsValue
	// jsQuery is JSON_QUERY → a json/jsonb value (wrapper / quotes controlled).
	jsQuery
)

// jsonPathFnKind selects which scalar jsonpath query function an reJsonPathFn node is (jsonpath.md §5).
type jsonPathFnKind int

const (
	// jpfExists is jsonb_path_exists → boolean (the sequence is non-empty).
	jpfExists jsonPathFnKind = iota
	// jpfQueryFirst is jsonb_path_query_first → the first sequence item, or NULL if empty.
	jpfQueryFirst
	// jpfQueryArray is jsonb_path_query_array → the sequence wrapped in a JSON array.
	jpfQueryArray
	// jpfMatch is jsonb_path_match (and the `@@` operator) → the single boolean the path/predicate
	// produces (22038 if not exactly one boolean item).
	jpfMatch
)

// rExpr is a resolved expression over fixed column indices, ready to evaluate against a
// row. Arithmetic/neg nodes carry their (promotion-tower) result type in `result` so the
// computed value can be range-checked against it.
type rExpr struct {
	kind     rExprKind
	index    int            // reColumn
	cInt     int64          // reConstInt
	cBool    bool           // reConstBool
	cText    string         // reConstText
	cDec     Decimal        // reConstDecimal
	cBytea   []byte         // reConstBytea
	cIv      Interval       // reConstInterval
	cFloat   float64        // reConstFloat32 / reConstFloat64 (a f32 const is held as the f64 of its value)
	op       binaryOp       // reArith, reCompare
	result   scalarType     // reCast target; reNeg / reArith result type
	typmod   *decimalTypmod // reCast: a decimal target's numeric(p,s) typmod
	varchar  *uint32        // reCast: a varchar(n) text target's max length — truncate (types.md §15)
	castElem *colType       // reArrayCast: the target element ColType (nil ⇒ array → text)
	lhs      *rExpr         // reArith, reCompare, reAnd, reOr, reDistinct
	rhs      *rExpr         // reArith, reCompare, reAnd, reOr, reDistinct
	operand  *rExpr         // reCast, reNeg, reNot, reIsNull, reCasing
	negated  bool           // reIsNull, reDistinct
	// insensitive carries ILIKE (reLike) / ~* (reRegex); casingUpper selects upper vs lower
	// (reCasing) — both collation.md §16.
	insensitive bool
	casingUpper bool
	// atTzToTimestamptz selects the AT TIME ZONE direction (reAtTimeZone): false is timestamptz→
	// timestamp, true is timestamp→timestamptz (timezones.md §6).
	atTzToTimestamptz bool
	// reRegex / reRegexFunc: rxProgram is the precompiled NFA for a CONSTANT pattern (compiled once at
	// resolve, the `col ~ 'literal'` case — regex.md §5); nil means the pattern is non-constant
	// (compiled per row at eval). rxCompileCharged is the one-shot flag charging a precompiled
	// program's regex_compile cost once per statement execution (on first eval), not per row. rxFunc
	// selects the reRegexFunc kernel (regexp_replace / regexp_match); its arg nodes reuse `sargs`.
	rxProgram        *regexProgram
	rxCompileCharged bool
	rxFunc           regexFunc
	// collation is the derived collation of a reCompare (spec/design/collation.md §7). nil is the
	// C / default byte order (the unchanged fast path); non-nil is a loaded collation that orders the
	// ORDERING comparisons (< <= > >=) by its UCA sort key. =/<> stay byte-equality regardless
	// (deterministic-collation equality IS byte-identity), but it is derived + conflict-checked
	// (42P21) for every comparison op.
	collation *Collation

	// reCase: (condition, result) arms, the ELSE result (constNull for an implicit ELSE), and
	// whether the unified result type is decimal (so integer results widen to decimal at eval).
	caseArms    []rCaseArm
	caseEls     *rExpr
	caseDecimal bool

	// reScalarFunc: the scalar function (abs/round) and its argument nodes. `result` holds the
	// static result type — for abs over an integer it is the operand's integer type, so the
	// magnitude is range-checked at that boundary; otherwise decimal.
	sfunc scalarFunc
	sargs []*rExpr
	// reArrayFunc: the polymorphic array function; its argument nodes reuse `sargs`. The result
	// type lives in the surrounding resolvedType (carried out of resolve), not on the node — the
	// kernel produces the result value from the operands (an array value is self-describing).
	afunc arrayFunc
	// reRangeFunc: the polymorphic range accessor; its single range argument reuses `sargs`. Like
	// reArrayFunc the result type lives in the surrounding resolvedType (carried out of resolve), not
	// on the node — the kernel produces the result value from the operand (a range is self-describing).
	rfunc rangeFunc
	// reRangeCtor: the element scalar of the range being built (i32range → i32). The result range type
	// is recovered from it (a bijection); the bound/flags argument nodes reuse `sargs`. reRangeOp also
	// uses `relem` — the range's element scalar, for the element-overload coercion at eval.
	relem scalarType
	// reRangeOp: the range boolean operator kernel. Its two operand nodes reuse `sargs`.
	rop rangeOp
	// reRangeSetOp: the range set operator kernel (+ union, - difference, * intersection, range_merge).
	// Its two range operand nodes reuse `sargs`; no element scalar is carried (the kernels work off the
	// self-describing operand values).
	rsop rangeSetOp
	// reVariadic: the VARIADIC counting function and its call shape. Argument nodes reuse `sargs`;
	// `variadicArray` true ⇒ the VARIADIC-array form (one array operand), false ⇒ the spread form.
	vfunc         variadicFunc
	variadicArray bool
	// reJsonBuild: which json/jsonb builder + render. `jbKind` selects array vs object; `jbJson` true ⇒
	// the `json` (compact / builder-spacing) render, false ⇒ the `jsonb` (canonical) render. Argument
	// nodes reuse `sargs`; `variadicArray` (above) records the VARIADIC-array call shape.
	jbKind jsonBuildKind
	jbJson bool

	// reJsonPathFn: which scalar jsonpath query function (jsonb_path_exists / _query_first /
	// _query_array). Argument nodes are in `sargs` = [ctx jsonb, path jsonpath] (jsonpath.md §5).
	jpFnKind jsonPathFnKind

	// reJsonSqlFn: a SQL/JSON query function JSON_EXISTS / JSON_VALUE / JSON_QUERY (json-sql-functions.md
	// §5, S2). `sargs` = [ctx, path]; `result`/`typmod` hold the RETURNING scalar type + decimal typmod.
	// jsKind selects the function; jsWrapper/jsKeepQuotes/jsOnEmpty/jsOnError drive the result.
	jsKind       jsonSqlKind
	jsWrapper    jsonWrapper
	jsKeepQuotes bool
	jsOnEmpty    jsonOnBehavior
	jsOnError    jsonOnBehavior

	// reJsonSetInsert: which path mutation (jsonb_set vs jsonb_insert). Argument nodes are in
	// `sargs` = [target, path, value, flag] (json-sql-functions.md §2).
	psMode pathSetMode

	// reArray: `nested` marks a multidim-stacking constructor (its element nodes evaluate to
	// arrays, stacked into one higher dimension — spec/design/array.md §4).
	nested bool
	// reSubscript: the subscript specs applied to `operand`, and whether any is a slice (so the
	// whole access is a slice — spec/design/array.md §6).
	subs    []rSubscript
	isSlice bool
	// reConstArray: a folded array constant (its full shape preserved).
	cArray *ArrayVal
	// reConstRange: a folded range constant (already canonicalized).
	cRange *RangeVal
	// reConstJsonb: a folded jsonb constant — the canonical node tree (parsed + canonicalized at
	// resolve). A reConstJson holds its verbatim text in cText (no extra field).
	cJsonb *JsonNode

	// reIsJson: the optional kind word (json-sql-functions.md §5) and the WITH UNIQUE KEYS flag. The
	// operand reuses `operand`; `negated` carries IS NOT JSON.
	jpKind   jsonPredicateKind
	jpUnique bool

	// reJsonGet: the jsonb accessor operator (`-> ->> #> #>>`). `lhs` is the jsonb base, `rhs` the
	// key/index/path argument (spec/design/json-sql-functions.md §1).
	jgop jsonGetOp

	// reJsonHasKey: which key-existence operator (`?`/`?|`/`?&`) — one-key / any-key / all-keys.
	// `lhs` is the jsonb base, `rhs` the text key (`?`) or text[] of keys (`?|`/`?&`).
	hasKey hasKeyKind

	// reJsonDelete: which delete form (`-` key/index/keys, `#-` path). `lhs` is the jsonb base,
	// `rhs` the key (text) / index (int) / keys-or-path (text[]) argument.
	delKind deleteKind

	// reQuantified: `lhs` is the scalar, `rhs` the array node, `op` the comparison, `quantAll`
	// selects ALL (true) vs ANY/SOME (false) (spec/design/array-functions.md §11).
	quantAll bool

	// reOuterColumn: the number of frames out (`index` reuses the column index field).
	level int
	// reSubquery: the resolved inner plan, which form, and (for sqIn) the resolved lhs (`lhs`)
	// + the NOT flag (`negated`). reInValues: `lhs` + the constant `list` + `negated`.
	subPlan *queryPlan
	subKind subqueryKind
	list    []Value
}

// rSubscript is one resolved subscript spec in a reSubscript (spec/design/array.md §6): an index
// `a[i]` (isSlice false), or a slice `a[m:n]` whose bounds may be nil (omitted: `a[:n]`/`a[m:]`/`a[:]`).
type rSubscript struct {
	isSlice bool
	index   *rExpr
	lower   *rExpr
	upper   *rExpr
}

// ============================================================================
// Query plans — the resolved, owned form of a query, executable repeatedly (a correlated
// subquery is re-run once per outer row). planQuery (the resolve half of the old runSelect)
// produces a queryPlan; execQueryPlan (the execute half) consumes it against an outer-row
// environment. The split lets a subquery be resolved ONCE — so its structural/type errors fire
// even over an empty outer — yet executed many times (spec/design/grammar.md §26).
// ============================================================================

// queryPlan is a resolved query expression: a SELECT plan or a set-op plan (mirrors QueryExpr).
// Exactly one of sel / setop is non-nil.
type queryPlan struct {
	sel    *selectPlan
	setop  *setOpPlan
	values *valuesPlan
	with   *withPlan
}

// withPlan is a planned nested `WITH … query_expr` (spec/design/cte.md §7): the nested CTE bindings
// + their inline/materialize modes, and the inner query plan that references them. At execution the
// bindings are materialized once and body runs against a fresh CTE context (they establish their
// own scope — the enclosing context is NOT chained in, the documented narrowing §7).
type withPlan struct {
	bindings []*cteBinding
	modes    []cteMode
	body     queryPlan
}

// columnTypes returns the plan's output column types (for a subquery's plan-time column-count
// check + element type).
func (p *queryPlan) columnTypes() []resolvedType {
	if p.sel != nil {
		return p.sel.columnTypes
	}
	if p.values != nil {
		return p.values.columnTypes
	}
	if p.with != nil {
		return p.with.body.columnTypes()
	}
	return p.setop.columnTypes
}

// columnNames returns the plan's output column names — the basis for a CTE's synthetic relation
// when there is no column-rename list (spec/design/cte.md §1).
func (p *queryPlan) columnNames() []string {
	if p.sel != nil {
		return p.sel.columnNames
	}
	if p.values != nil {
		return p.values.columnNames
	}
	if p.with != nil {
		return p.with.body.columnNames()
	}
	return p.setop.columnNames
}

// valuesPlan is a resolved VALUES-body relation (spec/design/grammar.md §42), executable to its
// literal rows — the FROM-position sibling of INSERT … VALUES. rows[r][c] is row r, column c, each
// resolved as a CONSTANT (the body is non-LATERAL, planned parent=nil, so it reads no row).
// columnTypes is the per-column type unified across the rows like a set operation (§25), and
// columnNames is column1, column2, … (PostgreSQL; the derived table's optional column-rename list
// overrides them at the synthetic relation). All rows have len(columnTypes) values.
type valuesPlan struct {
	rows        [][]*rExpr
	columnTypes []resolvedType
	columnNames []string
}

// cteMode is how a referenced CTE is evaluated (spec/design/cte.md §3, cost.md §3). Decided per CTE
// from its reference count and [NOT] MATERIALIZED hint: a single-reference CTE is cteInline, a
// multi-reference (or MATERIALIZED) one is cteMaterialize.
type cteMode int

const (
	// cteInline runs the body in place at each reference (re-evaluates per outer row under
	// correlation, matching PostgreSQL); charges the body's intrinsic cost, no cte_scan_row.
	cteInline cteMode = iota
	// cteMaterialize runs the body once, buffers the rows; each reference scans the buffer,
	// charging cte_scan_row per buffered row.
	cteMaterialize
)

// cteBinding is a planned common table expression, owned by runWith for the whole statement
// (spec/design/cte.md). name is lowercased for case-insensitive FROM matching; table is the
// synthetic relation exposing the body's output columns; source is the planned body (a query plan,
// or — spec/design/writable-cte.md — a data-modifying statement); hint is the [NOT] MATERIALIZED
// override (nil = default); refs counts the FROM references resolved to it during planning (the
// inline-vs-materialize decision — cost.md §3).
//
// For a RECURSIVE CTE (spec/design/recursive-cte.md) source holds the non-recursive (anchor) term
// (its column types fix the synthetic relation's) and recursive carries the recursive term + the
// UNION ALL flag; the binding is in scope inside its own recursive term, so the self-reference
// resolves to it (refs then counts the self-reference too).
type cteBinding struct {
	name  string
	table *catTable
	// source is what this binding evaluates to (cte.md, writable-cte.md): a planned query body, or
	// a data-modifying statement (dm non-nil). Exactly one of plan/dm is meaningful (selected by dm).
	// A data-modifying CTE is always materialized (writable-cte.md §3), so the inline-execution path
	// never touches a dm binding.
	plan      queryPlan // valid when dm == nil
	dm        *dmCte    // non-nil for a data-modifying CTE binding
	recursive *recursiveTerm
	hint      *bool
	refs      int
}

// isDml reports whether this binding is a data-modifying CTE (its source is a statement, not a query
// plan) — writable-cte.md.
func (b *cteBinding) isDml() bool { return b.dm != nil }

// dmCte is a data-modifying CTE's body (spec/design/writable-cte.md): the INSERT/UPDATE/DELETE to run
// (cloned from the AST, executed with the statement's CTE context threaded in) and whether it has no
// RETURNING clause — in which case a FROM reference to it is 0A000 (§5). Exactly one of
// insert/update/delete is non-nil.
type dmCte struct {
	insert      *insert
	update      *update
	delete      *deleteStmt
	noReturning bool
}

// recursiveTerm is the recursive half of a WITH RECURSIVE CTE (spec/design/recursive-cte.md §4):
// the planned recursive term (the UNION's right operand, which references the CTE once) and whether
// the body is UNION ALL (keep every row) versus UNION (drop rows duplicating any already emitted).
type recursiveTerm struct {
	plan     queryPlan
	unionAll bool
}

// cteCtx is the per-statement CTE execution context, threaded through exec* and evalEnv so a FROM
// reference (any nesting depth) can deliver a CTE's rows (spec/design/cte.md §5). modes and bindings
// are fixed after planning; buffers is filled before the main query runs — one slot per CTE in list
// order, holding the materialized rows of a cteMaterialize CTE (an empty placeholder for a cteInline
// one, whose body is run in place from bindings[ci].plan instead). bindings also serves a
// data-modifying CTE's own inner queries, which resolve against the earlier bindings when the
// writable-CTE orchestrator executes them (writable-cte.md §2). The zero value (all nil) is the empty
// context — no CTEs in scope (every non-WITH execution path).
type cteCtx struct {
	modes    []cteMode
	bindings []*cteBinding
	buffers  [][]storedRow
}

// planRel is one relation in a SELECT plan: the table name (looked up in the store at exec), the
// flat offset of its first column, and its column count (for NULL-padding).
type planRel struct {
	tableName string
	// db is the relation's explicit database qualifier (attached-databases.md §3), passed to the
	// scope-aware store funnels at exec (lkpStoreScoped etc.). nil for a bare implicit-scope name → the
	// funnels fall through to the temp-first walk (behavior-neutral for every unqualified query).
	db       *string
	offset   int
	colCount int
	// srf is non-nil when this relation is a COMPUTED set-returning function (generate_series)
	// rather than a base table: tableName is then the function name (never looked up in the
	// store) and the executor generates the rows instead of scanning (functions.md §10).
	srf *srfPlan
	// cte is non-nil (pointing to the index into the statement's CTE list — spec/design/cte.md)
	// when this relation is a reference to a common-table expression rather than a base table:
	// tableName is then the CTE name (never looked up in the store) and the executor delivers its
	// rows from the per-statement cteCtx (a materialized buffer, or the inlined body run in place).
	cte *int
	// derived is non-nil when this relation is a DERIVED TABLE — `FROM (SELECT …) [AS] t`
	// (spec/design/grammar.md §42): a parenthesized subquery used as a relation, mechanically an
	// anonymous always-inlined single-reference CTE. tableName is the alias (never looked up in the
	// store); the executor runs this plan in place, charging its intrinsic cost — no cte_scan_row.
	// Non-lateral it reads no outer row; a lateral one reads the left-hand row.
	derived *queryPlan
	// lateral is true when this relation is a CORRELATED LATERAL item (spec/design/grammar.md §44):
	// its derived body / SRF args reference an earlier sibling (or an enclosing query), so the
	// executor re-materializes it ONCE PER combined left-hand row (with that row pushed as its
	// immediate outer — the correlated-subquery mechanism), rather than materializing it once. Always
	// false for the first relation. Only a srf or derived relation is ever lateral.
	lateral bool
}

// srfKind selects which set-returning function a srfPlan is, picking the row generator at exec
// (spec/design/functions.md §10, array-functions.md §9). The dispatch is hand-written per core.
type srfKind int

const (
	// srfGenerateSeries is generate_series(start, stop[, step]) — an integer series (functions.md §10).
	srfGenerateSeries srfKind = iota
	// srfUnnest is unnest(anyarray) — one row per array element, flattened row-major (array-functions.md §9).
	srfUnnest
	// srfJsonbArrayElements is jsonb_array_elements(jsonb) — one `jsonb` row per array element
	// (json-sql-functions.md §3).
	srfJsonbArrayElements
	// srfJsonbArrayElementsText is jsonb_array_elements_text(jsonb) — one `text` row per array element
	// (the `->>`-style render).
	srfJsonbArrayElementsText
	// srfJsonbObjectKeys is jsonb_object_keys(jsonb) — one `text` row per object key, in canonical key order.
	srfJsonbObjectKeys
	// srfJsonObjectKeys is json_object_keys(json) — one `text` row per object key, in INPUT order
	// (duplicates kept).
	srfJsonObjectKeys
	// srfJsonbEach is jsonb_each(jsonb) — one `(key text, value jsonb)` row per top-level object
	// member, canonical key order (json-sql-functions.md §3). A two-column SRF (the C0 multi-column
	// synthetic table).
	srfJsonbEach
	// srfJsonbEachText is jsonb_each_text(jsonb) — one `(key text, value text)` row per member (the
	// `->>`-style value).
	srfJsonbEachText
	// srfJSONRecord is json[b]_to_record(doc) (R1, json-table.md §2) — ONE record row: map the JSON
	// object's members to the C0 col-def-list columns by name, coercing each to its declared type.
	srfJSONRecord
	// srfJSONRecordset is json[b]_to_recordset(doc) (R1) — setof record: one record row per element
	// of a top-level JSON array (a non-array → 22023).
	srfJSONRecordset
	// srfJsonbPathQuery is jsonb_path_query(jsonb, jsonpath) (P2, jsonpath.md §5.2) — one `jsonb` row
	// per item of the path's evaluation sequence over the context document. `args` is `[ctx, path]`.
	srfJsonbPathQuery
	// srfJsonTable is JSON_TABLE(ctx, path COLUMNS (…)) (T1, json-table.md §3) — a multi-column
	// relation produced by the recursive default-plan expansion. `args` is `[ctx]`; the resolved column
	// tree is the srfPlan's `jsonTable` field.
	srfJsonTable
	// srfJedTables is the jed_tables catalog relation (spec/design/introspection.md §5): a read-only
	// COMPUTED relation — one row per user table of the qualified database, derived at execution from
	// its pinned catalog snapshot. Not a function (it is resolved as a table name), but it rides the
	// srf plan shape so every "computed, not scanned" gate handles it: no store, no index pushdown, no
	// PK order, excluded from the streaming/vectorized fast paths. `args` is empty; the scope is the
	// srfPlan's introspectScope.
	srfJedTables
	// srfJedColumns is the jed_columns catalog relation (introspection.md §5) — one row per column of
	// every user table of the qualified database, in (table, ordinal) order.
	srfJedColumns
	// srfJedIndexes is the jed_indexes catalog relation (introspection.md §5.1, slice I2) — one row
	// per secondary index of every user table (name, table, columns, is_unique, method).
	srfJedIndexes
	// srfJedConstraints is the jed_constraints catalog relation (introspection.md §5.1, slice I2) —
	// one row per CHECK / UNIQUE / FK / EXCLUDE constraint of every user table.
	srfJedConstraints
)

// srfPlan is a resolved set-returning-function row source (spec/design/functions.md §10,
// array-functions.md §9). kind selects the generator: generate_series(start, stop[, step]) (args =
// 2 or 3 integers) or unnest(anyarray) (args = the single array expression). Non-LATERAL, so each
// arg evaluates against the params/outer environment with no local row. The produced column's type
// lives on the synthetic relation (built in resolveSRF).
type srfPlan struct {
	kind srfKind
	args []*rExpr
	// recordCols is the declared output columns for a record-returning SRF (srfJSONRecord[set]) — the
	// C0 col-def list, used to map JSON members to columns by name + coerce. nil for every other kind.
	recordCols []catColumn
	// jsonTable is the resolved column tree for a JSON_TABLE SRF (srfJsonTable), else nil.
	jsonTable *jtPlan
	// introspectScope is the validated database scope of a catalog relation (srfJedTables /
	// srfJedColumns — introspection.md §5): "main" (also the unqualified default), "temp", or a
	// lowercased attachment name. "" for every other kind.
	introspectScope string
}

// jtPlan is a resolved JSON_TABLE plan (T1, json-table.md §3) — the compiled root path + the column
// tree + the total flattened width.
type jtPlan struct {
	// rootPath is the compiled root jsonpath (its evaluation over `ctx` yields the row items).
	rootPath string
	// width is the total number of flattened output columns.
	width int
	// columns is the top-level column tree.
	columns []jtCol
}

// jtCol is one resolved JSON_TABLE column (json-table.md §3.3). Leaf columns carry their flat output
// index; a nested column carries its child subtree. Modeled as a tagged union (one struct per kind).
type jtCol interface{ isJtCol() }

// jtColOrdinality is `FOR ORDINALITY` — the level's 1-based row counter, written to flat index `idx`.
type jtColOrdinality struct {
	idx int
}

// jtColRegular is a regular column: evaluate `path` over the row item, apply JSON_VALUE (scalar) or
// JSON_QUERY (json/jsonb) semantics, coerce to `returning`, and write it to flat index `idx`.
type jtColRegular struct {
	idx       int
	returning scalarType
	decimal   *decimalTypmod
	path      string
	// query selects JSON_QUERY semantics (json/jsonb returning) vs JSON_VALUE (scalar).
	query   bool
	wrapper jsonWrapper
	onEmpty jsonOnBehavior
	onError jsonOnBehavior
}

// jtColExists is an EXISTS column: JSON_EXISTS of `path`, coerced to `returning` (bool/int), written
// to flat index `idx`.
type jtColExists struct {
	idx       int
	returning scalarType
	path      string
	onError   jsonOnBehavior
}

// jtColNested is a NESTED PATH subtree: expanded over the row item (the default-plan LEFT OUTER /
// sibling UNION).
type jtColNested struct {
	path    string
	columns []jtCol
}

func (*jtColOrdinality) isJtCol() {}
func (*jtColRegular) isJtCol()    {}
func (*jtColExists) isJtCol()     {}
func (*jtColNested) isJtCol()     {}

// planJoin is one join in a SELECT plan: its kind and resolved ON predicate (nil for CROSS). The
// right relation is rels[k+1].
type planJoin struct {
	kind joinKind
	on   *rExpr
}

// orderSlot is a resolved ORDER BY key: a flat/synthetic slot + the per-key direction flags + an
// optional collation. A nil collation is the C/value order; a non-nil collation orders this key by
// its UCA sort key (spec/design/collation.md §8) via the decorate sorter — it never reaches the
// spill Sorter (collation is in-memory only this slice), which ignores the field.
type orderSlot struct {
	idx        int
	descending bool
	nullsFirst bool
	collation  *Collation
}

// selectPlan is a resolved SELECT, executable against an outer-row environment (the execute half
// of the old runSelect, lifted to a value so a correlated subquery can re-run it per outer row).
type selectPlan struct {
	rels      []planRel
	joins     []planJoin
	filter    *rExpr
	isAgg     bool
	groupKeys []int
	// groupExprs is the materialized general-expression GROUP BY keys (`GROUP BY a + b`,
	// aggregates.md §15), in synthetic-slot order. Before bucketing, each post-WHERE row evaluates
	// these and appends the values at flat slots inputWidth+k, so a master grouping key index in
	// groupKeys / groupSets may point at one — the whole-row bucket machinery stays slot-based. Empty
	// when every grouping key is a plain column (the common case, byte-identical to before).
	groupExprs []*rExpr
	// groupSets are the grouping sets to compute (spec/design/aggregates.md §12). A plain GROUP BY
	// (and the whole-table aggregate) is a single set; ROLLUP/CUBE/GROUPING SETS produce several.
	groupSets []groupSetPlan
	// groupingSpecs has one entry per GROUPING() call in the projection / HAVING, in synthetic-slot
	// order: the master-grouping-column positions of its arguments. Each call's value per group row is
	// computed from the row's grouping-set mask and appended after the aggregate results.
	groupingSpecs [][]int
	aggSpecs      []aggSpec
	// hasWindow is true when the select list has a window function — the query runs the blocking
	// WINDOW stage (after WHERE, before ORDER BY/LIMIT) and takes the eager path (never streaming).
	// Mutually exclusive with isAgg in S0 (spec/design/window.md §5.2).
	hasWindow bool
	// windowSpecs is one resolved window function per select-list OVER call (empty unless hasWindow).
	// The window stage appends each spec's per-row result after the input columns and the materialized
	// window keys, so the projection references result i as flat slot input_width+len(windowKeys)+i
	// (spec/design/window.md §5.1).
	windowSpecs []windowSpec
	// windowKeys is the materialized window-key expressions (a non-column PARTITION BY / ORDER BY key
	// — `PARTITION BY a + b`, `ORDER BY a % 2`), in synthetic-slot order. Before the window stage each
	// row evaluates these and appends the values at flat slots input_width+k, so the slot-based
	// partition / sort / frame machinery is unchanged. Empty when every window key is a bare column.
	windowKeys []*rExpr
	having     *rExpr
	order      []orderSlot
	// orderExprs is the materialized ORDER BY expression-key expressions (`ORDER BY a + 1`,
	// `ORDER BY abs(b)`), in the order their sort slots reference them. Just before the sort each row
	// evaluates these and appends the values at final_row_width+k (after any window / grouped columns),
	// so the slot-based sort stays unchanged — the window-key precedent (window.md §5.1). Empty when
	// every ORDER BY key is a bare column or ordinal (the common case, byte-identical to before).
	orderExprs  []*rExpr
	projections []*rExpr
	columnNames []string
	columnTypes []resolvedType
	distinct    bool
	limit       *int64
	offset      *int64
	// pkOrdered reports that ORDER BY is satisfied by the single base relation's PRIMARY-KEY scan
	// order — the table tree already yields rows in this order, so the sort is elided (and with a
	// LIMIT the scan short-circuits a top-N). True iff the query is a single-table, non-aggregate,
	// non-DISTINCT SELECT whose ORDER BY keys are a prefix of the PK columns, all one direction with
	// the column's stored key collation (spec/design/cost.md §3 "ORDER BY satisfied by primary-key
	// order"). Secondary-index order is a follow-on.
	pkOrdered bool
	// pkReverse is the PK scan direction when pkOrdered: true ⇒ the order is all-DESC over the full
	// PK, served by a REVERSE scan; false ⇒ all-ASC (forward). Always false when !pkOrdered.
	pkReverse bool
	// indexOrder reports that ORDER BY is satisfied by walking a B-tree SECONDARY index in key order
	// (with a LIMIT top-N) — non-nil when the PK scan does not satisfy the order but the index does
	// (cost.md §3 "secondary-index order"). Mutually exclusive with pkOrdered (the PK scan is
	// cheaper). nil keeps the eager/streaming sort.
	indexOrder *indexOrderPlan
	// joinPkOrdered reports that ORDER BY is satisfied by the OUTER relation's PK scan order in a
	// two-table INNER/CROSS join (cost.md §3 "JOIN"): the nested loop drives the outer in PK order, so
	// its output is already in order — the sort is elided and a LIMIT short-circuits the loop. Set only
	// for exactly two non-lateral base relations, a LIMIT, and a forward outer-PK ORDER BY.
	joinPkOrdered bool
	// relBounds is the scan-bound pushdown, ONE entry per relation in rels: the WHERE
	// conjuncts that bound that relation's storage key, so its scan seeks/ranges instead of walking
	// the whole B-tree (spec/design/cost.md §3 "bounded scan"). nil ⇒ a full scan of that relation.
	// In a JOIN each base table is bounded independently by the WHERE predicates on its OWN primary
	// key against a CONSTANT (literal/param/outer) — a cross-relation `b.pk = a.x` is the
	// index-nested-loop case (a follow-on). The residual filter stays the WHOLE `filter`, re-applied
	// after the join — the bound only narrows which rows are scanned.
	relBounds []*scanBound
	// relINLBounds is the INDEX-NESTED-LOOP scan bounds, one per relation (cost.md §3 "JOIN").
	// Non-nil for a join inner relation whose primary key / indexed column is compared to a SIBLING
	// column of an earlier relation (`a JOIN b ON b.pk = a.x`) — a per-outer-row bound resolved from
	// the combined left-hand row. When set, that relation is NOT materialized once up front; the join
	// loop re-materializes it per left row (like a correlated LATERAL), seeking instead of
	// full-scanning — O(N·M) → O(N·log M). nil ⇒ the ordinary once-materialized relBounds path. A
	// non-nil entry takes precedence over relBounds for that relation.
	relINLBounds []*scanBound
	// relMasks is the TOUCHED SET per relation (cost.md §3 "The touched set"; large-values.md
	// §14): which of its columns this query statically references. Drives the chain-page_read /
	// value_decompress portion of the scan's up-front cost block — an untouched spilled or
	// compressed column charges nothing, however many records the bound admits.
	relMasks [][]bool
}

// setOpPlan is a resolved set operation: both operands planned with the same parent scope, the
// unified output types, and the trailing ORDER BY / LIMIT / OFFSET resolved by output column.
type setOpPlan struct {
	op          setOpKind
	all         bool
	lhs         queryPlan
	rhs         queryPlan
	columnNames []string
	columnTypes []resolvedType
	order       []orderSlot
	limit       *int64
	offset      *int64
}

// evalEnv is the environment threaded into the per-row evaluator (spec/design/grammar.md §26):
// the engine (to run a correlated subquery's plan), the bound parameters, and the stack of
// enclosing rows (innermost LAST) a correlated reference reads. outer is empty at the top level;
// a correlated subquery pushes the current row before running its inner plan, so an reOuterColumn
// at frame `level` reads outer[len(outer)-level][index].
type evalEnv struct {
	exec   *engine
	params []Value
	outer  []storedRow
	// The per-statement entropy+clock state (spec/design/entropy.md §5): the uuidv7 monotonic counter
	// + the once-resolved statement clock. The injected random/clock functions live on exec.session.seam
	// (handle-scoped); only the volatile uuid generators touch any of this.
	rng *stmtRng
	// ctes is the statement's CTE execution context (spec/design/cte.md §5), so a FROM reference at
	// any nesting depth delivers a CTE's rows. The zero cteCtx for every non-WITH statement.
	ctes cteCtx
}

// rCaseArm is one resolved (condition, result) branch of a reCase node (spec/design/grammar.md
// §23). The condition is the searched boolean predicate, or the simple form's resolved
// `operand = value` equality.
type rCaseArm struct {
	cond   *rExpr
	result *rExpr
}

// ============================================================================
// Aggregate resolution + accumulation (spec/design/aggregates.md).
//
// An aggregate query's select list resolves in "collect" mode: each aggregate call is
// collected into an aggSpec (its plan + resolved argument) and replaced by a reference to a
// synthetic-row slot (an reColumn indexing the finalized aggregate results), so the existing
// evaluator projects the result with no new node. Outside collect mode (WHERE / ON / an
// aggregate's own argument / any non-aggregate query) a column resolves normally and an
// aggregate call is a 42803 grouping error.
// ============================================================================

// aggCtx threads the aggregate-resolution mode through resolve. collecting == false is the
// Forbidden mode (a FuncCall is 42803; columns resolve normally); collecting == true is an
// aggregate query's projection (a FuncCall collects into specs and resolves to a synthetic
// slot len(groupKeys)+index; a column resolves to its position among groupKeys if it is a
// grouping key, else 42803). groupKeys holds the resolved flat indices of the GROUP BY
// columns (empty for whole-table aggregation). The synthetic row the projection evaluates
// against is [group_key_values..., agg_results...].
type aggCtx struct {
	collecting bool
	groupKeys  []int
	// groupKeyExprs is parallel to groupKeys: for each master grouping key, a non-nil *groupKeyExpr
	// (canonical AST + resolved type) if it is a general EXPRESSION key (`GROUP BY a + b`,
	// aggregates.md §15) — so a projection / HAVING / ORDER BY expression that structurally matches it
	// resolves to that group's synthetic slot — or nil for a plain COLUMN key (matched by the column
	// path instead).
	groupKeyExprs []*groupKeyExpr
	specs         []aggSpec
	// windowing marks a non-aggregate WINDOW query's projection (spec/design/window.md §5.1):
	// bare columns resolve to the real input row (like the Forbidden mode), and a FuncCall carrying
	// an OVER clause collects into windowSpecs and resolves to the synthetic slot
	// windowBase+window_index — the window stage appends each function's result after the input
	// columns. S0 narrows window + aggregate/GROUP BY to 0A000, so collecting and windowing are
	// never both set.
	windowing   bool
	windowBase  int
	windowSpecs []windowSpec
	// windowKeys collects the materialized window-key expressions (a non-column PARTITION BY / ORDER
	// BY key — `PARTITION BY a + b`, or `ORDER BY sum(x) + 1`), each resolved to the placeholder slot
	// windowKeyBase+k. A bare column or a bare aggregate (`ORDER BY sum(x)`) resolves to its real row
	// slot and is NOT materialized (spec/design/window.md §5.1).
	windowKeys []*rExpr
	// groupingSpecs collects one entry per GROUPING(c1,…,ck) call from the projection / HAVING — the
	// master-grouping-column POSITIONS (indices into groupKeys) of its arguments. Each call resolves
	// to the placeholder slot groupingGsBase+index, rebased after resolution to its real trailing
	// synthetic slot (spec/design/aggregates.md §12).
	groupingSpecs [][]int
}

// windowSpec is one resolved window function (spec/design/window.md §5.1): its plan, the resolved
// PARTITION BY key column slots (flat input-row indices), and the resolved within-partition ORDER
// BY (sort keys over the input row, PK tie-break applied by the stable sort over the PK-ordered
// scan).
type windowSpec struct {
	plan      windowPlan
	partition []int
	order     []orderSlot
	// args holds the resolved function arguments (empty for the no-argument ranking functions;
	// ntile's bucket count is one integer argument, evaluated once per partition). Future
	// offset/value functions (lag/lead/nth_value, S2+) carry their value/offset/default here.
	// For an aggregate window (plan == planAgg) the aggregate operand (if any) is args[0];
	// COUNT(*) has empty args.
	args []*rExpr
	// aggPlan is the aggregate runtime plan when plan == planAgg (S3 — sum/count/min/max/avg
	// OVER (...)). Go's windowPlan is an int enum and cannot carry a payload like Rust's
	// WindowPlan::Agg(AggPlan) tuple variant, so the aggregate plan rides alongside here.
	aggPlan aggPlan
	// frame is the resolved explicit frame, or nil for the default frame (RANGE UNBOUNDED PRECEDING
	// TO CURRENT ROW with an ORDER BY, the whole partition without — window.md §6). Mirrors Rust's
	// WindowSpec.frame.
	frame *resolvedFrame
	// filter is agg(x) FILTER (WHERE cond) OVER (…) — a per-frame-row boolean restricting which frame
	// rows fold into the window aggregate (aggregates.md §20). Non-nil only for an aggregate window
	// function (a non-aggregate window function with FILTER is 0A000). A FILTER disables the
	// sliding-frame optimization (a filtered row can't be cleanly un-folded) — every frame re-folds.
	filter *rExpr
}

// resolvedFrame is a resolved window frame (spec/design/window.md §6). ROWS physical offsets,
// GROUPS peer-group offsets (both integer counts), and RANGE value offsets over the ordering key.
type resolvedFrame struct {
	mode    frameMode
	start   resolvedBound
	end     resolvedBound
	exclude frameExclusion // EXCLUDE … — rows dropped from [lo, hi) per current row (window.md §6)
}

// resolvedBoundKind distinguishes the five resolved frame-boundary forms.
type resolvedBoundKind int

const (
	boundUnboundedPreceding resolvedBoundKind = iota
	boundPreceding                            // offset before the current row; offVal carries it
	boundCurrentRow
	boundFollowing // offset after the current row; offVal carries it
	boundUnboundedFollowing
)

// resolvedBound is one resolved frame boundary; offVal carries the non-negative offset for
// boundPreceding / boundFollowing (unused otherwise) — a ValInt row/group count for ROWS/GROUPS,
// or the numeric Value (Int over an integer key, Decimal over a decimal key) for RANGE.
type resolvedBound struct {
	kind   resolvedBoundKind
	offVal Value
}

// windowPlan is the runtime plan for one window function (spec/design/window.md §4). S0:
// row_number only; ranking / offset / aggregate-window / frame plans land in S1–S4.
type windowPlan int

const (
	// planRowNumber — ROW_NUMBER(): the 1-based sequence position within the partition.
	planRowNumber windowPlan = iota
	// planRank — RANK(): 1 + the number of rows in earlier peer groups (ties share a rank, gap).
	planRank
	// planDenseRank — DENSE_RANK(): 1 + the number of earlier peer groups (ties share, no gap).
	planDenseRank
	// planPercentRank — PERCENT_RANK(): (rank-1)/(N-1), 0 when N=1; f64 (PG's float8, window.md §4).
	planPercentRank
	// planCumeDist — CUME_DIST(): (rows through the current peer group)/N; f64 (PG's float8).
	planCumeDist
	// planNtile — NTILE(n): distribute the partition into n ranked buckets (larger first),
	// numbered 1..n. Position-based (not peer-based); n <= 0 → 22014; NULL n → NULL for every row.
	planNtile
	// planLag — LAG(value [, offset [, default]]): value `offset` rows EARLIER in the partition
	// (default offset 1); out-of-range → default (or NULL). Frame-insensitive (sorted position).
	planLag
	// planLead — LEAD(value [, offset [, default]]): value `offset` rows LATER in the partition;
	// otherwise identical to LAG (the offset direction flips).
	planLead
	// planAgg — an aggregate used as a window function (S3): sum/count/min/max/avg(...) OVER (...),
	// folded over the row's default frame (running with a window ORDER BY, whole-partition without)
	// or an explicit frame (S4 — spec/design/window.md §6). Reuses the aggregate `acc` kernels; the
	// aggregate plan is held in windowSpec.aggPlan and the operand (if any) is args[0]. Mirrors
	// Rust's WindowPlan::Agg.
	planAgg
	// planFirstValue — FIRST_VALUE(v): the value of the frame's first row (S4). args[0] is the
	// value expression; frame-sensitive. Mirrors Rust's WindowPlan::FirstValue.
	planFirstValue
	// planLastValue — LAST_VALUE(v): the value of the frame's last row (S4). args[0] is the value
	// expression; frame-sensitive. Mirrors Rust's WindowPlan::LastValue.
	planLastValue
	// planNthValue — NTH_VALUE(v, n): the value of the frame's n-th row, NULL if the frame has < n
	// rows (S4). args[0] is the value, args[1] the position; frame-sensitive. Mirrors Rust's
	// WindowPlan::NthValue.
	planNthValue
)

// aggPlan is the runtime plan for one aggregate, fixed at resolve from the function + operand
// type (the PG widening — spec/design/aggregates.md §3).
type aggPlan int

const (
	planCountStar  aggPlan = iota // COUNT(*) — count every row
	planCount                     // COUNT(expr) — count non-NULL inputs
	planSumInt                    // SUM(i16|i32) — accumulate i64, result i64 (trap at i64)
	planSumDecimal                // SUM(i64|decimal) — accumulate decimal, result decimal
	planAvg                       // AVG — decimal sum + i64 count; result sum/count (NULL if 0)
	planSumFloat32                // SUM(f32) — canonical-order fold, result f32 (float.md §7)
	planSumFloat64                // SUM(f64) — canonical-order fold, result f64
	planAvgFloat32                // AVG(f32) — fold sum / count, result f32
	planAvgFloat64                // AVG(f64) — fold sum / count, result f64
	planMin
	planMax
	planJsonbAgg       // jsonb_agg(x) — aggregate the JSON images into a jsonb array
	planJsonAgg        // json_agg(x) — same array, typed json (spaced canonical render)
	planJsonbAggStrict // jsonb_agg_strict(x) — skip NULL-valued rows; jsonb array
	planJsonAggStrict  // json_agg_strict(x) — skip NULL-valued rows; json array
	// json[b]_object_agg[_unique] (B4) — aggregate (key, value) pairs (a Row operand) into a JSON
	// object (json-sql-functions.md §4). The plan encodes the json-vs-jsonb render and the _unique
	// flag; the operand is a 2-field Row(key, value) the fold splits back out.
	planJsonbObjectAgg       // jsonb_object_agg(k, v) — canonical (last-wins dedup, key sort, spaced)
	planJsonObjectAgg        // json_object_agg(k, v) — row order + dups, '{ … }' brace-padded spacing
	planJsonbObjectAggUnique // jsonb_object_agg_unique(k, v) — as jsonb, but 22030 on a duplicate key
	planJsonObjectAggUnique  // json_object_agg_unique(k, v) — as json, but 22030 on a duplicate key
	// Ordered-set aggregates (spec/design/aggregates.md §13). The WITHIN GROUP direction +
	// percentile fraction live on the aggSpec/acc, not the plan.
	planMode           // mode() — the most frequent value (tie → first in sort order)
	planPercentileDisc // percentile_disc(f) — the discrete percentile (an actual input value)
	planPercentileCont // percentile_cont(f) — the continuous (interpolated) percentile, f64
	// percentile_cont(f) over an interval input — the continuous percentile interpolated in the
	// interval domain (lo + (hi-lo)·pct, PG interval_lerp); result interval (aggregates.md §13).
	// Values buffered as ValInterval in osaVals (the non-cont branch).
	planPercentileContInterval
	// Hypothetical-set aggregates (spec/design/aggregates.md §19): rank/dense_rank/percent_rank/
	// cume_dist used WITH a WITHIN GROUP clause (these names are ALSO window functions; the WITHIN
	// GROUP clause routes them here). The hypothetical-row direct args + the WITHIN GROUP key
	// operands + per-key sort specs live on the aggSpec's hypo field, not the plan.
	planHypoRank        // rank(args) — 1 + the number of group rows that sort strictly before; result i64
	planHypoDenseRank   // dense_rank(args) — 1 + the number of DISTINCT values strictly before; result i64
	planHypoPercentRank // percent_rank(args) — (rank − 1) / N; result f64
	planHypoCumeDist    // cume_dist(args) — (#rows ≤ hyp + 1) / (N + 1); result f64
)

// aggSpec is one resolved aggregate: its plan and its resolved argument (evaluated per input
// row against the real row). operand is nil for COUNT(*). distinct (COUNT(DISTINCT x),
// aggregates.md §5) folds only the distinct non-NULL argument values — the fold loop keeps a
// per-group value-canonical set and skips a value already seen. Only set in the aggregation
// stage; a window aggregate is never DISTINCT (0A000, rejected at resolve).
type aggSpec struct {
	plan     aggPlan
	operand  *rExpr
	distinct bool
	// filter is the resolved FILTER (WHERE cond) boolean predicate (SUM(x) FILTER (WHERE cond) —
	// aggregates.md §11); nil for an unfiltered aggregate. The fold loop evaluates it per input row
	// and folds only the rows for which it is TRUE (so the filter applies before the DISTINCT dedup).
	filter *rExpr
	// osaDesc / osaFrac are the ordered-set aggregate parameters (mode/percentile_* —
	// aggregates.md §13/§17), set only for the planMode/planPercentile* plans. osaDesc is the
	// WITHIN GROUP sort direction; osaFrac is the resolved **direct argument** (the percentile
	// fraction) — resolved in the grouped context so it references grouping columns by their
	// synthetic key slots (a non-grouped column is 42803, matching PG's "direct arguments … must
	// use only grouped columns") and is evaluated **per group** at finalize against the synthetic
	// row. nil for mode (no direct argument).
	osaDesc bool
	osaFrac *rExpr
	// osaCollation is the WITHIN GROUP key's collation (aggregates.md §13) — non-nil for an explicit
	// COLLATE on the key or a column's frozen non-C collation, nil for the default byte (C) order. The
	// finalize sort applies it to the buffered text values.
	osaCollation *Collation
	// hypo is the hypothetical-set aggregate parameters (rank/dense_rank/percent_rank/cume_dist
	// WITHIN GROUP — aggregates.md §19), set only for the planHypo* plans, nil otherwise. (operand
	// is nil here — the keys are buffered as a tuple per row from hypo.keys.)
	hypo *hypoParams
}

// keySort is a single WITHIN GROUP ordering-key sort spec (aggregates.md §13/§19): direction, NULL
// placement, and an optional collation (text keys only).
type keySort struct {
	desc       bool
	nullsFirst bool
	collation  *Collation
}

// hypoParams are the resolve-time parameters of a hypothetical-set aggregate (aggregates.md §19).
// args are the hypothetical-row direct arguments (evaluated PER GROUP at finalize, like an OSA
// fraction — they may reference grouping columns); keys are the WITHIN GROUP key operands
// (evaluated PER ROW during the fold and buffered as a tuple); sorts is the per-key ordering spec.
// The three slices have equal length (the arity check at resolve).
type hypoParams struct {
	args  []*rExpr
	keys  []*rExpr
	sorts []keySort
}

// acc is a running aggregate accumulator (one per aggSpec), folded per input row then finalized.
type acc struct {
	plan     aggPlan
	count    int64
	sumInt   int64
	sumDec   Decimal
	seen     bool
	cur      Value
	hasCur   bool
	floatSum *floatSumAcc // non-nil for the float SUM/AVG plans (the streaming scan-order fold — float.md §7)
	// json_agg / jsonb_agg accumulator (B4): the inputs' JSON-image nodes in row order. jsonAsJSON
	// selects the `json` result type (vs jsonb); jsonStrict skips a NULL-valued row. `seen` is reused
	// to mark the group non-empty even when the strict filter drops every row (empty group → NULL,
	// all-skipped group → `[]`).
	jsonNodes  []JsonNode
	jsonAsJSON bool
	jsonStrict bool
	// json[b]_object_agg[_unique] accumulator (B4): the (key, value) pairs in row order. objAgg is
	// true for the object-agg plans; objUnique errors 22030 on a duplicate key. jsonAsJSON selects
	// the json (brace-padded, row order, dups) vs jsonb (canonical, last-wins) finalize render, and
	// `seen` distinguishes a zero-row group (→ NULL) from a non-empty one (→ an object).
	objPairs  []objAggPair
	objAgg    bool
	objUnique bool
	// Ordered-set aggregate state (spec/design/aggregates.md §13): the WITHIN GROUP direction +
	// the **evaluated** percentile fraction for this group, plus the collected non-NULL values
	// (percentile_cont widens to f64 into osaFloats; mode/percentile_disc keep the Value in
	// osaVals). osaFrac is the per-group fraction Value — the direct argument is evaluated per
	// group against the synthetic row just before finalize (aggregates.md §13/§17): non-nil for
	// percentile_* (the Value may be NULL → NULL result, or numeric), nil for mode. Sorted +
	// computed at finalize.
	osaDesc bool
	osaFrac *Value
	// osaCollation is the WITHIN GROUP key collation (aggregates.md §13) applied to the finalize sort
	// of the buffered text values; nil is the default byte (C) order.
	osaCollation *Collation
	osaVals      []Value
	osaFloats    []float64
	// hypoRows buffers every row's WITHIN GROUP key TUPLE for a hypothetical-set aggregate
	// (rank/dense_rank/percent_rank/cume_dist — aggregates.md §19). The fold loop appends each tuple
	// (no NULL-skip — every row counts); at finalize (in the group-emission loop, where the per-group
	// hypothetical row + the spec's sort specs are available) finalizeHypothetical counts how that
	// hypothetical row would rank. plan selects the result formula.
	hypoRows [][]Value
}

// objAggPair is one (key, value) pair accumulated by json[b]_object_agg (the key already coerced to
// text, the value its raw Value carried to finalize where it becomes its to_jsonb / json image).
type objAggPair struct {
	key string
	val Value
}

func newAcc(plan aggPlan) *acc {
	a := &acc{plan: plan}
	if plan == planSumDecimal || plan == planAvg {
		a.sumDec = decimalFromInt64(0)
	}
	switch plan {
	case planSumFloat32, planAvgFloat32:
		a.floatSum = newFloatSumAcc(true)
	case planSumFloat64, planAvgFloat64:
		a.floatSum = newFloatSumAcc(false)
	case planJsonbAgg:
	case planJsonAgg:
		a.jsonAsJSON = true
	case planJsonbAggStrict:
		a.jsonStrict = true
	case planJsonAggStrict:
		a.jsonAsJSON, a.jsonStrict = true, true
	case planJsonbObjectAgg:
		a.objAgg = true
	case planJsonObjectAgg:
		a.objAgg, a.jsonAsJSON = true, true
	case planJsonbObjectAggUnique:
		a.objAgg, a.objUnique = true, true
	case planJsonObjectAggUnique:
		a.objAgg, a.jsonAsJSON, a.objUnique = true, true, true
	}
	return a
}

// newAccFromSpec builds the accumulator for one resolved aggregate. Ordered-set aggregates
// (mode/percentile_* — aggregates.md §13) need their WITHIN GROUP direction + fraction (carried on
// the spec, not the plan); every other plan delegates to newAcc. Only the aggregation stage builds
// ordered-set accumulators — the window stage (which calls newAcc directly) never sees one (an
// ordered-set aggregate with OVER is 0A000, rejected at resolve).
func newAccFromSpec(s aggSpec) *acc {
	switch s.plan {
	case planMode, planPercentileDisc, planPercentileCont, planPercentileContInterval:
		// The per-group fraction (osaFrac) is filled in just before finalize (the direct argument
		// evaluated against the synthetic row); mode keeps it nil.
		return &acc{plan: s.plan, osaDesc: s.osaDesc, osaCollation: s.osaCollation}
	case planHypoRank, planHypoDenseRank, planHypoPercentRank, planHypoCumeDist:
		// A hypothetical-set aggregate buffers each row's WITHIN GROUP key tuple (aggregates.md §19);
		// it is finalized inline in the group-emission loop (it needs the spec's sort specs).
		return &acc{plan: s.plan}
	default:
		return newAcc(s.plan)
	}
}

// clone returns an independent snapshot of the running accumulator, so the window stage can
// finalize a peer-group's cumulative value without consuming the still-running acc (Rust's
// `acc.clone().finalize()`). The acc struct copies by value, but floatSum is a POINTER whose
// `finite` slice would otherwise alias the original — deep-copy it (the only shared-reference
// field; `cur` holds a Value finalize only reads, never mutates). Mirrors Rust's #[derive(Clone)]
// on Acc with its deep slice clone.
func (a *acc) clone() *acc {
	c := *a // value copy (count/sumInt/sumDec/seen/cur/hasCur are independent)
	if a.floatSum != nil {
		// floatSum is a streaming running total (no slice) — a plain value copy is independent.
		fs := *a.floatSum
		c.floatSum = &fs
	}
	if a.jsonNodes != nil {
		// Deep-copy the node slice so a window peer-group's finalize doesn't consume the running
		// acc's nodes (mirrors the floatSum slice clone above).
		c.jsonNodes = append([]JsonNode(nil), a.jsonNodes...)
	}
	if a.objPairs != nil {
		// Deep-copy the (key, value) pairs slice (as jsonNodes above) so a cloned-finalize over a
		// window peer-group doesn't alias the still-running acc.
		c.objPairs = append([]objAggPair(nil), a.objPairs...)
	}
	// Ordered-set accumulators are never windowed (clone is the window-stage snapshot), but deep-copy
	// the collected slices anyway so a clone never aliases the original.
	if a.osaVals != nil {
		c.osaVals = append([]Value(nil), a.osaVals...)
	}
	if a.osaFloats != nil {
		c.osaFloats = append([]float64(nil), a.osaFloats...)
	}
	// Hypothetical-set accumulators are never windowed (clone is the window-stage snapshot), but
	// deep-copy the buffered tuples anyway so a clone never aliases the original.
	if a.hypoRows != nil {
		c.hypoRows = make([][]Value, len(a.hypoRows))
		for i := range a.hypoRows {
			c.hypoRows[i] = append([]Value(nil), a.hypoRows[i]...)
		}
	}
	return &c
}

// fold folds one input value into the accumulator. NULL arguments are skipped (COUNT(*)
// ignores the value and always counts). Traps 22003 on SUM/AVG overflow at the result bound.
// A decimal SUM/AVG fold charges size-scaled decimal_work against the running accumulator
// (the `+` formula — spec/design/cost.md §3 "decimal_work"); MIN/MAX folds are direct Value
// compares like the sort's and stay unmetered.
func (a *acc) fold(v Value, m *costMeter) error {
	switch a.plan {
	case planCountStar:
		a.count++
	case planCount:
		if !v.IsNull() {
			a.count++
		}
	case planSumInt:
		if !v.IsNull() {
			s := a.sumInt + v.Int
			if (v.Int > 0 && s < a.sumInt) || (v.Int < 0 && s > a.sumInt) {
				return overflowErr(scalarInt64)
			}
			a.sumInt = s
			a.seen = true
		}
	case planSumDecimal:
		if !v.IsNull() {
			in := toDecimal(v)
			m.Charge(costs.DecimalWork * (workLinear(a.sumDec, in) - 1))
			if err := m.Guard(); err != nil {
				return err
			}
			// Uncapped: the running sum may exceed the §2 format cap mid-fold; only the FINAL
			// result is cap-checked (in finalize), matching PG and making the trap
			// order-independent (spec/design/decimal.md §2, determinism.md §7).
			a.sumDec = a.sumDec.AddUncapped(in)
			a.seen = true
		}
	case planAvg:
		if !v.IsNull() {
			in := toDecimal(v)
			m.Charge(costs.DecimalWork * (workLinear(a.sumDec, in) - 1))
			if err := m.Guard(); err != nil {
				return err
			}
			// Uncapped (as planSumDecimal): the average's final divide brings the value back in
			// range, so AVG never traps on an over-cap intermediate sum the way PG does not.
			a.sumDec = a.sumDec.AddUncapped(in)
			a.count++
		}
	case planSumFloat32, planSumFloat64, planAvgFloat32, planAvgFloat64:
		// Float SUM/AVG fold into the streaming running total in scan order; NULLs are skipped. The
		// fold order is ledgered non-deterministic (spec/design/float.md §7). The per-row
		// aggregate_accumulate is charged by the caller, so this stays O(1)/row and O(1) memory.
		if !v.IsNull() {
			a.floatSum.add(v)
		}
	case planMin, planMax:
		if !v.IsNull() {
			if !a.hasCur {
				a.cur, a.hasCur = v, true
			} else {
				c := valueCmp(a.cur, v)
				keepCur := (a.plan == planMin && c <= 0) || (a.plan == planMax && c >= 0)
				if !keepCur {
					a.cur = v
				}
			}
		}
	case planJsonbAgg, planJsonAgg, planJsonbAggStrict, planJsonAggStrict:
		// Mark the group non-empty even when the strict filter drops this row (an all-skipped group
		// still finalizes to `[]`, not NULL — only a zero-row group is NULL).
		a.seen = true
		// Non-strict: a NULL input contributes a JSON null; `_strict` skips it. Each input's JSON
		// image is the to_jsonb kernel (deferred 0A000 sources propagate here). One generated_row
		// per appended element.
		if !(a.jsonStrict && v.IsNull()) {
			m.Charge(costs.GeneratedRow)
			if err := m.Guard(); err != nil {
				return err
			}
			node, err := valueToNode(v)
			if err != nil {
				return err
			}
			a.jsonNodes = append(a.jsonNodes, node)
		}
	case planJsonbObjectAgg, planJsonObjectAgg, planJsonbObjectAggUnique, planJsonObjectAggUnique:
		// The operand is a Row(key, value) composite; mark the group non-empty (an empty group → NULL,
		// not `{}`) and split the two fields back out.
		a.seen = true
		m.Charge(costs.GeneratedRow)
		if err := m.Guard(); err != nil {
			return err
		}
		if v.Kind != ValComposite || v.composite() == nil || len(*v.composite()) != 2 {
			panic("BUG: object_agg operand is a 2-field Row")
		}
		fields := *v.composite()
		kv, vv := fields[0], fields[1]
		// The key coerces to text (text/integer/decimal/boolean); a NULL key → 22023, but with a
		// DIFFERENT message from build_object's "key must not be null" (NULL handled here, before
		// objectKeyText, so the non-NULL coercion + the non-scalar 0A000 still reuse it).
		if kv.Kind == ValNull {
			return newError(InvalidParameterValue, "field name must not be null")
		}
		key, err := objectKeyText(kv, 1)
		if err != nil {
			return err
		}
		if a.objUnique {
			for i := range a.objPairs {
				if a.objPairs[i].key == key {
					return newError(DuplicateJsonObjectKeyValue, "duplicate JSON object key value")
				}
			}
		}
		a.objPairs = append(a.objPairs, objAggPair{key: key, val: vv})
	case planMode, planPercentileDisc, planPercentileCont, planPercentileContInterval:
		// Collect the non-NULL aggregated argument (the WITHIN GROUP order key, evaluated per row).
		// percentile_cont (numeric) widens each value to f64 up front (the correctly-rounded cast,
		// matching PG's numeric→float8); mode/percentile_disc and percentile_cont over interval keep
		// the Value (the latter interpolates in the interval domain at finalize).
		if !v.IsNull() {
			if a.plan == planPercentileCont {
				f, err := percentileInputF64(v)
				if err != nil {
					return err
				}
				a.osaFloats = append(a.osaFloats, f)
			} else {
				a.osaVals = append(a.osaVals, v)
			}
		}
	case planHypoRank, planHypoDenseRank, planHypoPercentRank, planHypoCumeDist:
		// A hypothetical-set aggregate buffers its key tuple in the fold LOOP (which has the row),
		// not through acc.fold (aggregates.md §19), so this is never reached.
		panic("a hypothetical-set accumulator buffers tuples in the fold loop")
	}
	return nil
}

// unfold removes one input value — the inverse of fold — used ONLY by the sliding-window
// optimization (window.md §5.2/§8) for the exactly-invertible COUNT / COUNT(*) (integer counters:
// add-then-remove is exact and order-independent). Every other accumulator is never un-folded — a
// moving frame over SUM/AVG/MIN/MAX/float re-folds from scratch instead (decimal scale,
// intermediate-overflow trap order, and float non-associativity make them unsafe to invert).
func (a *acc) unfold(v Value, _ *costMeter) {
	switch a.plan {
	case planCountStar:
		a.count--
	case planCount:
		if !v.IsNull() {
			a.count--
		}
	default:
		panic("only COUNT/COUNT(*) are un-folded by the sliding-window optimization")
	}
}

// finalize produces the aggregate's final value over the group. COUNT → its count (0 over
// empty); SUM/MIN/MAX → NULL over an empty/all-NULL group; AVG → sum/count (NULL if count 0).
func (a *acc) finalize() (Value, error) {
	switch a.plan {
	case planCountStar, planCount:
		return IntValue(a.count), nil
	case planSumInt:
		if a.seen {
			return IntValue(a.sumInt), nil
		}
		return NullValue(), nil
	case planSumDecimal:
		if a.seen {
			// The only cap check for the fold: the FINAL sum traps 22003 if over the §2 cap
			// (PG's make_result), but no intermediate does (decimal.md §2).
			d, err := a.sumDec.CheckCap()
			if err != nil {
				return NullValue(), err
			}
			return DecimalValue(d), nil
		}
		return NullValue(), nil
	case planAvg:
		if a.count == 0 {
			return NullValue(), nil
		}
		// Div cap-checks its (in-range) result; the over-cap-capable running sum is never
		// surfaced directly, so AVG matches PG even when SUM would overflow.
		d, err := a.sumDec.Div(decimalFromInt64(a.count))
		if err != nil {
			return NullValue(), err
		}
		return DecimalValue(d), nil
	case planSumFloat32:
		f, ok, err := a.floatSum.sumF32()
		if err != nil || !ok {
			return NullValue(), err
		}
		return Float32Value(f), nil
	case planSumFloat64:
		f, ok, err := a.floatSum.sumF64()
		if err != nil || !ok {
			return NullValue(), err
		}
		return Float64Value(f), nil
	case planAvgFloat32:
		f, ok, err := a.floatSum.avgF32()
		if err != nil || !ok {
			return NullValue(), err
		}
		return Float32Value(f), nil
	case planAvgFloat64:
		f, ok, err := a.floatSum.avgF64()
		if err != nil || !ok {
			return NullValue(), err
		}
		return Float64Value(f), nil
	case planJsonbAgg, planJsonAgg, planJsonbAggStrict, planJsonAggStrict:
		// json_agg/jsonb_agg: NULL over an empty (zero-row) group; else the JSON array. A non-empty
		// group the strict filter emptied still finalizes to `[]` (seen is true, jsonNodes empty).
		if !a.seen {
			return NullValue(), nil
		}
		arr := JsonNode{Kind: JArray, Arr: a.jsonNodes}
		// Both json_agg and jsonb_agg render the SPACED canonical form (PG joins the element texts
		// with ", "); the json variant is just typed `json` carrying that same text. (A json input
		// element is canonicalized by valueToNode — a documented divergence from PG's verbatim.)
		if a.jsonAsJSON {
			return JsonValue(jsonbOut(&arr)), nil
		}
		return JsonbValue(arr), nil
	case planJsonbObjectAgg, planJsonObjectAgg, planJsonbObjectAggUnique, planJsonObjectAggUnique:
		// json[b]_object_agg: NULL over an empty (zero-row) group; else the JSON object. json keeps
		// the group's row order + duplicate keys and PG's '{ … }' brace-padded ', ' / ' : ' spacing;
		// jsonb canonicalizes (last-wins dedup + canonical key sort) via makeObject.
		if !a.seen {
			return NullValue(), nil
		}
		if a.jsonAsJSON {
			parts := make([]string, 0, len(a.objPairs))
			for _, p := range a.objPairs {
				// The key's json-quoted form (jsonCompactOut of a JSON string node), then ` : `, then
				// the value's json image (json verbatim / jsonb canonical-spaced / else compact).
				img, err := elemJsonText(p.val)
				if err != nil {
					return NullValue(), err
				}
				keyNode := JsonNode{Kind: JString, S: p.key}
				parts = append(parts, jsonCompactOut(&keyNode)+" : "+img)
			}
			// PG's json_object_agg PADS the braces (`{ … }`) — distinct from json_build_object, which
			// does NOT pad.
			return JsonValue("{ " + strings.Join(parts, ", ") + " }"), nil
		}
		members := make([]JsonMember, 0, len(a.objPairs))
		for _, p := range a.objPairs {
			node, err := valueToNode(p.val)
			if err != nil {
				return NullValue(), err
			}
			members = append(members, JsonMember{Key: p.key, Val: node})
		}
		return JsonbValue(makeObject(members)), nil
	case planMode, planPercentileDisc, planPercentileCont, planPercentileContInterval:
		return a.finalizeOrderedSet()
	case planHypoRank, planHypoDenseRank, planHypoPercentRank, planHypoCumeDist:
		// A hypothetical-set aggregate is finalized in the group-emission loop (it needs the spec's
		// per-key sort specs), never through acc.finalize (aggregates.md §19).
		panic("a hypothetical-set accumulator is finalized in the group-emission loop")
	default: // planMin, planMax
		if a.hasCur {
			return a.cur, nil
		}
		return NullValue(), nil
	}
}

// finalizeOrderedSet computes an ordered-set aggregate's value over its collected group
// (spec/design/aggregates.md §13). mode → the most frequent value (tie → first in WITHIN GROUP sort
// order); percentile_disc → an actual value at 1-based row ceil(p·N); percentile_cont → the
// interpolated f64. The fraction range check (22003) fires here, after the NULL-fraction check and
// before the empty-group check — matching PG.
func (a *acc) finalizeOrderedSet() (Value, error) {
	desc := a.osaDesc
	switch a.plan {
	case planMode:
		vals := a.osaVals
		if len(vals) == 0 {
			return NullValue(), nil
		}
		// Sort by the WITHIN GROUP order (honoring the key's collation), then take the first value of
		// the longest run of equal values. Run equality is value-canonical (byte equality), so the
		// collation affects only which tied value comes first.
		if err := sortOsaVals(vals, a.osaCollation, desc); err != nil {
			return NullValue(), err
		}
		// The first value of the longest run of equal values — the most frequent, ties broken by
		// sort order (the first such run).
		bestIdx, bestCount, runStart := 0, 1, 0
		for i := 1; i < len(vals); i++ {
			if valueCmp(vals[i], vals[runStart]) == 0 {
				if runLen := i - runStart + 1; runLen > bestCount {
					bestCount, bestIdx = runLen, runStart
				}
			} else {
				runStart = i
			}
		}
		return vals[bestIdx], nil
	case planPercentileDisc:
		// percentile_disc: an actual sorted value at row ceil(p·N). The fraction may be a scalar or
		// an array (aggregates.md §18); finalizePercentile dispatches and applies the NULL /
		// range-check / empty rules per PG, computing each percentile over the sorted vals.
		vals := a.osaVals
		if err := sortOsaVals(vals, a.osaCollation, desc); err != nil {
			return NullValue(), err
		}
		return finalizePercentile(a.osaFrac, len(vals) == 0, func(p float64) (Value, error) {
			return percentileDiscAt(vals, p), nil
		})
	case planPercentileCont:
		fs := a.osaFloats
		sort.SliceStable(fs, func(i, j int) bool {
			return dirCmp(floatTotalCmp(fs[i], fs[j]), desc) < 0
		})
		return finalizePercentile(a.osaFrac, len(fs) == 0, func(p float64) (Value, error) {
			return Float64Value(percentileContAt(fs, p)), nil
		})
	case planPercentileContInterval:
		// percentile_cont over interval input: interpolate in the interval domain (PG interval_lerp
		// — aggregates.md §13). Values are sorted by their canonical span (interval has no collation,
		// so sortOsaVals uses the value order).
		vals := a.osaVals
		if err := sortOsaVals(vals, a.osaCollation, desc); err != nil {
			return NullValue(), err
		}
		return finalizePercentile(a.osaFrac, len(vals) == 0, func(p float64) (Value, error) {
			n := len(vals)
			pos := p * float64(n-1)
			first := int(math.Floor(pos))
			second := int(math.Ceil(pos))
			lo := expectInterval(vals[first])
			if first == second {
				return IntervalValue(lo), nil
			}
			hi := expectInterval(vals[second])
			r, err := intervalLerp(lo, hi, pos-float64(first))
			if err != nil {
				return NullValue(), err
			}
			return IntervalValue(r), nil
		})
	default:
		panic("finalizeOrderedSet called for a non-ordered-set plan")
	}
}

// finalizePercentile applies the percentile fraction (scalar or array) to a sorted group, computing
// each percentile via compute (spec/design/aggregates.md §13/§18). PG's check order is preserved: a
// scalar None/NULL fraction → NULL; otherwise the range check (22003) fires per fraction BEFORE the
// empty-group check; an empty/all-NULL group → NULL (the whole result, even for an array). For an
// array fraction the result is an array with one percentile per element (a NULL element → a NULL
// element), after every non-NULL element has passed the range check.
func finalizePercentile(frac *Value, empty bool, compute func(p float64) (Value, error)) (Value, error) {
	if frac == nil || frac.IsNull() {
		return NullValue(), nil
	}
	if frac.Kind == ValArray {
		// Range-check every non-NULL element FIRST (before the empty-group check, PG).
		fracs := make([]*float64, 0, len(frac.arrayVal().Elements))
		for i := range frac.arrayVal().Elements {
			el := frac.arrayVal().Elements[i]
			pf, err := fractionToF64(&el)
			if err != nil {
				return NullValue(), err
			}
			if pf != nil {
				if err := checkPercentileFraction(*pf); err != nil {
					return NullValue(), err
				}
			}
			fracs = append(fracs, pf)
		}
		if empty {
			return NullValue(), nil // an empty/all-NULL group → NULL (not an array of NULLs), PG
		}
		out := make([]Value, 0, len(fracs))
		for _, pf := range fracs {
			if pf == nil {
				out = append(out, NullValue())
				continue
			}
			v, err := compute(*pf)
			if err != nil {
				return NullValue(), err
			}
			out = append(out, v)
		}
		return arrayValueOf(&ArrayVal{Dims: []int{len(out)}, Lbounds: []int32{1}, Elements: out}), nil
	}
	p, err := fractionToF64(frac)
	if err != nil {
		return NullValue(), err
	}
	if p == nil {
		return NullValue(), nil
	}
	if err := checkPercentileFraction(*p); err != nil {
		return NullValue(), err
	}
	if empty {
		return NullValue(), nil
	}
	return compute(*p)
}

// expectInterval returns the Interval of a buffered ValInterval (a planPercentileContInterval group
// only ever buffers intervals — the resolver gates the operand to interval).
func expectInterval(v Value) Interval {
	if v.Kind != ValInterval {
		panic("percentile_cont(interval) buffered a non-interval")
	}
	return v.interval()
}

// intervalLerp(lo, hi, pct) = lo + (hi - lo)·pct, PG's orderedsetaggs.c interval interpolation
// (spec/design/aggregates.md §13). intervalMul below replicates PG's exact field-cascade + rounding
// so the result is byte-identical to PostgreSQL.
func intervalLerp(lo, hi Interval, pct float64) (Interval, error) {
	diff, err := hi.Sub(lo)
	if err != nil {
		return Interval{}, err
	}
	scaled, err := intervalMul(diff, pct)
	if err != nil {
		return Interval{}, err
	}
	return scaled.Add(lo)
}

// intervalMul is interval * f64, byte-identical to PostgreSQL's interval_mul (timestamp.c): multiply
// each field by the factor, then cascade the fractional month/day parts down to days/micros with
// PG's TSROUND (round to microsecond precision) and the 30 days/month, 86400 s/day conversions. The
// operand is finite (no infinite intervals here) and the factor is a finite fraction in [0,1].
func intervalMul(span Interval, factor float64) (Interval, error) {
	const (
		daysPerMonthF = 30.0
		secsPerDayF   = 86400.0
		usecsPerSecF  = 1_000_000.0
	)
	// TSROUND: round to microsecond precision (PG TS_PREC_INV = 1e6). PG rint = ties-to-EVEN.
	tsround := func(j float64) float64 { return math.RoundToEven(j*usecsPerSecF) / usecsPerSecF }
	oor := func() error { return newError(DatetimeFieldOverflow, "interval out of range") }
	// FLOAT8_FITS_IN_INT32/64: x in [INT_MIN, -INT_MIN) — matches Rust's fits_i32/fits_i64.
	fitsI32 := func(x float64) bool { return x >= float64(math.MinInt32) && x < -float64(math.MinInt32) }
	fitsI64 := func(x float64) bool { return x >= float64(math.MinInt64) && x < -float64(math.MinInt64) }

	origMonth := span.Months
	origDay := span.Days

	resultDouble := float64(span.Months) * factor
	if math.IsNaN(resultDouble) || !fitsI32(resultDouble) {
		return Interval{}, oor()
	}
	resultMonth := int32(resultDouble)

	resultDouble = float64(span.Days) * factor
	if math.IsNaN(resultDouble) || !fitsI32(resultDouble) {
		return Interval{}, oor()
	}
	resultDay := int32(resultDouble)

	// Cascade fractional months → days, fractional days → micros (PG's exact sequence).
	monthRemainderDays := tsround((float64(origMonth)*factor - float64(resultMonth)) * daysPerMonthF)
	secRemainder := tsround(
		(float64(origDay)*factor - float64(resultDay) + monthRemainderDays -
			float64(int64(monthRemainderDays))) * secsPerDayF,
	)
	// Might exceed a day from rounding / cascade — push whole days up.
	if math.Abs(secRemainder) >= secsPerDayF {
		add := int32(secRemainder / secsPerDayF)
		nd, ok := addI32(resultDay, add)
		if !ok {
			return Interval{}, oor()
		}
		resultDay = nd
		secRemainder -= float64(int64(secRemainder/secsPerDayF)) * secsPerDayF
	}
	nd, ok := addI32(resultDay, int32(monthRemainderDays))
	if !ok {
		return Interval{}, oor()
	}
	resultDay = nd
	resultDouble = math.RoundToEven(float64(span.Micros)*factor + secRemainder*usecsPerSecF)
	if math.IsNaN(resultDouble) || !fitsI64(resultDouble) {
		return Interval{}, oor()
	}
	return Interval{Months: resultMonth, Days: resultDay, Micros: int64(resultDouble)}, nil
}

// fractionToF64 converts an evaluated percentile fraction (the direct argument, evaluated per
// group) to f64 (aggregates.md §13/§17). nil / a NULL value → nil (a NULL fraction yields NULL). A
// numeric value (the resolver restricts the fraction to a numeric family) widens via the IEEE /
// correctly-rounded decimal cast. The range check (22003) is applied by the caller after this.
func fractionToF64(frac *Value) (*float64, error) {
	if frac == nil || frac.IsNull() {
		return nil, nil
	}
	switch frac.Kind {
	case ValFloat64:
		f := frac.F64()
		return &f, nil
	case ValFloat32:
		f := float64(frac.F32())
		return &f, nil
	case ValInt:
		f := float64(frac.Int)
		return &f, nil
	case ValDecimal:
		f, err := decimalToFloat64(*frac.decimal())
		if err != nil {
			return nil, err
		}
		return &f, nil
	default:
		panic("a non-numeric percentile fraction is rejected at resolve")
	}
}

// percentileDiscAt computes percentile_disc over the already-sorted group values: the value at row
// ceil(p·N) (1-based), i.e. the smallest K with K/N ≥ p (PG orderedsetaggs.c). Caller guarantees
// non-empty + the fraction in range. spec/design/aggregates.md §13.
func percentileDiscAt(vals []Value, p float64) Value {
	n := len(vals)
	// PG: rownum = ceil(p·N) (1-based), then the value at max(rownum, 1).
	rownum := int(math.Ceil(p * float64(n)))
	idx := 0
	if rownum >= 1 {
		idx = rownum - 1
	}
	if idx > n-1 {
		idx = n - 1
	}
	return vals[idx]
}

// percentileContAt computes percentile_cont over the already-sorted f64 group values: interpolate
// between the two bracketing rows, in f64 with PG's exact operation order — bit-identical across
// cores and to PG (spec/design/aggregates.md §13). Caller guarantees non-empty + the fraction in
// range.
func percentileContAt(floats []float64, p float64) float64 {
	n := len(floats)
	pos := p * float64(n-1)
	first := int(math.Floor(pos))
	second := int(math.Ceil(pos))
	if first == second {
		return floats[first]
	}
	lo, hi := floats[first], floats[second]
	proportion := pos - float64(first)
	return lo + (proportion * (hi - lo))
}

// dirCmp applies a WITHIN GROUP sort direction to a comparison result (DESC reverses).
func dirCmp(c int, desc bool) int {
	if desc {
		return -c
	}
	return c
}

// sortOsaVals sorts an ordered-set aggregate's buffered values by its WITHIN GROUP order
// (aggregates.md §13). With no collation, the value-canonical comparison (the same total order
// ORDER BY/MIN/MAX use). With a collation, a stable decorate-sort by the precomputed collation sort
// key bytes (a collated key is always text; an unmapped code point fails 0A000 at this deterministic
// point, like the query ORDER BY). The stable sort keeps collation-equal values in scan order, so
// the result is deterministic and cross-core identical.
func sortOsaVals(vals []Value, collation *Collation, desc bool) error {
	if collation == nil {
		sort.SliceStable(vals, func(i, j int) bool {
			return dirCmp(valueCmp(vals[i], vals[j]), desc) < 0
		})
		return nil
	}
	// Decorate each value with its collation sort key (text only), sort stably by the key bytes, then
	// undecorate. The keys are built once up front so a SortKey failure (0A000 for an unmapped code
	// point) surfaces at this deterministic point rather than inside the comparator.
	type deco struct {
		key []byte
		val Value
	}
	d := make([]deco, len(vals))
	for i, v := range vals {
		if v.Kind != ValText {
			panic("a collated WITHIN GROUP key buffers only text")
		}
		sk, err := sortKey(collation, v.str())
		if err != nil {
			return err
		}
		d[i] = deco{key: sk, val: v}
	}
	sort.SliceStable(d, func(i, j int) bool {
		return dirCmp(bytes.Compare(d[i].key, d[j].key), desc) < 0
	})
	for i := range d {
		vals[i] = d[i].val
	}
	return nil
}

// finalizeHypothetical computes a hypothetical-set aggregate's value (aggregates.md §19): given the
// buffered group key tuples rows, the per-group hypothetical row hyp, and the WITHIN GROUP per-key
// sort specs, count where hyp would rank. rank = 1 + rows strictly before hyp; dense_rank = 1 +
// distinct values strictly before; percent_rank = (rank-1)/N; cume_dist = (#rows ≤ hyp + 1)/(N+1) —
// PG's orderedsetaggs.c formulas exactly. Over an empty group: rank/dense_rank 1, percent_rank 0,
// cume_dist 1.
func finalizeHypothetical(plan aggPlan, rows [][]Value, hyp []Value, sorts []keySort) (Value, error) {
	n := len(rows)
	if n == 0 {
		switch plan {
		case planHypoRank, planHypoDenseRank:
			return IntValue(1), nil
		case planHypoPercentRank:
			return Float64Value(0.0), nil
		case planHypoCumeDist:
			return Float64Value(1.0), nil
		default:
			panic("finalizeHypothetical only for the hypothetical-set plans")
		}
	}
	var strictlyBefore int64
	var le int64 // rows that sort ≤ hyp (for cume_dist's rank with flag +1)
	// The distinct strictly-before key tuples (for dense_rank), value-canonical (the group-key
	// distinctRowKey, the same form the GROUP BY bucketing uses — collapses 1.5/1.50, NULL with NULL).
	distinct := make(map[string]bool)
	for _, r := range rows {
		ord, err := hypoCmp(r, hyp, sorts)
		if err != nil {
			return NullValue(), err
		}
		switch {
		case ord < 0:
			strictlyBefore++
			le++
			distinct[distinctRowKey(r)] = true
		case ord == 0:
			le++
		}
	}
	switch plan {
	case planHypoRank:
		return IntValue(strictlyBefore + 1), nil
	case planHypoDenseRank:
		return IntValue(int64(len(distinct)) + 1), nil
	case planHypoPercentRank:
		return Float64Value(float64(strictlyBefore) / float64(n)), nil
	case planHypoCumeDist:
		return Float64Value(float64(le+1) / float64(n+1)), nil
	default:
		panic("finalizeHypothetical only for the hypothetical-set plans")
	}
}

// hypoCmp compares a buffered key tuple a to the hypothetical row b by the WITHIN GROUP order
// (aggregates.md §19): the first key whose comparison is non-equal decides. Each key honors its NULL
// placement, direction, and collation (a collated text key can fail 0A000).
func hypoCmp(a, b []Value, sorts []keySort) (int, error) {
	for i, ks := range sorts {
		ord, err := compareHypoKey(a[i], b[i], ks)
		if err != nil {
			return 0, err
		}
		if ord != 0 {
			return ord, nil
		}
	}
	return 0, nil
}

// compareHypoKey compares one WITHIN GROUP key pair under its sort spec (NULL placement + direction +
// collation), mirroring the query ORDER BY key comparison plus the collated-text path (aggregates.md
// §19).
func compareHypoKey(a, b Value, ks keySort) (int, error) {
	switch {
	case a.IsNull() && b.IsNull():
		return 0, nil
	case a.IsNull():
		if ks.nullsFirst {
			return -1, nil
		}
		return 1, nil
	case b.IsNull():
		if ks.nullsFirst {
			return 1, nil
		}
		return -1, nil
	default:
		var base int
		if ks.collation != nil && a.Kind == ValText && b.Kind == ValText {
			c, err := collatedCmp(ks.collation, a.str(), b.str())
			if err != nil {
				return 0, err
			}
			base = c
		} else {
			base = valueCmp(a, b)
		}
		return dirCmp(base, ks.desc), nil
	}
}

// checkPercentileFraction is the percentile fraction range gate (aggregates.md §13): < 0, > 1, or
// NaN is 22003 (numeric_value_out_of_range), matching PG's "percentile value … is not between 0 and
// 1". Called per group at finalize, after the NULL-fraction check.
func checkPercentileFraction(p float64) error {
	if math.IsNaN(p) || p < 0 || p > 1 {
		return newError(NumericValueOutOfRange, fmt.Sprintf("percentile value %v is not between 0 and 1", p))
	}
	return nil
}

// percentileInputF64 widens a numeric value to f64 for percentile_cont (aggregates.md §13):
// integers via the IEEE cast, decimals via the correctly-rounded decimal→f64 cast (matching PG's
// numeric→float8), floats unchanged. The resolver restricts the operand to a numeric family.
func percentileInputF64(v Value) (float64, error) {
	switch v.Kind {
	case ValInt:
		return float64(v.Int), nil
	case ValFloat32, ValFloat64:
		return v.asF64(), nil
	case ValDecimal:
		return decimalToFloat64(*v.decimal())
	default:
		panic("resolver restricts percentile_cont to a numeric operand")
	}
}

// itemsHaveAggregate reports whether any select item contains an aggregate call.
func itemsHaveAggregate(items selectItems) bool {
	if items.All {
		return false
	}
	for _, it := range items.Items {
		if exprHasAggregate(it.Expr) {
			return true
		}
	}
	return false
}

// windowDefHasAggregate reports whether a window definition's PARTITION BY / ORDER BY keys contain
// an aggregate (`OVER (ORDER BY sum(x))` — spec/design/window.md §5.1). Such an aggregate makes the
// query an aggregate query (a whole-table aggregate if there is no GROUP BY), exactly as a top-level
// aggregate would, so the window keys resolve against the grouped row. Used by both the inline-over
// walk in exprHasAggregate and the WINDOW-clause scan that computes isAgg.
func windowDefHasAggregate(wd *windowDef) bool {
	for _, p := range wd.Partition {
		if exprHasAggregate(p) {
			return true
		}
	}
	for _, k := range wd.Order {
		if exprHasAggregate(k.Expr) {
			return true
		}
	}
	return false
}

// windowsHaveAggregate reports whether any WINDOW-clause entry's keys contain an aggregate (`WINDOW w
// AS (ORDER BY sum(x))`), which — like a top-level aggregate — makes the query an aggregate query
// (spec/design/window.md §5.1). The entries are still named references at this point (the OVER-name
// desugar runs later), so the WINDOW clause is scanned directly.
func windowsHaveAggregate(windows []namedWindow) bool {
	for i := range windows {
		if windowDefHasAggregate(&windows[i].Def) {
			return true
		}
	}
	return false
}

// isAggregateName reports whether name (case-insensitive) is a registered aggregate surface
// (COUNT/SUM/MIN/MAX/AVG). Data-driven over the catalog (Aggregates); consulted by the grouping
// + CHECK-structure walks.
func isAggregateName(name string) bool {
	lname := toLowerASCII(name)
	for i := range aggregates {
		if toLowerASCII(aggregates[i].Surface) == lname {
			return true
		}
	}
	if _, ok := objectAggClassify(name); ok {
		return true
	}
	// The ordered-set aggregates are aggregates for these purposes too (they fold a set of rows)
	// but are not catalog rows (their result/arg mold is special, like GROUPING()).
	return isOrderedSetAggregateName(name)
}

// isOrderedSetAggregateName reports whether name is an ordered-set aggregate surface (mode /
// percentile_cont / percentile_disc — spec/design/aggregates.md §13). These take a WITHIN GROUP
// (ORDER BY …) clause and are resolved by resolveOrderedSetAggregate, intercepted before the generic
// aggregate/scalar dispatch.
func isOrderedSetAggregateName(name string) bool {
	switch toLowerASCII(name) {
	case "mode", "percentile_cont", "percentile_disc":
		return true
	}
	return false
}

// isHypotheticalSetName reports whether name is a hypothetical-set aggregate surface — rank /
// dense_rank / percent_rank / cume_dist used with WITHIN GROUP (spec/design/aggregates.md §19).
// These names are ALSO window functions; the WITHIN GROUP clause routes them here instead of the
// window path.
func isHypotheticalSetName(name string) bool {
	switch toLowerASCII(name) {
	case "rank", "dense_rank", "percent_rank", "cume_dist":
		return true
	}
	return false
}

// objectAggClassify classifies a json[b]_object_agg[_unique] name → (plan, ok). These 2-argument
// aggregates are hand-resolved (the single-operand aggregate catalog can't express a key/value
// pair), like jsonb_set among the scalar functions (json-sql-functions.md §4).
func objectAggClassify(name string) (aggPlan, bool) {
	switch toLowerASCII(name) {
	case "jsonb_object_agg":
		return planJsonbObjectAgg, true
	case "json_object_agg":
		return planJsonObjectAgg, true
	case "jsonb_object_agg_unique":
		return planJsonbObjectAggUnique, true
	case "json_object_agg_unique":
		return planJsonObjectAggUnique, true
	default:
		return 0, false
	}
}

// isWindowOnlyName reports whether name is a registered WINDOW-only function surface
// (row_number/…). Data-driven over the catalog (Windows). Such a function REQUIRES an OVER clause —
// used without one it is 42809 (spec/design/window.md §7). The catalog aggregates double as window
// functions but are not in Windows, so they are still valid without OVER.
func isWindowOnlyName(name string) bool {
	lname := toLowerASCII(name)
	for i := range windows {
		if toLowerASCII(windows[i].Surface) == lname {
			return true
		}
	}
	return false
}

// subscriptSpecExprs returns the sub-expressions of one AST subscript spec (an index, or a slice's
// present bounds) — for the Expr tree walkers (spec/design/array.md §6).
func subscriptSpecExprs(s subscriptSpec) []*exprNode {
	if !s.IsSlice {
		return []*exprNode{s.Index}
	}
	var out []*exprNode
	if s.Lower != nil {
		out = append(out, s.Lower)
	}
	if s.Upper != nil {
		out = append(out, s.Upper)
	}
	return out
}

// exprHasAggregate reports whether an expression tree contains an AGGREGATE call anywhere. A
// scalar-function call is not itself an aggregate but may CONTAIN one (abs(sum(x))), so its
// arguments are walked.
func exprHasAggregate(e exprNode) bool {
	switch e.Kind {
	case exprFuncCall:
		// An aggregate name carrying OVER (inline or a named-window reference) is a WINDOW
		// function, not a bare aggregate (so a `sum(x) OVER ()` / `sum(x) OVER w` query is a window
		// query, not an aggregate query). Mirrors Rust: (over.is_none() && over_name.is_none() &&
		// is_aggregate_name(name)) || any arg has an aggregate. (Detection runs before the
		// OVER-name desugar.)
		if e.FuncCall.Over == nil && e.FuncCall.OverName == "" && isAggregateName(e.FuncCall.Name) {
			return true
		}
		// A hypothetical-set name with a WITHIN GROUP clause (`rank(x) WITHIN GROUP (…)`) is an
		// aggregate (aggregates.md §19), so the query is an aggregate query. Mirrors Rust's
		// (within_group.is_some() && is_hypothetical_set_name(name)).
		if e.FuncCall.WithinGroup != nil && isHypotheticalSetName(e.FuncCall.Name) {
			return true
		}
		for _, a := range e.FuncCall.Args {
			if exprHasAggregate(*a) {
				return true
			}
		}
		// An aggregate INSIDE the inline window definition's keys (`rank() OVER (ORDER BY sum(x))`)
		// also makes the query an aggregate query — those keys resolve against the grouped row (§5.1).
		if e.FuncCall.Over != nil && windowDefHasAggregate(e.FuncCall.Over) {
			return true
		}
		return false
	case exprCast:
		return exprHasAggregate(e.Cast.Inner)
	case exprExtract:
		return exprHasAggregate(e.Extract.Source)
	case exprCollate:
		return exprHasAggregate(e.Collate.Inner)
	case exprUnary:
		return exprHasAggregate(e.Unary.Operand)
	case exprIsNull:
		return exprHasAggregate(e.IsNullOf.Operand)
	case exprIsJson:
		return exprHasAggregate(e.IsJsonOf.Operand)
	case exprJsonCtor:
		return exprHasAggregate(e.JsonCtorOf.Operand)
	case exprJsonExists:
		return exprHasAggregate(e.JsonExists.Ctx) || exprHasAggregate(e.JsonExists.Path)
	case exprJsonValue:
		return exprHasAggregate(e.JsonValue.Ctx) || exprHasAggregate(e.JsonValue.Path)
	case exprJsonQuery:
		return exprHasAggregate(e.JsonQuery.Ctx) || exprHasAggregate(e.JsonQuery.Path)
	case exprBinary:
		return exprHasAggregate(e.Binary.Lhs) || exprHasAggregate(e.Binary.Rhs)
	case exprIsDistinct:
		return exprHasAggregate(e.IsDistinct.Lhs) || exprHasAggregate(e.IsDistinct.Rhs)
	case exprIn:
		if exprHasAggregate(e.In.Lhs) {
			return true
		}
		for _, elem := range e.In.List {
			if exprHasAggregate(elem) {
				return true
			}
		}
		return false
	case exprBetween:
		return exprHasAggregate(e.Between.Lhs) || exprHasAggregate(e.Between.Lo) || exprHasAggregate(e.Between.Hi)
	case exprLike:
		return exprHasAggregate(e.Like.Lhs) || exprHasAggregate(e.Like.Rhs)
	case exprRegex:
		return exprHasAggregate(e.Regex.Lhs) || exprHasAggregate(e.Regex.Rhs)
	case exprCase:
		if e.Case.Operand != nil && exprHasAggregate(*e.Case.Operand) {
			return true
		}
		for _, w := range e.Case.Whens {
			if exprHasAggregate(w.Cond) || exprHasAggregate(w.Result) {
				return true
			}
		}
		return e.Case.Els != nil && exprHasAggregate(*e.Case.Els)
	case exprFieldAccess, exprFieldStar:
		// Field selection `(expr).field` / `(expr).*` recurses into the composite base
		// (spec/design/composite.md §S4) — an aggregate hidden in the base must surface.
		return exprHasAggregate(*e.Base)
	case exprQualifiedStar:
		return false // `t.*` is a leaf relation reference — no aggregate
	case exprSubscript:
		// `base[..]` — an aggregate hidden in the base array or any subscript bound must surface.
		if exprHasAggregate(*e.Base) {
			return true
		}
		for _, s := range e.Subscripts {
			for _, x := range subscriptSpecExprs(s) {
				if exprHasAggregate(*x) {
					return true
				}
			}
		}
		return false
	case exprRow, exprArray:
		// A ROW(...) / ARRAY[...] constructor recurses into its element expressions.
		for _, it := range e.RowItems {
			if exprHasAggregate(it) {
				return true
			}
		}
		return false
	case exprQuantified:
		return exprHasAggregate(e.Quantified.Lhs) || exprHasAggregate(e.Quantified.Array)
	default:
		return false
	}
}

// itemsHaveWindow reports whether any select item contains a window-function call (a FuncCall
// carrying OVER). A window query resolves its projection in window mode (spec/design/window.md §5.1).
func itemsHaveWindow(items selectItems) bool {
	if items.All {
		return false
	}
	for _, it := range items.Items {
		if exprHasWindow(it.Expr) {
			return true
		}
	}
	return false
}

// orderByHasWindow reports whether any ORDER BY key is (or contains) a window function, so a query
// whose only OVER call sits in the ORDER BY still sets up the window machinery (grammar.md §10,
// window.md §5.1). An ordinal/column key carries no expression.
func orderByHasWindow(keys []orderKey) bool {
	for _, k := range keys {
		if k.Expr != nil && exprHasWindow(*k.Expr) {
			return true
		}
	}
	return false
}

// exprHasWindow reports whether an expression tree contains a window-function call anywhere (a
// FuncCall whose Over is set). An ordinary call may CONTAIN one in its arguments
// (abs(row_number() OVER ())), so the arguments are walked; a window call's own PARTITION BY /
// ORDER BY may not contain a window function (rejected at resolve, 42P20), so they are not walked
// here. A subquery is an independent query: a window function inside it is the subquery's own.
func exprHasWindow(e exprNode) bool {
	switch e.Kind {
	case exprFuncCall:
		if e.FuncCall.Over != nil || e.FuncCall.OverName != "" {
			return true
		}
		for _, a := range e.FuncCall.Args {
			if exprHasWindow(*a) {
				return true
			}
		}
		return false
	case exprCast:
		return exprHasWindow(e.Cast.Inner)
	case exprExtract:
		return exprHasWindow(e.Extract.Source)
	case exprCollate:
		return exprHasWindow(e.Collate.Inner)
	case exprUnary:
		return exprHasWindow(e.Unary.Operand)
	case exprIsNull:
		return exprHasWindow(e.IsNullOf.Operand)
	case exprIsJson:
		return exprHasWindow(e.IsJsonOf.Operand)
	case exprJsonCtor:
		return exprHasWindow(e.JsonCtorOf.Operand)
	case exprJsonExists:
		return exprHasWindow(e.JsonExists.Ctx) || exprHasWindow(e.JsonExists.Path)
	case exprJsonValue:
		return exprHasWindow(e.JsonValue.Ctx) || exprHasWindow(e.JsonValue.Path)
	case exprJsonQuery:
		return exprHasWindow(e.JsonQuery.Ctx) || exprHasWindow(e.JsonQuery.Path)
	case exprBinary:
		return exprHasWindow(e.Binary.Lhs) || exprHasWindow(e.Binary.Rhs)
	case exprIsDistinct:
		return exprHasWindow(e.IsDistinct.Lhs) || exprHasWindow(e.IsDistinct.Rhs)
	case exprIn:
		if exprHasWindow(e.In.Lhs) {
			return true
		}
		for _, elem := range e.In.List {
			if exprHasWindow(elem) {
				return true
			}
		}
		return false
	case exprBetween:
		return exprHasWindow(e.Between.Lhs) || exprHasWindow(e.Between.Lo) || exprHasWindow(e.Between.Hi)
	case exprLike:
		return exprHasWindow(e.Like.Lhs) || exprHasWindow(e.Like.Rhs)
	case exprRegex:
		return exprHasWindow(e.Regex.Lhs) || exprHasWindow(e.Regex.Rhs)
	case exprCase:
		if e.Case.Operand != nil && exprHasWindow(*e.Case.Operand) {
			return true
		}
		for _, w := range e.Case.Whens {
			if exprHasWindow(w.Cond) || exprHasWindow(w.Result) {
				return true
			}
		}
		return e.Case.Els != nil && exprHasWindow(*e.Case.Els)
	case exprFieldAccess, exprFieldStar:
		return exprHasWindow(*e.Base)
	case exprQualifiedStar:
		return false // `t.*` is a leaf relation reference — no window function

	case exprSubscript:
		if exprHasWindow(*e.Base) {
			return true
		}
		for _, s := range e.Subscripts {
			for _, x := range subscriptSpecExprs(s) {
				if exprHasWindow(*x) {
					return true
				}
			}
		}
		return false
	case exprRow, exprArray:
		for _, it := range e.RowItems {
			if exprHasWindow(it) {
				return true
			}
		}
		return false
	case exprQuantified:
		return exprHasWindow(e.Quantified.Lhs) || exprHasWindow(e.Quantified.Array)
	default:
		return false
	}
}

// extendWindow applies the base-window merge rules (spec/design/window.md §5, PostgreSQL
// transformWindowDefinitions): a definition that names a base copies the base's PARTITION BY and —
// if the base has one — its ORDER BY, and supplies its own frame. The extender may not add a
// PARTITION BY (42P20, even when the base has none), may add an ORDER BY only when the base has
// none (42P20 otherwise), and the base must not carry a frame (42P20). The three checks fire in
// PostgreSQL's priority order: PARTITION, then ORDER, then frame. Returns the merged inline
// definition (Base == "").
func extendWindow(base, ext windowDef, baseName string) (windowDef, error) {
	if len(ext.Partition) > 0 {
		return windowDef{}, newError(WindowingError, fmt.Sprintf("cannot override PARTITION BY clause of window %q", baseName))
	}
	if len(base.Order) > 0 && len(ext.Order) > 0 {
		return windowDef{}, newError(WindowingError, fmt.Sprintf("cannot override ORDER BY clause of window %q", baseName))
	}
	if base.Frame != nil {
		return windowDef{}, newError(WindowingError, fmt.Sprintf("cannot copy window %q because it has a frame clause", baseName))
	}
	order := ext.Order
	if len(base.Order) > 0 {
		order = base.Order
	}
	return windowDef{Base: "", Partition: base.Partition, Order: order, Frame: ext.Frame}, nil
}

// resolveWindowClause resolves a WINDOW clause into all-inline definitions (spec/design/window.md
// §5). Entries are processed left-to-right; an entry naming a base extends an already-resolved
// earlier entry (a self- or forward-reference is therefore "does not exist" — 42704), via
// extendWindow. Every entry is resolved — even ones no OVER references — matching PostgreSQL's
// whole-clause check.
func resolveWindowClause(windows []namedWindow) ([]namedWindow, error) {
	resolved := make([]namedWindow, 0, len(windows))
	for _, nw := range windows {
		r := nw.Def
		if nw.Def.Base != "" {
			base, err := lookupWindow(resolved, nw.Def.Base)
			if err != nil {
				return nil, err
			}
			r, err = extendWindow(base, nw.Def, nw.Def.Base)
			if err != nil {
				return nil, err
			}
		}
		resolved = append(resolved, namedWindow{Name: nw.Name, Def: r})
	}
	return resolved, nil
}

// lookupWindow finds a (resolved, Base == "") window definition by name in windows,
// case-insensitively, or raises 42704 `window "<name>" does not exist`.
func lookupWindow(windows []namedWindow, name string) (windowDef, error) {
	for i := range windows {
		if strings.EqualFold(windows[i].Name, name) {
			return windows[i].Def, nil
		}
	}
	return windowDef{}, newError(UndefinedObject, fmt.Sprintf("window %q does not exist", name))
}

// desugarItems desugars `OVER name` / `OVER (base …)` references in a select list to their
// WINDOW-clause definitions before resolution (spec/design/window.md §5): a pure OverName reference
// gets the named definition copied into Over, an inline Over with a Base is merged onto the named
// base (extendWindow); an undefined name is 42704. After this every window call carries an inline
// Over (Base == ""), so resolution (S0–S4) handles named and inline windows uniformly. Returns a
// fresh SelectItems (the original AST is not mutated — the FuncCall pointers along each rewritten
// path are freshly allocated).
func desugarItems(items selectItems, windows []namedWindow) (selectItems, error) {
	if items.All {
		return items, nil
	}
	out := selectItems{Items: make([]selectItem, len(items.Items))}
	for i, it := range items.Items {
		e, err := desugarNamedWindows(it.Expr, windows)
		if err != nil {
			return selectItems{}, err
		}
		out.Items[i] = selectItem{Expr: e, Alias: it.Alias}
	}
	return out, nil
}

// desugarNamedWindows recursively rewrites every `OVER name` (OverName set) in e to its definition
// from windows (copied into Over), erroring 42704 if the name is absent. Mirrors Rust's
// desugar_named_windows: the FuncCall arm rewrites the reference and recurses into the arguments;
// the other arms recurse into their sub-expressions; leaves, subscripts, and subqueries (independent)
// carry no top-level window ref to rewrite. The walk returns a fresh Expr so the original AST stays
// unmutated.
func desugarNamedWindows(e exprNode, windows []namedWindow) (exprNode, error) {
	switch e.Kind {
	case exprFuncCall:
		fc := *e.FuncCall // shallow copy; we replace Args/Over/OverName below
		if fc.OverName != "" {
			// `OVER name` — a pure reference: copy the named definition whole, frame included (no
			// merge rules; copying a framed window is forbidden only for the extend form below — §5).
			def, err := lookupWindow(windows, fc.OverName)
			if err != nil {
				return exprNode{}, err
			}
			fc.Over = &def
			fc.OverName = ""
		} else if fc.Over != nil && fc.Over.Base != "" {
			// `OVER (base …)` — an extend: merge the inline definition onto the named base.
			base, err := lookupWindow(windows, fc.Over.Base)
			if err != nil {
				return exprNode{}, err
			}
			merged, err := extendWindow(base, *fc.Over, fc.Over.Base)
			if err != nil {
				return exprNode{}, err
			}
			fc.Over = &merged
		}
		if len(fc.Args) > 0 {
			args := make([]*exprNode, len(fc.Args))
			for i, a := range fc.Args {
				na, err := desugarNamedWindows(*a, windows)
				if err != nil {
					return exprNode{}, err
				}
				args[i] = &na
			}
			fc.Args = args
		}
		ne := e
		ne.FuncCall = &fc
		return ne, nil
	case exprCast:
		inner, err := desugarNamedWindows(e.Cast.Inner, windows)
		if err != nil {
			return exprNode{}, err
		}
		nc := *e.Cast
		nc.Inner = inner
		ne := e
		ne.Cast = &nc
		return ne, nil
	case exprExtract:
		src, err := desugarNamedWindows(e.Extract.Source, windows)
		if err != nil {
			return exprNode{}, err
		}
		nx := *e.Extract
		nx.Source = src
		ne := e
		ne.Extract = &nx
		return ne, nil
	case exprCollate:
		inner, err := desugarNamedWindows(e.Collate.Inner, windows)
		if err != nil {
			return exprNode{}, err
		}
		nc := *e.Collate
		nc.Inner = inner
		ne := e
		ne.Collate = &nc
		return ne, nil
	case exprUnary:
		op, err := desugarNamedWindows(e.Unary.Operand, windows)
		if err != nil {
			return exprNode{}, err
		}
		nu := *e.Unary
		nu.Operand = op
		ne := e
		ne.Unary = &nu
		return ne, nil
	case exprIsNull:
		op, err := desugarNamedWindows(e.IsNullOf.Operand, windows)
		if err != nil {
			return exprNode{}, err
		}
		ni := *e.IsNullOf
		ni.Operand = op
		ne := e
		ne.IsNullOf = &ni
		return ne, nil
	case exprIsJson:
		op, err := desugarNamedWindows(e.IsJsonOf.Operand, windows)
		if err != nil {
			return exprNode{}, err
		}
		ni := *e.IsJsonOf
		ni.Operand = op
		ne := e
		ne.IsJsonOf = &ni
		return ne, nil
	case exprJsonCtor:
		op, err := desugarNamedWindows(e.JsonCtorOf.Operand, windows)
		if err != nil {
			return exprNode{}, err
		}
		ni := *e.JsonCtorOf
		ni.Operand = op
		ne := e
		ne.JsonCtorOf = &ni
		return ne, nil
	case exprJsonExists:
		ctx, err := desugarNamedWindows(e.JsonExists.Ctx, windows)
		if err != nil {
			return exprNode{}, err
		}
		path, err := desugarNamedWindows(e.JsonExists.Path, windows)
		if err != nil {
			return exprNode{}, err
		}
		nj := *e.JsonExists
		nj.Ctx = ctx
		nj.Path = path
		ne := e
		ne.JsonExists = &nj
		return ne, nil
	case exprJsonValue:
		ctx, err := desugarNamedWindows(e.JsonValue.Ctx, windows)
		if err != nil {
			return exprNode{}, err
		}
		path, err := desugarNamedWindows(e.JsonValue.Path, windows)
		if err != nil {
			return exprNode{}, err
		}
		nj := *e.JsonValue
		nj.Ctx = ctx
		nj.Path = path
		ne := e
		ne.JsonValue = &nj
		return ne, nil
	case exprJsonQuery:
		ctx, err := desugarNamedWindows(e.JsonQuery.Ctx, windows)
		if err != nil {
			return exprNode{}, err
		}
		path, err := desugarNamedWindows(e.JsonQuery.Path, windows)
		if err != nil {
			return exprNode{}, err
		}
		nj := *e.JsonQuery
		nj.Ctx = ctx
		nj.Path = path
		ne := e
		ne.JsonQuery = &nj
		return ne, nil
	case exprBinary:
		lhs, err := desugarNamedWindows(e.Binary.Lhs, windows)
		if err != nil {
			return exprNode{}, err
		}
		rhs, err := desugarNamedWindows(e.Binary.Rhs, windows)
		if err != nil {
			return exprNode{}, err
		}
		nb := *e.Binary
		nb.Lhs = lhs
		nb.Rhs = rhs
		ne := e
		ne.Binary = &nb
		return ne, nil
	case exprIsDistinct:
		lhs, err := desugarNamedWindows(e.IsDistinct.Lhs, windows)
		if err != nil {
			return exprNode{}, err
		}
		rhs, err := desugarNamedWindows(e.IsDistinct.Rhs, windows)
		if err != nil {
			return exprNode{}, err
		}
		nd := *e.IsDistinct
		nd.Lhs = lhs
		nd.Rhs = rhs
		ne := e
		ne.IsDistinct = &nd
		return ne, nil
	case exprIn:
		lhs, err := desugarNamedWindows(e.In.Lhs, windows)
		if err != nil {
			return exprNode{}, err
		}
		list := make([]exprNode, len(e.In.List))
		for i, x := range e.In.List {
			nx, err := desugarNamedWindows(x, windows)
			if err != nil {
				return exprNode{}, err
			}
			list[i] = nx
		}
		nin := *e.In
		nin.Lhs = lhs
		nin.List = list
		ne := e
		ne.In = &nin
		return ne, nil
	case exprQuantified:
		lhs, err := desugarNamedWindows(e.Quantified.Lhs, windows)
		if err != nil {
			return exprNode{}, err
		}
		arr, err := desugarNamedWindows(e.Quantified.Array, windows)
		if err != nil {
			return exprNode{}, err
		}
		nq := *e.Quantified
		nq.Lhs = lhs
		nq.Array = arr
		ne := e
		ne.Quantified = &nq
		return ne, nil
	case exprBetween:
		lhs, err := desugarNamedWindows(e.Between.Lhs, windows)
		if err != nil {
			return exprNode{}, err
		}
		lo, err := desugarNamedWindows(e.Between.Lo, windows)
		if err != nil {
			return exprNode{}, err
		}
		hi, err := desugarNamedWindows(e.Between.Hi, windows)
		if err != nil {
			return exprNode{}, err
		}
		nbt := *e.Between
		nbt.Lhs = lhs
		nbt.Lo = lo
		nbt.Hi = hi
		ne := e
		ne.Between = &nbt
		return ne, nil
	case exprLike:
		lhs, err := desugarNamedWindows(e.Like.Lhs, windows)
		if err != nil {
			return exprNode{}, err
		}
		rhs, err := desugarNamedWindows(e.Like.Rhs, windows)
		if err != nil {
			return exprNode{}, err
		}
		nl := *e.Like
		nl.Lhs = lhs
		nl.Rhs = rhs
		ne := e
		ne.Like = &nl
		return ne, nil
	case exprRegex:
		lhs, err := desugarNamedWindows(e.Regex.Lhs, windows)
		if err != nil {
			return exprNode{}, err
		}
		rhs, err := desugarNamedWindows(e.Regex.Rhs, windows)
		if err != nil {
			return exprNode{}, err
		}
		nr := *e.Regex
		nr.Lhs = lhs
		nr.Rhs = rhs
		ne := e
		ne.Regex = &nr
		return ne, nil
	case exprRow, exprArray:
		items := make([]exprNode, len(e.RowItems))
		for i, x := range e.RowItems {
			nx, err := desugarNamedWindows(x, windows)
			if err != nil {
				return exprNode{}, err
			}
			items[i] = nx
		}
		ne := e
		ne.RowItems = items
		return ne, nil
	case exprQualifiedStar:
		return e, nil // a leaf relation reference — no named window to desugar
	case exprFieldAccess, exprFieldStar:
		base, err := desugarNamedWindows(*e.Base, windows)
		if err != nil {
			return exprNode{}, err
		}
		ne := e
		ne.Base = &base
		return ne, nil
	case exprCase:
		nc := *e.Case
		if e.Case.Operand != nil {
			op, err := desugarNamedWindows(*e.Case.Operand, windows)
			if err != nil {
				return exprNode{}, err
			}
			nc.Operand = &op
		}
		whens := make([]caseWhen, len(e.Case.Whens))
		for i, w := range e.Case.Whens {
			cond, err := desugarNamedWindows(w.Cond, windows)
			if err != nil {
				return exprNode{}, err
			}
			res, err := desugarNamedWindows(w.Result, windows)
			if err != nil {
				return exprNode{}, err
			}
			whens[i] = caseWhen{Cond: cond, Result: res}
		}
		nc.Whens = whens
		if e.Case.Els != nil {
			els, err := desugarNamedWindows(*e.Case.Els, windows)
			if err != nil {
				return exprNode{}, err
			}
			nc.Els = &els
		}
		ne := e
		ne.Case = &nc
		return ne, nil
	default:
		// Leaves, subscripts, and subqueries (independent) carry no top-level window ref to rewrite.
		return e, nil
	}
}

// rejectCheckStructure applies the structural CHECK-expression rejections
// (spec/design/constraints.md §4.1) in a single depth-first pre-order walk before
// resolution: a subquery is 0A000, an aggregate call 42803, a bind parameter 42P02 — PG's
// codes and messages (oracle-probed; PG interleaves these with resolution in parse order,
// a documented micro-order divergence).
func rejectCheckStructure(e exprNode) error {
	switch e.Kind {
	case exprScalarSubquery, exprExists, exprInSubquery, exprQuantifiedSubquery:
		return newError(FeatureNotSupported, "cannot use subquery in check constraint")
	case exprParam:
		return newError(UndefinedParameter,
			"there is no parameter $"+strconv.FormatUint(e.Param, 10))
	case exprFuncCall:
		if isAggregateName(e.FuncCall.Name) {
			return newError(GroupingError,
				"aggregate functions are not allowed in check constraints")
		}
		for _, a := range e.FuncCall.Args {
			if err := rejectCheckStructure(*a); err != nil {
				return err
			}
		}
		return nil
	case exprCast:
		return rejectCheckStructure(e.Cast.Inner)
	case exprExtract:
		return rejectCheckStructure(e.Extract.Source)
	case exprCollate:
		return rejectCheckStructure(e.Collate.Inner)
	case exprUnary:
		return rejectCheckStructure(e.Unary.Operand)
	case exprIsNull:
		return rejectCheckStructure(e.IsNullOf.Operand)
	case exprIsJson:
		return rejectCheckStructure(e.IsJsonOf.Operand)
	case exprJsonCtor:
		return rejectCheckStructure(e.JsonCtorOf.Operand)
	case exprJsonExists:
		if err := rejectCheckStructure(e.JsonExists.Ctx); err != nil {
			return err
		}
		return rejectCheckStructure(e.JsonExists.Path)
	case exprJsonValue:
		if err := rejectCheckStructure(e.JsonValue.Ctx); err != nil {
			return err
		}
		return rejectCheckStructure(e.JsonValue.Path)
	case exprJsonQuery:
		if err := rejectCheckStructure(e.JsonQuery.Ctx); err != nil {
			return err
		}
		return rejectCheckStructure(e.JsonQuery.Path)
	case exprBinary:
		if err := rejectCheckStructure(e.Binary.Lhs); err != nil {
			return err
		}
		return rejectCheckStructure(e.Binary.Rhs)
	case exprIsDistinct:
		if err := rejectCheckStructure(e.IsDistinct.Lhs); err != nil {
			return err
		}
		return rejectCheckStructure(e.IsDistinct.Rhs)
	case exprLike:
		if err := rejectCheckStructure(e.Like.Lhs); err != nil {
			return err
		}
		return rejectCheckStructure(e.Like.Rhs)
	case exprRegex:
		if err := rejectCheckStructure(e.Regex.Lhs); err != nil {
			return err
		}
		return rejectCheckStructure(e.Regex.Rhs)
	case exprIn:
		if err := rejectCheckStructure(e.In.Lhs); err != nil {
			return err
		}
		for _, elem := range e.In.List {
			if err := rejectCheckStructure(elem); err != nil {
				return err
			}
		}
		return nil
	case exprBetween:
		if err := rejectCheckStructure(e.Between.Lhs); err != nil {
			return err
		}
		if err := rejectCheckStructure(e.Between.Lo); err != nil {
			return err
		}
		return rejectCheckStructure(e.Between.Hi)
	case exprCase:
		if e.Case.Operand != nil {
			if err := rejectCheckStructure(*e.Case.Operand); err != nil {
				return err
			}
		}
		for _, w := range e.Case.Whens {
			if err := rejectCheckStructure(w.Cond); err != nil {
				return err
			}
			if err := rejectCheckStructure(w.Result); err != nil {
				return err
			}
		}
		if e.Case.Els != nil {
			return rejectCheckStructure(*e.Case.Els)
		}
		return nil
	case exprFieldAccess, exprFieldStar:
		// Recurse into the composite base (spec/design/composite.md §S4) so a forbidden
		// subquery/aggregate/parameter hidden there is still rejected.
		return rejectCheckStructure(*e.Base)
	case exprQualifiedStar:
		return nil // cannot syntactically reach a CHECK (select-item-only); accept structurally
	case exprSubscript:
		// Recurse into the array base and every subscript bound.
		if err := rejectCheckStructure(*e.Base); err != nil {
			return err
		}
		for _, s := range e.Subscripts {
			for _, x := range subscriptSpecExprs(s) {
				if err := rejectCheckStructure(*x); err != nil {
					return err
				}
			}
		}
		return nil
	case exprQuantified:
		if err := rejectCheckStructure(e.Quantified.Lhs); err != nil {
			return err
		}
		return rejectCheckStructure(e.Quantified.Array)
	default: // ExprColumn, ExprQualifiedColumn, ExprLiteral
		return nil
	}
}

// rejectDefaultStructure is the structural pre-walk for a DEFAULT expression (constraints.md
// §2), run before name/type resolution (the same micro-order divergence from PG that
// rejectCheckStructure carries). A default extends the CHECK rejections with one more: it may
// NOT reference a column (it is computed before the row exists). Codes match PostgreSQL
// (oracle-probed): a column reference / subquery is 0A000, an aggregate 42803, a parameter 42P02.
func rejectDefaultStructure(e exprNode) error {
	switch e.Kind {
	case exprColumn, exprQualifiedColumn:
		return newError(FeatureNotSupported, "cannot use column reference in DEFAULT expression")
	case exprScalarSubquery, exprExists, exprInSubquery, exprQuantifiedSubquery:
		return newError(FeatureNotSupported, "cannot use subquery in DEFAULT expression")
	case exprParam:
		return newError(UndefinedParameter,
			"there is no parameter $"+strconv.FormatUint(e.Param, 10))
	case exprFuncCall:
		if isAggregateName(e.FuncCall.Name) {
			return newError(GroupingError,
				"aggregate functions are not allowed in DEFAULT expressions")
		}
		for _, a := range e.FuncCall.Args {
			if err := rejectDefaultStructure(*a); err != nil {
				return err
			}
		}
		return nil
	case exprCast:
		return rejectDefaultStructure(e.Cast.Inner)
	case exprExtract:
		return rejectDefaultStructure(e.Extract.Source)
	case exprCollate:
		return rejectDefaultStructure(e.Collate.Inner)
	case exprUnary:
		return rejectDefaultStructure(e.Unary.Operand)
	case exprIsNull:
		return rejectDefaultStructure(e.IsNullOf.Operand)
	case exprIsJson:
		return rejectDefaultStructure(e.IsJsonOf.Operand)
	case exprJsonCtor:
		return rejectDefaultStructure(e.JsonCtorOf.Operand)
	case exprJsonExists:
		if err := rejectDefaultStructure(e.JsonExists.Ctx); err != nil {
			return err
		}
		return rejectDefaultStructure(e.JsonExists.Path)
	case exprJsonValue:
		if err := rejectDefaultStructure(e.JsonValue.Ctx); err != nil {
			return err
		}
		return rejectDefaultStructure(e.JsonValue.Path)
	case exprJsonQuery:
		if err := rejectDefaultStructure(e.JsonQuery.Ctx); err != nil {
			return err
		}
		return rejectDefaultStructure(e.JsonQuery.Path)
	case exprBinary:
		if err := rejectDefaultStructure(e.Binary.Lhs); err != nil {
			return err
		}
		return rejectDefaultStructure(e.Binary.Rhs)
	case exprIsDistinct:
		if err := rejectDefaultStructure(e.IsDistinct.Lhs); err != nil {
			return err
		}
		return rejectDefaultStructure(e.IsDistinct.Rhs)
	case exprLike:
		if err := rejectDefaultStructure(e.Like.Lhs); err != nil {
			return err
		}
		return rejectDefaultStructure(e.Like.Rhs)
	case exprRegex:
		if err := rejectDefaultStructure(e.Regex.Lhs); err != nil {
			return err
		}
		return rejectDefaultStructure(e.Regex.Rhs)
	case exprIn:
		if err := rejectDefaultStructure(e.In.Lhs); err != nil {
			return err
		}
		for _, elem := range e.In.List {
			if err := rejectDefaultStructure(elem); err != nil {
				return err
			}
		}
		return nil
	case exprBetween:
		if err := rejectDefaultStructure(e.Between.Lhs); err != nil {
			return err
		}
		if err := rejectDefaultStructure(e.Between.Lo); err != nil {
			return err
		}
		return rejectDefaultStructure(e.Between.Hi)
	case exprCase:
		if e.Case.Operand != nil {
			if err := rejectDefaultStructure(*e.Case.Operand); err != nil {
				return err
			}
		}
		for _, w := range e.Case.Whens {
			if err := rejectDefaultStructure(w.Cond); err != nil {
				return err
			}
			if err := rejectDefaultStructure(w.Result); err != nil {
				return err
			}
		}
		if e.Case.Els != nil {
			return rejectDefaultStructure(*e.Case.Els)
		}
		return nil
	case exprFieldAccess, exprFieldStar:
		// Recurse into the composite base (spec/design/composite.md §S4).
		return rejectDefaultStructure(*e.Base)
	case exprQualifiedStar:
		return nil // cannot syntactically reach a DEFAULT (select-item-only); accept structurally
	case exprSubscript:
		// Recurse into the array base and every subscript bound.
		if err := rejectDefaultStructure(*e.Base); err != nil {
			return err
		}
		for _, s := range e.Subscripts {
			for _, x := range subscriptSpecExprs(s) {
				if err := rejectDefaultStructure(*x); err != nil {
					return err
				}
			}
		}
		return nil
	case exprQuantified:
		if err := rejectDefaultStructure(e.Quantified.Lhs); err != nil {
			return err
		}
		return rejectDefaultStructure(e.Quantified.Array)
	default: // ExprLiteral, ExprTypedLiteral
		return nil
	}
}

// checkReferencedColumns returns the distinct columns a CHECK expression references, as
// indices into columns — the input to PG's auto-naming rule (constraints.md §4.3: exactly
// one distinct column → <table>_<col>_check). Resolution already validated every
// reference, so an unknown name is simply skipped; a qualified reference counts its column
// like a bare one (oracle-probed).
func checkReferencedColumns(e exprNode, columns []catColumn) []int {
	var out []int
	var walk func(e exprNode)
	note := func(name string) {
		for i := range columns {
			if strings.EqualFold(columns[i].Name, name) {
				if !slices.Contains(out, i) {
					out = append(out, i)
				}
				return
			}
		}
	}
	walk = func(e exprNode) {
		switch e.Kind {
		case exprColumn, exprQualifiedColumn:
			note(e.Column)
		case exprCast:
			walk(e.Cast.Inner)
		case exprExtract:
			walk(e.Extract.Source)
		case exprCollate:
			walk(e.Collate.Inner)
		case exprUnary:
			walk(e.Unary.Operand)
		case exprIsNull:
			walk(e.IsNullOf.Operand)
		case exprIsJson:
			walk(e.IsJsonOf.Operand)
		case exprJsonCtor:
			walk(e.JsonCtorOf.Operand)
		case exprJsonExists:
			walk(e.JsonExists.Ctx)
			walk(e.JsonExists.Path)
		case exprJsonValue:
			walk(e.JsonValue.Ctx)
			walk(e.JsonValue.Path)
		case exprJsonQuery:
			walk(e.JsonQuery.Ctx)
			walk(e.JsonQuery.Path)
		case exprBinary:
			walk(e.Binary.Lhs)
			walk(e.Binary.Rhs)
		case exprIsDistinct:
			walk(e.IsDistinct.Lhs)
			walk(e.IsDistinct.Rhs)
		case exprLike:
			walk(e.Like.Lhs)
			walk(e.Like.Rhs)
		case exprRegex:
			walk(e.Regex.Lhs)
			walk(e.Regex.Rhs)
		case exprIn:
			walk(e.In.Lhs)
			for _, elem := range e.In.List {
				walk(elem)
			}
		case exprBetween:
			walk(e.Between.Lhs)
			walk(e.Between.Lo)
			walk(e.Between.Hi)
		case exprCase:
			if e.Case.Operand != nil {
				walk(*e.Case.Operand)
			}
			for _, w := range e.Case.Whens {
				walk(w.Cond)
				walk(w.Result)
			}
			if e.Case.Els != nil {
				walk(*e.Case.Els)
			}
		case exprFuncCall:
			for _, a := range e.FuncCall.Args {
				walk(*a)
			}
		case exprFieldAccess, exprFieldStar:
			// Field selection recurses into the composite base (spec/design/composite.md §S4).
			walk(*e.Base)
		case exprQualifiedStar:
			// `t.*` cannot appear in a CHECK expression (select-item-only); no columns to note.
		case exprSubscript:
			// `base[..]` recurses into the array base and every subscript bound.
			walk(*e.Base)
			for _, s := range e.Subscripts {
				for _, x := range subscriptSpecExprs(s) {
					walk(*x)
				}
			}
		case exprQuantified:
			walk(e.Quantified.Lhs)
			walk(e.Quantified.Array)
		}
	}
	walk(e)
	return out
}

// resolveAggregate resolves an aggregate call into a synthetic-row reference, collecting its
// aggSpec. Valid only in collect mode; in Forbidden mode (WHERE/ON/nested) it is 42803. The
// operand resolves in a fresh Forbidden sub-context (a nested aggregate is 42803; its columns
// resolve against the real row). The result type follows the PG widening (aggregates.md §3).
func resolveAggregate(s *scope, fc *funcCallExpr, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	if !ag.collecting {
		return nil, resolvedType{}, newError(GroupingError, "aggregate functions are not allowed here")
	}
	name := toLowerASCII(fc.Name)
	sub := &aggCtx{collecting: false}
	var (
		plan    aggPlan
		operand *rExpr
		result  resolvedType
	)
	// json[b]_object_agg[_unique] take TWO operands (key, value) — resolve both and encode as a Row
	// operand for the single-operand aggregate framework (the fold splits the composite back out).
	if objPlan, ok := objectAggClassify(name); ok {
		if fc.Star || len(fc.Args) != 2 {
			return nil, resolvedType{}, noAggOverload(name)
		}
		rk, _, err := resolve(s, *fc.Args[0], nil, sub, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		rv, _, err := resolve(s, *fc.Args[1], nil, sub, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		operand := &rExpr{kind: reRow, sargs: []*rExpr{rk, rv}}
		result := resolvedType{kind: rtJsonb}
		if objPlan == planJsonObjectAgg || objPlan == planJsonObjectAggUnique {
			result = resolvedType{kind: rtJson}
		}
		slot := len(ag.groupKeys) + len(ag.specs)
		ag.specs = append(ag.specs, aggSpec{plan: objPlan, operand: operand})
		return &rExpr{kind: reColumn, index: slot}, result, nil
	}
	if fc.Star {
		// Only COUNT has a star overload (aggregates.md §3); SUM(*) etc. is a syntax error.
		if !aggregateHasStar(name) {
			return nil, resolvedType{}, newError(SyntaxError, "* is only valid as the argument of COUNT")
		}
		plan, operand, result = planCountStar, nil, resolvedType{kind: rtInt, intTy: scalarInt64}
	} else {
		// One operand, resolved in a fresh Forbidden sub-context. The registry validates the
		// (surface, operand-family) overload exists (else 42883) and yields its result code; the
		// plan + result type follow from it (the PG widening).
		arg, err := aggArg(fc)
		if err != nil {
			return nil, resolvedType{}, err
		}
		// An aggregate's argument may not contain a window function (PG 42803 — window.md §7): the
		// window stage runs AFTER aggregation, so a window result cannot be folded into an aggregate.
		if exprHasWindow(arg) {
			return nil, resolvedType{}, newError(GroupingError, "aggregate function calls cannot contain window function calls")
		}
		r, t, err := resolve(s, arg, nil, sub, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		desc := lookupAggregateOverload(name, t)
		if desc == nil {
			return nil, resolvedType{}, noAggOverload(name)
		}
		plan, result = aggregatePlan(name, desc.Result, t)
		operand = r
	}
	// FILTER (WHERE cond): resolve the per-row predicate against the input row with aggregates
	// FORBIDDEN — an aggregate inside FILTER is 42803, matching PG (aggregates.md §11). A non-boolean
	// condition (or an untyped NULL, always unknown → folds no row) is 42804. The fold loop evaluates
	// this per row and folds only the rows for which it is TRUE.
	var filter *rExpr
	if fc.Filter != nil {
		fsub := &aggCtx{collecting: false}
		rf, ft, err := resolve(s, *fc.Filter, nil, fsub, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if ft.kind != rtBool && ft.kind != rtNull {
			return nil, resolvedType{}, typeError("argument of FILTER must be type boolean")
		}
		filter = rf
	}
	// Aggregate results follow the group-key values in the synthetic row.
	slot := len(ag.groupKeys) + len(ag.specs)
	ag.specs = append(ag.specs, aggSpec{plan: plan, operand: operand, distinct: fc.Distinct, filter: filter})
	return &rExpr{kind: reColumn, index: slot}, result, nil
}

// resolveOrderedSetAggregate resolves agg(direct_args) WITHIN GROUP (ORDER BY key) — mode,
// percentile_cont, percentile_disc (spec/design/aggregates.md §13). Like resolveAggregate it is
// valid only in collect mode (else 42803) and folds into the same aggSpec list, returning a
// synthetic-row reference. The WITHIN GROUP key is the aggregate's operand (resolved with aggregates
// forbidden — a nested aggregate is 42803); the parenthesized Args are the per-group direct argument
// (the percentile fraction; empty for mode).
func resolveOrderedSetAggregate(s *scope, fc *funcCallExpr, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	if !ag.collecting {
		return nil, resolvedType{}, newError(GroupingError, "aggregate functions are not allowed here")
	}
	// DISTINCT cannot decorate an ordered-set aggregate (PG: a 42601 syntax error).
	if fc.Distinct {
		return nil, resolvedType{}, newError(SyntaxError, "DISTINCT is not allowed with ordered-set aggregates")
	}
	name := toLowerASCII(fc.Name)
	// Exactly one WITHIN GROUP sort key (PG models a second as a missing overload → 42883).
	if len(fc.WithinGroup) != 1 {
		return nil, resolvedType{}, noAggOverload(name)
	}
	key := fc.WithinGroup[0]
	// The aggregated argument: the WITHIN GROUP order key, resolved per row with aggregates FORBIDDEN
	// (a nested aggregate in the order key is 42803, matching PG). A general-expression key
	// (`ORDER BY a + b`) carries Expr; a bare/qualified column key carries Column (rebuilt here as an
	// Expr so both paths share one resolve).
	var keyExpr exprNode
	if key.Expr != nil {
		keyExpr = *key.Expr
	} else if key.Qualifier != "" {
		keyExpr = exprNode{Kind: exprQualifiedColumn, Qualifier: key.Qualifier, Column: key.Column}
	} else {
		keyExpr = exprNode{Kind: exprColumn, Column: key.Column}
	}
	sub := &aggCtx{collecting: false}
	operand, optype, err := resolve(s, keyExpr, nil, sub, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	// The WITHIN GROUP key's COLLATION drives the sort (aggregates.md §13): an explicit COLLATE on the
	// key (text operand only — else "collations are not supported by type T", like the query ORDER BY),
	// else a bare/qualified column key inherits its column's frozen collation; otherwise the default C
	// (byte) order. Resolved to the loaded Collation (42704 if not loaded). The finalize sort applies
	// it (an unmapped code point → 0A000 there).
	var collation *Collation
	if key.Collation != "" {
		if optype.kind != rtText && optype.kind != rtNull {
			return nil, resolvedType{}, typeError(fmt.Sprintf("collations are not supported by type %s", rtName(optype)))
		}
		if collation, err = resolveCollationName(s.catalog, key.Collation); err != nil {
			return nil, resolvedType{}, err
		}
	} else if key.Expr == nil {
		// A bare/qualified column key with no explicit COLLATE inherits the column's frozen collation.
		var r resolved
		if key.Qualifier != "" {
			r, err = s.resolveQualified(key.Qualifier, key.Column)
		} else {
			r, err = s.resolveBare(key.Column)
		}
		if err != nil {
			return nil, resolvedType{}, err
		}
		if cn := s.columnOf(r).Collation; cn != "" {
			if collation, err = resolveCollationName(s.catalog, cn); err != nil {
				return nil, resolvedType{}, err
			}
		}
	}
	var (
		plan   aggPlan
		frac   *rExpr
		result resolvedType
	)
	switch name {
	case "mode":
		// mode() takes no direct argument; mode(x) matches no overload (42883).
		if len(fc.Args) != 0 {
			return nil, resolvedType{}, noAggOverload(name)
		}
		plan, frac, result = planMode, nil, optype
	case "percentile_disc":
		// An ARRAY fraction (percentile_disc(ARRAY[…])) returns an array of percentiles, one per
		// element; a scalar fraction returns one value (aggregates.md §18).
		f, isArray, err := resolveOsaFraction(s, name, fc.Args, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		plan, frac, result = planPercentileDisc, f, arrayIf(optype, isArray)
	case "percentile_cont":
		// percentile_cont interpolates: over a NUMERIC input it widens to f64 and returns f64; over
		// an INTERVAL input it interpolates in the interval domain (PG interval_lerp) and returns
		// interval. Any other WITHIN GROUP type matches no overload (42883). The fraction resolves
		// first (matching Rust's order) so an arity/type error on it is raised before the operand
		// check. An ARRAY fraction makes the result an array of those percentiles (aggregates.md §18).
		f, isArray, err := resolveOsaFraction(s, name, fc.Args, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		switch optype.kind {
		case rtInt, rtDecimal, rtFloat32, rtFloat64:
			plan, frac, result = planPercentileCont, f, arrayIf(resolvedType{kind: rtFloat64}, isArray)
		case rtInterval:
			plan, frac, result = planPercentileContInterval, f, arrayIf(resolvedType{kind: rtInterval}, isArray)
		default:
			return nil, resolvedType{}, noAggOverload(name)
		}
	default:
		panic("isOrderedSetAggregateName gates the three names above")
	}
	// FILTER (WHERE cond): resolved per input row with aggregates forbidden, exactly as for an
	// ordinary aggregate (aggregates.md §11) — a non-boolean cond is 42804, a nested aggregate 42803.
	var filter *rExpr
	if fc.Filter != nil {
		fsub := &aggCtx{collecting: false}
		rf, ft, err := resolve(s, *fc.Filter, nil, fsub, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if ft.kind != rtBool && ft.kind != rtNull {
			return nil, resolvedType{}, typeError("argument of FILTER must be type boolean")
		}
		filter = rf
	}
	slot := len(ag.groupKeys) + len(ag.specs)
	ag.specs = append(ag.specs, aggSpec{plan: plan, operand: operand, distinct: false, filter: filter, osaDesc: key.Descending, osaFrac: frac, osaCollation: collation})
	return &rExpr{kind: reColumn, index: slot}, result, nil
}

// resolveHypotheticalSetAggregate resolves a hypothetical-set aggregate f(direct_args) WITHIN GROUP
// (ORDER BY keys) — rank, dense_rank, percent_rank, cume_dist (spec/design/aggregates.md §19). The
// direct args are the hypothetical row; the WITHIN GROUP keys are the sort columns. Their counts
// must match (else 42883). Each key operand is buffered per row; each direct arg is evaluated per
// group (it may reference grouping columns) and coerced to the key's type. Like the other
// ordered-set aggregates, OVER is 0A000, DISTINCT is 42601, and it is valid only in a collecting
// context.
func resolveHypotheticalSetAggregate(s *scope, fc *funcCallExpr, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	if !ag.collecting {
		return nil, resolvedType{}, newError(GroupingError, "aggregate functions are not allowed here")
	}
	if fc.Distinct {
		return nil, resolvedType{}, newError(SyntaxError, "DISTINCT is not allowed with ordered-set aggregates")
	}
	name := toLowerASCII(fc.Name)
	// The number of hypothetical direct arguments must match the number of ordering columns (PG
	// models a mismatch as a missing overload → 42883).
	if len(fc.Args) == 0 || len(fc.Args) != len(fc.WithinGroup) {
		return nil, resolvedType{}, noAggOverload(name)
	}
	// Resolve each WITHIN GROUP key operand (per row, aggregates forbidden) + its sort spec, then the
	// matching direct argument (per group, in the grouped context so it may reference grouping
	// columns) coerced to the key's type.
	keyNodes := make([]*rExpr, 0, len(fc.WithinGroup))
	sorts := make([]keySort, 0, len(fc.WithinGroup))
	argNodes := make([]*rExpr, 0, len(fc.Args))
	for i := range fc.WithinGroup {
		key := fc.WithinGroup[i]
		arg := fc.Args[i]
		// The WITHIN GROUP order key, resolved per row with aggregates FORBIDDEN (a nested aggregate is
		// 42803). A general-expression key carries Expr; a bare/qualified column key carries Column.
		var keyExpr exprNode
		if key.Expr != nil {
			keyExpr = *key.Expr
		} else if key.Qualifier != "" {
			keyExpr = exprNode{Kind: exprQualifiedColumn, Qualifier: key.Qualifier, Column: key.Column}
		} else {
			keyExpr = exprNode{Kind: exprColumn, Column: key.Column}
		}
		sub := &aggCtx{collecting: false}
		knode, ktype, err := resolve(s, keyExpr, nil, sub, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		// The key's collation (explicit COLLATE — text only — or a bare/qualified column's frozen
		// collation), §13. An unknown name is 42704; a COLLATE on a non-text key is 42804.
		var collation *Collation
		if key.Collation != "" {
			if ktype.kind != rtText && ktype.kind != rtNull {
				return nil, resolvedType{}, typeError(fmt.Sprintf("collations are not supported by type %s", rtName(ktype)))
			}
			if collation, err = resolveCollationName(s.catalog, key.Collation); err != nil {
				return nil, resolvedType{}, err
			}
		} else if key.Expr == nil {
			var r resolved
			if key.Qualifier != "" {
				r, err = s.resolveQualified(key.Qualifier, key.Column)
			} else {
				r, err = s.resolveBare(key.Column)
			}
			if err != nil {
				return nil, resolvedType{}, err
			}
			if cn := s.columnOf(r).Collation; cn != "" {
				if collation, err = resolveCollationName(s.catalog, cn); err != nil {
					return nil, resolvedType{}, err
				}
			}
		}
		// The hypothetical direct arg, evaluated per group (grouped context); a literal adapts to the
		// key's scalar type via the hint. Its type must match the key's family (else 42883).
		var hint *scalarType
		if t, err := typeFromResolved(ktype); err == nil && t.Comp == nil && t.Array == nil && t.Range == nil {
			st := t.Scalar
			hint = &st
		}
		anode, atype, err := resolve(s, *arg, hint, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if !hypoArgCompatible(atype, ktype) {
			return nil, resolvedType{}, noAggOverload(name)
		}
		keyNodes = append(keyNodes, knode)
		sorts = append(sorts, keySort{desc: key.Descending, nullsFirst: key.NullsFirst, collation: collation})
		argNodes = append(argNodes, anode)
	}

	// FILTER (WHERE cond): per-input-row predicate (aggregates forbidden); restricts buffered rows.
	var filter *rExpr
	if fc.Filter != nil {
		fsub := &aggCtx{collecting: false}
		rf, ft, err := resolve(s, *fc.Filter, nil, fsub, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if ft.kind != rtBool && ft.kind != rtNull {
			return nil, resolvedType{}, typeError("argument of FILTER must be type boolean")
		}
		filter = rf
	}

	var (
		plan   aggPlan
		result resolvedType
	)
	switch name {
	case "rank":
		plan, result = planHypoRank, resolvedType{kind: rtInt, intTy: scalarInt64}
	case "dense_rank":
		plan, result = planHypoDenseRank, resolvedType{kind: rtInt, intTy: scalarInt64}
	case "percent_rank":
		plan, result = planHypoPercentRank, resolvedType{kind: rtFloat64}
	case "cume_dist":
		plan, result = planHypoCumeDist, resolvedType{kind: rtFloat64}
	default:
		panic("isHypotheticalSetName gates the four names above")
	}
	slot := len(ag.groupKeys) + len(ag.specs)
	ag.specs = append(ag.specs, aggSpec{plan: plan, operand: nil, distinct: false, filter: filter, hypo: &hypoParams{args: argNodes, keys: keyNodes, sorts: sorts}})
	return &rExpr{kind: reColumn, index: slot}, result, nil
}

// hypoArgCompatible reports whether a hypothetical direct argument of type arg is comparable with the
// WITHIN GROUP key of type key (aggregates.md §19). A NULL arg is always allowed; otherwise the two
// must be the same scalar family (numeric Int/Decimal/Float each only match themselves, exactly as
// the value comparator orders them), so the buffered key tuple and the hypothetical row compare
// meaningfully.
func hypoArgCompatible(arg, key resolvedType) bool {
	if arg.kind == rtNull {
		return true
	}
	switch {
	case arg.kind == rtInt && key.kind == rtInt,
		arg.kind == rtDecimal && key.kind == rtDecimal,
		isFloatKind(arg.kind) && isFloatKind(key.kind),
		arg.kind == rtText && key.kind == rtText,
		arg.kind == rtBool && key.kind == rtBool,
		arg.kind == rtBytea && key.kind == rtBytea,
		arg.kind == rtUuid && key.kind == rtUuid,
		arg.kind == rtTimestamp && key.kind == rtTimestamp,
		arg.kind == rtTimestamptz && key.kind == rtTimestamptz,
		arg.kind == rtDate && key.kind == rtDate,
		arg.kind == rtInterval && key.kind == rtInterval:
		return true
	}
	return false
}

// resolveOsaFraction resolves an ordered-set aggregate's direct argument — the percentile fraction
// (aggregates.md §13/§17/§18). The fraction is evaluated **once per group**, so it may be any
// expression over **grouping columns** (resolved here in the grouped agg context, so a grouping
// column binds its synthetic key slot and a non-grouped column is 42803 — PG's "direct arguments …
// must use only grouped columns"; a constant folds the usual way). An aggregate inside the fraction
// is 42803 (PG forbids nesting). Resolved with a float hint so a bare numeric literal folds to f64.
// The returned node is stored and evaluated per group at finalize. Returns (node, isArray) — a
// NUMERIC array fraction (percentile_cont(ARRAY[…])) computes one percentile per element and returns
// an array (§18). A non-numeric fraction or a wrong argument count matches no overload (42883); a
// NULL fraction yields a NULL result at finalize.
func resolveOsaFraction(s *scope, name string, args []*exprNode, ag *aggCtx, params *paramTypes) (*rExpr, bool, error) {
	if len(args) != 1 {
		return nil, false, noAggOverload(name) // wrong argument count
	}
	// The fraction is evaluated before the fold (it is a direct argument, not an aggregate operand),
	// so a nested aggregate is illegal — 42803, matching PG.
	if exprHasAggregate(*args[0]) {
		return nil, false, newError(GroupingError, "aggregate function calls cannot be nested")
	}
	fl := scalarFloat64
	rarg, rtype, err := resolve(s, *args[0], &fl, ag, params)
	if err != nil {
		return nil, false, err
	}
	switch rtype.kind {
	case rtNull, rtFloat32, rtFloat64, rtInt, rtDecimal:
		return rarg, false, nil
	case rtArray:
		// A NUMERIC array fraction returns an array of percentiles, one per element (§18); a
		// non-numeric element matches no overload.
		switch rtype.elem.kind {
		case rtFloat32, rtFloat64, rtInt, rtDecimal:
			return rarg, true, nil
		default:
			return nil, false, noAggOverload(name)
		}
	default:
		return nil, false, noAggOverload(name) // a non-numeric fraction matches no overload
	}
}

// arrayIf returns Array(t) when isArray, else t — the result type of an ordered-set aggregate whose
// direct argument is an array vs. a scalar fraction (aggregates.md §18).
func arrayIf(t resolvedType, isArray bool) resolvedType {
	if isArray {
		return resolvedType{kind: rtArray, elem: &t}
	}
	return t
}

// resolveGrouping resolves GROUPING(c1, …, ck) (spec/design/aggregates.md §12) — the grouping-sets
// membership function. Valid only in a grouped query's projection / HAVING (collecting); each
// argument must be one of the master grouping columns, else 42803 (matching PostgreSQL). Returns an
// integer (i32) whose bit (k-1-j) is 1 iff c_j is grouped away in the row's grouping set. The value
// is computed per group row at execution from the grouping set's mask, so the call resolves to the
// placeholder slot groupingGsBase+index (rebased to its real trailing synthetic slot afterwards).
func resolveGrouping(s *scope, fc *funcCallExpr, ag *aggCtx) (*rExpr, resolvedType, error) {
	if fc.Star {
		// GROUPING(*) — PG raises a syntax error; mirror the COUNT-only `*` message (42601).
		return nil, resolvedType{}, newError(SyntaxError, "* is only valid as the argument of COUNT")
	}
	if len(fc.Args) == 0 {
		// GROUPING() with no arguments — PG raises a syntax error (42601).
		return nil, resolvedType{}, newError(SyntaxError, "GROUPING requires at least one argument")
	}
	groupingArgErr := func() error {
		return newError(GroupingError, "arguments to GROUPING must be grouping expressions of the associated query level")
	}
	// GROUPING is meaningful only in a grouped query (ag.collecting) — including a grouped query that
	// ALSO has window functions (GROUPING SETS + window, aggregates.md §21); outside one its arguments
	// cannot be grouping expressions.
	if !ag.collecting {
		return nil, resolvedType{}, groupingArgErr()
	}
	positions := make([]int, 0, len(fc.Args))
	for _, arg := range fc.Args {
		var (
			r   resolved
			err error
		)
		switch arg.Kind {
		case exprColumn:
			r, err = s.resolveBare(arg.Column)
		case exprQualifiedColumn:
			r, err = s.resolveQualified(arg.Qualifier, arg.Column)
		default:
			// A non-column argument is never a grouping column (jed groups by columns only).
			return nil, resolvedType{}, groupingArgErr()
		}
		if err != nil {
			return nil, resolvedType{}, err
		}
		if r.level != 0 {
			return nil, resolvedType{}, groupingArgErr()
		}
		pos := -1
		for p, gk := range ag.groupKeys {
			if gk == r.index {
				pos = p
				break
			}
		}
		if pos < 0 {
			return nil, resolvedType{}, groupingArgErr()
		}
		positions = append(positions, pos)
	}
	slot := groupingGsBase + len(ag.groupingSpecs)
	ag.groupingSpecs = append(ag.groupingSpecs, positions)
	return &rExpr{kind: reColumn, index: slot}, resolvedType{kind: rtInt, intTy: scalarInt32}, nil
}

// resolveWindowCall resolves a window-function call `f(args) OVER (window_definition)`
// (spec/design/window.md §5.1). Valid only in a window query's projection (ag.windowing); anywhere
// else (WHERE / JOIN ON / HAVING / an aggregate query) it is 42P20. The call collects into a
// windowSpec and resolves to the synthetic slot windowBase+window_index. S0: only row_number().
func resolveWindowCall(s *scope, fc *funcCallExpr, filter *exprNode, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	name := toLowerASCII(fc.Name)
	// The plan + result type from the function name. S0: only row_number(); an aggregate name with
	// OVER (a window aggregate, S3) resolves to planAgg carrying the aggregate plan in wagg; any
	// other name is 42883.
	var (
		plan   windowPlan
		result resolvedType
		wagg   aggPlan // the aggregate plan, valid only when plan == planAgg
	)
	// The sub-context window ARGUMENTS resolve in (spec/design/window.md §5.1). In a grouped query
	// (ag.collecting) they resolve against the grouped row — a nested aggregate collects into the
	// query's SHARED specs and a bare column must be a grouping key (else 42803) — so `sub` is a
	// collecting context seeded with the running specs; a nested window is then 42P20 (sub is not
	// windowing). In a plain window query `sub` is Forbidden (no aggregate/window nesting). The grown
	// specs are written back into ag at the end so the next window's nested aggregates keep numbering.
	sub := &aggCtx{}
	if ag.collecting {
		// Seed with the running grouping specs too, so a GROUPING() nested in a window argument collects
		// into the query's shared grouping specs (GROUPING SETS + window, aggregates.md §21); written
		// back alongside specs below.
		sub = &aggCtx{collecting: true, groupKeys: ag.groupKeys, groupKeyExprs: ag.groupKeyExprs, specs: ag.specs, groupingSpecs: ag.groupingSpecs}
	}
	// The frame-insensitive no-argument ranking functions (S0/S1): row_number/rank/dense_rank → i64.
	noArgI64, isNoArg := map[string]windowPlan{
		"row_number": planRowNumber,
		"rank":       planRank,
		"dense_rank": planDenseRank,
	}[name]
	// The frame-insensitive no-argument ratio functions (S1): percent_rank/cume_dist → f64
	// (PG's float8 — the ratio is the IEEE correctly-rounded f64 division, window.md §4).
	noArgRatio, isNoArgRatio := map[string]windowPlan{
		"percent_rank": planPercentRank,
		"cume_dist":    planCumeDist,
	}[name]
	var wargs []*rExpr
	switch {
	case isNoArg:
		if fc.Star || len(fc.Args) != 0 {
			return nil, resolvedType{}, newError(UndefinedFunction, name+" takes no arguments")
		}
		plan = noArgI64
		result = resolvedType{kind: rtInt, intTy: scalarInt64}
	case isNoArgRatio:
		if fc.Star || len(fc.Args) != 0 {
			return nil, resolvedType{}, newError(UndefinedFunction, name+" takes no arguments")
		}
		plan = noArgRatio
		result = resolvedType{kind: rtFloat64}
	case name == "ntile":
		// ntile(n) — one integer bucket-count argument (window.md §4), resolved in a fresh
		// Forbidden sub-context (no aggregate/window nesting in a window argument).
		if fc.Star || len(fc.Args) != 1 {
			return nil, resolvedType{}, newError(UndefinedFunction, "ntile takes exactly one argument")
		}
		anode, aty, err := resolve(s, *fc.Args[0], nil, sub, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if aty.kind != rtInt && aty.kind != rtNull {
			return nil, resolvedType{}, typeError("argument of ntile must be integer")
		}
		wargs = append(wargs, anode)
		plan = planNtile
		result = resolvedType{kind: rtInt, intTy: scalarInt64}
	case name == "lag" || name == "lead":
		// lag/lead(value [, offset [, default]]) — window.md §4. The value expression's type is the
		// result; offset is an integer (default 1); default (returned when the offset leaves the
		// partition) must match the value type. Args resolved in a fresh Forbidden sub-context.
		if fc.Star || len(fc.Args) == 0 || len(fc.Args) > 3 {
			return nil, resolvedType{}, newError(UndefinedFunction, name+" takes 1 to 3 arguments")
		}
		vnode, vty, err := resolve(s, *fc.Args[0], nil, sub, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		// The scalar hint for resolving the default literal: the value's scalar for an int/float
		// value type, else none (mirrors Rust's Int(s) | Float(s) => Some(*s)).
		var hint *scalarType
		switch vty.kind {
		case rtInt:
			h := vty.intTy
			hint = &h
		case rtFloat32:
			h := scalarFloat32
			hint = &h
		case rtFloat64:
			h := scalarFloat64
			hint = &h
		}
		wargs = append(wargs, vnode)
		if len(fc.Args) >= 2 {
			onode, oty, err := resolve(s, *fc.Args[1], nil, sub, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			if oty.kind != rtInt && oty.kind != rtNull {
				return nil, resolvedType{}, typeError("offset of " + name + " must be integer")
			}
			wargs = append(wargs, onode)
		}
		if len(fc.Args) == 3 {
			dnode, dty, err := resolve(s, *fc.Args[2], hint, sub, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			if dty.kind != rtNull && !resolvedTypeEqual(dty, vty) {
				return nil, resolvedType{}, typeError("default of " + name + " must match the value type")
			}
			wargs = append(wargs, dnode)
		}
		if name == "lag" {
			plan = planLag
		} else {
			plan = planLead
		}
		result = vty
	case isAggregateName(name):
		// An aggregate used as a window function (S3): reuse the aggregate overload resolution to
		// get the plan + result type; applyWindowStage folds it over the default frame (running
		// with a window ORDER BY, whole-partition without — spec/design/window.md §6).
		if fc.Star {
			// Only COUNT has a star overload; SUM(*) OVER () etc. is a syntax error.
			if !aggregateHasStar(name) {
				return nil, resolvedType{}, newError(SyntaxError, "* is only valid as the argument of COUNT")
			}
			wagg = planCountStar
			result = resolvedType{kind: rtInt, intTy: scalarInt64}
		} else {
			// One operand, resolved in a fresh Forbidden sub-context (no aggregate/window nesting
			// in a window aggregate's argument). The registry validates the (surface, operand-family)
			// overload exists (else 42883); the plan + result type follow the PG widening.
			arg, err := aggArg(fc)
			if err != nil {
				return nil, resolvedType{}, err
			}
			r, t, err := resolve(s, arg, nil, sub, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			desc := lookupAggregateOverload(name, t)
			if desc == nil {
				return nil, resolvedType{}, noAggOverload(name)
			}
			wagg, result = aggregatePlan(name, desc.Result, t)
			wargs = append(wargs, r) // the aggregate operand → args[0]
		}
		plan = planAgg
	case name == "first_value" || name == "last_value" || name == "nth_value":
		// Frame-sensitive value pickers (S4, window.md §4). first/last_value take one value
		// expression (→ result type); nth_value takes the value + an integer position. Args
		// resolved in a fresh Forbidden sub-context (no aggregate/window nesting).
		if fc.Star {
			return nil, resolvedType{}, newError(SyntaxError, "* is only valid as the argument of COUNT")
		}
		want := 1
		if name == "nth_value" {
			want = 2
		}
		if len(fc.Args) != want {
			return nil, resolvedType{}, newError(UndefinedFunction,
				fmt.Sprintf("%s takes %d argument(s)", name, want))
		}
		vnode, vty, err := resolve(s, *fc.Args[0], nil, sub, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		wargs = append(wargs, vnode)
		if name == "first_value" {
			plan = planFirstValue
		} else if name == "last_value" {
			plan = planLastValue
		} else {
			nnode, nty, err := resolve(s, *fc.Args[1], nil, sub, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			if nty.kind != rtInt && nty.kind != rtNull {
				return nil, resolvedType{}, typeError("position of nth_value must be integer")
			}
			wargs = append(wargs, nnode)
			plan = planNthValue
		}
		result = vty
	default:
		return nil, resolvedType{}, newError(UndefinedFunction, name+" is not a window function")
	}
	// Resolve the window definition (PARTITION BY / ORDER BY expressions → slots, explicit frame).
	// Keys resolve in `sub` (the grouped collecting ctx — a bare grouping column → its grouped-row
	// slot and an aggregate → an agg slot, else 42803; or plain Forbidden, columns → real input
	// slots); a non-column key materializes into ag.windowKeys at a windowKeyBase+k placeholder
	// (window.md §5.1).
	partition, order, frame, err := resolveWindowDef(s, fc.Over, sub, &ag.windowKeys, params)
	if err != nil {
		return nil, resolvedType{}, err
	}
	// FILTER (WHERE cond) on a window aggregate (aggregates.md §20): a per-frame-row boolean over the
	// INPUT row, resolved with aggregates forbidden (a nested aggregate is 42803, a non-boolean 42804)
	// — exactly the non-window FILTER rule (§11). The window stage folds only the frame rows it keeps.
	var rfilter *rExpr
	if filter != nil {
		fsub := &aggCtx{collecting: false}
		rf, ft, ferr := resolve(s, *filter, nil, fsub, params)
		if ferr != nil {
			return nil, resolvedType{}, ferr
		}
		if ft.kind != rtBool && ft.kind != rtNull {
			return nil, resolvedType{}, typeError("argument of FILTER must be type boolean")
		}
		rfilter = rf
	}
	// A window function is allowed only in a window query's projection. In WHERE / a JOIN ON /
	// HAVING / an aggregate-only query ag is not windowing → 42P20 (window.md §7).
	if !ag.windowing {
		return nil, resolvedType{}, newError(WindowingError, "window functions are not allowed here")
	}
	// Write back the (possibly grown) aggregate specs collected from this call's arguments AND window
	// keys so the next window's nested aggregates continue the numbering (grouped query — window.md
	// §5.1).
	if ag.collecting {
		ag.specs = sub.specs
		ag.groupingSpecs = sub.groupingSpecs
	}
	slot := ag.windowBase + len(ag.windowSpecs)
	ag.windowSpecs = append(ag.windowSpecs, windowSpec{plan: plan, partition: partition, order: order, args: wargs, aggPlan: wagg, frame: frame, filter: rfilter})
	return &rExpr{kind: reColumn, index: slot}, result, nil
}

// windowKeySlot maps a resolved window-key expression to the slot the window stage indexes
// (spec/design/window.md §5.1). A bare column / aggregate (reColumn) keeps its real row slot — the
// input slot for a plain query, the grouped-row slot for a grouped one — so a column-only window is
// byte-identical to before. Any compound expression is materialized into *windowKeys at the
// placeholder slot windowKeyBase+k (rebased once the row layout is final). A key referencing an
// enclosing query (a correlated window — clause names it) is the deferred follow-on (0A000).
func windowKeySlot(rexpr *rExpr, clause string, windowKeys *[]*rExpr) (int, error) {
	if rexprReferencesOuter(rexpr, 0) {
		return 0, newError(FeatureNotSupported, clause+" may not reference an outer query column")
	}
	if rexpr.kind == reColumn {
		return rexpr.index, nil
	}
	k := len(*windowKeys)
	*windowKeys = append(*windowKeys, rexpr)
	return windowKeyBase + k, nil
}

// resolveWindowDef resolves the PARTITION BY and within-partition ORDER BY (→ sort keys) of an
// OVER (...) clause. Each key is a general expression (spec/design/window.md §5.1) resolved against
// keyCtx: a plain window query passes a Forbidden ctx (columns → real input slots, an aggregate is
// 42803), a grouped one passes a collecting ctx sharing the query's aggregate specs (a bare column →
// its grouping-column slot or 42803, an aggregate sum(x) collects → its agg slot). A bare-column /
// aggregate key (reColumn) keeps its real slot; any compound key is materialized into windowKeys at a
// windowKeyBase+k placeholder. A key referencing an enclosing-query column (a correlated window) is
// 0A000; a window function inside a key is rejected by keyCtx (42P20).
func resolveWindowDef(s *scope, wd *windowDef, keyCtx *aggCtx, windowKeys *[]*rExpr, params *paramTypes) ([]int, []orderSlot, *resolvedFrame, error) {
	partition := make([]int, 0, len(wd.Partition))
	for _, key := range wd.Partition {
		rexpr, _, err := resolve(s, key, nil, keyCtx, params)
		if err != nil {
			return nil, nil, nil, err
		}
		slot, err := windowKeySlot(rexpr, "PARTITION BY", windowKeys)
		if err != nil {
			return nil, nil, nil, err
		}
		partition = append(partition, slot)
	}
	order := make([]orderSlot, 0, len(wd.Order))
	// The ORDER BY key types, captured in lockstep with order — a RANGE value-offset frame folds
	// key ± offset over the single ordering key, so it needs the key's type (§6).
	orderTypes := make([]dataType, 0, len(wd.Order))
	for _, key := range wd.Order {
		rexpr, ty, err := resolve(s, key.Expr, nil, keyCtx, params)
		if err != nil {
			return nil, nil, nil, err
		}
		// The sort-key collation. An explicit trailing COLLATE (rare — parseExpr usually absorbs a
		// COLLATE into the key expression) must be on a text key (42804); otherwise the collation is
		// DERIVED from the key expression (collation.md §1) — a COLLATE inside it is explicit, a bare
		// text column is its frozen implicit collation, every other shape resets to none (C). A
		// collated window ORDER BY honors the collation in both the per-partition sort and peer
		// determination (window.md §3/§5); COLLATE "C" resolves to nil (the raw-byte fast path).
		var coll *Collation
		if key.Collation != "" {
			if ty.kind != rtText && ty.kind != rtNull {
				return nil, nil, nil, typeError(fmt.Sprintf("collations are not supported by type %s", rtName(ty)))
			}
			if coll, err = resolveCollationName(s.catalog, key.Collation); err != nil {
				return nil, nil, nil, err
			}
		} else {
			d, derr := deriveCollation(s, key.Expr)
			if derr != nil {
				return nil, nil, nil, derr
			}
			if coll, err = resolveDeriv(s.catalog, d); err != nil {
				return nil, nil, nil, err
			}
		}
		slot, err := windowKeySlot(rexpr, "window ORDER BY", windowKeys)
		if err != nil {
			return nil, nil, nil, err
		}
		order = append(order, orderSlot{idx: slot, descending: key.Descending, nullsFirst: key.NullsFirst, collation: coll})
		kt, err := typeFromResolved(ty)
		if err != nil {
			return nil, nil, nil, err
		}
		orderTypes = append(orderTypes, kt)
	}
	// The explicit frame (window.md §6): ROWS / GROUPS integer-count offsets, RANGE value offsets.
	var frame *resolvedFrame
	if wd.Frame != nil {
		f, err := resolveFrame(wd.Frame, order, orderTypes)
		if err != nil {
			return nil, nil, nil, err
		}
		frame = f
	}
	return partition, order, frame, nil
}

// resolveFrame resolves an explicit frame clause (spec/design/window.md §6). GROUPS requires an
// ORDER BY (42P20); a RANGE value offset requires exactly one ORDER BY column (42P20) of an integer,
// decimal, or float type (a timestamp/date key is the deferred D4 follow-on, any other type is
// 0A000). A negative offset is 22013. Mirrors Rust's resolve_frame.
func resolveFrame(f *windowFrame, order []orderSlot, orderTypes []dataType) (*resolvedFrame, error) {
	isOffset := func(b frameBound) bool { return b.Kind == framePreceding || b.Kind == frameFollowing }
	hasOffset := isOffset(f.Start) || isOffset(f.End)
	switch f.Mode {
	case frameRows:
		start, err := resolveIntBound(f.Start)
		if err != nil {
			return nil, err
		}
		end, err := resolveIntBound(f.End)
		if err != nil {
			return nil, err
		}
		return &resolvedFrame{mode: frameRows, start: start, end: end, exclude: f.Exclude}, nil
	case frameGroups:
		if len(order) == 0 {
			return nil, newError(WindowingError, "GROUPS mode requires an ORDER BY clause")
		}
		start, err := resolveIntBound(f.Start)
		if err != nil {
			return nil, err
		}
		end, err := resolveIntBound(f.End)
		if err != nil {
			return nil, err
		}
		return &resolvedFrame{mode: frameGroups, start: start, end: end, exclude: f.Exclude}, nil
	default: // FrameRange
		if hasOffset {
			if len(order) != 1 {
				return nil, newError(WindowingError,
					"RANGE with offset PRECEDING/FOLLOWING requires exactly one ORDER BY column")
			}
			kt := orderTypes[0]
			if !(kt.IsInteger() || kt.IsDecimal() || kt.IsFloat()) {
				return nil, newError(FeatureNotSupported, fmt.Sprintf(
					"RANGE with offset PRECEDING/FOLLOWING is not supported for column type %s", kt.CanonicalName(),
				))
			}
			start, err := resolveRangeBound(f.Start, kt)
			if err != nil {
				return nil, err
			}
			end, err := resolveRangeBound(f.End, kt)
			if err != nil {
				return nil, err
			}
			return &resolvedFrame{mode: frameRange, start: start, end: end, exclude: f.Exclude}, nil
		}
		// RANGE with only UNBOUNDED / CURRENT ROW bounds — peer/edge based, any number of ORDER BY
		// keys (or none); no key arithmetic, so it reuses the plain bound resolution.
		start, err := resolveIntBound(f.Start)
		if err != nil {
			return nil, err
		}
		end, err := resolveIntBound(f.End)
		if err != nil {
			return nil, err
		}
		return &resolvedFrame{mode: frameRange, start: start, end: end, exclude: f.Exclude}, nil
	}
}

// resolveIntBound resolves a ROWS/GROUPS frame bound: the offset of `n PRECEDING`/`n FOLLOWING` must
// be a non-negative integer literal (22013 if negative; a non-literal/non-integer offset is 0A000).
func resolveIntBound(b frameBound) (resolvedBound, error) {
	offset := func(e exprNode) (Value, error) {
		if e.Kind == exprLiteral && e.Literal != nil && e.Literal.Kind == literalInt {
			if e.Literal.Int >= 0 {
				return IntValue(e.Literal.Int), nil
			}
			return Value{}, newError(InvalidPrecedingOrFollowingSize,
				"frame starting or ending offset must not be negative")
		}
		return Value{}, newError(FeatureNotSupported, "frame offset must be a non-negative integer literal")
	}
	switch b.Kind {
	case frameUnboundedPreceding:
		return resolvedBound{kind: boundUnboundedPreceding}, nil
	case frameCurrentRow:
		return resolvedBound{kind: boundCurrentRow}, nil
	case frameUnboundedFollowing:
		return resolvedBound{kind: boundUnboundedFollowing}, nil
	case framePreceding:
		v, err := offset(b.Offset)
		if err != nil {
			return resolvedBound{}, err
		}
		return resolvedBound{kind: boundPreceding, offVal: v}, nil
	case frameFollowing:
		v, err := offset(b.Offset)
		if err != nil {
			return resolvedBound{}, err
		}
		return resolvedBound{kind: boundFollowing, offVal: v}, nil
	default:
		return resolvedBound{}, newError(FeatureNotSupported, "unsupported frame bound")
	}
}

// resolveRangeBound resolves a RANGE value-offset bound (window.md §6). The offset literal must be a
// non-negative numeric matching the ordering key type: an integer key takes an integer offset (a
// decimal offset is 0A000, matching PG); a decimal key takes an integer (widened) or decimal offset;
// a float key takes an integer or decimal offset converted to f64 (PG's in_range_float*_float8 — the
// offset is float8 for both f32 and f64 keys). The decimal→f64 conversion traps 22003 on overflow
// (jed's float-cast rule); an int offset is always finite.
func resolveRangeBound(b frameBound, kt dataType) (resolvedBound, error) {
	offset := func(e exprNode) (Value, error) {
		if e.Kind != exprLiteral || e.Literal == nil {
			return Value{}, newError(FeatureNotSupported, "frame offset must be a non-negative numeric literal")
		}
		switch e.Literal.Kind {
		case literalInt:
			if e.Literal.Int < 0 {
				return Value{}, newError(InvalidPrecedingOrFollowingSize,
					"frame starting or ending offset must not be negative")
			}
			if kt.IsFloat() {
				return Float64Value(float64(e.Literal.Int)), nil
			}
			if kt.IsDecimal() {
				return DecimalValue(decimalFromInt64(e.Literal.Int)), nil
			}
			return IntValue(e.Literal.Int), nil
		case literalDecimal:
			if e.Literal.Dec.Neg && !e.Literal.Dec.IsZero() {
				return Value{}, newError(InvalidPrecedingOrFollowingSize,
					"frame starting or ending offset must not be negative")
			}
			if kt.IsFloat() {
				f, err := decimalToFloat64(e.Literal.Dec)
				if err != nil {
					return Value{}, err
				}
				return Float64Value(f), nil
			}
			if !kt.IsDecimal() {
				return Value{}, newError(FeatureNotSupported, fmt.Sprintf(
					"RANGE with offset PRECEDING/FOLLOWING is not supported for column type %s and offset type decimal",
					kt.CanonicalName(),
				))
			}
			return DecimalValue(e.Literal.Dec), nil
		default:
			return Value{}, newError(FeatureNotSupported, "frame offset must be a non-negative numeric literal")
		}
	}
	switch b.Kind {
	case frameUnboundedPreceding:
		return resolvedBound{kind: boundUnboundedPreceding}, nil
	case frameCurrentRow:
		return resolvedBound{kind: boundCurrentRow}, nil
	case frameUnboundedFollowing:
		return resolvedBound{kind: boundUnboundedFollowing}, nil
	case framePreceding:
		v, err := offset(b.Offset)
		if err != nil {
			return resolvedBound{}, err
		}
		return resolvedBound{kind: boundPreceding, offVal: v}, nil
	case frameFollowing:
		v, err := offset(b.Offset)
		if err != nil {
			return resolvedBound{}, err
		}
		return resolvedBound{kind: boundFollowing, offVal: v}, nil
	default:
		return resolvedBound{}, newError(FeatureNotSupported, "unsupported frame bound")
	}
}

// aggArg returns the single argument of a non-star aggregate call. Each aggregate takes
// exactly one argument; a different count matches no aggregate overload and is 42883 (PG).
func aggArg(fc *funcCallExpr) (exprNode, error) {
	if len(fc.Args) != 1 {
		return exprNode{}, newError(UndefinedFunction, "no aggregate function matches the given argument count")
	}
	return *fc.Args[0], nil
}

// noAggOverload is 42883 — an aggregate over an operand family it has no overload for.
func noAggOverload(fn string) error {
	return newError(UndefinedFunction, "no "+fn+" aggregate for that argument type")
}

// noFuncOverload is 42883 — a scalar function over argument types it has no overload for.
func noFuncOverload(fn string) error {
	return newError(UndefinedFunction, "no "+fn+" function for those argument types")
}
