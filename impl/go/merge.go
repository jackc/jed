package jed

// MERGE (spec/design/merge.md): resolve the statement against its per-clause scopes (§2), run the
// matching pass over the buffered source and the target probe (§3), plan one action per joined pair
// (§4), validate the placed rows against the statement's end state (§5), project RETURNING (§7), and
// write (§6). Two-phase and all-or-nothing, like every jed write.

import (
	"fmt"
	"slices"
)

// mergeClausePlan is a resolved WHEN clause. Its condition and expressions read the clause kind's
// evaluation row: [target | source] for MATCHED, [source] for NOT MATCHED [BY TARGET], [target] for
// NOT MATCHED BY SOURCE (merge.md §2).
type mergeClausePlan struct {
	kind      mergeMatch
	condition *rExpr
	action    mergeActionKind
	// assigns is an UPDATE action's resolved assignments, in written order.
	assigns []assignPlan
	// provided[i] = p: table column i takes INSERT value slot p; -1: its default.
	provided []int
	// values[p] is an INSERT action's resolved slot p, nil for a DEFAULT slot (whose column is then
	// defaulted) or a value an OVERRIDING USER VALUE identity column discards.
	values []*rExpr
	// rowTable is the table an INSERT action's row is built over: the target, with the view column
	// defaults an INSERT through a view applies in place of the table's (views.md §16.1).
	rowTable *catTable
	// defaults are those columns' expression defaults, resolved for this action (constraints.md §2).
	defaults []*rExpr
}

// mergeView is a MERGE into an automatically updatable view, retargeted at its base table (merge.md
// §8, views.md §15): the write layering of each action clause (nil for DO NOTHING), each validated for
// its own action kind and target columns, and the layering the statement's scopes expose.
type mergeView struct {
	clauses []*viewWrite
	top     *viewWrite
}

// mergeReturning is a resolved RETURNING projection: the nodes and the output schema.
type mergeReturning struct {
	nodes   []*rExpr
	names   []string
	types   []resolvedType
	derivs  []deriv
	typmods []typmod
}

// mergePlan is a resolved MERGE (merge.md §2/§3), owning everything execution needs.
type mergePlan struct {
	source queryPlan
	// m is the source's width.
	m int
	// on is the join condition over [target | source]: it decides a match.
	on *rExpr
	// probe is the join condition with the source columns as outer references (level 1): the target
	// probe's access path is planned from it (§3). It never decides a match.
	probe   *rExpr
	clauses []mergeClausePlan
	// hash is the hash form of the matching loop (§3), chosen at plan time when its estimate is
	// cheaper than the per-source-row probe (estimator.md §9.4); nil — the nested loop.
	hash *hashJoinPlan
	// targetFilter is the target view's WHERE chain over the target row (views.md §15.2), for the
	// not-matched-by-source scan; the join condition already leads with it.
	targetFilter *rExpr
	// viewChecks are the check-option checks of a MERGE into a view (views.md §15.6), over a placed
	// row.
	viewChecks []viewCheck
	returning  *mergeReturning
}

// hasBySource reports whether a WHEN NOT MATCHED BY SOURCE clause runs the full target scan (§3).
func (p *mergePlan) hasBySource() bool {
	return slices.ContainsFunc(p.clauses, func(c mergeClausePlan) bool { return c.kind == mergeNotMatchedBySource })
}

// mergeAction is one action of the action list (merge.md §4), in matching-pass order. src is the
// source row index (-1 for a not-matched-by-source row).
type mergeAction struct {
	kind mergeActionKind
	// key is an updated row's old key or a deleted row's key; nil for an insert.
	key []byte
	// newKey is an updated row's new key.
	newKey []byte
	// newRow is the inserted row or an updated row's new version.
	newRow storedRow
	// oldRow is an updated or deleted row's pre-statement version.
	oldRow storedRow
	src    int
	clause int
}

// mergeScope is a scope over rels with no parent: the shape every MERGE scope takes (merge.md §2).
func mergeScope(catalog *engine, rels []scopeRel, ctes []*cteBinding) *scope {
	return &scope{rels: rels, catalog: catalog, allowSubquery: true, ctes: ctes}
}

// mergeSourceRel is the source relation at flat offset, labeled by its alias or name (empty for an
// unaliased derived table, whose columns are then reachable only bare).
func mergeSourceRel(table *catTable, label string, offset int) scopeRel {
	return scopeRel{label: label, table: table, offset: offset}
}

// mergeVersionRel is a qualifier-only row-version relation (old / new) over the target's columns — a
// view's columns for a MERGE into a view.
func mergeVersionRel(table *catTable, vw *viewWrite, label string, offset int) scopeRel {
	rel := scopeRel{label: label, table: table, offset: offset, qualifierOnly: true}
	if vw != nil {
		rel.layer = vw.top
	}
	return rel
}

// mergeTargetRel is the target relation at flat offset: targetRel moved.
func mergeTargetRel(table *catTable, vw *viewWrite, alias *string, offset int) scopeRel {
	rel := targetRel(table, vw, alias)
	rel.offset = offset
	return rel
}

// withSourceGroup exposes a joined source's inner labels and hides its USING/NATURAL copies
// (grammar.md §15) when the source sits at flat offset of s, FROM ordinal rel (merge.md §2). A no-op
// for any other source.
func withSourceGroup(s *scope, group *joinGroupLayout, offset, rel int) {
	if group == nil {
		return
	}
	s.subRels = make([]subRel, len(group.subRels))
	for i, sr := range group.subRels {
		slots := make([]int, len(sr.slots))
		for k, slot := range sr.slots {
			slots[k] = offset + slot
		}
		s.subRels[i] = subRel{label: sr.label, table: sr.table, slots: slots, rel: rel}
	}
	s.hidden = make([]int, len(group.hidden))
	for i, h := range group.hidden {
		s.hidden[i] = offset + h
	}
}

// executeMerge runs a MERGE (spec/design/merge.md). An error names the target's database
// (error-fields.md §12).
func (db *engine) executeMerge(m *mergeStmt, params []Value, ctx cteCtx) (outcome, error) {
	out, err := db.executeMergeTarget(m, params, ctx)
	if err != nil {
		return out, db.stampDatabase(m.DB, err)
	}
	return out, nil
}

// mergeTarget makes the target checks every MERGE makes before resolving anything (merge.md §8): a
// catalog relation is 42809, a read-only attachment 25006, a missing table 42P01. A view target is
// retargeted at its base table first (rewriteMergeTarget): the returned statement then names the
// table (the written one is left intact), and the returned mergeView carries the view layering.
func (db *engine) mergeTarget(m *mergeStmt) (*mergeStmt, *mergeView, error) {
	if err := checkCatalogRelWrite(m.Table); err != nil {
		return nil, nil, err
	}
	if err := db.checkAttachmentWritable(m.DB); err != nil {
		return nil, nil, err
	}
	m, mv, err := db.rewriteMergeTarget(m)
	if err != nil {
		return nil, nil, err
	}
	if err := db.checkTableQualifier(m.DB, m.Table); err != nil {
		return nil, nil, atTarget(err, m.TablePos)
	}
	if _, ok := db.lkpTableScoped(m.DB, m.Table); !ok {
		return nil, nil, newError(UndefinedTable, "table does not exist: "+m.Table).withPosition(int32(m.TablePos))
	}
	return m, mv, nil
}

