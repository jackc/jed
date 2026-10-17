package jed

import (
	"errors"
	"slices"
)

// Error Hint text (spec/design/error-fields.md §11): PostgreSQL's misspelled-column suggestion
// (errorMissingColumn in parse_relation.c), its table-alias suggestion, its LATERAL / qualified-name
// hints over the FROM relations a reference cannot see (§11.8), PostgreSQL's fixed hint sentences
// (§11.7), plus the scope walk that stamps a column reference's Position (§10.2). The matcher is pure over the names in scope; the
// scope gathers the candidates in the §11.1 walk order.

// PostgreSQL 18's fixed hint sentences (spec/design/error-fields.md §11.7–§11.8), byte-for-byte.
const (
	// hintFunctionNoMatch — 42883, no function matches a call's name and argument types (parse_func.c).
	hintFunctionNoMatch = "No function matches the given name and argument types. You might need to add explicit type casts."
	// hintFunctionAmbiguous — 42725, several functions match and none is best (parse_func.c).
	hintFunctionAmbiguous = "Could not choose a best candidate function. You might need to add explicit type casts."
	// hintOperatorNoMatch — 42883, no binary operator matches its operand types (parse_oper.c op_error).
	hintOperatorNoMatch = "No operator matches the given name and argument types. You might need to add explicit type casts."
	// hintOperatorNoMatchPrefix — 42883, no prefix operator matches its operand type (op_error, one
	// argument).
	hintOperatorNoMatchPrefix = "No operator matches the given name and argument type. You might need to add an explicit type cast."
	// hintOperatorAmbiguous — 42725, several operators match and none is best (op_error).
	hintOperatorAmbiguous = "Could not choose a best candidate operator. You might need to add explicit type casts."
	// hintNoOrderingOperator — 42883, a sort key's type has no ordering operator
	// (get_sort_group_operators).
	hintNoOrderingOperator = "Use an explicit ordering operator or modify the query."
	// hintAssignmentMismatch — 42804, a value of the wrong type for its column or default
	// (transformAssignedExpr, cookDefault).
	hintAssignmentMismatch = "You will need to rewrite or cast the expression."
	// hintRecursiveTermType — 42804, a recursive CTE column whose recursive term changes its type
	// (parse_cte.c).
	hintRecursiveTermType = "Cast the output of the non-recursive term to the correct type."
	// hintLateralColumn — 42703, the column is in a FROM item to the left of a non-LATERAL subquery
	// (errorMissingColumn).
	hintLateralColumn = "To reference that column, you must mark this subquery with LATERAL."
	// hintLateralTable — 42P01, the table is a FROM item to the left of a non-LATERAL subquery
	// (errorMissingRTE).
	hintLateralTable = "To reference that table, you must mark this subquery with LATERAL."
	// hintQualifyColumn — 42703, the column's relation is visible only by its qualifier
	// (errorMissingColumn).
	hintQualifyColumn = "To reference that column, you must use a table-qualified name."
	// hintTryQualified — 42703, several unreachable relations have the column (errorMissingColumn).
	hintTryQualified = "Try using a table-qualified name."
	// hintCollationImplicitMismatch — 42P21, two different implicit collations meet where a collation
	// is required at plan time (select_common_collation, assign_collations_walker, parse_collate.c).
	hintCollationImplicitMismatch = "You can choose the collation by applying the COLLATE clause to one or both expressions."
	// hintRecursiveTermCollation — 42P21, a recursive CTE's recursive term changes a column's
	// collation (parse_cte.c).
	hintRecursiveTermCollation = "Use the COLLATE clause to set the collation of the non-recursive term."
	// hintIndeterminateCollation — 42P22, no collation could be determined for a value that needs one
	// (varstr_cmp, intorel_startup, …).
	hintIndeterminateCollation = "Use the COLLATE clause to set the collation explicitly."
	// hintDropCascade — 2BP01, a DROP (or ALTER TABLE … DROP) that other objects depend on, where the
	// statement accepts CASCADE (reportDependentObjects, dependency.c).
	hintDropCascade = "Use DROP ... CASCADE to drop the dependent objects too."
)

// dropOwnerInsteadHint — 2BP01, dropping an index a constraint owns (findDependentObjects,
// dependency.c).
func dropOwnerInsteadHint(constraint, table string) string {
	return "You can drop constraint " + constraint + " on table " + table + " instead."
}

