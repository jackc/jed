package jed

import (
	"fmt"
	"slices"
	"sort"
	"strings"
)

// Views (spec/design/views.md): CREATE VIEW / DROP VIEW, reading a view as the equivalent derived
// table (§4), the view-body planning trace that yields the dependency list and the `*` freezing
// (§5/§6), the RENAME / DROP COLUMN text rewrites (§7), and the re-validation of dependent views after
// ALTER TABLE (§8). Mirrors impl/rust views.rs and impl/ts views.ts.
//
// A view is stored as canonical token text and planned again at every use. The trace is how the
// engine learns what that text binds to: while a view body is planned at CREATE VIEW, at
// re-validation, or before a rewrite, the planner records into engine.viewTrace
//   - each FROM item that binds a catalog table or view, keyed by the position of its table-name
//     token;
//   - each base-table column a name in the body binds to (bare, qualified, correlated,
//     USING/NATURAL on both sides, and `*` expansions);
//   - each written `*` / `rel.*` select item with the columns it expanded to.
//
// Positions are token indices into lex(definition), so every edit is a token edit on the stored text
// followed by the canonical re-render (format.md *Check-expression text*). A nested view reference
// records only the view itself: its own body is planned with the trace suspended.

// viewTrace is what one traced planning pass of a view body binds (views.md §5/§6/§7).
type viewTrace struct {
	// items are the FROM items that bind a catalog table or view, keyed by the table-name token
	// position. Keyed (not appended) because a pushed-down derived body is planned twice.
	items map[tokPos]traceItem
	// catalogRels are the built-in jed_ catalog relations the body names (privilege-checked at
	// CREATE, §9).
	catalogRels map[string]bool
	// columns is every referenced base-table column.
	columns map[traceColumn]bool
	// viewColumns is every referenced column of a view, as (view name, column ordinal). Not a
	// persisted dependency (§6): re-validation after CREATE OR REPLACE VIEW reads it (§8).
	viewColumns map[traceColumn]bool
	// stars is each written star's expansion, keyed by its token position.
	stars map[tokPos]traceStar
	// sequences are the sequences the body names in a constant nextval / currval / setval argument
	// (§6).
	sequences map[string]bool
	// types are the composite types the body names as a type (§6); the output column types join at
	// CREATE.
	types map[string]bool
	// readsTemp is whether the body reads a temporary relation or sequence, so the view is temporary
	// (§10).
	readsTemp bool
}

func newViewTrace() *viewTrace {
	return &viewTrace{
		items:       map[tokPos]traceItem{},
		catalogRels: map[string]bool{},
		columns:     map[traceColumn]bool{},
		viewColumns: map[traceColumn]bool{},
		stars:       map[tokPos]traceStar{},
		sequences:   map[string]bool{},
		types:       map[string]bool{},
	}
}

// traceItem is one traced FROM item (views.md §7): what it binds and where it is written.
type traceItem struct {
	kind      viewDepKind
	name      string
	namePos   tokPos
	aliasPos  tokPos
	listOpen  tokPos
	listClose tokPos
}

// traceColumn is a referenced base-table column, as (table name, column ordinal).
type traceColumn struct {
	table   string
	ordinal int
}

// traceStar is one written `*` / `rel.*` and the columns it expanded to (views.md §5).
type traceStar struct {
	qualified bool
	cols      []starColumn
}

// starColumn is one column of a frozen star: the label it renders under (hasLabel false = a bare
// "column") and the column name.
type starColumn struct {
	label    string
	hasLabel bool
	column   string
}

// viewNotSupported is the 0A000 every view-only narrowing raises.
func viewNotSupported(msg string) error {
	return newError(FeatureNotSupported, msg)
}

// renamedBaseTable is a copy of a base table (or CTE relation) relabeled by a FROM item's column
// alias list (grammar.md §15): the first len(list) columns take the new names, the rest keep theirs;
// a longer list is 42P10. The copy keeps the table name, so the relation still reads the table's own
// store — the rename is a pure resolution-time relabeling.
func renamedBaseTable(table *catTable, tref *tableRef, list []string) (*catTable, error) {
	label := tableRefLabel(tref)
	if len(list) > len(table.Columns) {
		return nil, newError(InvalidColumnReference, fmt.Sprintf(
			"table \"%s\" has %d columns available but %d columns specified", label, len(table.Columns), len(list)))
	}
	renamed := *table
	renamed.Columns = append([]catColumn(nil), table.Columns...)
	for i, alias := range list {
		renamed.Columns[i].Name = alias
	}
	return &renamed, nil
}

// tableRefLabel is a FROM item's label: its alias, else its name.
func tableRefLabel(tref *tableRef) string {
	if tref.Alias != nil {
		return *tref.Alias
	}
	return tref.Name
}

// --- The planning trace ----------------------------------------------------------------------------

// viewTracing reports whether a view body is being traced right now.
func (db *engine) viewTracing() bool { return db.viewTrace != nil }

// traceColumn records a referenced base-table column (views.md §6).
func (db *engine) traceColumn(table string, ordinal int) {
	if db.viewTrace != nil {
		db.viewTrace.columns[traceColumn{table: table, ordinal: ordinal}] = true
	}
}

// traceRelColumn records a column resolved against a FROM relation: a catalog base table's as a
// dependency (views.md §6), a view reference's as a view column (§8). Every other relation records
// none.
func (db *engine) traceRelColumn(rel *scopeRel, ordinal int) {
	if db.viewTrace == nil {
		return
	}
	if rel.base {
		db.traceColumn(rel.table.Name, ordinal)
	} else if rel.view != "" {
		db.viewTrace.viewColumns[traceColumn{table: rel.view, ordinal: ordinal}] = true
	}
}

// traceCatalogItem records a built-in catalog relation named by the body (views.md §9). The body
// reads only its own database (§4.1/§10), so a temp. or attachment-qualified catalog relation is
// 0A000.
func (db *engine) traceCatalogItem(tref *tableRef) error {
	if db.viewTrace == nil {
		return nil
	}
	if tref.DB != nil && *tref.DB != "main" {
		return viewNotSupported(fmt.Sprintf("a view cannot read the relation \"%s.%s\" of another database", *tref.DB, tref.Name))
	}
	db.viewTrace.catalogRels[tref.Name] = true
	return nil
}

// traceStar records a written star's expansion (views.md §5).
func (db *engine) traceStar(pos tokPos, qualified bool, cols []starColumn) {
	if db.viewTrace != nil {
		db.viewTrace.stars[pos] = traceStar{qualified: qualified, cols: cols}
	}
}

// traceSequence records the sequence a traced body names in the constant first argument of nextval /
// currval / setval (views.md §6). The name must be a sequence, else 42P01, as PostgreSQL's regclass
// reading of the constant fails at definition. A temporary sequence makes the view temporary (§10).
// Text that is not one identifier is left to the call to report.
func (db *engine) traceSequence(text string) error {
	if db.viewTrace == nil {
		return nil
	}
	name, err := parseNameText(text)
	if err != nil {
		return nil
	}
	if db.sequence(name) == nil {
		return newError(UndefinedTable, fmt.Sprintf("relation \"%s\" does not exist", name))
	}
	if db.isTempSequence(name) {
		db.viewTrace.readsTemp = true
	}
	db.viewTrace.sequences[name] = true
	return nil
}

// traceType records a composite type a traced body names as a type — a cast target or a typed
// literal (views.md §6). Array suffixes are stripped by the caller.
func (db *engine) traceType(name string) {
	if db != nil && db.viewTrace != nil {
		db.viewTrace.types[name] = true
	}
}

// traceTableItem records a FROM item that binds a base table. A view may read only the relations of
// its own database and the session's temporary domain — an attached database's table is 0A000; a
// temporary table makes the view temporary (views.md §10).
func (db *engine) traceTableItem(tref *tableRef, table *catTable) error {
	if db.viewTrace == nil {
		return nil
	}
	temp := false
	if tref.DB == nil {
		temp = db.isTempTable(tref.Name)
	} else {
		temp = *tref.DB == "temp"
	}
	if isAttachmentScope(tref.DB) {
		return viewNotSupported(fmt.Sprintf("a view cannot read the attached relation \"%s.%s\"", *tref.DB, tref.Name))
	}
	if temp {
		db.viewTrace.readsTemp = true
	}
	db.viewTrace.items[tref.NamePos] = traceItem{
		kind: viewDepTable, name: table.Name,
		namePos: tref.NamePos, aliasPos: tref.AliasPos, listOpen: tref.ListOpen, listClose: tref.ListClose,
	}
	return nil
}

// maxViewExpansions is the most views one view reference written outside any view body may expand
// to, itself and every view reached through it included (spec/design/views.md §4.1; 54001).
const maxViewExpansions = 1024

// viewSite is where a view reference sits (spec/design/views.md §4.1): the parser's nesting depth at
// the written FROM item, within the text that names the view, and the view body's own nesting depth.
type viewSite struct {
	depth     int
	bodyDepth int
}

// viewSiteBase returns the depth a view body referenced at site starts from, within the statement
// (views.md §4.1). The reference sits at site.depth within the text that names it, which itself starts
// at viewBase, so the body nests as it would with every view reference replaced by its parenthesized
// body. 54001 when the body would reach past maxExprDepth.
func (db *engine) viewSiteBase(site viewSite) (int, error) {
	base := db.viewBase + site.depth
	if base+site.bodyDepth > maxExprDepth {
		return 0, newError(StatementTooComplex, fmt.Sprintf(
			"statement too complex: nesting depth exceeds the maximum of %d", maxExprDepth))
	}
	return base, nil
}

