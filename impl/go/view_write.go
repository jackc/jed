package jed

// Writing through an automatically updatable view (spec/design/views.md §15). Mirrors impl/rust
// executor/view_write.rs and impl/ts view_write.ts (whose Engine methods live in executor.ts).
//
// A write through view `v` is the equivalent statement on the table at the bottom of v's view
// chain. prepareViewWrite validates the target (§15.3) and builds one writeLayer per level: the
// columns visible at that level and what each one is — a reference to a column of the level below,
// or a read-only select item. The DML executors then run against the base table with a scope whose
// target relation carries the top layer, so a name binds to the view's column (a layered
// resolution). An updatable column resolves to the base column itself; a read-only one to its
// select item, resolved in the view body's isolated scope at the depth of the reference
// (resolveLayerRef). The views' WHERE clauses resolve the same way and are AND-ed ahead of the
// statement's own (resolveWriteFilter); the checked views' WHERE clauses become the statement's
// check-option checks (resolveViewChecks, §15.6).

import "fmt"

// writeKind is the statement kind a write through a view performs.
type writeKind int

const (
	writeInsert writeKind = iota
	writeUpdate
	writeDelete
)

func (k writeKind) verb() string {
	switch k {
	case writeInsert:
		return "insert into"
	case writeUpdate:
		return "update"
	}
	return "delete from"
}

// layerCol is what one visible column of a writeLayer is: a reference to column ref of the level
// below (the table's column when the layer is the table), or, when expr is non-nil, a read-only
// select item written against the level below under belowLabel.
type layerCol struct {
	ref  int
	expr *exprNode
}

// writeLayer is one level of a write through a view chain (views.md §15.2). The bottom layer is the
// table: its columns are the table's (renamed by the referencing body's column alias list), each a
// reference to itself. A view layer's columns are the view's output columns.
type writeLayer struct {
	// view is the view this layer is; "" for the table.
	view string
	// columns are the visible columns: names as visible at this level, with types and collations.
	columns []catColumn
	cols    []layerCol
	// below is the level the view's select items and WHERE are written against; nil for the table.
	below *writeLayer
	// belowLabel is the label of the view body's FROM item (its alias, else the relation's name).
	belowLabel string
	// filter is the view's WHERE, written against below.
	filter *exprNode
	// check is the view's own check option (views.md §15.6); checkOptionNone for the table.
	check checkOption
}

// follow returns where layer column col leads: a base table column (owner nil), or the read-only
// select item that defines it with the layer that owns it.
func (l *writeLayer) follow(col int) (base int, owner *writeLayer, expr *exprNode) {
	c := l.cols[col]
	if c.expr != nil {
		return -1, l, c.expr
	}
	if l.below == nil {
		return c.ref, nil, nil
	}
	return l.below.follow(c.ref)
}

// viewsBottomUp returns the view layers from the one nearest the table up to this one.
func (l *writeLayer) viewsBottomUp() []*writeLayer {
	var out []*writeLayer
	for layer := l; layer != nil; layer = layer.below {
		if layer.view != "" {
			out = append([]*writeLayer{layer}, out...)
		}
	}
	return out
}

// viewWrite is a validated write through a view: the target view, the base table, the top layer,
// and the base column each statement target maps to (in the statement's target order).
type viewWrite struct {
	view    string
	table   string
	top     *writeLayer
	targets []string
}

// chainLevel is one level of the chain while it is validated top-down.
type chainLevel struct {
	view   *viewDef
	rename []string
	label  string
	cols   []layerCol
	filter *exprNode
}

// viewCheck is one check-option check of a write through a view (views.md §15.6): the checked view's
// name and its WHERE resolved over the base row.
type viewCheck struct {
	view string
	expr *rExpr
}

// checkOptionViolation is the 44000 a new row that fails a checked view's WHERE raises (views.md
// §15.6).
func checkOptionViolation(view string) error {
	return newError(WithCheckOptionViolation, fmt.Sprintf("new row violates check option for view \"%s\"", view))
}

// evalViewChecks evaluates the check-option checks (views.md §15.6) on one new row, nearest the table
// first: each WHERE is a filter, so FALSE or NULL fails with 44000 naming the view. Metered
// expression work.
func evalViewChecks(checks []viewCheck, row storedRow, env *evalEnv, m *costMeter) error {
	if len(checks) == 0 {
		return nil
	}
	if err := m.Guard(); err != nil {
		return err
	}
	for _, c := range checks {
		keep, err := c.expr.evalFilter(row, env, m)
		if err != nil {
			return err
		}
		if !keep {
			return checkOptionViolation(c.view)
		}
	}
	return nil
}