// mergeNode is the plan-node label of a MERGE (explain.md §5): `Merge <table>`, naming the base table
// of a view target as EXPLAIN renders it (views.md §15.5). A target the rewrite refuses names itself;
// its error surfaces when the MERGE runs.
func (db *engine) mergeNode(m *mergeStmt) string {
	if rewritten, _, err := db.rewriteMergeTarget(m); err == nil {
		m = rewritten
	}
	return "Merge " + m.Table
}

// mergeTable is the target table of a MERGE mergeTarget validated.
func (db *engine) mergeTable(m *mergeStmt) *catTable {
	table, ok := db.lkpTableScoped(m.DB, m.Table)
	if !ok {
		panic("mergeTarget validated the target")
	}
	return table
}

// rewriteMergeTarget retargets a MERGE into an automatically updatable view at its base table
// (merge.md §8, views.md §15.2), clause by clause in written order: an UPDATE's SET columns and an
// INSERT's column list name view columns and are validated like the standalone statement's (42703,
// 42701, a read-only column 0A000, a view not updatable for the action 55000), then become the base
// columns; `SET c = DEFAULT` assigns the view column's default. An INSERT without a column list
// targets the view's first columns, one per value. It returns a copy of the statement and the write,
// or the statement unchanged and nil when the target is a table.
func (db *engine) rewriteMergeTarget(m *mergeStmt) (*mergeStmt, *mergeView, error) {
	view, err := db.dmlTargetView(m.DB, m.Table)
	if err != nil {
		return nil, nil, err
	}
	if view == nil {
		return m, nil, nil
	}
	c := *m
	c.Clauses = slices.Clone(m.Clauses)
	clauses := make([]*viewWrite, len(c.Clauses))
	for ci := range c.Clauses {
		cl := &c.Clauses[ci]
		var vw *viewWrite
		switch cl.Action {
		case mergeDoNothing:
		case mergeDelete:
			if vw, err = db.prepareViewWrite(m.DB, m.Table, writeDelete, nil, nil, nil, nil); err != nil {
				return nil, nil, err
			}
		case mergeUpdate:
			targets := make([]string, len(cl.Assignments))
			pos := make([]srcPos, len(cl.Assignments))
			for i, a := range cl.Assignments {
				targets[i], pos[i] = a.Column, a.Pos
			}
			if vw, err = db.prepareViewWrite(m.DB, m.Table, writeUpdate, targets, pos, nil, nil); err != nil {
				return nil, nil, err
			}
			assigns := make([]assignment, len(cl.Assignments))
			for i, a := range cl.Assignments {
				if a.IsDefault {
					a.IsDefault = false
					if a.Value, err = db.viewSetDefault(vw.view, a.Column); err != nil {
						return nil, nil, err
					}
				}
				a.Column = vw.targets[i]
				assigns[i] = a
			}
			cl.Assignments = assigns
		case mergeInsert:
			targets := cl.Columns
			if targets == nil {
				targets = slices.Clone(view.Columns[:min(len(cl.Values), len(view.Columns))])
			}
			if vw, err = db.prepareViewWrite(m.DB, m.Table, writeInsert, targets, cl.ColumnPos, nil, nil); err != nil {
				return nil, nil, err
			}
			cl.Columns = append(make([]string, 0, len(vw.targets)), vw.targets...)
		}
		clauses[ci] = vw
	}
	// The layering the scopes expose: the chain validated for the weakest action, so a MERGE of only
	// DO NOTHING clauses still names an updatable view.
	top, err := db.prepareViewWrite(m.DB, m.Table, writeDelete, nil, nil, nil, nil)
	if err != nil {
		return nil, nil, err
	}
	c.Table, c.DB = top.table, nil
	return &c, &mergeView{clauses: clauses, top: top}, nil
}

