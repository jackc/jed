package jed

import (
	"bytes"
	"fmt"
	"slices"
	"strings"
)

func singleValuesInsertEligible(candidateCount int, hasConflict bool) bool {
	return candidateCount == 1 && !hasConflict
}

// Row mutation — INSERT / UPDATE / DELETE and ON CONFLICT (spec/design/constraints.md, upsert.md).
// This file holds secondary-index entry encoding (indexEntryKey/indexPrefixKey/gistEntries/ginEntries,
// exclusion-constraint probing), the ON CONFLICT arbiter and conflict plan (resolveArbiter/
// resolveOnConflict), executeInsert and its row engine (insertRows/runInsertRows/insertRowsOnConflict),
// executeDelete and executeUpdate (two-phase, all-or-nothing, PK re-keying), and RETURNING projection.

// resolvedKey is one index key element resolved for a statement's maintenance
// (spec/design/indexes.md §4): a plain column (Expr == nil — encoded from columns[Col].Type +
// colls[Col]) or a resolved expression (Expr != nil, carrying its rExpr, its encoding key Type,
// and its collation, evaluated against each row — unmetered — to yield the key value). Built once
// per statement by (*engine).resolveIndex from an indexDef.
type resolvedKey struct {
	Col  int        // column ordinal (column key)
	Expr *rExpr     // resolved expression (expression key)
	Ty   dataType   // result key type (expression key)
	Coll *Collation // derived collation (expression key)
}

// resolvedIndex is an index resolved for one statement's maintenance: the def's identity (name /
// unique / kind) plus its per-element resolvedKeys. Owned (no borrow of the catalog). GIN/GiST
// indexes are always plain-column (this slice), so their entry builders read the ordinals back via
// columnOrdinals.
type resolvedIndex struct {
	Name   string
	Unique bool
	Kind   indexKind
	Keys   []resolvedKey
	// Predicate is a PARTIAL index's resolved WHERE predicate (spec/design/indexes.md §9): evaluated
	// against each row (unmetered, like a key expression), a row is indexed / constrained ONLY when it
	// is TRUE. nil for an ordinary (full) index — every row is indexed.
	Predicate *rExpr
}

type insertAlwaysTarget struct{ col, pos int }

// insertValuesPlan is the immutable, resolution-derived part of a plain INSERT ... VALUES. It owns
// no session, transaction, store, pager, parameter value, meter, or statement seam. A cache hit
// reuses this structure while all dynamic gates/evaluation remain in executeInsertValuesPlan.
type insertValuesPlan struct {
	table          *catTable
	colTypes       []colType
	pk             []int
	checks         []namedCheck
	defaultExprs   []*rExpr
	rindexes       []resolvedIndex
	colls          []*Collation
	provided       []int
	arity          int
	retNodes       []*rExpr
	retNames       []string
	retTypes       []string
	ptys           []scalarType
	cacheable      bool
	alwaysTargeted []insertAlwaysTarget
}

// insertTargetSignature is schema-only: estimator revisions are deliberately absent because every
// successful INSERT advances them. database is the target snapshot/attachment identity; core keeps
// a prepared statement from crossing owning Database handles even if a target token were shared.
type insertTargetSignature struct {
	core     *sharedCore
	database *estimatorDatabaseIdentity
	catGen   uint64
	table    string
}

type insertCache struct {
	sig  insertTargetSignature
	plan *insertValuesPlan
}

func (db *engine) insertTargetSignature(dbScope *string, tableName string) (insertTargetSignature, bool) {
	table := strings.ToLower(tableName)
	var snap *snapshot
	if dbScope == nil {
		if _, ok := db.tempSnap().tableByKey(table); ok {
			return insertTargetSignature{}, false
		}
		snap = db.readSnap()
	} else {
		switch strings.ToLower(*dbScope) {
		case "temp":
			return insertTargetSignature{}, false
		case "main":
			snap = db.readSnap()
		default:
			snap = db.attachReadSnap(strings.ToLower(*dbScope))
		}
	}
	if snap == nil {
		return insertTargetSignature{}, false
	}
	if _, ok := snap.tableByKey(table); !ok {
		return insertTargetSignature{}, false
	}
	return insertTargetSignature{
		core: db.core, database: snap.estimatorIdentity, catGen: snap.catGen, table: table,
	}, true
}

// committedInsertTargetSignature is the fill guard: an explicit write transaction may publish a
// plan only while its visible target schema still exactly matches the committed base. Row writes do
// not change this signature, so the first INSERT in a long transaction can fill and the remainder
// can hit; working DDL (including unrelated catalog DDL) makes it differ and therefore cannot fill.
func (db *engine) committedInsertTargetSignature(dbScope *string, tableName string) (insertTargetSignature, bool) {
	table := strings.ToLower(tableName)
	var snap *snapshot
	if dbScope == nil {
		if _, ok := db.tempSnap().tableByKey(table); ok {
			return insertTargetSignature{}, false
		}
		snap = db.committed
	} else {
		switch strings.ToLower(*dbScope) {
		case "temp":
			return insertTargetSignature{}, false
		case "main":
			snap = db.committed
		default:
			snap = db.attachedCommitted[strings.ToLower(*dbScope)]
		}
	}
	if snap == nil {
		return insertTargetSignature{}, false
	}
	if _, ok := snap.tableByKey(table); !ok {
		return insertTargetSignature{}, false
	}
	return insertTargetSignature{
		core: db.core, database: snap.estimatorIdentity, catGen: snap.catGen, table: table,
	}, true
}

func (db *engine) insertTargetSignatureMatches(dbScope *string, want insertTargetSignature) bool {
	got, ok := db.insertTargetSignature(dbScope, want.table)
	return ok && got.core == want.core && got.database == want.database &&
		got.catGen == want.catGen && got.table == want.table
}

// columnOrdinals returns the plain-column ordinals of a GIN/GiST index (always all columns, this
// slice); it panics on an expression key (a GIN/GiST expression key is structurally impossible).
func (r *resolvedIndex) columnOrdinals() []int {
	out := make([]int, len(r.Keys))
	for i, k := range r.Keys {
		if k.Expr != nil {
			panic("GIN/GiST index keys are plain columns")
		}
		out[i] = k.Col
	}
	return out
}

