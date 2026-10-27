package jed

// integrity_constraints.go — the integrity check's constraint stage (spec/design/integrity-check.md
// §3.4, §4 step 3): under IntegrityCheckOptions.Constraints, each table in name byte order re-checks
// its declared constraints against its stored rows — the column pass (NOT NULL, typmods), then its
// named constraints (CHECK, unique index, FOREIGN KEY) in name byte order, ties by that kind order.
// Every pass re-reads the rows (or index entries) in key order, so memory stays bounded.

import (
	"bytes"
	"errors"
	"fmt"
	"slices"
	"sort"
)

// integrityNamedKind orders a table's named constraints that share a name (§3.4).
type integrityNamedKind int

const (
	integrityNamedCheck integrityNamedKind = iota
	integrityNamedUnique
	integrityNamedForeignKey
)

// integrityNamed is one named constraint object of a table.
type integrityNamed struct {
	name  string
	kind  integrityNamedKind
	check *checkConstraint
	index *indexDef
	fk    *foreignKey
}

// checkConstraints runs the constraint stage for table name: its column pass, then its named
// constraints. A table whose tree was unreadable records table_unreadable for the column pass (the
// table) and each named constraint, and checks nothing (§3.5).
func (k *integrityChecker) checkConstraints(name string) error {
	table := k.snap.tables[name]
	store := k.snap.stores[name]
	var named []integrityNamed
	for i := range table.Checks {
		named = append(named, integrityNamed{name: table.Checks[i].Name, kind: integrityNamedCheck, check: &table.Checks[i]})
	}
	for i := range table.Indexes {
		if def := &table.Indexes[i]; def.Unique && def.Kind == indexBtree {
			named = append(named, integrityNamed{name: def.Name, kind: integrityNamedUnique, index: def})
		}
	}
	for i := range table.ForeignKeys {
		named = append(named, integrityNamed{name: table.ForeignKeys[i].Name, kind: integrityNamedForeignKey, fk: &table.ForeignKeys[i]})
	}
	sort.SliceStable(named, func(a, b int) bool {
		if named[a].name != named[b].name {
			return named[a].name < named[b].name
		}
		return named[a].kind < named[b].kind
	})
	if k.tableUnreadable[name] {
		k.skip(name, IntegritySkipTableUnreadable)
		for _, n := range named {
			k.skip(n.name, IntegritySkipTableUnreadable)
		}
		return nil
	}
	var root pageRef
	if r := store.treeRoot(); r != nil {
		root = r.ref()
	}
	if err := k.checkColumns(table, store, root); err != nil {
		return err
	}
	for _, n := range named {
		var err error
		switch n.kind {
		case integrityNamedCheck:
			err = k.checkCheck(table, store, root, n.check)
		case integrityNamedUnique:
			err = k.checkUnique(table, store, n.index)
		default:
			err = k.checkForeignKey(table, store, root, n.fk)
		}
		if err != nil {
			return err
		}
	}
	return nil
}