// inViewBody runs f as the planning of a view body (views.md §4.1) referenced at site: the trace of
// an enclosing view body is suspended (a nested view's own relations are not the outer view's
// dependencies), the body's nesting depth counts from the reference's toward maxExprDepth, and the
// expansion toward maxViewExpansions (54001).
func (db *engine) inViewBody(site viewSite, f func() error) error {
	base, err := db.viewSiteBase(site)
	if err != nil {
		return err
	}
	depth := db.viewDepth + 1
	// A view reference written outside any view body starts a new expansion count.
	expansions := db.viewExpansions + 1
	if depth == 1 {
		expansions = 1
	}
	if expansions > maxViewExpansions {
		return newError(StatementTooComplex, fmt.Sprintf(
			"statement too complex: a view reference expands to more than %d views", maxViewExpansions))
	}
	db.viewExpansions = expansions
	saved := db.viewTrace
	db.viewTrace = nil
	db.viewDepth = depth
	savedBase := db.viewBase
	db.viewBase = base
	err = f()
	db.viewBase = savedBase
	db.viewDepth = depth - 1
	if depth == 1 {
		db.viewColumns = nil
	}
	db.viewTrace = saved
	return err
}

// traceViewBody plans body (a view definition) in the isolated view scope with a fresh trace,
// returning the plan and what it bound (views.md §5–§8). Used at CREATE, at re-validation, and before
// a rename rewrite; never for an ordinary read.
func (db *engine) traceViewBody(body *queryExpr) (queryPlan, *viewTrace, error) {
	saved := db.viewTrace
	trace := newViewTrace()
	db.viewTrace = trace
	plan, err := db.planQuery(*body, nil, nil, &paramTypes{})
	db.viewTrace = saved
	return plan, trace, err
}

// --- Reading a view (views.md §4) ------------------------------------------------------------------

// relationView returns the view a relation reference that is not a CTE names, or nil (§4): a built-in
// catalog relation, then a table (session-local temp first for a bare name), then a view (the
// session's temporary view first). Only bare, main., and temp. names reach a view; another qualifier
// never does (§10).
func (db *engine) relationView(qualifier *string, name string) *viewDef {
	var view *viewDef
	shadowed := false
	switch {
	case qualifier == nil:
		view = db.visibleView(name)
		_, shadowed = db.lkpTable(name)
	case *qualifier == "main":
		view = db.readSnap().view(name)
		_, shadowed = db.readSnap().table(name)
	case *qualifier == "temp":
		view = db.tempSnap().view(name)
		shadowed = db.isTempTable(name)
	default:
		return nil
	}
	if view == nil || shadowed {
		return nil
	}
	if _, ok := catalogRelKind(name); ok {
		return nil
	}
	return view
}

// viewReference reports whether the FROM item tref names a view and, if so, returns the equivalent
// derived table `( <definition> ) AS <label> ( <columns> )`, the view's name, and where the reference
// sits. A name resolves to
// a statement-local CTE, then a built-in catalog relation, then a table, then a view (§4); tables
// and views share one namespace. A column list on the reference renames over the view's stored names
// (a longer list is left in place so the derived-table planning raises its 42P10). A view behind an
// attachment qualifier is rejected by the FROM build (§10), so only bare and main. names expand. A
// view whose stored definition does not parse is XX003 (spec/design/unparsable-definitions.md §3.1).
func (db *engine) viewReference(tref *tableRef, ctes []*cteBinding) (tableRef, string, *viewSite, error) {
	if tref.IsFunc || tref.Subquery != nil || tref.Values != nil || tref.JsonTable != nil {
		return tableRef{}, "", nil, nil
	}
	if tref.DB == nil {
		for _, b := range ctes {
			if b.name == tref.Name {
				return tableRef{}, "", nil, nil
			}
		}
	}
	view := db.relationView(tref.DB, tref.Name)
	if view == nil {
		return tableRef{}, "", nil, nil
	}
	if view.Unparsed {
		return tableRef{}, "", nil, unparsableViewError(view.Name)
	}
	label := view.Name
	if tref.Alias != nil {
		label = *tref.Alias
	}
	var columnAliases []string
	switch {
	case tref.ColumnAliases != nil && len(tref.ColumnAliases) > len(view.Columns):
		columnAliases = append([]string(nil), tref.ColumnAliases...)
	case tref.ColumnAliases != nil:
		columnAliases = append([]string(nil), view.Columns...)
		copy(columnAliases, tref.ColumnAliases)
	default:
		columnAliases = append([]string(nil), view.Columns...)
	}
	l := label
	derived := tableRef{Name: label, Alias: &l, Subquery: view.Body, ColumnAliases: columnAliases}
	return derived, view.Name, &viewSite{depth: int(tref.Depth), bodyDepth: view.Depth}, nil
}

// planViewReference plans a view reference's body (views.md §4.1): a fresh top-level scope with no
// correlation, no outer CTEs, and no parameters. While a view body is traced, the reference records
// the dependency on the view itself.
func (db *engine) planViewReference(viewName string, body *queryExpr, written *tableRef, site viewSite, ptypes *paramTypes) (queryPlan, error) {
	if db.viewTrace != nil {
		if db.tempSnap().view(viewName) != nil {
			db.viewTrace.readsTemp = true
		}
		db.viewTrace.items[written.NamePos] = traceItem{
			kind: viewDepView, name: viewName,
			namePos: written.NamePos, aliasPos: written.AliasPos, listOpen: written.ListOpen, listClose: written.ListClose,
		}
	}
	var plan queryPlan
	err := db.inViewBody(site, func() error {
		var perr error
		plan, perr = db.planQuery(*body, nil, nil, ptypes)
		if perr != nil {
			return perr
		}
		if db.viewColumns == nil {
			db.viewColumns = make(map[string]*valuesPlan)
		}
		if _, ok := db.viewColumns[viewName]; !ok {
			db.viewColumns[viewName] = &valuesPlan{
				columnTypes: plan.columnTypes(), columnNames: plan.columnNames(), columnDerivs: plan.columnDerivs(),
				columnTypmods: plan.columnTypmods(),
			}
		}
		return nil
	})
	return plan, err
}

// viewColumnTypes returns the canonical type text of each output column of view (introspection.md
// §5): the body is planned exactly as a reference to the view would plan it, so a body that cannot be
// planned raises the error reading the view would. The text carries the typmod the column keeps
// (create-table-as.md §2), and an untyped NULL column reads as text, the type a derived-table column
// of unknown type takes.
func (db *engine) viewColumnTypes(view *viewDef) ([]string, error) {
	if view.Unparsed {
		return nil, unparsableViewError(view.Name)
	}
	var plan queryPlan
	err := db.inViewBody(viewSite{depth: 0, bodyDepth: view.Depth}, func() error {
		var perr error
		plan, perr = db.planQuery(*view.Body, nil, nil, &paramTypes{})
		return perr
	})
	if err != nil {
		return nil, err
	}
	types, typmods := plan.columnTypes(), plan.columnTypmods()
	out := make([]string, len(types))
	for i, t := range types {
		out[i] = viewColumnTypeText(t, typmods[i])
	}
	return out, nil
}

// viewStandIn is the stand-in for a view reference inside a shape (planner.md §3.2) whose view was
// already planned during this expansion: a row-less VALUES relation with the view's output columns.
// A shape is read only for its own outputs and pushability, never for its relations' bodies.
func (db *engine) viewStandIn(viewName string) (queryPlan, bool) {
	cols, ok := db.viewColumns[viewName]
	if !ok {
		return queryPlan{}, false
	}
	return queryPlan{values: cols}, true
}

// rejectAttachedView refuses a view reached through an attachment qualifier (`FROM reports.v`) with
// 0A000 in this slice (views.md §10). Only an attachment that holds a VIEW of that name (and no
// table) is refused.
func (db *engine) rejectAttachedView(q *string, name string) error {
	if !isAttachmentScope(q) {
		return nil
	}
	snap := db.attachReadSnap(*q)
	if snap == nil {
		return nil
	}
	if _, ok := snap.table(name); !ok && snap.view(name) != nil {
		return viewNotSupported(fmt.Sprintf("views in an attached database are not supported: %s.%s", *q, name))
	}
	return nil
}

// viewsInDependencyOrder returns every view of the main database in dependency order: a view after
// every view it reads, ties in name byte order (views.md §11 — the CLI dump order).
func (db *engine) viewsInDependencyOrder() []*viewDef {
	pending := db.readSnap().viewsSorted()
	out := make([]*viewDef, 0, len(pending))
	done := map[string]bool{}
	for len(pending) > 0 {
		ready := -1
		for i, v := range pending {
			ok := true
			for _, d := range v.Deps {
				if d.Kind == viewDepView && !done[d.Name] {
					ok = false
					break
				}
			}
			if ok {
				ready = i
				break
			}
		}
		if ready < 0 {
			panic("view dependencies are acyclic")
		}
		out = append(out, pending[ready])
		done[pending[ready].Name] = true
		pending = slices.Delete(pending, ready, ready+1)
	}
	return out
}

// isView reports whether name names a view visible to the session: a temporary view of the session or
// a view of the main database (the relation namespace, views.md §2/§10).
func (db *engine) isView(name string) bool { return db.visibleView(name) != nil }

// visibleView returns the view name visible to the session, the session's temporary view first
// (views.md §10). Preclude-overlaps keeps a name in at most one domain (temp-tables.md §3).
func (db *engine) visibleView(name string) *viewDef {
	if v := db.tempSnap().view(name); v != nil {
		return v
	}
	return db.readSnap().view(name)
}

// isTempView reports whether name is a temporary view of the session (views.md §10).
func (db *engine) isTempView(name string) bool { return db.tempSnap().view(name) != nil }