// foldViewChecks folds the uncorrelated subqueries of the check-option checks once per statement
// (cost.md §3), reading the pre-statement snapshot.
func (db *engine) foldViewChecks(checks []viewCheck, bound []Value, ctx cteCtx, meter *costMeter) error {
	for _, c := range checks {
		if err := db.foldUncorrelatedInRExpr(c.expr, bound, ctx, &meter.Accrued); err != nil {
			return err
		}
	}
	return nil
}

func notUpdatable(kind writeKind, view string) error {
	return newError(ObjectNotInPrerequisiteState, fmt.Sprintf("cannot %s view \"%s\"", kind.verb(), view))
}

// columnRefPosition is the position of the FROM item's column a select item names, when the item
// is a plain column reference (bare, or qualified by the FROM item's label) — an updatable view
// column (§15.1) — else -1.
func columnRefPosition(e *exprNode, label string, below []string) int {
	var name string
	switch {
	case e.Kind == exprColumn:
		name = e.Column
	case e.Kind == exprQualifiedColumn && e.Qualifier == label:
		name = e.Column
	default:
		return -1
	}
	for i, n := range below {
		if n == name {
			return i
		}
	}
	return -1
}

// renamedNames applies a column alias list over a relation's column names (a partial rename,
// grammar.md §15).
func renamedNames(names []string, list []string) []string {
	out := append([]string(nil), names...)
	for i := 0; i < len(list) && i < len(out); i++ {
		out[i] = list[i]
	}
	return out
}

// dmlTargetView returns the view a DML target names, or nil: tables and views share one namespace,
// and only bare and main. names reach a view. A view behind an attachment qualifier is 0A000
// (views.md §10).
func (db *engine) dmlTargetView(q *string, name string) (*viewDef, error) {
	if err := db.rejectAttachedView(q, name); err != nil {
		return nil, err
	}
	snap := db.readSnap()
	switch {
	case q == nil:
		if _, ok := db.lkpTable(name); ok {
			return nil, nil
		}
	case *q != "main":
		return nil, nil
	}
	if _, ok := snap.table(name); ok {
		return nil, nil
	}
	return snap.view(name), nil
}

// updatableShape returns the single FROM item of an automatically updatable body (views.md §15.1),
// or nil. For INSERT / UPDATE (checkCols) the body must also output at least one plain column
// reference. A stored body has no `*`; at CREATE VIEW a `*` or `label.*` item counts as plain column
// references, which it expands to (§1.1 step 4).
func (db *engine) updatableShape(body *queryExpr, checkCols bool) (*selectStmt, *tableRef) {
	sel := body.Select
	if sel == nil {
		return nil, nil
	}
	if sel.Distinct || len(sel.GroupBy) > 0 || sel.Having != nil || sel.Limit != nil || sel.Offset != nil ||
		len(sel.Joins) > 0 || sel.JoinGroup {
		return nil, nil
	}
	levelHas := func(e exprNode) bool { return exprHasAggregate(e) || exprHasWindow(e) }
	for _, it := range sel.Items.Items {
		if levelHas(it.Expr) {
			return nil, nil
		}
	}
	for _, k := range sel.OrderBy {
		if k.Expr != nil && levelHas(*k.Expr) {
			return nil, nil
		}
	}
	tref := sel.From
	if tref == nil || tref.IsFunc || tref.Subquery != nil || tref.Values != nil || tref.JsonTable != nil ||
		tref.Lateral || (tref.DB != nil && *tref.DB != "main") {
		return nil, nil
	}
	if _, ok := catalogRelKind(tref.Name); ok {
		return nil, nil
	}
	snap := db.readSnap()
	if _, ok := snap.table(tref.Name); !ok && snap.view(tref.Name) == nil {
		return nil, nil
	}
	if checkCols {
		label := tref.Name
		if tref.Alias != nil {
			label = *tref.Alias
		}
		plain := sel.Items.All
		for _, it := range sel.Items.Items {
			if it.Expr.Kind == exprColumn ||
				((it.Expr.Kind == exprQualifiedColumn || it.Expr.Kind == exprQualifiedStar) && it.Expr.Qualifier == label) {
				plain = true
				break
			}
		}
		if !plain {
			return nil, nil
		}
	}
	return sel, tref
}