// unreachableColumnDetail — 42703 Detail: one relation the reference cannot see has the column
// (errorMissingColumn, spec/design/error-fields.md §11.8).
func unreachableColumnDetail(column, label string) string {
	return "There is a column named \"" + column + "\" in table \"" + label +
		"\", but it cannot be referenced from this part of the query."
}

// unreachableColumnsDetail — 42703 Detail: several relations the reference cannot see have the
// column (errorMissingColumn).
func unreachableColumnsDetail(column string) string {
	return "There are columns named \"" + column +
		"\", but they are in tables that cannot be referenced from this part of the query."
}

// unreachableTableDetail — 42P01 Detail: the qualifier names a relation the reference cannot see
// (errorMissingRTE).
func unreachableTableDetail(label string) string {
	return "There is an entry for table \"" + label + "\", but it cannot be referenced from this part of the query."
}

// maxFuzzyDistance is PostgreSQL's MAX_FUZZY_DISTANCE; the search starts one above it.
const maxFuzzyDistance = 3

// levenshtein is the Levenshtein distance from a to b over characters (runes), every edit costing 1.
// PostgreSQL's bounded varstr_levenshtein_less_equal may stop early above its bound, but a distance
// above the bound is never used, so the full distance gives the same outcome (§11.1).
func levenshtein(a, b string) int {
	br := []rune(b)
	prev := make([]int, len(br)+1)
	cur := make([]int, len(br)+1)
	for j := range prev {
		prev[j] = j
	}
	i := 0
	for _, ca := range a {
		cur[0] = i + 1
		for j, cb := range br {
			sub := prev[j]
			if ca != cb {
				sub++
			}
			cur[j+1] = min(sub, prev[j+1]+1, cur[j]+1)
		}
		prev, cur = cur, prev
		i++
	}
	return prev[len(br)]
}

// fuzzyCandidate is one (label, column) match of the §11.1 search.
type fuzzyCandidate struct {
	label  string
	column string
}

// fuzzyColumnMatch is the fuzzy-match state of §11.1 step 3, fed candidates in walk order.
type fuzzyColumnMatch struct {
	// target is the written column name c.
	target string
	// qualifier is the written qualifier q; hasQualifier is false for a bare reference.
	qualifier    string
	hasQualifier bool
	distance     int
	first        *fuzzyCandidate
	second       *fuzzyCandidate
	exact        bool
}

func newFuzzyColumnMatch(qualifier string, hasQualifier bool, target string) *fuzzyColumnMatch {
	return &fuzzyColumnMatch{
		target:       target,
		qualifier:    qualifier,
		hasQualifier: hasQualifier,
		distance:     maxFuzzyDistance + 1,
	}
}

// penalty is the relation penalty for a relation labeled label (§11.1 step 2).
func (m *fuzzyColumnMatch) penalty(label string) int {
	if !m.hasQualifier {
		return 0
	}
	return levenshtein(m.qualifier, label)
}

// offer offers one candidate column of the relation labeled label, whose penalty is penalty.
func (m *fuzzyColumnMatch) offer(label string, penalty int, column string) {
	if penalty > m.distance {
		return
	}
	d := levenshtein(column, m.target)
	if penalty == 0 && d == 0 {
		m.exact = true
	}
	// len in bytes, as PostgreSQL's strlen.
	if d > len(m.target)/2 {
		return
	}
	d += penalty
	candidate := &fuzzyCandidate{label: label, column: column}
	if d < m.distance {
		m.distance = d
		m.first = candidate
		m.second = nil
	} else if d == m.distance {
		if m.second != nil {
			// Three at one distance are too many; keep the distance so only a closer match counts.
			m.first = nil
			m.second = nil
		} else if m.first != nil {
			m.second = candidate
		}
	}
}

// hint is the hint text (§11.1 step 5), or "" — no match, or an exact match (step 4).
func (m *fuzzyColumnMatch) hint() string {
	if m.exact || m.first == nil {
		return ""
	}
	if m.second == nil {
		return "Perhaps you meant to reference the column \"" + m.first.label + "." + m.first.column + "\"."
	}
	return "Perhaps you meant to reference the column \"" + m.first.label + "." + m.first.column +
		"\" or the column \"" + m.second.label + "." + m.second.column + "\"."
}

// aliasHint is the §11.2 table-alias hint for an aliased relation labeled label.
func aliasHint(label string) string {
	return "Perhaps you meant to reference the table alias \"" + label + "\"."
}