// sessionViews returns every view visible to the session — the main database's and the session's
// temporary ones — in name byte order (views.md §10).
func (db *engine) sessionViews() []*viewDef {
	out := append(db.readSnap().viewsSorted(), db.tempSnap().viewsSorted()...)
	sort.SliceStable(out, func(i, j int) bool { return out[i].Name < out[j].Name })
	return out
}

// hasAnyViews reports whether the main database or the session's temporary domain holds a view.
func (db *engine) hasAnyViews() bool {
	return len(db.readSnap().views) > 0 || len(db.tempSnap().views) > 0
}

// viewsDependingOnAny returns the views, of either domain, that depend on the object name of the
// given kind, in name byte order (views.md §6).
func (db *engine) viewsDependingOnAny(kind viewDepKind, name string) []*viewDef {
	var out []*viewDef
	for _, v := range db.sessionViews() {
		for _, d := range v.Deps {
			if d.Kind == kind && d.Name == name {
				out = append(out, v)
				break
			}
		}
	}
	return out
}

// viewDropClosureAny returns the views a CASCADE drop removes (views.md §6.1), across both domains:
// the seeds and every view that depends on one of them, transitively, in name byte order.
func (db *engine) viewDropClosureAny(seeds []string) []string {
	closure := map[string]bool{}
	for _, s := range seeds {
		closure[s] = true
	}
	for {
		var added []string
		for _, v := range db.sessionViews() {
			if closure[v.Name] {
				continue
			}
			for _, d := range v.Deps {
				if d.Kind == viewDepView && closure[d.Name] {
					added = append(added, v.Name)
					break
				}
			}
		}
		if len(added) == 0 {
			break
		}
		for _, n := range added {
			closure[n] = true
		}
	}
	out := make([]string, 0, len(closure))
	for n := range closure {
		out = append(out, n)
	}
	sort.Strings(out)
	return out
}

// viewObject is a dropped object a view may depend on: its dependency kind and name.
type viewObject struct {
	kind viewDepKind
	name string
}

// ownedSequenceViews is the view check of an ALTER TABLE action that drops the sequences a column
// owns — DROP COLUMN and DROP IDENTITY (views.md §6.2): a view depending on one of them blocks the
// action (2BP01, naming describe) unless cascade, which returns the views to drop with it.
func (db *engine) ownedSequenceViews(seqs []string, cascade bool, describe string) ([]string, error) {
	if len(seqs) == 0 {
		return nil, nil
	}
	objects := make([]viewObject, len(seqs))
	for i, s := range seqs {
		objects[i] = viewObject{kind: viewDepSequence, name: s}
	}
	return db.dropDependentViews(objects, cascade, func(viewDep) string { return describe })
}

// viewDependingOn is a view whose dependency list names a dropped object, with that dependency.
type viewDependingOn struct {
	view string
	dep  viewDep
}

// viewsDependingOnSet returns the views of either domain whose dependency list names one of objects,
// with the first such dependency, in view name byte order (the 2BP01 scan, views.md §6).
func (db *engine) viewsDependingOnSet(objects []viewObject) []viewDependingOn {
	var out []viewDependingOn
	for _, v := range db.sessionViews() {
		for _, d := range v.Deps {
			if slices.ContainsFunc(objects, func(o viewObject) bool { return o.kind == d.Kind && o.name == d.Name }) {
				out = append(out, viewDependingOn{view: v.Name, dep: d})
				break
			}
		}
	}
	return out
}

// tempWorking is the session's temporary domain for a write, flagging it dirty (temp-tables.md §2).
func (db *engine) tempWorking() *snapshot {
	db.session.tx.tempDirty = true
	return db.session.tx.tempWorking
}

// putViewScoped installs a view in its domain: the session's temporary domain or the main database.
func (db *engine) putViewScoped(def *viewDef, temp bool) {
	if temp {
		db.tempWorking().putView(def)
	} else {
		db.working().putView(def)
	}
}

// removeViewScoped removes the view name from whichever domain holds it.
func (db *engine) removeViewScoped(name string) {
	if db.isTempView(name) {
		db.tempWorking().removeView(name)
	} else {
		db.working().removeView(name)
	}
}

// dropDependentViews returns the views a drop of the objects dropped reaches (views.md §6/§6.1):
// under RESTRICT the first view in name byte order that depends on one of them is 2BP01, naming what
// (describe, e.g. `sequence s`) it depends on; under CASCADE the closure of those views is returned
// to be dropped with them.
func (db *engine) dropDependentViews(dropped []viewObject, cascade bool, describe func(viewDep) string) ([]string, error) {
	users := db.viewsDependingOnSet(dropped)
	if len(users) > 0 && !cascade {
		return nil, newError(DependentObjectsStillExist, fmt.Sprintf(
			"cannot drop %s because view %s depends on it", describe(users[0].dep), users[0].view)).hinted(hintDropCascade)
	}
	if len(users) == 0 {
		return nil, nil
	}
	seeds := make([]string, len(users))
	for i, u := range users {
		seeds[i] = u.view
	}
	return db.viewDropClosureAny(seeds), nil
}

// --- CREATE VIEW (views.md §1.1) -------------------------------------------------------------------

func (db *engine) executeCreateView(cv *createView) (outcome, error) {
	// 1. The database qualifier (the allow_ddl / allow_temp_ddl gate already ran in admission): main,
	//    or temp for a temporary view (§10); a TEMP view names no other database.
	temporary := cv.Temporary
	if cv.DB != nil {
		switch q := *cv.DB; {
		case q == "temp":
			temporary = true
		case cv.Temporary:
			return outcome{}, newError(SyntaxError, fmt.Sprintf("a temporary view cannot be created in database \"%s\"", q))
		case q == "main":
		default:
			if db.attachReadSnap(q) == nil {
				return outcome{}, newError(UndefinedTable, `database "`+q+`" is not attached`)
			}
			return outcome{}, viewNotSupported("views in an attached database are not supported")
		}
	}
	// 2. The jed_ reservation (not suppressed by IF NOT EXISTS — introspection.md §4).
	if err := checkReservedName("view", cv.Name); err != nil {
		return outcome{}, err
	}
	// 3. IF NOT EXISTS: a taken name is a silent no-op; the body is not validated.
	if cv.IfNotExists && db.relationExists(cv.Name) {
		return outcome{Kind: outcomeStatement}, nil
	}
	// 4. Plan the body in the isolated view scope. A $N is 42P02 where the resolver meets it; one
	//    consumed by a special-cased form (a parameter CAST) is caught on the tokens right after
	//    planning.
	tokens, err := lex(cv.Body)
	if err != nil {
		return outcome{}, err
	}
	body, depth, err := parseViewDefinitionAt(cv.Body, cv.BodyPositions)
	if err != nil {
		return outcome{}, err
	}
	plan, trace, err := db.traceViewBody(body)
	if err != nil {
		return outcome{}, err
	}
	for _, t := range tokens {
		if t.Kind == tokParam {
			return outcome{}, newError(UndefinedParameter, fmt.Sprintf("there is no parameter $%d", t.Int))
		}
	}
	// The WITH ( … ) options (§16.2, 22023), merged with a trailing check option.
	options, err := parseViewOptions(cv.Options)
	if err != nil {
		return outcome{}, err
	}
	check := cv.CheckOption
	if options.hasCheck {
		if cv.CheckOption != checkOptionNone {
			return outcome{}, duplicateViewOption("check_option")
		}
		check = options.check
	}
	// A check option needs the view's own body to be updatable for INSERT / UPDATE (§15.6).
	if check != checkOptionNone {
		if sel, _ := db.updatableShape(body, true); sel == nil {
			return outcome{}, viewNotSupported("WITH CHECK OPTION is supported only on automatically updatable views")
		}
	}
	// 5. Column-list arity, then duplicate final output names, from the body as planned.
	bodyNames := append([]string(nil), plan.columnNames()...)
	bodyTypes := append([]resolvedType(nil), plan.columnTypes()...)
	bodyDerivs := append([]deriv(nil), plan.columnDerivs()...)
	bodyTypmods := append([]typmod(nil), plan.columnTypmods()...)
	columns, err := viewOutputNames(bodyNames, cv.Columns)
	if err != nil {
		return outcome{}, err
	}
	// A view column cannot be an anonymous record (or an array of one) — 42P16, as for a CREATE
	// TABLE … AS column (composite.md §1).
	for i, name := range columns {
		if err := rejectPseudoType(name, bodyTypes[i]); err != nil {
			return outcome{}, err
		}
	}
	// 6. Freeze every expanded `*` and verify the expanded text plans to the same output.
	definition := cv.Body
	if len(trace.stars) > 0 {
		frozen := freezeStars(tokens, trace)
		notByName := viewNotSupported("a view's * expansion cannot be written by name")
		refrozen, refrozenDepth, perr := parseViewDefinition(frozen)
		if perr != nil {
			return outcome{}, notByName
		}
		plan2, trace2, terr := db.traceViewBody(refrozen)
		if terr != nil {
			return outcome{}, notByName
		}
		names2, types2 := plan2.columnNames(), plan2.columnTypes()
		if !slices.Equal(names2, bodyNames) || len(types2) != len(bodyTypes) {
			return outcome{}, notByName
		}
		for i := range types2 {
			if !resolvedTypeEqual(types2[i], bodyTypes[i]) {
				return outcome{}, notByName
			}
		}
		definition, body, depth, trace = frozen, refrozen, refrozenDepth, trace2
	} else if len(cv.BodyPositions) > 0 {
		// The stored body carries no positions: they point into this statement, not into the
		// view's later uses (spec/design/error-fields.md §10.1).
		stored, _, perr := parseViewDefinition(cv.Body)
		if perr != nil {
			return outcome{}, perr
		}
		body = stored
	}
	deps := db.traceDependencies(trace, bodyTypes)
	// A body that reads a temporary relation makes the view temporary, as in PostgreSQL (§10).
	temporary = temporary || trace.readsTemp
	// 7. The creating session's SELECT privilege on every relation the body reads (§9).
	if err := db.checkViewOwnerPrivileges(deps, trace); err != nil {
		return outcome{}, err
	}
	// 8. A taken name: 42P07, or under OR REPLACE the replacement of a view (§1.2) and 42809 for any
	//    other relation.
	var replaced *viewDef
	if db.relationExists(cv.Name) {
		if !cv.OrReplace {
			return outcome{}, newError(DuplicateTable, fmt.Sprintf("relation \"%s\" already exists", cv.Name))
		}
		replaced = db.visibleView(cv.Name)
		if replaced == nil {
			return outcome{}, notAView(cv.Name)
		}
		// A temporary view stays temporary under OR REPLACE; a persistent view cannot be replaced by
		// a temporary one (preclude-overlaps, §10).
		if db.isTempView(cv.Name) {
			temporary = true
		} else if temporary {
			return outcome{}, newError(DuplicateTable, fmt.Sprintf("relation \"%s\" already exists", cv.Name))
		}
		// 9. Column compatibility (42P16), 10. no cycle (42P17).
		if err := db.checkViewReplacement(replaced, columns, bodyTypes, bodyDerivs, bodyTypmods); err != nil {
			return outcome{}, err
		}
		if db.viewReads(deps, cv.Name) {
			return outcome{}, newError(InvalidObjectDefinition, fmt.Sprintf(
				"infinite recursion detected in rules for relation \"%s\"", cv.Name))
		}
	}
	// 9 (12 under OR REPLACE). Write the catalog entry. A replacement keeps the defaults of the
	// columns it keeps (§16.1); its options are the statement's.
	defaults := make([]*defaultExprDef, len(columns))
	if replaced != nil {
		copy(defaults, replaced.Defaults)
	}
	def := &viewDef{
		Name: cv.Name, Columns: columns, Definition: definition, Deps: deps, Body: body, Depth: depth,
		CheckOption: check, Defaults: defaults,
		SecurityBarrier: options.securityBarrier != nil && *options.securityBarrier,
		SecurityInvoker: options.securityInvoker != nil && *options.securityInvoker,
	}
	db.putViewScoped(def, temporary)
	// 11. The views that read a replaced view are re-validated against the replacement (§8).
	if replaced != nil {
		if err := db.revalidateAfterReplace(replaced); err != nil {
			return outcome{}, err
		}
	}
	return outcome{Kind: outcomeStatement}, nil
}

