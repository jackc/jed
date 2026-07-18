package jed

import (
	"bytes"
	"fmt"
	"slices"
	"sort"
	"strconv"
	"strings"
)

// alterConstraintState is the statement-local catalog work needed by ALTER TABLE slice 2. The
// owning table is edited directly; CASCADE copies any other child tables here so nothing reaches a
// working snapshot until the complete action list and validation scan succeed.
type alterConstraintState struct {
	added map[string]bool
	other map[string]*catTable
}

// alterColumnSource describes one final table column during an ALTER rewrite. original >= 0 copies
// that old row slot; added captures the ADD action's default before later actions can edit it.
type alterColumnSource struct {
	original    int
	added       *catColumn
	typeChanged bool
}

func dropAlterColumn(t *catTable, d *alterDropColumn, snap *snapshot, original *catTable, oldKey string, sources *[]alterColumnSource, pending *[]*sequenceDef, st *alterConstraintState) error {
	ci := t.ColumnIndex(d.Name)
	if ci < 0 {
		if d.IfExists {
			return nil
		}
		return newError(UndefinedColumn, "column does not exist: "+d.Name)
	}
	if slices.Contains(t.PK, ci) {
		return newError(FeatureNotSupported, "dropping a primary-key column is not supported yet")
	}
	droppedSource := (*sources)[ci]
	usesExpr := func(e exprNode) bool { return slices.Contains(checkReferencedColumns(e, t.Columns), ci) }
	indexUses := func(ix indexDef) bool {
		for _, key := range ix.Keys {
			if (key.Expr == nil && key.Col == ci) || (key.Expr != nil && usesExpr(key.Expr.Expr)) {
				return true
			}
		}
		return ix.Predicate != nil && usesExpr(ix.Predicate.Expr)
	}
	// PostgreSQL owns an FK through its local (referencing) columns. Dropping one of those
	// columns removes the whole FK even under RESTRICT. A self-referential FK that uses the
	// column only on its referenced side is still a dependency, like an FK from another table.
	fkUsesLocalColumn := func(fk foreignKey) bool { return slices.Contains(fk.Columns, ci) }
	fkUsesSelfRefColumn := func(fk foreignKey) bool {
		return strings.EqualFold(fk.RefTable, t.Name) && slices.Contains(fk.RefColumns, ci)
	}
	selfRefFkDep := false
	for _, fk := range t.ForeignKeys {
		selfRefFkDep = selfRefFkDep || (fkUsesSelfRefColumn(fk) && !fkUsesLocalColumn(fk))
	}
	incomingDep := false
	if source := droppedSource; source.original >= 0 {
		for key, base := range snap.tables {
			if strings.EqualFold(key, oldKey) {
				continue
			}
			child := base
			if changed := st.other[key]; changed != nil {
				child = changed
			}
			for _, fk := range child.ForeignKeys {
				if strings.EqualFold(fk.RefTable, original.Name) && slices.Contains(fk.RefColumns, source.original) {
					incomingDep = true
				}
			}
		}
	}
	if !d.Cascade && (selfRefFkDep || incomingDep) {
		return newError(DependentObjectsStillExist, "cannot drop column "+d.Name+" because other objects depend on it")
	}
	if d.Cascade && (*sources)[ci].original >= 0 {
		oldColumn := (*sources)[ci].original
		for key, base := range snap.tables {
			if strings.EqualFold(key, oldKey) {
				continue
			}
			child := base
			if changed := st.other[key]; changed != nil {
				child = changed
			}
			fks := make([]foreignKey, 0, len(child.ForeignKeys))
			for _, fk := range child.ForeignKeys {
				if strings.EqualFold(fk.RefTable, original.Name) && slices.Contains(fk.RefColumns, oldColumn) {
					continue
				}
				fks = append(fks, fk)
			}
			if len(fks) != len(child.ForeignKeys) {
				next := *child
				next.ForeignKeys = fks
				st.other[key] = &next
			}
		}
	}
	// PostgreSQL treats same-table CHECKs, indexes (including UNIQUE), and EXCLUDE
	// constraints as internally dependent on their columns. They disappear even under
	// RESTRICT; CASCADE is needed only for external dependents such as incoming FKs.
	checks := t.Checks[:0]
	for _, c := range t.Checks {
		if usesExpr(c.Expr) {
			delete(st.added, strings.ToLower(c.Name))
		} else {
			checks = append(checks, c)
		}
	}
	t.Checks = checks
	indexes := t.Indexes[:0]
	for _, ix := range t.Indexes {
		if indexUses(ix) {
			delete(st.added, strings.ToLower(ix.Name))
		} else {
			indexes = append(indexes, ix)
		}
	}
	t.Indexes = indexes
	exclusions := t.Exclusions[:0]
	for _, ex := range t.Exclusions {
		uses := false
		for _, el := range ex.Elements {
			uses = uses || el.Column == ci
		}
		if uses {
			delete(st.added, strings.ToLower(ex.Name))
		} else {
			exclusions = append(exclusions, ex)
		}
	}
	t.Exclusions = exclusions
	// Local-side FKs are internally owned by the dropped column and disappear without CASCADE.
	// CASCADE additionally removes a self-reference that depends only on the referenced side.
	fks := t.ForeignKeys[:0]
	for _, fk := range t.ForeignKeys {
		if fkUsesLocalColumn(fk) || (d.Cascade && fkUsesSelfRefColumn(fk)) {
			delete(st.added, strings.ToLower(fk.Name))
		} else {
			fks = append(fks, fk)
		}
	}
	t.ForeignKeys = fks
	t.Columns = slices.Delete(t.Columns, ci, ci+1)
	*sources = slices.Delete(*sources, ci, ci+1)
	for i := range t.PK {
		if t.PK[i] > ci {
			t.PK[i]--
		}
	}
	for i := range t.Indexes {
		for j := range t.Indexes[i].Keys {
			if t.Indexes[i].Keys[j].Expr == nil && t.Indexes[i].Keys[j].Col > ci {
				t.Indexes[i].Keys[j].Col--
			}
		}
	}
	for i := range t.ForeignKeys {
		for j := range t.ForeignKeys[i].Columns {
			if t.ForeignKeys[i].Columns[j] > ci {
				t.ForeignKeys[i].Columns[j]--
			}
		}
		if strings.EqualFold(t.ForeignKeys[i].RefTable, t.Name) {
			for j := range t.ForeignKeys[i].RefColumns {
				if t.ForeignKeys[i].RefColumns[j] > ci {
					t.ForeignKeys[i].RefColumns[j]--
				}
			}
		}
	}
	for i := range t.Exclusions {
		for j := range t.Exclusions[i].Elements {
			if t.Exclusions[i].Elements[j].Column > ci {
				t.Exclusions[i].Elements[j].Column--
			}
		}
	}
	// An owned sequence is part of the dropped column, so release an existing one now rather than
	// waiting for final ordinal remapping. Later actions must see the name as free, and a later
	// DEFAULT must not be able to advance the removed sequence and resurrect it during flush.
	if source := droppedSource; source.original >= 0 {
		for _, key := range snap.sequencesOwnedBy(original.Name) {
			seq := snap.sequence(key)
			if seq != nil && seq.OwnedBy != nil && int(seq.OwnedBy.Column) == source.original {
				snap.removeSequence(key)
			}
		}
	}
	kept := (*pending)[:0]
	for _, seq := range *pending {
		if seq.OwnedBy == nil || !strings.EqualFold(seq.OwnedBy.Table, t.Name) {
			kept = append(kept, seq)
			continue
		}
		if int(seq.OwnedBy.Column) == ci {
			continue
		}
		if int(seq.OwnedBy.Column) > ci {
			seq.OwnedBy.Column--
		}
		kept = append(kept, seq)
	}
	*pending = kept
	return nil
}