// checkColumns is the column pass (§3.4 step 1): per row in storage-key order, per column in ordinal
// order, a NULL in a NOT NULL or primary-key column is not_null_violation; a non-NULL value of a
// top-level scalar column with a typmod is typmod_violation when the column's write-time typmod
// coercion (storeValue, as INSERT applies it) raises or changes the value's stored bytes.
func (k *integrityChecker) checkColumns(table *catTable, store *tableStore, root pageRef) error {
	notNull := make([]bool, len(table.Columns))
	typmods := make([]*typmod, len(table.Columns))
	for c := range table.Columns {
		col := &table.Columns[c]
		notNull[c] = col.NotNull || slices.Contains(table.PK, c)
		tm := columnTypmod(col)
		ct := store.colTypes[c]
		scalar := !ct.Composite && ct.Elem == nil && ct.RangeElem == nil
		if scalar && (tm.Decimal != nil || tm.VarcharLen != nil || tm.Temporal != nil) {
			typmods[c] = &tm
		}
	}
	iz := k.engine().inputZone()
	return k.scanRows(root, store.colTypes, 0, func(key []byte, row storedRow) error {
		for c := range table.Columns {
			col := &table.Columns[c]
			v := row[c]
			if v.Kind == ValNull {
				if notNull[c] {
					if err := k.add(IntegrityNotNullViolation, table.Name, nil, key, "column "+col.Name+" is NULL"); err != nil {
						return err
					}
				}
				continue
			}
			if typmods[c] == nil {
				continue
			}
			ct := store.colTypes[c]
			stored, err := storeValue(v, ct.Scalar, *typmods[c], false, col.Name, iz)
			detail := "column " + col.Name + " is not unchanged by its typmod"
			if err != nil {
				var ee *EngineError
				if !errors.As(err, &ee) {
					return err
				}
				detail = "column " + col.Name + ": " + err.Error()
			} else if bytes.Equal(encodeValue(ct, v), encodeValue(ct, stored)) {
				continue
			}
			if err := k.add(IntegrityTypmodViolation, table.Name, nil, key, detail); err != nil {
				return err
			}
		}
		return nil
	})
}

// resolveCheck resolves a CHECK constraint as the write path does (resolveChecks), or names the skip
// that prevents checking it (§3.5): its stored text does not parse; a host function it pinned is not
// registered at that component and version; a column it reads has an unavailable collation.
func (k *integrityChecker) resolveCheck(table *catTable, c *checkConstraint) (*rExpr, string, error) {
	if c.Unparsed {
		return nil, IntegritySkipUnparsableDefinition, nil
	}
	env := k.snap.env
	for _, dep := range c.HostDeps {
		id, ok := env.hosts.resolveHost(dep.Name, dep.ArgTypes)
		if !ok || !hostDepMatches(env.hosts.function(id), []hostFuncDep{dep}) {
			return nil, IntegritySkipMissingHostFunction, nil
		}
	}
	node, _, err := resolve(singleScope(k.engine(), table), c.Expr, nil, &aggCtx{collecting: false}, &paramTypes{})
	if err == nil {
		err = verifyCheckHostDeps(c.Name, node, env.hosts, c.HostDeps)
		if err != nil {
			return nil, IntegritySkipMissingHostFunction, nil
		}
	}
	if err != nil {
		var ee *EngineError
		if !errors.As(err, &ee) {
			return nil, "", err
		}
		switch ee.State {
		case UnparsableStoredDefinition:
			return nil, IntegritySkipUnparsableDefinition, nil
		case UndefinedFunction:
			return nil, IntegritySkipMissingHostFunction, nil
		case CollationVersionMismatch:
			return nil, IntegritySkipMissingLoadedData, nil
		}
		return nil, "", err
	}
	if k.readsUnavailableCollation(table, node) {
		return nil, IntegritySkipMissingLoadedData, nil
	}
	return node, "", nil
}

// checkCheck re-evaluates one CHECK constraint on every row in storage-key order, as the write path
// does (evalChecks), unmetered with a fresh scalar allowance per row: FALSE is check_violation, an
// error check_error; TRUE and NULL pass.
func (k *integrityChecker) checkCheck(table *catTable, store *tableStore, root pageRef, c *checkConstraint) error {
	node, reason, err := k.resolveCheck(table, c)
	if err != nil {
		return err
	}
	if reason != "" {
		k.skip(c.Name, reason)
		return nil
	}
	eng := k.engine()
	env := eng.indexMaintEnv()
	return k.scanRows(root, store.colTypes, 0, func(key []byte, row storedRow) error {
		eng.session.resetStatementMemory()
		v, err := node.eval(row, env, eng.session.scratchMeter())
		if err != nil {
			var ee *EngineError
			if !errors.As(err, &ee) {
				return err
			}
			return k.add(IntegrityCheckError, c.Name, nil, key, err.Error())
		}
		if v.Kind == ValBool && !v.boolVal() {
			return k.add(IntegrityCheckViolation, c.Name, nil, key, "the row fails the check")
		}
		return nil
	})
}