// resolvedToKeyType returns the order-preserving key Type an index-expression result encodes
// under, or ok=false when the result type is not key-encodable (a composite / json / unknown
// result — 0A000 at CREATE INDEX). Every scalar is keyable (encoding.md §2); a keyable-scalar-
// element array/range is too.
func resolvedToKeyType(rt resolvedType) (dataType, bool) {
	switch rt.kind {
	case rtInt:
		return scalarT(rt.intTy), true
	case rtFloat32:
		return scalarT(scalarFloat32), true
	case rtFloat64:
		return scalarT(scalarFloat64), true
	case rtBool:
		return scalarT(scalarBool), true
	case rtText:
		return scalarT(scalarText), true
	case rtDecimal:
		return scalarT(scalarDecimal), true
	case rtBytea:
		return scalarT(scalarBytea), true
	case rtUuid:
		return scalarT(scalarUuid), true
	case rtTimestamp:
		return scalarT(scalarTimestamp), true
	case rtTimestamptz:
		return scalarT(scalarTimestamptz), true
	case rtInterval:
		return scalarT(scalarInterval), true
	case rtDate:
		return scalarT(scalarDate), true
	case rtArray:
		et, ok := resolvedToKeyType(*rt.elem)
		if !ok || !et.isScalar() || !isKeyableScalarType(et.Scalar) {
			return dataType{}, false // a composite-element array is not keyable
		}
		return dataType{Array: &et}, true
	case rtRange:
		et, ok := resolvedToKeyType(*rt.elem)
		if !ok {
			return dataType{}, false
		}
		return dataType{Range: &et}, true
	default: // rtNull, rtComposite, rtJson, rtJsonb, rtJsonPath
		return dataType{}, false
	}
}

// indexKeySlot returns one key element's value + encoding type + collation for a row: the column
// value (a column key) or the evaluated expression (an expression key — unmetered, env for the
// immutable eval). Index maintenance is unmetered (cost.md §3), so a throwaway meter absorbs the
// eval charge.
func indexKeySlot(key resolvedKey, columns []catColumn, colls []*Collation, row storedRow, env *evalEnv) (Value, dataType, *Collation, error) {
	if key.Expr == nil {
		return row[key.Col], columns[key.Col].Type, colls[key.Col], nil
	}
	v, err := key.Expr.eval(row, env, newMeter()) // maintenance eval is unmetered (cost.md §3)
	if err != nil {
		return Value{}, dataType{}, nil, err
	}
	return v, key.Ty, key.Coll, nil
}

// indexEntryKey builds a secondary-index entry key (spec/design/indexes.md §3): each key element
// as the encoding.md §2.2 nullable slot — 0x00 + the type's bare order-preserving key bytes when
// present, the lone 0x01 for NULL (always tagged, even for a NOT NULL column) — then the row's
// storage key as the suffix. A column key's value is always resident (a fixed-width type never
// spills, and a spillable text/bytea would over-fill the entry key, rejected 0A000 at its insert);
// an expression key evaluates against the row (§4), faulting a referenced spilled value in through
// the evaluator's Unfetched backstop.
func indexEntryKey(columns []catColumn, colls []*Collation, rindex *resolvedIndex, storageKey []byte, row storedRow, env *evalEnv) ([]byte, error) {
	var out []byte
	for _, key := range rindex.Keys {
		val, ty, coll, err := indexKeySlot(key, columns, colls, row, env)
		if err != nil {
			return nil, err
		}
		if val.Kind == ValNull {
			out = append(out, 0x01)
			continue
		}
		// present tag, then the type's order-preserving key (range-aware §2.11, collated-text-aware §2.12)
		b, err := encodeTypedKey(ty, val, coll)
		if err != nil {
			return nil, err
		}
		out = append(out, 0x00)
		out = append(out, b...)
	}
	out = append(out, storageKey...)
	return out, nil
}

// indexRowQualifies reports whether a row is indexed by rindex (spec/design/indexes.md §9): always
// for an ordinary index, and for a PARTIAL index iff its WHERE predicate evaluates to TRUE (the 3VL
// WHERE rule — FALSE and NULL are excluded). The predicate eval is unmetered maintenance work (like
// a key expression's — cost.md §3), so a throwaway meter absorbs its charge.
func indexRowQualifies(rindex *resolvedIndex, row storedRow, env *evalEnv) (bool, error) {
	if rindex.Predicate == nil {
		return true, nil
	}
	v, err := rindex.Predicate.eval(row, env, newMeter())
	if err != nil {
		return false, err
	}
	return v.IsTrue(), nil
}

// indexEntryKeys returns the index entries a row contributes (spec/design/gin.md §4/§5): exactly
// one for an ordered (B-tree) index — the §3 nullable-slot entry key — or one per DISTINCT non-NULL
// element for a GIN index. Every write path (build, INSERT, DELETE, UPDATE) treats an index
// uniformly as "a row maps to a set of entries." A PARTIAL index contributes the EMPTY set for a row
// whose predicate is not TRUE (spec/design/indexes.md §9), which is what makes INSERT/DELETE/UPDATE
// maintenance uniform (the UPDATE old-set/new-set diff handles a row entering/leaving/moving for
// free). colls (column-ordinal-indexed) selects each text key column's collated form (§2.12); GIN
// elements are fixed-width, so a GIN index never collates.
func indexEntryKeys(columns []catColumn, colls []*Collation, rindex *resolvedIndex, storageKey []byte, row storedRow, env *evalEnv) ([][]byte, error) {
	if ok, err := indexRowQualifies(rindex, row, env); err != nil {
		return nil, err
	} else if !ok {
		return nil, nil // partial index: a non-qualifying row contributes no entry
	}
	if rindex.Kind == indexGin {
		return ginEntries(columns, rindex.columnOrdinals(), storageKey, row), nil
	}
	if rindex.Kind == indexGist {
		return gistEntries(columns, rindex.columnOrdinals(), storageKey, row), nil
	}
	ek, err := indexEntryKey(columns, colls, rindex, storageKey, row, env)
	if err != nil {
		return nil, err
	}
	return [][]byte{ek}, nil
}

// indexEntryKeysColumns returns the entry keys for a COLUMN-ONLY index, without an eval env
// (spec/design/indexes.md §4) — the collation-realign rebuild path, which runs on a snapshot with
// no engine to evaluate an expression key. An expression index is C-collated (its keys never
// change on a collation upgrade), so it is fail-closed 0A000 there; this asserts the index is
// plain-column and reuses the ordinary builder with a nil env (a column key never touches env).
func indexEntryKeysColumns(columns []catColumn, colls []*Collation, def indexDef, storageKey []byte, row storedRow) ([][]byte, error) {
	cols := def.columnOrdinals()
	if cols == nil {
		panic("indexEntryKeysColumns called on an expression index")
	}
	keys := make([]resolvedKey, len(cols))
	for i, c := range cols {
		keys[i] = resolvedKey{Col: c}
	}
	ri := &resolvedIndex{Name: def.Name, Unique: def.Unique, Kind: def.Kind, Keys: keys}
	return indexEntryKeys(columns, colls, ri, storageKey, row, nil)
}