// checkViewOwnerPrivileges is the owner check of a view (views.md §9): the session needs SELECT on
// every persistent table and view the body reads and on each jed_ catalog relation it names (42501).
// A temporary relation is the session's own and is not privilege-checked, as everywhere.
func (db *engine) checkViewOwnerPrivileges(deps []viewDep, trace *viewTrace) error {
	if db.session.privileges.IsPermissive() {
		return nil
	}
	var read []string
	for _, d := range deps {
		if d.Kind != viewDepTable && d.Kind != viewDepView {
			continue
		}
		if db.isTempTable(d.Name) || db.isTempView(d.Name) {
			continue
		}
		read = append(read, d.Name)
	}
	var cats []string
	for name := range trace.catalogRels {
		cats = append(cats, name)
	}
	sort.Strings(cats)
	read = append(read, cats...)
	for _, name := range read {
		if !db.session.privileges.AllowsTable(name, PrivSelect) {
			return newError(InsufficientPrivilege, "permission denied for table "+name)
		}
	}
	return nil
}

// checkViewReplacement is the CREATE OR REPLACE VIEW column compatibility check (views.md §1.2 step 9,
// PostgreSQL's checkViewColumns): no fewer columns, then each existing column keeps its name, type
// (typmod included), and collation, all 42P16. The existing types and collations come from planning
// the view's current body; a body that no longer plans compares only the names and the count.
func (db *engine) checkViewReplacement(old *viewDef, names []string, types []resolvedType, derivs []deriv, typmods []typmod) error {
	invalid := func(msg string) error { return newError(InvalidTableDefinition, msg) }
	if len(names) < len(old.Columns) {
		return invalid("cannot drop columns from view")
	}
	// A body that does not parse in this binary compares like one that no longer plans
	// (spec/design/unparsable-definitions.md §3.1).
	planned := false
	var current queryPlan
	if !old.Unparsed {
		var err error
		current, _, err = db.traceViewBody(old.Body)
		planned = err == nil
	}
	for i, oldName := range old.Columns {
		if names[i] != oldName {
			return invalid(fmt.Sprintf("cannot change name of view column \"%s\" to \"%s\"", oldName, names[i]))
		}
		if !planned {
			continue
		}
		oldType := current.columnTypes()[i]
		wasType := viewColumnTypeText(oldType, current.columnTypmods()[i])
		isType := viewColumnTypeText(types[i], typmods[i])
		if !resolvedTypeEqual(oldType, types[i]) || wasType != isType {
			return invalid(fmt.Sprintf("cannot change data type of view column \"%s\" from %s to %s",
				oldName, wasType, isType))
		}
		was, wasOK := db.viewCollation(current.columnDerivs()[i])
		is, isOK := db.viewCollation(derivs[i])
		if was != is || wasOK != isOK {
			return invalid(fmt.Sprintf("cannot change collation of view column \"%s\" from \"%s\" to \"%s\"",
				oldName, was, is))
		}
	}
	return nil
}

// viewCollation is a view output column's collation for the replacement check (views.md §1.2): the
// name its derivation carries, the database default for a column that derives none (PostgreSQL's
// `default`), and none (false) for an indeterminate one.
func (db *engine) viewCollation(d deriv) (string, bool) {
	switch d.kind {
	case derivImplicit, derivExplicit:
		return d.name, true
	case derivNone:
		return db.DefaultCollation(), true
	}
	return "", false
}

// viewReads reports whether a body with dependency list deps reads the view target, directly or
// through other views (the 42P17 cycle check, views.md §1.2 step 10).
func (db *engine) viewReads(deps []viewDep, target string) bool {
	var pending []string
	for _, d := range deps {
		if d.Kind == viewDepView {
			pending = append(pending, d.Name)
		}
	}
	seen := map[string]bool{}
	for len(pending) > 0 {
		name := pending[len(pending)-1]
		pending = pending[:len(pending)-1]
		if name == target {
			return true
		}
		if seen[name] {
			continue
		}
		seen[name] = true
		if view := db.visibleView(name); view != nil {
			for _, d := range view.Deps {
				if d.Kind == viewDepView {
					pending = append(pending, d.Name)
				}
			}
		}
	}
	return false
}

// revalidateAfterReplace re-validates the views that read a view old just replaced (views.md §8):
// every view that reads it, directly or through other views, must plan in name byte order; a direct
// dependent must also keep its dependency list and reference no column the replacement appended.
func (db *engine) revalidateAfterReplace(old *viewDef) error {
	readers := map[string]bool{}
	pending := []string{old.Name}
	for len(pending) > 0 {
		name := pending[len(pending)-1]
		pending = pending[:len(pending)-1]
		for _, v := range db.viewsDependingOnAny(viewDepView, name) {
			if !readers[v.Name] {
				readers[v.Name] = true
				pending = append(pending, v.Name)
			}
		}
	}
	names := make([]string, 0, len(readers))
	for name := range readers {
		names = append(names, name)
	}
	sort.Strings(names)
	breaks := func(view string) error {
		return viewNotSupported(fmt.Sprintf(
			"cannot replace view %s because it would change the definition of view %s", old.Name, view))
	}
	for _, name := range names {
		view := db.visibleView(name)
		if view.Unparsed {
			return unparsableViewError(name)
		}
		plan, trace, err := db.traceViewBody(view.Body)
		if isUnparsableError(err) {
			return err
		}
		if err != nil {
			return breaks(name)
		}
		direct := false
		for _, d := range view.Deps {
			if d.Kind == viewDepView && d.Name == old.Name {
				direct = true
			}
		}
		if !direct {
			continue
		}
		for col := range trace.viewColumns {
			if col.table == old.Name && col.ordinal >= len(old.Columns) {
				return breaks(name)
			}
		}
		if !viewDepsEqual(db.traceDependencies(trace, plan.columnTypes()), view.Deps) {
			return breaks(name)
		}
	}
	return nil
}

// traceDependencies is the persisted dependency list of a traced view body (views.md §6): every bound
// table (with its referenced columns, by current name) and view, every sequence the body names, and
// every composite type it names or outputs (output, the planned column types), in ascending (name,
// kind) byte order.
func (db *engine) traceDependencies(trace *viewTrace, output []resolvedType) []viewDep {
	type key struct {
		name string
		kind viewDepKind
	}
	deps := map[key]*viewDep{}
	add := func(kind viewDepKind, name string) *viewDep {
		k := key{name: name, kind: kind}
		if deps[k] == nil {
			deps[k] = &viewDep{Kind: kind, Name: name}
		}
		return deps[k]
	}
	for _, item := range trace.items {
		add(item.kind, item.name)
	}
	for name := range trace.sequences {
		add(viewDepSequence, name)
	}
	types := map[string]bool{}
	for name := range trace.types {
		types[name] = true
	}
	for _, t := range output {
		outputCompositeNames(t, types)
	}
	for name := range types {
		add(viewDepType, name)
	}
	for col := range trace.columns {
		table, ok := db.lkpTable(col.table)
		if !ok || col.ordinal >= len(table.Columns) {
			continue
		}
		name := table.Columns[col.ordinal].Name
		dep := add(viewDepTable, col.table)
		if !slices.Contains(dep.Columns, name) {
			dep.Columns = append(dep.Columns, name)
		}
	}
	out := make([]viewDep, 0, len(deps))
	for _, d := range deps {
		dep := *d
		sort.Strings(dep.Columns)
		out = append(out, dep)
	}
	sort.Slice(out, func(i, j int) bool { return viewDepLess(out[i], out[j]) })
	return out
}