// prepareViewWrite validates a write through name and builds its layer chain (views.md §15.3 steps
// 2–6), or returns nil when the target is not a view. targets are the statement's target view
// columns: the INSERT column list (nil = every view column) or the UPDATE SET columns, in order;
// DELETE has none. targetPos holds each target's character position, index-aligned (nil when
// unknown), for a 42703 (spec/design/error-fields.md §10.2).
func (db *engine) prepareViewWrite(q *string, name string, kind writeKind, onConflict bool, targets []string, targetPos []srcPos) (*viewWrite, error) {
	view, err := db.dmlTargetView(q, name)
	if err != nil || view == nil {
		return nil, err
	}
	checkCols := kind != writeDelete
	// 2. The target view is updatable for the statement's kind (55000).
	if sel, _ := db.updatableShape(view.Body, checkCols); sel == nil {
		return nil, notUpdatable(kind, view.Name)
	}
	// 3. ON CONFLICT on a view is a follow-on.
	if onConflict {
		return nil, newError(FeatureNotSupported, "ON CONFLICT on a view is not supported yet")
	}
	// 4. The view is planned as a read of it would be (§4): its body's errors and the nesting and
	// expansion limits. The plan yields every chain view's output types and collations.
	metas, err := db.planWriteTarget(view)
	if err != nil {
		return nil, err
	}
	// 5. The statement's targets against the view's output columns.
	var cur []int
	if checkCols {
		if targets == nil {
			for i := range view.Columns {
				cur = append(cur, i)
			}
		} else {
			for ti, t := range targets {
				idx := -1
				for i, c := range view.Columns {
					if c == t {
						idx = i
						break
					}
				}
				if idx < 0 {
					return nil, newError(UndefinedColumn, fmt.Sprintf("column %s of relation %s does not exist", t, view.Name)).
						withPosition(srcPosOf(targetPos, ti))
				}
				for _, c := range cur {
					if c == idx {
						if kind == writeInsert {
							return nil, newError(DuplicateColumn, fmt.Sprintf("column %s specified more than once", t))
						}
						return nil, newError(DuplicateColumn, fmt.Sprintf("column %s assigned more than once", t))
					}
				}
				cur = append(cur, idx)
			}
		}
	}
	// 6. Each view from the target down: updatable, then its target columns.
	var levels []chainLevel
	levelView := view
	var rename []string
	var table *catTable
	var tableRename []string
	for {
		sel, tref := db.updatableShape(levelView.Body, checkCols)
		if sel == nil {
			return nil, notUpdatable(kind, levelView.Name)
		}
		label := tref.Name
		if tref.Alias != nil {
			label = *tref.Alias
		}
		snap := db.readSnap()
		var belowNames []string
		base, isTable := snap.table(tref.Name)
		if isTable {
			for _, c := range base.Columns {
				belowNames = append(belowNames, c.Name)
			}
		} else {
			belowNames = snap.view(tref.Name).Columns
		}
		belowNames = renamedNames(belowNames, tref.ColumnAliases)
		cols := make([]layerCol, len(sel.Items.Items))
		for i := range sel.Items.Items {
			e := &sel.Items.Items[i].Expr
			if j := columnRefPosition(e, label, belowNames); j >= 0 {
				cols[i] = layerCol{ref: j}
			} else {
				cols[i] = layerCol{expr: e}
			}
		}
		if checkCols {
			first := -1
			for _, c := range cur {
				if cols[c].expr != nil && (first < 0 || c < first) {
					first = c
				}
			}
			if first >= 0 {
				verb := "update"
				if kind == writeInsert {
					verb = "insert into"
				}
				return nil, newError(FeatureNotSupported, fmt.Sprintf(
					"cannot %s column \"%s\" of view \"%s\"", verb, levelView.Columns[first], levelView.Name))
			}
			next := make([]int, len(cur))
			for i, c := range cur {
				next[i] = cols[c].ref
			}
			cur = next
		}
		levels = append(levels, chainLevel{view: levelView, rename: rename, label: label, cols: cols, filter: sel.Filter})
		if isTable {
			table, tableRename = base, tref.ColumnAliases
			break
		}
		levelView = snap.view(tref.Name)
		rename = tref.ColumnAliases
	}
	// 7. Two targets that reach the same table column (42701).
	for i, c := range cur {
		for _, d := range cur[:i] {
			if c == d {
				return nil, newError(DuplicateColumn, fmt.Sprintf("column %s assigned more than once", table.Columns[c].Name))
			}
		}
	}
	// Build the layers bottom-up: the table, then each view.
	tableCols := append([]catColumn(nil), table.Columns...)
	for i := 0; i < len(tableRename) && i < len(tableCols); i++ {
		tableCols[i].Name = tableRename[i]
	}
	layer := &writeLayer{columns: tableCols, cols: make([]layerCol, len(tableCols))}
	for i := range layer.cols {
		layer.cols[i] = layerCol{ref: i}
	}
	for i := len(levels) - 1; i >= 0; i-- {
		level := levels[i]
		meta := metas[level.view.Name]
		names := renamedNames(level.view.Columns, level.rename)
		synth, err := cteSyntheticTableCols(level.view.Name, names, meta.columnTypes, meta.columnDerivs, nil)
		if err != nil {
			return nil, err
		}
		layer = &writeLayer{
			view: level.view.Name, columns: synth.Columns, cols: level.cols, below: layer,
			belowLabel: level.label, filter: level.filter, check: level.view.CheckOption,
		}
	}
	vw := &viewWrite{view: layer.view, table: table.Name, top: layer}
	for _, c := range cur {
		vw.targets = append(vw.targets, table.Columns[c].Name)
	}
	return vw, nil
}

