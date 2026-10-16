package jed

import (
	"bytes"
	"errors"
	"fmt"
	"math"
	"reflect"
	"slices"
	"strings"
	"unicode/utf8"
)

// Name/expression resolution — the parser-AST → resolved-expression (rExpr) pass. This file holds the
// resolution scope for multi-table FROM (scopeRel/scope and resolveBare/resolveQualified/columnAt),
// parameter-type inference (paramTypes/bindParams), projection & ORDER BY resolution
// (resolveProjections/outputName/orderAliasMatch), the master resolve() dispatcher over expression
// nodes, binary-operator resolution (resolveBinary/resolveOperandPair and the gcd/lcm helpers), and
// collation derivation (deriveCollation/combineDeriv). Function-call overload resolution lives in
// resolve_func.go.

// ============================================================================
// Resolution scope (multi-table FROM — spec/design/grammar.md §15).
//
// A scope is the ordered list of relations a SELECT's FROM clause puts in scope, each
// carrying the flat COLUMN OFFSET at which its columns begin in the concatenated (joined)
// row. A resolved column reference bakes a single flat index offset+local into reColumn, so
// the joined row is just each relation's row concatenated in FROM order and the evaluator is
// unchanged. A single-table SELECT / UPDATE / DELETE is a one-relation scope (offset 0).
//
// NOTE (forward-compat): the scope keys resolution ONLY on column name and type — never on a
// column's NotNull / PrimaryKey flags. A column on the nullable side of a future outer join
// is NULL-extended at runtime regardless of its declared nullability (grammar.md §15).
// ============================================================================

// scopeRel is one relation in a FROM scope: its label (alias, else table name — the exact
// name, matched byte-exactly), the table, and the flat offset of its first column. A
// qualifierOnly relation is visible ONLY to qualified references — the RETURNING old/new
// row-version pseudo-relations (grammar.md §32): bare-column resolution skips it (no new
// ambiguity), every other statement never builds one.
type scopeRel struct {
	label         string
	table         *catTable
	offset        int
	qualifierOnly bool
	// db is the relation's explicit database qualifier (attached-databases.md §3), carried from the
	// tableRef so the store is re-looked-up in the right database at exec (a store is resolved by name
	// per-access, recon). nil for a bare (implicit-scope) name — then the scoped funnels fall through
	// to the temp-first walk, so this is behavior-neutral for every unqualified query.
	db *string
	// cte is non-nil (pointing to the index into the statement's CTE list — spec/design/cte.md)
	// when this relation is a reference to a CTE rather than a base table: its table is the
	// binding's synthetic relation and exec delivers its rows from the cteCtx. nil for a base
	// table / SRF / pseudo-relation.
	cte *int
	// base is whether this relation is a catalog BASE TABLE (possibly under a column alias list —
	// grammar.md §15). A view body's planning trace records the columns resolved against such a
	// relation as dependencies (spec/design/views.md §6); every other relation kind records none.
	base bool
	// view is the view this relation is a reference to (spec/design/views.md §4), else "". A traced
	// view body records the columns resolved against it, so re-validation after CREATE OR REPLACE
	// VIEW can tell whether a dependent binds a column the replacement appended (§8).
	view string
	// layer is the target of a write through a view (spec/design/views.md §15.2): the names this
	// relation exposes are the layer's columns, not table's, and each resolves through the layer to a
	// base column or a read-only select item. table stays the base table, so flat indices keep the
	// base row's layout. Nil for every other relation.
	layer *writeLayer
}

// resolved is how a column reference resolved against the scope CHAIN (spec/design/grammar.md
// §26): level==0 is a LOCAL column of this query (a flat index into the joined row); level>=1
// is a correlated OUTER reference to an enclosing query (level hops outward, index the flat
// column index within that ancestor's row).
type resolved struct {
	level int
	index int
	// layered marks column col of the layered relation rel (a write through a view, views.md
	// §15.2), found level scopes out; index is then unused. Resolved to an expression by
	// resolveLayerRef.
	layered bool
	rel     int
	col     int
}

// scope is the relations a query's FROM clause puts in scope, in FROM order, plus the enclosing
// scope chain (for correlated references) and the catalog (so a subquery's own FROM resolves).
type scope struct {
	rels []scopeRel
	// parent is the enclosing query's scope, for correlated resolution (nil at top level).
	parent *scope
	// catalog lets a subquery's inner FROM tables be looked up during planning.
	catalog *engine
	// allowSubquery is true inside a SELECT (and its nested subqueries), false for UPDATE/DELETE
	// (a subquery there is 0A000 this slice).
	allowSubquery bool
	// ctes is the statement's CTE bindings visible here (spec/design/cte.md §2). Inherited
	// DIRECTLY down into nested scopes (a subquery sees the same ctes), NOT via the parent chain —
	// so CTE lookup never counts as a correlation level. Empty for every non-WITH statement.
	ctes []*cteBinding
	// merges are the USING/NATURAL merged columns (spec/design/grammar.md §15) — a bare reference
	// to a merge name resolves to its index (checked before the per-relation search, so it is never
	// the underlying copies' 42702 ambiguity). Empty except in a SELECT whose FROM has a USING join.
	merges []mergeCol
	// hidden is the flat indices SUPERSEDED by a merge — the underlying left+right copies, omitted
	// from `*` expansion (still reachable qualified) — plus the copies an unaliased parenthesized
	// join carries after its star columns. Empty unless the FROM has a USING/NATURAL join.
	hidden []int
	// subRels are the relation labels an UNALIASED parenthesized join keeps visible
	// (spec/design/grammar.md §15): `(a JOIN b ON …)` is one FROM relation, yet `a.x` and `b.*`
	// still name its inner relations. Reached only by qualified references; never physical.
	subRels []subRel
	// star is the flat indices bare `*` expands to, in order (grammar.md §15): PostgreSQL's per-join
	// layout — each USING/NATURAL join's merged columns, then its left input's remaining columns,
	// then its right input's — concatenated across comma items. Nil for a scope with no FROM join
	// planning (then `*` is every non-hidden column in FROM order).
	star []int
	// storageCollation marks a PERSISTED index expression's scope (an expression key or a
	// partial-index predicate — spec/design/collation.md §1 "Function results"): collation
	// derivation keeps the storage rule (a column, COLLATE, ||; a function result carries none), so
	// stored key bytes and partial-index membership never change meaning under the query-side
	// function propagation.
	storageCollation bool
	// subqueryDerivs records each scalar subquery resolved against this scope, keyed by its AST
	// node, with its output column's collation derivation (spec/design/collation.md §1 "Scalar
	// subqueries") and typmod (spec/design/create-table-as.md §2). resolve records them when it plans
	// the body, so deriveCollation / deriveTypmod — which work on the AST — can read them without
	// planning the body again. Nil until the first record (lazily allocated); a lookup always follows
	// the resolve of the same node, which overwrites any stale entry.
	subqueryDerivs map[*queryExpr]subqueryOutput
	// lateralView is set on a LATERAL FENCE only (spec/design/grammar.md §15): the empty scope a
	// parenthesized join's body is planned against when the join is not the first item of its FROM.
	// The fence hides the relations to the join's left from the body's ON clauses and non-LATERAL
	// items (PostgreSQL's lateral-only namespace entries), while this view — the prefix of those
	// relations, at the same correlation level — is what the body's LATERAL items and table
	// functions resolve against. Only a parenthesized-join body reads it; any other SELECT planned
	// against a fence strips it.
	lateralView *scope
	// lateralHit is set when a column reference resolves to one of THIS scope's relations. Read on a
	// parenthesized join's lateral prefix to learn whether the join's body correlates to the
	// relations on its left (grammar.md §15).
	lateralHit bool
	// lateralRefused is set on a LATERAL prefix whose relations are the left side of a RIGHT or FULL
	// join (PostgreSQL's p_lateral_ok = false, spec/design/grammar.md §44): a reference that resolves
	// to one of them is 42P10. A reference that passes them to an outer level is unaffected.
	lateralRefused bool
	// sharesParentLevel is set on a LATERAL prefix inside a parenthesized join's body
	// (spec/design/grammar.md §15): its parent — the fence's view — holds relations PostgreSQL places
	// in the same FROM namespace, so a bare column matching in both is ambiguous (42702), not shadowed.
	sharesParentLevel bool
	// hints is what a name-resolution hint needs beyond the visible relations (error-fields.md
	// §11.8). It never affects resolution.
	hints scopeHints
}

// lateralFence is the fence a parenthesized join's body is planned against (spec/design/grammar.md
// §15): an empty level over parent, so the body's ON clauses and non-LATERAL items see no relation to
// the join's left, carrying view — the prefix of those relations, one scope at the same correlation
// level — for the body's LATERAL items and table functions.
func lateralFence(parent, view *scope, catalog *engine, ctes []*cteBinding) *scope {
	return &scope{parent: parent, catalog: catalog, allowSubquery: true, ctes: ctes, lateralView: view}
}

// withoutLateralView is this fence without its lateral view: the same empty level, read by a SELECT
// that is not the parenthesized join the fence was built for (§15).
func (s *scope) withoutLateralView() *scope {
	return &scope{parent: s.parent, catalog: s.catalog, allowSubquery: true, ctes: s.ctes}
}

// subqueryOutput is a scalar subquery's recorded output column (scope.subqueryDerivs): its
// derivation as seen from outside the subquery, and its typmod.
type subqueryOutput struct {
	d  deriv
	tm typmod
}

// subRel is one inner relation label of an unaliased parenthesized join (grammar.md §15): its
// table's columns live at the flat slots `slots` of the join's single FROM relation (slot k holds
// column k). rel is the owning FROM ordinal, so a partial ON scope sees only the labels of the
// relations it spans.
type subRel struct {
	label string
	table *catTable
	slots []int
	rel   int
}

// subRelsWithin returns the sub-relation labels owned by FROM ordinals lo..hi inclusive.
func subRelsWithin(subs []subRel, lo, hi int) []subRel {
	var out []subRel
	for _, sr := range subs {
		if sr.rel >= lo && sr.rel <= hi {
			out = append(out, sr)
		}
	}
	return out
}

// mergeCol is a USING/NATURAL merged column (spec/design/grammar.md §15): name is the (exact)
// join column and index the flat row index a bare reference resolves to — the surviving side (the
// left column for INNER/LEFT, the right for RIGHT; FULL JOIN USING, a COALESCE, is deferred 0A000).
// Both underlying copies are recorded in the scope's hidden set.
type mergeCol struct {
	name  string
	index int
}

// singleScope is a one-relation scope with no parent (the single-table UPDATE / DELETE case).
// Subqueries ARE allowed: a correlated reference resolves to the target row via the per-row
// outer environment (the subquery's parent is this scope), an uncorrelated one folds once
// (spec/design/grammar.md §26). SELECT builds its own scope in planSelect.
func singleScope(catalog *engine, t *catTable) *scope {
	return &scope{rels: []scopeRel{{label: t.Name, table: t, offset: 0}}, catalog: catalog, allowSubquery: true}
}

// indexScope is the scope a PERSISTED index expression (an expression key or a partial-index
// predicate) resolves against: singleScope with the storage collation rule (storageCollation).
func indexScope(catalog *engine, t *catTable) *scope {
	s := singleScope(catalog, t)
	s.storageCollation = true
	return s
}

// emptyScope is the column-less scope a DEFAULT expression resolves against (constraints.md
// §2): a default may not reference a column (rejected as 0A000 by the structural pre-walk
// before resolution) and may not contain a subquery, so there are no relations and subqueries
// are disallowed.
func emptyScope(catalog *engine) *scope {
	return &scope{catalog: catalog, allowSubquery: false}
}

// returningScope is the scope a RETURNING list resolves against (grammar.md §32): the target
// table at offset 0 (bare and table-qualified references read the BASE row), plus the old/new
// row-version pseudo-relations as QUALIFIER-ONLY rels over the concatenated projection row
// [base | other]. baseIsOld says which version the base row is: false for INSERT/UPDATE
// (base = the new row, `old` reads the other half), true for DELETE (base = the old row,
// `new` reads the other half) — the absent version is the all-NULL row the caller appends.
// Explicit WITH (OLD AS o, NEW AS n) aliases are installed first and hide that version's standard
// name; an unaliased default is suppressed when its label is already occupied (including by the
// target table). Explicit aliases may not collide with the target or each other (42712). An
// INSERT's AS alias (upsert.md §5) replaces the target's label, so the table (or view) name is no
// longer a visible qualifier — referencing it is 42P01 with the table-alias hint (error-fields.md
// §11.2).
func returningScope(catalog *engine, t *catTable, baseIsOld bool, returning *returningClause, vw *viewWrite, alias *string) (*scope, error) {
	n := len(t.Columns)
	var layer *writeLayer
	if vw != nil {
		layer = vw.top
	}
	oldOffset, newOffset := n, 0
	if baseIsOld {
		oldOffset, newOffset = 0, n
	}
	rels := []scopeRel{targetRel(t, vw, alias)}
	for _, explicit := range []struct {
		alias  *string
		offset int
	}{{returning.OldAlias, oldOffset}, {returning.NewAlias, newOffset}} {
		if explicit.alias != nil {
			alias := *explicit.alias
			for _, rel := range rels {
				if rel.label == alias {
					return nil, newError(DuplicateAlias, "table name "+alias+" specified more than once")
				}
			}
			rels = append(rels, scopeRel{label: alias, table: t, offset: explicit.offset, qualifierOnly: true, layer: layer})
		}
	}
	for _, pseudo := range []struct {
		label   string
		offset  int
		aliased bool
	}{{"old", oldOffset, returning.OldAlias != nil}, {"new", newOffset, returning.NewAlias != nil}} {
		occupied := false
		for _, rel := range rels {
			occupied = occupied || rel.label == pseudo.label
		}
		if !pseudo.aliased && !occupied {
			rels = append(rels, scopeRel{label: pseudo.label, table: t, offset: pseudo.offset, qualifierOnly: true, layer: layer})
		}
	}
	return &scope{rels: rels, catalog: catalog, allowSubquery: true}, nil
}

// targetRel is the target relation of an INSERT's RETURNING / DO UPDATE scope (or an
// UPDATE/DELETE's RETURNING) at offset 0: labeled by the INSERT's AS alias when given (upsert.md
// §5), else by the view's name for a write through a view (views.md §15.2), else by the table's
// name. The catalog name is recorded (base / view) so a reference by the hidden table or view name
// gets the table-alias hint (error-fields.md §11.2).
func targetRel(t *catTable, vw *viewWrite, alias *string) scopeRel {
	rel := scopeRel{label: t.Name, table: t, offset: 0, base: true}
	if vw != nil {
		rel.label, rel.view, rel.base, rel.layer = vw.view, vw.view, false, vw.top
	}
	if alias != nil {
		rel.label = *alias
	}
	return rel
}

// onConflictExcludedScope is the scope a DO UPDATE's SET/WHERE resolve against
// (spec/design/upsert.md §5): the target table at offset 0 — labeled by the INSERT's AS alias when
// given — whose bare and label-qualified references read the EXISTING conflicting row, plus
// `excluded` as a QUALIFIER-ONLY relation at offset n over the combined row [existing | proposed]
// (excluded.col reads the proposed row). A target labeled `excluded` (a table so named, or AS
// excluded) keeps BOTH relations under the one label, so excluded.col is 42P09 (PostgreSQL's rule
// — resolveQualified). For an INSERT through a view (vw, views.md §15.7) both relations expose the
// view's columns over their halves of the combined row.
func onConflictExcludedScope(catalog *engine, t *catTable, vw *viewWrite, alias *string) *scope {
	n := len(t.Columns)
	excluded := scopeRel{label: "excluded", table: t, offset: n, qualifierOnly: true}
	if vw != nil {
		excluded.layer = vw.top
	}
	rels := []scopeRel{targetRel(t, vw, alias), excluded}
	return &scope{rels: rels, catalog: catalog, allowSubquery: true}
}

// outerOf lifts a parent-scope resolution into the child's frame: one more hop outward.
func outerOf(r resolved) resolved {
	r.level++
	return r
}

// resolveBare resolves a bare name in expression position: a column (resolveBareColumn) when any
// scope in the chain has one, else a WHOLE-ROW reference to a scalar function relation of that
// label (resolveScalarFuncRow) — PostgreSQL's transformColumnRef order, which tries every level for
// a column before trying the name as a relation. 42703 when neither matches.
func (s *scope) resolveBare(name string) (resolved, error) {
	r, err := s.resolveBareColumn(name)
	if err == nil {
		return r, nil
	}
	if e, ok := err.(*EngineError); !ok || e.State != UndefinedColumn {
		return resolved{}, err
	}
	if r, ok, ferr := s.resolveScalarFuncRow(name); ferr != nil || ok {
		return r, ferr
	}
	return resolved{}, err
}

// hit records a reference resolved to this scope's relation label (lateralHit), or refuses it when
// this scope is the left side of a RIGHT/FULL join — PostgreSQL's check_lateral_ref_ok, raised at the
// first matching relation (grammar.md §44).
func (s *scope) hit(label string) error {
	if err := s.refuse(label); err != nil {
		return err
	}
	s.lateralHit = true
	return nil
}

// refuse is 42P10 when this scope is the left side of a RIGHT/FULL join (hit).
func (s *scope) refuse(label string) error {
	if s.lateralRefused {
		return newError(InvalidColumnReference, "invalid reference to FROM-clause entry for table "+label).
			withDetail("The combining JOIN type must be INNER or LEFT for a LATERAL reference.")
	}
	return nil
}

// parentLevelHasColumn reports whether the fence views PostgreSQL places in this scope's FROM
// namespace (sharesParentLevel, grammar.md §15) have a visible column name, raising that namespace's
// errors in its order — outermost first: a second match is 42702, a refused first match 42P10.
func (s *scope) parentLevelHasColumn(name string) (bool, error) {
	if !s.sharesParentLevel || s.parent == nil {
		return false, nil
	}
	return s.parent.levelHasColumn(name)
}

// levelHasColumn is parentLevelHasColumn, then this scope's own columns.
func (s *scope) levelHasColumn(name string) (bool, error) {
	seen, err := s.parentLevelHasColumn(name)
	if err != nil {
		return false, err
	}
	matched := func(label string) error {
		if seen {
			return ambiguousColumn(name)
		}
		if err := s.refuse(label); err != nil {
			return err
		}
		seen = true
		return nil
	}
	for _, m := range s.merges {
		if m.name == name {
			if err := matched("unnamed_join"); err != nil {
				return false, err
			}
			break
		}
	}
	for _, r := range s.rels {
		if r.qualifierOnly {
			continue
		}
		if r.layer != nil {
			for _, c := range r.layer.columns {
				if c.Name == name {
					if err := matched(r.label); err != nil {
						return false, err
					}
				}
			}
			continue
		}
		for local, c := range r.table.Columns {
			idx := r.offset + local
			if c.Name == name && !slices.Contains(s.hidden, idx) {
				if err := matched(s.refusedLabel(&r, idx)); err != nil {
					return false, err
				}
			}
		}
	}
	return seen, nil
}

// refusedLabel is the label PostgreSQL names a relation by when a bare column idx of this scope is
// refused (hit): its label, else unnamed_join for an unaliased parenthesized join's column, else
// unnamed_subquery (an unaliased derived table).
func (s *scope) refusedLabel(r *scopeRel, idx int) string {
	if r.label != "" {
		return r.label
	}
	for _, sr := range s.subRels {
		if slices.Contains(sr.slots, idx) {
			return "unnamed_join"
		}
	}
	return "unnamed_subquery"
}

// resolveScalarFuncRow resolves a bare name that labels a scalar set-returning function's relation
// as a whole-row reference to it (spec/design/functions.md §10): a scalar function's row IS its one
// column, so `SELECT e FROM jsonb_array_elements(…) AS e` reads the element (PostgreSQL's
// makeWholeRowVar for a scalar function). Searched THIS scope, then outward (nearest wins). No other
// relation kind has a whole-row value in jed (no implicit per-table row types).
func (s *scope) resolveScalarFuncRow(name string) (resolved, bool, error) {
	for _, r := range s.rels {
		if r.qualifierOnly || r.layer != nil || r.label != name || !r.table.ScalarFunc {
			continue
		}
		if err := s.hit(name); err != nil {
			return resolved{}, false, err
		}
		return resolved{level: 0, index: r.offset}, true, nil
	}
	// An unaliased parenthesized join keeps its inner function relation's label visible.
	for _, sr := range s.subRels {
		if sr.label == name && sr.table.ScalarFunc {
			if err := s.hit(name); err != nil {
				return resolved{}, false, err
			}
			return resolved{level: 0, index: sr.slots[0]}, true, nil
		}
	}
	if s.parent != nil {
		r, ok, err := s.parent.resolveScalarFuncRow(name)
		if err != nil || !ok {
			return resolved{}, ok, err
		}
		return outerOf(r), true, nil
	}
	return resolved{}, false, nil
}

// resolveBareColumn resolves a bare column name against THIS scope, then OUTWARD through the parent
// chain. Within one scope: two+ relations have it → 42702 ambiguous; exactly one → local; none
// → fall through to the parent. A name found only in an ancestor is an outer reference (nearest
// scope wins — an inner match shadows an outer one). 42703 only if no scope in the chain has it.
// A qualifier-only rel (the RETURNING old/new pseudo-relations) is invisible here — no new
// ambiguity (grammar.md §32). A USING column resolves through this directly (a column only).
func (s *scope) resolveBareColumn(name string) (resolved, error) {
	// A USING/NATURAL MERGE column resolves to its surviving side (grammar.md §15), seeded here so
	// the bare name binds the merged column rather than its two (hidden) underlying copies — which
	// is why such a join column is unambiguous. A non-hidden column elsewhere with the same name
	// still makes the reference ambiguous (a third relation sharing the name).
	// A match in the fence views of the same FROM namespace comes first (§15).
	levelSeen, err := s.parentLevelHasColumn(name)
	if err != nil {
		return resolved{}, err
	}
	found := -1
	for _, m := range s.merges {
		if m.name == name {
			found = m.index
			break
		}
	}
	if found >= 0 {
		if levelSeen {
			return resolved{}, ambiguousColumn(name)
		}
		if err := s.hit("unnamed_join"); err != nil {
			return resolved{}, err
		}
	}
	var layered *resolved
	for ri, r := range s.rels {
		if r.qualifierOnly {
			continue
		}
		// A write through a view exposes the view's columns (views.md §15.2).
		if r.layer != nil {
			for col, c := range r.layer.columns {
				if c.Name == name {
					if levelSeen || found >= 0 || layered != nil {
						return resolved{}, ambiguousColumn(name)
					}
					if err := s.hit(r.label); err != nil {
						return resolved{}, err
					}
					layered = &resolved{layered: true, rel: ri, col: col}
				}
			}
			continue
		}
		// Count EVERY matching column, not just the first per relation: a synthetic relation (a CTE
		// or derived table) may carry two columns of the same name, and a bare reference to that name
		// is ambiguous (42702) exactly as a match across two relations is (cte.md §2, grammar.md §42).
		// Base tables have unique column names, so this only ever fires for a duplicate-output-name
		// synthetic relation.
		for local, c := range r.table.Columns {
			idx := r.offset + local
			// A merge's underlying copies are superseded by the merge above — skip them.
			if slices.Contains(s.hidden, idx) {
				continue
			}
			if c.Name == name {
				if levelSeen || found >= 0 || layered != nil {
					return resolved{}, ambiguousColumn(name)
				}
				if err := s.hit(s.refusedLabel(&r, idx)); err != nil {
					return resolved{}, err
				}
				found = idx
			}
		}
	}
	if layered != nil {
		return *layered, nil
	}
	if found >= 0 {
		s.traceFlat(found)
		return resolved{level: 0, index: found}, nil
	}
	if s.parent != nil {
		r, err := s.parent.resolveBareColumn(name)
		if err != nil {
			return resolved{}, err
		}
		return outerOf(r), nil
	}
	return resolved{}, undefinedColumn(name)
}