// resolveBareAt is resolveBare for a column reference written at pos: a name-resolution error
// carries the reference's position and its hint (spec/design/error-fields.md §10.2, §11).
func (s *scope) resolveBareAt(name string, pos srcPos) (resolved, error) {
	r, err := s.resolveBare(name)
	if err != nil {
		return resolved{}, s.locateColumnError(err, "", false, name, pos)
	}
	return r, nil
}

// resolveQualifiedAt is resolveQualified for a reference written at pos (§10.2, §11).
func (s *scope) resolveQualifiedAt(qualifier, name string, pos srcPos) (resolved, error) {
	r, err := s.resolveQualified(qualifier, name)
	if err != nil {
		return resolved{}, s.locateColumnError(err, qualifier, true, name, pos)
	}
	return r, nil
}

// locateColumnError stamps a column reference's 42703 / 42702 / 42P01 / 42P09 / 42P10 with its position, and adds
// the misspelled-column (§11.1), table-alias (§11.2), or LATERAL (§11.8) hint. An error already
// positioned came from an inner site, which wins.
func (s *scope) locateColumnError(err error, qualifier string, hasQualifier bool, name string, pos srcPos) error {
	var e *EngineError
	if !errors.As(err, &e) || e.Position != 0 {
		return err
	}
	var hint, detail string
	switch e.State {
	case UndefinedColumn:
		hint, detail = s.columnHint(qualifier, hasQualifier, name)
		e.withPosition(int32(pos)).withHint(hint)
	case UndefinedTable:
		e.withPosition(int32(pos))
		if hasQualifier {
			hint, detail = s.tableHint(qualifier)
			e.withHint(hint)
		}
	case AmbiguousColumn, AmbiguousAlias, InvalidColumnReference:
		e.withPosition(int32(pos))
	}
	if e.Detail == "" {
		e.withDetail(detail)
	}
	return err
}

// unseenRel is a relation of a query level's FROM clause that a reference there cannot see.
// PostgreSQL's range table holds such relations, and its 42703 / 42P01 hints search them
// (spec/design/error-fields.md §11.8).
type unseenRel struct {
	label   string
	columns []string
	// catalog is the base table or view the relation names (the §11.2 search), else "".
	catalog string
	// lateralOK is whether marking the reference's subquery LATERAL would make the relation visible.
	lateralOK bool
	// qualifiedOnly is whether the relation is an inner relation of a completed unaliased join, so its
	// columns are reachable only through its qualifier.
	qualifiedOnly bool
}

// unseenFrame is the relations to the left of a non-LATERAL derived table or parenthesized join,
// recorded while its body is planned (spec/design/error-fields.md §11.8). depth is the scope-chain
// depth of the scope the body is planned against, so the hint walk visits the relations right after
// the body's own outermost level. parenJoin marks a parenthesized join's body: its ON clauses cannot
// reach these relations even with LATERAL.
type unseenFrame struct {
	depth     int
	parenJoin bool
	rels      []unseenRel
}

// scopeHints is a scope's hint-only facts (spec/design/error-fields.md §11.8). They never affect
// resolution.
type scopeHints struct {
	// unseen is this level's FROM relations the scope cannot see, before its own: a JOIN's ON clause
	// cannot see the comma items to its left.
	unseen []unseenRel
	// onClause is whether the scope resolves a JOIN's ON clause.
	onClause bool
	// lateralActive is whether the scope resolves a table function's arguments (PostgreSQL's
	// p_lateral_active): a hint never suggests LATERAL from here.
	lateralActive bool
}

// hintEntry is one relation the hint walk visits (spec/design/error-fields.md §11.8), in walk order.
type hintEntry struct {
	label         string
	columns       []string
	catalog       string
	visible       bool
	lateralOK     bool
	qualifiedOnly bool
}

// scopeDepth is the number of levels in a scope chain (0 for none).
func scopeDepth(s *scope) int {
	depth := 0
	for level := s; level != nil; level = level.parent {
		depth++
	}
	return depth
}

// hintRelation is one labeled relation of the §11.1 walk: its label, column names, and the base table
// or view it names (§11.2), else "".
type hintRelation struct {
	label   string
	columns []string
	catalog string
}

