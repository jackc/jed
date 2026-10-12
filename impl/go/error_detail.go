package jed

// Error Detail — PostgreSQL's DETAIL sentence naming the values that broke a constraint
// (spec/design/error-fields.md §9). Built only after the error is decided, from values the
// statement already holds: unmetered, so it can never change the code, the cost, or which error
// fires. Every helper applies the §9.4 privilege gate and returns "" when the detail is withheld or
// cannot be rendered (withDetail("") leaves the field absent). Mirrors
// impl/rust/src/executor/error_detail.rs.

import (
	"bytes"
	"strings"
	"unicode/utf8"
)

// The 23505 endings: a write's duplicate (`already exists.`) and a unique-index build over existing
// duplicates (`is duplicated.` — error-fields.md §9.2).
const (
	detailExists     = "already exists."
	detailDuplicated = "is duplicated."
)

// detailValueMax is the per-value cap, in bytes, before a rendered value is cut and suffixed `...`
// (§9.3 — PG's ExecBuildSlotValueDescription maxfieldlen).
const detailValueMax = 64

// detailClip cuts s to at most detailValueMax bytes at a UTF-8 character boundary, appending `...`
// when anything was cut (§9.3).
func detailClip(s string) string {
	if len(s) <= detailValueMax {
		return s
	}
	end := detailValueMax
	for end > 0 && !utf8.RuneStart(s[end]) {
		end--
	}
	return s[:end] + "..."
}

// detailColumnLabel is a column label: jed's identifier quoting rule (grammar.md §3).
func detailColumnLabel(columns []catColumn, ordinal int) string {
	return quoteIdentifier(columns[ordinal].Name)
}

// detailKeyPhrase is `Key (a, b)=(1, x)` — the shared key phrase of the 23505/23503/23P01 forms.
func detailKeyPhrase(labels, values []string) string {
	return "Key (" + strings.Join(labels, ", ") + ")=(" + strings.Join(values, ", ") + ")"
}

// detailVisible is the §9.4 gate: whether this session may see relation's values.
func (db *engine) detailVisible(relation string) bool {
	p := &db.session.privileges
	return p.IsPermissive() || p.AllowsTable(relation, PrivSelect)
}

// detailValue renders one value through the result-cell renderer in the session zone (§9.3): a
// top-level NULL is `null`, and the text is clipped. ok=false if a deferred value cannot be
// resolved or the value cannot render (an out-of-range zoned instant) — the detail is then omitted,
// never wrong.
func (db *engine) detailValue(v Value) (string, bool) {
	if v.Kind == ValUnfetched {
		r, err := resolveUnfetchedSelf(v.unfetched())
		if err != nil {
			return "", false
		}
		v = r
	}
	switch v.Kind {
	case ValNull:
		return "null", true
	case ValText:
		return detailClip(v.str()), true
	}
	s, err := v.renderIn(db.session.timeZone)
	if err != nil {
		return "", false
	}
	return detailClip(s), true
}

// detailValues renders each value (detailValue); ok=false if any cannot render.
func (db *engine) detailValues(values []Value) ([]string, bool) {
	out := make([]string, len(values))
	for i, v := range values {
		s, ok := db.detailValue(v)
		if !ok {
			return nil, false
		}
		out[i] = s
	}
	return out, true
}

// columnKeyPhrase is the key phrase over plain-column ordinals of row.
func (db *engine) columnKeyPhrase(columns []catColumn, ordinals []int, row storedRow) (string, bool) {
	labels := make([]string, len(ordinals))
	values := make([]Value, len(ordinals))
	for i, o := range ordinals {
		labels[i] = detailColumnLabel(columns, o)
		values[i] = row[o]
	}
	rendered, ok := db.detailValues(values)
	if !ok {
		return "", false
	}
	return detailKeyPhrase(labels, rendered), true
}

// pkDetail is 23505 on the primary key: `Key (id)=(1) already exists.` (or `is duplicated.` from a
// DDL build — pass ending).
func (db *engine) pkDetail(relation string, columns []catColumn, pk []int, row storedRow, ending string) string {
	if !db.detailVisible(relation) {
		return ""
	}
	phrase, ok := db.columnKeyPhrase(columns, pk, row)
	if !ok {
		return ""
	}
	return phrase + " " + ending
}