// planMerge resolves a MERGE against its target (merge.md §2): the source query, the join condition,
// every WHEN clause in written order, then RETURNING — PostgreSQL's analysis order.
func (db *engine) planMerge(m *mergeStmt, table *catTable, mv *mergeView, bindings []*cteBinding, ptypes *paramTypes) (*mergePlan, error) {
	n := len(table.Columns)
	var vw *viewWrite
	if mv != nil {
		vw = mv.top
	}
	source, err := db.planQuery(m.Source, nil, bindings, ptypes)
	if err != nil {
		return nil, err
	}
	label := ""
	if m.SourceLabel != nil {
		label = *m.SourceLabel
	}
	// A joined source is an unaliased join group (merge.md §2): one relation with no label whose inner
	// labels stay visible, its columns the group's (hidden USING copies included).
	group := derivedJoinGroup(&source)
	var srcTable *catTable
	if group != nil {
		srcTable = &catTable{Columns: group.columns}
	} else if srcTable, err = cteSyntheticTable(label, &source, nil); err != nil {
		return nil, err
	}
	mw := len(srcTable.Columns)
	targetLabel := table.Name
	if vw != nil {
		targetLabel = vw.view
	}
	if m.Alias != nil {
		targetLabel = *m.Alias
	}
	if label != "" && label == targetLabel {
		return nil, newError(DuplicateAlias, fmt.Sprintf("name \"%s\" specified more than once", label))
	}
	both := mergeScope(db, []scopeRel{mergeTargetRel(table, vw, m.Alias, 0), mergeSourceRel(srcTable, label, n)}, bindings)
	withSourceGroup(both, group, n, 1)
	srcOnly := mergeScope(db, []scopeRel{mergeSourceRel(srcTable, label, 0)}, bindings)
	withSourceGroup(srcOnly, group, 0, 0)
	tgtOnly := mergeScope(db, []scopeRel{mergeTargetRel(table, vw, m.Alias, 0)}, bindings)
	// A view target's rows are those its WHERE chain shows (views.md §15.2): the chain leads the join
	// condition (and filters the not-matched-by-source scan), AND-ed left-deep.
	lead := func(cond *rExpr) (*rExpr, error) {
		view, err := db.resolveWriteFilter(tgtOnly, table, vw, nil, &paramTypes{})
		if err != nil || view == nil {
			return cond, err
		}
		return &rExpr{kind: reAnd, lhs: view, rhs: cond}, nil
	}
	on, err := resolveBooleanFilter(both, &m.On, ptypes)
	if err != nil {
		return nil, err
	}
	if on, err = lead(on); err != nil {
		return nil, err
	}
	// The probe form: the target is the relation, the source an enclosing level (§3). The combined
	// resolution above already raised every name error.
	probeScope := mergeScope(db, []scopeRel{mergeTargetRel(table, vw, m.Alias, 0)}, bindings)
	probeScope.parent = srcOnly
	probe, err := resolveBooleanFilter(probeScope, &m.On, &paramTypes{})
	if err != nil {
		return nil, err
	}
	if probe, err = lead(probe); err != nil {
		return nil, err
	}
	targetFilter, err := db.resolveWriteFilter(tgtOnly, table, vw, nil, &paramTypes{})
	if err != nil {
		return nil, err
	}
	hash := db.mergeHashChoice(m.DB, table, source, on, probe, both)
	viewChecks, err := db.resolveViewChecks(table, vw)
	if err != nil {
		return nil, err
	}

	clauses := make([]mergeClausePlan, 0, len(m.Clauses))
	for ci := range m.Clauses {
		c := &m.Clauses[ci]
		s := both
		switch c.Kind {
		case mergeNotMatchedByTarget:
			s = srcOnly
		case mergeNotMatchedBySource:
			s = tgtOnly
		}
		cp := mergeClausePlan{kind: c.Kind, action: c.Action}
		if c.Condition != nil {
			if cp.condition, err = resolveBooleanFilter(s, c.Condition, ptypes); err != nil {
				return nil, err
			}
		}
		switch c.Action {
		case mergeUpdate:
			if cp.assigns, err = db.mergeAssignments(s, table, c.Assignments, ptypes); err != nil {
				return nil, err
			}
		case mergeInsert:
			// Through a view, the view column defaults replace the table's (views.md §16.1).
			rowTable := table
			if mv != nil {
				if cvw := mv.clauses[ci]; cvw != nil && len(cvw.defaultOverrides) > 0 {
					if rowTable, err = db.tableWithViewDefaults(table, cvw, c.Overriding); err != nil {
						return nil, err
					}
				}
			}
			provided, slotCol, err := mergeInsertColumns(rowTable, c.Columns, c.ColumnPos, c.Overriding, c.Values)
			if err != nil {
				return nil, err
			}
			// The omitted columns' expression defaults, resolved once for this action.
			need := make([]bool, n)
			anyNeed := false
			for i, p := range provided {
				if p < 0 {
					need[i], anyNeed = true, true
				}
			}
			defaults := make([]*rExpr, n)
			if anyNeed {
				if defaults, err = db.resolveDefaultExprsFor(rowTable, need, &paramTypes{}); err != nil {
					return nil, err
				}
			}
			nodes := make([]*rExpr, len(c.Values))
			for p, v := range c.Values {
				i := slotCol[p]
				// A DEFAULT slot, or a value an OVERRIDING USER VALUE identity column discards, is not
				// resolved (as INSERT).
				if v.IsDefault || i < 0 || provided[i] < 0 {
					continue
				}
				col := table.Columns[i]
				if target, ok := col.Type.AsScalar(); ok {
					node, ty, err := resolve(s, v.Expr, &target, &aggCtx{collecting: false}, ptypes)
					if err != nil {
						return nil, err
					}
					if !assignableTo(ty, target) {
						return nil, positionError(assignmentError(fmt.Sprintf(
							"column %s is of type %s but expression is of type %s",
							col.Name, col.Type.CanonicalName(), rtName(ty),
						)), exprLocation(v.Expr))
					}
					nodes[p] = node
				} else {
					node, err := resolveContainerAssign(s, col, v.Expr, &aggCtx{collecting: false}, ptypes)
					if err != nil {
						return nil, err
					}
					nodes[p] = node
				}
			}
			cp.provided, cp.values, cp.rowTable, cp.defaults = provided, nodes, rowTable, defaults
		}
		clauses = append(clauses, cp)
	}

	var returning *mergeReturning
	if m.Returning != nil {
		s, err := db.mergeReturningScope(table, vw, m.Alias, srcTable, label, m.Returning)
		if err != nil {
			return nil, err
		}
		withSourceGroup(s, group, 0, 0)
		s.ctes = bindings
		nodes, names, types, derivs, typmods, err := resolveProjections(s, m.Returning.Items, &aggCtx{collecting: false}, ptypes)
		if err != nil {
			return nil, err
		}
		returning = &mergeReturning{nodes: nodes, names: names, types: types, derivs: derivs, typmods: typmods}
	}
	return &mergePlan{
		source: source, m: mw, on: on, probe: probe, clauses: clauses, hash: hash,
		targetFilter: targetFilter, viewChecks: viewChecks, returning: returning,
	}, nil
}

// planMergeDetached is planMerge for a MERGE that is not about to run — a writable CTE's RETURNING
// schema, EXPLAIN's render and estimate. The CTE references the planning counts are restored: the
// orchestrator static-counts a data-modifying part's references (writable-cte.md §3).
func (db *engine) planMergeDetached(m *mergeStmt, table *catTable, mv *mergeView, bindings []*cteBinding, ptypes *paramTypes) (*mergePlan, error) {
	type savedRefs struct {
		refs     int
		expanded bool
	}
	saved := make([]savedRefs, len(bindings))
	for i, b := range bindings {
		saved[i] = savedRefs{b.refs, b.expanded}
	}
	plan, err := db.planMerge(m, table, mv, bindings, ptypes)
	for i, b := range bindings {
		b.refs, b.expanded = saved[i].refs, saved[i].expanded
	}
	return plan, err
}

// mergeHashChoice chooses the matching loop's form (merge.md §3, estimator.md §9.4): the hash join
// when the probe's selected path is a full scan, the join condition passes the hash join's structural
// gate (cost.md §3 "JOIN"), at least one conjunct equates a target column with a source column of the
// same keyable type, and the hash estimate is strictly cheaper than the nested loop's. The estimates
// read no CTE context, so EXPLAIN and execution choose identically.
func (db *engine) mergeHashChoice(dbScope *string, table *catTable, source queryPlan, on, probe *rExpr, s *scope) *hashJoinPlan {
	probePlan := db.planMutationScan(dbScope, table, probe, whereContradicts(probe))
	if probePlan.bound != nil || probePlan.contradiction {
		return nil
	}
	n := len(table.Columns)
	var conjuncts []*rExpr
	flattenHashJoinConjuncts(on, &conjuncts)
	var keys []hashJoinKey
	for _, e := range conjuncts {
		if !hashJoinSafeConjunct(e) {
			return nil
		}
		if e.op != opEq || e.lhs.kind != reColumn || e.rhs.kind != reColumn {
			continue
		}
		a, b := e.lhs.index, e.rhs.index
		var target, sourceCol int
		switch {
		case a < n && b >= n:
			target, sourceCol = a, b
		case a >= n && b < n:
			target, sourceCol = b, a
		default:
			continue
		}
		tTy, tok := hashJoinColumnType(s.rels, target)
		sTy, sok := hashJoinColumnType(s.rels, sourceCol)
		if !tok || !sok || !hashJoinTypesEqual(tTy, sTy) || !hashJoinKeyableType(tTy) {
			continue
		}
		keys = append(keys, hashJoinKey{left: sourceCol, right: target, ty: tTy})
	}
	if len(keys) == 0 {
		return nil
	}
	hash := &hashJoinPlan{keys: keys}
	sourceEstimate := db.estimateQueryPlan(source, nil)
	nested, hashed := db.estimateMergeLoop(table, dbScope, sourceEstimate, on, probe, hash, nil)
	if hashed == nil || hashed.root.cost() >= nested.root.cost() {
		return nil
	}
	return hash
}