func newAlterConstraintState() *alterConstraintState {
	return &alterConstraintState{added: map[string]bool{}, other: map[string]*catTable{}}
}

func constraintNameTaken(t *catTable, name string) bool {
	for _, c := range t.Checks {
		if strings.EqualFold(c.Name, name) {
			return true
		}
	}
	for _, i := range t.Indexes {
		if i.Unique && strings.EqualFold(i.Name, name) {
			return true
		}
	}
	for _, f := range t.ForeignKeys {
		if strings.EqualFold(f.Name, name) {
			return true
		}
	}
	for _, e := range t.Exclusions {
		if strings.EqualFold(e.Name, name) {
			return true
		}
	}
	return false
}

func (db *engine) addAlterConstraint(t *catTable, def *alterConstraintDef, snap *snapshot, relationTaken func(string) bool, st *alterConstraintState) error {
	if def.Check != nil {
		d := def.Check
		if err := rejectCheckStructure(d.Expr); err != nil {
			return err
		}
		_, ty, err := resolve(singleScope(db, t), d.Expr, nil, &aggCtx{collecting: false}, &paramTypes{})
		if err != nil {
			return err
		}
		if ty.kind != rtBool && ty.kind != rtNull {
			return typeError("argument of CHECK must be boolean")
		}
		name := d.Name
		if name != "" {
			if constraintNameTaken(t, name) {
				return newError(DuplicateObject, "constraint "+name+" for relation "+t.Name+" already exists")
			}
		} else {
			refs := checkReferencedColumns(d.Expr, t.Columns)
			base := strings.ToLower(t.Name) + "_check"
			if len(refs) == 1 {
				base = strings.ToLower(t.Name) + "_" + strings.ToLower(t.Columns[refs[0]].Name) + "_check"
			}
			name = base
			for n := 1; constraintNameTaken(t, name); n++ {
				name = base + strconv.Itoa(n)
			}
		}
		t.Checks = append(t.Checks, checkConstraint{Name: name, ExprText: d.Text, Expr: d.Expr})
		sort.SliceStable(t.Checks, func(i, j int) bool { return strings.ToLower(t.Checks[i].Name) < strings.ToLower(t.Checks[j].Name) })
		st.added[strings.ToLower(name)] = true
		return nil
	}
	if def.Unique != nil {
		d := def.Unique
		cols := make([]int, 0, len(d.Columns))
		for _, n := range d.Columns {
			ci := t.ColumnIndex(n)
			if ci < 0 {
				return newError(UndefinedColumn, "column "+n+" named in key does not exist")
			}
			if slices.Contains(cols, ci) {
				return newError(DuplicateColumn, "column "+n+" appears twice in unique constraint")
			}
			ty := t.Columns[ci].Type
			if !db.typeIsKeyable(ty) {
				return newError(FeatureNotSupported, "a unique constraint on "+ty.CanonicalName()+" is not supported yet")
			}
			cols = append(cols, ci)
		}
		name := d.Name
		if name != "" {
			if err := checkReservedName("constraint", name); err != nil {
				return err
			}
			if relationTaken(name) {
				return newError(DuplicateTable, "relation already exists: "+name)
			}
			if constraintNameTaken(t, name) {
				return newError(DuplicateObject, "constraint "+name+" for relation "+t.Name+" already exists")
			}
		} else {
			base := strings.ToLower(t.Name)
			for _, ci := range cols {
				base += "_" + strings.ToLower(t.Columns[ci].Name)
			}
			base += "_key"
			name = base
			for n := 1; relationTaken(name) || constraintNameTaken(t, name); n++ {
				name = base + strconv.Itoa(n)
			}
		}
		t.Indexes = append(t.Indexes, indexDef{Name: name, Keys: columnKeys(cols), Unique: true, Kind: indexBtree})
		sort.SliceStable(t.Indexes, func(i, j int) bool { return strings.ToLower(t.Indexes[i].Name) < strings.ToLower(t.Indexes[j].Name) })
		st.added[strings.ToLower(name)] = true
		return nil
	}
	if def.Foreign != nil {
		d := def.Foreign
		local := make([]int, 0, len(d.Columns))
		for _, n := range d.Columns {
			ci := t.ColumnIndex(n)
			if ci < 0 {
				return newError(UndefinedColumn, "column "+n+" named in key does not exist")
			}
			if slices.Contains(local, ci) {
				return newError(DuplicateColumn, "column "+n+" appears twice in foreign key constraint")
			}
			local = append(local, ci)
		}
		var parent *catTable
		if strings.EqualFold(d.RefTable, t.Name) {
			parent = t
		} else {
			parent, _ = snap.table(d.RefTable)
		}
		if parent == nil {
			return newError(UndefinedTable, "table does not exist: "+d.RefTable)
		}
		var refs []int
		if d.RefColumns == nil {
			if len(parent.PK) == 0 {
				return newError(UndefinedObject, "there is no primary key for referenced table "+parent.Name)
			}
			refs = append([]int(nil), parent.PK...)
		} else {
			for _, n := range d.RefColumns {
				ci := parent.ColumnIndex(n)
				if ci < 0 {
					return newError(UndefinedColumn, "column "+n+" named in key does not exist")
				}
				if slices.Contains(refs, ci) {
					return newError(DuplicateColumn, "column "+n+" appears twice in foreign key constraint")
				}
				refs = append(refs, ci)
			}
		}
		if len(local) != len(refs) {
			return newError(InvalidForeignKey, "number of referencing and referenced columns for foreign key disagree")
		}
		name := d.Name
		if name != "" {
			if constraintNameTaken(t, name) {
				return newError(DuplicateObject, "constraint "+name+" for relation "+t.Name+" already exists")
			}
		} else {
			base := strings.ToLower(t.Name)
			for _, ci := range local {
				base += "_" + strings.ToLower(t.Columns[ci].Name)
			}
			base += "_fkey"
			name = base
			for n := 1; constraintNameTaken(t, name); n++ {
				name = base + strconv.Itoa(n)
			}
		}
		onDelete, err := newFkAction(d.OnDelete, "DELETE")
		if err != nil {
			return err
		}
		onUpdate, err := newFkAction(d.OnUpdate, "UPDATE")
		if err != nil {
			return err
		}
		refSet := sortedUnique(refs)
		matched := len(parent.PK) > 0 && slices.Equal(sortedUnique(parent.PK), refSet)
		if !matched {
			for _, ix := range parent.Indexes {
				if cs := ix.columnOrdinals(); ix.Unique && cs != nil && slices.Equal(sortedUnique(cs), refSet) {
					matched = true
					break
				}
			}
		}
		if !matched {
			return newError(InvalidForeignKey, "there is no unique constraint matching given keys for referenced table "+parent.Name)
		}
		for i := range local {
			if !typesEqual(t.Columns[local[i]].Type, parent.Columns[refs[i]].Type) {
				return newError(DatatypeMismatch, fmt.Sprintf("foreign key constraint %s cannot be implemented: key columns %s and %s are of incompatible types: %s and %s", name, t.Columns[local[i]].Name, parent.Columns[refs[i]].Name, t.Columns[local[i]].Type.CanonicalName(), parent.Columns[refs[i]].Type.CanonicalName()))
			}
		}
		t.ForeignKeys = append(t.ForeignKeys, foreignKey{Name: name, Columns: local, RefTable: parent.Name, RefColumns: refs, OnDelete: onDelete, OnUpdate: onUpdate})
		sort.SliceStable(t.ForeignKeys, func(i, j int) bool {
			return strings.ToLower(t.ForeignKeys[i].Name) < strings.ToLower(t.ForeignKeys[j].Name)
		})
		st.added[strings.ToLower(name)] = true
		return nil
	}
	d := def.Exclude
	if d.Using != "" && !strings.EqualFold(d.Using, "gist") {
		return newError(UndefinedObject, "access method "+d.Using+" does not support exclusion constraints")
	}
	cols := make([]int, 0, len(d.Elements))
	elements := make([]exclusionElement, 0, len(d.Elements))
	for _, el := range d.Elements {
		ci := t.ColumnIndex(el.Column)
		if ci < 0 {
			return newError(UndefinedColumn, "column "+el.Column+" named in key does not exist")
		}
		if slices.Contains(cols, ci) {
			return newError(DuplicateColumn, "column "+el.Column+" appears twice in exclusion constraint")
		}
		ty := t.Columns[ci].Type
		var op exclusionOp
		switch el.Op {
		case "&&":
			if !ty.IsRange() {
				return newError(UndefinedObject, "data type "+ty.CanonicalName()+" has no default operator class for access method gist that accepts operator &&")
			}
			op = exclOverlaps
		case "=":
			if isGistScalarType(ty) {
				op = exclEqual
			} else if isGistDeferredScalarType(ty) {
				return newError(FeatureNotSupported, "an exclusion constraint with = over "+ty.CanonicalName()+" is not supported yet")
			} else {
				return newError(UndefinedObject, "data type "+ty.CanonicalName()+" has no default operator class for access method gist")
			}
		default:
			return newError(FeatureNotSupported, "exclusion constraint operator "+el.Op+" is not supported yet")
		}
		cols = append(cols, ci)
		elements = append(elements, exclusionElement{Column: ci, Op: op})
	}
	name := d.Name
	if name != "" {
		if err := checkReservedName("constraint", name); err != nil {
			return err
		}
		if relationTaken(name) {
			return newError(DuplicateTable, "relation already exists: "+name)
		}
		if constraintNameTaken(t, name) {
			return newError(DuplicateObject, "constraint "+name+" for relation "+t.Name+" already exists")
		}
	} else {
		base := strings.ToLower(t.Name)
		for _, ci := range cols {
			base += "_" + strings.ToLower(t.Columns[ci].Name)
		}
		base += "_excl"
		name = base
		for n := 1; relationTaken(name) || constraintNameTaken(t, name); n++ {
			name = base + strconv.Itoa(n)
		}
	}
	t.Indexes = append(t.Indexes, indexDef{Name: name, Keys: columnKeys(cols), Kind: indexGist})
	sort.SliceStable(t.Indexes, func(i, j int) bool { return strings.ToLower(t.Indexes[i].Name) < strings.ToLower(t.Indexes[j].Name) })
	t.Exclusions = append(t.Exclusions, exclusionConstraint{Name: name, Index: name, Elements: elements})
	sort.SliceStable(t.Exclusions, func(i, j int) bool {
		return strings.ToLower(t.Exclusions[i].Name) < strings.ToLower(t.Exclusions[j].Name)
	})
	st.added[strings.ToLower(name)] = true
	return nil
}