// indexDetail is 23505 on a unique index: the index's key labels (a column's quoted name, an
// expression's persisted canonical text) and the row's key values, evaluated afresh (unmetered) for
// an expression key.
func (db *engine) indexDetail(relation string, columns []catColumn, def *indexDef, rindex *resolvedIndex, row storedRow, ending string) string {
	if !db.detailVisible(relation) {
		return ""
	}
	if len(def.Keys) != len(rindex.Keys) {
		return ""
	}
	env := db.indexMaintEnv()
	labels := make([]string, len(def.Keys))
	values := make([]string, len(def.Keys))
	for i, k := range def.Keys {
		if k.Expr != nil {
			labels[i] = k.Expr.ExprText
		} else {
			labels[i] = detailColumnLabel(columns, k.Col)
		}
		rk := rindex.Keys[i]
		var v Value
		if rk.Expr != nil {
			ev, err := rk.Expr.eval(row, env, db.session.scratchMeter())
			if err != nil {
				return ""
			}
			v = ev
		} else {
			v = row[rk.Col]
		}
		s, ok := db.detailValue(v)
		if !ok {
			return ""
		}
		values[i] = s
	}
	return detailKeyPhrase(labels, values) + " " + ending
}

// indexDetailByName is indexDetail for the raise sites that hold only the resolved index: its
// definition is found by name on table's catalog entry.
func (db *engine) indexDetailByName(table *catTable, rindex *resolvedIndex, row storedRow, ending string) string {
	for i := range table.Indexes {
		if table.Indexes[i].Name == rindex.Name {
			return db.indexDetail(table.Name, table.Columns, &table.Indexes[i], rindex, row, ending)
		}
	}
	return ""
}

// failingRowDetail is 23514 / 23502: `Failing row contains (…).` — every column of the candidate
// row.
func (db *engine) failingRowDetail(relation string, row storedRow) string {
	if !db.detailVisible(relation) {
		return ""
	}
	values, ok := db.detailValues(row)
	if !ok {
		return ""
	}
	return "Failing row contains (" + strings.Join(values, ", ") + ")."
}

// fkChildDetail is 23503 child side: `Key (p)=(99) is not present in table "t".` — degraded to the
// value-free `Key is not present in table "t".` without SELECT on the child (§9.4).
func (db *engine) fkChildDetail(relation string, columns []catColumn, fk *foreignKey, row storedRow) string {
	tail := "is not present in table \"" + fk.RefTable + "\"."
	if !db.detailVisible(relation) {
		return "Key " + tail
	}
	phrase, ok := db.columnKeyPhrase(columns, fk.Columns, row)
	if !ok {
		return ""
	}
	return phrase + " " + tail
}

// fkParentDetail is 23503 parent side: `Key (id)=(1) is still referenced from table "c".` — the
// parent's referenced columns over values, the removed/changed parent tuple in FK slot order;
// value-free without SELECT on the parent (§9.4).
func (db *engine) fkParentDetail(parent *catTable, fk *foreignKey, child string, values []Value) string {
	tail := "is still referenced from table \"" + child + "\"."
	if !db.detailVisible(parent.Name) {
		return "Key " + tail
	}
	labels := make([]string, len(fk.RefColumns))
	for i, o := range fk.RefColumns {
		labels[i] = detailColumnLabel(parent.Columns, o)
	}
	rendered, ok := db.detailValues(values)
	if !ok {
		return ""
	}
	return detailKeyPhrase(labels, rendered) + " " + tail
}

// exclusionDetail is 23P01: `Key (r)=(new) conflicts with existing key (r)=(other).` (existing
// false gives the DDL build's `conflicts with key`).
func (db *engine) exclusionDetail(relation string, columns []catColumn, exc exclusionConstraint, row, other storedRow, existing bool) string {
	if !db.detailVisible(relation) {
		return ""
	}
	ordinals := make([]int, len(exc.Elements))
	for i, e := range exc.Elements {
		ordinals[i] = e.Column
	}
	newPhrase, ok := db.columnKeyPhrase(columns, ordinals, row)
	if !ok {
		return ""
	}
	if len(other) != len(columns) {
		return "" // an unresolvable conflicting row only omits the Detail
	}
	oldPhrase, ok := db.columnKeyPhrase(columns, ordinals, other)
	if !ok {
		return ""
	}
	with := "key"
	if existing {
		with = "existing key"
	}
	return newPhrase + " conflicts with " + with + " " + strings.TrimPrefix(oldPhrase, "Key ") + "."
}

// notNullRowViolation is 23502 for a NULL in NOT NULL column of the complete candidate row
// (error-fields.md §9.5), stamped with the relation and carrying the failing-row Detail.
func (db *engine) notNullRowViolation(relation, column string, row storedRow) *EngineError {
	return newNotNullViolation(column).withTable(relation).withDetail(db.failingRowDetail(relation, row))
}

// leastKey returns the least (bytewise) of keys, skipping any in skip; ok=false when none remain.
// The exclusion stored-row probe reports the least storage key among its matches so the Detail is
// deterministic (error-fields.md §9.2).
func leastKey(keys [][]byte, skip map[string]struct{}) ([]byte, bool) {
	var least []byte
	found := false
	for _, k := range keys {
		if skip != nil {
			if _, own := skip[string(k)]; own {
				continue
			}
		}
		if !found || bytes.Compare(k, least) < 0 {
			least = k
			found = true
		}
	}
	return least, found
}