// mergeAssignments resolves one UPDATE action's assignments against its clause scope — the
// standalone UPDATE rules: 42703, 42701, 428C9, the scalar / container assignment gates,
// SET col = DEFAULT.
func (db *engine) mergeAssignments(s *scope, table *catTable, assigns []assignment, ptypes *paramTypes) ([]assignPlan, error) {
	// SET col = DEFAULT resolves the assigned columns' expression defaults once per clause, as the
	// standalone UPDATE does (constraints.md §2). An unknown column is left to the 42703 below.
	var defaultExprs []*rExpr
	if slices.ContainsFunc(assigns, func(a assignment) bool { return a.IsDefault }) {
		need := make([]bool, len(table.Columns))
		for _, a := range assigns {
			if idx := table.ColumnIndex(a.Column); a.IsDefault && idx >= 0 {
				need[idx] = true
			}
		}
		var err error
		if defaultExprs, err = db.resolveDefaultExprsFor(table, need, &paramTypes{}); err != nil {
			return nil, err
		}
	}
	plans := make([]assignPlan, 0, len(assigns))
	for _, a := range assigns {
		idx := table.ColumnIndex(a.Column)
		if idx < 0 {
			return nil, newError(UndefinedColumn, "column does not exist: "+a.Column).withPosition(int32(a.Pos))
		}
		if c := table.Columns[idx].Identity; c != nil && *c == identityAlways && !a.IsDefault {
			return nil, newGeneratedAlwaysUpdate(a.Column)
		}
		for _, p := range plans {
			if p.idx == idx {
				return nil, newError(DuplicateColumn, "column "+a.Column+" assigned more than once")
			}
		}
		col := table.Columns[idx]
		plan := assignPlan{idx: idx, name: col.Name, typmod: columnTypmod(&col), notNull: col.NotNull}
		scalar, isScalar := col.Type.AsScalar()
		if isScalar {
			plan.target = scalar
		} else {
			ct := resolveColType(col.Type, db.readSnap().types)
			plan.colType = &ct
		}
		switch {
		case a.IsDefault:
			plan.source = defaultExprs[idx]
			if plan.source == nil {
				plan.source = valueToRExpr(defaultOrNull(col))
			}
		case isScalar:
			src, err := resolveScalarAssign(s, a.Value, scalar, a.Column, ptypes)
			if err != nil {
				return nil, err
			}
			plan.source = src
		default:
			src, err := resolveContainerAssign(s, col, a.Value, &aggCtx{collecting: false}, ptypes)
			if err != nil {
				return nil, err
			}
			plan.source = src
		}
		plans = append(plans, plan)
	}
	return plans, nil
}

// mergeReturningScope is the RETURNING scope (merge.md §7): the source at offset 0, the target at m,
// and the old / new qualifier-only versions at m + n / m + 2n, over the row
// [source | target | old | new | action]; merge_action() reads the last slot. Aliases follow
// grammar.md §32.
func (db *engine) mergeReturningScope(table *catTable, vw *viewWrite, alias *string, srcTable *catTable, label string, ret *returningClause) (*scope, error) {
	n := len(table.Columns)
	mw := len(srcTable.Columns)
	rels := []scopeRel{mergeSourceRel(srcTable, label, 0), mergeTargetRel(table, vw, alias, mw)}
	for _, explicit := range []struct {
		alias  *string
		offset int
	}{{ret.OldAlias, mw + n}, {ret.NewAlias, mw + 2*n}} {
		if explicit.alias == nil {
			continue
		}
		a := *explicit.alias
		for _, r := range rels {
			if r.label == a {
				return nil, newError(DuplicateAlias, "table name "+a+" specified more than once")
			}
		}
		rels = append(rels, mergeVersionRel(table, vw, a, explicit.offset))
	}
	for _, pseudo := range []struct {
		label   string
		offset  int
		aliased bool
	}{{"old", mw + n, ret.OldAlias != nil}, {"new", mw + 2*n, ret.NewAlias != nil}} {
		occupied := false
		for _, r := range rels {
			occupied = occupied || r.label == pseudo.label
		}
		if !pseudo.aliased && !occupied {
			rels = append(rels, mergeVersionRel(table, vw, pseudo.label, pseudo.offset))
		}
	}
	s := mergeScope(db, rels, nil)
	slot := mw + 3*n
	s.mergeAction = &slot
	return s, nil
}