// gistEntries builds a GiST index's entry keys for one row (spec/design/gist.md §4.1): exactly one
// leaf key, encodeRangeBody(bound) ‖ storage_key (the GIN term ‖ skey pattern), so all existing
// index maintenance (insert/update/delete) reuses it unchanged. A NULL range value is not indexed;
// the empty range is a real value and IS indexed. cols is the index's plain-column ordinals.
func gistEntries(columns []catColumn, cols []int, storageKey []byte, row storedRow) [][]byte {
	ops := make([]gistOpclass, len(cols))
	bound := make([]gistBound, len(cols))
	for i, ci := range cols {
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
func ginEntries(columns []catColumn, cols []int, storageKey []byte, row storedRow) [][]byte {
	ci := cols[0]
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
func indexPrefixKey(columns []catColumn, colls []*Collation, rindex *resolvedIndex, row storedRow, env *evalEnv) ([]byte, bool, error) {
	// A partial index constrains only its qualifying rows (indexes.md §9): a non-qualifying row is
	// exempt from uniqueness, exactly like a NULL-bearing prefix (ok=false).
	if ok, err := indexRowQualifies(rindex, row, env); err != nil {
		return nil, false, err
	} else if !ok {
		return nil, false, nil
	}
	var out []byte
	for _, key := range rindex.Keys {
		val, ty, coll, err := indexKeySlot(key, columns, colls, row, env)
		if err != nil {
			return nil, false, err
		}
		if val.Kind == ValNull {
			return nil, false, nil
		}
		// present tag, then the type's order-preserving key (range-aware §2.11, collated-text-aware §2.12)
		b, err := encodeTypedKey(ty, val, coll)
		if err != nil {
			return nil, false, err
		}
		out = append(out, 0x00)
		out = append(out, b...)
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
			// A conflict-target COLUMN list matches only a plain-column unique index (an expression
			// unique index is arbitrated by ON CONSTRAINT <name> — upsert.md §3). A PARTIAL unique
			// index is NOT matched by a bare column list (PostgreSQL requires the predicate to be
			// restated — a deferred upsert follow-on, indexes.md §9): so a column target that only a
			// partial index covers reports "no matching arbiter", agreeing with PG.
			if def.Unique && def.Predicate == nil {
				if cols := def.columnOrdinals(); cols != nil && sameIntSet(cols, want) {
					return &arbiter{indexPos: i}, nil
				}
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
func arbiterKey(arb *arbiter, table *catTable, pk []int, colls []*Collation, rindexes []resolvedIndex, row storedRow, env *evalEnv) ([]byte, bool, error) {
	if arb.isPK {
		k, err := encodePkKey(table, pk, colls, row)
		if err != nil {
			return nil, false, err
		}
		return k, true, nil
	}
	return indexPrefixKey(table.Columns, colls, &rindexes[arb.indexPos], row, env)
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
func (db *engine) rowConflictsCommitted(store *tableStore, table *catTable, pk []int, colls []*Collation, rindexes []resolvedIndex, row storedRow) (bool, error) {
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
	for i := range rindexes {
		rindex := &rindexes[i]
		if !rindex.Unique {
			continue
		}
		prefix, ok, err := db.indexPrefix(table.Columns, colls, rindex, row)
		if err != nil {
			return false, err
		}
		if !ok {
			continue
		}
		entries, err := db.lkpIndexStore(strings.ToLower(rindex.Name)).RangeEntries(uniqueProbeBound(prefix))
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
	return db.executeInsertCached(ins, params, ctx, nil, false)
}

func (db *engine) executeInsertCached(ins *insert, params []Value, ctx cteCtx, ic *insertStmtCache, allowCacheFill bool) (outcome, error) {
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
	// Slice 2 caches only a top-level plain VALUES disposition. Writable CTE children reach this
	// method without a cache slot; SELECT and ON CONFLICT retain the existing general path.
	if ins.Select == nil && ins.OnConflict == nil {
		return db.executeInsertValuesCached(ins, params, ctx, ic, allowCacheFill)
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
		db.markEstimatorMutation(ins.DB, ins.Table)
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
	useSingle := singleValuesInsertEligible(len(ins.Rows), cplan != nil)
	var singleValues []Value
	rowCap := len(ins.Rows)
	if useSingle {
		rowCap = 0
	}
	rows := make([][]Value, 0, rowCap)
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
				} else if ct := store.colTypes[i]; ct.Elem == nil && ct.RangeElem == nil && !ct.Composite &&
					ct.Scalar == scalarDate && !iv.IsParam && !iv.IsArray && !iv.IsRow &&
					iv.Lit.Kind == literalText && dateClockIsSpecial(iv.Lit.Str) {
					// A date-special string in a VALUES slot is LITERAL adaptation (date.md §6):
					// 'epoch' is the constant 1970-01-01, and a clock-relative word
					// ('today'/'now'/…) names the statement-clock day in the session zone,
					// computed here through the shared stmtRng — never a stored constant. An
					// ordinary date string takes the normal materialize path (parse to a value);
					// non-literal text DATA (INSERT … SELECT, a $N bind) stays strict — the
					// specials are literal/cast syntax, not an assignment coercion.
					off, epoch, _ := dateClockSpecial(iv.Lit.Str)
					if epoch {
						rv[p] = DateValue(0)
					} else {
						dv, err := dateClockValue(db, stmtRng, meter, int64(off))
						if err != nil {
							return outcome{}, err
						}
						rv[p] = dv
					}
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
		if useSingle {
			singleValues = rv
		} else {
			rows = append(rows, rv)
		}
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
	var affected int64
	var returned [][]Value
	if useSingle {
		returned, err = db.insertOne(table, store, ins.DB, pk, checks, defaultExprs, nil, nil, stmtRng, provided, singleValues, retNodes, bound, ctx, meter)
		affected = 1
	} else {
		affected, returned, err = db.runInsertRows(table, store, ins.DB, pk, checks, defaultExprs, stmtRng, provided, rows, cplan, retNodes, bound, ctx, meter)
	}
	if err != nil {
		return outcome{}, err
	}
	db.markEstimatorMutation(ins.DB, ins.Table)
	return dmlOutcome(retNames, retTypes, returned, affected, meter.Accrued), nil
}

// executeInsertValuesCached resolves or reuses the immutable part of a plain INSERT ... VALUES,
// then performs all per-execution binding, gates, default/constraint/index/RETURNING evaluation and
// writes against the current working snapshot. Planning is unmetered; the execution below is the
// same one-row/batch path used before the cache existed.
func (db *engine) executeInsertValuesCached(ins *insert, params []Value, ctx cteCtx, ic *insertStmtCache, allowCacheFill bool) (outcome, error) {
	if ic != nil {
		if cached := ic.p.Load(); cached != nil && db.insertTargetSignatureMatches(ins.DB, cached.sig) {
			if err := db.ensureCollationsWritable(cached.plan.table.Columns); err != nil {
				return outcome{}, err
			}
			return db.executeInsertValuesPlan(ins, params, ctx, cached.plan, false)
		}
	}

	plan, err := db.resolveInsertValuesPlan(ins, ctx)
	if err != nil {
		return outcome{}, err
	}
	sig, hasSig := db.insertTargetSignature(ins.DB, ins.Table)
	out, err := db.executeInsertValuesPlan(ins, params, ctx, plan, !plan.cacheable)
	if err != nil {
		return outcome{}, err
	}
	committedSig, committedOK := db.committedInsertTargetSignature(ins.DB, ins.Table)
	if ic != nil && allowCacheFill && hasSig && committedOK && plan.cacheable &&
		sig.core == committedSig.core && sig.database == committedSig.database &&
		sig.catGen == committedSig.catGen && sig.table == committedSig.table {
		ic.p.Store(&insertCache{sig: committedSig, plan: plan})
	}
	return out, nil
}

func (db *engine) resolveInsertValuesPlan(ins *insert, ctx cteCtx) (*insertValuesPlan, error) {
	table, ok := db.lkpTableScoped(ins.DB, ins.Table)
	if !ok {
		return nil, newError(UndefinedTable, "table does not exist: "+ins.Table)
	}
	if err := db.ensureCollationsWritable(table.Columns); err != nil {
		return nil, err
	}
	store := db.lkpStoreScoped(ins.DB, ins.Table)
	ptypes := &paramTypes{}
	checks, err := db.resolveChecksWithParams(table, ptypes)
	if err != nil {
		return nil, err
	}
	defaultExprs, err := db.resolveDefaultExprsWithParams(table, ptypes)
	if err != nil {
		return nil, err
	}
	rindexes, err := db.resolveTableIndexesWithParams(table, ptypes)
	if err != nil {
		return nil, err
	}

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
				return nil, newError(UndefinedColumn, fmt.Sprintf(
					"column %s of relation %s does not exist", name, table.Name,
				))
			}
			if provided[idx] >= 0 {
				return nil, newError(DuplicateColumn,
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

	if ins.Overriding != nil && *ins.Overriding == overridingUser {
		for i, col := range table.Columns {
			if col.Identity != nil {
				provided[i] = -1
			}
		}
	}
	var alwaysTargeted []insertAlwaysTarget
	if !(ins.Overriding != nil && *ins.Overriding == overridingSystem) {
		for i, col := range table.Columns {
			if col.Identity != nil && *col.Identity == identityAlways && provided[i] >= 0 {
				alwaysTargeted = append(alwaysTargeted, insertAlwaysTarget{col: i, pos: provided[i]})
			}
		}
	}

	for _, values := range ins.Rows {
		if len(values) != arity {
			expected := "columns are"
			if ins.Columns != nil {
				expected = "target columns are"
			}
			return nil, newError(SyntaxError, fmt.Sprintf(
				"INSERT row has %d values but %d %s expected for table %s",
				len(values), arity, expected, table.Name,
			))
		}
		for i, col := range table.Columns {
			if p := provided[i]; p >= 0 {
				if iv := values[p]; iv.IsParam && !col.Type.IsComposite() {
					ct := col.Type.ScalarTy()
					if err := ptypes.note(int(iv.Param)-1, &ct); err != nil {
						return nil, err
					}
				}
			}
		}
	}
	for _, at := range alwaysTargeted {
		for _, values := range ins.Rows {
			if !values[at.pos].IsDefault {
				return nil, newError(GeneratedAlways, fmt.Sprintf(
					"cannot insert a non-DEFAULT value into column %s", table.Columns[at.col].Name,
				))
			}
		}
	}

	var retNodes []*rExpr
	var retNames []string
	var retTypes []string
	if ins.Returning != nil {
		if retNodes, retNames, retTypes, err = db.resolveReturning(table, *ins.Returning, false, ctx.bindings, ptypes); err != nil {
			return nil, err
		}
	}
	ptys, err := ptypes.finalize()
	if err != nil {
		return nil, err
	}
	return &insertValuesPlan{
		table:          table,
		colTypes:       append([]colType(nil), store.colTypes...),
		pk:             table.PKIndices(),
		checks:         checks,
		defaultExprs:   defaultExprs,
		rindexes:       rindexes,
		colls:          db.columnCollations(table.Columns),
		provided:       provided,
		arity:          arity,
		retNodes:       retNodes,
		retNames:       retNames,
		retTypes:       retTypes,
		ptys:           ptys,
		cacheable:      !ptypes.uncacheable,
		alwaysTargeted: alwaysTargeted,
	}, nil
}

func (db *engine) executeInsertValuesPlan(ins *insert, params []Value, ctx cteCtx, plan *insertValuesPlan, foldReturning bool) (outcome, error) {
	bound, err := bindParams(params, plan.ptys)
	if err != nil {
		return outcome{}, err
	}
	store := db.writeStoreScoped(ins.DB, ins.Table)
	stmtRng := newStmtRng()
	meter := db.session.newMeter()
	useSingle := len(ins.Rows) == 1
	var singleValues []Value
	rowCap := len(ins.Rows)
	if useSingle {
		rowCap = 0
	}
	rows := make([][]Value, 0, rowCap)
	for _, values := range ins.Rows {
		rv := make([]Value, plan.arity)
		for i, col := range plan.table.Columns {
			if p := plan.provided[i]; p >= 0 {
				iv := values[p]
				if iv.IsDefault {
					dv, err := db.evalDefault(col, plan.defaultExprs[i], stmtRng, meter)
					if err != nil {
						return outcome{}, err
					}
					rv[p] = dv
				} else if ct := plan.colTypes[i]; ct.Elem == nil && ct.RangeElem == nil && !ct.Composite &&
					ct.Scalar == scalarDate && !iv.IsParam && !iv.IsArray && !iv.IsRow &&
					iv.Lit.Kind == literalText && dateClockIsSpecial(iv.Lit.Str) {
					off, epoch, _ := dateClockSpecial(iv.Lit.Str)
					if epoch {
						rv[p] = DateValue(0)
					} else {
						dv, err := dateClockValue(db, stmtRng, meter, int64(off))
						if err != nil {
							return outcome{}, err
						}
						rv[p] = dv
					}
				} else {
					mv, err := materializeInsertValue(iv, plan.colTypes[i], bound)
					if err != nil {
						return outcome{}, err
					}
					rv[p] = mv
				}
			}
		}
		if useSingle {
			singleValues = rv
		} else {
			rows = append(rows, rv)
		}
	}
	if foldReturning {
		for _, node := range plan.retNodes {
			if err := db.foldUncorrelatedInRExpr(node, bound, ctx, &meter.Accrued); err != nil {
				return outcome{}, err
			}
		}
	}
	var affected int64
	var returned [][]Value
	if useSingle {
		returned, err = db.insertOne(plan.table, store, ins.DB, plan.pk, plan.checks, plan.defaultExprs,
			plan.rindexes, plan.colls, stmtRng, plan.provided, singleValues, plan.retNodes, bound, ctx, meter)
		affected = 1
	} else {
		returned, err = db.insertRows(plan.table, store, ins.DB, plan.pk, plan.checks, plan.defaultExprs,
			plan.rindexes, plan.colls, stmtRng, plan.provided, rows, plan.retNodes, bound, ctx, meter)
		affected = int64(len(rows))
	}
	if err != nil {
		return outcome{}, err
	}
	db.markEstimatorMutation(ins.DB, ins.Table)
	return dmlOutcome(plan.retNames, plan.retTypes, returned, affected, meter.Accrued), nil
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
func (db *engine) insertRows(table *catTable, store *tableStore, dbScope *string, pk []int, checks []namedCheck, defaultExprs []*rExpr, rindexes []resolvedIndex, colls []*Collation, rng *stmtRng, provided []int, rows [][]Value, returning []*rExpr, params []Value, ctes cteCtx, meter *costMeter) ([][]Value, error) {
	n := len(table.Columns)
	// The eval env for phase-1 index-expression evaluation (index eval is unmetered; params/CTEs
	// are empty — an index expression cannot reference them). The per-statement rng is shared with
	// the CHECK / default evaluation.
	idxEnv := &evalEnv{exec: db, rng: rng}
	type preparedRow struct {
		key []byte // nil for a no-PK table (rowid allocated in phase 2)
		row storedRow
		// prefixes are the row's per-index entry keys WITHOUT the storage-key suffix (one sub-slice
		// per index, rindexes order), computed with the eval env in phase 1 (so an expression-key
		// eval error aborts before any write). Phase 2 appends the row's final storage key (the
		// rowid, allocated there) to each — entry = prefix ‖ key is byte-identical to building it
		// with the key directly (the suffix is always the tail — indexes.md §3).
		prefixes [][][]byte
	}
	prepared := make([]preparedRow, 0, len(rows))
	seenKeys := make(map[string]struct{})
	// Per UNIQUE index (catalog/name order), the prefixes earlier rows of this batch
	// claimed — an in-batch duplicate traps 23505 like a stored one (indexes.md §8).
	var uniq []*resolvedIndex
	for i := range rindexes {
		if rindexes[i].Unique {
			uniq = append(uniq, &rindexes[i])
		}
	}
	seenPrefixes := make([]map[string]struct{}, len(uniq))
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
				// Stamp the target relation onto a column-store failure (23502/22003/22001) —
				// in scope here, not inside the coercion (spec/design/error-fields.md §4).
				return nil, stampTable(err, table.Name)
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
				return nil, newUniqueViolation(table.Name, pkeyName(table.Name))
			}
			// The duplicate probe reads the pin (readSnap) — under the writable-CTE read pin
			// (writable-cte.md §2) it sees the PRE-statement table, not an earlier sub-statement's
			// staged rows; a cross-sub-statement key collision is caught in phase 2 below instead.
			// readSnap == working for an ordinary INSERT, so this is unchanged there.
			if _, exists, err := db.lkpStoreScoped(dbScope, table.Name).Get(key); err != nil {
				return nil, err
			} else if exists {
				return nil, newUniqueViolation(table.Name, pkeyName(table.Name))
			}
			seenKeys[string(key)] = struct{}{}
		}
		// UNIQUE-index probes (indexes.md §8), AFTER the primary-key duplicate check (PG
		// reports the PK first when both are violated — probed): per unique index in
		// catalog (name) order, a fully-non-NULL key tuple (its slot prefix) must match no
		// existing entry and no earlier row of this batch. Unmetered validation, like the
		// PK duplicate check (cost.md §3).
		for u, rindex := range uniq {
			prefix, ok, err := indexPrefixKey(table.Columns, colls, rindex, row, idxEnv)
			if err != nil {
				return nil, err
			}
			if !ok {
				continue
			}
			istore := db.lkpIndexStoreScoped(dbScope, strings.ToLower(rindex.Name))
			stored, err := istore.RangeEntries(uniqueProbeBound(prefix))
			if err != nil {
				return nil, err
			}
			if _, dup := seenPrefixes[u][string(prefix)]; dup || len(stored) > 0 {
				return nil, newUniqueViolation(table.Name, rindex.Name)
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
		// Compute this row's per-index entry keys WITHOUT the suffix (phase 1 — evaluates every
		// expression key, so an eval error aborts here before any write; unmetered). Phase 2
		// appends the final storage key.
		rowPrefixes := make([][][]byte, len(rindexes))
		for k := range rindexes {
			eks, err := indexEntryKeys(table.Columns, colls, &rindexes[k], nil, row, idxEnv)
			if err != nil {
				return nil, err
			}
			rowPrefixes[k] = eks
		}
		prepared = append(prepared, preparedRow{key: key, row: row, prefixes: rowPrefixes})
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
			if err := db.validateInsertFKStored(relation, fk, probe); err != nil {
				return nil, err
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
			for _, pr := range prepared {
				if db.insertExclusionConflictsStored(tcols, exc, pr.row) {
					return nil, newExclusionViolation(table.Name, exc.Name)
				}
			}
			for i := range prepared {
				for j := 0; j < i; j++ {
					if exclusionPairConflicts(tcols, exc, prepared[i].row, prepared[j].row) {
						return nil, newExclusionViolation(table.Name, exc.Name)
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
	// (spec/fileformat/format.md, spec/design/grammar.md §12). Append the final storage key to
	// each phase-1 entry prefix (byte-identical to building the entry with the key directly —
	// indexes.md §3/§4), then write the entries after the rows (an index write cannot fail, so
	// all-or-nothing is unchanged).
	indexInserts := make([][][]byte, len(table.Indexes))
	for _, pr := range prepared {
		key := pr.key
		if key == nil {
			key = encodeInt(scalarInt64, store.AllocRowid())
		}
		for k := range table.Indexes {
			for _, p := range pr.prefixes[k] {
				ek := append(append([]byte{}, p...), key...)
				indexInserts[k] = append(indexInserts[k], ek)
			}
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
			return nil, newUniqueViolation(table.Name, pkeyName(table.Name))
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
				return nil, newUniqueViolation(table.Name, def.Name)
			}
		}
	}
	return returned, nil
}

// insertOne is the plain one-candidate INSERT ... VALUES specialization. It retains the general
// path's validation and write order (constraints.md §7) but needs no within-batch maps, stringified
// byte keys, prepared-row slice, or per-index write buffers.
func (db *engine) insertOne(table *catTable, store *tableStore, dbScope *string, pk []int, checks []namedCheck, defaultExprs []*rExpr, rindexes []resolvedIndex, colls []*Collation, rng *stmtRng, provided []int, values []Value, returning []*rExpr, params []Value, ctes cteCtx, meter *costMeter) ([][]Value, error) {
	n := len(table.Columns)
	var err error
	env := &evalEnv{exec: db, rng: rng}

	row := make(storedRow, n)
	for i, col := range table.Columns {
		var candidate Value
		if p := provided[i]; p >= 0 {
			candidate = values[p]
		} else {
			candidate, err = db.evalDefault(col, defaultExprs[i], rng, meter)
			if err != nil {
				return nil, err
			}
		}
		row[i], err = coerceForStore(candidate, store.colTypes[i], col.Decimal, col.VarcharLen, col.NotNull, col.Name)
		if err != nil {
			return nil, stampTable(err, table.Name)
		}
	}

	if len(checks) > 0 {
		if err := meter.Guard(); err != nil {
			return nil, err
		}
		if err := evalChecks(checks, table.Name, row, env, meter); err != nil {
			return nil, err
		}
	}

	var key []byte
	if len(pk) > 0 {
		key, err = encodePkKey(table, pk, colls, row)
		if err != nil {
			return nil, err
		}
		if _, exists, err := db.lkpStoreScoped(dbScope, table.Name).Get(key); err != nil {
			return nil, err
		} else if exists {
			return nil, newUniqueViolation(table.Name, pkeyName(table.Name))
		}
	}

	for i := range rindexes {
		rindex := &rindexes[i]
		if !rindex.Unique {
			continue
		}
		prefix, ok, err := indexPrefixKey(table.Columns, colls, rindex, row, env)
		if err != nil {
			return nil, err
		}
		if !ok {
			continue
		}
		stored, err := db.lkpIndexStoreScoped(dbScope, strings.ToLower(rindex.Name)).RangeEntries(uniqueProbeBound(prefix))
		if err != nil {
			return nil, err
		}
		if len(stored) > 0 {
			return nil, newUniqueViolation(table.Name, rindex.Name)
		}
	}

	kb := key
	var placeholder [8]byte
	if kb == nil {
		kb = placeholder[:]
	}
	cunits := int64(store.WriteCompressUnits(kb, row))

	// One group per index remains necessary because one GIN row can produce several entries. The
	// batch-only outer row dimension is gone, and these owned prefixes become final entries in place.
	indexEntries := make([][][]byte, len(rindexes))
	for i := range rindexes {
		indexEntries[i], err = indexEntryKeys(table.Columns, colls, &rindexes[i], nil, row, env)
		if err != nil {
			return nil, err
		}
	}

	relation := table.Name
	for fki := range table.ForeignKeys {
		fk := &table.ForeignKeys[fki]
		parent, ok := db.Table(fk.RefTable)
		if !ok {
			continue
		}
		parentColls := db.columnCollations(parent.Columns)
		var supplied fkProbe
		suppliedOK := false
		if strings.EqualFold(fk.RefTable, relation) {
			supplied, suppliedOK, err = buildFkProbe(fk, parent, parentColls, row, fk.RefColumns)
			if err != nil {
				return nil, err
			}
		}
		probe, probeOK, err := buildFkProbe(fk, parent, parentColls, row, fk.Columns)
		if err != nil {
			return nil, err
		}
		if !probeOK {
			continue
		}
		if suppliedOK && bytes.Equal(supplied.bytes, probe.bytes) {
			continue
		}
		if err := db.validateInsertFKStored(relation, fk, probe); err != nil {
			return nil, err
		}
	}

	for _, exc := range table.Exclusions {
		if db.insertExclusionConflictsStored(table.Columns, exc, row) {
			return nil, newExclusionViolation(table.Name, exc.Name)
		}
	}

	meter.Charge(costs.ValueCompress * cunits)
	if err := meter.Guard(); err != nil {
		return nil, err
	}

	var returned [][]Value
	if returning != nil {
		returned, err = db.projectReturning(returning, []storedRow{row}, nil, params, ctes, meter)
		if err != nil {
			return nil, err
		}
	}

	if key == nil {
		key = encodeInt(scalarInt64, store.AllocRowid())
	}
	for i := range indexEntries {
		for j := range indexEntries[i] {
			indexEntries[i][j] = append(indexEntries[i][j], key...)
		}
	}
	inserted, err := store.Insert(key, row)
	if err != nil {
		return nil, err
	}
	if !inserted {
		return nil, newUniqueViolation(table.Name, pkeyName(table.Name))
	}
	for i, def := range table.Indexes {
		istore := db.writeIndexStoreScoped(dbScope, strings.ToLower(def.Name))
		for _, entry := range indexEntries[i] {
			inserted, err := istore.Insert(entry, nil)
			if err != nil {
				return nil, err
			}
			if !inserted {
				return nil, newUniqueViolation(table.Name, def.Name)
			}
		}
	}
	return returned, nil
}

// validateInsertFKStored completes one child-side FK check after the caller handles the statement
// end-state match. The batch and one-row paths share the stored probe and violation construction.
func (db *engine) validateInsertFKStored(relation string, fk *foreignKey, probe fkProbe) error {
	hit, err := db.fkProbeHits(probe, fk.RefTable)
	if err != nil {
		return err
	}
	if !hit {
		return newFKViolationInsert(relation, fk.Name)
	}
	return nil
}

// insertExclusionConflictsStored probes the resident GiST rows for one candidate. The batch caller
// separately retains its pairwise end-state pass; the stored-row semantics are shared.
func (db *engine) insertExclusionConflictsStored(columns []catColumn, exc exclusionConstraint, row storedRow) bool {
	query, strats, ok := exclusionProbeQuery(columns, exc, row)
	if !ok {
		return false
	}
	tree := db.readSnap().gistTreeFor(strings.ToLower(exc.Index))
	if tree == nil {
		return false
	}
	hits, _, _ := tree.search(query, strats)
	return len(hits) > 0
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
	rindexes, err := db.resolveTableIndexes(table)
	if err != nil {
		return 0, nil, err
	}
	returned, err := db.insertRows(table, store, dbScope, pk, checks, defaultExprs, rindexes,
		db.columnCollations(table.Columns), rng, provided, rows, returning, params, ctes, meter)
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
	// Resolve the indexes once (column ordinals + resolved expression keys — indexes.md §4),
	// parallel to table.Indexes; every uniqueness probe / entry build below evaluates any
	// expression key through it (unmetered, via db.indexPrefix / db.indexEntries).
	rindexes, err := db.resolveTableIndexes(table)
	if err != nil {
		return 0, nil, err
	}
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
				return 0, nil, stampTable(err, relation)
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
			committed, err := db.rowConflictsCommitted(store, table, pk, colls, rindexes, row)
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
					prefix, ok, err := db.indexPrefix(table.Columns, colls, &rindexes[ix], row)
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
				prefix, ok, err := db.indexPrefix(table.Columns, colls, &rindexes[ix], row)
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
		ak, ok, err := db.arbiterProbeKey(plan.arb, table, pk, colls, rindexes, row)
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
				return 0, nil, stampTable(err, relation)
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
				return 0, nil, newUniqueViolation(relation, pkeyName(relation))
			}
			if _, dup := seen[string(k)]; dup {
				return 0, nil, newUniqueViolation(relation, pkeyName(relation))
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
			rindex := &rindexes[ix]
			istore := db.lkpIndexStore(strings.ToLower(def.Name))
			batch := make(map[string]struct{})
			for _, newRow := range newRows {
				prefix, ok, err := db.indexPrefix(table.Columns, colls, rindex, newRow)
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
					return 0, nil, newUniqueViolation(table.Name, def.Name)
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
				return 0, nil, newFKViolationInsert(relation, fk.Name)
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
					return 0, nil, newFKViolationDelete(parent.Name, r.fk.Name, r.childTable)
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

	// Precompute all secondary-index entries in a &db pass BEFORE the writes (evaluating every
	// expression key — an error aborts before any write): each inserted row's entry PREFIXES
	// (empty suffix; phase 2 appends its storage key — the rowid is allocated there) and each
	// updated row's old/new entry sets (keys already known).
	insertPrefixes := make([][][][]byte, len(inserts))
	for i, row := range inserts {
		rp := make([][][]byte, len(rindexes))
		for k := range rindexes {
			eks, err := db.indexEntries(table.Columns, colls, &rindexes[k], nil, row)
			if err != nil {
				return 0, nil, err
			}
			rp[k] = eks
		}
		insertPrefixes[i] = rp
	}
	type indexMove struct{ removals, insertions [][]byte }
	indexMoves := make([][]indexMove, len(table.Indexes))
	for _, u := range updates {
		for k := range rindexes {
			oldEks, err := db.indexEntries(table.Columns, colls, &rindexes[k], u.key, u.oldRow)
			if err != nil {
				return 0, nil, err
			}
			newEks, err := db.indexEntries(table.Columns, colls, &rindexes[k], u.key, u.newRow)
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

	// Phase 2 — every row validated. Insert the new rows (rowid alloc for a no-PK table; append
	// each row's storage key to its precomputed entry prefixes), then replace the updated rows.
	indexAdds := make([][][]byte, len(table.Indexes))
	for i, row := range inserts {
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
		for k := range table.Indexes {
			for _, p := range insertPrefixes[i][k] {
				ek := append(append([]byte{}, p...), key...)
				indexAdds[k] = append(indexAdds[k], ek)
			}
		}
		ok, err := store.Insert(key, row)
		if err != nil {
			return 0, nil, err
		}
		if !ok {
			panic("pre-validated INSERT key must be unique")
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
func (db *engine) resolveReturning(table *catTable, returning returningClause, baseIsOld bool, ctes []*cteBinding, ptypes *paramTypes) ([]*rExpr, []string, []string, error) {
	s, err := returningScope(db, table, baseIsOld, &returning)
	if err != nil {
		return nil, nil, nil, err
	}
	s.ctes = ctes
	nodes, names, types, err := resolveProjections(s, returning.Items, &aggCtx{collecting: false}, ptypes)
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
	// Plan and execute the target scan through the shared mutation access-path seam. The plan is
	// selected after uncorrelated folding, matching the old inline detector timing; the batch keeps
	// storage keys for phase 2 and reports the same up-front units as before.
	scanPlan := db.planMutationScan(del.DB, table, filter)
	scanBefore := meter.Accrued
	batch, err := db.executeMutationScan(scanPlan, del.Table, bound, env, meter, mask)
	if err != nil {
		return outcome{}, err
	}
	scanActual := meter.Accrued - scanBefore
	if batch.empty {
		db.explainActual.record("Scan "+del.Table, scanActual)
		if filter != nil {
			db.explainActual.record("Filter", scanActual)
		}
		return dmlOutcome(retNames, retTypes, nil, 0, meter.Accrued), nil
	}
	entries, overlap, slabs := batch.entries, batch.pages, batch.slabs
	blockCost := costs.PageRead*int64(overlap) + costs.ValueDecompress*int64(slabs)
	meter.Charge(blockCost)
	scanActual += blockCost
	filterActual := scanActual
	for _, e := range entries {
		if err := meter.Guard(); err != nil { // enforce the cost ceiling per scanned row (CLAUDE.md §13)
			return outcome{}, err
		}
		meter.Charge(costs.StorageRowRead)
		scanActual += costs.StorageRowRead
		filterActual += costs.StorageRowRead
		// Materialize the filter's columns if the lazy load left them unfetched — exactly the
		// touched set the block above charged (large-values.md §14).
		row, err := store.resolveColumns(e.Row, mask)
		if err != nil {
			return outcome{}, err
		}
		keep := true
		if filter != nil {
			beforeFilter := meter.Accrued
			v, err := filter.eval(row, env, meter)
			if err != nil {
				return outcome{}, err
			}
			filterActual += meter.Accrued - beforeFilter
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
	db.explainActual.record("Scan "+del.Table, scanActual)
	if filter != nil {
		db.explainActual.record("Filter", filterActual)
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
					return outcome{}, newFKViolationDelete(parent.Name, r.fk.Name, r.childTable)
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
	// Precompute the entries to remove for each index in a &db pass (evaluating any expression key
	// against the stored row), BEFORE the removals below. Resolve the table's indexes once.
	// toRemove[k] is index k's entries (rindexes order = table.Indexes order).
	rindexes, err := db.resolveTableIndexes(table)
	if err != nil {
		return outcome{}, err
	}
	toRemove := make([][][]byte, len(rindexes))
	for k := range rindexes {
		for _, m := range matched {
			eks, err := db.indexEntries(table.Columns, colls, &rindexes[k], m.key, m.row)
			if err != nil {
				return outcome{}, err
			}
			toRemove[k] = append(toRemove[k], eks...)
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
	for k, def := range table.Indexes {
		istore := db.writeIndexStoreScoped(del.DB, strings.ToLower(def.Name))
		for _, ek := range toRemove[k] {
			if _, err := istore.Remove(ek); err != nil {
				return outcome{}, err
			}
		}
	}
	db.markEstimatorMutation(del.DB, del.Table)
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
	// Resolve the indexes once (column ordinals + resolved expression keys — indexes.md §4),
	// parallel to table.Indexes; the unique end-state validation and the entry-move computation
	// below evaluate any expression key through it (unmetered).
	rindexes, err := db.resolveTableIndexes(table)
	if err != nil {
		return outcome{}, err
	}
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
	// UPDATE SET col = DEFAULT reuses INSERT's once-per-statement resolution of expression
	// defaults. Constant/no defaults become constant rExpr nodes below, so applying them is free;
	// expression defaults evaluate once per matched row through the UPDATE meter and statement RNG.
	var defaultExprs []*rExpr
	if slices.ContainsFunc(upd.Assignments, func(a assignment) bool { return a.IsDefault }) {
		defaultExprs, err = db.resolveDefaultExprs(table)
		if err != nil {
			return outcome{}, err
		}
	}
	pkMembers := table.PKIndices()
	plans := make([]assignPlan, 0, len(upd.Assignments))
	for _, a := range upd.Assignments {
		idx := table.ColumnIndex(a.Column)
		if idx < 0 {
			return outcome{}, newError(UndefinedColumn, "column does not exist: "+a.Column)
		}
		// A GENERATED ALWAYS identity column can only be set to DEFAULT (sequences.md §13.4).
		// An ordinary assignment is 428C9; DEFAULT is allowed and advances the owned sequence.
		if c := table.Columns[idx].Identity; c != nil && *c == identityAlways && !a.IsDefault {
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
		if a.IsDefault {
			src := defaultExprs[idx]
			if src == nil {
				src = valueToRExpr(defaultOrNull(col))
			}
			plan := assignPlan{
				idx: idx, name: col.Name, decimal: col.Decimal, varcharLen: col.VarcharLen,
				notNull: col.NotNull, source: src,
			}
			if scalar, ok := col.Type.AsScalar(); ok {
				plan.target = scalar
			} else {
				ct := resolveColType(col.Type, s.catalog.readSnap().types)
				plan.colType = &ct
			}
			plans = append(plans, plan)
			continue
		}
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
	// Plan and execute the target scan through the shared mutation access-path seam. The keyed batch
	// is over the pre-update state and feeds the unchanged two-phase rewrite below.
	scanPlan := db.planMutationScan(upd.DB, table, filter)
	scanBefore := meter.Accrued
	batch, err := db.executeMutationScan(scanPlan, upd.Table, bound, env, meter, mask)
	if err != nil {
		return outcome{}, err
	}
	scanActual := meter.Accrued - scanBefore
	if batch.empty {
		db.explainActual.record("Scan "+upd.Table, scanActual)
		if filter != nil {
			db.explainActual.record("Filter", scanActual)
		}
		return dmlOutcome(retNames, retTypes, nil, 0, meter.Accrued), nil
	}
	entries, overlap, slabs := batch.entries, batch.pages, batch.slabs
	blockCost := costs.PageRead*int64(overlap) + costs.ValueDecompress*int64(slabs)
	meter.Charge(blockCost)
	scanActual += blockCost
	filterActual := scanActual
	for _, e := range entries {
		if err := meter.Guard(); err != nil { // enforce the cost ceiling per scanned row (CLAUDE.md §13)
			return outcome{}, err
		}
		meter.Charge(costs.StorageRowRead)
		scanActual += costs.StorageRowRead
		filterActual += costs.StorageRowRead
		// Materialize the filter's + assignment sources' columns if the lazy load left them
		// unfetched — exactly the touched set the block above charged (large-values.md §14).
		row, err := store.resolveColumns(e.Row, mask)
		if err != nil {
			return outcome{}, err
		}
		if filter != nil {
			beforeFilter := meter.Accrued
			v, err := filter.eval(row, env, meter)
			if err != nil {
				return outcome{}, err
			}
			if !v.IsTrue() {
				filterActual += meter.Accrued - beforeFilter
				continue
			}
			filterActual += meter.Accrued - beforeFilter
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
				return outcome{}, stampTable(err, table.Name)
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
	db.explainActual.record("Scan "+upd.Table, scanActual)
	if filter != nil {
		db.explainActual.record("Filter", filterActual)
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
				return outcome{}, newUniqueViolation(table.Name, pkeyName(table.Name))
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
		for ix := range table.Indexes {
			def := table.Indexes[ix]
			if !def.Unique {
				continue
			}
			rindex := &rindexes[ix]
			istore := db.lkpIndexStoreScoped(upd.DB, strings.ToLower(def.Name))
			batch := make(map[string]struct{})
			for _, u := range updates {
				prefix, ok, err := db.indexPrefix(table.Columns, colls, rindex, u.row)
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
					return outcome{}, newUniqueViolation(table.Name, def.Name)
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
					return outcome{}, newExclusionViolation(table.Name, exc.Name)
				}
			}
			for i := range updates {
				for j := 0; j < i; j++ {
					if exclusionPairConflicts(table.Columns, exc, updates[i].row, updates[j].row) {
						return outcome{}, newExclusionViolation(table.Name, exc.Name)
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
				return outcome{}, newFKViolationInsert(relation, fk.Name)
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
					return outcome{}, newFKViolationDelete(parent.Name, r.fk.Name, r.childTable)
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
		for k := range table.Indexes {
			// The row's old and new entry SETS (one entry for an ordered index, one per term for
			// GIN — gin.md §5). Remove old−new, insert new−old: a shared entry is left untouched,
			// keeping the copy-on-write dirty set byte-identical across cores. Computed via a &db
			// pass (evaluating any expression key), before the phase-2 writes.
			oldEks, err := db.indexEntries(table.Columns, colls, &rindexes[k], u.key, u.oldRow)
			if err != nil {
				return outcome{}, err
			}
			newEks, err := db.indexEntries(table.Columns, colls, &rindexes[k], u.newKey, u.row)
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
				return outcome{}, newUniqueViolation(table.Name, pkeyName(table.Name))
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
						return outcome{}, newUniqueViolation(table.Name, def.Name)
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
	db.markEstimatorMutation(upd.DB, upd.Table)
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