// planWriteTarget plans the target view as a read of it at the statement's top level would (views.md
// §4/§4.1), returning the output names, types, and collation derivations of every view in its
// expansion.
func (db *engine) planWriteTarget(view *viewDef) (map[string]*valuesPlan, error) {
	metas := map[string]*valuesPlan{}
	err := db.inViewBody(viewSite{depth: 0, bodyDepth: view.Depth}, func() error {
		plan, err := db.planQuery(*view.Body, nil, nil, &paramTypes{})
		if err != nil {
			return err
		}
		for k, v := range db.viewColumns {
			metas[k] = v
		}
		metas[view.Name] = &valuesPlan{
			columnTypes: plan.columnTypes(), columnNames: plan.columnNames(), columnDerivs: plan.columnDerivs(),
		}
		return nil
	})
	return metas, err
}

// resolveWriteFilter resolves the complete WHERE of a write (views.md §15.2): every view's WHERE, the
// one nearest the table first, then the statement's own, AND-ed left-deep — the WHERE of the
// equivalent base statement, a filter whose top-level AND chain stops at the first conjunct that is
// not TRUE (cost.md §3). Each view's WHERE resolves in its body's isolated scope over the base row.
func (db *engine) resolveWriteFilter(s *scope, table *catTable, vw *viewWrite, user *exprNode, ptypes *paramTypes) (*rExpr, error) {
	var out *rExpr
	join := func(node *rExpr) {
		if out == nil {
			out = node
		} else {
			out = &rExpr{kind: reAnd, lhs: out, rhs: node}
		}
	}
	if vw != nil {
		for _, layer := range vw.top.viewsBottomUp() {
			if layer.filter == nil {
				continue
			}
			ls := layerBodyScope(db, table, 0, layer.belowLabel, layer.below)
			node, err := resolveBooleanFilter(ls, layer.filter, &paramTypes{})
			if err != nil {
				return nil, err
			}
			join(node)
		}
	}
	if user != nil {
		node, err := resolveBooleanFilter(s, user, ptypes)
		if err != nil {
			return nil, err
		}
		join(node)
	}
	return out, nil
}

// resolveViewChecks returns the check-option checks of a write through a view (views.md §15.6): the
// WHERE of every checked view — one with its own check option, or below a CASCADED one — nearest the
// table first, each resolved in its body's isolated scope over the base row. Empty when the target is
// a table or no view in the chain is checked.
func (db *engine) resolveViewChecks(table *catTable, vw *viewWrite) ([]viewCheck, error) {
	if vw == nil {
		return nil, nil
	}
	var out []viewCheck
	cascaded := false
	for layer := vw.top; layer != nil; layer = layer.below {
		if layer.view == "" {
			continue
		}
		if (layer.check != checkOptionNone || cascaded) && layer.filter != nil {
			ls := layerBodyScope(db, table, 0, layer.belowLabel, layer.below)
			node, err := resolveBooleanFilter(ls, layer.filter, &paramTypes{})
			if err != nil {
				return nil, err
			}
			out = append([]viewCheck{{view: layer.view, expr: node}}, out...)
		}
		if layer.check == checkOptionCascaded {
			cascaded = true
		}
	}
	return out, nil
}