func (db *engine) executeMergeTarget(written *mergeStmt, params []Value, ctx cteCtx) (outcome, error) {
	// The statement resolves before its values are bound; a plan that read one restarts it with the
	// values visible (estimator.md §6.1).
	planning := db.beginPlanning()
	defer planning.end()
	m, mv, err := db.mergeTarget(written)
	if err != nil {
		return outcome{}, err
	}
	table := db.mergeTable(m)
	if err := db.ensureCollationsWritable(table.Columns); err != nil {
		return outcome{}, err
	}
	n := len(table.Columns)
	pkMembers := table.PKIndices()
	rindexes, err := db.resolveTableIndexes(table)
	if err != nil {
		return outcome{}, err
	}
	checks, err := db.resolveChecks(table)
	if err != nil {
		return outcome{}, err
	}
	store := db.lkpStoreScoped(m.DB, m.Table)
	colTypes := store.colTypes
	colls := db.columnCollations(table.Columns)
	ptypes := &paramTypes{}
	plan, err := db.planMerge(m, table, mv, ctx.bindings, ptypes)
	if err != nil {
		return outcome{}, err
	}
	ptys, err := ptypes.finalize()
	if err != nil {
		return outcome{}, err
	}
	bound, err := bindParams(params, ptys, db.inputZone())
	if err != nil {
		return outcome{}, err
	}
	if planning.bind(bound) {
		var out outcome
		err := db.restartPlanning(bound, func() (err error) {
			out, err = db.executeMerge(written, params, ctx)
			return err
		})
		return out, err
	}

	// Fold the globally-uncorrelated subqueries once, in statement order: the source, the join
	// condition, each clause's condition and action expressions, then RETURNING (cost.md §3).
	meter := db.session.newMeter()
	contradiction := whereContradicts(plan.probe)
	if err := db.foldUncorrelatedInPlan(&plan.source, bound, ctx, &meter.Accrued); err != nil {
		return outcome{}, err
	}
	if err := db.foldUncorrelatedInRExpr(plan.on, bound, ctx, &meter.Accrued); err != nil {
		return outcome{}, err
	}
	if plan.targetFilter != nil {
		if err := db.foldUncorrelatedInRExpr(plan.targetFilter, bound, ctx, &meter.Accrued); err != nil {
			return outcome{}, err
		}
	}
	for ci := range plan.clauses {
		c := &plan.clauses[ci]
		if c.condition != nil {
			if err := db.foldUncorrelatedInRExpr(c.condition, bound, ctx, &meter.Accrued); err != nil {
				return outcome{}, err
			}
		}
		for i := range c.assigns {
			if err := db.foldUncorrelatedInRExpr(c.assigns[i].source, bound, ctx, &meter.Accrued); err != nil {
				return outcome{}, err
			}
		}
		for _, v := range c.values {
			if v == nil {
				continue
			}
			if err := db.foldUncorrelatedInRExpr(v, bound, ctx, &meter.Accrued); err != nil {
				return outcome{}, err
			}
		}
	}
	if plan.returning != nil {
		for _, node := range plan.returning.nodes {
			if err := db.foldUncorrelatedInRExpr(node, bound, ctx, &meter.Accrued); err != nil {
				return outcome{}, err
			}
		}
	}
	if err := db.foldViewChecks(plan.viewChecks, bound, ctx, meter); err != nil {
		return outcome{}, err
	}

	// The source (§3): SELECT * FROM source_ref, its rows buffered and its cost charged.
	q, err := db.execQueryPlan(&plan.source, nil, bound, ctx)
	if err != nil {
		return outcome{}, err
	}
	meter.Charge(q.cost)
	srcRows := rowsFromValues(q.rows)
	mw := plan.m

	mask := mergeTouched(plan, n)

	env := &evalEnv{exec: db, params: bound, rng: newStmtRng(), ctes: ctx}
	pkChanged := make([]bool, len(plan.clauses))
	for ci := range plan.clauses {
		pkChanged[ci] = slices.ContainsFunc(plan.clauses[ci].assigns, func(a assignPlan) bool {
			return slices.Contains(pkMembers, a.idx)
		})
	}
	var actions []mergeAction
	affected := map[string]struct{}{}
	matched := map[string]struct{}{}

	// actOnTarget plans the action a target row's chosen clause takes (§4): the 21000 check, then an
	// UPDATE's new row (assignments, NOT NULL over the whole row, carried large values, CHECK, the
	// re-key) or a DELETE. stored is the row as the scan returned it, before any resolution.
	actOnTarget := func(ci int, key []byte, row, stored storedRow, src int, evalRow storedRow) error {
		c := &plan.clauses[ci]
		if c.action == mergeDoNothing {
			return nil
		}
		if _, again := affected[string(key)]; again {
			return newError(CardinalityViolation, "MERGE command cannot affect row a second time")
		}
		switch c.action {
		case mergeDelete:
			affected[string(key)] = struct{}{}
			actions = append(actions, mergeAction{kind: mergeDelete, key: key, oldRow: row, src: src, clause: ci})
		case mergeUpdate:
			newRow := make(storedRow, len(row))
			copy(newRow, row)
			nullCol := ""
			assigned := make([]bool, n)
			for _, a := range c.assigns {
				assigned[a.idx] = true
				raw, err := a.source.eval(evalRow, env, meter)
				if err != nil {
					return err
				}
				if a.notNull && raw.Kind == ValNull {
					if nullCol == "" {
						nullCol = a.name
					}
					newRow[a.idx] = NullValue()
					continue
				}
				checked, err := a.check(raw, db.inputZone())
				if err != nil {
					return stampTable(err, table.Name)
				}
				newRow[a.idx] = checked
			}
			if nullCol != "" {
				return db.notNullRowViolation(table.Name, nullCol, newRow)
			}
			// An unassigned non-key large value is carried in its stored form (large-values.md §15);
			// every other deferred value is resolved, unmetered.
			for i := range newRow {
				carried := !assigned[i] && !slices.Contains(pkMembers, i)
				if sv := stored[i]; carried && sv.Kind == ValUnfetched && sv.unfetched().Form != 0x00 {
					newRow[i] = sv
				} else if newRow[i].Kind == ValUnfetched {
					v, err := store.resolveValue(i, newRow[i].unfetched())
					if err != nil {
						return err
					}
					newRow[i] = v
				}
			}
			if err := evalChecks(checks, table.Name, newRow, env, meter); err != nil {
				return err
			}
			newKey := key
			if pkChanged[ci] {
				var err error
				if newKey, err = encodePkKey(colTypes, pkMembers, colls, newRow); err != nil {
					return err
				}
			}
			newRow, err := store.settleCarried(newKey, newRow)
			if err != nil {
				return err
			}
			affected[string(key)] = struct{}{}
			actions = append(actions, mergeAction{kind: mergeUpdate, key: key, newKey: newKey, newRow: newRow, oldRow: row, src: src, clause: ci})
		default:
			panic("a target row takes UPDATE, DELETE, or DO NOTHING")
		}
		return nil
	}
	// choose returns the first clause of kind with no condition, or whose condition is TRUE (§4).
	choose := func(kind mergeMatch, evalRow storedRow) (int, bool, error) {
		for ci := range plan.clauses {
			c := &plan.clauses[ci]
			if c.kind != kind {
				continue
			}
			if c.condition == nil {
				return ci, true, nil
			}
			ok, err := c.condition.evalFilter(evalRow, env, meter)
			if err != nil {
				return 0, false, err
			}
			if ok {
				return ci, true, nil
			}
		}
		return 0, false, nil
	}

	// The matching pass (§3): per source row, the probe, the join condition, the matched pairs, or the
	// not-matched action.
	scanPlan := db.planMutationScan(m.DB, table, plan.probe, contradiction)
	hasBySource := plan.hasBySource()
	// EXPLAIN ANALYZE attribution (explain.md §3): the probe scans, the matching loop (the source, the
	// probes, and the join condition), and the not-matched-by-source scan.
	var probeActual int64
	loopActual := q.cost
	// The hash form of the matching loop (§3): one full scan of the target, its rows' key columns
	// hashed into a table each source row probes; the candidates keep scan order.
	var hashed *hashJoinTable
	var targets []mergeCandidate
	if plan.hash != nil {
		before := meter.Accrued
		batch, err := db.executeMutationScan(scanPlan, m.Table, bound, env, meter, mask)
		if err != nil {
			return outcome{}, err
		}
		meter.Charge(costs.PageRead*int64(batch.pages) + costs.ValueDecompress*int64(batch.slabs))
		targets = make([]mergeCandidate, 0, len(batch.entries))
		for _, e := range batch.entries {
			if err := meter.Step(); err != nil {
				return outcome{}, err
			}
			meter.Charge(costs.StorageRowRead)
			row, err := store.resolveColumns(e.Row, mask)
			if err != nil {
				return outcome{}, err
			}
			targets = append(targets, mergeCandidate{key: e.Key, row: row, stored: e.Row})
		}
		probeActual += meter.Accrued - before
		rows := make([]storedRow, len(targets))
		for i := range targets {
			rows[i] = targets[i].row
		}
		if hashed, err = newHashJoinTable(plan.hash, 0, n, rows, meter); err != nil {
			return outcome{}, err
		}
		defer hashed.release()
		loopActual += meter.Accrued - before
	}
	for si, src := range srcRows {
		if err := meter.Step(); err != nil {
			return outcome{}, err
		}
		// This source row's candidate target rows: the hash table's exact-key matches, already read;
		// or the probe scan's entries, read one by one below.
		var entries []mergeCandidate
		prefetched := hashed != nil
		if prefetched {
			before := meter.Accrued
			hits, err := hashed.probePositions(plan.hash, src, meter)
			if err != nil {
				return outcome{}, err
			}
			loopActual += meter.Accrued - before
			entries = make([]mergeCandidate, len(hits))
			for i, h := range hits {
				entries[i] = targets[h]
			}
		} else {
			probeBefore := meter.Accrued
			probeEnv := &evalEnv{exec: db, params: bound, outer: []storedRow{src}, rng: env.rng, ctes: ctx}
			batch, err := db.executeMutationScan(scanPlan, m.Table, bound, probeEnv, meter, mask)
			if err != nil {
				return outcome{}, err
			}
			meter.Charge(costs.PageRead*int64(batch.pages) + costs.ValueDecompress*int64(batch.slabs))
			probeActual += meter.Accrued - probeBefore
			loopActual += meter.Accrued - probeBefore
			entries = make([]mergeCandidate, len(batch.entries))
			for i, e := range batch.entries {
				entries[i] = mergeCandidate{key: e.Key, row: e.Row, stored: e.Row}
			}
		}
		anyMatch := false
		for _, e := range entries {
			row := e.row
			if !prefetched {
				if err := meter.Step(); err != nil {
					return outcome{}, err
				}
				meter.Charge(costs.StorageRowRead)
				probeActual += costs.StorageRowRead
				loopActual += costs.StorageRowRead
				if row, err = store.resolveColumns(e.stored, mask); err != nil {
					return outcome{}, err
				}
			}
			combined := make(storedRow, 0, len(row)+len(src))
			combined = append(append(combined, row...), src...)
			onBefore := meter.Accrued
			ok, err := plan.on.evalFilter(combined, env, meter)
			loopActual += meter.Accrued - onBefore
			if err != nil {
				return outcome{}, err
			}
			if !ok {
				continue
			}
			anyMatch = true
			if hasBySource {
				matched[string(e.key)] = struct{}{}
			}
			ci, chosen, err := choose(mergeMatched, combined)
			if err != nil {
				return outcome{}, err
			}
			if !chosen {
				continue
			}
			if row, err = store.resolveInlineColumns(row); err != nil {
				return outcome{}, err
			}
			if err := actOnTarget(ci, e.key, row, e.stored, si, combined); err != nil {
				return outcome{}, err
			}
		}
		if anyMatch {
			continue
		}
		ci, chosen, err := choose(mergeNotMatchedByTarget, src)
		if err != nil {
			return outcome{}, err
		}
		if !chosen || plan.clauses[ci].action != mergeInsert {
			continue // no clause, or DO NOTHING
		}
		c := &plan.clauses[ci]
		vals := make([]Value, len(c.values))
		for p, node := range c.values {
			if node == nil {
				vals[p] = NullValue()
				continue
			}
			if err := meter.Checkpoint(); err != nil {
				return outcome{}, err
			}
			if vals[p], err = node.eval(src, env, meter); err != nil {
				return outcome{}, err
			}
		}
		row, err := db.buildInsertRow(c.rowTable, colTypes, c.provided, vals, c.defaults, env.rng, meter)
		if err != nil {
			return outcome{}, err
		}
		if err := evalChecks(checks, table.Name, row, env, meter); err != nil {
			return outcome{}, err
		}
		actions = append(actions, mergeAction{kind: mergeInsert, newRow: row, src: si, clause: ci})
	}
	loopNode := "Nested Loop"
	if plan.hash != nil {
		loopNode = "Hash Join"
	}
	db.explainActual.record(loopNode, loopActual)
	db.explainActual.record("Scan "+m.Table, probeActual)
	// Not matched by source (§3): one full scan in storage-key order.
	if hasBySource {
		scanBefore := meter.Accrued
		full := db.planMutationScan(m.DB, table, plan.targetFilter, whereContradicts(plan.targetFilter))
		batch, err := db.executeMutationScan(full, m.Table, bound, env, meter, mask)
		if err != nil {
			return outcome{}, err
		}
		meter.Charge(costs.PageRead*int64(batch.pages) + costs.ValueDecompress*int64(batch.slabs))
		scanActual := meter.Accrued - scanBefore
		for _, e := range batch.entries {
			if err := meter.Step(); err != nil {
				return outcome{}, err
			}
			meter.Charge(costs.StorageRowRead)
			scanActual += costs.StorageRowRead
			if _, ok := matched[string(e.Key)]; ok {
				continue
			}
			row, err := store.resolveColumns(e.Row, mask)
			if err != nil {
				return outcome{}, err
			}
			// A view target's row outside its WHERE chain is not a target row (views.md §15.2).
			if plan.targetFilter != nil {
				before := meter.Accrued
				shown, err := plan.targetFilter.evalFilter(row, env, meter)
				scanActual += meter.Accrued - before
				if err != nil {
					return outcome{}, err
				}
				if !shown {
					continue
				}
			}
			ci, chosen, err := choose(mergeNotMatchedBySource, row)
			if err != nil {
				return outcome{}, err
			}
			if !chosen {
				continue
			}
			if row, err = store.resolveInlineColumns(row); err != nil {
				return outcome{}, err
			}
			if err := actOnTarget(ci, e.Key, row, e.Row, -1, row); err != nil {
				return outcome{}, err
			}
		}
		db.explainActual.record("Scan "+m.Table, scanActual)
	}

	// End-state validation (§5). The vacated keys: every updated row's old key and every deleted
	// row's key. The placed rows: every inserted row and every updated row's new version, in
	// action-list order.
	vacated := make(map[string]struct{})
	for _, a := range actions {
		if a.kind != mergeInsert {
			vacated[string(a.key)] = struct{}{}
		}
	}
	// Each insert's primary key, computed once (a table without one allocates a rowid in phase 2).
	insertKeys := make([][]byte, len(actions))
	if len(pkMembers) > 0 {
		for ai, a := range actions {
			if a.kind == mergeInsert {
				if insertKeys[ai], err = encodePkKey(colTypes, pkMembers, colls, a.newRow); err != nil {
					return outcome{}, err
				}
			}
		}
		seen := make(map[string]struct{})
		for ai, a := range actions {
			var key []byte
			switch a.kind {
			case mergeInsert:
				key = insertKeys[ai]
			case mergeUpdate:
				key = a.newKey
			default:
				continue
			}
			_, exists, err := store.Get(key)
			if err != nil {
				return outcome{}, err
			}
			_, isVacated := vacated[string(key)]
			_, dup := seen[string(key)]
			if (exists && !isVacated) || dup {
				return outcome{}, newUniqueViolation(table.Name, pkeyName(table.Name)).
					withDetail(db.pkDetail(table.Name, table.Columns, pkMembers, a.newRow, detailExists))
			}
			seen[string(key)] = struct{}{}
		}
	}
	var placed []storedRow
	for _, a := range actions {
		if a.kind != mergeDelete {
			placed = append(placed, a.newRow)
		}
	}
	for k := range rindexes {
		if len(placed) == 0 {
			break
		}
		rindex := &rindexes[k]
		if !rindex.Unique {
			continue
		}
		istore := db.lkpIndexStoreScoped(m.DB, rindex.Name)
		batch := make(map[string]struct{})
		for _, row := range placed {
			prefix, ok, err := db.indexPrefix(colTypes, colls, rindex, row)
			if err != nil {
				return outcome{}, err
			}
			if !ok {
				continue
			}
			_, conflict := batch[string(prefix)]
			batch[string(prefix)] = struct{}{}
			if !conflict {
				entries, err := istore.RangeEntries(uniqueProbeBound(prefix))
				if err != nil {
					return outcome{}, err
				}
				for _, e := range entries {
					if _, own := vacated[string(e.Key[len(prefix):])]; !own {
						conflict = true
						break
					}
				}
			}
			if conflict {
				return outcome{}, newUniqueViolation(table.Name, rindex.Name).
					withDetail(db.indexDetailByName(table, rindex, row, detailExists))
			}
		}
	}
	// The check option of a MERGE into a view (views.md §15.6): each placed row, after the end-state
	// keys and before EXCLUDE and FOREIGN KEY.
	for _, row := range placed {
		if err := evalViewChecks(plan.viewChecks, row, env, meter); err != nil {
			return outcome{}, err
		}
	}
	if err := db.validateExclusions(m.DB, table, colTypes, placed, vacated); err != nil {
		return outcome{}, err
	}
	// FOREIGN KEY child side: every inserted row, and every updated row whose clause assigned one of
	// the key's columns; a self-reference may be satisfied by a placed row.
	fkTemp, hasFKs := db.fkDomain(m.DB, m.Table)
	for fki := range table.ForeignKeys {
		fk := &table.ForeignKeys[fki]
		parent, ok := db.fkSnap(fkTemp).table(fk.RefTable)
		if !ok {
			continue
		}
		parentColls := db.columnCollations(parent.Columns)
		batch := make(map[string]struct{})
		if fk.RefTable == table.Name {
			for _, row := range placed {
				probe, ok, err := buildFkProbe(fk, parent, parentColls, row, fk.RefColumns)
				if err != nil {
					return outcome{}, err
				}
				if ok {
					batch[string(probe.bytes)] = struct{}{}
				}
			}
		}
		for _, a := range actions {
			switch a.kind {
			case mergeDelete:
				continue
			case mergeUpdate:
				if !slices.ContainsFunc(plan.clauses[a.clause].assigns, func(p assignPlan) bool {
					return slices.Contains(fk.Columns, p.idx)
				}) {
					continue
				}
			}
			probe, ok, err := buildFkProbe(fk, parent, parentColls, a.newRow, fk.Columns)
			if err != nil {
				return outcome{}, err
			}
			if !ok {
				if fkMatchFullViolated(fk, a.newRow) {
					if err := db.fkChildCheck(table.Name, table.Columns, fk, fkProbe{}, false, a.newRow, fkTemp); err != nil {
						return outcome{}, err
					}
				}
				continue
			}
			if _, inBatch := batch[string(probe.bytes)]; inBatch {
				continue
			}
			if err := db.fkChildCheck(table.Name, table.Columns, fk, probe, true, a.newRow, fkTemp); err != nil {
				return outcome{}, err
			}
		}
	}

	// The compression attempts of every placed row's disposition plan, then the ceiling — the last
	// abort point before a write (§5). A rowid table's 8-byte placeholder stands in for the key a
	// phase-2 insert allocates (only the key length feeds the plan).
	var cunits int64
	for ai, a := range actions {
		switch a.kind {
		case mergeInsert:
			key := insertKeys[ai]
			if key == nil {
				key = make([]byte, 8)
			}
			cunits += int64(store.WriteCompressUnits(key, a.newRow))
		case mergeUpdate:
			cunits += int64(store.WriteCompressUnits(a.newKey, a.newRow))
		}
	}
	meter.Charge(costs.ValueCompress * cunits)
	if err := meter.Checkpoint(); err != nil {
		return outcome{}, err
	}

	// RETURNING (§7): one row per action over [source | target | old | new | action].
	var returned [][]Value
	if plan.returning != nil {
		renv := &evalEnv{exec: db, params: bound, rng: newStmtRng(), ctes: ctx}
		nullTarget := make(storedRow, n)
		nullSrc := make(storedRow, mw)
		for i := range nullTarget {
			nullTarget[i] = NullValue()
		}
		for i := range nullSrc {
			nullSrc[i] = NullValue()
		}
		returned = make([][]Value, 0, len(actions))
		for _, a := range actions {
			if err := meter.Step(); err != nil {
				return outcome{}, err
			}
			meter.Charge(costs.RowProduced)
			src := nullSrc
			if a.src >= 0 {
				src = srcRows[a.src]
			}
			var target, old, nw storedRow
			var tag string
			switch a.kind {
			case mergeInsert:
				target, old, nw, tag = a.newRow, nullTarget, a.newRow, "INSERT"
			case mergeUpdate:
				target, old, nw, tag = a.newRow, a.oldRow, a.newRow, "UPDATE"
			default:
				target, old, nw, tag = a.oldRow, a.oldRow, nullTarget, "DELETE"
			}
			row := make(storedRow, 0, mw+3*n+1)
			row = append(row, src...)
			row = append(row, target...)
			row = append(row, old...)
			row = append(row, nw...)
			row = append(row, TextValue(tag))
			vals := make([]Value, 0, len(plan.returning.nodes))
			for _, node := range plan.returning.nodes {
				v, err := node.eval(row, renv, meter)
				if err != nil {
					return outcome{}, err
				}
				vals = append(vals, v)
			}
			if err := meter.admitRow(vals); err != nil {
				return outcome{}, err
			}
			returned = append(returned, vals)
		}
	}

	// Precompute every index entry change before the writes (§6), per action: a deleted row's
	// entries and an update's moved-away entries (removals), an update's moved-to entries, and an
	// insert's entry prefixes (its key is appended in phase 2, where a rowid table allocates it).
	removes := make([][]indexEntry, len(table.Indexes))
	// actionAdds[ai][k]: an update's added entries, or an insert's entry prefixes, for index k.
	actionAdds := make([][][][]byte, len(actions))
	for ai, a := range actions {
		perIndex := make([][][]byte, len(rindexes))
		for k := range rindexes {
			rindex := &rindexes[k]
			switch a.kind {
			case mergeDelete:
				eks, err := db.indexEntries(table.Columns, colTypes, colls, rindex, a.key, a.oldRow)
				if err != nil {
					return outcome{}, err
				}
				for _, ek := range eks {
					removes[k] = append(removes[k], indexEntry{ek, len(a.key)})
				}
			case mergeUpdate:
				oldEks, err := db.indexEntries(table.Columns, colTypes, colls, rindex, a.key, a.oldRow)
				if err != nil {
					return outcome{}, err
				}
				newEks, err := db.indexEntries(table.Columns, colTypes, colls, rindex, a.newKey, a.newRow)
				if err != nil {
					return outcome{}, err
				}
				for _, ek := range bytesDiff(oldEks, newEks) {
					removes[k] = append(removes[k], indexEntry{ek, len(a.key)})
				}
				perIndex[k] = bytesDiff(newEks, oldEks)
			case mergeInsert:
				prefixes, err := db.indexEntries(table.Columns, colTypes, colls, rindex, nil, a.newRow)
				if err != nil {
					return outcome{}, err
				}
				perIndex[k] = prefixes
			}
		}
		actionAdds[ai] = perIndex
	}

	// Phase 2 (§6). 1: vacate — each deleted row and each re-keyed row's old key. 2: place — each
	// insert (its key, or an allocated rowid) and each updated row, in action-list order. 3: per
	// index, every removal, then every addition.
	writeStore := db.writeStoreScoped(m.DB, m.Table)
	for _, a := range actions {
		if a.kind == mergeDelete || (a.kind == mergeUpdate && string(a.key) != string(a.newKey)) {
			if _, err := writeStore.Remove(a.key); err != nil {
				return outcome{}, err
			}
		}
	}
	adds := make([][]indexEntry, len(table.Indexes))
	var fkDeletes []storedRow
	var fkUpdates []fkUpdateTransition
	for ai, a := range actions {
		switch a.kind {
		case mergeInsert:
			key := insertKeys[ai]
			if key == nil {
				key = encodeInt(scalarInt64, writeStore.AllocRowid())
			}
			for k, prefixes := range actionAdds[ai] {
				for _, p := range prefixes {
					ek := append(append([]byte{}, p...), key...)
					adds[k] = append(adds[k], indexEntry{ek, len(key)})
				}
			}
			inserted, err := writeStore.Insert(key, a.newRow)
			if err != nil {
				return outcome{}, err
			}
			if !inserted {
				// Reachable only under the writable-CTE read pin (writable-cte.md §7).
				return outcome{}, newUniqueViolation(table.Name, pkeyName(table.Name))
			}
		case mergeUpdate:
			for k, eks := range actionAdds[ai] {
				for _, ek := range eks {
					adds[k] = append(adds[k], indexEntry{ek, len(a.newKey)})
				}
			}
			if string(a.key) != string(a.newKey) {
				inserted, err := writeStore.Insert(a.newKey, a.newRow)
				if err != nil {
					return outcome{}, err
				}
				if !inserted {
					return outcome{}, newUniqueViolation(table.Name, pkeyName(table.Name))
				}
			} else if err := writeStore.Replace(a.key, a.newRow); err != nil {
				return outcome{}, err
			}
			fkUpdates = append(fkUpdates, fkUpdateTransition{newRow: a.newRow, oldRow: a.oldRow})
		case mergeDelete:
			fkDeletes = append(fkDeletes, a.oldRow)
		}
	}
	for k, def := range table.Indexes {
		istore := db.writeIndexStoreScoped(m.DB, def.Name)
		for _, ek := range removes[k] {
			if _, err := indexEntryRemove(istore, def.Kind, ek.key, ek.skeyLen); err != nil {
				return outcome{}, err
			}
		}
		for _, ek := range adds[k] {
			inserted, err := indexEntryInsert(istore, def.Kind, ek.key, ek.skeyLen)
			if err != nil {
				return outcome{}, err
			}
			if !inserted {
				if def.Unique {
					return outcome{}, newUniqueViolation(table.Name, def.Name)
				}
				return outcome{}, newError(DataCorrupted, "index entry keys are unique (storage-key suffix)")
			}
		}
	}
	if err := db.syncPendingWrites(meter); err != nil {
		return outcome{}, err
	}
	if len(actions) > 0 {
		db.markEstimatorMutation(m.DB, m.Table)
	}
	if err := db.applyFkDeleteActions(table, fkDeletes, meter, fkTemp, hasFKs); err != nil {
		return outcome{}, err
	}
	if err := db.applyFkUpdateActions(table, fkUpdates, meter, fkTemp, hasFKs); err != nil {
		return outcome{}, err
	}
	if plan.returning != nil {
		if returned == nil {
			returned = [][]Value{}
		}
		return outcome{Kind: outcomeQuery, ColumnNames: plan.returning.names, ColumnTypes: typeNames(plan.returning.types), Rows: returned, Cost: meter.Accrued}, nil
	}
	return outcome{Kind: outcomeStatement, Cost: meter.Accrued, RowsAffected: int64(len(actions)), HasRowsAffected: true}, nil
}