// uniqueEntrySlots is each key element's resolved type and key form, for splitting a unique index's
// entries into key prefix and storage key; reason names the skip when an expression key's type
// cannot be resolved (§3.5).
func (k *integrityChecker) uniqueEntrySlots(table *catTable, store *tableStore, def *indexDef) ([]colType, []bool, string, error) {
	types := make([]colType, len(def.Keys))
	collated := make([]bool, len(def.Keys))
	var rindex resolvedIndex
	for i, key := range def.Keys {
		if key.Expr == nil {
			types[i] = store.colTypes[key.Col]
			collated[i] = table.Columns[key.Col].Collation != ""
			continue
		}
		if rindex.Keys == nil {
			r, reason, err := k.resolveForAgreement(table, *def)
			if err != nil || reason != "" {
				return nil, nil, reason, err
			}
			rindex = r
		}
		types[i] = resolveColType(rindex.Keys[i].Ty, k.snap.types)
		collated[i] = rindex.Keys[i].Coll != nil
	}
	return types, collated, "", nil
}

// splitUniqueEntry splits an ordered index entry into the length of its key prefix (every element's
// §2.16 ordered slot) and whether an element is NULL. ok=false for an entry whose slots do not parse.
func splitUniqueEntry(entry []byte, def *indexDef, types []colType, collated []bool) (int, bool, bool) {
	pos := 0
	hasNull := false
	for i, ct := range types {
		if pos >= len(entry) {
			return 0, false, false
		}
		order := def.keyOrder(i)
		tag := entry[pos]
		pos++
		if tag == order.nullTag() {
			hasNull = true
			continue
		}
		if tag != order.presentTag() {
			return 0, false, false
		}
		var m byte
		if order.Desc {
			m = 0xFF
		}
		n, ok := colTypeKeyBodyLen(ct, collated[i], entry[pos:], m)
		if !ok {
			return 0, false, false
		}
		pos += n
	}
	return pos, hasNull, true
}

// checkUnique reads a unique B-tree index's entries in order: an entry whose key prefix equals the
// previous entry's, with no NULL element, is unique_violation keyed by its own storage key (§3.4). An
// index whose walk reported a finding records index_unreadable instead.
func (k *integrityChecker) checkUnique(table *catTable, store *tableStore, def *indexDef) error {
	types, collated, reason, err := k.uniqueEntrySlots(table, store, def)
	if err != nil {
		return err
	}
	if reason != "" {
		k.skip(def.Name, reason)
		return nil
	}
	if !k.indexClean[def.Name] {
		k.skip(def.Name, IntegritySkipIndexUnreadable)
		return nil
	}
	ist := k.snap.indexStores[def.Name]
	if ist == nil {
		return newError(DataCorrupted, "index "+def.Name+" has no store")
	}
	var root pageRef
	if r := ist.treeRoot(); r != nil {
		root = r.ref()
	}
	var prev []byte
	return k.scanRows(root, nil, 0, func(entry []byte, _ storedRow) error {
		n, hasNull, ok := splitUniqueEntry(entry, def, types, collated)
		if !ok || hasNull {
			prev = nil
			return nil
		}
		prefix := entry[:n]
		if prev != nil && bytes.Equal(prefix, prev) {
			if err := k.add(IntegrityUniqueViolation, def.Name, nil, entry[n:], "the key repeats the previous entry's"); err != nil {
				return err
			}
		}
		prev = append(prev[:0], prefix...)
		return nil
	})
}

// fkReferencedIndex is the referenced unique index a foreign key's probe goes through, as
// buildFkProbe chooses it: nil when the referenced columns are the parent's primary key, else the
// first unique index over exactly that column set. ok=false when neither matches.
func fkReferencedIndex(fk *foreignKey, parent *catTable) (*indexDef, bool) {
	refSet := sortedUnique(fk.RefColumns)
	if len(parent.PK) > 0 && slices.Equal(sortedUnique(parent.PK), refSet) {
		return nil, true
	}
	for i := range parent.Indexes {
		ix := &parent.Indexes[i]
		if cols := ix.columnOrdinals(); ix.Unique && cols != nil && slices.Equal(sortedUnique(cols), refSet) {
			return ix, true
		}
	}
	return nil, false
}