// hintRelations is the labeled relations this level can see, in the §11.1 walk order: the FROM
// relations in order (a write through a view contributes the layer's columns), then the inner
// relations of an unaliased parenthesized join.
func (s *scope) hintRelations() []hintRelation {
	var out []hintRelation
	for _, r := range s.rels {
		if r.label == "" {
			continue
		}
		var cols []string
		if r.layer != nil {
			for _, c := range r.layer.columns {
				cols = append(cols, c.Name)
			}
		} else {
			for _, c := range r.table.Columns {
				cols = append(cols, c.Name)
			}
		}
		out = append(out, hintRelation{label: r.label, columns: cols, catalog: relCatalogName(&r)})
	}
	for _, sr := range s.subRels {
		if sr.label == "" {
			continue
		}
		var cols []string
		for _, c := range sr.table.Columns {
			cols = append(cols, c.Name)
		}
		out = append(out, hintRelation{label: sr.label, columns: cols})
	}
	return out
}

// hintWalk is every relation a hint searches, in PostgreSQL's range-table walk order
// (spec/design/error-fields.md §11.1, §11.8): at each level outward, the relations this level cannot
// see to the left of an ON clause, the visible relations, then the relations hidden from the derived
// bodies planned against the level's parent (innermost first).
func (s *scope) hintWalk() []hintEntry {
	var frames []unseenFrame
	if s.catalog != nil {
		frames = s.catalog.unseenFrames
	}
	lateralActive := s.hints.lateralActive
	unseen := func(u unseenRel, blocked bool) hintEntry {
		return hintEntry{
			label:         u.label,
			columns:       u.columns,
			catalog:       u.catalog,
			lateralOK:     u.lateralOK && !blocked && !lateralActive,
			qualifiedOnly: u.qualifiedOnly && !blocked,
		}
	}
	var out []hintEntry
	depth := scopeDepth(s)
	for level := s; level != nil; level = level.parent {
		for _, u := range level.hints.unseen {
			out = append(out, unseen(u, true))
		}
		for _, r := range level.hintRelations() {
			out = append(out, hintEntry{label: r.label, columns: r.columns, catalog: r.catalog, visible: true})
		}
		depth--
		for fi := len(frames) - 1; fi >= 0; fi-- {
			f := frames[fi]
			if f.depth != depth {
				continue
			}
			// A parenthesized join's ON clause cannot see the relations to the join's left.
			blocked := level.hints.onClause && f.parenJoin
			for _, u := range f.rels {
				out = append(out, unseen(u, blocked))
			}
		}
	}
	return out
}

// columnHint is PostgreSQL's errorMissingColumn hint and detail (spec/design/error-fields.md §11.1,
// §11.8), each "" when absent: a relation that has the column but cannot be seen from here yields the
// "cannot be referenced" detail and the LATERAL or qualified-name hint; otherwise the misspelled-column
// suggestion.
func (s *scope) columnHint(qualifier string, hasQualifier bool, name string) (string, string) {
	entries := s.hintWalk()
	m := newFuzzyColumnMatch(qualifier, hasQualifier, name)
	var exact []hintEntry
	for _, e := range entries {
		penalty := m.penalty(e.label)
		for _, col := range e.columns {
			m.offer(e.label, penalty, col)
		}
		if penalty == 0 && slices.Contains(e.columns, name) {
			exact = append(exact, e)
		}
	}
	switch {
	case len(exact) == 0:
		return m.hint(), ""
	case len(exact) == 1:
		hint := ""
		if exact[0].lateralOK {
			hint = hintLateralColumn
		} else if !hasQualifier && exact[0].qualifiedOnly {
			hint = hintQualifyColumn
		}
		return hint, unreachableColumnDetail(name, exact[0].label)
	case !hasQualifier:
		return hintTryQualified, unreachableColumnsDetail(name)
	}
	return "", unreachableColumnsDetail(name)
}

// tableHint is PostgreSQL's errorMissingRTE hint and detail (spec/design/error-fields.md §11.2,
// §11.8), each "" when absent: the first relation in the walk labeled qualifier, or naming that base
// table or view. A visible one under a different label is the table-alias hint; any other found
// relation gets the "cannot be referenced" detail, plus the LATERAL hint when it is hidden from a
// non-LATERAL subquery.
func (s *scope) tableHint(qualifier string) (string, string) {
	for _, e := range s.hintWalk() {
		if e.label != qualifier && (e.catalog == "" || e.catalog != qualifier) {
			continue
		}
		if e.visible && e.label != qualifier {
			return aliasHint(e.label), ""
		}
		hint := ""
		if !e.visible && e.lateralOK {
			hint = hintLateralTable
		}
		return hint, unreachableTableDetail(e.label)
	}
	return "", ""
}