// mergeCandidate is one target row the matching loop considers for a source row: its storage key, the
// row with the touched set resolved (the probe form resolves it as the row is read), and the row as
// the scan returned it.
type mergeCandidate struct {
	key    []byte
	row    storedRow
	stored storedRow
}

// mergeTouched is the touched set of a target row (merge.md §9): every target column the join
// condition, a MATCHED or NOT MATCHED BY SOURCE condition or SET right-hand side, or RETURNING
// (target, old., or new.) reads.
func mergeTouched(plan *mergePlan, n int) []bool {
	mask := make([]bool, n)
	collectTouched(plan.on, 0, mask)
	for ci := range plan.clauses {
		c := &plan.clauses[ci]
		if c.kind == mergeNotMatchedByTarget {
			continue
		}
		collectTouched(c.condition, 0, mask)
		for i := range c.assigns {
			collectTouched(c.assigns[i].source, 0, mask)
		}
	}
	if plan.returning != nil {
		mw := plan.m
		retMask := make([]bool, mw+3*n+1)
		for _, node := range plan.returning.nodes {
			collectTouched(node, 0, retMask)
		}
		for i := range mask {
			mask[i] = mask[i] || retMask[mw+i] || retMask[mw+n+i] || retMask[mw+2*n+i]
		}
	}
	return mask
}