// starLabel is the label a frozen `*` column renders with (spec/design/views.md §5): hasLabel false
// (a bare "column") for a USING/NATURAL merged column and for a relation with no label (an unaliased
// derived table); an unaliased parenthesized join's column takes its inner relation's label; any
// other column its relation's label.
func (s *scope) starLabel(idx int) (string, bool) {
	for _, m := range s.merges {
		if m.index == idx {
			return "", false
		}
	}
	for _, r := range s.rels {
		if idx >= r.offset && idx < r.offset+len(r.table.Columns) {
			if r.label != "" {
				return r.label, true
			}
			for _, sr := range s.subRels {
				if slices.Contains(sr.slots, idx) {
					return sr.label, true
				}
			}
			return "", false
		}
	}
	return "", false
}

// traceFlat records a resolved column of this scope as a view dependency when a view body is being
// traced and the column belongs to a catalog base table (spec/design/views.md §6). A no-op on every
// ordinary statement.
func (s *scope) traceFlat(idx int) {
	if s.catalog == nil || s.catalog.viewTrace == nil {
		return
	}
	for _, r := range s.rels {
		if idx >= r.offset && idx < r.offset+len(r.table.Columns) {
			s.catalog.traceRelColumn(&r, idx-r.offset)
			return
		}
	}
}

// resolveQualified resolves a qualified rel.col against THIS scope, then outward. A qualifier
// naming a relation here binds — a missing column is then 42703 (no fall-through). Only an
// unknown qualifier walks outward (42P01 if no ancestor has it).
func (s *scope) resolveQualified(qualifier, name string) (resolved, error) {
	q := qualifier
	// Two relations under one label at this level — only the DO UPDATE scope of a target labeled
	// `excluded` builds one (every FROM rejects a duplicate label up front, 42712): PostgreSQL's
	// 42P09 (upsert.md §5).
	labeled := 0
	for _, r := range s.rels {
		if r.label == q {
			labeled++
		}
	}
	if labeled > 1 {
		return resolved{}, newError(AmbiguousAlias, "table reference "+qualifier+" is ambiguous")
	}
	for ri, r := range s.rels {
		if r.label == q && r.layer != nil {
			if err := s.hit(qualifier); err != nil {
				return resolved{}, err
			}
			found := -1
			for col, c := range r.layer.columns {
				if c.Name == name {
					if found >= 0 {
						return resolved{}, ambiguousColumn(name)
					}
					found = col
				}
			}
			if found < 0 {
				return resolved{}, undefinedColumn(name)
			}
			return resolved{layered: true, rel: ri, col: found}, nil
		}
	}
	for _, r := range s.rels {
		if r.label == q {
			// PostgreSQL refuses the relation itself, before looking up the column.
			if err := s.hit(qualifier); err != nil {
				return resolved{}, err
			}
			// A synthetic relation (a derived table, an aliased parenthesized join) may carry two
			// columns of one name; a qualified reference to it is ambiguous (42702) exactly as a bare
			// one is (PG). A USING/NATURAL copy hidden by a merge yields to the visible column.
			found, visible, ambiguous := -1, -1, false
			for local, c := range r.table.Columns {
				if c.Name != name {
					continue
				}
				idx := r.offset + local
				if found < 0 {
					found = idx
				}
				if !slices.Contains(s.hidden, idx) {
					ambiguous = ambiguous || visible >= 0
					visible = idx
				}
			}
			switch {
			case found < 0:
				return resolved{}, undefinedColumn(name)
			case ambiguous:
				return resolved{}, ambiguousColumn(name)
			case visible >= 0:
				s.traceFlat(visible)
				return resolved{level: 0, index: visible}, nil
			}
			s.traceFlat(found)
			return resolved{level: 0, index: found}, nil
		}
	}
	for _, sr := range s.subRels {
		if sr.label == q {
			if err := s.hit(qualifier); err != nil {
				return resolved{}, err
			}
			local := sr.table.ColumnIndex(name)
			if local < 0 {
				return resolved{}, undefinedColumn(name)
			}
			return resolved{level: 0, index: sr.slots[local]}, nil
		}
	}
	if s.parent != nil {
		r, err := s.parent.resolveQualified(qualifier, name)
		if err != nil {
			return resolved{}, err
		}
		return outerOf(r), nil
	}
	return resolved{}, missingFromEntry(qualifier)
}

// width returns the flat column count of this scope (the input-row width), the column count a
// `SELECT *` expands to and the base for an ORDER BY ordinal over `*` (grammar.md §10).
func (s *scope) width() int {
	n := 0
	for i := range s.rels {
		n += len(s.rels[i].table.Columns)
	}
	return n
}

// columnAt returns the column at a flat index in THIS scope (index known valid).
func (s *scope) columnAt(flat int) *catColumn {
	for i := range s.rels {
		r := s.rels[i]
		n := len(r.table.Columns)
		if flat >= r.offset && flat < r.offset+n {
			return &r.table.Columns[flat-r.offset]
		}
	}
	panic("a resolved flat column index is always in range")
}

// naturalCommonCols is the USING column list a NATURAL join derives (grammar.md §15): the column
// names common to the join's LEFT input (leftNames, its `*` columns in output order) and the right
// relation, in LEFT order with each name taken once (its first occurrence). An empty result
// degenerates the join to a CROSS join. (A merged column on the left keeps its underlying name, so
// a re-merge via a NATURAL chain is found here too.)
func naturalCommonCols(leftNames []string, right *scopeRel) []string {
	seen := map[string]bool{}
	var out []string
	for _, name := range leftNames {
		lc := name
		if !seen[lc] {
			seen[lc] = true
			if right.table.ColumnIndex(name) >= 0 {
				out = append(out, name)
			}
		}
	}
	return out
}

// ancestor returns the scope `level` hops outward (1 = immediate parent).
func (s *scope) ancestor(level int) *scope {
	cur := s
	for i := 0; i < level; i++ {
		cur = cur.parent
	}
	return cur
}

// columnOf returns the column a resolution refers to — local here, or outer in an ancestor.
func (s *scope) columnOf(r resolved) *catColumn {
	if r.layered {
		return &s.ancestor(r.level).rels[r.rel].layer.columns[r.col]
	}
	return s.ancestor(r.level).columnAt(r.index)
}

// undefinedColumn is 42703 — a column name that no relation in scope defines.
func undefinedColumn(name string) error {
	return newError(UndefinedColumn, "column does not exist: "+name)
}

// resolveFieldOf builds field selection `(base).field` over an already-resolved `base` node and
// its static type (spec/design/composite.md §S4): `base` must be composite — else 42809
// (wrong_object_type, PG's "column notation applied to non-composite") — and `field` must name one
// of its fields by exact name (PG folds the identifier), else 42703 (undefined_column).
// Returns the reField node carrying the fixed field ordinal, plus the field's static type.
func resolveFieldOf(baseNode *rExpr, baseTy resolvedType, field string) (*rExpr, resolvedType, error) {
	if baseTy.kind != rtComposite {
		return nil, resolvedType{}, newError(WrongObjectType, fmt.Sprintf(
			"column notation .%s applied to type %s, which is not a composite type",
			field, rtName(baseTy),
		))
	}
	for i, f := range baseTy.comp.fields {
		if f.name == field {
			return &rExpr{kind: reField, operand: baseNode, index: i}, f.ty, nil
		}
	}
	return nil, resolvedType{}, undefinedColumn(field)
}

// ambiguousColumn is 42702 — a bare column name that more than one relation in scope defines.
func ambiguousColumn(name string) error {
	return newError(AmbiguousColumn, "column reference "+name+" is ambiguous")
}

// missingFromEntry is 42P01 — a qualifier that names no relation in the FROM clause.
func missingFromEntry(qualifier string) error {
	return newError(UndefinedTable, "missing FROM-clause entry for table "+qualifier)
}

// paramTypes accumulates the inferred type of each bind parameter ($N) across every clause of a
// statement (spec/design/api.md §5). types[i] is the inferred scalar type of $(i+1); a nil entry
// marks a parameter referenced before any context fixed its type.
type paramTypes struct {
	// types is each parameter's inferred type: a scalar, or a container — an array, range, or named
	// composite (spec/design/api.md §5) — as the resolved column type the binder coerces into.
	types []*colType
	// uncacheable is set during resolution when a node is created that makes the resolved plan
	// un-reusable across executions: an reSubquery (the uncorrelated-subquery fold rewrites it to a
	// constant baking in THIS execution's bound params) or a precompiled-regex node (whose one-shot
	// rxCompileCharged cost flag mutates during eval, so a reused plan would under-charge the 2nd+
	// execute). A prepared statement's plan cache fills only when this stayed false — flagging at the
	// node's birth is complete regardless of where in the plan tree it lands (spec/design/api.md §2.4).
	uncacheable bool
	// Dependencies accumulate recursively at operation resolution (index-dependencies.md).
	// Session timezone, clock, entropy, sequences, session variables, and runtime text-to-date
	// inputs make a persisted expression nonimmutable (42P17). This is separate from plan
	// cacheability: ordinary queries can evaluate these operations afresh on every execution.
	nonimmutable bool
	indexContext bool
	timezoneDeps timeZoneDeps
	// frame, when set, also receives every type noted here (the EXPLAIN typing pass, plan_params.go).
	frame *planFrame
}

// note records that $(idx0+1) appears with scalar context type ty (nil = no context here). It
// unifies with any prior inference (noteCol).
func (p *paramTypes) note(idx0 int, ty *scalarType) error {
	if ty == nil {
		return p.noteCol(idx0, nil)
	}
	ct := scalarColType(*ty)
	return p.noteCol(idx0, &ct)
}

// noteCol records that $(idx0+1) appears with context type ty, a scalar or a container (nil = no
// context here). It unifies with any prior inference: equal types agree, two integer widths widen to
// the wider, an incompatible pair is 42804.
func (p *paramTypes) noteCol(idx0 int, ty *colType) error {
	for idx0 >= len(p.types) {
		p.types = append(p.types, nil)
	}
	p.frame.noteType(idx0, ty)
	if ty == nil {
		return nil
	}
	if p.types[idx0] == nil {
		t := *ty
		p.types[idx0] = &t
		return nil
	}
	u, err := unifyParamType(*p.types[idx0], *ty, idx0)
	if err != nil {
		return err
	}
	p.types[idx0] = &u
	return nil
}

// noteInputZone records a resolve-time literal fold that read an offset-less timestamptz in the
// session zone (timezones.md §9.3): the folded instant depends on the zone, so the plan must not be
// shared through a cache (a cached plan serves every session) and must not persist (an index
// expression or predicate is re-resolved per statement, in whatever zone that statement runs under —
// 42P17).
func (p *paramTypes) noteInputZone(iz *inputZone) {
	if iz.consulted {
		p.uncacheable = true
		p.nonimmutable = true
	}
}

// finalize returns the ordered parameter types. A slot referenced but never typed — including a
// gap in $1..$N — is 42P18 indeterminate_datatype.
func (p *paramTypes) finalize() ([]colType, error) {
	out := make([]colType, 0, len(p.types))
	for i, t := range p.types {
		if t == nil {
			return nil, newError(IndeterminateDatatype,
				fmt.Sprintf("could not determine data type of parameter $%d", i+1))
		}
		out = append(out, *t)
	}
	return out, nil
}

// isScalar reports whether t is a scalar column type (not an array, range, or composite).
func (t colType) isScalar() bool { return !t.Composite && t.Elem == nil && t.RangeElem == nil }

// unifyParamType unifies two inferred types for the same parameter: equal agrees; two integer
// widths widen to the wider; any other mismatch is 42804 (spec/design/api.md §5).
func unifyParamType(a, b colType, idx0 int) (colType, error) {
	if a.isScalar() && b.isScalar() {
		if a.Scalar == b.Scalar {
			return a, nil
		}
		if a.Scalar.IsInteger() && b.Scalar.IsInteger() {
			if a.Scalar.Rank() >= b.Scalar.Rank() {
				return a, nil
			}
			return b, nil
		}
	} else if reflect.DeepEqual(a, b) {
		return a, nil
	}
	return colType{}, newError(DatatypeMismatch,
		fmt.Sprintf("inconsistent types inferred for parameter $%d", idx0+1))
}

// dateClockLiteral resolves a date-context string literal naming one of the special values beyond
// ±infinity (date.md §6): 'epoch' folds to the constant 1970-01-01 like any date literal, while
// the CLOCK-RELATIVE words 'today' / 'now' / 'tomorrow' / 'yesterday' become the STABLE
// reDateClock node — the statement clock's day in the session zone, computed at EVAL and never
// folded at resolve. (PostgreSQL folds the literal at parse — the frozen-'today'
// DEFAULT/index/prepared-statement footgun — a documented divergence; jed's node re-evaluates per
// execution, so a cached plan tracks the clock.) The node flags the plan non-immutable, exactly
// like the runtime text→date cast (42P17 in an index expression). ok=false for an ordinary date
// string, which takes the caller's normal parse-to-constant path.
func dateClockLiteral(s string, params *paramTypes) (*rExpr, resolvedType, bool) {
	off, epoch, ok := dateClockSpecial(s)
	if !ok {
		return nil, resolvedType{}, false
	}
	if epoch {
		return &rExpr{kind: reConstDate, cInt: 0}, resolvedType{kind: rtDate}, true
	}
	params.nonimmutable = true
	return &rExpr{kind: reDateClock, cInt: int64(off)}, resolvedType{kind: rtDate}, true
}

// bindParams coerces each supplied bind value to its inferred parameter type, two-phase /
// all-or-nothing like INSERT (spec/design/api.md §5): a count mismatch is 42601 and every value
// is validated up front (22003/42804/22P02/23502 via storeValue) before any row is touched.
func bindParams(supplied []Value, types []colType, iz *inputZone) ([]Value, error) {
	return bindParamsWithLabels(supplied, types, paramLabels(len(types)), iz)
}

func paramLabels(n int) []string {
	labels := make([]string, n)
	for i := range labels {
		labels[i] = fmt.Sprintf("$%d", i+1)
	}
	return labels
}

func bindParamsWithLabels(supplied []Value, types []colType, labels []string, iz *inputZone) ([]Value, error) {
	if len(supplied) != len(types) {
		return nil, newError(SyntaxError, fmt.Sprintf(
			"bind parameter count mismatch: statement expects %d, got %d", len(types), len(supplied),
		))
	}
	bound := make([]Value, len(types))
	for i, ty := range types {
		var v Value
		var err error
		if ty.isScalar() {
			// A text value bound to a timestamptz parameter reads an offset-less string in the session
			// zone, as PostgreSQL's text-format parameter input does (timezones.md §9.3).
			v, err = storeValue(supplied[i], ty.Scalar, nil, nil, false, labels[i], iz)
		} else {
			v, err = bindContainerParam(supplied[i], ty, labels[i], iz)
		}
		if err != nil {
			return nil, err
		}
		bound[i] = v
	}
	return bound, nil
}

// bindContainerParam coerces a host value to an array, range, or composite parameter type
// (spec/design/api.md §5). A text value is read by the type's input function, as a string literal
// assigned to the type is; a range value is canonicalized as a range constructor's result is; every
// other value takes the storage coercion of a column of the type.
func bindContainerParam(v Value, ty colType, label string, iz *inputZone) (Value, error) {
	if v.Kind == ValText {
		switch {
		case ty.Elem != nil:
			return coerceStringToArray(v.str(), *ty.Elem, iz)
		case ty.RangeElem != nil:
			desc, _ := rangeForElement(ty.RangeElem.Scalar)
			rv, err := coerceStringToRange(v.str(), desc, iz)
			if err != nil {
				return Value{}, err
			}
			return RangeValue(rv), nil
		default:
			return coerceRecordTextToValue(v.str(), ty, iz)
		}
	}
	if v.Kind == ValRange && ty.RangeElem != nil {
		rv := v.rangeVal()
		if rv.Empty {
			return v, nil
		}
		lower, err := coerceRangeParamBound(rv.Lower, ty.RangeElem.Scalar, label, iz)
		if err != nil {
			return Value{}, err
		}
		upper, err := coerceRangeParamBound(rv.Upper, ty.RangeElem.Scalar, label, iz)
		if err != nil {
			return Value{}, err
		}
		desc, _ := rangeForElement(ty.RangeElem.Scalar)
		out, err := finalizeRange(desc, lower, upper, rv.LowerInc, rv.UpperInc)
		if err != nil {
			return Value{}, err
		}
		return RangeValue(out), nil
	}
	return coerceForStore(v, ty, nil, nil, false, label, iz)
}

// coerceRangeParamBound coerces a bound range value's finite bound to the element type (nil, an
// infinite bound, stays nil).
func coerceRangeParamBound(b *Value, elem scalarType, label string, iz *inputZone) (*Value, error) {
	if b == nil {
		return nil, nil
	}
	v, err := storeValue(*b, elem, nil, nil, false, label, iz)
	if err != nil {
		return nil, err
	}
	return &v, nil
}

// resolvedTypeOf is the resolved (static) type of a column of scalar type ty.
func resolvedTypeOf(ty scalarType) resolvedType {
	switch {
	case ty.IsText():
		return resolvedType{kind: rtText}
	case ty.IsBool():
		return resolvedType{kind: rtBool}
	case ty.IsDecimal():
		return resolvedType{kind: rtDecimal}
	case ty.IsBytea():
		return resolvedType{kind: rtBytea}
	case ty.IsUuid():
		return resolvedType{kind: rtUuid}
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
	case ty.IsJson():
		return resolvedType{kind: rtJson}
	case ty.IsJsonb():
		return resolvedType{kind: rtJsonb}
	default:
		return resolvedType{kind: rtInt, intTy: ty}
	}
}

// resolvedTypeOfCol is the resolved static type of a column of open type ty (spec/design/composite.md
// §5): a scalar via resolvedTypeOf, or a composite resolved against the snapshot's type catalog (the
// reference is guaranteed present — resolved at load / CREATE TYPE). Recursive for nested composites.
func resolvedTypeOfCol(ty dataType, snap *snapshot) resolvedType {
	if ty.Array != nil {
		elem := resolvedTypeOfCol(*ty.Array, snap)
		return resolvedType{kind: rtArray, elem: &elem}
	}
	if ty.Range != nil {
		elem := resolvedTypeOfCol(*ty.Range, snap)
		return resolvedType{kind: rtRange, elem: &elem}
	}
	if ty.Record != nil {
		// An anonymous record round-trips to the nameless composite a ROW(…) resolves to.
		fields := make([]compositeRField, len(*ty.Record))
		for i, f := range *ty.Record {
			fields[i] = compositeRField{name: f.Name, ty: resolvedTypeOfCol(f.Type, snap)}
		}
		return resolvedType{kind: rtComposite, comp: &compositeRType{fields: fields}}
	}
	if ty.Comp == nil {
		return resolvedTypeOf(ty.Scalar)
	}
	def := snap.compositeType(ty.Comp.Name)
	fields := make([]compositeRField, len(def.Fields))
	for i, f := range def.Fields {
		fields[i] = compositeRField{name: f.Name, ty: resolvedTypeOfCol(f.Type, snap)}
	}
	return resolvedType{kind: rtComposite, comp: &compositeRType{named: true, name: def.Name, fields: fields}}
}

// resolveProjections resolves SELECT items into evaluable projections (any result type is
// allowed in the select list, including boolean — SELECT a = b), each paired with its output
// column name (spec/design/grammar.md §8), collation derivation (spec/design/collation.md §1), and
// typmod (spec/design/create-table-as.md §2).
// `*` expands across ALL relations in FROM order, each relation's columns in catalog order (§15).
// expandLayerColumns expands a layered relation's `*` (a write through a view, views.md §15.2): each
// of the view's columns, by name, resolved through the layer.
func expandLayerColumns(s *scope, rel int, ag *aggCtx) ([]*rExpr, []string, []resolvedType, []deriv, []typmod, error) {
	layer := s.rels[rel].layer
	var ps []*rExpr
	var names []string
	var types []resolvedType
	var derivs []deriv
	var typmods []typmod
	for col := range layer.columns {
		c := &layer.columns[col]
		node, ty, err := resolveColumnRef(s, ag, resolved{layered: true, rel: rel, col: col}, c.Name)
		if err != nil {
			return nil, nil, nil, nil, nil, err
		}
		ps = append(ps, node)
		names = append(names, c.Name)
		types = append(types, ty)
		derivs = append(derivs, catColumnDeriv(c))
		typmods = append(typmods, columnTypmod(c))
	}
	return ps, names, types, derivs, typmods, nil
}

func resolveProjections(s *scope, items selectItems, ag *aggCtx, params *paramTypes) ([]*rExpr, []string, []resolvedType, []deriv, []typmod, error) {
	if items.All {
		// `*` with nothing to expand — a FROM-less SELECT — is PostgreSQL's exact error
		// (grammar.md §34). Qualifier-only rels don't count: they are RETURNING's old/new
		// pseudo-relations, and that scope always also carries the real relation.
		expandable := false
		for _, r := range s.rels {
			if !r.qualifierOnly {
				expandable = true
				break
			}
		}
		if !expandable {
			return nil, nil, nil, nil, nil, newError(SyntaxError, "SELECT * with no tables specified is not valid")
		}
		var ps []*rExpr
		var names []string
		var types []resolvedType
		var derivs []deriv
		var typmods []typmod
		// A write through a view returns the view's columns (views.md §15.2).
		for ri, r := range s.rels {
			if r.layer != nil && !r.qualifierOnly {
				return expandLayerColumns(s, ri, ag)
			}
		}
		// The FROM planner fixed the `*` order (USING/NATURAL merges per join — PostgreSQL,
		// grammar.md §15); a merge's underlying copies are hidden and so absent from it.
		star := s.star
		var starCols []*catColumn
		if star == nil {
			// The RETURNING old/new pseudo-relations are qualifier-only: `*` expands the real
			// relations' columns exactly as before (grammar.md §32).
			for _, r := range s.rels {
				if r.qualifierOnly {
					continue
				}
				shown := len(r.table.Columns) - starHiddenCount(&r, s.ctes)
				for i := range shown {
					idx := r.offset + i
					if slices.Contains(s.hidden, idx) {
						continue
					}
					star = append(star, idx)
					starCols = append(starCols, &r.table.Columns[i])
				}
			}
		} else {
			for _, idx := range star {
				starCols = append(starCols, s.columnAt(idx))
			}
		}
		for k, idx := range star {
			c := starCols[k]
			ps = append(ps, &rExpr{kind: reColumn, index: idx})
			names = append(names, c.Name)
			types = append(types, resolvedTypeOfCol(c.Type, s.catalog.readSnap()))
			derivs = append(derivs, catColumnDeriv(c))
			typmods = append(typmods, columnTypmod(c))
			s.traceFlat(idx)
		}
		// A written `*` in a view body is frozen into its expansion (views.md §5).
		if items.StarPos != 0 && s.catalog != nil && s.catalog.viewTracing() {
			cols := make([]starColumn, len(star))
			for k, idx := range star {
				label, hasLabel := s.starLabel(idx)
				cols[k] = starColumn{label: label, hasLabel: hasLabel, column: starCols[k].Name}
			}
			s.catalog.traceStar(items.StarPos, false, cols)
		}
		return ps, names, types, derivs, typmods, nil
	}
	ps := make([]*rExpr, 0, len(items.Items))
	names := make([]string, 0, len(items.Items))
	types := make([]resolvedType, 0, len(items.Items))
	derivs := make([]deriv, 0, len(items.Items))
	typmods := make([]typmod, 0, len(items.Items))
	for _, it := range items.Items {
		// `t.*` expands the FROM relation labeled Qualifier into one output column per column, in
		// catalog order (grammar.md §15) — like bare `*` but for one named relation and mixable with
		// other items. Resolved against the LOCAL scope only (like bare `*`); an unknown label is
		// 42P01, exactly as a qualified column ref.
		if it.Expr.Kind == exprQualifiedStar {
			want := it.Expr.Qualifier
			// `v.*` (or `old.*` / `new.*`) over a write through a view (views.md §15.2).
			layered := false
			for ri := range s.rels {
				if s.rels[ri].label == want && s.rels[ri].layer != nil {
					lps, lnames, ltypes, lderivs, ltypmods, err := expandLayerColumns(s, ri, ag)
					if err != nil {
						return nil, nil, nil, nil, nil, err
					}
					ps, names = append(ps, lps...), append(names, lnames...)
					types, derivs = append(types, ltypes...), append(derivs, lderivs...)
					typmods = append(typmods, ltypmods...)
					layered = true
					break
				}
			}
			if layered {
				continue
			}
			var table *catTable
			var slots []int
			for i := range s.rels {
				if s.rels[i].label == want {
					table = s.rels[i].table
					for c := range len(table.Columns) - starHiddenCount(&s.rels[i], s.ctes) {
						slots = append(slots, s.rels[i].offset+c)
					}
					break
				}
			}
			// An inner label of an unaliased parenthesized join expands to its own columns (§15).
			for i := range s.subRels {
				if table == nil && s.subRels[i].label == want {
					table, slots = s.subRels[i].table, s.subRels[i].slots
				}
			}
			if table == nil {
				return nil, nil, nil, nil, nil, newError(UndefinedTable, "missing FROM-clause entry for table "+it.Expr.Qualifier)
			}
			for i := range slots {
				ps = append(ps, &rExpr{kind: reColumn, index: slots[i]})
				names = append(names, table.Columns[i].Name)
				types = append(types, resolvedTypeOfCol(table.Columns[i].Type, s.catalog.readSnap()))
				derivs = append(derivs, catColumnDeriv(&table.Columns[i]))
				typmods = append(typmods, columnTypmod(&table.Columns[i]))
				s.traceFlat(slots[i])
			}
			// A written `rel.*` in a view body is frozen into `"rel" . "col"` references (views.md §5).
			if it.Expr.StarPos != 0 && s.catalog != nil && s.catalog.viewTracing() {
				cols := make([]starColumn, len(slots))
				for i := range slots {
					cols[i] = starColumn{label: want, hasLabel: true, column: table.Columns[i].Name}
				}
				s.catalog.traceStar(it.Expr.StarPos, true, cols)
			}
			continue
		}
		// `(expr).*` expands a composite base into one output column per field, in declaration
		// order (spec/design/composite.md §S4). The base is resolved once and each output column is
		// a reField node over a shared base node — the base is pure, so sharing the resolved node is
		// safe (no clone needed, unlike Rust where RExpr isn't Clone). A non-composite base is 42809.
		if it.Expr.Kind == exprFieldStar {
			baseNode, baseTy, err := resolve(s, *it.Expr.Base, nil, ag, params)
			if err != nil {
				return nil, nil, nil, nil, nil, err
			}
			if baseTy.kind != rtComposite {
				return nil, nil, nil, nil, nil, newError(WrongObjectType, fmt.Sprintf(
					"column notation .* applied to type %s, which is not a composite type",
					rtName(baseTy),
				))
			}
			// Each field keeps its declared typmod (PostgreSQL's FieldSelect).
			var fieldTypmods []typmod
			if def := compositeOfExpr(s, *it.Expr.Base); def != nil {
				for j := range def.Fields {
					fieldTypmods = append(fieldTypmods, fieldTypmod(&def.Fields[j]))
				}
			}
			for i, f := range baseTy.comp.fields {
				ps = append(ps, &rExpr{kind: reField, operand: baseNode, index: i})
				names = append(names, f.name)
				types = append(types, f.ty)
				derivs = append(derivs, deriv{})
				tm := typmod{}
				if i < len(fieldTypmods) {
					tm = fieldTypmods[i]
				}
				typmods = append(typmods, tm)
			}
			continue
		}
		node, ty, err := resolve(s, it.Expr, nil, ag, params)
		if err != nil {
			return nil, nil, nil, nil, nil, err
		}
		ps = append(ps, node)
		types = append(types, ty)
		// The output column's collation derivation (spec/design/collation.md §1 "Derived relations"),
		// which a derived table / CTE over this body carries into its synthetic column.
		d := deriv{}
		if ty.collatable() {
			if d, err = deriveCollation(s, it.Expr); err != nil {
				return nil, nil, nil, nil, nil, err
			}
		}
		derivs = append(derivs, d)
		typmods = append(typmods, deriveTypmod(s, it.Expr))
		if it.Alias != nil {
			names = append(names, *it.Alias)
		} else {
			names = append(names, outputName(s, it.Expr))
		}
	}
	return ps, names, types, derivs, typmods, nil
}