// outputCompositeNames adds the named composite types a view output column of type t carries — the
// type itself or an array's element type (views.md §6).
func outputCompositeNames(t resolvedType, out map[string]bool) {
	switch t.kind {
	case rtComposite:
		if t.comp != nil && t.comp.named {
			out[t.comp.name] = true
		}
	case rtArray:
		if t.elem != nil {
			outputCompositeNames(*t.elem, out)
		}
	}
}

// viewOptionValues are the view options a WITH ( … ) or SET ( … ) list sets (views.md §16.2): each
// set (hasCheck, non-nil) when the list names it.
type viewOptionValues struct {
	hasCheck        bool
	check           checkOption
	securityBarrier *bool
	securityInvoker *bool
}

// duplicateViewOption is the 22023 a view option named twice raises (PostgreSQL's message).
func duplicateViewOption(name string) error {
	return newError(InvalidParameterValue, fmt.Sprintf("parameter \"%s\" specified more than once", name))
}

// parseViewOptions validates a view option list (views.md §16.2), PostgreSQL's reloption rules: the
// names are check_option (local or cascaded, case-insensitive), security_barrier, and
// security_invoker (booleans); a name without a value is true. An unknown name, a repeated one, or a
// bad value is 22023.
func parseViewOptions(options []viewOption) (viewOptionValues, error) {
	var out viewOptionValues
	for _, opt := range options {
		value := "true"
		if opt.HasValue {
			value = opt.Value
		}
		switch opt.Name {
		case "check_option":
			if out.hasCheck {
				return out, duplicateViewOption(opt.Name)
			}
			switch {
			case asciiLower(value) == "local":
				out.check = checkOptionLocal
			case asciiLower(value) == "cascaded":
				out.check = checkOptionCascaded
			default:
				return out, newError(InvalidParameterValue, fmt.Sprintf("invalid value for enum option \"check_option\": %s", value))
			}
			out.hasCheck = true
		case "security_barrier", "security_invoker":
			slot := &out.securityBarrier
			if opt.Name == "security_invoker" {
				slot = &out.securityInvoker
			}
			if *slot != nil {
				return out, duplicateViewOption(opt.Name)
			}
			b, ok := parseReloptionBool(value)
			if !ok {
				return out, newError(InvalidParameterValue, fmt.Sprintf("invalid value for boolean option \"%s\": %s", opt.Name, value))
			}
			*slot = &b
		default:
			return out, newError(InvalidParameterValue, fmt.Sprintf("unrecognized parameter \"%s\"", opt.Name))
		}
	}
	return out, nil
}

// parseReloptionBool is PostgreSQL's parse_bool (views.md §16.2): a case-insensitive prefix of true,
// false, yes, or no; on, or a prefix of off of at least two characters; 1 or 0.
func parseReloptionBool(value string) (bool, bool) {
	v := asciiLower(value)
	if v == "" {
		return false, false
	}
	switch v[0] {
	case 't':
		return true, strings.HasPrefix("true", v)
	case 'f':
		return false, strings.HasPrefix("false", v)
	case 'y':
		return true, strings.HasPrefix("yes", v)
	case 'n':
		return false, strings.HasPrefix("no", v)
	case 'o':
		if v == "on" {
			return true, true
		}
		return false, len(v) >= 2 && strings.HasPrefix("off", v)
	case '1':
		return true, len(v) == 1
	case '0':
		return false, len(v) == 1
	}
	return false, false
}

// --- DROP VIEW (views.md §1, §6) ---------------------------------------------------------------------

func (db *engine) executeDropView(dv *dropView) (outcome, error) {
	// Phase 1: resolve every name (42P01 / 42809) before any dependency check.
	var targets []string
	for _, qn := range dv.Names {
		name := qn.Name
		if isCatalogRelName(name) {
			return outcome{}, newError(WrongObjectType, fmt.Sprintf("cannot drop system relation \"%s\"", name))
		}
		if qn.DB != nil {
			switch *qn.DB {
			case "main":
				if db.isTempView(name) {
					if dv.IfExists {
						continue
					}
					return outcome{}, newError(UndefinedTable, fmt.Sprintf("view \"main.%s\" does not exist", name))
				}
			case "temp":
				switch {
				case db.isTempView(name):
				case db.isTempTable(name) || db.isTempSequence(name):
					return outcome{}, notAView(name)
				default:
					if dv.IfExists {
						continue
					}
					return outcome{}, newError(UndefinedTable, fmt.Sprintf("view \"temp.%s\" does not exist", name))
				}
			default:
				if db.attachReadSnap(*qn.DB) == nil {
					return outcome{}, newError(UndefinedTable, `database "`+*qn.DB+`" is not attached`)
				}
				return outcome{}, viewNotSupported("views in an attached database are not supported")
			}
		}
		// Deduplicate only after the qualifier is validated, so a repeated name cannot skip a bad
		// `temp.`/attachment qualifier.
		if slices.Contains(targets, name) {
			continue
		}
		if db.isView(name) {
			targets = append(targets, name)
			continue
		}
		if db.relationExists(name) {
			return outcome{}, notAView(name)
		}
		if dv.IfExists {
			continue
		}
		return outcome{}, newError(UndefinedTable, fmt.Sprintf("view \"%s\" does not exist", name))
	}
	// Phase 2: under RESTRICT a dependent view outside the drop set blocks (2BP01), naming the first
	// in name byte order; under CASCADE every dependent view is dropped too, transitively. Both
	// domains: a temporary view may read a persistent one (§10).
	if dv.Cascade {
		targets = db.viewDropClosureAny(targets)
	}
	for _, dependent := range db.sessionViews() {
		if slices.Contains(targets, dependent.Name) {
			continue
		}
		for _, d := range dependent.Deps {
			if d.Kind != viewDepView || !slices.Contains(targets, d.Name) {
				continue
			}
			return outcome{}, newError(DependentObjectsStillExist, fmt.Sprintf(
				"cannot drop view %s because view %s depends on it", d.Name, dependent.Name)).hinted(hintDropCascade)
		}
	}
	// Phase 3: remove.
	for _, name := range targets {
		db.removeViewScoped(name)
	}
	return outcome{Kind: outcomeStatement}, nil
}

// viewColumnTypeText is the type text of a view output column (introspection.md §5, views.md §1.2):
// the result type with the typmod the column keeps applied at a scalar leaf (`varchar(3)`,
// `decimal(5,2)`), and an untyped NULL as text. An array keeps no element typmod
// (create-table-as.md §5).
func viewColumnTypeText(t resolvedType, tm typmod) string {
	switch {
	case t.kind == rtNull:
		return "text"
	case t.kind == rtText && tm.VarcharLen != nil:
		return fmt.Sprintf("varchar(%d)", *tm.VarcharLen)
	case t.kind == rtDecimal && tm.Decimal != nil:
		return fmt.Sprintf("decimal(%d,%d)", tm.Decimal.Precision, tm.Decimal.Scale)
	case (t.kind == rtTimestamp || t.kind == rtTimestamptz || t.kind == rtInterval) && tm.Temporal != nil:
		return rtName(t) + temporalTypmodText(*tm.Temporal)
	}
	return rtName(t)
}

// viewOutputNames is the view's output column names (views.md §1): the column list applied over the
// body's names left to right. A longer list is 42601; a duplicate final name is 42701; no column is
// 0A000.
func viewOutputNames(bodyNames, list []string) ([]string, error) {
	names := append([]string(nil), bodyNames...)
	if list != nil {
		if len(list) > len(names) {
			return nil, newError(SyntaxError, "CREATE VIEW specifies more column names than columns")
		}
		copy(names, list)
	}
	for i, name := range names {
		if slices.Contains(names[:i], name) {
			return nil, newError(DuplicateColumn, fmt.Sprintf("column \"%s\" specified more than once", name))
		}
	}
	if len(names) == 0 {
		return nil, viewNotSupported("a view must have at least one column")
	}
	return names, nil
}

// freezeStars replaces every traced written star in the definition's tokens by its expansion
// (views.md §5): a labeled column as `"label" . "column"`, an unlabeled one as `"column"`,
// comma-separated.
func freezeStars(tokens []token, trace *viewTrace) string {
	out := make([]token, 0, len(tokens))
	for i := 0; i < len(tokens); {
		if tokens[i].Kind == tokEof {
			break
		}
		star, ok := trace.stars[tokPos(i)]
		if !ok {
			out = append(out, tokens[i])
			i++
			continue
		}
		for k, c := range star.cols {
			if k > 0 {
				out = append(out, token{Kind: tokComma})
			}
			if c.hasLabel {
				out = append(out, nameToken(c.label), token{Kind: tokDot})
			}
			out = append(out, nameToken(c.column))
		}
		if star.qualified {
			i += 3
		} else {
			i++
		}
	}
	return renderTokens(out)
}

// --- Seeing through view references: write classification and function privileges (views.md §4.1,
// §9). A view body runs inside the referencing statement, so a body that calls nextval makes a
// read-shaped statement a write, and the functions it calls need the reader's EXECUTE. ---------------