// writeTargetScope is the target scope of a write (UPDATE / DELETE): singleScope, or, for a write
// through a view, the base table relabeled with the view's name and exposing the view's columns
// (views.md §15.2).
func writeTargetScope(catalog *engine, t *catTable, vw *viewWrite) *scope {
	s := singleScope(catalog, t)
	if vw != nil {
		s.rels[0].label = vw.view
		s.rels[0].layer = vw.top
	}
	return s
}

// layerBodyScope is the isolated scope a view body's select items and WHERE resolve in during a
// write through the view (views.md §4.1, §15.2): the body's single FROM item, labeled label,
// exposing layer's columns over the base row at flat offset. No parent and no CTEs.
func layerBodyScope(catalog *engine, t *catTable, offset int, label string, layer *writeLayer) *scope {
	return &scope{
		rels:          []scopeRel{{label: label, table: t, offset: offset, layer: layer}},
		catalog:       catalog,
		allowSubquery: true,
	}
}

// resolveLayerRef resolves a reference to column col of the layered relation rel found level scopes
// out (views.md §15.2): an updatable column is the base column itself; a read-only one is its select
// item, resolved in the owning view body's isolated scope placed level scopes out, so its column
// references read the same base row.
func resolveLayerRef(s *scope, ag *aggCtx, r resolved, name string) (*rExpr, resolvedType, error) {
	anc := s
	for i := 0; i < r.level; i++ {
		anc = anc.parent
	}
	rel := &anc.rels[r.rel]
	base, owner, expr := rel.layer.follow(r.col)
	if owner == nil {
		idx := rel.offset + base
		if r.level == 0 {
			return collectColumn(s, ag, idx, name)
		}
		return &rExpr{kind: reOuterColumn, level: r.level, index: idx},
			resolvedTypeOfCol(anc.columnAt(idx).Type, s.catalog.readSnap()), nil
	}
	body := layerBodyScope(s.catalog, rel.table, rel.offset, owner.belowLabel, owner.below)
	at := body
	for i := 0; i < r.level; i++ {
		at = &scope{parent: at, catalog: s.catalog, allowSubquery: true}
	}
	return resolve(at, *expr, nil, &aggCtx{collecting: false}, &paramTypes{})
}

// rewriteInsertTarget retargets an INSERT through an updatable view at its base table (views.md
// §15.2): the column list becomes the base columns the view's targets map to. It returns a copy of
// the statement (the written one is left intact) and the write, or the statement unchanged and nil
// when the target is not a view.
func (db *engine) rewriteInsertTarget(ins *insert) (*insert, *viewWrite, error) {
	vw, err := db.prepareViewWrite(ins.DB, ins.Table, writeInsert, ins.OnConflict != nil, ins.Columns, ins.ColumnPos)
	if err != nil || vw == nil {
		return ins, nil, err
	}
	c := *ins
	c.Table, c.DB = vw.table, nil
	c.Columns = append(make([]string, 0, len(vw.targets)), vw.targets...)
	return &c, vw, nil
}

// rewriteUpdateTarget retargets an UPDATE through an updatable view at its base table (views.md
// §15.2): each assignment names its base column, and `SET c = DEFAULT` assigns the view column's
// default, which is NULL (jed has no view column defaults).
func (db *engine) rewriteUpdateTarget(upd *update) (*update, *viewWrite, error) {
	targets := make([]string, len(upd.Assignments))
	targetPos := make([]srcPos, len(upd.Assignments))
	for i, a := range upd.Assignments {
		targets[i] = a.Column
		targetPos[i] = a.Pos
	}
	vw, err := db.prepareViewWrite(upd.DB, upd.Table, writeUpdate, false, targets, targetPos)
	if err != nil || vw == nil {
		return upd, nil, err
	}
	c := *upd
	c.Table, c.DB = vw.table, nil
	c.Assignments = make([]assignment, len(upd.Assignments))
	for i, a := range upd.Assignments {
		a.Column = vw.targets[i]
		if a.IsDefault {
			a.IsDefault = false
			a.Value = exprNode{Kind: exprLiteral, Literal: &literal{Kind: literalNull}}
		}
		c.Assignments[i] = a
	}
	return &c, vw, nil
}

// rewriteDeleteTarget retargets a DELETE through an updatable view at its base table (views.md
// §15.2).
func (db *engine) rewriteDeleteTarget(del *deleteStmt) (*deleteStmt, *viewWrite, error) {
	vw, err := db.prepareViewWrite(del.DB, del.Table, writeDelete, false, nil, nil)
	if err != nil || vw == nil {
		return del, nil, err
	}
	c := *del
	c.Table, c.DB = vw.table, nil
	return &c, vw, nil
}