// outputName is the output column name of an un-aliased select item (grammar.md §8/§15): a
// bare or qualified column reference takes the catalog's canonical name (never the qualifier,
// never the SELECT spelling); every other expression takes the fixed "?column?". The column
// is known to exist — resolve validated it.
func outputName(s *scope, e exprNode) string {
	switch e.Kind {
	case exprColumn:
		// A whole-row reference to a scalar function relation (no column of the name) is named by
		// the reference itself (PG).
		if r, err := s.resolveBareColumn(e.Column); err == nil {
			return s.columnOf(r).Name
		}
		return e.Column
	case exprQualifiedColumn:
		if r, err := s.resolveQualified(e.Qualifier, e.Column); err == nil {
			return s.columnOf(r).Name
		}
		return e.Column
	case exprFuncCall:
		// An un-aliased aggregate call is named by its exact function name (PG; §8).
		return e.FuncCall.Name
	case exprCoalesce:
		// The fixed keyword lowercased (PG; grammar.md §51) — no expression printer needed.
		return "coalesce"
	case exprGreatestLeast:
		// The fixed keyword lowercased (PG; grammar.md §52).
		if e.Greatest {
			return "greatest"
		}
		return "least"
	case exprFieldAccess:
		// A field selection takes the FIELD name (PG names the output column after the selected
		// field, exact — spec/design/composite.md §S4).
		return e.Field
	case exprSubscript:
		// A subscript takes the base array's name (PG names `a[1]` after `a`); `a[1][2]` recurses to
		// the same base. A non-column base falls through to `?column?`.
		return outputName(s, *e.Base)
	default:
		return "?column?"
	}
}

// orderAliasMatch resolves a bare ORDER BY name against the SELECT output columns — PostgreSQL's
// SQL92 rule that an ORDER BY simple name binds an OUTPUT column (an AS alias or an item's derived
// name — grammar.md §8/§10) BEFORE an input column, the opposite of GROUP BY's precedence. Returns
// the matching select-list item's expression (the caller routes it exactly like the same ordinal:
// a plain column stays on the slot fast path, a computed item is materialized), or nil when no
// output name matches (the caller falls back to the FROM scope, the prior behavior). Matching is
// byte-exact on the folded name (grammar.md §3). Only an explicit list is scanned — with * the output names are the scope
// columns, so the FROM-scope fallback already binds the same column. Two items of the same name
// with DIFFERENT expressions are ambiguous (42702); the same expression twice is not, matching PG.
func orderAliasMatch(items selectItems, name string, s *scope) (*exprNode, error) {
	if items.All {
		return nil, nil
	}
	var found *exprNode
	for i := range items.Items {
		it := &items.Items[i]
		var oname string
		if it.Alias != nil {
			oname = *it.Alias
		} else {
			oname = outputName(s, it.Expr)
		}
		if oname != name {
			continue
		}
		if found == nil {
			found = &it.Expr
		} else if !exprEqual(*found, it.Expr) {
			return nil, newError(AmbiguousColumn, fmt.Sprintf("ORDER BY \"%s\" is ambiguous", name))
		}
	}
	return found, nil
}

// resolveBooleanFilter resolves a WHERE / ON expression; it must resolve to boolean (or an
// untyped NULL, which is always unknown → no rows). An integer- or text-valued one is 42804.
func resolveBooleanFilter(s *scope, e *exprNode, params *paramTypes) (*rExpr, error) {
	// WHERE / ON filters run before any grouping, so an aggregate here is 42803 (Forbidden).
	node, ty, err := resolve(s, *e, nil, &aggCtx{collecting: false}, params)
	if err != nil {
		return nil, err
	}
	if ty.kind != rtBool && ty.kind != rtNull {
		return nil, positionError(typeError("argument of WHERE must be boolean"), exprLocation(*e))
	}
	return node, nil
}

// resolveColumnRef turns a chain resolution into a resolved node + type (§26). A Local column
// obeys the grouping rule (collectColumn); an Outer (correlated) reference is a per-outer-row
// CONSTANT, so it bypasses that rule and resolves to an reOuterColumn reading the enclosing row at
// eval; its type is the ancestor column's.
func resolveColumnRef(s *scope, ag *aggCtx, r resolved, name string) (*rExpr, resolvedType, error) {
	if r.layered {
		return resolveLayerRef(s, ag, r, name)
	}
	if r.level == 0 {
		return collectColumn(s, ag, r.index, name)
	}
	return &rExpr{kind: reOuterColumn, level: r.level, index: r.index}, resolvedTypeOfCol(s.columnOf(r).Type, s.catalog.readSnap()), nil
}

// planSubquery plans a subquery operand against the scope chain (§26). Rejects a non-SELECT
// context (UPDATE/DELETE/INSERT — allowSubquery=false) with 0A000. A $N inside the subquery is
// allowed: the shared params table is threaded into the inner plan, so a parameter typed by an
// inner context (WHERE inner.col = $1) infers statement-wide and unifies with any outer use of the
// same $N. A parameter with NO type context anywhere stays uninferred and finalize raises 42P18 (a
// documented divergence from PostgreSQL, which defaults such a $N to text — grammar.md §26). The
// inner query is resolved ONCE, with `s` as its parent, so correlated references become
// reOuterColumn and errors fire even over an empty outer.
func planSubquery(s *scope, inner queryExpr, params *paramTypes) (queryPlan, error) {
	if !s.allowSubquery {
		return queryPlan{}, newError(FeatureNotSupported, "subqueries are only supported in a SELECT statement")
	}
	// Any subquery makes the enclosing plan un-cacheable: the fold pass rewrites an uncorrelated one
	// (or an uncorrelated one nested inside a correlated one) into a constant using THIS execution's
	// bound params (foldUncorrelatedInSelect), so a reused plan would carry another execution's
	// folded constants. Every subquery form (scalar/EXISTS/IN/quantified) funnels through here.
	params.uncacheable = true
	// A subquery inherits the enclosing scope's CTE bindings directly (cte.md §2): a CTE is
	// visible inside a nested subquery without counting as a correlation level.
	return s.catalog.planQuery(inner, s, s.ctes, params)
}

// planRowSubquery plans a ROW-VALUED subquery (grammar.md §26) whose left operand is a row
// constructor of `arity` fields: the body must yield exactly `arity` columns (fewer / more is 42601,
// PostgreSQL's "subquery has too few / too many columns"). Returns the plan and the anonymous
// composite type its rows take (fields f1…fN over the column types, like a ROW(…) constructor).
func planRowSubquery(s *scope, inner queryExpr, arity int, params *paramTypes) (queryPlan, resolvedType, error) {
	plan, err := planSubquery(s, inner, params)
	if err != nil {
		return queryPlan{}, resolvedType{}, err
	}
	colTypes := plan.columnTypes()
	if len(colTypes) != arity {
		which := "many"
		if len(colTypes) < arity {
			which = "few"
		}
		return queryPlan{}, resolvedType{}, newError(SyntaxError, fmt.Sprintf("subquery has too %s columns", which))
	}
	fields := make([]compositeRField, len(colTypes))
	for i, ty := range colTypes {
		fields[i] = compositeRField{name: fmt.Sprintf("f%d", i+1), ty: ty}
	}
	return plan, resolvedType{kind: rtComposite, comp: &compositeRType{fields: fields}}, nil
}

// planMembershipSubquery plans the body of an IN / ANY / ALL subquery, returning the plan, the
// element type each body row contributes, and whether it is ROW-VALUED. A row-constructor `lhs`
// makes it row-valued (planRowSubquery); any other `lhs` needs exactly one body column (42601
// otherwise).
func planMembershipSubquery(s *scope, lhs exprNode, inner queryExpr, params *paramTypes) (queryPlan, resolvedType, bool, error) {
	if lhs.Kind == exprRow {
		plan, ty, err := planRowSubquery(s, inner, len(lhs.RowItems), params)
		if err != nil {
			return queryPlan{}, resolvedType{}, false, err
		}
		return plan, ty, true, nil
	}
	plan, err := planSubquery(s, inner, params)
	if err != nil {
		return queryPlan{}, resolvedType{}, false, err
	}
	if len(plan.columnTypes()) != 1 {
		return queryPlan{}, resolvedType{}, false, newError(SyntaxError, "subquery has too many columns")
	}
	return plan, plan.columnTypes()[0], false, nil
}

// resolveSubscriptInt resolves one array-subscript bound to an integer rExpr (a literal adapts to
// int4; a non-integer is 42804). A NULL-typed bound is accepted — it evaluates to a NULL subscript
// → NULL result (spec/design/array.md §6).
func resolveSubscriptInt(s *scope, e exprNode, ag *aggCtx, params *paramTypes) (*rExpr, error) {
	idxCtx := scalarInt32
	node, ty, err := resolve(s, e, &idxCtx, ag, params)
	if err != nil {
		return nil, err
	}
	if ty.kind != rtInt && ty.kind != rtNull {
		return nil, typeError(fmt.Sprintf("array subscript must be an integer, not %s", rtName(ty)))
	}
	return node, nil
}

// resolveSubscriptIntPtr resolves an optional (possibly-omitted) slice bound; nil stays nil.
func resolveSubscriptIntPtr(s *scope, e *exprNode, ag *aggCtx, params *paramTypes) (*rExpr, error) {
	if e == nil {
		return nil, nil
	}
	return resolveSubscriptInt(s, *e, ag, params)
}

// resolve resolves one Expr into an rExpr plus its static type. ctx (non-nil) is the
// type an untyped integer literal should adapt to (spec/design/types.md §6); nil
// defaults a bare literal to i64.
func resolve(s *scope, e exprNode, ctx *scalarType, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	r, ty, err := resolveNode(s, e, ctx, ag, params)
	if err != nil {
		return nil, resolvedType{}, locateNodeError(err, e)
	}
	return r, ty, nil
}

// firstPass is an operand's first, hint-free resolution, kept so a second, hinted pass can reuse it
// (reResolve). Two-pass operand resolution (`||`, the containment operators, the polymorphic array
// functions, …) resolves an operand once to learn its type, then again with the element type its
// sibling fixed. Resolving the whole operand twice would resolve every nested two-pass operand twice
// per level, so a nested chain would cost 2^depth before any cost is metered (cost.md §7).
type firstPass struct {
	node *rExpr
	ty   resolvedType
	// shape holds the child results a hint passes through to, for an ARRAY[…], COLLATE, or unary
	// minus whose children the hint can change; the zero value (shapeOpaque) otherwise.
	shape firstShape
}

type firstShapeKind uint8

const (
	shapeOpaque firstShapeKind = iota
	// shapeArray: an ARRAY[…] constructor — each item's type and shape (its node is the array's
	// element, sargs[i]).
	shapeArray
	// shapeWrap: a COLLATE (the node is the operand's) or a unary minus (the node's operand) — the
	// operand's type and shape.
	shapeWrap
)

type firstShape struct {
	kind  firstShapeKind
	items []typedShape // shapeArray
	inner *typedShape  // shapeWrap
}

// typedShape is a child's first-pass type and shape (its node lives in the parent's node).
type typedShape struct {
	ty    resolvedType
	shape firstShape
}

// opaqueFirst takes a resolution as final: a second pass resolves its expression afresh only if it is
// hint-sensitive.
func opaqueFirst(node *rExpr, ty resolvedType) firstPass {
	return firstPass{node: node, ty: ty}
}

// hintSensitive reports whether resolving e under a hint can differ from resolving it with none. The
// hint (ctx) reaches only an integer, decimal, or string literal and a bind parameter, through an
// ARRAY[…], a COLLATE, or a unary minus. (IN / BETWEEN pass ctx to their desugared OR / AND / NOT,
// which ignore it.) Every other node resolves its children with hints of its own, so its resolution
// is the same under any hint: the same node, type, errors, and parameter types (a repeated parameter
// inference is an idempotent join, paramTypes.noteCol).
func hintSensitive(e exprNode) bool {
	switch e.Kind {
	case exprParam:
		return true
	case exprLiteral:
		k := e.Literal.Kind
		return k == literalInt || k == literalDecimal || k == literalText
	case exprArray:
		for i := range e.RowItems {
			if hintSensitive(e.RowItems[i]) {
				return true
			}
		}
		return false
	case exprCollate:
		return hintSensitive(e.Collate.Inner)
	case exprUnary:
		return e.Unary.Op == opNeg && hintSensitive(e.Unary.Operand)
	}
	return false
}

// resolveFirst resolves e with no hint, as resolve(s, e, nil, …), keeping what reResolve needs.
func resolveFirst(s *scope, e exprNode, ag *aggCtx, params *paramTypes) (firstPass, error) {
	f, err := resolveFirstNode(s, e, ag, params)
	if err != nil {
		return firstPass{}, locateNodeError(err, e)
	}
	return f, nil
}

func resolveFirstNode(s *scope, e exprNode, ag *aggCtx, params *paramTypes) (firstPass, error) {
	// A node matching a GROUP BY expression resolves to its group slot under any hint.
	if !hintSensitive(e) || isGroupExpr(ag, e) {
		node, ty, err := resolveNode(s, e, nil, ag, params)
		return opaqueFirst(node, ty), err
	}
	switch {
	case e.Kind == exprArray:
		nodes := make([]*rExpr, len(e.RowItems))
		elemTypes := make([]resolvedType, len(e.RowItems))
		shapes := make([]typedShape, len(e.RowItems))
		for i := range e.RowItems {
			f, err := resolveFirst(s, e.RowItems[i], ag, params)
			if err != nil {
				return firstPass{}, err
			}
			nodes[i] = f.node
			elemTypes[i] = f.ty
			shapes[i] = typedShape{ty: f.ty, shape: f.shape}
		}
		node, ty, err := arrayCtorNode(e.RowItems, nodes, elemTypes)
		if err != nil {
			return firstPass{}, err
		}
		return firstPass{node: node, ty: ty, shape: firstShape{kind: shapeArray, items: shapes}}, nil
	case e.Kind == exprCollate:
		f, err := resolveFirst(s, e.Collate.Inner, ag, params)
		if err != nil {
			return firstPass{}, err
		}
		node, ty, err := collateNode(s, e.Collate.Collation, f.node, f.ty)
		if err != nil {
			return firstPass{}, err
		}
		return firstPass{node: node, ty: ty, shape: firstShape{kind: shapeWrap, inner: &typedShape{ty: f.ty, shape: f.shape}}}, nil
	case e.Kind == exprUnary && e.Unary.Op == opNeg:
		f, err := resolveFirst(s, e.Unary.Operand, ag, params)
		if err != nil {
			return firstPass{}, err
		}
		node, ty, err := negNode(f.node, f.ty)
		if err != nil {
			return firstPass{}, err
		}
		return firstPass{node: node, ty: ty, shape: firstShape{kind: shapeWrap, inner: &typedShape{ty: f.ty, shape: f.shape}}}, nil
	}
	node, ty, err := resolveNode(s, e, nil, ag, params)
	return opaqueFirst(node, ty), err
}

// reResolve resolves e under the hint ctx, as resolve(s, e, ctx, …), given its resolveFirst result.
// Only the literals and parameters the hint reaches are resolved again; every other subexpression
// keeps its first resolution, so each is resolved once however deeply two-pass operands nest.
func reResolve(s *scope, e exprNode, ctx *scalarType, first firstPass, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	if !hintSensitive(e) {
		return first.node, first.ty, nil
	}
	r, ty, err := reResolveNode(s, e, ctx, first, ag, params)
	if err != nil {
		return nil, resolvedType{}, locateNodeError(err, e)
	}
	return r, ty, nil
}

// isGroupExpr reports whether e matches a GROUP BY expression (resolveNode's group-slot rule).
func isGroupExpr(ag *aggCtx, e exprNode) bool {
	_, _, ok := matchGroupExpr(ag, e)
	return ok
}