// stmtIsWriteWithViews is stmtIsWrite plus the view check: a read-shaped statement that reads a view
// whose body (transitively) calls a sequence-mutating function is a write (sequences.md §4).
func (db *engine) stmtIsWriteWithViews(stmt statement) bool {
	if stmtIsWrite(stmt) {
		return true
	}
	if !db.hasAnyViews() {
		return false
	}
	if stmt.Explain != nil {
		if !stmt.Explain.Analyze {
			return false
		}
		stmt = *stmt.Explain.Inner
	}
	if stmt.Select == nil && stmt.SetOp == nil && stmt.With == nil {
		return false
	}
	var req privReq
	collectStmtPrivs(stmt, &req)
	found := false
	db.walkReadViews(req.relations, func(view *viewDef, _ *privReq) bool {
		found = queryCallsSeqMutator(view.Body)
		return found
	})
	return found
}

// walkReadViews visits each view the relation references in pending read, then each view their bodies
// read in turn (§4), resolving every reference as the planner does (relationView). visit receives the
// view and its body's collected requirements; returning true stops the walk. Each view is visited
// once, so the walk terminates whatever names a body holds.
func (db *engine) walkReadViews(pending []relationRef, visit func(*viewDef, *privReq) bool) {
	seen := map[string]bool{}
	for len(pending) > 0 {
		ref := pending[len(pending)-1]
		pending = pending[:len(pending)-1]
		view := db.relationView(ref.db, ref.name)
		if view == nil || seen[view.Name] {
			continue
		}
		seen[view.Name] = true
		// An unparsed body has nothing to collect; planning the statement reports it (XX003).
		if view.Unparsed {
			continue
		}
		var body privReq
		collectQueryPrivs(view.Body, &body, map[string]bool{})
		if visit(view, &body) {
			return
		}
		pending = append(pending, body.relations...)
	}
}

// collectViewFunctionPrivs adds the functions called in the bodies of the views a statement reads or
// writes through (transitively) to its privilege requirements (views.md §9, §15.4): function EXECUTE
// is the reader's or writer's, as for a derived table. The relations inside a body are NOT added —
// the view is the privilege boundary — except through a security_invoker view (§16.2), whose body's
// relations are checked against the reader, and whose base relation needs the writer's privileges on
// the view.
func (db *engine) collectViewFunctionPrivs(req *privReq) {
	if !db.hasAnyViews() {
		return
	}
	var writeTargets []string
	for _, t := range req.tables {
		if t.priv == PrivInsert || t.priv == PrivUpdate || t.priv == PrivDelete {
			writeTargets = append(writeTargets, t.name)
		}
	}
	pending := append([]relationRef(nil), req.relations...)
	for _, name := range writeTargets {
		pending = append(pending, relationRef{name: name})
	}
	var functions []string
	db.walkReadViews(pending, func(_ *viewDef, body *privReq) bool {
		functions = append(functions, body.functions...)
		return false
	})
	req.functions = append(req.functions, functions...)
	tables := db.invokerReadPrivs(append([]relationRef(nil), req.relations...))
	tables = append(tables, db.invokerWritePrivs(req.tables, writeTargets)...)
	req.tables = append(req.tables, tables...)
}

// invokerReadPrivs returns the SELECT privileges a read of the relations pending needs inside
// security_invoker views (views.md §16.2): each such view's body reads its relations as the reader,
// as a derived table would, and a security_invoker view it reads in turn is checked the same way. A
// definer view's body is not entered.
func (db *engine) invokerReadPrivs(pending []relationRef) []privTableReq {
	var out []privTableReq
	seen := map[string]bool{}
	for len(pending) > 0 {
		ref := pending[len(pending)-1]
		pending = pending[:len(pending)-1]
		view := db.relationView(ref.db, ref.name)
		if view == nil || !view.SecurityInvoker || seen[view.Name] {
			continue
		}
		seen[view.Name] = true
		if view.Unparsed {
			continue
		}
		var body privReq
		collectQueryPrivs(view.Body, &body, map[string]bool{})
		out = append(out, body.tables...)
		pending = append(pending, body.relations...)
	}
	return out
}

// invokerWritePrivs returns the privileges a write through a security_invoker view needs on the
// relations below it (views.md §16.2): every privilege the statement needs on the view is needed on
// its base relation too, plus SELECT there when the view's WHERE is evaluated (an UPDATE or DELETE),
// and what a subquery in that WHERE reads. A chain continues while the base relation is itself a
// security_invoker view.
func (db *engine) invokerWritePrivs(stmt []privTableReq, targets []string) []privTableReq {
	var out []privTableReq
	for _, target := range targets {
		var privs []Privilege
		for _, t := range stmt {
			if t.name == target {
				privs = append(privs, t.priv)
			}
		}
		name := target
		seen := map[string]bool{}
		for {
			view := db.relationView(nil, name)
			if view == nil || !view.SecurityInvoker || seen[view.Name] {
				break
			}
			seen[view.Name] = true
			if view.Unparsed {
				break
			}
			sel, tref := db.updatableShape(view.Body, false)
			if sel == nil {
				break
			}
			if sel.Filter != nil {
				if (slices.Contains(privs, PrivUpdate) || slices.Contains(privs, PrivDelete)) && !slices.Contains(privs, PrivSelect) {
					privs = append(privs, PrivSelect)
				}
				var req privReq
				collectExprPrivs(sel.Filter, &req, map[string]bool{})
				out = append(out, req.tables...)
			}
			for _, p := range privs {
				out = append(out, privTableReq{name: tref.Name, priv: p})
			}
			name = tref.Name
		}
	}
	return out
}

// --- Dependencies on tables (views.md §6) ----------------------------------------------------------

// dropTableViews is the DROP TABLE view-dependency check (§6). Under RESTRICT a view depending on a
// table being dropped — or on a sequence a dropped table owns, which the drop takes with it — blocks
// the statement (2BP01), naming the first in name byte order. Under CASCADE it returns the views to
// drop with the tables: every view depending on one of them, and every view depending on one of
// those, transitively. Views of both domains count (§10).
func (db *engine) dropTableViews(targets []string, cascade bool) ([]string, error) {
	var dropped []viewObject
	owner := map[string]string{}
	for _, t := range targets {
		dropped = append(dropped, viewObject{kind: viewDepTable, name: t})
		var owned []string
		if db.isTempTable(t) {
			owned = db.tempSnap().sequencesOwnedBy(t)
		} else {
			owned = db.readSnap().sequencesOwnedBy(t)
		}
		for _, s := range owned {
			owner[s] = t
			dropped = append(dropped, viewObject{kind: viewDepSequence, name: s})
		}
	}
	return db.dropDependentViews(dropped, cascade, func(d viewDep) string {
		if d.Kind == viewDepSequence {
			return "table " + owner[d.Name]
		}
		return "table " + d.Name
	})
}

// --- ALTER TABLE: rewrites (views.md §7) and re-validation (§8) -------------------------------------

// viewAlterChangeKind is what an ALTER TABLE / ALTER VIEW statement changed that the dependent views'
// text must follow (§7, §7.1).
type viewAlterChangeKind int

const (
	// viewAlterOther is every other action: dropped holds the original ordinals of the columns it
	// dropped (possibly none).
	viewAlterOther viewAlterChangeKind = iota
	// viewAlterRenameRelation is RENAME TO newName of the table or view.
	viewAlterRenameRelation
	// viewAlterRenameColumn is RENAME COLUMN of the column at ordinal to newName.
	viewAlterRenameColumn
)

type viewAlterChange struct {
	kind    viewAlterChangeKind
	newName string
	ordinal int
	dropped []int
}

// viewAlterPlan is the dependent views of a table or view captured before an ALTER TABLE / ALTER VIEW
// runs (§7/§7.1/§8): each view with the FROM items that bind the altered relation, as traced against
// the pre-statement catalog, and the relation's kind, pre-statement name, and column names.
type viewAlterPlan struct {
	kind    viewDepKind
	table   string
	columns []string
	views   []*viewDef
	items   [][]traceItem
}

// viewUsing returns the first dependent view (name byte order) that references the table's column at
// the pre-statement ordinal (§6), or "".
func (vp *viewAlterPlan) viewUsing(ordinal int) string {
	if users := vp.viewsUsing(ordinal); len(users) > 0 {
		return users[0]
	}
	return ""
}

// viewsUsing returns every dependent view (name byte order) that references the table's column at the
// pre-statement ordinal (§6).
func (vp *viewAlterPlan) viewsUsing(ordinal int) []string {
	column := vp.columns[ordinal]
	var out []string
	for _, v := range vp.views {
		for _, d := range v.Deps {
			if d.Kind == viewDepTable && d.Name == vp.table && slices.Contains(d.Columns, column) {
				out = append(out, v.Name)
				break
			}
		}
	}
	return out
}

// forget stops tracking the views a DROP COLUMN … CASCADE drops (§6): they no longer block the
// statement and are neither rewritten nor re-validated.
func (vp *viewAlterPlan) forget(dropped []string) {
	var views []*viewDef
	var items [][]traceItem
	for i, v := range vp.views {
		if !slices.Contains(dropped, v.Name) {
			views = append(views, v)
			items = append(items, vp.items[i])
		}
	}
	vp.views, vp.items = views, items
}

// alterBreaksView is the 0A000 an ALTER TABLE / ALTER VIEW raises when a dependent view would not
// survive it (§8).
func alterBreaksView(vp *viewAlterPlan, view string) error {
	kind := "table"
	if vp.kind == viewDepView {
		kind = "view"
	}
	return viewNotSupported(fmt.Sprintf("cannot alter %s %s because it would change the definition of view %s", kind, vp.table, view))
}

// notAView is the 42809 a statement that names views only raises for any other relation.
func notAView(name string) error {
	return newError(WrongObjectType, fmt.Sprintf("\"%s\" is not a view", name))
}