// mergeInsertColumns maps an INSERT action's value slots to target columns (merge.md §2):
// provided[i] is the slot table column i takes (-1 — its default: an omitted column, a DEFAULT slot,
// or an identity column under OVERRIDING USER VALUE), and slotCol[p] the column slot p fills. Without
// a column list the slots fill the first columns in order. Errors: an unknown column 42703, a repeated
// one 42701, more values than columns (or fewer than a list names) 42601, an explicit value for a
// GENERATED ALWAYS identity column without OVERRIDING SYSTEM VALUE 428C9.
func mergeInsertColumns(table *catTable, columns []string, columnPos []srcPos, overriding *overridingKind, values []mergeValue) (provided []int, slotCol []int, err error) {
	n := len(table.Columns)
	var targets []int
	if columns != nil {
		targets = make([]int, 0, len(columns))
		for p, name := range columns {
			idx := table.ColumnIndex(name)
			if idx < 0 {
				e := newError(UndefinedColumn, fmt.Sprintf("column %s of relation %s does not exist", name, table.Name))
				if p < len(columnPos) {
					e = e.withPosition(int32(columnPos[p]))
				}
				return nil, nil, e
			}
			if slices.Contains(targets, idx) {
				return nil, nil, newError(DuplicateColumn, fmt.Sprintf("column %s specified more than once", table.Columns[idx].Name))
			}
			targets = append(targets, idx)
		}
	} else {
		targets = make([]int, n)
		for i := range targets {
			targets[i] = i
		}
	}
	if len(values) > len(targets) {
		return nil, nil, newError(SyntaxError, "INSERT has more expressions than target columns")
	}
	if columns != nil && len(values) < len(targets) {
		return nil, nil, newError(SyntaxError, "INSERT has more target columns than expressions")
	}
	provided = make([]int, n)
	for i := range provided {
		provided[i] = -1
	}
	slotCol = make([]int, len(values))
	for p, v := range values {
		i := targets[p]
		slotCol[p] = i
		if v.IsDefault {
			continue
		}
		col := table.Columns[i]
		if col.Identity != nil && overriding != nil && *overriding == overridingUser {
			continue
		}
		if col.Identity != nil && *col.Identity == identityAlways && (overriding == nil || *overriding != overridingSystem) {
			return nil, nil, newGeneratedAlwaysInsert(col.Name)
		}
		provided[i] = p
	}
	return provided, slotCol, nil
}
