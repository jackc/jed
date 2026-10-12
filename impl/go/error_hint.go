package jed

import "errors"

// Error Hint text (spec/design/error-fields.md §11): PostgreSQL's misspelled-column suggestion
// (errorMissingColumn in parse_relation.c) and its table-alias suggestion, plus the scope walk that
// stamps a column reference's Position (§10.2). The matcher is pure over the names in scope; the
// scope gathers the candidates in the §11.1 walk order.

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

// locateColumnError stamps a column reference's 42703 / 42702 / 42P01 with its position, and adds
// the misspelled-column (§11.1) or table-alias (§11.2) hint. An error already positioned came from an
// inner site, which wins.
func (s *scope) locateColumnError(err error, qualifier string, hasQualifier bool, name string, pos srcPos) error {
	var e *EngineError
	if !errors.As(err, &e) || e.Position != 0 {
		return err
	}
	switch e.State {
	case UndefinedColumn:
		e.withPosition(int32(pos)).withHint(s.fuzzyColumnHint(qualifier, hasQualifier, name))
	case UndefinedTable:
		e.withPosition(int32(pos))
		if hasQualifier {
			e.withHint(s.tableAliasHint(qualifier))
		}
	case AmbiguousColumn:
		e.withPosition(int32(pos))
	}
	return err
}

// hintRelation is one labeled relation of the §11.1 walk: its label and column names.
type hintRelation struct {
	label   string
	columns []string
}

// hintRelations is the labeled relations of the §11.1 walk at this level: the FROM relations in
// order (a write through a view contributes the layer's columns), then the inner relations of an
// unaliased parenthesized join.
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
		out = append(out, hintRelation{label: r.label, columns: cols})
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

// fuzzyColumnHint is PostgreSQL's misspelled-column hint over the scope chain
// (spec/design/error-fields.md §11.1), or "".
func (s *scope) fuzzyColumnHint(qualifier string, hasQualifier bool, name string) string {
	m := newFuzzyColumnMatch(qualifier, hasQualifier, name)
	for level := s; level != nil; level = level.parent {
		for _, rel := range level.hintRelations() {
			penalty := m.penalty(rel.label)
			for _, col := range rel.columns {
				m.offer(rel.label, penalty, col)
			}
		}
	}
	return m.hint()
}

// tableAliasHint is the table-alias hint (spec/design/error-fields.md §11.2): the first base table or
// view reference in the scope chain whose catalog name is qualifier under a different label, or "".
func (s *scope) tableAliasHint(qualifier string) string {
	for level := s; level != nil; level = level.parent {
		for _, r := range level.rels {
			catalogName := ""
			switch {
			case r.view != "":
				catalogName = r.view
			case r.base:
				catalogName = r.table.Name
			}
			if catalogName != "" && catalogName == qualifier && r.label != qualifier && r.label != "" {
				return aliasHint(r.label)
			}
		}
	}
	return ""
}