// checkForeignKey checks every referencing row in storage-key order under the key's MATCH rule
// (§3.4): MATCH SIMPLE exempts a row with any NULL key column, MATCH FULL one with all NULL and
// rejects one with some; otherwise the key, built as the write path builds its probe (buildFkProbe),
// must be found in the referenced table's primary key or the referenced unique index, else
// foreign_key_violation keyed by the row.
func (k *integrityChecker) checkForeignKey(table *catTable, store *tableStore, root pageRef, fk *foreignKey) error {
	parent := k.snap.tables[fk.RefTable]
	if parent == nil {
		return newError(DataCorrupted, "foreign key "+fk.Name+" references a missing table")
	}
	via, ok := fkReferencedIndex(fk, parent)
	if !ok {
		return newError(DataCorrupted, "foreign key "+fk.Name+" references no unique key")
	}
	// The probe encodes under the referenced columns' collations, so it needs them as the stored keys
	// were built (§3.5 missing_loaded_data).
	parentColls := make([]*Collation, len(parent.Columns))
	for i, col := range parent.Columns {
		if col.Collation != "" {
			parentColls[i] = k.snap.resolveCollation(col.Collation)
		}
	}
	for _, c := range fk.RefColumns {
		if k.collationUnavailable(parent.Columns[c].Collation) {
			k.skip(fk.Name, IntegritySkipMissingLoadedData)
			return nil
		}
	}
	if k.tableUnreadable[fk.RefTable] {
		k.skip(fk.Name, IntegritySkipTableUnreadable)
		return nil
	}
	if via != nil && !k.indexClean[via.Name] {
		k.skip(fk.Name, IntegritySkipIndexUnreadable)
		return nil
	}
	parentStore := k.snap.stores[fk.RefTable]
	var ist *tableStore
	if via != nil {
		if ist = k.snap.indexStores[via.Name]; ist == nil {
			return newError(DataCorrupted, "index "+via.Name+" has no store")
		}
	}
	return k.scanRows(root, store.colTypes, 0, func(key []byte, row storedRow) error {
		nulls := 0
		for _, c := range fk.Columns {
			if row[c].Kind == ValNull {
				nulls++
			}
		}
		if nulls == len(fk.Columns) || nulls > 0 && !fk.MatchFull {
			return nil
		}
		found := false
		detail := "no referenced row"
		if nulls > 0 {
			detail = "MATCH FULL key is partly NULL"
		} else {
			probe, _, err := buildFkProbe(fk, parent, parentColls, row, fk.Columns)
			if err != nil {
				var ee *EngineError
				if !errors.As(err, &ee) {
					return err
				}
				detail = fmt.Sprintf("the key does not encode: %v", err)
			} else if found, err = k.fkProbeFound(probe, parentStore, ist); err != nil {
				return err
			}
		}
		if found {
			return nil
		}
		return k.add(IntegrityForeignKeyViolation, fk.Name, nil, key, detail)
	})
}

// fkProbeFound is fkProbeHits over the pinned snapshot: a point lookup in the referenced table, or a
// prefix lookup in the referenced unique index.
func (k *integrityChecker) fkProbeFound(probe fkProbe, parentStore, ist *tableStore) (bool, error) {
	switch probe.kind {
	case fkProbePk:
		_, ok, err := parentStore.Get(probe.bytes)
		return ok, err
	case fkProbeAbsent:
		return false, nil
	default:
		found := false
		err := ist.ScanRange(uniqueProbeBound(probe.bytes), func([]byte, storedRow) (bool, error) {
			found = true
			return false, nil
		})
		return found, err
	}
}