func (db *engine) dropAlterConstraint(t *catTable, d *dropConstraintDef, snap *snapshot, sources []alterColumnSource, st *alterConstraintState) error {
	nameKey := strings.ToLower(d.Name)
	for i, c := range t.Checks {
		if strings.EqualFold(c.Name, d.Name) {
			t.Checks = slices.Delete(t.Checks, i, i+1)
			delete(st.added, nameKey)
			return nil
		}
	}
	for i, f := range t.ForeignKeys {
		if strings.EqualFold(f.Name, d.Name) {
			t.ForeignKeys = slices.Delete(t.ForeignKeys, i, i+1)
			delete(st.added, nameKey)
			return nil
		}
	}
	for i, e := range t.Exclusions {
		if strings.EqualFold(e.Name, d.Name) {
			t.Exclusions = slices.Delete(t.Exclusions, i, i+1)
			for j, ix := range t.Indexes {
				if strings.EqualFold(ix.Name, e.Index) {
					t.Indexes = slices.Delete(t.Indexes, j, j+1)
					break
				}
			}
			delete(st.added, nameKey)
			return nil
		}
	}
	for i, ix := range t.Indexes {
		if ix.Unique && strings.EqualFold(ix.Name, d.Name) {
			cols := sortedUnique(ix.columnOrdinals())
			var originalCols []int
			for _, col := range cols {
				if col < 0 || col >= len(sources) || sources[col].original < 0 {
					originalCols = nil
					break
				}
				originalCols = append(originalCols, sources[col].original)
			}
			if originalCols != nil {
				originalCols = sortedUnique(originalCols)
			}
			var deps []struct {
				key string
				fk  foreignKey
			}
			for _, fk := range t.ForeignKeys {
				if strings.EqualFold(fk.RefTable, t.Name) && slices.Equal(sortedUnique(fk.RefColumns), cols) {
					deps = append(deps, struct {
						key string
						fk  foreignKey
					}{strings.ToLower(t.Name), fk})
				}
			}
			for key, base := range snap.tables {
				if strings.EqualFold(key, t.Name) {
					continue
				}
				ot := base
				if changed := st.other[key]; changed != nil {
					ot = changed
				}
				for _, fk := range ot.ForeignKeys {
					if originalCols != nil && strings.EqualFold(fk.RefTable, t.Name) && slices.Equal(sortedUnique(fk.RefColumns), originalCols) {
						deps = append(deps, struct {
							key string
							fk  foreignKey
						}{key, fk})
					}
				}
			}
			if len(deps) > 0 && !d.Cascade {
				return newError(DependentObjectsStillExist, "cannot drop constraint "+d.Name+" because other objects depend on it")
			}
			if d.Cascade {
				for _, dep := range deps {
					if dep.key == strings.ToLower(t.Name) {
						for j := len(t.ForeignKeys) - 1; j >= 0; j-- {
							if strings.EqualFold(t.ForeignKeys[j].Name, dep.fk.Name) {
								t.ForeignKeys = slices.Delete(t.ForeignKeys, j, j+1)
							}
						}
					} else {
						ot := st.other[dep.key]
						if ot == nil {
							base := snap.tables[dep.key]
							cp := *base
							cp.ForeignKeys = append([]foreignKey(nil), base.ForeignKeys...)
							ot = &cp
							st.other[dep.key] = ot
						}
						for j := len(ot.ForeignKeys) - 1; j >= 0; j-- {
							if strings.EqualFold(ot.ForeignKeys[j].Name, dep.fk.Name) {
								ot.ForeignKeys = slices.Delete(ot.ForeignKeys, j, j+1)
							}
						}
					}
				}
			}
			t.Indexes = slices.Delete(t.Indexes, i, i+1)
			delete(st.added, nameKey)
			return nil
		}
	}
	if d.IfExists {
		return nil
	}
	return newError(UndefinedObject, "constraint does not exist: "+d.Name)
}