func reResolveNode(s *scope, e exprNode, ctx *scalarType, first firstPass, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	switch {
	case e.Kind == exprArray && first.shape.kind == shapeArray && first.node.kind == reArray:
		nodes := make([]*rExpr, len(e.RowItems))
		elemTypes := make([]resolvedType, len(e.RowItems))
		for i := range e.RowItems {
			item := first.shape.items[i]
			f := firstPass{node: first.node.sargs[i], ty: item.ty, shape: item.shape}
			node, ty, err := reResolve(s, e.RowItems[i], ctx, f, ag, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			nodes[i] = node
			elemTypes[i] = ty
		}
		return arrayCtorNode(e.RowItems, nodes, elemTypes)
	case e.Kind == exprCollate && first.shape.kind == shapeWrap:
		f := firstPass{node: first.node, ty: first.shape.inner.ty, shape: first.shape.inner.shape}
		inner, ty, err := reResolve(s, e.Collate.Inner, ctx, f, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		return collateNode(s, e.Collate.Collation, inner, ty)
	case e.Kind == exprUnary && e.Unary.Op == opNeg && first.shape.kind == shapeWrap && first.node.kind == reNeg:
		f := firstPass{node: first.node.operand, ty: first.shape.inner.ty, shape: first.shape.inner.shape}
		operand, ty, err := reResolve(s, e.Unary.Operand, ctx, f, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		return negNode(operand, ty)
	}
	// A literal, a parameter, or a node resolved whole the first time: resolve it afresh.
	return resolveNode(s, e, ctx, ag, params)
}

// arrayCtorNode builds an ARRAY[…] constructor from its resolved items (spec/design/array.md §1). The
// item types unify; items that are themselves arrays make a nested (multidim-stacking) constructor
// whose result is the SAME array type (dimension-agnostic, §2/§4), otherwise a flat 1-D array of the
// unified element.
func arrayCtorNode(items []exprNode, nodes []*rExpr, elemTypes []resolvedType) (*rExpr, resolvedType, error) {
	common, err := unifyArrayElementTypes(elemTypes)
	if err != nil {
		return nil, resolvedType{}, positionError(err, mismatchLocation(elemTypes, items, arrayElementTypesUnify))
	}
	if common.kind == rtArray {
		return &rExpr{kind: reArray, sargs: nodes, nested: true}, common, nil
	}
	return &rExpr{kind: reArray, sargs: nodes}, resolvedType{kind: rtArray, elem: &common}, nil
}

// collateNode is `expr COLLATE "name"` over its resolved operand (spec/design/collation.md §1): the
// operand must be collatable (text / text[], 42804, PG-matching) and the named collation must exist
// ("C" or loaded, else 42704). A runtime PASSTHROUGH — a collation only changes the ORDERING
// comparisons / ORDER BY, derived from the AST at those sites — so it returns the operand unchanged.
func collateNode(s *scope, collation string, inner *rExpr, ty resolvedType) (*rExpr, resolvedType, error) {
	if !ty.collatable() && ty.kind != rtNull {
		return nil, resolvedType{}, typeError(fmt.Sprintf("collations are not supported by type %s", rtName(ty)))
	}
	if _, err := resolveCollationName(s.catalog, collation); err != nil {
		return nil, resolvedType{}, err
	}
	return inner, ty, nil
}

// negNode is unary minus over its resolved operand: numeric (integer / decimal / float, keeping the
// width), interval (interval.md §5), or NULL (`-NULL = NULL`); anything else is 42804.
func negNode(rop *rExpr, ty resolvedType) (*rExpr, resolvedType, error) {
	switch ty.kind {
	case rtInt:
		return &rExpr{kind: reNeg, operand: rop, result: ty.intTy},
			resolvedType{kind: rtInt, intTy: ty.intTy}, nil
	case rtDecimal:
		return &rExpr{kind: reNeg, operand: rop, result: scalarDecimal},
			resolvedType{kind: rtDecimal}, nil
	case rtNull:
		return &rExpr{kind: reNeg, operand: rop, result: scalarInt64}, // -NULL = NULL
			resolvedType{kind: rtInt, intTy: scalarInt64}, nil
	case rtInterval:
		return &rExpr{kind: reNeg, operand: rop, result: scalarInterval}, // -interval (interval.md §5)
			resolvedType{kind: rtInterval}, nil
	case rtFloat32:
		return &rExpr{kind: reNeg, operand: rop, result: scalarFloat32}, // -f32 (IEEE sign flip)
			resolvedType{kind: rtFloat32}, nil
	case rtFloat64:
		return &rExpr{kind: reNeg, operand: rop, result: scalarFloat64},
			resolvedType{kind: rtFloat64}, nil
	default: // rtBool, rtText, ...
		return nil, resolvedType{}, typeError("unary minus requires a numeric operand")
	}
}

// locateNodeError decides the position of an error leaving node e's resolution
// (spec/design/error-fields.md §10.2). A function call's 42883 / 42725 points at its name; an
// operator's 42883 / 42804 (no operator for these operand types) points at the operator and carries
// PostgreSQL's operator hint (§11.7). AND / OR / NOT name the offending operand instead, at their
// raise site. A string literal's input error points at the
// literal. An error an operand raised was already decided by that operand's own resolution, so the
// enclosing node never claims it.
func locateNodeError(err error, e exprNode) error {
	var ee *EngineError
	if !errors.As(err, &ee) {
		return err
	}
	operatorError := ee.State == UndefinedFunction || ee.State == DatatypeMismatch
	var pos srcPos
	operatorHint := ""
	switch e.Kind {
	case exprFuncCall:
		if ee.State == UndefinedFunction || ee.State == AmbiguousFunction {
			pos = e.Pos
		}
	case exprBinary:
		if operatorError && e.Binary.Op != opAnd && e.Binary.Op != opOr {
			operatorHint = hintOperatorNoMatch
			if isUnknownLiteral(e.Binary.Lhs) && isUnknownLiteral(e.Binary.Rhs) {
				operatorHint = hintOperatorAmbiguous
			}
			pos = e.Pos
		}
	case exprUnary:
		if operatorError && e.Unary.Op == opNeg {
			operatorHint = hintOperatorNoMatchPrefix
			if isUnknownLiteral(e.Unary.Operand) {
				operatorHint = hintOperatorAmbiguous
			}
			pos = e.Pos
		}
	case exprIsDistinct, exprLike, exprRegex, exprInSubquery, exprQuantified, exprQuantifiedSubquery:
		if operatorError {
			operatorHint = hintOperatorNoMatch
			pos = e.Pos
		}
	case exprLiteral:
		// A string literal adapting to its context (a uuid / bytea / date / json … sibling) is
		// input-converted here, like PostgreSQL's parse-time coercion of an unknown literal.
		if e.Literal.Kind == literalText {
			pos = e.Pos
		}
	}
	// The operator's own type error — not one an operand decided, nor one positioned inside a
	// subquery — is PostgreSQL's "operator does not exist" (§11.7).
	if operatorHint != "" && !ee.located && ee.Position == 0 {
		ee.defaultHint(operatorHint)
	}
	return locateError(err, pos)
}

// isUnknownLiteral reports whether e is an operand PostgreSQL types as unknown: a bare string literal
// or NULL. An operator over only such operands that has no implementation is PostgreSQL's
// ambiguous-operator case (spec/design/error-fields.md §11.7).
func isUnknownLiteral(e exprNode) bool {
	return e.Kind == exprLiteral && (e.Literal.Kind == literalText || e.Literal.Kind == literalNull)
}

// exprLocation is an expression's location for an error that names the whole expression —
// PostgreSQL's exprLocation, the leftmost character the expression was written at, its folded
// grouping parentheses excluded (spec/design/error-fields.md §10.2). 0 when jed did not record it (a
// bind parameter, a subquery, a constructor keyword, stored text): the error then carries no position
// rather than a wrong one.
func exprLocation(e exprNode) srcPos {
	switch e.Kind {
	case exprColumn, exprQualifiedColumn, exprLiteral, exprTypedLiteral, exprUnary, exprCase, exprFuncCall:
		return e.Pos
	case exprCast:
		// A `::` cast is located at its operand (the leftmost token). CAST(x AS t) is located at a
		// string or NULL literal operand (PostgreSQL coerces it in place); any other operand's
		// location depends on whether the coercion is a no-op, so it is unknown.
		in := e.Cast.Inner
		if e.Pos == 0 || (in.Kind == exprLiteral && (in.Literal.Kind == literalText || in.Literal.Kind == literalNull)) {
			return exprLocation(in)
		}
		return 0
	case exprCollate:
		return exprLocation(e.Collate.Inner)
	case exprFieldAccess, exprSubscript:
		return exprLocation(*e.Base)
	case exprIsNull:
		return exprLocation(e.IsNullOf.Operand)
	case exprIsJson:
		return exprLocation(e.IsJsonOf.Operand)
	case exprBinary:
		return exprLocation(e.Binary.Lhs)
	case exprIsDistinct:
		return exprLocation(e.IsDistinct.Lhs)
	case exprIn:
		return exprLocation(e.In.Lhs)
	case exprBetween:
		return exprLocation(e.Between.Lhs)
	case exprLike:
		return exprLocation(e.Like.Lhs)
	case exprRegex:
		return exprLocation(e.Regex.Lhs)
	case exprInSubquery:
		return exprLocation(e.InSubquery.Lhs)
	case exprQuantified:
		return exprLocation(e.Quantified.Lhs)
	case exprQuantifiedSubquery:
		return exprLocation(e.QuantifiedSubquery.Lhs)
	}
	return 0
}

// resolveParamNode resolves the bare parameter e as type ct (nil = no context here), whose resolved
// form is rty, recording the inference.
func resolveParamNode(s *scope, e exprNode, ct *colType, rty resolvedType, params *paramTypes) (*rExpr, resolvedType, error) {
	// A view body has no parameters (views.md §1.1/§4.1): 42P02 where the planner meets it, so
	// earlier resolution errors keep PostgreSQL's order.
	if s != nil && s.catalog != nil && s.catalog.viewTracing() {
		return nil, resolvedType{}, newError(UndefinedParameter, fmt.Sprintf("there is no parameter $%d", e.Param))
	}
	idx0 := int(e.Param) - 1
	if err := params.noteCol(idx0, ct); err != nil {
		return nil, resolvedType{}, err
	}
	var frame *planFrame
	if s != nil {
		frame = s.catalog.currentPlanFrame()
	}
	return &rExpr{kind: reParam, index: idx0, plan: frame}, rty, nil
}

// adoptContainerParam types e, when it is a bare parameter, as the container type other — an array,
// range, or named composite (spec/design/api.md §5, container-typed parameters). ok is false when e
// is not a parameter or other is not such a container.
func adoptContainerParam(s *scope, e exprNode, other resolvedType, params *paramTypes) (*rExpr, resolvedType, bool, error) {
	if e.Kind != exprParam {
		return nil, resolvedType{}, false, nil
	}
	ct, ok := containerParamType(s, other)
	if !ok {
		return nil, resolvedType{}, false, nil
	}
	node, ty, err := resolveParamNode(s, e, &ct, other, params)
	return node, ty, true, err
}

// containerParamType is the parameter type of a container-typed context: an array whose element is a
// scalar or a named composite, a range, or a named composite. ok is false for anything else (a
// scalar, an anonymous record, an untyped NULL).
func containerParamType(s *scope, rt resolvedType) (colType, bool) {
	switch rt.kind {
	case rtArray:
		var elem colType
		if h := ctxOf(*rt.elem); h != nil {
			elem = scalarColType(*h)
		} else if rt.elem.kind == rtComposite && rt.elem.comp.named {
			elem = resolveColType(compositeT(rt.elem.comp.name), s.catalog.readSnap().types)
		} else {
			return colType{}, false
		}
		return colType{Elem: &elem}, true
	case rtRange:
		es, ok := resolvedRangeElementScalar(rt.elem)
		if !ok {
			return colType{}, false
		}
		elem := scalarColType(es)
		return colType{RangeElem: &elem}, true
	case rtComposite:
		if !rt.comp.named {
			return colType{}, false
		}
		return resolveColType(compositeT(rt.comp.name), s.catalog.readSnap().types), true
	}
	return colType{}, false
}

func resolveNode(s *scope, e exprNode, ctx *scalarType, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	// GROUP BY a general expression (aggregates.md §15): a non-column expression that structurally
	// matches a grouping-expression key resolves to that group's synthetic key slot — so `SELECT a+b
	// … GROUP BY a+b` projects the grouped value, like a grouping column. Columns keep their own path
	// (matched by index); an aggregate operand / FILTER resolves under the Forbidden mode (no
	// groupKeyExprs), so this is correctly inert there (its `a+b` is a per-row value, not the group key).
	if e.Kind != exprColumn && e.Kind != exprQualifiedColumn {
		if slot, ty, ok := matchGroupExpr(ag, e); ok {
			return &rExpr{kind: reColumn, index: slot}, ty, nil
		}
	}
	switch e.Kind {
	case exprParam:
		// A bind parameter is an adaptable operand (like an integer/string literal): it takes its
		// type from ctx — the sibling operand, target column, or CAST target. Record the inferred
		// type (nil = no context here; finalize 42P18s a parameter that never gets one).
		if ctx == nil {
			return resolveParamNode(s, e, nil, resolvedType{kind: rtNull}, params)
		}
		ct := scalarColType(*ctx)
		return resolveParamNode(s, e, &ct, resolvedTypeOf(*ctx), params)
	case exprColumn:
		// Resolve against the scope CHAIN (§26). A Local match obeys the grouping rule; an Outer
		// (correlated) match is a per-outer-row constant exempt from it (resolveColumnRef).
		r, err := s.resolveBareAt(e.Column, e.Pos)
		if err != nil {
			return nil, resolvedType{}, err
		}
		return resolveColumnRef(s, ag, r, e.Column)
	case exprQualifiedColumn:
		// A bare `rel.col` resolves strictly against the FROM relations — `qualifier` MUST name a
		// relation (else 42P01), matching PostgreSQL. Composite field access on a column is the
		// **parens-required** `(col).field` form (spec/design/composite.md §1/§S4), an
		// ExprFieldAccess, never this bare qualified-column path (PG raises 42P01 for the
		// unparenthesized `col.field` / `t.col.field` spellings).
		r, err := s.resolveQualifiedAt(e.Qualifier, e.Column, e.Pos)
		if err != nil {
			return nil, resolvedType{}, err
		}
		return resolveColumnRef(s, ag, r, e.Column)
	case exprFieldAccess:
		// `(expr).field` — composite field selection (spec/design/composite.md §S4).
		node, ty, err := resolve(s, *e.Base, nil, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		return resolveFieldOf(node, ty, e.Field)
	case exprFieldStar:
		// `(expr).*` — whole-row expansion is a projection-list construct only; in a scalar
		// expression position it is unsupported (PG rejects row expansion here — 0A000).
		return nil, resolvedType{}, newError(FeatureNotSupported,
			"row expansion (.*) is not supported in this context")
	case exprQualifiedStar:
		// `t.*` is likewise projection-list only — resolveProjections expands it before ever
		// calling resolve(); reaching here means it appeared in a scalar position (which the parser
		// already rejects as 42601). Defensive parity with the FieldStar arm.
		return nil, resolvedType{}, newError(SyntaxError, "t.* is only allowed in a select list")
	case exprSubscript:
		// `base[..][..]` — array subscript (spec/design/array.md §6). The base must be an array
		// (else 42804). Each subscript bound is an integer (a literal adapts; a non-integer is
		// 42804). If any spec is a slice the result is the array type (a sub-array); otherwise it is
		// the element type. OOB / NULL → NULL is an evaluation-time rule, not a resolve error.
		baseNode, baseTy, err := resolve(s, *e.Base, nil, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if baseTy.kind != rtArray {
			return nil, resolvedType{}, typeError(fmt.Sprintf(
				"cannot subscript a value of type %s, which is not an array", rtName(baseTy),
			))
		}
		elemTy := *baseTy.elem
		isSlice := false
		for _, sp := range e.Subscripts {
			if sp.IsSlice {
				isSlice = true
				break
			}
		}
		rsubs := make([]rSubscript, len(e.Subscripts))
		for i, sp := range e.Subscripts {
			if sp.IsSlice {
				lower, err := resolveSubscriptIntPtr(s, sp.Lower, ag, params)
				if err != nil {
					return nil, resolvedType{}, err
				}
				upper, err := resolveSubscriptIntPtr(s, sp.Upper, ag, params)
				if err != nil {
					return nil, resolvedType{}, err
				}
				rsubs[i] = rSubscript{isSlice: true, lower: lower, upper: upper}
			} else {
				idxNode, err := resolveSubscriptInt(s, *sp.Index, ag, params)
				if err != nil {
					return nil, resolvedType{}, err
				}
				rsubs[i] = rSubscript{index: idxNode}
			}
		}
		// A slice yields a sub-array (the array type); all-index access yields an element.
		resTy := elemTy
		if isSlice {
			resTy = baseTy
		}
		return &rExpr{kind: reSubscript, operand: baseNode, subs: rsubs, isSlice: isSlice}, resTy, nil
	case exprRow:
		// A ROW(...) constructor (spec/design/composite.md §1): resolve each field (no context — a
		// field defaults like a bare expression), build the anonymous (structural) composite type
		// (name unset; fields named f1, f2, …) the result types as. Storing it into a named composite
		// column is structural — the materialize/coerce path handles the field-by-field assignment.
		nodes := make([]*rExpr, len(e.RowItems))
		fields := make([]compositeRField, len(e.RowItems))
		for i := range e.RowItems {
			node, ty, err := resolve(s, e.RowItems[i], nil, ag, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			nodes[i] = node
			fields[i] = compositeRField{name: fmt.Sprintf("f%d", i+1), ty: ty}
		}
		return &rExpr{kind: reRow, sargs: nodes}, resolvedType{kind: rtComposite, comp: &compositeRType{fields: fields}}, nil
	case exprArray:
		// An ARRAY[...] constructor (spec/design/array.md §1): resolve each element (natural type),
		// unify to a common element type, build a reArray. A bare empty ARRAY[] has no element type
		// to infer — use '{}'::T[] instead (the cast supplies it).
		if len(e.RowItems) == 0 {
			return nil, resolvedType{}, typeError(
				"cannot determine the element type of an empty ARRAY[]; write '{}'::T[]",
			)
		}
		// An element-type hint (ctx) flows down to the elements so an array literal adapts its
		// untyped integer/decimal literals exactly as a scalar literal does — e.g. resolving
		// ARRAY[7,8] with an i32 context yields i32[], not the default i64[] (the polymorphic
		// array functions pass the bound element type here, array-functions.md §2). Almost every
		// other caller passes nil, so the default 1-D unification is unchanged.
		nodes := make([]*rExpr, len(e.RowItems))
		elemTypes := make([]resolvedType, len(e.RowItems))
		for i := range e.RowItems {
			node, ty, err := resolve(s, e.RowItems[i], ctx, ag, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			nodes[i] = node
			elemTypes[i] = ty
		}
		return arrayCtorNode(e.RowItems, nodes, elemTypes)
	case exprFuncCall:
		// A hypothetical-set aggregate (rank/dense_rank/percent_rank/cume_dist — aggregates.md §19) is
		// one of these window-function names used WITH a WITHIN GROUP clause; that clause routes it
		// here instead of the window path. OVER + WITHIN GROUP together is 0A000.
		if isHypotheticalSetName(e.FuncCall.Name) && e.FuncCall.WithinGroup != nil {
			if e.FuncCall.Over != nil || e.FuncCall.OverName != "" {
				return nil, resolvedType{}, newError(FeatureNotSupported,
					fmt.Sprintf("OVER is not supported for hypothetical-set aggregate %s", e.FuncCall.Name))
			}
			return resolveHypotheticalSetAggregate(s, e.FuncCall, ag, params)
		}
		// An ordered-set aggregate (mode/percentile_cont/percentile_disc — aggregates.md §13)
		// carries WITHIN GROUP and is resolved by its own path. OVER on one is 0A000 (PG itself does
		// not support an ordered-set aggregate as a window function); WITHOUT a WITHIN GROUP it is
		// 42883 (PG: "function mode() does not exist").
		if isOrderedSetAggregateName(e.FuncCall.Name) {
			if e.FuncCall.Over != nil || e.FuncCall.OverName != "" {
				return nil, resolvedType{}, newError(FeatureNotSupported,
					fmt.Sprintf("OVER is not supported for ordered-set aggregate %s", e.FuncCall.Name))
			}
			if e.FuncCall.WithinGroup == nil {
				return nil, resolvedType{}, noAggOverload(e.FuncCall.Name)
			}
			return resolveOrderedSetAggregate(s, e.FuncCall, ag, params)
		}
		// WITHIN GROUP on a non-ordered-set function (an ordinary aggregate or a scalar function) is
		// 42883 — PG models it as a missing overload (`sum(numeric, numeric) does not exist`).
		if e.FuncCall.WithinGroup != nil {
			return nil, resolvedType{}, noAggOverload(e.FuncCall.Name)
		}
		// A trailing OVER makes this a window-function call (spec/design/window.md §5.1).
		if e.FuncCall.Over != nil {
			if e.FuncCall.Name == "grouping" {
				// GROUPING is not a window function — GROUPING(a) OVER () is a syntax error in
				// PostgreSQL (42601); match it rather than treating GROUPING as an unknown window fn.
				return nil, resolvedType{}, newError(SyntaxError, "OVER is not supported for GROUPING")
			}
			// DISTINCT is not implemented for window functions (PG 0A000 — aggregates.md §5):
			// a window aggregate folds over a frame, where per-frame de-duplication is undefined.
			if e.FuncCall.Distinct {
				return nil, resolvedType{}, newError(FeatureNotSupported,
					"DISTINCT is not implemented for window functions")
			}
			// Nor is an in-aggregate ORDER BY (PG 0A000 — aggregates.md §22): the window frame, not an
			// input sort, orders a window aggregate's fold.
			if e.FuncCall.AggOrder != nil {
				return nil, resolvedType{}, newError(FeatureNotSupported,
					"aggregate ORDER BY is not implemented for window functions")
			}
			// FILTER over a window function (aggregates.md §20). A window AGGREGATE folds only the
			// frame rows for which the filter is TRUE; a pure (non-aggregate) window function with
			// FILTER is PG's own 0A000 ("FILTER is not implemented for non-aggregate window
			// functions"). The filter is threaded into the windowSpec and applied in the window stage.
			if e.FuncCall.Filter != nil && !isAggregateName(e.FuncCall.Name) {
				return nil, resolvedType{}, newError(FeatureNotSupported,
					"FILTER is not implemented for non-aggregate window functions")
			}
			return resolveWindowCall(s, e.FuncCall, e.FuncCall.Filter, ag, params)
		}
		// A window-only function (row_number/…) used WITHOUT OVER is 42809 (PG's wrong_object_type,
		// not the windowing_error 42P20 it uses for a window in WHERE — window.md §7, oracle-verified).
		if isWindowOnlyName(e.FuncCall.Name) {
			return nil, resolvedType{}, newError(WrongObjectType,
				fmt.Sprintf("window function %s requires an OVER clause", e.FuncCall.Name))
		}
		return resolveFuncCall(s, e.FuncCall, ag, params)
	case exprLiteral:
		switch e.Literal.Kind {
		case literalNull:
			return &rExpr{kind: reConstNull}, resolvedType{kind: rtNull}, nil
		case literalBool:
			return &rExpr{kind: reConstBool, cBool: e.Literal.Bool}, resolvedType{kind: rtBool}, nil
		case literalText:
			// A string literal is text by default (collation C). It adapts to a BYTEA or a UUID
			// context (types.md §6/§13/§14): decode the bytea input (hex or escape) or the
			// PG-flexible uuid input (uuid) — 22P02 on malformed; any other context — including none —
			// keeps it text.
			// A string literal is text by default (collation C). It adapts to a BYTEA context (hex or
			// escape input, 22P02), a UUID context (PG-flexible input, 22P02 — types.md §6/§13/§14), or a
			// TIMESTAMP/TIMESTAMPTZ context (parse the datetime, 22007/22008 — spec/design/timestamp.md).
			switch {
			case ctx != nil && ctx.IsBytea():
				b, err := decodeByteaLiteral(e.Literal.Str)
				if err != nil {
					return nil, resolvedType{}, err
				}
				return &rExpr{kind: reConstBytea, cBytea: b}, resolvedType{kind: rtBytea}, nil
			case ctx != nil && ctx.IsUuid():
				b, err := decodeUUIDLiteral(e.Literal.Str)
				if err != nil {
					return nil, resolvedType{}, err
				}
				return &rExpr{kind: reConstUuid, cBytea: b}, resolvedType{kind: rtUuid}, nil
			case ctx != nil && ctx.IsTimestamp():
				m, err := parseTimestamp(e.Literal.Str)
				if err != nil {
					return nil, resolvedType{}, err
				}
				return &rExpr{kind: reConstTimestamp, cInt: m}, resolvedType{kind: rtTimestamp}, nil
			case ctx != nil && ctx.IsTimestamptz():
				// An offset-less string reads in the SESSION zone, as PostgreSQL's timestamptz_in
				// does (timezones.md §9.3).
				iz := s.catalog.inputZone()
				m, err := iz.parseTimestamptz(e.Literal.Str)
				if err != nil {
					return nil, resolvedType{}, err
				}
				params.noteInputZone(iz)
				return &rExpr{kind: reConstTimestamptz, cInt: m}, resolvedType{kind: rtTimestamptz}, nil
			case ctx != nil && ctx.IsDate():
				// A string adapts to a DATE context (parse the ISO date, dropping any time/offset;
				// 22007/22008 — spec/design/date.md §2), like timestamp adaptation. A clock-relative
				// special ('today'/'now'/…) becomes the STABLE reDateClock node instead of a
				// constant (date.md §6).
				if node, rt, ok := dateClockLiteral(e.Literal.Str, params); ok {
					return node, rt, nil
				}
				m, err := parseDate(e.Literal.Str)
				if err != nil {
					return nil, resolvedType{}, err
				}
				return &rExpr{kind: reConstDate, cInt: int64(m)}, resolvedType{kind: rtDate}, nil
			case ctx != nil && ctx.IsInterval():
				// A string adapts to an INTERVAL context (PostgreSQL's interval input,
				// 22007/22008 — spec/design/interval.md), like timestamp adaptation.
				iv, err := parseInterval(e.Literal.Str)
				if err != nil {
					return nil, resolvedType{}, err
				}
				return &rExpr{kind: reConstInterval, cIv: iv}, resolvedType{kind: rtInterval}, nil
			case ctx != nil && ctx.IsJson():
				// A string literal adapts to a json context (the sibling of a json column / a json
				// cast), so `jsoncol = '{"a":1}'` compares json × json; malformed → 22P02
				// (spec/design/json.md §4). json validates + stores verbatim.
				if err := validateJSON(e.Literal.Str); err != nil {
					return nil, resolvedType{}, err
				}
				return &rExpr{kind: reConstJson, cText: e.Literal.Str}, resolvedType{kind: rtJson}, nil
			case ctx != nil && ctx.IsJsonb():
				// A string literal adapts to a jsonb context (the sibling of a jsonb column / a jsonb
				// cast), so `jsonbcol = '{"a":1}'` compares jsonb × jsonb; malformed → 22P02
				// (spec/design/json.md §2). jsonb canonicalizes.
				node, err := jsonbIn(e.Literal.Str)
				if err != nil {
					return nil, resolvedType{}, err
				}
				return &rExpr{kind: reConstJsonb, cJsonb: &node}, resolvedType{kind: rtJsonb}, nil
			case ctx != nil && ctx.IsJsonPath():
				// A string literal adapts to a jsonpath context (a jsonpath function argument) — it
				// is compiled to a path at resolve (jsonpath.md §1); malformed → 42601.
				jp, err := compile(e.Literal.Str, s.catalog.session.env.propertyTable())
				if err != nil {
					return nil, resolvedType{}, err
				}
				return &rExpr{kind: reConstJsonPath, cText: jp.Render()}, resolvedType{kind: rtJsonPath}, nil
			}
			return &rExpr{kind: reConstText, cText: e.Literal.Str}, resolvedType{kind: rtText}, nil
		case literalDecimal:
			// A decimal literal is decimal by default, but ADAPTS to a FLOAT context: in a
			// f64/f32 column/operand context it coerces decimal→float at resolve (the
			// nearest binary value, round-ties-to-even — spec/design/float.md §4). Any other
			// context keeps it decimal (cap-checked, 22003 on an over-long coefficient/scale).
			if ctx != nil && ctx.IsFloat() {
				return floatConstFromDecimal(e.Literal.Dec, *ctx)
			}
			d, err := e.Literal.Dec.CheckCap()
			if err != nil {
				return nil, resolvedType{}, err
			}
			return &rExpr{kind: reConstDecimal, cDec: d}, resolvedType{kind: rtDecimal}, nil
		default: // LiteralInt
			// An integer literal adapts to an integer context, OR to a FLOAT context (int→float
			// at resolve — float.md §4). A non-integer/non-float context defaults to i64, and
			// the surrounding check then reports the mismatch (42804) / widens it (int→decimal).
			if ctx != nil && ctx.IsFloat() {
				if ctx.IsFloat32() {
					return &rExpr{kind: reConstFloat32, cFloat: float64(intToFloat32(e.Literal.Int))},
						resolvedType{kind: rtFloat32}, nil
				}
				return &rExpr{kind: reConstFloat64, cFloat: intToFloat64(e.Literal.Int)},
					resolvedType{kind: rtFloat64}, nil
			}
			ty := scalarInt64
			if ctx != nil && ctx.IsInteger() {
				ty = *ctx
			}
			if !ty.InRange(e.Literal.Int) {
				return nil, resolvedType{}, overflowErr(ty)
			}
			return &rExpr{kind: reConstInt, cInt: e.Literal.Int},
				resolvedType{kind: rtInt, intTy: ty}, nil
		}
	case exprTypedLiteral:
		// A typed string literal `type '...'` (spec/design/grammar.md §36) — PostgreSQL's
		// `type 'string'`, equal to CAST('string' AS type) over a string-literal operand. Resolve
		// the type by name (unknown → 42704) and coerce the string to it at resolve, context-free.
		// No typmod rides on the literal (the parser's one-token lookahead admits none).
		//
		// A composite type name (`addr '(Main,90210)'`) coerces the string via record_in
		// (spec/design/composite.md §8) — the same primitive as `'(…)'::addr`.
		if ct := s.catalog.readSnap().compositeType(e.TypeLitName); ct != nil {
			iz := s.catalog.inputZone()
			node, rt, err := coerceStringToComposite(e.TypeLitText, ct, s.catalog, iz)
			params.noteInputZone(iz)
			return node, rt, positionError(err, e.Pos)
		}
		// A range type name (`i32range '[1,5)'`, `int4range '…'`) coerces the string via range_in
		// against the element type (spec/design/ranges.md §5) — the same primitive as the cast.
		if desc, ok := rangeByName(e.TypeLitName); ok {
			iz := s.catalog.inputZone()
			node, rt, err := coerceStringToRangeExpr(e.TypeLitText, desc, iz)
			params.noteInputZone(iz)
			return node, rt, positionError(err, e.Pos)
		}
		target, _, _, err := resolveTypeAndTypmod(e.TypeLitName, nil)
		if err != nil {
			return nil, resolvedType{}, err
		}
		// DATE 'today' / DATE 'now' / … — the clock-relative specials become the STABLE
		// reDateClock node, exactly like the ctx-adaptation form (date.md §6).
		if target.IsDate() {
			if node, rt, ok := dateClockLiteral(e.TypeLitText, params); ok {
				return node, rt, nil
			}
		}
		iz := s.catalog.inputZone()
		node, rt, err := coerceStringLiteral(e.TypeLitText, target, nil, nil, iz)
		params.noteInputZone(iz)
		return node, rt, positionError(err, e.Pos)
	case exprScalarSubquery:
		// A subquery in expression position (§26): PLANNED ONCE against the scope chain here, so
		// its column-count / type errors fire even over an empty outer. planSubquery rejects a
		// non-SELECT context and a $N inside (both 0A000). The fold pass folds an uncorrelated one
		// to a constant; a correlated one is re-executed per outer row by the evaluator.
		plan, err := planSubquery(s, *e.Subquery, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if len(plan.columnTypes()) != 1 {
			return nil, resolvedType{}, newError(SyntaxError, "subquery must return only one column")
		}
		outType := plan.columnTypes()[0]
		noteSubqueryDeriv(s, e.Subquery, plan.columnDerivs()[0], plan.columnTypmods()[0])
		return &rExpr{kind: reSubquery, subPlan: &plan, subKind: sqScalar}, outType, nil
	case exprExists:
		// EXISTS ignores the select list entirely; the result is boolean, never NULL. A NOT
		// EXISTS parses as the unary NOT wrapping this, so negated here is always false.
		plan, err := planSubquery(s, *e.Subquery, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		return &rExpr{kind: reSubquery, subPlan: &plan, subKind: sqExists}, resolvedType{kind: rtBool}, nil
	case exprInSubquery:
		// The LHS is an OUTER expression (resolved in the current scope / agg context); the
		// subquery yields the single membership column. The test is `lhs = element`, so the pair
		// must be comparable (42804), exactly like a literal IN. A row-constructor LHS makes it
		// ROW-VALUED: N body columns, each row one anonymous composite element.
		is := e.InSubquery
		rlhs, lt, err := resolve(s, is.Lhs, nil, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		plan, elem, rowValued, err := planMembershipSubquery(s, is.Lhs, is.Query, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if err := classifyComparable(lt, elem); err != nil {
			return nil, resolvedType{}, err
		}
		// `=` ignores the collation at eval (byte identity, collation.md §7), but a conflict between
		// the operand and the body column is still 42P22 (PostgreSQL's texteq demands one).
		if _, err := membershipCollation(s, is.Lhs, lt, elem, rowValued, &plan); err != nil {
			return nil, resolvedType{}, err
		}
		return &rExpr{kind: reSubquery, subPlan: &plan, subKind: sqIn, lhs: rlhs, negated: is.Negated, rowValued: rowValued}, resolvedType{kind: rtBool}, nil
	case exprCollate:
		// `expr COLLATE "name"` (spec/design/collation.md §1) — a postfix collation operator. Resolve
		// the inner expression, require a collatable (text / text[]) type (42804, PG-matching), and validate
		// the named collation exists ("C" or loaded, else 42704). A runtime PASSTHROUGH: a collation
		// only changes the ORDERING comparisons / ORDER BY, derived from the AST at those sites
		// (explicitCollation / OrderKey.Collation), so resolving returns the inner node + type
		// unchanged. The hint flows through (COLLATE never changes the type).
		inner, ty, err := resolve(s, e.Collate.Inner, ctx, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		return collateNode(s, e.Collate.Collation, inner, ty)
	case exprExtract:
		// EXTRACT(field FROM source) (timezones.md §9.2, grammar.md §50). The field is SYNTACTIC and
		// validated at RESOLVE (not per row): an unsupported field for the source type is 0A000, an
		// unrecognized field is 22023 — surfaced by probing the kernel with a zero value of the source's
		// family. The source must be a datetime type (else 42883); the result is numeric.
		srcR, srcT, err := resolve(s, e.Extract.Source, nil, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		// A NULL source has no resolvable family; the value propagates to NULL at eval (the field is
		// not validated — a documented narrow edge vs. PG).
		if srcT.kind != rtNull {
			var probe extractSrc
			switch srcT.kind {
			case rtTimestamp:
				probe = extractSrc{kind: srcTs}
			case rtTimestamptz:
				probe = extractSrc{kind: srcTstz}
			case rtDate:
				probe = extractSrc{kind: srcDate}
			case rtInterval:
				probe = extractSrc{kind: srcIv}
			default:
				return nil, resolvedType{}, newError(UndefinedFunction,
					"function extract(text, "+rtName(srcT)+") does not exist").hinted(hintFunctionNoMatch)
			}
			if _, _, err := extractField(e.Extract.Field, probe); err != nil {
				return nil, resolvedType{}, err
			}
		}
		params.nonimmutable = params.nonimmutable || (srcT.kind == rtTimestamptz && !strings.EqualFold(e.Extract.Field, "epoch"))
		return &rExpr{kind: reExtract, cText: e.Extract.Field, operand: srcR}, resolvedType{kind: rtDecimal}, nil
	case exprCast:
		// An array cast target `…::T[]` (spec/design/array.md §7). v1 supports only the
		// string-literal form `'{…}'::T[]` and a bare NULL; every other array cast (runtime
		// text→array, array→text, element-wise array→array) is a documented 0A000 narrowing.
		if base, ok := strings.CutSuffix(e.Cast.TypeName, "[]"); ok {
			if e.Cast.TypeMod != nil {
				return nil, resolvedType{}, newError(FeatureNotSupported,
					"a type modifier on an array type is not supported yet")
			}
			snap := s.catalog.readSnap()
			var elemCol colType
			var elemRT resolvedType
			if elemScalar, scalarOK := scalarTypeFromName(base); scalarOK {
				elemCol = scalarColType(elemScalar)
				elemRT = resolvedTypeOf(elemScalar)
			} else if ctype := snap.compositeType(base); ctype != nil {
				elemTy := compositeT(ctype.Name)
				elemCol = resolveColType(elemTy, snap.types)
				elemRT = resolvedTypeOfCol(elemTy, snap)
			} else {
				return nil, resolvedType{}, newError(UndefinedObject, "type does not exist: "+base)
			}
			if in := e.Cast.Inner; in.Kind == exprLiteral && in.Literal != nil && in.Literal.Kind == literalText {
				iz := s.catalog.inputZone()
				val, err := coerceStringToArray(in.Literal.Str, elemCol, iz)
				if err != nil {
					return nil, resolvedType{}, positionError(err, in.Pos)
				}
				params.noteInputZone(iz)
				return valueToRExpr(val), resolvedType{kind: rtArray, elem: &elemRT}, nil
			}
			if in := e.Cast.Inner; in.Kind == exprLiteral && in.Literal != nil && in.Literal.Kind == literalNull {
				return &rExpr{kind: reConstNull}, resolvedType{kind: rtArray, elem: &elemRT}, nil
			}
			// A bind parameter cast to an array type takes that type (api.md §5).
			if e.Cast.Inner.Kind == exprParam {
				ct := colType{Elem: &elemCol}
				return resolveParamNode(s, e.Cast.Inner, &ct, resolvedType{kind: rtArray, elem: &elemRT}, params)
			}
			// A runtime (non-literal) operand: the two follow-on array-producing casts (array.md §7).
			// A text expression coerces per row via array_in (runtime text→T[]); an array of the SAME
			// element type is the identity (no node); an array of a DIFFERENT element type is an
			// element-wise array→array cast (each element through the scalar cast, when the element
			// pair is castable); a non-literal NULL adapts. Any other source is a 42804.
			rinner, ity, err := resolve(s, e.Cast.Inner, nil, ag, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			resultRT := resolvedType{kind: rtArray, elem: &elemRT}
			switch ity.kind {
			case rtNull:
				return rinner, resultRT, nil
			case rtText:
				ec := elemCol
				return &rExpr{kind: reArrayCast, operand: rinner, castElem: &ec}, resultRT, nil
			case rtArray:
				if resolvedTypeEqual(*ity.elem, elemRT) {
					return rinner, resultRT, nil // identity cast — same element type
				}
				srcS, srcScalar := resolvedToScalar(*ity.elem)
				tgtScalar := !elemCol.Composite && elemCol.Elem == nil && elemCol.RangeElem == nil
				if srcScalar && tgtScalar && scalarPairCastable(srcS, elemCol.Scalar) {
					// timestamptz elements render in the session zone (STABLE).
					params.nonimmutable = params.nonimmutable || (srcS == scalarTimestamptz && elemCol.Scalar.IsText())
					ec := elemCol
					return &rExpr{kind: reArrayCast, operand: rinner, castElem: &ec}, resultRT, nil
				}
				// A composite element on either side is the deferred composite cast surface (0A000).
				if !srcScalar || elemCol.Composite {
					return nil, resolvedType{}, newError(FeatureNotSupported,
						"casting between composite-element arrays is not supported yet")
				}
				// Both elements are scalars but no cast exists between them — forbidden (42804;
				// jed's strict-matrix convention, PG reports 42846).
				return nil, resolvedType{}, typeError("cannot cast " + rtName(ity) + " to " + base + "[]")
			default:
				return nil, resolvedType{}, typeError("cannot cast " + rtName(ity) + " to " + base + "[]")
			}
		}
		// A range cast target (`'[1,5)'::i32range`, `…::int4range`). Like array, v1 supports the
		// string-literal form and a bare NULL; every other range cast is a 0A000 narrowing
		// (spec/design/ranges.md §1/§5).
		if desc, ok := rangeByName(e.Cast.TypeName); ok {
			if e.Cast.TypeMod != nil {
				return nil, resolvedType{}, newError(FeatureNotSupported,
					"a type modifier on a range type is not supported")
			}
			elemRT := resolvedTypeOf(elementScalar(desc))
			if in := e.Cast.Inner; in.Kind == exprLiteral && in.Literal != nil && in.Literal.Kind == literalText {
				iz := s.catalog.inputZone()
				node, rt, err := coerceStringToRangeExpr(in.Literal.Str, desc, iz)
				params.noteInputZone(iz)
				return node, rt, positionError(err, in.Pos)
			}
			if in := e.Cast.Inner; in.Kind == exprLiteral && in.Literal != nil && in.Literal.Kind == literalNull {
				return &rExpr{kind: reConstNull}, resolvedType{kind: rtRange, elem: &elemRT}, nil
			}
			// A bind parameter cast to a range type takes that type (api.md §5).
			if e.Cast.Inner.Kind == exprParam {
				elemCol := scalarColType(elementScalar(desc))
				ct := colType{RangeElem: &elemCol}
				return resolveParamNode(s, e.Cast.Inner, &ct, resolvedType{kind: rtRange, elem: &elemRT}, params)
			}
			return nil, resolvedType{}, newError(FeatureNotSupported,
				"casting to a range type is only supported from a string literal this slice")
		}
		// A composite cast target (`'(…)'::addr`) — a CREATE TYPE name, not a built-in scalar
		// (spec/design/composite.md §8). A STRING LITERAL operand coerces via record_in (the
		// `'(…)'::addr` headline); a bare NULL adapts to the composite; a same-named composite
		// operand is the identity. Every other operand (a runtime text expression, an anonymous
		// ROW(…)) is a documented 0A000 narrowing this slice — relaxable. A type modifier on a
		// composite is meaningless (0A000).
		if ct := s.catalog.readSnap().compositeType(e.Cast.TypeName); ct != nil {
			if e.Cast.TypeMod != nil {
				return nil, resolvedType{}, newError(FeatureNotSupported,
					"a type modifier is not supported on a composite type")
			}
			if in := e.Cast.Inner; in.Kind == exprLiteral && in.Literal != nil && in.Literal.Kind == literalText {
				iz := s.catalog.inputZone()
				node, rt, err := coerceStringToComposite(in.Literal.Str, ct, s.catalog, iz)
				params.noteInputZone(iz)
				return node, rt, positionError(err, in.Pos)
			}
			// A bind parameter cast to a composite type takes that type (api.md §5).
			if e.Cast.Inner.Kind == exprParam {
				snap := s.catalog.readSnap()
				pct := resolveColType(compositeT(ct.Name), snap.types)
				return resolveParamNode(s, e.Cast.Inner, &pct, resolvedTypeOfCol(compositeT(ct.Name), snap), params)
			}
			rinner, ity, err := resolve(s, e.Cast.Inner, nil, ag, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			switch {
			case ity.kind == rtNull:
				return rinner, resolvedTypeOfCol(compositeT(ct.Name), s.catalog.readSnap()), nil
			case ity.kind == rtComposite && ity.comp.named && ity.comp.name == ct.Name:
				// An identical named composite is the identity cast.
				return rinner, ity, nil
			default:
				return nil, resolvedType{}, newError(FeatureNotSupported,
					"casting to a composite type is only supported from a string literal")
			}
		}
		target, typmod, varcharLen, err := resolveTypeAndTypmod(e.Cast.TypeName, e.Cast.TypeMod)
		if err != nil {
			return nil, resolvedType{}, err
		}
		// A string LITERAL operand is coerced to the target at resolve — CAST('42' AS int), the
		// same primitive as the `type 'string'` typed literal (grammar.md §36, types.md §5). The
		// ONLY text→T cast admitted ahead of the general cast slice; a non-literal text operand
		// still falls through to the deferred 0A000 below. A varchar(n) target truncates the literal
		// to n code points (types.md §15).
		if in := e.Cast.Inner; in.Kind == exprLiteral && in.Literal != nil && in.Literal.Kind == literalText {
			// 'today'::date / CAST('now' AS date) — the clock-relative specials become the STABLE
			// reDateClock node, exactly like the ctx-adaptation form (date.md §6).
			if target.IsDate() {
				if node, rt, ok := dateClockLiteral(in.Literal.Str, params); ok {
					return node, rt, nil
				}
			}
			iz := s.catalog.inputZone()
			// The literal's input error points at the literal (PostgreSQL coerces it at parse time —
			// spec/design/error-fields.md §10.2).
			node, rt, err := coerceStringLiteral(in.Literal.Str, target, typmod, varcharLen, iz)
			params.noteInputZone(iz)
			return node, rt, positionError(err, in.Pos)
		}
		// The JSON cast matrix (spec/design/json.md §6.1): casting TO json/jsonb from a runtime
		// text/json/jsonb expression (a string LITERAL operand was already coerced above by
		// coerceStringLiteral). text → json validates + stores verbatim; text → jsonb parses +
		// canonicalizes; json → jsonb re-parses + canonicalizes; jsonb → json renders the canonical
		// text; same-type is the identity. Any other source is a 42804 cast error (jed's invalid-cast
		// convention; PG reports 42846 — a documented divergence).
		if target.IsJson() || target.IsJsonb() {
			if e.Cast.Inner.Kind == exprParam {
				t := target
				pinner, _, err := resolve(s, e.Cast.Inner, &t, ag, params)
				if err != nil {
					return nil, resolvedType{}, err
				}
				return pinner, resolvedTypeOf(target), nil
			}
			rinner, ity, err := resolve(s, e.Cast.Inner, nil, ag, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			toRt := resolvedTypeOf(target)
			switch ity.kind {
			case rtNull:
				return rinner, toRt, nil
			case rtText, rtJson, rtJsonb:
				return &rExpr{kind: reCast, operand: rinner, result: target}, toRt, nil
			default:
				return nil, resolvedType{}, typeError(
					"cannot cast type " + rtName(ity) + " to " + target.CanonicalName(),
				)
			}
		}
		// Casting TO text (casts.toml, types.md §5): every built-in scalar renders its own output form,
		// the text a result cell shows — json verbatim, jsonb canonical, uuid lowercase, bytea \x hex,
		// timestamptz in the SESSION zone (timezones.md §9.5). Explicit only — stricter than PG's
		// assignment cast to text (a documented divergence), so INSERT INTO text_col VALUES (1) /
		// text_col = 1 stay 42804. A varchar(n) length truncates the rendered text (types.md §15).
		// `$1::text` declares the parameter as text, like every other cast target (api.md §5).
		// Composite and range sources stay 0A000.
		if target.IsText() {
			var innerCtx *scalarType
			if e.Cast.Inner.Kind == exprParam {
				t := scalarText
				innerCtx = &t
			}
			rinner, ity, err := resolve(s, e.Cast.Inner, innerCtx, ag, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			cast, err := castToText(rinner, ity, target, varcharLen, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			return cast, resolvedType{kind: rtText}, nil
		}
		// A boolean target (`CAST(x AS boolean)`, `x::boolean`) is the boolean cast slice
		// (spec/types/casts.toml, types.md §9). It needs the inner type to decide (only an i32 /
		// NULL / bool source is castable), so it is handled AFTER the inner is resolved, below.
		// A bytea TARGET: runtime text → bytea decodes the per-row string with the literal's own bytea
		// input (types.md §5/§13 — immutable, like PG's byteain), and the uuid cast slice admits uuid →
		// bytea (the 16 raw bytes — a jed cast PG lacks; casts.toml, types.md §14). A string LITERAL
		// was coerced above; a NULL adapts; a bytea operand is the identity. Any other source is a
		// forbidden pair (42804).
		if target.IsBytea() {
			if e.Cast.Inner.Kind == exprParam {
				t := scalarBytea
				pinner, _, err := resolve(s, e.Cast.Inner, &t, ag, params)
				if err != nil {
					return nil, resolvedType{}, err
				}
				return pinner, resolvedType{kind: rtBytea}, nil
			}
			rinner, ity, err := resolve(s, e.Cast.Inner, nil, ag, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			switch ity.kind {
			case rtNull, rtBytea:
				return rinner, resolvedType{kind: rtBytea}, nil
			case rtText, rtUuid:
				return &rExpr{kind: reCast, operand: rinner, result: target}, resolvedType{kind: rtBytea}, nil
			default:
				return nil, resolvedType{}, typeError("cannot cast " + rtName(ity) + " to bytea")
			}
		}
		// The uuid cast slice (spec/types/casts.toml, types.md §14): a uuid TARGET from a runtime text
		// or bytea expression. text → uuid runs uuid_in at eval (22P02 on malformed); bytea → uuid takes
		// the 16 raw bytes (22P02 on a length ≠ 16) — a jed cast PG lacks. A string LITERAL operand was
		// coerced above (the §6 adaptation); $1::uuid declares the param as uuid; a NULL adapts; a uuid
		// operand is the identity.
		if target.IsUuid() {
			if e.Cast.Inner.Kind == exprParam {
				t := scalarUuid
				pinner, _, err := resolve(s, e.Cast.Inner, &t, ag, params)
				if err != nil {
					return nil, resolvedType{}, err
				}
				return pinner, resolvedType{kind: rtUuid}, nil
			}
			rinner, ity, err := resolve(s, e.Cast.Inner, nil, ag, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			switch ity.kind {
			case rtNull, rtUuid:
				return rinner, resolvedType{kind: rtUuid}, nil
			case rtText, rtBytea:
				return &rExpr{kind: reCast, operand: rinner, result: target}, resolvedType{kind: rtUuid}, nil
			default:
				return nil, resolvedType{}, typeError("cannot cast " + rtName(ity) + " to uuid")
			}
		}
		// Datetime casts (timezones.md §9.3, types.md §5): a timestamp/timestamptz/date TARGET from
		// another datetime family or from runtime text. A same-family cast is the identity; a
		// cross-family or text cast becomes a reDateConvert node (the zone-crossing ones read the
		// session zone at eval); any other source is a forbidden pair (42804). A NULL operand adapts
		// to the target.
		if target.IsTimestamp() || target.IsTimestamptz() || target.IsDate() {
			if e.Cast.Inner.Kind == exprParam {
				t := target
				pinner, _, err := resolve(s, e.Cast.Inner, &t, ag, params)
				if err != nil {
					return nil, resolvedType{}, err
				}
				return pinner, resolvedTypeOf(target), nil
			}
			inner, ity, err := resolve(s, e.Cast.Inner, nil, ag, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			toRt := resolvedTypeOf(target)
			switch {
			case ity.kind == rtNull:
				return inner, toRt, nil
			case ity.kind == rtTimestamp && target.IsTimestamp(),
				ity.kind == rtTimestamptz && target.IsTimestamptz(),
				ity.kind == rtDate && target.IsDate():
				return inner, ity, nil
			case ity.kind == rtTimestamp || ity.kind == rtTimestamptz || ity.kind == rtDate:
				params.nonimmutable = params.nonimmutable || ity.kind == rtTimestamptz || target == scalarTimestamptz
				return &rExpr{kind: reDateConvert, operand: inner, result: target}, toRt, nil
			case ity.kind == rtText:
				// The runtime text → date / timestamp / timestamptz casts (date.md §6, types.md §5):
				// a NON-literal text source (a string LITERAL operand was already folded by the
				// literal-adaptation path above) parses per row via the literal's own parser
				// (22007/22008 per row); text → timestamptz reads an offset-less string in the session
				// zone. STABLE, not immutable — like PostgreSQL's date_in / timestamp_in /
				// timestamptz_in — so it flags the plan non-immutable (42P17 in an index expression).
				params.nonimmutable = true
				return &rExpr{kind: reDateConvert, operand: inner, result: target}, toRt, nil
			default:
				return nil, resolvedType{}, typeError("cannot cast " + rtName(ity) + " to " + target.CanonicalName())
			}
		}
		// An interval TARGET (interval.md §6, types.md §5): runtime text → interval parses the per-row
		// string with the literal's own parseInterval (22007/22008 per row) — STABLE, like
		// PostgreSQL's interval_in, so it flags the plan non-immutable (42P17 in an index
		// expression). An interval operand is the identity; $1::interval declares the param as
		// interval; a NULL adapts. Any other source is a forbidden pair (42804).
		if target.IsInterval() {
			if e.Cast.Inner.Kind == exprParam {
				t := scalarInterval
				pinner, _, err := resolve(s, e.Cast.Inner, &t, ag, params)
				if err != nil {
					return nil, resolvedType{}, err
				}
				return pinner, resolvedType{kind: rtInterval}, nil
			}
			rinner, ity, err := resolve(s, e.Cast.Inner, nil, ag, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			switch ity.kind {
			case rtNull, rtInterval:
				return rinner, resolvedType{kind: rtInterval}, nil
			case rtText:
				params.nonimmutable = true
				return &rExpr{kind: reCast, operand: rinner, result: target}, resolvedType{kind: rtInterval}, nil
			default:
				return nil, resolvedType{}, typeError("cannot cast " + rtName(ity) + " to interval")
			}
		}
		// A bind-parameter operand takes the cast TARGET as its inferred type — `$1::int` (and
		// `CAST($1 AS int)`) declares `$1` as int, the cast-target parameter-typing case
		// (spec/design/api.md §5, grammar.md §37). Every other operand resolves with NO literal
		// context (its value is range-checked / coerced against target at eval), so changing the
		// context only for a parameter leaves all existing CAST behavior untouched.
		var innerCtx *scalarType
		if e.Cast.Inner.Kind == exprParam {
			t := target
			innerCtx = &t
		} else if target.IsBool() {
			// A boolean target accepts only an i32 source (the boolean cast slice): an untyped
			// integer literal operand adapts to i32 (CAST(5 AS boolean) / 5::boolean), matching PG.
			// A column/expression keeps its own type; a literal beyond i32 range then traps 22003
			// (PG 42846 — a documented divergence).
			t := scalarInt32
			innerCtx = &t
		}
		inner, ity, err := resolve(s, e.Cast.Inner, innerCtx, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		// The boolean cast slice (spec/types/casts.toml, types.md §9): PG ties boolean↔integer to i32
		// ONLY and makes both directions explicit. A boolean TARGET takes an i32 / NULL / bool source
		// (the eval maps 0→false, nonzero→true); a boolean SOURCE produces an i32 (true→1, false→0).
		// Both are handled here, ahead of the generic numeric cast below — resultRt assumes an
		// int/decimal/float target, so a boolean target must not fall through. A bool⇄i16 / bool⇄i64
		// pair is a forbidden 42804 (jed's datatype-mismatch convention; PG reports 42846, casts.toml).
		if target.IsBool() {
			// A runtime `text` source is the runtime-text-cast slice (grammar.md §36): the eval
			// parses the per-row string via the same parseBoolLiteral (PG boolin) the 't'::boolean
			// literal uses. A string LITERAL operand was already coerced above, so a text source
			// here is non-literal (a column / expression).
			if (ity.kind == rtInt && ity.intTy == scalarInt32) || ity.kind == rtNull || ity.kind == rtBool || ity.kind == rtText {
				return &rExpr{kind: reCast, operand: inner, result: target, typmod: typmod},
					resolvedType{kind: rtBool}, nil
			}
			return nil, resolvedType{}, typeError("cannot cast " + rtName(ity) + " to boolean")
		}
		if ity.kind == rtBool {
			// boolean → i32 is the one boolean-source cast; any other target is forbidden (42804).
			if target == scalarInt32 {
				return &rExpr{kind: reCast, operand: inner, result: target, typmod: typmod},
					resolvedType{kind: rtInt, intTy: scalarInt32}, nil
			}
			return nil, resolvedType{}, typeError("cannot cast boolean to " + target.CanonicalName())
		}
		// A runtime `text` source to a numeric target is the runtime-text-cast slice (grammar.md
		// §36): the only targets reaching this generic path are int / decimal / float (text /
		// bytea / uuid / datetime / interval / bool / json targets all return in their own blocks
		// above), so a text source here casts to a number. The eval coerces the per-row string via
		// the same parse functions the literal form uses (22P02 / 22003 per row). A string LITERAL
		// operand was already folded above, so this text is non-literal — fall through to the
		// numeric cast node below.
		// Casting FROM bytea is likewise deferred (0A000).
		if ity.kind == rtBytea {
			return nil, resolvedType{}, newError(FeatureNotSupported, "casting from bytea is not supported yet")
		}
		// Casting FROM uuid is likewise deferred (0A000).
		if ity.kind == rtUuid {
			return nil, resolvedType{}, newError(FeatureNotSupported, "casting from uuid is not supported yet")
		}
		// Casting FROM a timestamp is likewise deferred (0A000).
		if ity.kind == rtTimestamp || ity.kind == rtTimestamptz {
			return nil, resolvedType{}, newError(FeatureNotSupported, "casting from a timestamp type is not supported yet")
		}
		// Casting FROM an interval is likewise deferred (0A000).
		if ity.kind == rtInterval {
			return nil, resolvedType{}, newError(FeatureNotSupported, "casting from an interval type is not supported yet")
		}
		// Casting FROM a date is likewise deferred (0A000; date↔timestamp unblocks the cross-family comparison — date.md §4/§6).
		if ity.kind == rtDate {
			return nil, resolvedType{}, newError(FeatureNotSupported, "casting from a date type is not supported yet")
		}
		// Casting FROM an array (array→text, element-wise array→array) is deferred (array.md §7/§12).
		if ity.kind == rtArray {
			return nil, resolvedType{}, newError(FeatureNotSupported, "casting an array value is not supported yet")
		}
		// Casting FROM json/jsonb (json↔jsonb, json[b]→text, text→json[b]) lands in J3
		// (spec/design/json.md §6); deferred this slice.
		if ity.kind == rtJson || ity.kind == rtJsonb {
			return nil, resolvedType{}, newError(FeatureNotSupported, "casting a json value is not supported yet")
		}
		// int→int (range check), int→decimal (widen), decimal→int (explicit, round),
		// decimal→decimal (re-scale), and NULL are all castable.
		// resolvedTypeOf maps the target to the right kind (incl. rtFloat32/rtFloat64). A float
		// source reaching here is int/decimal/float (others were deferred above); every cross-family
		// float cast is explicit (spec/design/float.md Â§6) â the only implicit float edge,
		// f32->f64, is the tower, never a CAST.
		resultRt := resolvedTypeOf(target)
		return &rExpr{kind: reCast, operand: inner, result: target, typmod: typmod}, resultRt, nil
	case exprUnary:
		if e.Unary.Op == opNeg {
			rop, ty, err := resolve(s, e.Unary.Operand, ctx, ag, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			return negNode(rop, ty)
		}
		// OpNot
		rop, ty, err := resolve(s, e.Unary.Operand, nil, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if err := requireBool(ty, "NOT requires a boolean operand"); err != nil {
			return nil, resolvedType{}, positionError(err, exprLocation(e.Unary.Operand))
		}
		return &rExpr{kind: reNot, operand: rop}, resolvedType{kind: rtBool}, nil
	case exprIsNull:
		rop, _, err := resolve(s, e.IsNullOf.Operand, nil, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		return &rExpr{kind: reIsNull, operand: rop, negated: e.IsNullOf.Negated},
			resolvedType{kind: rtBool}, nil
	case exprIsJson:
		// The operand must be a character string / json / jsonb (else 42804); a bare string literal
		// resolves as text. The predicate is always a definite boolean (NULL operand → NULL at eval).
		rop, ty, err := resolve(s, e.IsJsonOf.Operand, nil, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		switch ty.kind {
		case rtText, rtJson, rtJsonb, rtNull:
			// ok
		default:
			return nil, resolvedType{}, newError(DatatypeMismatch,
				fmt.Sprintf("cannot use type %s in IS JSON predicate", rtName(ty)))
		}
		return &rExpr{
				kind: reIsJson, operand: rop, negated: e.IsJsonOf.Negated,
				jpKind: e.IsJsonOf.Kind, jpUnique: e.IsJsonOf.UniqueKeys,
			},
			resolvedType{kind: rtBool}, nil
	case exprJsonCtor:
		// JSON(text) parses a character string to a `json` value (verbatim). The operand must be text
		// (a bare string literal stays text); a non-text operand → 42804. The result is `json`.
		textHint := scalarText
		rop, ty, err := resolve(s, e.JsonCtorOf.Operand, &textHint, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		switch ty.kind {
		case rtText, rtNull:
			// ok
		default:
			return nil, resolvedType{}, newError(DatatypeMismatch,
				fmt.Sprintf("cannot use type %s as JSON() input", rtName(ty)))
		}
		return &rExpr{
				kind: reJsonCtor, operand: rop, jpUnique: e.JsonCtorOf.UniqueKeys,
			},
			resolvedType{kind: rtJson}, nil
	case exprJsonExists:
		return resolveJSONSqlFn(s, jsExists, e.JsonExists.Ctx, e.JsonExists.Path, e.JsonExists.Passing, nil,
			jWWithout, true, nil, e.JsonExists.OnError, ag, params)
	case exprJsonValue:
		return resolveJSONSqlFn(s, jsValue, e.JsonValue.Ctx, e.JsonValue.Path, e.JsonValue.Passing, e.JsonValue.Returning,
			jWWithout, true, e.JsonValue.OnEmpty, e.JsonValue.OnError, ag, params)
	case exprJsonQuery:
		return resolveJSONSqlFn(s, jsQuery, e.JsonQuery.Ctx, e.JsonQuery.Path, e.JsonQuery.Passing, e.JsonQuery.Returning,
			e.JsonQuery.Wrapper, e.JsonQuery.KeepQuotes, e.JsonQuery.OnEmpty, e.JsonQuery.OnError, ag, params)
	case exprIsDistinct:
		// NULL-safe equality: the SAME operand contract as `=` — resolve the pair (a
		// literal adapts to its sibling; a text literal stays text), then require the
		// operands be comparable (both integer-ish or both text-ish; a mixed pair is
		// 42804). The result is always a definite boolean (functions.md §3).
		rl, lt, rr, rt, err := resolveOperandPair(s, e.IsDistinct.Lhs, e.IsDistinct.Rhs, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if err := classifyComparable(lt, rt); err != nil {
			return nil, resolvedType{}, err
		}
		return &rExpr{kind: reDistinct, lhs: rl, rhs: rr, negated: e.IsDistinct.Negated},
			resolvedType{kind: rtBool}, nil
	case exprIn:
		// An EMPTY list reaches here only from folding an IN-subquery whose result was empty
		// (grammar.md §26; the parser rejects literal `IN ()` → 42601). The value is a constant —
		// `x IN (empty)` = FALSE, `x NOT IN (empty)` = TRUE — for every x including NULL. Still
		// resolve the LHS so an undefined column / aggregate-context error fires, then return the
		// constant (a leaf — no operator_eval, cost.md §3).
		if len(e.In.List) == 0 {
			if _, _, err := resolve(s, e.In.Lhs, nil, ag, params); err != nil {
				return nil, resolvedType{}, err
			}
			return &rExpr{kind: reConstBool, cBool: e.In.Negated}, resolvedType{kind: rtBool}, nil
		}
		// Desugar to the OR-chain PostgreSQL DEFINES `IN` as: `x IN (a,b,c)` is
		// `x = a OR x = b OR x = c`; `NOT IN` is its negation (grammar.md §20). The list is
		// non-empty (the parser rejects `IN ()` → 42601). Resolving the desugared tree reuses
		// the `=`/OR/NOT machinery verbatim, so the three-valued NULL semantics, per-element
		// operand typing (a too-wide literal → 22003, a cross-family element → 42804), and cost
		// all fall out. The LHS is evaluated once per element (the OR-chain model — a documented
		// cost consequence, cost.md §3).
		var folded exprNode
		for i, elem := range e.In.List {
			eq := newBinaryExpr(opEq, e.In.Lhs, elem, e.Pos)
			if i == 0 {
				folded = eq
			} else {
				folded = newBinaryExpr(opOr, folded, eq, e.Pos)
			}
		}
		if e.In.Negated {
			folded = exprNode{Kind: exprUnary, Unary: &unaryExpr{Op: opNot, Operand: folded}, Pos: e.Pos}
		}
		return resolve(s, folded, ctx, ag, params)
	case exprBetween:
		// Desugar to `lhs >= lo AND lhs <= hi` (grammar.md §21). The Kleene AND gives the PG
		// result for a NULL bound: `5 BETWEEN 10 AND NULL` is `FALSE AND NULL` = FALSE (a FALSE
		// operand dominates), while `5 BETWEEN 1 AND NULL` is `TRUE AND NULL` = NULL. NOT BETWEEN
		// negates the whole conjunction. The LHS is evaluated twice (the desugar model — a
		// documented cost consequence, cost.md §3).
		// The desugared comparisons carry the BETWEEN's operator position (PostgreSQL's).
		ge := newBinaryExpr(opGe, e.Between.Lhs, e.Between.Lo, e.Pos)
		le := newBinaryExpr(opLe, e.Between.Lhs, e.Between.Hi, e.Pos)
		folded := newBinaryExpr(opAnd, ge, le, e.Pos)
		if e.Between.Negated {
			folded = exprNode{Kind: exprUnary, Unary: &unaryExpr{Op: opNot, Operand: folded}, Pos: e.Pos}
		}
		return resolve(s, folded, ctx, ag, params)
	case exprLike:
		// LIKE is text×text → boolean (grammar.md §22). Resolve the pair (a string literal stays
		// text), then require BOTH operands be text (or a bare NULL); a non-text operand is
		// 42804. We do NOT use classifyComparable here — it would wrongly accept bytea×bytea.
		rl, lt, rr, rt, err := resolveTextOperandPair(s, e.Like.Lhs, e.Like.Rhs, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if err := requireTextOrNull(lt); err != nil {
			return nil, resolvedType{}, err
		}
		if err := requireTextOrNull(rt); err != nil {
			return nil, resolvedType{}, err
		}
		return &rExpr{kind: reLike, lhs: rl, rhs: rr, negated: e.Like.Negated, insensitive: e.Like.Insensitive},
			resolvedType{kind: rtBool}, nil
	case exprRegex:
		// ~ / ~* / !~ / !~* — text×text → boolean (grammar.md §22b, regex.md). Same operand typing
		// as LIKE: resolve the pair, require both text (or a bare NULL); a non-text operand is 42804.
		rl, lt, rr, rt, err := resolveTextOperandPair(s, e.Regex.Lhs, e.Regex.Rhs, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if err := requireTextOrNull(lt); err != nil {
			return nil, resolvedType{}, err
		}
		if err := requireTextOrNull(rt); err != nil {
			return nil, resolvedType{}, err
		}
		// Precompile a CONSTANT pattern ONCE (regex.md §5); a non-constant pattern compiles per row at
		// eval. For ~* the constant is case-folded before compiling (the ILIKE mechanism). A malformed
		// pattern surfaces 2201B (and an oversized one 54001) here, at resolve, for the constant case.
		var prog *regexProgram
		if rr.kind == reConstText {
			pat := rr.cText
			if e.Regex.Insensitive {
				pat = foldRegexPattern(pat, s.catalog.session.env.propertyTable())
			}
			prog, err = compileRegex(pat)
			if err != nil {
				return nil, resolvedType{}, err
			}
		}
		// A precompiled (constant-pattern) program carries the one-shot rxCompileCharged cost flag
		// mutated on first eval — a reused plan would under-charge the 2nd+ execute, so never cache it.
		if prog != nil {
			params.uncacheable = true
		}
		return &rExpr{kind: reRegex, lhs: rl, rhs: rr, negated: e.Regex.Negated, insensitive: e.Regex.Insensitive, rxProgram: prog},
			resolvedType{kind: rtBool}, nil
	case exprCase:
		// Resolve each branch's condition: searched form requires a boolean WHEN (42804
		// otherwise); simple form desugars to `operand = value` (reusing the `=` operand pairing
		// + comparability check, so the value adapts to the operand's type). The operand is
		// evaluated once per tested branch (the desugar model, like IN).
		arms := make([]rCaseArm, 0, len(e.Case.Whens))
		resultTypes := make([]resolvedType, 0, len(e.Case.Whens)+1)
		for _, w := range e.Case.Whens {
			var rcond *rExpr
			if e.Case.Operand != nil {
				eq := newBinaryExpr(opEq, *e.Case.Operand, w.Cond, 0)
				rc, _, err := resolve(s, eq, nil, ag, params)
				if err != nil {
					return nil, resolvedType{}, err
				}
				rcond = rc
			} else {
				rc, cty, err := resolve(s, w.Cond, nil, ag, params)
				if err != nil {
					return nil, resolvedType{}, err
				}
				if err := requireBool(cty, "CASE WHEN condition must be boolean"); err != nil {
					return nil, resolvedType{}, positionError(err, exprLocation(w.Cond))
				}
				rcond = rc
			}
			rres, rty, err := resolve(s, w.Result, nil, ag, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			resultTypes = append(resultTypes, rty)
			arms = append(arms, rCaseArm{cond: rcond, result: rres})
		}
		var rels *rExpr
		if e.Case.Els != nil {
			r, ety, err := resolve(s, *e.Case.Els, nil, ag, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			rels = r
			resultTypes = append(resultTypes, ety)
		} else {
			rels = &rExpr{kind: reConstNull}
			resultTypes = append(resultTypes, resolvedType{kind: rtNull})
		}
		unified, err := unifyCaseTypes(resultTypes, "CASE result types must be compatible")
		if err != nil {
			// PostgreSQL scans the ELSE result first, then the THEN results in order.
			types := append([]resolvedType{resultTypes[len(resultTypes)-1]}, resultTypes[:len(resultTypes)-1]...)
			exprs := []exprNode{{Kind: exprLiteral, Literal: &literal{Kind: literalNull}}}
			if e.Case.Els != nil {
				exprs[0] = *e.Case.Els
			}
			for _, w := range e.Case.Whens {
				exprs = append(exprs, w.Result)
			}
			return nil, resolvedType{}, positionError(err, mismatchLocation(types, exprs, caseTypesUnify))
		}
		// A mixed-width float CASE is f64: widen the f32 arms (an ordinary cast, like GREATEST).
		if unified.kind == rtFloat64 {
			for i := range arms {
				arms[i].result = widenFloatToF64(arms[i].result, resultTypes[i])
			}
			rels = widenFloatToF64(rels, resultTypes[len(resultTypes)-1])
		}
		// A bare parameter in a result arm takes the unified scalar type. The first pass had no
		// result context, so it recorded the occurrence as unresolved; feed the common type back
		// before statement-wide parameter finalization (api.md §5, grammar.md §23).
		hint := scalarForParamHint(unified)
		for _, w := range e.Case.Whens {
			if w.Result.Kind == exprParam {
				if err := params.note(int(w.Result.Param)-1, hint); err != nil {
					return nil, resolvedType{}, err
				}
			}
		}
		if e.Case.Els != nil && e.Case.Els.Kind == exprParam {
			if err := params.note(int(e.Case.Els.Param)-1, hint); err != nil {
				return nil, resolvedType{}, err
			}
		}
		return &rExpr{kind: reCase, caseArms: arms, caseEls: rels, caseDecimal: unified.kind == rtDecimal},
			unified, nil
	case exprCoalesce:
		// COALESCE(a, b, …) (grammar.md §51): each argument resolves in the same agg context (an
		// aggregate argument is legal wherever an aggregate is), and the argument types unify to
		// one common type exactly like CASE's result arms.
		args := make([]*rExpr, 0, len(e.Coalesce))
		argTypes := make([]resolvedType, 0, len(e.Coalesce))
		for _, a := range e.Coalesce {
			ra, aty, err := resolve(s, a, nil, ag, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			args = append(args, ra)
			argTypes = append(argTypes, aty)
		}
		unified, err := unifyCaseTypes(argTypes, "COALESCE types must be compatible")
		if err != nil {
			return nil, resolvedType{}, positionError(err, mismatchLocation(argTypes, e.Coalesce, caseTypesUnify))
		}
		// A mixed-width float COALESCE is f64: widen the f32 arguments (as CASE).
		if unified.kind == rtFloat64 {
			for i, t := range argTypes {
				args[i] = widenFloatToF64(args[i], t)
			}
		}
		// A bare parameter argument takes COALESCE's unified scalar type. Each argument was
		// initially resolved without sibling context, so record the derived type now.
		hint := scalarForParamHint(unified)
		for _, a := range e.Coalesce {
			if a.Kind == exprParam {
				if err := params.note(int(a.Param)-1, hint); err != nil {
					return nil, resolvedType{}, err
				}
			}
		}
		return &rExpr{kind: reCoalesce, sargs: args, caseDecimal: unified.kind == rtDecimal},
			unified, nil
	case exprGreatestLeast:
		// GREATEST/LEAST(a, b, …) (grammar.md §52): each argument resolves in the same agg context,
		// and the argument types unify by CASE's rule (unifyCaseTypes). The winner is chosen by the
		// unified type's total order at eval, so — unlike CASE/COALESCE, which never compare — the
		// common type must also be orderable: the classifyComparable gate below.
		name := "least"
		if e.Greatest {
			name = "greatest"
		}
		args := make([]*rExpr, 0, len(e.GreatestLeast))
		argTypes := make([]resolvedType, 0, len(e.GreatestLeast))
		for _, a := range e.GreatestLeast {
			ra, aty, err := resolve(s, a, nil, ag, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			args = append(args, ra)
			argTypes = append(argTypes, aty)
		}
		unified, err := unifyCaseTypes(argTypes, strings.ToUpper(name)+" types must be compatible")
		if err != nil {
			return nil, resolvedType{}, positionError(err, mismatchLocation(argTypes, e.GreatestLeast, caseTypesUnify))
		}
		// The winner is chosen by the unified type's total order, so a non-orderable type
		// (json/jsonpath) or an incomparable pair is 42883/42804 HERE — never silently mis-ordered
		// by valueCmp's cross-family totality fallback.
		if err := classifyComparable(unified, unified); err != nil {
			return nil, resolvedType{}, err
		}
		// A bare parameter takes the unified scalar type (like CASE/COALESCE — grammar.md §42).
		hint := scalarForParamHint(unified)
		for _, a := range e.GreatestLeast {
			if a.Kind == exprParam {
				if err := params.note(int(a.Param)-1, hint); err != nil {
					return nil, resolvedType{}, err
				}
			}
		}
		// A mixed-width float set unifies to f64; widen the f32 arguments (an ordinary cast, whose
		// cost stays observable) so the comparator sees one width.
		if unified.kind == rtFloat64 {
			for i, t := range argTypes {
				args[i] = widenFloatToF64(args[i], t)
			}
		}
		// Text / text[] arguments derive one comparison collation (42P21/42P22 on conflict — §52).
		var coll *Collation
		if unified.collatable() {
			d := deriv{}
			for _, a := range e.GreatestLeast {
				ad, e2 := deriveCollation(s, a)
				if e2 != nil {
					return nil, resolvedType{}, e2
				}
				d, e2 = combineDeriv(d, ad)
				if e2 != nil {
					return nil, resolvedType{}, e2
				}
			}
			if coll, err = resolveDeriv(s.catalog, d); err != nil {
				return nil, resolvedType{}, err
			}
		}
		return &rExpr{kind: reGreatestLeast, sargs: args, caseDecimal: unified.kind == rtDecimal, greatest: e.Greatest, collation: coll},
			unified, nil
	case exprQuantified:
		return resolveQuantified(s, e.Quantified, ag, params)
	case exprQuantifiedSubquery:
		return resolveQuantifiedSubquery(s, e.QuantifiedSubquery, ag, params)
	default: // ExprBinary
		return resolveBinary(s, e.Binary, ag, params)
	}
}

// resolveCollationName resolves a collation NAME to its table (spec/design/collation.md §1). C is the
// built-in byte / code-point order → nil (the unchanged fast path); any other name resolves through
// the reference-only read path (the database's resolved set, then the binary's vendored set), else
// 42704.
func resolveCollationName(catalog *engine, name string) (*Collation, error) {
	if name == "C" {
		return nil, nil
	}
	if c := catalog.readSnap().resolveCollation(name); c != nil {
		return c, nil
	}
	return nil, newError(UndefinedObject, fmt.Sprintf("collation %q does not exist", name))
}

// A text expression's collation derivation (spec/design/collation.md §1, PG's rules). kind:
// derivNone (no collation — a non-text expr or a bare literal), derivImplicit (a column's frozen
// collation — C counts as a distinct implicit collation), derivExplicit (an explicit COLLATE),
// derivIndeterminate (two different implicit collations met — 42P22 when consumed; it dominates an
// implicit neighbour), or derivConflicted (a collatable column whose inputs settled to NO collation —
// PostgreSQL's InvalidOid: a UNION ALL / VALUES column over conflicting implicit collations, or a
// derived column over an indeterminate body output — it yields to any neighbour, but consuming it
// alone is 42P22).
const (
	derivNone = iota
	derivImplicit
	derivExplicit
	derivIndeterminate
	derivConflicted
)

type deriv struct {
	name string
	kind int
}

// deriveCollation derives the collation + derivation level of a (text) expression subtree. A COLLATE
// is explicit; a column reference is implicit (its frozen collation, C if none); || combines its
// operands. Every other text-valued node carries its inputs' collation, PostgreSQL's general rule: a
// scalar function whose catalog result is text (functionResultCollatable), a cast to text, the CASE
// results, COALESCE / GREATEST / LEAST, an input-typed aggregate or window function
// (aggResultCollatable), and a scalar subquery (its output column, implicit). A literal and every
// other shape derive none (no collation — takes a neighbour's). A persisted index expression's scope
// (storageCollation) keeps the earlier rule (column, COLLATE, ||).
func deriveCollation(s *scope, e exprNode) (deriv, error) {
	switch e.Kind {
	case exprCollate:
		return deriv{name: e.Collate.Collation, kind: derivExplicit}, nil
	case exprColumn:
		r, err := s.resolveBare(e.Column)
		return columnDeriv(s, r, err), nil
	case exprQualifiedColumn:
		r, err := s.resolveQualified(e.Qualifier, e.Column)
		return columnDeriv(s, r, err), nil
	case exprBinary:
		if e.Binary.Op == opConcat {
			l, err := deriveCollation(s, e.Binary.Lhs)
			if err != nil {
				return deriv{}, err
			}
			r, err := deriveCollation(s, e.Binary.Rhs)
			if err != nil {
				return deriv{}, err
			}
			return combineDeriv(l, r)
		}
		return deriv{}, nil
	}
	if s.storageCollation {
		return deriv{}, nil
	}
	switch e.Kind {
	case exprFuncCall:
		f := e.FuncCall
		switch {
		case !f.Star && f.Filter == nil && f.Over == nil && f.OverName == "" && f.WithinGroup == nil &&
			functionResultCollatable(f.Name):
			return combineDerivPtrs(s, f.Args)
		case !f.Star && f.WithinGroup == nil && aggResultCollatable(f.Name):
			// An input-typed aggregate / window function (min/max, lag/lead/first_value/…): its value
			// arguments. FILTER, OVER and an in-aggregate ORDER BY contribute nothing.
			return combineDerivPtrs(s, f.Args)
		case f.WithinGroup != nil && (f.Name == "mode" || f.Name == "percentile_disc"):
			// An ordered-set aggregate returning its key's type (mode / percentile_disc): the one key.
			if len(f.WithinGroup) == 0 {
				return deriv{}, nil
			}
			kd, err := orderKeyDeriv(s, &f.WithinGroup[0])
			if err != nil {
				return deriv{}, err
			}
			return combineDeriv(deriv{}, kd)
		}
		return deriv{}, nil
	case exprCast:
		// A cast to text / text[] keeps its operand's derivation (a non-collatable operand derives
		// none); a varchar(n) length coercion is a node of its own (PostgreSQL), so it combines like a
		// function and a conflicted operand yields.
		if !castTargetCollatable(e.Cast.TypeName) {
			return deriv{}, nil
		}
		d, err := deriveCollation(s, e.Cast.Inner)
		if err != nil {
			return deriv{}, err
		}
		if e.Cast.TypeMod != nil {
			return combineDeriv(deriv{}, d)
		}
		return d, nil
	case exprCase:
		// CASE combines its results only — never the simple-CASE operand or a WHEN condition.
		d := deriv{}
		for i := range e.Case.Whens {
			rd, err := deriveCollation(s, e.Case.Whens[i].Result)
			if err != nil {
				return deriv{}, err
			}
			if d, err = combineDeriv(d, rd); err != nil {
				return deriv{}, err
			}
		}
		if e.Case.Els != nil {
			ed, err := deriveCollation(s, *e.Case.Els)
			if err != nil {
				return deriv{}, err
			}
			return combineDeriv(d, ed)
		}
		return d, nil
	case exprArray:
		// ARRAY[e1, …] combines its elements, like a function's arguments (collation.md §1 "A text[]
		// carries its elements' collation").
		return combineDerivList(s, e.RowItems)
	case exprSubscript:
		// A subscript or slice carries its array's collation (the subscripts are integers).
		d, err := deriveCollation(s, *e.Base)
		if err != nil {
			return deriv{}, err
		}
		return combineDeriv(deriv{}, d)
	case exprCoalesce:
		return combineDerivList(s, e.Coalesce)
	case exprGreatestLeast:
		return combineDerivList(s, e.GreatestLeast)
	case exprScalarSubquery:
		// A scalar subquery: its output column's collation, recorded when resolve planned it.
		if o, ok := s.subqueryDerivs[e.Subquery]; ok {
			return o.d, nil
		}
		return deriv{}, nil
	default:
		return deriv{}, nil
	}
}

// combineDerivPtrs combines the derivations of a node's inputs, starting from none — so a lone
// conflicted input yields (PostgreSQL gives such a node its default collation).
func combineDerivPtrs(s *scope, args []*exprNode) (deriv, error) {
	d := deriv{}
	for _, a := range args {
		ad, err := deriveCollation(s, *a)
		if err != nil {
			return deriv{}, err
		}
		if d, err = combineDeriv(d, ad); err != nil {
			return deriv{}, err
		}
	}
	return d, nil
}

// combineDerivList is combineDerivPtrs over an inline argument list (COALESCE / GREATEST / LEAST).
func combineDerivList(s *scope, args []exprNode) (deriv, error) {
	d := deriv{}
	for i := range args {
		ad, err := deriveCollation(s, args[i])
		if err != nil {
			return deriv{}, err
		}
		if d, err = combineDeriv(d, ad); err != nil {
			return deriv{}, err
		}
	}
	return d, nil
}

// typmod is a query output column's type modifier (spec/design/create-table-as.md §2): the
// numeric(p,s) precision and scale, or the varchar(n) length, that a `CREATE TABLE … AS` column keeps.
// Both nil is no typmod. Only a decimal or text scalar carries one; an array's element typmod is not
// tracked (an array column cannot declare one — array.md §12). The zero value is no typmod.
type typmod struct {
	Decimal    *decimalTypmod
	VarcharLen *uint32
}

// columnTypmod is a column's declared typmod (a catalog column, or a synthetic one that recorded its
// body's).
func columnTypmod(col *catColumn) typmod {
	return typmod{Decimal: col.Decimal, VarcharLen: col.VarcharLen}
}

// fieldTypmod is the typmod of a composite field (PostgreSQL's FieldSelect keeps it).
func fieldTypmod(f *compositeField) typmod {
	return typmod{Decimal: f.Decimal, VarcharLen: f.VarcharLen}
}

// equal reports whether two typmods are the same (by value — the pointers may differ).
func (t typmod) equal(o typmod) bool {
	if (t.Decimal == nil) != (o.Decimal == nil) || (t.VarcharLen == nil) != (o.VarcharLen == nil) {
		return false
	}
	if t.Decimal != nil && *t.Decimal != *o.Decimal {
		return false
	}
	return t.VarcharLen == nil || *t.VarcharLen == *o.VarcharLen
}

// commonTypmod is the common typmod of several inputs (PostgreSQL's select_common_typmod, and
// exprTypmod for CASE / COALESCE / GREATEST / LEAST): their typmod when every input has the same one,
// else none. Typmods only exist on decimal and text values, so equal typmods mean one type.
func commonTypmod(inputs []typmod) typmod {
	if len(inputs) == 0 {
		return typmod{}
	}
	for _, t := range inputs[1:] {
		if !t.equal(inputs[0]) {
			return typmod{}
		}
	}
	return inputs[0]
}

// deriveTypmod is the typmod an expression keeps (spec/design/create-table-as.md §2, PostgreSQL's
// exprTypmod): a column reference keeps its column's, a cast to numeric(p,s) / varchar(n) has the
// cast's, a COLLATE keeps its operand's, CASE (with an ELSE), COALESCE, GREATEST, and LEAST keep their
// inputs' common typmod, a scalar subquery keeps its output column's, and a composite field selection
// keeps the field's. Every other expression — an operator, a function, an aggregate, a literal, a
// parameter, a cast without a typmod — has none.
func deriveTypmod(s *scope, e exprNode) typmod {
	switch e.Kind {
	case exprColumn:
		if r, err := s.resolveBare(e.Column); err == nil {
			return columnTypmod(s.columnOf(r))
		}
		return typmod{}
	case exprQualifiedColumn:
		if r, err := s.resolveQualified(e.Qualifier, e.Column); err == nil {
			return columnTypmod(s.columnOf(r))
		}
		return typmod{}
	case exprCollate:
		return deriveTypmod(s, e.Collate.Inner)
	case exprCast:
		if e.Cast.TypeMod == nil || strings.HasSuffix(e.Cast.TypeName, "[]") {
			return typmod{}
		}
		if _, d, vl, err := resolveTypeAndTypmod(e.Cast.TypeName, e.Cast.TypeMod); err == nil {
			return typmod{Decimal: d, VarcharLen: vl}
		}
		return typmod{}
	case exprCase:
		// CASE without an ELSE can yield a NULL of no typmod (PostgreSQL returns none).
		if e.Case.Els == nil {
			return typmod{}
		}
		tms := []typmod{deriveTypmod(s, *e.Case.Els)}
		for i := range e.Case.Whens {
			tms = append(tms, deriveTypmod(s, e.Case.Whens[i].Result))
		}
		return commonTypmod(tms)
	case exprCoalesce:
		return commonTypmodList(s, e.Coalesce)
	case exprGreatestLeast:
		return commonTypmodList(s, e.GreatestLeast)
	case exprScalarSubquery:
		// A scalar subquery: its output column's typmod, recorded when resolve planned it.
		if o, ok := s.subqueryDerivs[e.Subquery]; ok {
			return o.tm
		}
		return typmod{}
	case exprFieldAccess:
		if def := compositeOfExpr(s, *e.Base); def != nil {
			for i := range def.Fields {
				if def.Fields[i].Name == e.Field {
					return fieldTypmod(&def.Fields[i])
				}
			}
		}
		return typmod{}
	default:
		return typmod{}
	}
}

// commonTypmodList is commonTypmod over an inline argument list (COALESCE / GREATEST / LEAST).
func commonTypmodList(s *scope, args []exprNode) typmod {
	tms := make([]typmod, len(args))
	for i := range args {
		tms[i] = deriveTypmod(s, args[i])
	}
	return commonTypmod(tms)
}

// compositeOfExpr is the catalog composite type of an expression whose type is statically a named
// composite and whose fields' typmods are known: a composite column, a composite field of one, or a
// cast to a composite. nil for any other expression (its fields' typmods are then not tracked).
func compositeOfExpr(s *scope, e exprNode) *compositeType {
	named := func(ty dataType) *compositeType {
		if ty.Comp == nil || s.catalog == nil {
			return nil
		}
		return s.catalog.readSnap().compositeType(ty.Comp.Name)
	}
	switch e.Kind {
	case exprColumn:
		r, err := s.resolveBare(e.Column)
		if err != nil {
			return nil
		}
		return named(s.columnOf(r).Type)
	case exprQualifiedColumn:
		r, err := s.resolveQualified(e.Qualifier, e.Column)
		if err != nil {
			return nil
		}
		return named(s.columnOf(r).Type)
	case exprFieldAccess:
		def := compositeOfExpr(s, *e.Base)
		if def == nil {
			return nil
		}
		for i := range def.Fields {
			if def.Fields[i].Name == e.Field {
				return named(def.Fields[i].Type)
			}
		}
		return nil
	case exprCast:
		if e.Cast.TypeMod != nil || s.catalog == nil {
			return nil
		}
		return s.catalog.readSnap().compositeType(e.Cast.TypeName)
	default:
		return nil
	}
}

// orderKeyDeriv is the derivation of an ORDER BY / WITHIN GROUP key: a trailing COLLATE is explicit,
// else the key expression's (or bare / qualified column's) own derivation.
func orderKeyDeriv(s *scope, key *orderKey) (deriv, error) {
	if key.Collation != "" {
		return deriv{name: key.Collation, kind: derivExplicit}, nil
	}
	if key.Expr != nil {
		return deriveCollation(s, *key.Expr)
	}
	if key.Qualifier != "" {
		r, err := s.resolveQualified(key.Qualifier, key.Column)
		return columnDeriv(s, r, err), nil
	}
	r, err := s.resolveBare(key.Column)
	return columnDeriv(s, r, err), nil
}

// aggResultCollatable reports whether an aggregate / window function's result is its input's type,
// so it carries its value arguments' collation (spec/design/collation.md §1 "Aggregates and window
// functions"): read from the shared catalog — an aggregate or window-only function whose result is
// same_as_input. A non-text argument derives nothing, so sum/avg over numbers are unaffected.
func aggResultCollatable(name string) bool {
	for i := range aggregates {
		if aggregates[i].Result == "same_as_input" && surfaceMatches(aggregates[i].Surface, name) {
			return true
		}
	}
	for i := range windows {
		if windows[i].Result == "same_as_input" && surfaceMatches(windows[i].Surface, name) {
			return true
		}
	}
	return false
}

// noteSubqueryDeriv records a scalar subquery's output-column derivation on the scope it was resolved
// against (scope.subqueryDerivs), as the subquery expression derives it (collation.md §1 "Scalar
// subqueries"; subqueryOutputDeriv). The output column's typmod rides along unchanged
// (create-table-as.md §2).
func noteSubqueryDeriv(s *scope, q *queryExpr, body deriv, tm typmod) {
	if s.subqueryDerivs == nil {
		s.subqueryDerivs = make(map[*queryExpr]subqueryOutput)
	}
	s.subqueryDerivs[q] = subqueryOutput{d: subqueryOutputDeriv(body), tm: tm}
}

// subqueryOutputDeriv is a subquery output column's derivation as seen from outside it (collation.md
// §1): implicit for a body collation, none for none, indeterminate for a conflicted or indeterminate
// body output.
func subqueryOutputDeriv(body deriv) deriv {
	switch body.kind {
	case derivImplicit, derivExplicit:
		return deriv{name: body.name, kind: derivImplicit}
	case derivIndeterminate, derivConflicted:
		return deriv{kind: derivIndeterminate}
	default:
		return deriv{}
	}
}

// membershipCollation is the comparison collation of `lhs IN / op ANY|ALL (SELECT …)`
// (spec/design/collation.md §1): the left operand's derivation combined with the body column's, which
// is implicit outside the subquery. A conflict is 42P22. nil for a non-collatable or row-valued comparison.
func membershipCollation(s *scope, lhs exprNode, lt, elem resolvedType, rowValued bool, plan *queryPlan) (*Collation, error) {
	if rowValued || !lt.collatable() || !elem.collatable() {
		return nil, nil
	}
	ld, err := deriveCollation(s, lhs)
	if err != nil {
		return nil, err
	}
	d, err := combineDeriv(ld, subqueryOutputDeriv(plan.columnDerivs()[0]))
	if err != nil {
		return nil, err
	}
	return resolveDeriv(s.catalog, d)
}

// sortTargetCollation is the collation a sort / grouping target sorts under (collation.md §1 "A sort
// or grouping target must not be indeterminate"): a key expression combining two different implicit
// collations is 42P21 here, at plan time (PostgreSQL's parse-time check), while a key that only reads
// a conflicted column stays 42P22 at resolveDeriv.
func sortTargetCollation(catalog *engine, d deriv) (*Collation, error) {
	if err := rejectIndeterminateTarget(d); err != nil {
		return nil, err
	}
	return resolveDeriv(catalog, d)
}

// rejectIndeterminateTarget is 42P21 for an indeterminate sort / grouping target (see
// sortTargetCollation).
func rejectIndeterminateTarget(d deriv) error {
	if d.kind == derivIndeterminate {
		return newError(CollationMismatch, "collation mismatch between implicit collations")
	}
	return nil
}

// functionResultCollatable reports whether a scalar function's result carries its arguments'
// collation (spec/design/collation.md §1 "Function results"): read from the shared function catalog
// — any kind = "function" overload whose result is text, text[], or anyarray. A non-collatable
// argument derives no collation, so an overload taking only non-text arguments (the range
// lower/upper, array_append over i32[]) contributes nothing.
func functionResultCollatable(name string) bool {
	for i := range operators {
		o := &operators[i]
		if o.Kind == "function" && (o.Result == "text" || o.Result == "text[]" || o.Result == "anyarray" ||
			o.Result == "anycompatiblearray") &&
			o.Name == name {
			return true
		}
	}
	return false
}

// castTargetCollatable reports whether a cast's target type name is collatable — text (any of its
// spellings, varchar(n) included) or an array of text.
func castTargetCollatable(name string) bool {
	t, ok := scalarTypeFromName(strings.TrimSuffix(name, "[]"))
	return ok && t.IsText()
}

// commonCollation is the common collation of one set-operation / VALUES output column
// (spec/design/collation.md §1 "Set operations", PostgreSQL's select_common_collation): the inputs
// combine like an operator's operands (two different explicit collations are 42P21). Two different
// implicit collations — or an indeterminate input — conflict: noneOK (UNION ALL, VALUES) settles the
// column to derivConflicted, otherwise it is 42P21 now. An explicit result becomes the column's
// IMPLICIT collation (a reference to the output column is a plain column reference).
func commonCollation(inputs []deriv, noneOK bool) (deriv, error) {
	d := deriv{}
	for _, x := range inputs {
		var err error
		d, err = combineDeriv(d, x)
		if err != nil {
			return deriv{}, err
		}
	}
	switch d.kind {
	case derivExplicit:
		return deriv{name: d.name, kind: derivImplicit}, nil
	case derivIndeterminate:
		if noneOK {
			return deriv{kind: derivConflicted}, nil
		}
		var names []string
		for _, x := range inputs {
			if x.kind == derivImplicit {
				names = append(names, x.name)
			}
		}
		msg := "collation mismatch between implicit collations"
		if len(names) >= 2 && names[0] != names[1] {
			msg = fmt.Sprintf("collation mismatch between implicit collations \"%s\" and \"%s\"", names[0], names[1])
		}
		return deriv{}, newError(CollationMismatch, msg)
	default:
		return d, nil
	}
}

// columnDeriv is the implicit derivation of a resolved column reference: a text column carries its
// frozen collation (C → "C", a distinct implicit collation); a non-text or unresolvable reference
// is derivNone.
func columnDeriv(s *scope, r resolved, err error) deriv {
	if err != nil {
		return deriv{}
	}
	return catColumnDeriv(s.columnOf(r))
}

// catColumnDeriv is a column's derivation: implicit for a text / text[] column (its frozen collation, C if
// none), unless it is a synthetic relation's column whose body output derived no collation
// (catColumn.NoCollation) or settled to none over a conflict (catColumn.CollationConflict —
// spec/design/collation.md §1 "Derived relations").
func catColumnDeriv(col *catColumn) deriv {
	if !col.Type.IsCollatable() {
		return deriv{}
	}
	if col.NoCollation {
		return deriv{}
	}
	if col.CollationConflict {
		return deriv{kind: derivConflicted}
	}
	name := col.Collation
	if name == "" {
		name = "C"
	}
	return deriv{name: name, kind: derivImplicit}
}

// columnCollation is the concrete collation a sort over a bare column reference uses absent an
// explicit COLLATE: the column's derivation resolved (nil = byte order).
func columnCollation(s *scope, r resolved) (*Collation, error) {
	return resolveDeriv(s.catalog, catColumnDeriv(s.columnOf(r)))
}

// combineDeriv combines two operands' derivations (spec/design/collation.md §1/§7, PG's rules).
// Explicit dominates; two DIFFERENT explicit collations conflict eagerly (42P21); two different
// implicit collations yield derivIndeterminate (deferred to 42P22 on use); explicit resolves it.
func combineDeriv(a, b deriv) (deriv, error) {
	// A conflicted (no-collation) input yields to anything — PostgreSQL's InvalidOid strength.
	if a.kind == derivConflicted {
		return b, nil
	}
	if b.kind == derivConflicted {
		return a, nil
	}
	if a.kind == derivExplicit && b.kind == derivExplicit {
		if a.name != b.name {
			return deriv{}, newError(CollationMismatch,
				fmt.Sprintf("collation mismatch between explicit collations %q and %q", a.name, b.name))
		}
		return a, nil
	}
	if a.kind == derivExplicit {
		return a, nil
	}
	if b.kind == derivExplicit {
		return b, nil
	}
	if a.kind == derivIndeterminate || b.kind == derivIndeterminate {
		return deriv{kind: derivIndeterminate}, nil
	}
	if a.kind == derivImplicit && b.kind == derivImplicit {
		if a.name == b.name {
			return a, nil
		}
		return deriv{kind: derivIndeterminate}, nil
	}
	if a.kind == derivImplicit {
		return a, nil
	}
	return b, nil
}

// resolveDeriv resolves a derivation to the concrete collation a comparison / ORDER BY uses. none
// and C → nil (byte order, the fast path); a loaded name → its table (42704 if it vanished);
// derivIndeterminate / derivConflicted → 42P22 (the collation is required but ambiguous).
func resolveDeriv(catalog *engine, d deriv) (*Collation, error) {
	switch d.kind {
	case derivIndeterminate, derivConflicted:
		return nil, newError(IndeterminateCollation,
			"could not determine which collation to use for string comparison")
	case derivImplicit, derivExplicit:
		return resolveCollationName(catalog, d.name)
	default:
		return nil, nil
	}
}

// collatedCmp compares two non-NULL text values under a loaded collation (spec/design/collation.md
// §6/§7): order by the UCA sort keys, whose memcmp order IS the collation order. The caller charges
// the collate cost and handles NULLs.
func collatedCmp(coll *Collation, a, b string) (int, error) {
	ka, err := sortKey(coll, a)
	if err != nil {
		return 0, err
	}
	kb, err := sortKey(coll, b)
	if err != nil {
		return 0, err
	}
	return bytes.Compare(ka, kb), nil
}

// collatedArrayCmp is arrayTotalCmp over two text[] values with their text elements compared under
// coll (spec/design/collation.md §1, array.md §5). Each walked pair of present elements charges
// collate per code point of both (collation.md §11) when m is non-nil — the comparison operators and
// GREATEST/LEAST; the unmetered sort/min/max paths pass nil. A NULL element or the array shape
// decides without a charge.
func collatedArrayCmp(coll *Collation, a, b *ArrayVal, m *costMeter) (int, error) {
	n := len(a.Elements)
	if len(b.Elements) < n {
		n = len(b.Elements)
	}
	for i := 0; i < n; i++ {
		x, y := a.Elements[i], b.Elements[i]
		xn, yn := x.Kind == ValNull, y.Kind == ValNull
		switch {
		case xn && yn:
			continue
		case xn: // NULL sorts last
			return 1, nil
		case yn:
			return -1, nil
		}
		if m != nil {
			m.Charge(costs.Collate * int64(utf8.RuneCountInString(x.str())+utf8.RuneCountInString(y.str())))
			if err := m.Guard(); err != nil {
				return 0, err
			}
		}
		c, err := collatedCmp(coll, x.str(), y.str())
		if err != nil {
			return 0, err
		}
		if c != 0 {
			return c, nil
		}
	}
	return arrayShapeCmp(a, b), nil
}

// collationSortKey is a collated sort's decoration for one value (spec/design/collation.md §8): a
// text value's UCA sort key, a text[] value's collated array key (encoding.md §2.14 — memcmp-ordered
// exactly like collatedArrayCmp), nil for NULL. An unmapped code point is 0A000.
func collationSortKey(coll *Collation, v Value) ([]byte, error) {
	switch v.Kind {
	case ValText:
		return sortKey(coll, v.str())
	case ValArray:
		return encodeArrayKey(scalarText, v.arrayVal(), coll)
	default:
		return nil, nil
	}
}

// collatedOrValueCmp compares two non-NULL values under coll when it is set and they are text or
// text[] (collatedValueCmp, unmetered), else by the value order — the min/max fold and the
// hypothetical-set key comparison.
func collatedOrValueCmp(coll *Collation, a, b Value) (int, error) {
	if coll != nil && ((a.Kind == ValText && b.Kind == ValText) || (a.Kind == ValArray && b.Kind == ValArray)) {
		return collatedValueCmp(coll, a, b, nil)
	}
	return valueCmp(a, b), nil
}

// collatedValueCmp compares two non-NULL values of one collatable type under coll: text by its sort
// key, text[] element-wise (collatedArrayCmp, metered when m is non-nil). The caller handles NULL and
// charges a text pair's collate itself.
func collatedValueCmp(coll *Collation, a, b Value, m *costMeter) (int, error) {
	if a.Kind == ValArray {
		return collatedArrayCmp(coll, a.arrayVal(), b.arrayVal(), m)
	}
	return collatedCmp(coll, a.str(), b.str())
}

func resolveBinary(s *scope, b *binaryExpr, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, error) {
	switch b.Op {
	case opAdd, opSub, opMul, opDiv, opMod:
		// jsonb `-` is the delete operator (json-sql-functions.md §1, J6), NOT arithmetic — its right
		// operand is a key/index/keys, never an arithmetic value. Peek the LHS type; a jsonb LHS with
		// `-` routes to the delete resolver. (Only `-` has a jsonb meaning; `+ * / %` over a jsonb
		// operand fall through and 42804 in the numeric path.)
		//
		// A non-adaptable LHS resolves with no hint either way, so the peek's resolution is the
		// operand pair's: resolving it a second time would double the work at every level of an
		// `a - b - c …` chain (cost.md §7). An adaptable LHS (a literal / $N) is a leaf.
		var peekL *rExpr
		var peekT resolvedType
		if b.Op == opSub {
			rl, lt, err := resolve(s, b.Lhs, nil, ag, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			if lt.kind == rtJsonb {
				return resolveJSONbDelete(s, false, b.Rhs, rl, ag, params)
			}
			if !isAdaptableOperand(b.Lhs) {
				peekL, peekT = rl, lt
			}
		}
		// Arithmetic is overloaded across integer and decimal. Resolve the operand pair (an
		// integer literal adapts to an integer sibling), then pick the family: both integer →
		// integer arithmetic; at least one decimal → decimal arithmetic (the integer operand
		// widens at eval); a text/boolean operand is a 42804 (spec/design/decimal.md §4).
		var rl, rr *rExpr
		var lt, rt resolvedType
		var err error
		if peekL != nil {
			rl, lt, rr, rt, err = resolveOperandPairWithLhs(s, peekL, peekT, b.Rhs, ag, params)
		} else {
			rl, lt, rr, rt, err = resolveOperandPair(s, b.Lhs, b.Rhs, ag, params)
		}
		if err != nil {
			return nil, resolvedType{}, err
		}
		// Range set operators (RF4, spec/design/range-functions.md §4): `+` union, `-` difference, `*`
		// intersection over two ranges. A range operand in any of these three is the set-op axis — both
		// operands must be ranges of a common element type, else 42883 (matching PG's "operator does not
		// exist"); the numeric/temporal arithmetic below never sees a range. `/` and `%` have no range
		// meaning and fall straight through.
		if (b.Op == opAdd || b.Op == opSub || b.Op == opMul) && (lt.kind == rtRange || rt.kind == rtRange) {
			return resolveRangeSetOp(b.Op, rl, lt, rr, rt)
		}
		// Date arithmetic (spec/design/date.md §6): date ± int → date, date − date → i32 (days
		// between), date ± interval → timestamp. Checked BEFORE the interval/timestamp rules below:
		// a `date ± interval` pair has an interval operand, which would otherwise make
		// temporalArithResult report a 42804 (date is not one of its temporal kinds). Any other
		// arithmetic combination involving a date is a 42804 from dateArithResult.
		if lt.kind == rtDate || rt.kind == rtDate {
			st, derr := dateArithResult(b.Op, lt.kind, rt.kind)
			if derr != nil {
				return nil, resolvedType{}, derr
			}
			return &rExpr{kind: reArith, op: b.Op, lhs: rl, rhs: rr, result: st}, resolvedTypeOf(st), nil
		}
		// interval ×÷ number → interval (the exact cascade; spec/design/interval.md §5). Checked
		// before the ±-only temporal rule below.
		if st, isScale := intervalScaleResult(b.Op, lt.kind, rt.kind); isScale {
			return &rExpr{kind: reArith, op: b.Op, lhs: rl, rhs: rr, result: st}, resolvedTypeOf(st), nil
		}
		// Temporal arithmetic (spec/design/interval.md §5): interval ± interval, timestamp[tz] ±
		// interval, interval + timestamp[tz], and timestamp[tz] − timestamp[tz] → interval. The
		// eval dispatches on the value kinds; here we settle the result type. A temporal operand
		// in any other combination is a 42804.
		if st, isTemporal, terr := temporalArithResult(b.Op, lt.kind, rt.kind); isTemporal {
			if terr != nil {
				return nil, resolvedType{}, terr
			}
			return &rExpr{kind: reArith, op: b.Op, lhs: rl, rhs: rr, result: st}, resolvedTypeOf(st), nil
		}
		// Float arithmetic (spec/design/float.md §5): float ⊕ float → float, mixed widths promote
		// to f64. Float is a STRICT island — float ⊕ int/decimal is a 42804 (no cross-family
		// promotion, UNLIKE PG; only literals adapt to a float context — §6). A bare NULL operand
		// adopts the other side's float type so `f + NULL` types as that float and evaluates NULL.
		lFloat, rFloat := isFloatKind(lt.kind), isFloatKind(rt.kind)
		if lFloat || rFloat {
			l, r := lt.kind, rt.kind
			if l == rtNull {
				l = rt.kind
			}
			if r == rtNull {
				r = lt.kind
			}
			if !isFloatKind(l) || !isFloatKind(r) {
				return nil, resolvedType{}, typeError("arithmetic operators require operands of the same family")
			}
			st := promoteFloat(l, r)
			return &rExpr{kind: reArith, op: b.Op, lhs: rl, rhs: rr, result: st}, resolvedTypeOf(st), nil
		}
		if err := requireNumericOperand(lt); err != nil {
			return nil, resolvedType{}, err
		}
		if err := requireNumericOperand(rt); err != nil {
			return nil, resolvedType{}, err
		}
		if lt.kind == rtDecimal || rt.kind == rtDecimal {
			return &rExpr{kind: reArith, op: b.Op, lhs: rl, rhs: rr, result: scalarDecimal},
				resolvedType{kind: rtDecimal}, nil
		}
		result := promote(lt, rt)
		return &rExpr{kind: reArith, op: b.Op, lhs: rl, rhs: rr, result: result},
			resolvedType{kind: rtInt, intTy: result}, nil
	case opEq, opNe, opLt, opGt, opLe, opGe:
		// Comparison is overloaded across families: integer×integer or text×text. Resolve
		// the operands (a literal adapts to its sibling; text literals stay text), then
		// require they be comparable — a mixed integer/text pair is 42804. The runtime
		// comparison (Eq3/Lt3/Gt3) dispatches on the value kinds.
		//
		// `ROW(a, b) op (SELECT x, y …)` — a row constructor compared with a subquery is a
		// ROW-VALUED subquery (grammar.md §26): the body must have exactly as many columns as the
		// row, and its single row compares as an anonymous composite. Only this direction — a
		// subquery on the left stays a scalar subquery (42601 for >1 column), like PostgreSQL.
		var rl, rr *rExpr
		var lt, rt resolvedType
		var err error
		if b.Lhs.Kind == exprRow && b.Rhs.Kind == exprScalarSubquery {
			rl, lt, err = resolve(s, b.Lhs, nil, ag, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
			plan, ty, perr := planRowSubquery(s, *b.Rhs.Subquery, len(b.Lhs.RowItems), params)
			if perr != nil {
				return nil, resolvedType{}, perr
			}
			rr = &rExpr{kind: reSubquery, subPlan: &plan, subKind: sqScalar, rowValued: true}
			rt = ty
		} else {
			rl, lt, rr, rt, err = resolveOperandPair(s, b.Lhs, b.Rhs, ag, params)
			if err != nil {
				return nil, resolvedType{}, err
			}
		}
		if err := classifyComparable(lt, rt); err != nil {
			return nil, resolvedType{}, err
		}
		// Derive the comparison's collation (spec/design/collation.md §1/§7). Only a text×text or
		// text[]×text[] comparison is collatable; a COLLATE on a non-collatable operand was already
		// rejected 42804 at the Collate node. Each operand's derivation (explicit COLLATE / implicit column collation / none)
		// is combined per PG's rules: two different EXPLICIT collations conflict (42P21); two different
		// IMPLICIT collations are indeterminate (42P22 when consumed here). Derived for ALL comparison
		// ops incl =/<> (PG raises the conflict regardless), even though =/<> ignore the collation at
		// eval (byte equality, §7).
		var coll *Collation
		if lt.collatable() && rt.collatable() {
			ld, err := deriveCollation(s, b.Lhs)
			if err != nil {
				return nil, resolvedType{}, err
			}
			rd, err := deriveCollation(s, b.Rhs)
			if err != nil {
				return nil, resolvedType{}, err
			}
			d, err := combineDeriv(ld, rd)
			if err != nil {
				return nil, resolvedType{}, err
			}
			if coll, err = resolveDeriv(s.catalog, d); err != nil {
				return nil, resolvedType{}, err
			}
		}
		return &rExpr{kind: reCompare, op: b.Op, lhs: rl, rhs: rr, collation: coll},
			resolvedType{kind: rtBool}, nil
	case opConcat:
		return resolveConcat(s, b.Lhs, b.Rhs, ag, params)
	// The containment/overlap operators (@>/<@/&&, shared by arrays and ranges) and the five
	// range-only positional/adjacency operators (<</>>/&</&>/-|-) all dispatch here: the operand type
	// chooses the array axis (array-functions.md §10) or the range axis (range-functions.md §3).
	case opContains, opContainedBy, opOverlaps,
		opStrictlyLeft, opStrictlyRight, opNotExtendRight, opNotExtendLeft, opAdjacent:
		return resolveSetOp(s, b.Op, b.Lhs, b.Rhs, ag, params)
	// The jsonb accessor operators (spec/design/json-sql-functions.md §1, J4).
	case opJsonGet, opJsonGetText, opJsonGetPath, opJsonGetPathText:
		return resolveJSONAccess(s, b.Op, b.Lhs, b.Rhs, ag, params)
	// The jsonb key-existence operators (spec/design/json-sql-functions.md §1, J5).
	case opJsonHasKey:
		return resolveJSONHasKey(s, hkOne, b.Lhs, b.Rhs, ag, params)
	case opJsonHasAnyKey:
		return resolveJSONHasKey(s, hkAny, b.Lhs, b.Rhs, ag, params)
	case opJsonHasAllKeys:
		return resolveJSONHasKey(s, hkAll, b.Lhs, b.Rhs, ag, params)
	// `jsonb @? jsonpath` = jsonb_path_exists, `jsonb @@ jsonpath` = jsonb_path_match
	// (jsonpath.md §6). Both reuse the jsonpath kernels.
	case opJsonPathExists, opJsonPathMatch:
		sym, fnKind := "@?", jpfExistsSilent
		if b.Op == opJsonPathMatch {
			sym, fnKind = "@@", jpfMatchSilent
		}
		jsonbHint := scalarJsonb
		ctx, ct, err := resolve(s, b.Lhs, &jsonbHint, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if ct.kind != rtJsonb && ct.kind != rtNull {
			return nil, resolvedType{}, newError(UndefinedFunction,
				fmt.Sprintf("operator does not exist: %s %s jsonpath", rtName(ct), sym))
		}
		pathHint := scalarJsonPath
		path, pt, err := resolve(s, b.Rhs, &pathHint, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if pt.kind != rtJsonPath && pt.kind != rtNull {
			return nil, resolvedType{}, newError(UndefinedFunction,
				fmt.Sprintf("operator does not exist: jsonb %s (a non-jsonpath)", sym))
		}
		return &rExpr{kind: reJsonPathFn, jpFnKind: fnKind, sargs: []*rExpr{ctx, path}},
			resolvedType{kind: rtBool}, nil
	// The jsonb delete-at-path operator `#-` (spec/design/json-sql-functions.md §1, J6). `||` and
	// `-` (delete) are dispatched by operand type in resolveConcat / the arithmetic arm.
	case opJsonDeletePath:
		jsonbHint := scalarJsonb
		rbase, baseTy, err := resolve(s, b.Lhs, &jsonbHint, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		switch baseTy.kind {
		case rtJsonb, rtNull:
		default:
			return nil, resolvedType{}, newError(UndefinedFunction,
				fmt.Sprintf("operator does not exist: %s #- text[]", rtName(baseTy)))
		}
		return resolveJSONbDelete(s, true, b.Rhs, rbase, ag, params)
	default: // OpAnd, OpOr
		rl, lt, err := resolve(s, b.Lhs, nil, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		rr, rt, err := resolve(s, b.Rhs, nil, ag, params)
		if err != nil {
			return nil, resolvedType{}, err
		}
		// A non-boolean operand is located at that operand (PostgreSQL's coerce_to_boolean).
		if err := requireBool(lt, "AND/OR requires boolean operands"); err != nil {
			return nil, resolvedType{}, positionError(err, exprLocation(b.Lhs))
		}
		if err := requireBool(rt, "AND/OR requires boolean operands"); err != nil {
			return nil, resolvedType{}, positionError(err, exprLocation(b.Rhs))
		}
		kind := reAnd
		if b.Op == opOr {
			kind = reOr
		}
		return &rExpr{kind: kind, lhs: rl, rhs: rr}, resolvedType{kind: rtBool}, nil
	}
}

// resolveOperandPair resolves the two operands of a binary operator, giving a bare
// *integer* literal the other operand's integer type as context (so `small + 1` types `1`
// as i16, and `small + 100000` traps 22003 at resolve). A text literal needs no context
// (it is always text); when the sibling is text, an integer literal gets no integer
// context (ctxOf returns nil) and defaults to i64 — the caller's family check then
// reports the mismatch. This does NOT enforce a family — resolveIntPair (arithmetic) and
// classifyComparable (comparison) layer that on top.
// resolveTextOperandPair resolves the operand pair of a text-only operator (LIKE/ILIKE, the regex
// matches): as resolveOperandPair, except a bind parameter operand adopts text, the only operand
// type the operator declares, whatever its sibling ('x%' LIKE $1, $1 LIKE $2 — api.md §5).
func resolveTextOperandPair(s *scope, lhs, rhs exprNode, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, *rExpr, resolvedType, error) {
	lhsParam, rhsParam := lhs.Kind == exprParam, rhs.Kind == exprParam
	if !lhsParam && !rhsParam {
		return resolveOperandPair(s, lhs, rhs, ag, params)
	}
	text := scalarText
	hint := func(isParam bool) *scalarType {
		if isParam {
			return &text
		}
		return nil
	}
	rl, lt, err := resolve(s, lhs, hint(lhsParam), ag, params)
	if err != nil {
		return nil, resolvedType{}, nil, resolvedType{}, err
	}
	rr, rt, err := resolve(s, rhs, hint(rhsParam), ag, params)
	if err != nil {
		return nil, resolvedType{}, nil, resolvedType{}, err
	}
	return rl, lt, rr, rt, nil
}

func resolveOperandPair(s *scope, lhs, rhs exprNode, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, *rExpr, resolvedType, error) {
	lhsLit := isAdaptableOperand(lhs)
	rhsLit := isAdaptableOperand(rhs)
	var rl, rr *rExpr
	var lt, rt resolvedType
	var err error
	switch {
	case lhsLit && rhsLit:
		i64 := scalarInt64
		if rl, lt, err = resolve(s, lhs, &i64, ag, params); err != nil {
			return nil, resolvedType{}, nil, resolvedType{}, err
		}
		rr, rt, err = resolve(s, rhs, &i64, ag, params)
	case lhsLit:
		if rr, rt, err = resolve(s, rhs, nil, ag, params); err != nil {
			return nil, resolvedType{}, nil, resolvedType{}, err
		}
		// A bare parameter facing a container takes the container's type (api.md §5).
		var adopted bool
		if rl, lt, adopted, err = adoptContainerParam(s, lhs, rt, params); !adopted && err == nil {
			rl, lt, err = resolve(s, lhs, ctxOf(rt), ag, params)
		}
	default:
		if rl, lt, err = resolve(s, lhs, nil, ag, params); err != nil {
			return nil, resolvedType{}, nil, resolvedType{}, err
		}
		return resolveOperandPairWithLhs(s, rl, lt, rhs, ag, params)
	}
	if err != nil {
		return nil, resolvedType{}, nil, resolvedType{}, err
	}
	return rl, lt, rr, rt, nil
}

// resolveOperandPairWithLhs is resolveOperandPair for a non-adaptable LHS already resolved (with no
// hint) to (rl, lt).
func resolveOperandPairWithLhs(s *scope, rl *rExpr, lt resolvedType, rhs exprNode, ag *aggCtx, params *paramTypes) (*rExpr, resolvedType, *rExpr, resolvedType, error) {
	var rr *rExpr
	var rt resolvedType
	var err error
	if isAdaptableOperand(rhs) {
		var adopted bool
		if rr, rt, adopted, err = adoptContainerParam(s, rhs, lt, params); !adopted && err == nil {
			rr, rt, err = resolve(s, rhs, ctxOf(lt), ag, params)
		}
	} else {
		rr, rt, err = resolve(s, rhs, nil, ag, params)
	}
	if err != nil {
		return nil, resolvedType{}, nil, resolvedType{}, err
	}
	return rl, lt, rr, rt, nil
}

// resolveIntOrDecimalPair resolves a two-numeric scalar function (gcd/lcm) by reusing the arithmetic
// operand-pair resolution (literal adaptation), then settling the result type. Both operands must be
// integer or decimal (a float/other operand → 42883); the result is the promoted integer type when
// both are integer, else decimal (an integer operand promotes, as PG does).
func resolveIntOrDecimalPair(s *scope, name string, lhs, rhs exprNode, ag *aggCtx, params *paramTypes) (*rExpr, *rExpr, scalarType, error) {
	rl, lt, rr, rt, err := resolveOperandPair(s, lhs, rhs, ag, params)
	if err != nil {
		return nil, nil, 0, err
	}
	ok := func(k rtKind) bool { return k == rtInt || k == rtDecimal || k == rtNull }
	if !ok(lt.kind) || !ok(rt.kind) {
		return nil, nil, 0, noFuncOverload(name)
	}
	if lt.kind == rtDecimal || rt.kind == rtDecimal {
		return rl, rr, scalarResultType("decimal", nil), nil
	}
	return rl, rr, promote(lt, rt), nil
}

// valueToDecimal returns a non-NULL integer/decimal value as a Decimal (the integer→decimal
// promotion gcd/lcm/div use).
func valueToDecimal(v Value) Decimal {
	if v.Kind == ValInt {
		return decimalFromInt64(v.Int)
	}
	return *v.decimal()
}

// gcdI64 is the gcd of two int64 by the Euclidean algorithm, NON-NEGATIVE. ok is false iff the
// magnitude is math.MinInt64 (its abs overflows int64) — the caller maps that to 22003, like PG.
// The b == -1 guard avoids the math.MinInt64 % -1 overflow (the remainder is always 0).
func gcdI64(a, b int64) (int64, bool) {
	for b != 0 {
		t := int64(0)
		if b != -1 {
			t = a % b
		}
		a, b = b, t
	}
	if a == math.MinInt64 {
		return 0, false
	}
	if a < 0 {
		a = -a
	}
	return a, true
}

// gcdDecimal is the gcd of two decimals by the Euclidean algorithm over Rem, NON-NEGATIVE at scale
// max(sₐ, s_b) (PG numeric gcd). The values share a fixed scale through the chain, so it reduces to
// an integer gcd and terminates; the final pad to the target scale is exact.
func gcdDecimal(a, b Decimal) (Decimal, error) {
	target := a.Scale
	if b.Scale > target {
		target = b.Scale
	}
	x, y := a, b
	for !y.IsZero() {
		r, err := x.Rem(y)
		if err != nil {
			return Decimal{}, err
		}
		x, y = y, r
	}
	return x.Abs().RoundToScale(target), nil
}

// lcmI64 is the lcm of two int64, NON-NEGATIVE: |a/gcd·b| with checked arithmetic. ok is false on
// int64 overflow (the product, or the final abs) — the caller maps that (or an out-of-result-type
// magnitude) to 22003, like PG. lcm(_, 0) = 0.
func lcmI64(a, b int64) (int64, bool) {
	if a == 0 || b == 0 {
		return 0, true
	}
	g, ok := gcdI64(a, b)
	if !ok {
		return 0, false
	}
	q := a / g
	prod := q * b
	if b != 0 && prod/b != q { // overflow check on the multiply
		return 0, false
	}
	if prod == math.MinInt64 {
		return 0, false
	}
	if prod < 0 {
		prod = -prod
	}
	return prod, true
}

// lcmDecimal is the lcm of two decimals, NON-NEGATIVE at scale max(sₐ, s_b): |a/gcd·b| (the a/gcd
// division is exact). lcm(_, 0) = 0. A magnitude over the decimal value cap traps 22003 via the mul.
func lcmDecimal(a, b Decimal) (Decimal, error) {
	target := a.Scale
	if b.Scale > target {
		target = b.Scale
	}
	if a.IsZero() || b.IsZero() {
		return Decimal{Scale: target}, nil // zero at the target scale (empty Limbs)
	}
	g, err := gcdDecimal(a, b)
	if err != nil {
		return Decimal{}, err
	}
	q, err := a.Div(g)
	if err != nil {
		return Decimal{}, err
	}
	prod, err := q.Mul(b)
	if err != nil {
		return Decimal{}, err
	}
	return prod.Abs().RoundToScale(target), nil
}

// satAdd1 is n+1 saturating at MaxInt64 (so an out-of-int4 count+1 stays out of range for the
// width_bucket range-check rather than wrapping to a negative).
func satAdd1(n int64) int64 {
	if n == math.MaxInt64 {
		return math.MaxInt64
	}
	return n + 1
}

// widenFloatToF64 wraps an f32-typed node in an f32 → f64 cast (an ordinary cast, whose cost stays
// observable) — how CASE / COALESCE / GREATEST / LEAST widen a mixed-width float set to its unified
// f64. Any other node is returned unchanged.
func widenFloatToF64(node *rExpr, t resolvedType) *rExpr {
	if t.kind == rtFloat32 {
		return &rExpr{kind: reCast, operand: node, result: scalarFloat64}
	}
	return node
}

// castToText converts a resolved operand of type ity to text (casts.toml, types.md §5) — the
// explicit `x::text` cast and the `text || anynonarray` operand rendering (string-functions.md §5)
// share it. target is the text-family cast target (text, or varchar whose length varcharLen
// truncates). Composite and range sources stay 0A000.
func castToText(rinner *rExpr, ity resolvedType, target scalarType, varcharLen *uint32, params *paramTypes) (*rExpr, error) {
	cast := &rExpr{kind: reCast, operand: rinner, result: target, varchar: varcharLen}
	switch ity.kind {
	case rtNull:
		// A NULL adapts (NULL → NULL, no truncation needed).
		return rinner, nil
	case rtText:
		// text → text: the identity, UNLESS a varchar(n) length is present — then it becomes a
		// reCast node that silently truncates to n code points at eval.
		if varcharLen != nil {
			return cast, nil
		}
		return rinner, nil
	case rtTimestamptz:
		// timestamptz → text reads the session zone, so it is STABLE (42P17 in an index
		// expression), as PG's timestamptz_out is.
		params.nonimmutable = true
		return cast, nil
	case rtInt, rtDecimal, rtFloat32, rtFloat64, rtBool, rtBytea, rtUuid, rtTimestamp, rtDate,
		rtInterval, rtJson, rtJsonb, rtJsonPath:
		return cast, nil
	case rtArray:
		// array → text (spec/design/array.md §7): array_out renders {…} per row. Explicit only,
		// like the scalar casts above. Handled by reArrayCast. A timestamptz anywhere in the
		// element type renders in the session zone, so the cast is then STABLE (timezones.md
		// §9.5).
		params.nonimmutable = params.nonimmutable || (ity.elem != nil && ity.elem.containsTimestamptz())
		return &rExpr{kind: reArrayCast, operand: rinner}, nil
	default:
		return nil, newError(FeatureNotSupported, "casting to text is not supported yet")
	}
}
