package jed

import (
	"fmt"
	"slices"
	"sort"
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
	// stars is each written star's expansion, keyed by its token position.
	stars map[tokPos]traceStar
}

func newViewTrace() *viewTrace {
	return &viewTrace{
		items:       map[tokPos]traceItem{},
		catalogRels: map[string]bool{},
		columns:     map[traceColumn]bool{},
		stars:       map[tokPos]traceStar{},
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

// traceTableItem records a FROM item that binds a base table. A persistent view may read only the
// persistent relations of its own database: a temporary table (bare or temp.-qualified) or an
// attached database's table is 0A000 (views.md §10).
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
	if temp {
		return viewNotSupported(fmt.Sprintf("a view cannot read the temporary relation \"%s\"", tref.Name))
	}
	if isAttachmentScope(tref.DB) {
		return viewNotSupported(fmt.Sprintf("a view cannot read the attached relation \"%s.%s\"", *tref.DB, tref.Name))
	}
	db.viewTrace.items[tref.NamePos] = traceItem{
		kind: viewDepTable, name: table.Name,
		namePos: tref.NamePos, aliasPos: tref.AliasPos, listOpen: tref.ListOpen, listClose: tref.ListClose,
	}
	return nil
}

// inViewBody runs f as the planning of a view body (views.md §4.1): the trace of an enclosing view
// body is suspended (a nested view's own relations are not the outer view's dependencies), and the
// expansion nesting counts toward maxExprDepth (54001).
func (db *engine) inViewBody(f func() error) error {
	depth := db.viewDepth + 1
	if depth > maxExprDepth {
		return newError(StatementTooComplex, fmt.Sprintf(
			"statement too complex: nesting depth exceeds the maximum of %d", maxExprDepth))
	}
	saved := db.viewTrace
	db.viewTrace = nil
	db.viewDepth = depth
	err := f()
	db.viewDepth = depth - 1
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

// viewReference reports whether the FROM item tref names a view and, if so, returns the equivalent
// derived table `( <definition> ) AS <label> ( <columns> )` and the view's name. A name resolves to
// a statement-local CTE, then a built-in catalog relation, then a table, then a view (§4); tables
// and views share one namespace. A column list on the reference renames over the view's stored names
// (a longer list is left in place so the derived-table planning raises its 42P10). A view behind an
// attachment qualifier is rejected by the FROM build (§10), so only bare and main. names expand.
func (db *engine) viewReference(tref *tableRef, ctes []*cteBinding) (tableRef, string, bool) {
	if tref.IsFunc || tref.Subquery != nil || tref.Values != nil || tref.JsonTable != nil {
		return tableRef{}, "", false
	}
	if tref.DB != nil && *tref.DB != "main" {
		return tableRef{}, "", false
	}
	snap := db.readSnap()
	if len(snap.views) == 0 {
		return tableRef{}, "", false
	}
	view := snap.view(tref.Name)
	if view == nil {
		return tableRef{}, "", false
	}
	if tref.DB == nil {
		for _, b := range ctes {
			if b.name == tref.Name {
				return tableRef{}, "", false
			}
		}
		if _, ok := catalogRelKind(tref.Name); ok {
			return tableRef{}, "", false
		}
		if _, ok := db.lkpTable(tref.Name); ok {
			return tableRef{}, "", false
		}
	} else {
		if _, ok := catalogRelKind(tref.Name); ok {
			return tableRef{}, "", false
		}
		if _, ok := snap.table(tref.Name); ok {
			return tableRef{}, "", false
		}
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
	return derived, view.Name, true
}

// planViewReference plans a view reference's body (views.md §4.1): a fresh top-level scope with no
// correlation, no outer CTEs, and no parameters. While a view body is traced, the reference records
// the dependency on the view itself.
func (db *engine) planViewReference(viewName string, body *queryExpr, written *tableRef, ptypes *paramTypes) (queryPlan, error) {
	if db.viewTrace != nil {
		db.viewTrace.items[written.NamePos] = traceItem{
			kind: viewDepView, name: viewName,
			namePos: written.NamePos, aliasPos: written.AliasPos, listOpen: written.ListOpen, listClose: written.ListClose,
		}
	}
	var plan queryPlan
	err := db.inViewBody(func() error {
		var perr error
		plan, perr = db.planQuery(*body, nil, nil, ptypes)
		return perr
	})
	return plan, err
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

// checkViewWrite refuses a DML target that is a view with 55000 (views.md §2): every view is
// non-updatable in this slice. Checked when the target is resolved, before any column, value, or
// RETURNING check.
func (db *engine) checkViewWrite(q *string, name, verb string) error {
	var snap *snapshot
	if q == nil {
		if _, ok := db.lkpTable(name); ok {
			return nil
		}
		snap = db.readSnap()
	} else {
		snap = db.snapForScope(*q)
		if snap == nil {
			return nil
		}
	}
	if _, ok := snap.table(name); !ok && snap.view(name) != nil {
		return newError(ObjectNotInPrerequisiteState, fmt.Sprintf("cannot %s view \"%s\"", verb, name))
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

// isView reports whether name names a view of the main database (the relation namespace, views.md
// §2).
func (db *engine) isView(name string) bool { return db.readSnap().view(name) != nil }

// --- CREATE VIEW (views.md §1.1) -------------------------------------------------------------------

func (db *engine) executeCreateView(cv *createView) (outcome, error) {
	// 1. The database qualifier (the allow_ddl gate already ran in admission): main is the only
	//    database a view may live in this slice.
	if cv.DB != nil {
		switch *cv.DB {
		case "main":
		case "temp":
			return outcome{}, viewNotSupported("temporary views are not supported")
		default:
			if db.attachReadSnap(*cv.DB) == nil {
				return outcome{}, newError(UndefinedTable, `database "`+*cv.DB+`" is not attached`)
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
	body, err := parseViewDefinition(cv.Body)
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
	// 5. Column-list arity, then duplicate final output names, from the body as planned.
	bodyNames := append([]string(nil), plan.columnNames()...)
	bodyTypes := append([]resolvedType(nil), plan.columnTypes()...)
	columns, err := viewOutputNames(bodyNames, cv.Columns)
	if err != nil {
		return outcome{}, err
	}
	// 6. Freeze every expanded `*` and verify the expanded text plans to the same output.
	definition := cv.Body
	if len(trace.stars) > 0 {
		frozen := freezeStars(tokens, trace)
		notByName := viewNotSupported("a view's * expansion cannot be written by name")
		refrozen, perr := parseViewDefinition(frozen)
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
		definition, body, trace = frozen, refrozen, trace2
	}
	deps := db.traceDependencies(trace)
	// 7. The creating session's SELECT privilege on every relation the body reads (§9).
	if !db.session.privileges.IsPermissive() {
		var read []string
		for _, d := range deps {
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
				return outcome{}, newError(InsufficientPrivilege, "permission denied for table "+name)
			}
		}
	}
	// 8. A taken name.
	if db.relationExists(cv.Name) {
		return outcome{}, newError(DuplicateTable, fmt.Sprintf("relation \"%s\" already exists", cv.Name))
	}
	// 9. Write the catalog entry; it must fit one catalog page.
	def := &viewDef{Name: cv.Name, Columns: columns, Definition: definition, Deps: deps, Body: body}
	if err := db.checkViewEntryFits(def); err != nil {
		return outcome{}, err
	}
	db.working().putView(def)
	return outcome{Kind: outcomeStatement}, nil
}

// checkViewEntryFits requires a view's catalog entry to fit one catalog page (format.md *Catalog*): a
// longer definition is 0A000 (views.md §1.1 step 9).
func (db *engine) checkViewEntryFits(def *viewDef) error {
	pageSize := db.pageSize
	if pageSize == 0 {
		pageSize = DefaultPageSize
	}
	capacity := int(pageSize) - pageHeader
	if 1+len(viewEntryBytes(def)) > capacity {
		return viewNotSupported(fmt.Sprintf("the definition of view \"%s\" is too long", def.Name))
	}
	return nil
}

// traceDependencies is the persisted dependency list of a traced view body (views.md §6): every bound
// table (with its referenced columns, by current name) and view, in ascending name byte order.
func (db *engine) traceDependencies(trace *viewTrace) []viewDep {
	deps := map[string]*viewDep{}
	for _, item := range trace.items {
		if deps[item.name] == nil {
			deps[item.name] = &viewDep{Kind: item.kind, Name: item.name}
		}
	}
	snap := db.readSnap()
	for col := range trace.columns {
		table, ok := snap.table(col.table)
		if !ok || col.ordinal >= len(table.Columns) {
			continue
		}
		name := table.Columns[col.ordinal].Name
		dep := deps[col.table]
		if dep == nil {
			dep = &viewDep{Kind: viewDepTable, Name: col.table}
			deps[col.table] = dep
		}
		if !slices.Contains(dep.Columns, name) {
			dep.Columns = append(dep.Columns, name)
		}
	}
	names := make([]string, 0, len(deps))
	for name := range deps {
		names = append(names, name)
	}
	sort.Strings(names)
	out := make([]viewDep, 0, len(names))
	for _, name := range names {
		dep := *deps[name]
		sort.Strings(dep.Columns)
		out = append(out, dep)
	}
	return out
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
			case "temp":
				if db.isTempTable(name) || db.isTempSequence(name) {
					return outcome{}, newError(WrongObjectType, fmt.Sprintf("\"%s\" is not a view", name))
				}
				if dv.IfExists {
					continue
				}
				return outcome{}, newError(UndefinedTable, fmt.Sprintf("view \"temp.%s\" does not exist", name))
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
			return outcome{}, newError(WrongObjectType, fmt.Sprintf("\"%s\" is not a view", name))
		}
		if dv.IfExists {
			continue
		}
		return outcome{}, newError(UndefinedTable, fmt.Sprintf("view \"%s\" does not exist", name))
	}
	// Phase 2: a dependent view outside the drop set blocks (RESTRICT 2BP01; CASCADE would drop it —
	// a follow-on, 0A000). The first blocking view in name byte order is named.
	for _, dependent := range db.readSnap().viewsSorted() {
		if slices.Contains(targets, dependent.Name) {
			continue
		}
		for _, d := range dependent.Deps {
			if d.Kind != viewDepView || !slices.Contains(targets, d.Name) {
				continue
			}
			if dv.Cascade {
				return outcome{}, viewNotSupported(fmt.Sprintf(
					"DROP VIEW ... CASCADE of view %s with dependent view %s is not supported", d.Name, dependent.Name))
			}
			return outcome{}, newError(DependentObjectsStillExist, fmt.Sprintf(
				"cannot drop view %s because view %s depends on it", d.Name, dependent.Name))
		}
	}
	// Phase 3: remove.
	for _, name := range targets {
		db.working().removeView(name)
	}
	return outcome{Kind: outcomeStatement}, nil
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
	if len(db.readSnap().views) == 0 {
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
	pending := make([]string, 0, len(req.tables))
	for _, t := range req.tables {
		pending = append(pending, t.name)
	}
	return db.viewsCallSeqMutator(pending)
}

// viewsCallSeqMutator reports whether any of the views named in pending (of the main database) calls
// a sequence-mutating function in its body or in the body of any view it reads. The walk visits each
// view once, so it terminates whatever names a body holds.
func (db *engine) viewsCallSeqMutator(pending []string) bool {
	snap := db.readSnap()
	seen := map[string]bool{}
	for len(pending) > 0 {
		name := pending[len(pending)-1]
		pending = pending[:len(pending)-1]
		if seen[name] {
			continue
		}
		seen[name] = true
		view := snap.view(name)
		if view == nil {
			continue
		}
		if queryCallsSeqMutator(view.Body) {
			return true
		}
		var body privReq
		collectQueryPrivs(view.Body, &body, map[string]bool{})
		for _, t := range body.tables {
			pending = append(pending, t.name)
		}
	}
	return false
}

// collectViewFunctionPrivs adds the functions called in the bodies of the views a statement reads
// (transitively) to its privilege requirements (views.md §9): function EXECUTE is the reader's, as
// for a derived table. The relations inside a body are NOT added — the view is the privilege
// boundary.
func (db *engine) collectViewFunctionPrivs(req *privReq) {
	snap := db.readSnap()
	if len(snap.views) == 0 {
		return
	}
	var pending []string
	for _, t := range req.tables {
		pending = append(pending, t.name)
	}
	seen := map[string]bool{}
	for len(pending) > 0 {
		name := pending[len(pending)-1]
		pending = pending[:len(pending)-1]
		if seen[name] {
			continue
		}
		seen[name] = true
		view := snap.view(name)
		if view == nil {
			continue
		}
		var body privReq
		collectQueryPrivs(view.Body, &body, map[string]bool{})
		req.functions = append(req.functions, body.functions...)
		for _, t := range body.tables {
			pending = append(pending, t.name)
		}
	}
}

// --- Dependencies on tables (views.md §6) ----------------------------------------------------------

// checkDropTableViews is the DROP TABLE view-dependency check (§6): a view depending on a table being
// dropped blocks the statement — 2BP01 under RESTRICT, 0A000 under CASCADE (dropping dependent views
// is a follow-on). The first blocking view in name byte order is named.
func (db *engine) checkDropTableViews(targets []string, cascade bool) error {
	for _, view := range db.readSnap().viewsSorted() {
		for _, d := range view.Deps {
			if d.Kind != viewDepTable || !slices.Contains(targets, d.Name) {
				continue
			}
			if cascade {
				return viewNotSupported(fmt.Sprintf(
					"DROP TABLE ... CASCADE of table %s with dependent view %s is not supported", d.Name, view.Name))
			}
			return newError(DependentObjectsStillExist, fmt.Sprintf(
				"cannot drop table %s because view %s depends on it", d.Name, view.Name))
		}
	}
	return nil
}

// --- ALTER TABLE: rewrites (views.md §7) and re-validation (§8) -------------------------------------

// viewAlterChangeKind is what an ALTER TABLE statement changed that the dependent views' text must
// follow (§7).
type viewAlterChangeKind int

const (
	// viewAlterOther is every other action: dropped holds the original ordinals of the columns it
	// dropped (possibly none).
	viewAlterOther viewAlterChangeKind = iota
	// viewAlterRenameTable is RENAME TO newName.
	viewAlterRenameTable
	// viewAlterRenameColumn is RENAME COLUMN of the column at ordinal to newName.
	viewAlterRenameColumn
)

type viewAlterChange struct {
	kind    viewAlterChangeKind
	newName string
	ordinal int
	dropped []int
}

// viewAlterPlan is the dependent views of a table captured before an ALTER TABLE runs (§7/§8): each
// view with the FROM items that bind the table, as traced against the pre-statement catalog, and the
// table's pre-statement name and column names.
type viewAlterPlan struct {
	table   string
	columns []string
	views   []*viewDef
	items   [][]traceItem
}

// viewUsing returns the first dependent view (name byte order) that references the table's column at
// the pre-statement ordinal (§6), or "".
func (vp *viewAlterPlan) viewUsing(ordinal int) string {
	column := vp.columns[ordinal]
	for _, v := range vp.views {
		for _, d := range v.Deps {
			if d.Kind == viewDepTable && d.Name == vp.table && slices.Contains(d.Columns, column) {
				return v.Name
			}
		}
	}
	return ""
}

// alterBreaksView is the 0A000 an ALTER TABLE raises when a dependent view would not survive it (§8).
func alterBreaksView(table, view string) error {
	return viewNotSupported(fmt.Sprintf("cannot alter table %s because it would change the definition of view %s", table, view))
}

// prepareViewAlter captures the views that depend on table (of the main database) before an ALTER
// TABLE runs. When the action edits view text (rewrites), each view is planned against the
// pre-statement catalog to find the FROM items that bind the table; a view that cannot be planned
// fails the statement (0A000). nil when no view depends on the table.
func (db *engine) prepareViewAlter(table *catTable, rewrites bool) (*viewAlterPlan, error) {
	dependents := db.readSnap().viewsDependingOn(viewDepTable, table.Name)
	if len(dependents) == 0 {
		return nil, nil
	}
	vp := &viewAlterPlan{table: table.Name}
	for _, c := range table.Columns {
		vp.columns = append(vp.columns, c.Name)
	}
	for _, view := range dependents {
		var items []traceItem
		if rewrites {
			_, trace, err := db.traceViewBody(view.Body)
			if err != nil {
				return nil, alterBreaksView(table.Name, view.Name)
			}
			for _, item := range trace.items {
				if item.kind == viewDepTable && item.name == table.Name {
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
		body, err := parseViewDefinition(definition)
		if err != nil {
			return alterBreaksView(vp.table, view.Name)
		}
		bodies[i], expecteds[i] = body, expected
		if definition != view.Definition || !viewDepsEqual(expected, view.Deps) {
			updated := &viewDef{
				Name: view.Name, Columns: view.Columns, Definition: definition, Deps: expected, Body: body,
			}
			if err := db.checkViewEntryFits(updated); err != nil {
				return err
			}
			db.working().putView(updated)
		}
	}
	for i, view := range vp.views {
		_, trace, err := db.traceViewBody(bodies[i])
		if err != nil {
			return alterBreaksView(vp.table, view.Name)
		}
		if !viewDepsEqual(db.traceDependencies(trace), expecteds[i]) {
			return alterBreaksView(vp.table, view.Name)
		}
	}
	return nil
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
		if d.Kind == viewDepTable && d.Name == vp.table {
			switch change.kind {
			case viewAlterRenameTable:
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
	sort.SliceStable(out, func(i, j int) bool { return out[i].Name < out[j].Name })
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
		case viewAlterRenameTable:
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