// prepareViewAlter captures the views that depend on table before an ALTER TABLE runs. When the action
// edits view text (rewrites), each view is planned against the pre-statement catalog to find the FROM
// items that bind the table; a view that cannot be planned fails the statement (0A000). nil when no
// view depends on the table. A temporary view of the session counts (§10).
func (db *engine) prepareViewAlter(table *catTable, rewrites bool) (*viewAlterPlan, error) {
	columns := make([]string, 0, len(table.Columns))
	for _, c := range table.Columns {
		columns = append(columns, c.Name)
	}
	return db.prepareDependents(viewDepTable, table.Name, columns, rewrites)
}

// prepareDependents captures the views, of either domain, that depend on the table or view name before
// the statement that alters it runs (§7/§7.1/§8). nil when no view depends on it.
func (db *engine) prepareDependents(kind viewDepKind, name string, columns []string, rewrites bool) (*viewAlterPlan, error) {
	dependents := db.viewsDependingOnAny(kind, name)

	if len(dependents) == 0 {
		return nil, nil
	}
	vp := &viewAlterPlan{kind: kind, table: name, columns: columns}
	for _, view := range dependents {
		// A dependent is rewritten and re-planned, which needs its parsed body
		// (spec/design/unparsable-definitions.md §3.1).
		if view.Unparsed {
			return nil, unparsableViewError(view.Name)
		}
		var items []traceItem
		if rewrites {
			_, trace, err := db.traceViewBody(view.Body)
			if isUnparsableError(err) {
				return nil, err
			}
			if err != nil {
				return nil, alterBreaksView(vp, view.Name)
			}
			for _, item := range trace.items {
				if item.kind == kind && item.name == name {
					items = append(items, item)
				}
			}
			sort.Slice(items, func(i, j int) bool { return items[i].namePos < items[j].namePos })
		}
		vp.views = append(vp.views, view)
		vp.items = append(vp.items, items)
	}
	return vp, nil
}

// finishViewAlter runs after an ALTER TABLE changed the working catalog: rewrite each dependent
// view's text for the statement's renames / dropped columns (§7), then re-validate it — it must plan
// in the post-statement catalog and bind exactly its (renamed) dependency list (§8). Every rewrite
// is installed before any view is re-validated, because a dependent view may also read another
// dependent view, whose body must already be the rewritten one. A failure is 0A000 and the
// statement fails, which discards every change it made.
func (db *engine) finishViewAlter(vp *viewAlterPlan, change viewAlterChange) error {
	bodies := make([]*queryExpr, len(vp.views))
	expecteds := make([][]viewDep, len(vp.views))
	for i, view := range vp.views {
		definition, err := rewriteViewText(view.Definition, vp.items[i], vp, change)
		if err != nil {
			return err
		}
		expected := renamedDeps(view.Deps, vp, change)
		body, depth, err := parseViewDefinition(definition)
		if err != nil {
			return alterBreaksView(vp, view.Name)
		}
		bodies[i], expecteds[i] = body, expected
		if definition != view.Definition || !viewDepsEqual(expected, view.Deps) {
			updated := *view
			updated.Definition, updated.Deps, updated.Body, updated.Depth = definition, expected, body, depth
			db.putViewScoped(&updated, db.isTempView(view.Name))
		}
	}
	for i, view := range vp.views {
		plan, trace, err := db.traceViewBody(bodies[i])
		if isUnparsableError(err) {
			return err
		}
		if err != nil {
			return alterBreaksView(vp, view.Name)
		}
		if !viewDepsEqual(db.traceDependencies(trace, plan.columnTypes()), expecteds[i]) {
			return alterBreaksView(vp, view.Name)
		}
	}
	return nil
}

// executeAlterView runs `ALTER VIEW [IF EXISTS] [main. | temp.]v RENAME TO v2` / `RENAME [COLUMN] a TO
// b`, and the same ALTER TABLE forms on a view (views.md §7.1): the views that read v are rewritten to
// keep binding it, then re-validated (§8). The other actions — column defaults and options — change
// only the view's own entry (§16).
func (db *engine) executeAlterView(av *alterView) (outcome, error) {
	done := outcome{Kind: outcomeStatement}
	// 1. The database qualifier (the allow_ddl / allow_temp_ddl gate already ran in admission).
	var view *viewDef
	switch {
	case av.DB == nil:
		view = db.visibleView(av.Name)
	case *av.DB == "main":
		view = db.readSnap().view(av.Name)
	case *av.DB == "temp":
		view = db.tempSnap().view(av.Name)
	default:
		if db.attachReadSnap(*av.DB) == nil {
			return outcome{}, newError(UndefinedTable, `database "`+*av.DB+`" is not attached`)
		}
		return outcome{}, viewNotSupported("views in an attached database are not supported")
	}
	// 2. The name: a view, else 42809 for another relation, else 42P01 (IF EXISTS: no-op).
	if view == nil {
		// Another relation of the named database (the session's whole namespace when bare).
		var other bool
		if av.DB == nil {
			other = db.relationExists(av.Name)
		} else {
			snap := db.readSnap()
			if *av.DB != "main" {
				snap = db.tempSnap()
			}
			_, isTable := snap.table(av.Name)
			_, _, isIndex := snap.findIndex(av.Name)
			other = isTable || isIndex || snap.sequence(av.Name) != nil
		}
		if isCatalogRelName(av.Name) || other {
			return outcome{}, notAView(av.Name)
		}
		if av.IfExists {
			return done, nil
		}
		return outcome{}, newError(UndefinedTable, fmt.Sprintf("relation \"%s\" does not exist", av.Name))
	}
	temp := db.isTempView(view.Name)
	// 3. The new name or the column.
	var change viewAlterChange
	switch {
	case av.Edits != nil:
		edited, err := db.applyViewEdits(view, av.Edits)
		if err != nil {
			return outcome{}, err
		}
		db.putViewScoped(edited, temp)
		return done, nil
	case av.RenameColumn == nil:
		if err := checkReservedName("view", av.RenameTo); err != nil {
			return outcome{}, err
		}
		if isCatalogRelName(av.RenameTo) || db.relationExists(av.RenameTo) {
			return outcome{}, newError(DuplicateTable, fmt.Sprintf("relation \"%s\" already exists", av.RenameTo))
		}
		change = viewAlterChange{kind: viewAlterRenameRelation, newName: av.RenameTo}
	default:
		ordinal := slices.Index(view.Columns, av.RenameColumn.Old)
		if ordinal < 0 {
			return outcome{}, newError(UndefinedColumn, fmt.Sprintf("column \"%s\" does not exist", av.RenameColumn.Old))
		}
		if slices.Contains(view.Columns, av.RenameColumn.New) {
			return outcome{}, newError(DuplicateColumn, fmt.Sprintf(
				"column \"%s\" of relation \"%s\" already exists", av.RenameColumn.New, view.Name))
		}
		change = viewAlterChange{kind: viewAlterRenameColumn, newName: av.RenameColumn.New, ordinal: ordinal}
	}
	// 4. Capture the dependents against the pre-statement catalog, rename, then rewrite and re-validate
	//    them (5).
	vp, err := db.prepareDependents(viewDepView, view.Name, append([]string(nil), view.Columns...), true)
	if err != nil {
		return outcome{}, err
	}
	renamed := *view
	renamed.Columns = append([]string(nil), view.Columns...)
	if change.kind == viewAlterRenameRelation {
		db.removeViewScoped(view.Name)
		renamed.Name = change.newName
	} else {
		renamed.Columns[change.ordinal] = change.newName
	}
	db.putViewScoped(&renamed, temp)
	if vp != nil {
		if err := db.finishViewAlter(vp, change); err != nil {
			return outcome{}, err
		}
	}
	return done, nil
}

// applyViewEdits applies the non-rename ALTER VIEW actions to view, in order (views.md §16): a column
// default is validated against the column's type, from the planned body, as a table column's SET
// DEFAULT is; SET ( … ) validates its options (22023) and a check option the view's shape (0A000);
// RESET ( … ) clears the named options (an unknown name is ignored, as in PostgreSQL). Turning
// security_invoker off makes the view a definer view, so the session must pass the owner check (§9).
func (db *engine) applyViewEdits(view *viewDef, edits []viewEdit) (*viewDef, error) {
	out := *view
	out.Defaults = make([]*defaultExprDef, len(view.Columns))
	copy(out.Defaults, view.Defaults)
	var columns []catColumn
	columnIndex := func(name string) (int, error) {
		if i := slices.Index(view.Columns, name); i >= 0 {
			return i, nil
		}
		return -1, newError(UndefinedColumn, fmt.Sprintf("column \"%s\" of relation \"%s\" does not exist", name, view.Name))
	}
	for _, edit := range edits {
		switch edit.Kind {
		case viewEditSetDefault:
			i, err := columnIndex(edit.Column)
			if err != nil {
				return nil, err
			}
			if columns == nil {
				if columns, err = db.viewTypedColumns(view); err != nil {
					return nil, err
				}
			}
			def := edit.Default
			val, expr, err := db.buildAlterDefault(columns[i], &def)
			if err != nil {
				return nil, err
			}
			switch {
			case (val != nil && val.IsNull()) || (val == nil && expr == nil):
				out.Defaults[i] = nil
			case val != nil:
				text := val.sqlLiteral()
				e, err := parseExpression(text)
				if err != nil {
					return nil, err
				}
				out.Defaults[i] = &defaultExprDef{ExprText: text, Expr: e}
			default:
				out.Defaults[i] = expr
			}
		case viewEditDropDefault:
			i, err := columnIndex(edit.Column)
			if err != nil {
				return nil, err
			}
			out.Defaults[i] = nil
		case viewEditSetOptions:
			options, err := parseViewOptions(edit.Options)
			if err != nil {
				return nil, err
			}
			if options.hasCheck {
				if view.Unparsed {
					return nil, unparsableViewError(view.Name)
				}
				if sel, _ := db.updatableShape(view.Body, true); sel == nil {
					return nil, viewNotSupported("WITH CHECK OPTION is supported only on automatically updatable views")
				}
				out.CheckOption = options.check
			}
			if options.securityBarrier != nil {
				out.SecurityBarrier = *options.securityBarrier
			}
			if options.securityInvoker != nil {
				out.SecurityInvoker = *options.securityInvoker
			}
		case viewEditResetOptions:
			for _, name := range edit.Names {
				switch name {
				case "check_option":
					out.CheckOption = checkOptionNone
				case "security_barrier":
					out.SecurityBarrier = false
				case "security_invoker":
					out.SecurityInvoker = false
				}
			}
		}
	}
	if view.SecurityInvoker && !out.SecurityInvoker {
		if view.Unparsed {
			return nil, unparsableViewError(view.Name)
		}
		_, trace, err := db.traceViewBody(view.Body)
		if err != nil {
			return nil, err
		}
		if err := db.checkViewOwnerPrivileges(view.Deps, trace); err != nil {
			return nil, err
		}
	}
	return &out, nil
}