// validateAlterConstraints scans once, checks every newly-added surviving constraint against the
// final table definition, and returns sorted backing-index entries for publication.
func (db *engine) validateAlterConstraints(original, t *catTable, dbScope *string, snap *snapshot, st *alterConstraintState, meter *costMeter, rewritten []entry) (map[string][][]byte, error) {
	if len(st.added) == 0 {
		return nil, nil
	}
	mask := make([]bool, len(t.Columns))
	for i := range mask {
		mask[i] = true
	}
	store := db.lkpStoreScoped(dbScope, original.Name)
	// The resolved column types for the NEW table shape `t` (the vehicle a composite index column's
	// key encoder needs, encoding.md §2.15). Resolved from t.Columns — NOT store.colTypes, which still
	// reflects the OLD shape (fewer columns) when this ALTER also adds a column and the rows are the
	// rewritten new-shape rows — against the main catalog (readSnap), where composite types live for
	// both main and temp tables (CREATE TYPE is persistent), matching the sibling rewriteColTypes.
	colTypes := make([]colType, len(t.Columns))
	for i, c := range t.Columns {
		colTypes[i] = resolveColType(c.Type, db.readSnap().types)
	}
	rows := rewritten
	if rows == nil {
		var pages, slabs int
		var err error
		rows, pages, slabs, err = store.ScanWithUnits(mask)
		if err != nil {
			return nil, err
		}
		meter.Charge(costs.PageRead*int64(pages) + costs.ValueDecompress*int64(slabs))
	}
	checks, err := db.resolveChecks(t)
	if err != nil {
		return nil, err
	}
	colls := db.columnCollations(t.Columns)
	seen := map[string]map[string]bool{}
	entries := map[string][][]byte{}
	// Presence in entries, including an empty slice for an empty table, is the publication-time
	// identity marker for a newly-added backing index. This distinguishes DROP+ADD reuse of an old
	// name from an unchanged old index whose store may be retained.
	for _, ix := range t.Indexes {
		nk := strings.ToLower(ix.Name)
		if st.added[nk] {
			entries[nk] = nil
		}
	}
	for _, e := range rows {
		if err := meter.Guard(); err != nil {
			return nil, err
		}
		if rewritten == nil {
			meter.Charge(costs.StorageRowRead)
		}
		row, err := store.resolveInlineColumns(e.Row)
		if err != nil {
			return nil, err
		}
		for _, c := range checks {
			if st.added[strings.ToLower(c.name)] {
				if err := evalChecks([]namedCheck{c}, t.Name, row, &evalEnv{exec: db, rng: newStmtRng()}, meter); err != nil {
					return nil, err
				}
			}
		}
		for _, ix := range t.Indexes {
			nk := strings.ToLower(ix.Name)
			if !st.added[nk] {
				continue
			}
			ri, err := db.resolveIndex(t, ix)
			if err != nil {
				return nil, err
			}
			if ix.Unique {
				p, ok, err := db.indexPrefix(colTypes, colls, &ri, row)
				if err != nil {
					return nil, err
				}
				if ok {
					if seen[nk] == nil {
						seen[nk] = map[string]bool{}
					}
					if seen[nk][string(p)] {
						return nil, newUniqueViolation(t.Name, ix.Name)
					}
					seen[nk][string(p)] = true
				}
			}
			eks, err := db.indexEntries(t.Columns, colTypes, colls, &ri, e.Key, row)
			if err != nil {
				return nil, err
			}
			entries[nk] = append(entries[nk], eks...)
		}
	}
	// FK validation uses byte-identical parent probes. A self-reference checks the scanned end state;
	// other parents are unchanged by this statement and can use their existing PK/UNIQUE stores.
	for _, fk := range t.ForeignKeys {
		if !st.added[strings.ToLower(fk.Name)] {
			continue
		}
		var parent *catTable
		if strings.EqualFold(fk.RefTable, t.Name) {
			parent = t
		} else {
			parent, _ = snap.table(fk.RefTable)
		}
		pc := db.columnCollations(parent.Columns)
		for _, e := range rows {
			row, err := store.resolveInlineColumns(e.Row)
			if err != nil {
				return nil, err
			}
			probe, ok, err := buildFkProbe(&fk, parent, pc, row, fk.Columns)
			if err != nil {
				return nil, err
			}
			if !ok {
				continue
			}
			hit := false
			if parent == t {
				for _, pe := range rows {
					meter.Charge(costs.ConstraintCheck)
					if err := meter.Guard(); err != nil {
						return nil, err
					}
					pr, er := store.resolveInlineColumns(pe.Row)
					if er != nil {
						return nil, er
					}
					pp, pok, er := buildFkProbe(&fk, parent, pc, pr, fk.RefColumns)
					if er != nil {
						return nil, er
					}
					if pok && bytes.Equal(probe.bytes, pp.bytes) {
						hit = true
						break
					}
				}
			} else {
				hit, err = db.fkProbeHits(probe, parent.Name)
				if err != nil {
					return nil, err
				}
			}
			if !hit {
				return nil, newFKViolationInsert(t.Name, fk.Name)
			}
		}
	}
	for _, ex := range t.Exclusions {
		if !st.added[strings.ToLower(ex.Name)] {
			continue
		}
		for i := 0; i < len(rows); i++ {
			a, err := store.resolveInlineColumns(rows[i].Row)
			if err != nil {
				return nil, err
			}
			for j := 0; j < i; j++ {
				meter.Charge(costs.ConstraintCheck)
				if err := meter.Guard(); err != nil {
					return nil, err
				}
				b, err := store.resolveInlineColumns(rows[j].Row)
				if err != nil {
					return nil, err
				}
				if exclusionPairConflicts(t.Columns, ex, a, b) {
					return nil, newExclusionViolation(t.Name, ex.Name)
				}
			}
		}
	}
	for k := range entries {
		slices.SortFunc(entries[k], bytes.Compare)
	}
	return entries, nil
}