// renameSequenceInViews is ALTER SEQUENCE s RENAME TO s2's effect on views (views.md §7.2): every view
// that depends on s keeps naming it — the constant argument of each nextval / currval / setval call
// that names s becomes 's2' (the new name as an identifier, quoted when it must be), and the
// dependency follows. Sequences are resolved by name everywhere, so every such constant names this
// sequence.
func (db *engine) renameSequenceInViews(old, new string) error {
	for _, view := range db.viewsDependingOnAny(viewDepSequence, old) {
		tokens, err := lex(view.Definition)
		if err != nil {
			return err
		}
		for i := 0; i+2 < len(tokens); i++ {
			t := tokens[i]
			call := t.Kind == tokWord && (t.Word == "nextval" || t.Word == "currval" || t.Word == "setval") &&
				tokens[i+1].Kind == tokLParen
			if !call || tokens[i+2].Kind != tokStr {
				continue
			}
			if n, err := parseNameText(tokens[i+2].Word); err == nil && n == old {
				tokens[i+2] = token{Kind: tokStr, Word: quoteIdentifier(new)}
			}
		}
		end := len(tokens)
		for i, t := range tokens {
			if t.Kind == tokEof {
				end = i
				break
			}
		}
		definition := renderTokens(tokens[:end])
		body, depth, err := parseViewDefinition(definition)
		if err != nil {
			return err
		}
		deps := make([]viewDep, len(view.Deps))
		for i, d := range view.Deps {
			if d.Kind == viewDepSequence && d.Name == old {
				d.Name = new
			}
			deps[i] = d
		}
		sort.SliceStable(deps, func(i, j int) bool { return viewDepLess(deps[i], deps[j]) })
		updated := *view
		updated.Definition, updated.Deps, updated.Body, updated.Depth = definition, deps, body, depth
		db.putViewScoped(&updated, db.isTempView(view.Name))
	}
	return nil
}

// viewTypedColumns returns the output columns of view as table columns typed by its planned body
// (views.md §16.1): what a column default is validated against. An untyped NULL column is text.
func (db *engine) viewTypedColumns(view *viewDef) ([]catColumn, error) {
	if view.Unparsed {
		return nil, unparsableViewError(view.Name)
	}
	var plan queryPlan
	err := db.inViewBody(viewSite{depth: 0, bodyDepth: view.Depth}, func() error {
		var perr error
		plan, perr = db.planQuery(*view.Body, nil, nil, &paramTypes{})
		return perr
	})
	if err != nil {
		return nil, err
	}
	types := append([]resolvedType(nil), plan.columnTypes()...)
	for i, t := range types {
		if t.kind == rtNull {
			types[i] = resolvedType{kind: rtText}
		}
	}
	synth, err := cteSyntheticTableCols(view.Name, view.Columns, types, plan.columnDerivs(), nil, nil)
	if err != nil {
		return nil, err
	}
	return synth.Columns, nil
}

// viewDepsEqual reports whether two dependency lists are identical.
func viewDepsEqual(a, b []viewDep) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i].Kind != b[i].Kind || a[i].Name != b[i].Name || !slices.Equal(a[i].Columns, b[i].Columns) {
			return false
		}
	}
	return true
}

// renamedDeps is a view's dependency list after the statement's own renames (§8): the table's entry
// follows a RENAME TO, its column entry follows a RENAME COLUMN, and the list is re-sorted.
func renamedDeps(deps []viewDep, vp *viewAlterPlan, change viewAlterChange) []viewDep {
	out := make([]viewDep, len(deps))
	for i, d := range deps {
		d.Columns = append([]string(nil), d.Columns...)
		if d.Kind == vp.kind && d.Name == vp.table {
			switch change.kind {
			case viewAlterRenameRelation:
				d.Name = change.newName
			case viewAlterRenameColumn:
				old := vp.columns[change.ordinal]
				for k, c := range d.Columns {
					if c == old {
						d.Columns[k] = change.newName
					}
				}
				sort.Strings(d.Columns)
			}
		}
		out[i] = d
	}
	sort.SliceStable(out, func(i, j int) bool { return viewDepLess(out[i], out[j]) })
	return out
}

// rewriteViewText applies the §7 token edits to a stored definition for each FROM item in items (all
// of which bind the altered table) and re-renders it canonically. Every inserted or substituted name
// is a quoted identifier and an inserted keyword is the bare word `as`; untouched tokens keep their
// spelling.
func rewriteViewText(definition string, items []traceItem, vp *viewAlterPlan, change viewAlterChange) (string, error) {
	tokens, err := lex(definition)
	if err != nil {
		return "", err
	}
	edits := newTokenEdits()
	asWord := token{Kind: tokWord, Word: "as"}
	for _, item := range items {
		namePos := int(item.namePos)
		switch change.kind {
		case viewAlterRenameRelation:
			edits.replace(namePos, []token{nameToken(change.newName)})
			if item.aliasPos == 0 {
				edits.insertAfter(namePos, []token{asWord, nameToken(vp.table)})
			}
		case viewAlterRenameColumn:
			// The names this item exposed for positions 0..=k before the statement.
			var list []string
			if item.listOpen != 0 {
				list = listEntries(tokens, int(item.listOpen), int(item.listClose))
			}
			if len(list) > change.ordinal {
				continue
			}
			exposed := func(j int) string {
				if j < len(list) {
					return list[j]
				}
				return vp.columns[j]
			}
			var tail []token
			for j := len(list); j <= change.ordinal; j++ {
				if j > 0 {
					tail = append(tail, token{Kind: tokComma})
				}
				tail = append(tail, nameToken(exposed(j)))
			}
			switch {
			case item.aliasPos == 0:
				ins := []token{asWord, nameToken(vp.table), {Kind: tokLParen}}
				ins = append(ins, tail...)
				ins = append(ins, token{Kind: tokRParen})
				edits.insertAfter(namePos, ins)
			case item.listOpen == 0:
				ins := append([]token{{Kind: tokLParen}}, tail...)
				ins = append(ins, token{Kind: tokRParen})
				edits.insertAfter(int(item.aliasPos), ins)
			default:
				edits.insertAfter(int(item.listClose)-1, tail)
			}
		default:
			if item.listOpen == 0 {
				continue
			}
			open, closing := int(item.listOpen), int(item.listClose)
			n := (closing - open) / 2
			var gone []int
			for _, j := range change.dropped {
				if j < n {
					gone = append(gone, j)
				}
			}
			if len(gone) == 0 {
				continue
			}
			if len(gone) == n {
				// The whole list goes, with its parentheses.
				for t := open; t <= closing; t++ {
					edits.delete(t)
				}
				continue
			}
			// Remove each dropped entry with one separating comma: the comma before it, or — for an
			// entry before the first kept one — the comma after it.
			keptFirst := -1
			for j := 0; j < n; j++ {
				if !slices.Contains(gone, j) {
					keptFirst = j
					break
				}
			}
			for _, j := range gone {
				entry := open + 1 + 2*j
				edits.delete(entry)
				if j < keptFirst {
					edits.delete(entry + 1)
				} else {
					edits.delete(entry - 1)
				}
			}
		}
	}
	return edits.apply(tokens), nil
}

// listEntries returns the identifiers of a column alias list whose `(` / `)` are at open / closing.
func listEntries(tokens []token, open, closing int) []string {
	var out []string
	for i := open + 1; i < closing; i += 2 {
		if isIdentKind(tokens[i].Kind) {
			out = append(out, tokens[i].Word)
		}
	}
	return out
}

// tokenEdits is a set of token edits keyed by token position, applied in one pass so earlier edits
// never shift later positions.
type tokenEdits struct {
	replaced map[int][]token
	inserted map[int][]token
	deleted  map[int]bool
}

func newTokenEdits() *tokenEdits {
	return &tokenEdits{replaced: map[int][]token{}, inserted: map[int][]token{}, deleted: map[int]bool{}}
}

func (e *tokenEdits) replace(pos int, with []token) { e.replaced[pos] = with }

func (e *tokenEdits) insertAfter(pos int, tokens []token) {
	e.inserted[pos] = append(e.inserted[pos], tokens...)
}

func (e *tokenEdits) delete(pos int) { e.deleted[pos] = true }

func (e *tokenEdits) apply(tokens []token) string {
	out := make([]token, 0, len(tokens))
	for i, t := range tokens {
		if t.Kind == tokEof {
			break
		}
		if !e.deleted[i] {
			if with, ok := e.replaced[i]; ok {
				out = append(out, with...)
			} else {
				out = append(out, t)
			}
		}
		out = append(out, e.inserted[i]...)
	}
	return renderTokens(out)
}
