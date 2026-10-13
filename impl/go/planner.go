package jed

import (
	"errors"
	"fmt"
	"slices"
)

// SELECT planning, Stage 1 — the RESOLVE half (spec/design/planner.md §2): planSelect resolves a
// parsed SELECT into the logical selectPlan (FROM relations, join tree, WHERE/GROUP/HAVING,
// projections, ORDER BY, LIMIT/OFFSET) plus the touched-set annotation (computeRelMasks), then
// hands it to the physical/access-path selection pass (optimizeSelect, optimize.go) — resolve
// decides names/types/errors, never an access path. Query-level orchestration (SELECT/setop/CTE)
// is in plan_query.go; the access-path mechanisms are in access_path.go; SRFs are in srf.go.

// runSelect analyzes and runs a SELECT: resolve projected columns and the WHERE/ORDER BY columns
// against the catalog, scan the table in primary-key order, filter by the predicate (three-valued
// — only TRUE keeps a row), optionally re-sort by ORDER BY, then project. Rows are produced
// deterministically (CLAUDE.md §10). Returns the rows with each output column's NAME and resolved
// TYPE (the types let INSERT ... SELECT gate assignability up front — §24) and the accrued cost.
// planSelect resolves a SELECT into a *selectPlan against the scope chain (parent = the enclosing
// query's scope, for correlated references — grammar.md §26). The resolve half of the old
// runSelect: build the FROM scope, resolve every clause, infer $N types into ptypes. No row is
// touched and no parameter is bound here (runQueryExpr binds once, after the whole tree is planned).
func (db *engine) planSelect(sel *selectStmt, parent *scope, ctes []*cteBinding, ptypes *paramTypes) (*selectPlan, error) {
	return db.planSelectPushed(sel, parent, ctes, ptypes, nil)
}

// planSelectPushed is planSelect with stage-2 body-pushed conjuncts (spec/design/planner.md §3.2)
// appended to the resolved WHERE, in order, before the plan is rewritten and optimized: a derived
// body planned again by its enclosing SELECT. The conjuncts are already resolved against this
// SELECT's FROM slots.
func (db *engine) planSelectPushed(sel *selectStmt, parent *scope, ctes []*cteBinding, ptypes *paramTypes, pushed []*rExpr) (*selectPlan, error) {
	// Build the FROM scope: resolve each table reference (42P01 if unknown), compute each
	// relation's flat column offset in FROM order, and reject a duplicate label — a self-join
	// without distinct aliases is 42712 (spec/design/grammar.md §15). A FROM-less SELECT
	// (sel.From == nil) builds an EMPTY scope: nothing local resolves, so bare columns fall
	// through to `parent` (the correlated-subquery rule) or 42703 at top level
	// (spec/design/grammar.md §34). The scope links to `parent` (correlation) + the catalog
	// (so a subquery resolves its own FROM); allowSubquery is true.
	tableRefs := make([]tableRef, 0, 1+len(sel.Joins))
	if sel.From != nil {
		tableRefs = append(tableRefs, *sel.From)
	}
	for _, j := range sel.Joins {
		tableRefs = append(tableRefs, j.Table)
	}
	// A base name that resolves to a VIEW (after a CTE, a catalog relation, and a table —
	// spec/design/views.md §4) is the equivalent derived table `(<definition>) AS <label>
	// (<columns>)`, planned in the isolated view scope below. writtenRefs keeps the written items (their
	// token positions are what a traced view body records).
	writtenRefs := append([]tableRef(nil), tableRefs...)
	viewNames := make([]string, len(tableRefs))
	isolated := make([]*viewSite, len(tableRefs))
	for i := range tableRefs {
		if derived, name, site := db.viewReference(&tableRefs[i], ctes); site != nil {
			tableRefs[i] = derived
			viewNames[i] = name
			isolated[i] = site
		}
	}
	// A FROM item is a base table, a set-returning function (grammar.md §35), or a derived table
	// (§42). For a LATERAL item (§44) the body / SRF args resolve against the PREFIX of relations to
	// its left (a dependent join), so the build runs in FROM order and a prefix scope over the
	// already-resolved rels is handed to the body.
	var rels []scopeRel
	srfPlans := make([]*srfPlan, len(tableRefs))       // aligned with rels; nil = a base table
	derivedPlans := make([]*queryPlan, len(tableRefs)) // aligned with rels; non-nil = a derived table
	// lateralFlags[i] is true when FROM item i is a CORRELATED lateral relation (§44) — its body /
	// SRF args reference an earlier sibling (or an enclosing query), so the executor re-materializes
	// it per combined left-hand row. A non-correlated item (or the first item) is materialized once.
	lateralFlags := make([]bool, len(tableRefs))
	// shaped[i] is true when derivedPlans[i] holds only the body's SHAPE (planner.md §3.2), which
	// stage 2 reads and planDerivedBodies then replaces with the body's real plan. Only a SELECT with
	// a WHERE, an ON, or pushed conjuncts can push into a body, so only its bodies are shaped. Inside
	// a shape (db.planShape set) bodies are planned once and kept.
	shaping := db.planShape
	mayPush := sel.Filter != nil || len(pushed) > 0
	for _, j := range sel.Joins {
		mayPush = mayPush || j.On != nil
	}
	shapeBodies := !shaping && mayPush
	shaped := make([]bool, len(tableRefs))
	// cteBodies[i] is ci when FROM item i is a reference to CTE ci whose body it plans in place
	// (planner.md §3.4), else -1.
	cteBodies := make([]int, len(tableRefs))
	for i := range cteBodies {
		cteBodies[i] = -1
	}
	seenLabels := make(map[string]bool)
	offset := 0
	// The inner labels and hidden USING copies of each unaliased parenthesized join (§15).
	var subRels []subRel
	var groupHidden []int
	// A lateral fence (§15) is read only by the parenthesized-join body it was built for: any other
	// SELECT planned against one sees through it as the empty level it is.
	if parent != nil && parent.lateralView != nil && !sel.ParenJoin {
		parent = parent.withoutLateralView()
	}
	// The scope this FROM's LATERAL items and table functions resolve their left relations against:
	// parent, unless this is a parenthesized join's body planned against a fence — then the fence's
	// view, so they also see the relations to the join's left (§15).
	lateralParentScope := parent
	if parent != nil && parent.lateralView != nil {
		lateralParentScope = parent.lateralView
	}
	for i, tref := range tableRefs {
		var t *catTable
		var cteIdx *int
		// base is whether this relation is a catalog base table (possibly under a column alias list);
		// a traced view body records the columns resolved against it (views.md §6).
		base := false
		isDerived := tref.Subquery != nil || tref.Values != nil
		// A FROM item is lateral-ELIGIBLE when it can see earlier siblings: a derived table / VALUES
		// body explicitly marked LATERAL, or ANY table function (implicitly lateral — §44). The first
		// item (i == 0) has no earlier sibling, so it is never lateral; an SRF there resolves against
		// `parent` (the enclosing query) exactly as before.
		lateralEligible := i > 0 && ((isDerived && tref.Lateral) || tref.IsFunc || tref.JsonTable != nil)
		// The prefix scope a LATERAL item resolves against: the relations to its left, chained to the
		// enclosing query's parent (so a sibling column correlates as Outer{level=1}, an enclosing one
		// deeper). nil when not lateral-eligible.
		var lateralParent *scope
		if lateralEligible {
			lateralParent = &scope{rels: rels, parent: lateralParentScope, catalog: db, allowSubquery: true, ctes: ctes, subRels: subRels}
		}
		if isDerived {
			// Plan the body. LATERAL → parent is the prefix scope (a sibling/outer column correlates);
			// otherwise the ENCLOSING query's scope (§42): the body sees no sibling FROM item, but a
			// column of an enclosing query level correlates in the same frame as this query's own
			// outer references (PostgreSQL). A VALUES body resolves its values the same way.
			// A LATERAL first item sees no sibling, but inside a fenced parenthesized join it still sees
			// the relations to the join's left (§15).
			bodyParent := parent
			if lateralEligible {
				bodyParent = lateralParent
			} else if tref.Lateral {
				bodyParent = lateralParentScope
			}
			joinLateral := false
			//   A query body is first planned as a SHAPE (planner.md §3.2): stage 2 needs only its output
			// columns and pushability, and the body is planned for real once stage 2 knows what it pushes
			// into it. A correlated LATERAL body takes no pushdown, so it is planned for real right away,
			// against the same prefix scope.
			var plan queryPlan
			standIn, isStandIn := queryPlan{}, false
			if isolated[i] != nil && shaping {
				if _, err := db.viewSiteBase(*isolated[i]); err != nil {
					return nil, err
				}
				standIn, isStandIn = db.viewStandIn(viewNames[i])
			}
			if isStandIn {
				plan = standIn
			} else if isolated[i] != nil {
				err := db.planBodyFirst(shapeBodies, func() error {
					p, perr := db.planViewReference(viewNames[i], tref.Subquery, &writtenRefs[i], *isolated[i], ptypes)
					plan = p
					return perr
				})
				if err != nil {
					return nil, err
				}
			} else if i > 0 && parenJoinReachesLeft(&tref) {
				// A parenthesized join whose LATERAL items may reach the relations to its left (§15):
				// plan its body against a lateral fence — the left relations hidden from its ON clauses
				// and non-LATERAL items, visible to its LATERAL items. A first planning, as a shape,
				// learns whether any reference reached them. If so the join is a correlated lateral
				// relation, planned for real against the fence; otherwise it is planned as any other
				// derived body.
				prefix := &scope{rels: rels, parent: lateralParentScope, catalog: db, allowSubquery: true, ctes: ctes, subRels: subRels}
				fence := lateralFence(parent, prefix, db, ctes)
				err := db.planBodyFirst(true, func() error {
					_, perr := db.planQuery(*tref.Subquery, fence, ctes, ptypes)
					return perr
				})
				if err != nil {
					return nil, err
				}
				joinLateral = prefix.lateralHit
				err = db.planAgain(ctes, func() error {
					if joinLateral {
						p, perr := db.planQuery(*tref.Subquery, fence, ctes, ptypes)
						plan = p
						return perr
					}
					return db.planBodyFirst(shapeBodies, func() error {
						p, perr := db.planQuery(*tref.Subquery, parent, ctes, ptypes)
						plan = p
						return perr
					})
				})
				if err != nil {
					return nil, err
				}
			} else if tref.Subquery != nil {
				err := db.planBodyFirst(shapeBodies, func() error {
					p, perr := db.planQuery(*tref.Subquery, bodyParent, ctes, ptypes)
					plan = p
					return perr
				})
				if err != nil {
					return nil, err
				}
				if shapeBodies && lateralEligible && queryPlanReferencesOuter(&plan, 0) {
					err := db.planAgain(ctes, func() error {
						p, perr := db.planQuery(*tref.Subquery, bodyParent, ctes, ptypes)
						plan = p
						return perr
					})
					if err != nil {
						return nil, err
					}
				}
			} else {
				vp, verr := db.planValues(tref.Values, bodyParent, ctes, ptypes)
				if verr != nil {
					return nil, verr
				}
				plan = queryPlan{values: vp}
			}
			lateralFlags[i] = joinLateral || (lateralEligible && queryPlanReferencesOuter(&plan, 0))
			shaped[i] = shapeBodies && tref.Subquery != nil && !lateralFlags[i]
			label := ""
			if tref.Alias != nil {
				label = *tref.Alias
			}
			tbl, terr := cteSyntheticTable(label, &plan, tref.ColumnAliases)
			if terr != nil {
				return nil, terr
			}
			// An unaliased parenthesized join's columns are its inner columns, collation included.
			if group := derivedJoinGroup(&plan); group != nil {
				tbl = &catTable{Name: label, Columns: group.columns}
			}
			t = tbl
			derivedPlans[i] = &plan
		} else if tref.IsFunc {
			// A table function (SRF) — implicitly lateral. At i>0 its args resolve against the prefix
			// scope (a sibling column then correlates); at i==0 against `parent` (the enclosing query
			// / params — functions.md §10), or inside a fenced parenthesized join its lateral view (§15).
			srfParent := lateralParentScope
			if lateralEligible {
				srfParent = lateralParent
			}
			tbl, sp, serr := db.resolveSRF(tref.Name, tref.Args, tref.Alias, tref.ColumnDefs, srfParent, ctes, ptypes)
			if serr != nil {
				// A table function's own 42883 (no function for these argument types) points at its
				// name, like a call in an expression (spec/design/error-fields.md §10.2); an argument's
				// error keeps its own.
				var ee *EngineError
				if errors.As(serr, &ee) && ee.State == UndefinedFunction {
					serr = locateError(serr, tref.Src)
				}
				return nil, serr
			}
			relationName := tref.Name
			if tref.Alias != nil {
				relationName = *tref.Alias
			}
			if err := applySRFColumnAliases(tbl, relationName, tref.ColumnAliases); err != nil {
				return nil, err
			}
			t = tbl
			srfPlans[i] = sp
			if lateralEligible {
				for _, a := range sp.args {
					if rexprReferencesOuter(a, 0) {
						lateralFlags[i] = true
						break
					}
				}
			}
		} else if tref.JsonTable != nil {
			// A JSON_TABLE source (T1, json-table.md §3) — implicitly lateral like an SRF; its ctx
			// resolves against the prefix scope (so `JSON_TABLE(sibling.doc, …)` works), or `parent` at
			// i==0 (inside a fenced parenthesized join, its lateral view — §15).
			jtParent := lateralParentScope
			if lateralEligible {
				jtParent = lateralParent
			}
			tbl, sp, jerr := db.resolveJSONTable(tref.JsonTable, tref.Alias, jtParent, ctes, ptypes)
			if jerr != nil {
				return nil, jerr
			}
			t = tbl
			srfPlans[i] = sp
			if lateralEligible {
				for _, a := range sp.args {
					if rexprReferencesOuter(a, 0) {
						lateralFlags[i] = true
						break
					}
				}
			}
		} else if tref.DB != nil {
			// A database-QUALIFIED name reaches an attachment's table directly (attached-databases.md
			// §3): it never resolves to a CTE (a CTE has no database qualifier, so `main.x`/`temp.x`
			// cannot name one) and the qualifier fixes the scope (no temp-vs-persistent shadow).
			// A built-in catalog relation resolves in EVERY database's relation namespace
			// (temp.jed_tables, reports.jed_tables — introspection.md §5), before the user catalog;
			// only the qualifier itself needs validating.
			if kind, ok := catalogRelKind(tref.Name); ok {
				scope, serr := db.resolveCatalogScope(tref.DB)
				if serr != nil {
					return nil, serr
				}
				t = catalogRelTable(kind)
				if err := applySRFColumnAliases(t, tableRefLabel(&tref), tref.ColumnAliases); err != nil {
					return nil, err
				}
				if err := db.traceCatalogItem(&tref); err != nil {
					return nil, err
				}
				srfPlans[i] = &srfPlan{kind: kind, introspectScope: scope}
			} else {
				// A view reached through an attachment qualifier is a follow-on (views.md §10).
				if err := db.rejectAttachedView(tref.DB, tref.Name); err != nil {
					return nil, err
				}
				// Validate the qualifier against the implicit scope, then resolve through the temp-first
				// funnel (which, by preclude-overlaps, lands in the validated scope).
				if err := db.checkTableQualifier(tref.DB, tref.Name); err != nil {
					// An unknown qualified relation is positioned at the name (error-fields.md §10.2).
					var e *EngineError
					if errors.As(err, &e) && e.State == UndefinedTable {
						e.withPosition(int32(tref.Src))
					}
					return nil, err
				}
				// Route to the qualified database's catalog (attached-databases.md §3): main/temp fall through
				// to the temp-first funnel (preclude-overlaps lands them in the validated scope), a host
				// attachment resolves in its own snapshot — where its table lives ONLY.
				tbl, ok := db.lkpTableScoped(tref.DB, tref.Name)
				if !ok {
					return nil, newError(UndefinedTable, "table does not exist: "+*tref.DB+"."+tref.Name).
						withPosition(int32(tref.Src))
				}
				if err := db.traceTableItem(&tref, tbl); err != nil {
					return nil, err
				}
				base = true
				if tref.ColumnAliases != nil {
					var rerr error
					if tbl, rerr = renamedBaseTable(tbl, &tref, tref.ColumnAliases); rerr != nil {
						return nil, rerr
					}
				}
				t = tbl
			}
		} else {
			// A plain FROM name (not an SRF call) may resolve to a CTE, which SHADOWS a catalog
			// table of the same name (cte.md §2); lookup is byte-exact. A hit bumps the
			// binding's reference count (the inline-vs-materialize decision — cost.md §3).
			lname := tref.Name
			ci := -1
			for j := len(ctes) - 1; j >= 0; j-- {
				if ctes[j].name == lname {
					ci = j
					break
				}
			}
			if ci >= 0 {
				// A data-modifying CTE with no RETURNING produces no columns, so a FROM reference to
				// it is 0A000 (writable-cte.md §5; PostgreSQL's addRangeTableEntryForCTE check), raised
				// at resolution before any execution.
				if ctes[ci].dm != nil && ctes[ci].dm.noReturning {
					return nil, newError(FeatureNotSupported,
						"WITH query "+lname+" does not have a RETURNING clause")
				}
				ctes[ci].refs++
				// A CTE inlined at its reference plans its body here, like a derived table (planner.md
				// §3.4) — once per statement, never inside a shape or a definition planned with
				// expansion off. Only the earlier bindings are in its scope, and it is uncorrelated,
				// so it is planned against no parent.
				if ctes[ci].atReference && !shaping && !db.cteExpansionOff && !ctes[ci].expanded {
					ctes[ci].expanded = true
					earlier := ctes[:ci:ci]
					var body queryPlan
					err := db.planBodyFirst(shapeBodies, func() error {
						return db.planAgain(ctes, func() error {
							p, perr := db.planQuery(*ctes[ci].bodyAst, nil, earlier, ptypes)
							body = p
							return perr
						})
					})
					if err != nil {
						return nil, err
					}
					derivedPlans[i] = &body
					shaped[i] = shapeBodies
					cteBodies[i] = ci
				}
				idx := ci
				cteIdx = &idx
				t = ctes[ci].table
				if tref.ColumnAliases != nil {
					var rerr error
					if t, rerr = renamedBaseTable(t, &tref, tref.ColumnAliases); rerr != nil {
						return nil, rerr
					}
				}
			} else if kind, ok := catalogRelKind(tref.Name); ok {
				// A built-in catalog relation (introspection.md §5), checked AFTER a CTE (a CTE
				// shadows it — PG-matching) and BEFORE the user catalog. Unqualified = the implicit
				// scope (main).
				t = catalogRelTable(kind)
				if err := applySRFColumnAliases(t, tableRefLabel(&tref), tref.ColumnAliases); err != nil {
					return nil, err
				}
				if err := db.traceCatalogItem(&tref); err != nil {
					return nil, err
				}
				srfPlans[i] = &srfPlan{kind: kind, introspectScope: "main"}
			} else {
				tbl, ok := db.lkpTable(tref.Name) // temp-first (temp-tables.md §3)
				if !ok {
					return nil, newError(UndefinedTable, "table does not exist: "+tref.Name).
						withPosition(int32(tref.Src))
				}
				if err := db.traceTableItem(&tref, tbl); err != nil {
					return nil, err
				}
				base = true
				// A column alias list relabels the table's columns (grammar.md §15): a copy read
				// from the table's own store.
				if tref.ColumnAliases != nil {
					var rerr error
					if tbl, rerr = renamedBaseTable(tbl, &tref, tref.ColumnAliases); rerr != nil {
						return nil, rerr
					}
				}
				t = tbl
			}
		}
		// RIGHT/FULL JOIN to a CORRELATED lateral item is rejected (§44): the right side cannot be both
		// kept whole and evaluated per left row. (i ≥ 1 here, so the item carries a join kind.)
		if lateralFlags[i] && (sel.Joins[i-1].Kind == joinRight || sel.Joins[i-1].Kind == joinFull) {
			return nil, newError(InvalidColumnReference,
				"invalid reference to FROM-clause entry for a LATERAL item: the combining JOIN type must be INNER or LEFT")
		}
		label := t.Name
		if tref.Alias != nil {
			label = *tref.Alias
		}
		// An unaliased derived table (grammar.md §42, PG 18) has an EMPTY label — it has no
		// qualifier, so two of them never collide and the duplicate-label check is skipped (its bare
		// columns still resolve, and stay ambiguous via resolveBare). Every other relation has a
		// non-empty label (a table/function name or an explicit alias).
		labels := []string{label}
		// An unaliased parenthesized join (§15) is one relation with no label of its own, whose inner
		// labels stay visible — and so take part in the duplicate-label check.
		if group := derivedJoinGroup(derivedPlans[i]); group != nil {
			for _, sr := range group.subRels {
				slots := make([]int, len(sr.slots))
				for c, slot := range sr.slots {
					slots[c] = offset + slot
				}
				subRels = append(subRels, subRel{label: sr.label, table: sr.table, slots: slots, rel: i})
				labels = append(labels, sr.label)
			}
			for _, h := range group.hidden {
				groupHidden = append(groupHidden, offset+h)
			}
		}
		for _, label := range labels {
			if label == "" {
				continue
			}
			if seenLabels[label] {
				return nil, newError(DuplicateAlias, "table name "+label+" specified more than once")
			}
			seenLabels[label] = true
		}
		rels = append(rels, scopeRel{label: label, table: t, offset: offset, cte: cteIdx, db: tref.DB, base: base, view: viewNames[i]})
		offset += len(t.Columns)
	}

	// USING/NATURAL merged columns + every join's resolved predicate (grammar.md §15) — computed
	// BEFORE the scope so GROUP BY / DISTINCT / projection / WHERE all see the merge columns; a plain
	// ON join resolves here too. Joins are processed left-to-right so a later join's left side sees
	// the merges introduced by earlier ones (a USING chain). For each USING column the synthesized
	// predicate is `left.col = right.col` (3-valued, like any ON); the SURVIVING side becomes the
	// single merge column — the left for INNER/LEFT, the right for RIGHT (FULL JOIN USING, a COALESCE,
	// is 0A000). Both copies are hidden from `*`. Merges/predicates respect the comma SEGMENT (commit 1).
	var merges []mergeCol
	hidden := groupHidden
	// The `*` expansion order is built per comma item, PostgreSQL's rule (grammar.md §15): each join's
	// output is (its merged columns, in USING order) ++ (the left input's columns minus those merged
	// here) ++ (the right input's columns minus its merged copies), so in a left-deep chain the NEWEST
	// join's merges come first. star holds the completed comma items; segStar the current one.
	flat := &scope{rels: rels}
	relStar := func(i int) []int {
		var out []int
		for c := range rels[i].table.Columns {
			if idx := rels[i].offset + c; !slices.Contains(groupHidden, idx) {
				out = append(out, idx)
			}
		}
		return out
	}
	var star, segStar []int
	if len(rels) > 0 {
		segStar = relStar(0)
	}
	joinPreds := make([]*rExpr, len(sel.Joins))
	for k := range sel.Joins {
		j := &sel.Joins[k]
		if j.Comma {
			star = append(star, segStar...)
			segStar = relStar(k + 1)
			continue
		}
		seg := k + 1
		for seg >= 1 && !sel.Joins[seg-1].Comma {
			seg--
		}
		segOff := rels[seg].offset
		var segMerges []mergeCol
		for _, m := range merges {
			if m.index >= segOff {
				segMerges = append(segMerges, m)
			}
		}
		var segHidden []int
		for _, i := range hidden {
			if i >= segOff {
				segHidden = append(segHidden, i)
			}
		}
		// A NATURAL join (grammar.md §15) derives its USING list as the column names common to both
		// sides (left order); an explicit USING uses its written list. A NATURAL join with NO common
		// column degenerates to a CROSS join (an empty list → no predicate, no merge).
		var usingCols []string
		if j.Using != nil {
			usingCols = j.Using
		} else if j.Natural {
			leftNames := make([]string, len(segStar))
			for n, idx := range segStar {
				leftNames[n] = flat.columnAt(idx).Name
			}
			usingCols = naturalCommonCols(leftNames, &rels[k+1])
		}
		rightStar := relStar(k + 1)
		switch {
		case len(usingCols) > 0:
			if j.Kind == joinFull {
				return nil, newError(FeatureNotSupported, "FULL JOIN with a merged (USING/NATURAL) condition is not supported yet")
			}
			left := &scope{rels: rels[seg : k+1], parent: parent, catalog: db, allowSubquery: true, ctes: ctes, merges: segMerges, hidden: segHidden}
			// The synthesized predicate compares the two chosen columns through one-column pseudo
			// relations at their flat slots, so it binds exactly those columns even when a side is an
			// unlabeled relation or carries a second column of the same name.
			var pairRels []scopeRel
			var predAST *exprNode
			var merged, leftDrop, rightDrop []int
			for n, name := range usingCols {
				lr, lerr := left.resolveBareColumn(name)
				if lerr != nil || lr.level != 0 {
					return nil, newError(UndefinedColumn, "column \""+name+"\" specified in USING clause does not exist in left table")
				}
				li := lr.index
				rightRel := &rels[k+1]
				rl := rightRel.table.ColumnIndex(name)
				if rl < 0 {
					return nil, newError(UndefinedColumn, "column \""+name+"\" specified in USING clause does not exist in right table")
				}
				db.traceRelColumn(rightRel, rl)
				ri := rightRel.offset + rl
				lq, rq := fmt.Sprintf("\x00l%d", n), fmt.Sprintf("\x00r%d", n)
				pairRels = append(pairRels,
					scopeRel{label: lq, table: &catTable{Name: lq, Columns: []catColumn{*left.columnAt(li)}}, offset: li},
					scopeRel{label: rq, table: &catTable{Name: rq, Columns: []catColumn{rightRel.table.Columns[rl]}}, offset: ri})
				eq := newBinaryExpr(opEq,
					exprNode{Kind: exprQualifiedColumn, Qualifier: lq, Column: name},
					exprNode{Kind: exprQualifiedColumn, Qualifier: rq, Column: name}, 0)
				if predAST == nil {
					predAST = &eq
				} else {
					a := newBinaryExpr(opAnd, *predAST, eq, 0)
					predAST = &a
				}
				mi := li
				if j.Kind == joinRight {
					mi = ri
				}
				merges = slices.DeleteFunc(merges, func(m mergeCol) bool { return m.name == name })
				merges = append(merges, mergeCol{name: name, index: mi})
				hidden = append(hidden, li, ri)
				merged, leftDrop, rightDrop = append(merged, mi), append(leftDrop, li), append(rightDrop, ri)
			}
			for _, idx := range segStar {
				if !slices.Contains(leftDrop, idx) {
					merged = append(merged, idx)
				}
			}
			for _, idx := range rightStar {
				if !slices.Contains(rightDrop, idx) {
					merged = append(merged, idx)
				}
			}
			segStar = merged
			pair := &scope{rels: pairRels, catalog: db, ctes: ctes}
			pred, perr := resolveBooleanFilter(pair, predAST, ptypes)
			if perr != nil {
				return nil, perr
			}
			joinPreds[k] = pred
		case j.On != nil:
			partial := &scope{rels: rels[seg : k+2], parent: parent, catalog: db, allowSubquery: true, ctes: ctes, merges: segMerges, hidden: segHidden, subRels: subRelsWithin(subRels, seg, k+1)}
			pred, perr := resolveBooleanFilter(partial, j.On, ptypes)
			if perr != nil {
				return nil, perr
			}
			joinPreds[k] = pred
			segStar = append(segStar, rightStar...)
		default:
			segStar = append(segStar, rightStar...)
		}
	}
	star = append(star, segStar...)

	s := &scope{rels: rels, parent: parent, catalog: db, allowSubquery: true, ctes: ctes, merges: merges, hidden: hidden, subRels: subRels, star: star}

	// Resolve projections (paired with output names — §8), the optional WHERE (must be
	// boolean), and the ORDER BY keys against the full scope. A bare key ambiguous across
	// relations is 42702; an unknown qualifier is 42P01 (§15).
	// Resolve GROUP BY keys to flat row indices (a key is a bare/qualified column — grammar.md
	// §18). An unknown column is 42703, an ambiguous bare key 42702.
	var err error
	// Expand GROUP BY (including ROLLUP / CUBE / GROUPING SETS) into a list of grouping sets, resolve
	// each set's columns to flat row indices, and build the master grouping-column list (groupKeys) —
	// the ordered union of every set's columns, i.e. the columns groupable in at least one set
	// (spec/design/aggregates.md §12). A plain GROUP BY a, b expands to a single set [a, b]; no GROUP
	// BY expands to a single empty set (the whole-table grand total). An unknown column is 42703.
	// Each grouping term is one of (aggregates.md §15): a bare/qualified COLUMN; a select-list ORDINAL
	// (a bare integer literal — `GROUP BY 1`); an output ALIAS (a bare name that is not an input
	// column — PG's input-column-first rule); or a general EXPRESSION (`GROUP BY a+b`). A column key
	// keeps its real row slot (groupKeys holds its flat index); an expression key is MATERIALIZED —
	// its node collected into groupExprs and evaluated per row into a synthetic column inputWidth+k
	// whose index is the master key. groupKeyExprs records each master key's canonical AST (set for
	// expression keys) so a matching projection / HAVING / ORDER BY expression resolves to its
	// synthetic slot. The whole-row equality bucket machinery (resolvedSets, GROUPING SETS) is
	// unchanged — it works on master key indices.
	expanded, err := expandGroupBy(sel.GroupBy)
	if err != nil {
		return nil, err
	}
	inputWidth := s.width()
	groupKeys := make([]int, 0)
	groupKeyExprs := make([]*groupKeyExpr, 0)
	groupExprs := make([]*rExpr, 0)
	resolvedSets := make([][]int, 0, len(expanded))
	for _, set := range expanded {
		idxs := make([]int, 0, len(set))
		for _, key := range set {
			gr, gerr := resolveGroupTerm(s, *key, sel.Items, ptypes)
			if gerr != nil {
				return nil, gerr
			}
			var idx int
			if gr.isColumn {
				// `json` has no equality operator (PG ships no hash/btree opclass — spec/design/json.md
				// §5), so GROUP BY a json column is 42883. jsonb IS groupable.
				if s.columnAt(gr.index).Type.IsJson() {
					return nil, newError(UndefinedFunction, "could not identify an equality operator for type json")
				}
				idx = gr.index
				found := false
				for _, gk := range groupKeys {
					if gk == idx {
						found = true
						break
					}
				}
				if !found {
					groupKeys = append(groupKeys, idx)
					groupKeyExprs = append(groupKeyExprs, nil)
				}
			} else {
				// Grouping is byte equality, but an indeterminate key is still 42P21
				// (spec/design/collation.md §1 "A sort or grouping target").
				if gr.ty.collatable() {
					d, derr := deriveCollation(s, gr.canon)
					if derr != nil {
						return nil, derr
					}
					if err := rejectIndeterminateTarget(d); err != nil {
						return nil, err
					}
				}
				if gr.ty.kind == rtJson {
					return nil, newError(UndefinedFunction, "could not identify an equality operator for type json")
				}
				// Reuse an identical expression key already registered (`GROUP BY a+b, a+b`).
				pos := -1
				for p, gk := range groupKeyExprs {
					if gk != nil && exprEqual(gk.canon, gr.canon) {
						pos = p
						break
					}
				}
				if pos >= 0 {
					idx = groupKeys[pos]
				} else {
					synth := inputWidth + len(groupExprs)
					groupExprs = append(groupExprs, gr.node)
					groupKeys = append(groupKeys, synth)
					groupKeyExprs = append(groupKeyExprs, &groupKeyExpr{canon: gr.canon, ty: gr.ty})
					idx = synth
				}
			}
			idxs = append(idxs, idx)
		}
		resolvedSets = append(resolvedSets, idxs)
	}

	// Functional-dependency grouping (aggregates.md §16, PG): when there is a SINGLE grouping set
	// that contains every primary-key column of a base table T, T's PK functionally determines every
	// column of T, so any T column (and expressions over them) may appear ungrouped. Make them
	// groupable by adding T's remaining columns as extra master grouping keys — the grouping is
	// UNCHANGED (each is constant within a group, so bucketing by [pk…, others…] yields the same
	// partition as by [pk…] alone, even across a join). Restricted to a single set: PG rejects the
	// dependency when a grouping set omits the PK. A CTE / derived table / SRF has an empty PK (a
	// synthetic key), so only base tables with a real PK contribute.
	if len(resolvedSets) == 1 {
		var extra []int
		for ri := range s.rels {
			rel := &s.rels[ri]
			if rel.qualifierOnly || rel.cte != nil || len(rel.table.PK) == 0 {
				continue
			}
			pkGrouped := true
			for _, ord := range rel.table.PK {
				if !slices.Contains(groupKeys, rel.offset+ord) {
					pkGrouped = false
					break
				}
			}
			if !pkGrouped {
				continue
			}
			for c := range rel.table.Columns {
				idx := rel.offset + c
				if !slices.Contains(groupKeys, idx) && !slices.Contains(extra, idx) {
					extra = append(extra, idx)
				}
			}
		}
		for _, idx := range extra {
			groupKeys = append(groupKeys, idx)
			groupKeyExprs = append(groupKeyExprs, nil)
			resolvedSets[0] = append(resolvedSets[0], idx)
		}
	}

	// An aggregate query has a GROUP BY or an aggregate in the select list. Its projection
	// resolves in collect mode — aggregates collect into synthetic slots and a non-grouped
	// column is 42803 (spec/design/aggregates.md §4/§6); a plain query resolves in Forbidden
	// mode (columns normal). Output names per grammar.md §8.
	// GROUP BY, an aggregate in the select list, OR a HAVING clause all make this an aggregate
	// query (HAVING alone groups the whole table — grammar.md §19). An aggregate inside a window
	// definition's keys also does — inline (`OVER (ORDER BY sum(x))`, caught by itemsHaveAggregate)
	// or in a WINDOW-clause entry (`WINDOW w AS (ORDER BY sum(x))`, scanned here before the desugar).
	// Note len(sel.GroupBy) (not groupKeys): GROUP BY GROUPING SETS (()) has an empty master list yet
	// is still an aggregate query (the whole-table grand total).
	isAgg := len(sel.GroupBy) > 0 || itemsHaveAggregate(sel.Items) || sel.Having != nil ||
		windowsHaveAggregate(sel.Windows)
	// A window query (a select-list OVER call) resolves its projection in window mode, where bare
	// columns read the input/grouped row and window calls collect into synthetic slots
	// (spec/design/window.md §5.1). A grouped query that ALSO windows is both collecting and
	// windowing (the window stage runs over the grouped rows — §2); a plain window query is only
	// windowing.
	// A window function may appear in the SELECT list OR in an ORDER BY key (grammar.md §10): either
	// sets up the window machinery so the key can be sorted by the computed window value.
	hasWindowSyntax := itemsHaveWindow(sel.Items) || orderByHasWindow(sel.OrderBy)
	projAgg := &aggCtx{collecting: isAgg, groupKeys: groupKeys, groupKeyExprs: groupKeyExprs}
	if hasWindowSyntax {
		projAgg.windowing = true
		// Window results land AFTER the materialized window keys, and (for a grouped query) after
		// every aggregate — neither final count is known until resolution finishes (an aggregate may
		// be nested in a later window argument or in HAVING). So a window result carries the
		// PLACEHOLDER base windowResultBase, rebased afterwards to inputWidth+len(windowKeys)+w
		// (window.md §5.1). A materialized window key carries windowKeyBase+k, rebased to inputWidth+k.
		projAgg.windowBase = windowResultBase
	}
	// Resolve the WINDOW clause: an entry may extend an earlier entry (`w2 AS (w ORDER BY …)` —
	// window.md §5), so each is merged against the already-resolved earlier entries (a missing/
	// forward/self base is 42704; PARTITION/ORDER overrides and a framed base are 42P20). Every
	// entry is resolved, even unreferenced ones, matching PostgreSQL. The result is all-inline
	// (Base == "") definitions the desugar pass copies/extends from.
	windowsResolved := sel.Windows
	if len(sel.Windows) > 0 {
		windowsResolved, err = resolveWindowClause(sel.Windows)
		if err != nil {
			return nil, err
		}
	}
	// Desugar `OVER name` / `OVER (base …)` references to their WINDOW-clause definitions before
	// resolution (window.md §5). The projection resolves against the desugared items; a reference to
	// an undefined window is 42704. A plain query with no window clause/refs uses sel.Items unchanged.
	items := sel.Items
	if hasWindowSyntax {
		items, err = desugarItems(sel.Items, windowsResolved)
		if err != nil {
			return nil, err
		}
	}
	projections, columnNames, columnTypes, columnDerivs, columnTypmods, err := resolveProjections(s, items, projAgg, ptypes)
	if err != nil {
		return nil, err
	}
	var group *joinGroupLayout
	if sel.JoinGroup {
		group = joinGroupOutputs(s, &projections, &columnNames, &columnTypes, &columnDerivs, &columnTypmods)
	}
	aggSpecs := projAgg.specs
	windowSpecs := projAgg.windowSpecs
	windowKeys := projAgg.windowKeys
	groupingSpecs := projAgg.groupingSpecs
	hasWindow := len(windowSpecs) > 0
	// SELECT DISTINCT dedups the projected rows by equality, but `json` has no equality operator
	// (PG ships no opclass — spec/design/json.md §5), so a json output column under DISTINCT is
	// 42883. jsonb IS distinguishable (its btree equality, §5).
	if sel.Distinct {
		for _, t := range columnTypes {
			if t.kind == rtJson {
				return nil, newError(UndefinedFunction, "could not identify an equality operator for type json")
			}
		}
		// Each DISTINCT output column is a grouping target: an indeterminate one is 42P21
		// (spec/design/collation.md §1 "A sort or grouping target").
		for _, d := range columnDerivs {
			if err := rejectIndeterminateTarget(d); err != nil {
				return nil, err
			}
		}
	}
	// HAVING resolves in collect mode with window functions FORBIDDEN (42P20 — HAVING runs BEFORE the
	// window stage, window.md §7), continuing the aggregate specs (and GROUPING() calls) so they slot
	// after the projection's. It must be boolean (42804). A HAVING aggregate, like a projection one, is
	// part of the grouped row, so the window slots that follow are rebased over the final aggregate count.
	var having *rExpr
	if sel.Having != nil {
		hctx := &aggCtx{collecting: true, groupKeys: groupKeys, groupKeyExprs: groupKeyExprs, specs: aggSpecs, groupingSpecs: groupingSpecs}
		node, ty, herr := resolve(s, *sel.Having, nil, hctx, ptypes)
		if herr != nil {
			return nil, herr
		}
		if ty.kind != rtBool && ty.kind != rtNull {
			return nil, positionError(typeError("argument of HAVING must be boolean"), exprLocation(*sel.Having))
		}
		having = node
		aggSpecs = hctx.specs
		groupingSpecs = hctx.groupingSpecs
	}
	// (The window / GROUPING() placeholder rebases run AFTER the ORDER BY resolution below, because an
	// ORDER BY key may itself introduce a window function / aggregate / GROUPING() — so the final spec
	// counts, and thus every placeholder's real slot, are not known until ORDER BY is resolved.)
	// Build the grouping sets (spec/design/aggregates.md §12). For an aggregate query with no GROUP BY
	// this is the single empty (whole-table) set; otherwise one entry per resolved set, each recording
	// its bucket key columns, the per-master-slot value source (or -1 = NULL), and the GROUPING() mask.
	var groupSets []groupSetPlan
	if isAgg {
		groupSets = make([]groupSetPlan, 0, len(resolvedSets))
		for _, set := range resolvedSets {
			slotSrc := make([]int, len(groupKeys))
			for p := range slotSrc {
				slotSrc[p] = -1
			}
			for j, fidx := range set {
				for p, gk := range groupKeys {
					if gk == fidx {
						slotSrc[p] = j
						break
					}
				}
			}
			var mask int64
			for p, src := range slotSrc {
				if src < 0 {
					mask |= int64(1) << uint(p)
				}
			}
			keyCols := make([]int, len(set))
			copy(keyCols, set)
			groupSets = append(groupSets, groupSetPlan{keyCols: keyCols, slotSrc: slotSrc, mask: mask})
		}
	}
	// (The GROUPING SETS/window mutual-exclusion check and the GROUPING() placeholder rebase also run
	// after the ORDER BY resolution below — an ORDER BY GROUPING() grows groupingSpecs.)
	// SELECT DISTINCT over an aggregate query's output (output-row dedup) dedups the projected
	// group rows by equality, keeping the first occurrence, then LIMIT/OFFSET (aggregates.md §10) —
	// the same project->dedup->window pipeline as the non-aggregate DISTINCT path. The ORDER BY
	// restriction (each key must be a select-list item) is enforced once for both at the §11 block.
	var filter *rExpr
	if sel.Filter != nil {
		filter, err = resolveBooleanFilter(s, sel.Filter, ptypes)
		if err != nil {
			return nil, err
		}
	}
	// ORDER BY resolution. In an aggregate query a key resolves against the GROUP KEYS — a
	// grouping column gives its synthetic-row slot, a non-grouping column is 42803 (the
	// grouping-error rule, grammar.md §18); the sort runs on the group rows. In a plain query
	// keys resolve against the FROM scope (a flat row index). An outer (correlated) ORDER BY key
	// — ordering by an enclosing-query constant — is degenerate and 0A000 (§26).
	// ORDER BY resolution (spec/design/grammar.md §10). Each key is one of three modes (set at parse):
	// an output-column ORDINAL, a COLUMN reference, or a general EXPRESSION. A column / ordinal-to-column
	// key resolves to a real row slot (against the GROUP KEYS in an aggregate query — a grouping column
	// gives its synthetic slot, a non-grouping column is 42803; else against the FROM scope). A general-
	// expression key (and an ordinal pointing at a COMPUTED select-list item) is MATERIALIZED: its
	// expression is resolved here (introducing a new aggregate in a grouped query if it names one),
	// collected into orderExprs, and given a placeholder sort slot orderExprBase+k rebased to
	// final_width+k below — the window-key precedent (window.md §5.1).
	order := make([]orderSlot, 0, len(sel.OrderBy))
	var orderExprs []*rExpr
	for _, key := range sel.OrderBy {
		// Classify the key into a row slot (a column / ordinal-to-column) or a source expression (a
		// general expression, or an ordinal pointing at a computed projection).
		var slotRes resolved
		var orderExpr *exprNode
		if key.Ordinal != nil {
			ord := *key.Ordinal
			var ncols int64
			if items.All {
				ncols = int64(s.width())
			} else {
				ncols = int64(len(items.Items))
			}
			if ord < 1 || ord > ncols {
				return nil, newError(InvalidColumnReference,
					fmt.Sprintf("ORDER BY position %d is not in select list", ord))
			}
			pos := int(ord - 1)
			if items.All {
				slotRes = resolved{level: 0, index: pos}
			} else {
				switch e := items.Items[pos].Expr; e.Kind {
				case exprColumn:
					if slotRes, err = s.resolveBare(e.Column); err != nil {
						return nil, err
					}
				case exprQualifiedColumn:
					if slotRes, err = s.resolveQualified(e.Qualifier, e.Column); err != nil {
						return nil, err
					}
				default:
					orderExpr = &items.Items[pos].Expr
				}
			}
		} else if key.Expr != nil {
			orderExpr = key.Expr
		} else if key.Qualifier != "" {
			// A qualified key (`t.a`) is always an input column — never an output alias (PG; §10).
			if slotRes, err = s.resolveQualifiedAt(key.Qualifier, key.Column, key.Pos); err != nil {
				return nil, err
			}
		} else {
			// A bare name resolves an OUTPUT column (an AS alias or item's derived name) BEFORE an input
			// column — PostgreSQL's SQL92 rule (grammar.md §10). A match routes the item EXACTLY like the
			// same ORDER BY ordinal; no match falls through to the FROM scope (the prior behavior).
			matched, merr := orderAliasMatch(items, key.Column, s)
			if merr != nil {
				return nil, merr
			}
			switch {
			case matched == nil:
				if slotRes, err = s.resolveBareAt(key.Column, key.Pos); err != nil {
					return nil, err
				}
			case matched.Kind == exprColumn:
				if slotRes, err = s.resolveBare(matched.Column); err != nil {
					return nil, err
				}
			case matched.Kind == exprQualifiedColumn:
				if slotRes, err = s.resolveQualified(matched.Qualifier, matched.Column); err != nil {
					return nil, err
				}
			default:
				orderExpr = matched
			}
		}

		if orderExpr == nil {
			// A column / ordinal-to-column key resolves to a real row slot.
			r := slotRes
			if r.level != 0 {
				// A correlated (outer) column ORDER BY key — the local sort row has no slot for an
				// enclosing-query column, so materialize it as an OuterColumn expression evaluated per row
				// against the outer-row environment (query.order_by_correlated), exactly like a general-
				// expression key. PostgreSQL accepts it (a degenerate constant leading key).
				rexpr, ty, rerr := resolveColumnRef(s, &aggCtx{}, r, key.Column)
				if rerr != nil {
					return nil, rerr
				}
				if ty.kind == rtJson {
					return nil, newError(UndefinedFunction, "could not identify an ordering operator for type json")
				}
				var coll *Collation
				if key.Collation != "" {
					if !ty.collatable() && ty.kind != rtNull {
						return nil, typeError(fmt.Sprintf("collations are not supported by type %s", rtName(ty)))
					}
					if coll, err = resolveCollationName(s.catalog, key.Collation); err != nil {
						return nil, err
					}
				} else if coll, err = columnCollation(s, r); err != nil {
					return nil, err
				}
				k := len(orderExprs)
				orderExprs = append(orderExprs, rexpr)
				order = append(order, orderSlot{idx: orderExprBase + k, descending: key.Descending, nullsFirst: key.NullsFirst, collation: coll})
				continue
			}
			// `json` has no ordering operator (PG ships no btree opclass — spec/design/json.md §5):
			// ORDER BY a json column is 42883. jsonb IS orderable (its btree total order, §5).
			if s.columnOf(r).Type.IsJson() {
				return nil, newError(UndefinedFunction, "could not identify an ordering operator for type json")
			}
			idx := r.index
			// The sort key's collation (spec/design/collation.md §1/§7). An explicit COLLATE must be on a
			// text column (42804) and name a loaded collation ("C" → byte order, else 42704); absent a
			// clause, the key inherits the column's frozen (implicit) collation.
			var coll *Collation
			if key.Collation != "" {
				if !s.columnOf(r).Type.IsCollatable() {
					return nil, typeError(fmt.Sprintf(
						"collations are not supported by type %s", s.columnOf(r).Type.CanonicalName(),
					))
				}
				if coll, err = resolveCollationName(s.catalog, key.Collation); err != nil {
					return nil, err
				}
			} else if coll, err = columnCollation(s, r); err != nil {
				return nil, err
			}
			slot := idx
			if isAgg {
				slot = -1
				for pos, gk := range groupKeys {
					if gk == idx {
						slot = pos
						break
					}
				}
				if slot < 0 {
					return nil, groupingErrorColumn(key.Column)
				}
			}
			order = append(order, orderSlot{idx: slot, descending: key.Descending, nullsFirst: key.NullsFirst, collation: coll})
			continue
		}

		// Resolve the key expression in the SAME context the projection used, so a window function /
		// GROUPING() / aggregate it contains collects into the shared specs and references the same
		// placeholders (rebased together after this loop — grammar.md §10): a grouped query collects over
		// the group keys + aggregates + GROUPING() calls (a new aggregate or GROUPING() the select list
		// lacks is allowed); a window query collects window specs/keys; a grouped+window query does both
		// (query.order_by_grouped_window); a plain query forbids aggregates (42803) and window functions
		// (42P20).
		var rexpr *rExpr
		var ty resolvedType
		octx := &aggCtx{collecting: isAgg, groupKeys: groupKeys, groupKeyExprs: groupKeyExprs, specs: aggSpecs, groupingSpecs: groupingSpecs}
		if hasWindowSyntax {
			octx.windowing = true
			octx.windowBase = windowResultBase
			octx.windowSpecs = windowSpecs
			octx.windowKeys = windowKeys
		}
		rexpr, ty, err = resolve(s, *orderExpr, nil, octx, ptypes)
		if err != nil {
			return nil, err
		}
		aggSpecs = octx.specs
		groupingSpecs = octx.groupingSpecs
		windowSpecs = octx.windowSpecs
		windowKeys = octx.windowKeys
		// A correlated ORDER BY expression (one referencing an enclosing query) is allowed
		// (query.order_by_correlated): the outer column is a per-evaluation constant of the enclosing
		// row, evaluated against the outer-row environment still in scope when materializeOrderExprs
		// runs. PostgreSQL accepts it; it is a degenerate (constant) leading key.
		// A non-orderable result type — json (no btree opclass) — is 42883; jsonb orders.
		if ty.kind == rtJson {
			return nil, newError(UndefinedFunction, "could not identify an ordering operator for type json")
		}
		// The collation of an expression key (collation.md §1): an explicit trailing COLLATE (rare —
		// parseExpr usually absorbs one into the key) must be on a text key (42804); otherwise it is
		// DERIVED from the key expression.
		var coll *Collation
		if key.Collation != "" {
			if !ty.collatable() && ty.kind != rtNull {
				return nil, typeError(fmt.Sprintf("collations are not supported by type %s", rtName(ty)))
			}
			if coll, err = resolveCollationName(s.catalog, key.Collation); err != nil {
				return nil, err
			}
		} else {
			d, derr := deriveCollation(s, *orderExpr)
			if derr != nil {
				return nil, derr
			}
			if coll, err = sortTargetCollation(s.catalog, d); err != nil {
				return nil, err
			}
		}
		k := len(orderExprs)
		orderExprs = append(orderExprs, rexpr)
		order = append(order, orderSlot{idx: orderExprBase + k, descending: key.Descending, nullsFirst: key.NullsFirst, collation: coll})
	}
	// All specs are now final (an ORDER BY key may have introduced a window function / aggregate /
	// GROUPING()). Recompute hasWindow and rebase every placeholder — in the projections, HAVING, AND
	// the materialized ORDER BY expressions — to its real trailing slot (window.md §5.1). The window
	// stage's row is [input… , materialized window keys… , window results…]; inputWidth is the grouped
	// row's width (group keys + every aggregate) for a grouped+window query, else the FROM scope width.
	hasWindow = len(windowSpecs) > 0
	if hasWindow {
		// The grouped row the window stage extends is [master cols…, agg results…, GROUPING results…]
		// (the GROUPING columns precede the window columns — aggregates.md §21), so a grouped+window
		// query's window input width includes the GROUPING() results.
		inputWidth := 0
		if isAgg {
			inputWidth = len(groupKeys) + len(aggSpecs) + len(groupingSpecs)
		} else {
			inputWidth = s.width()
		}
		keyBase := inputWidth
		resultBase := inputWidth + len(windowKeys)
		// Bound to [windowKeyBase, 2·windowKeyBase) so a GROUPING() placeholder (the higher
		// groupingGsBase) in a window key is not clobbered here (it rebases below — §21).
		for i := range windowSpecs {
			for j, pk := range windowSpecs[i].partition {
				if pk >= windowKeyBase && pk < windowKeyBase*2 {
					windowSpecs[i].partition[j] = keyBase + (pk - windowKeyBase)
				}
			}
			for j := range windowSpecs[i].order {
				if windowSpecs[i].order[j].idx >= windowKeyBase && windowSpecs[i].order[j].idx < windowKeyBase*2 {
					windowSpecs[i].order[j].idx = keyBase + (windowSpecs[i].order[j].idx - windowKeyBase)
				}
			}
		}
		for _, p := range projections {
			rebasePlaceholderCols(p, windowResultBase, resultBase)
		}
		for _, oe := range orderExprs {
			rebasePlaceholderCols(oe, windowResultBase, resultBase)
		}
	}
	// GROUPING SETS / GROUPING() combined with window functions (aggregates.md §21): the window stage
	// runs over the unioned grouping-set rows. The grouped row is [master cols…, agg results…, GROUPING
	// results…] and the window stage appends [window keys…, window results…] after, so the two no longer
	// collide — GROUPING rebases below the window bases.
	// Rebase the GROUPING() placeholder slots to their real trailing synthetic slots
	// len(groupKeys)+len(aggSpecs)+g (the GROUPING results follow the master columns and aggregate
	// results — §12), in the projections, HAVING, and the materialized ORDER BY expressions.
	if len(groupingSpecs) > 0 {
		gbase := len(groupKeys) + len(aggSpecs)
		for _, p := range projections {
			rebasePlaceholderCols(p, groupingGsBase, gbase)
		}
		if having != nil {
			rebasePlaceholderCols(having, groupingGsBase, gbase)
		}
		for _, oe := range orderExprs {
			rebasePlaceholderCols(oe, groupingGsBase, gbase)
		}
	}
	// Rebase each materialized expression-key slot to its real trailing position now that the row layout
	// is final. The materialized order values are appended AFTER the input / window / grouped columns
	// (grammar.md §10): for a grouped+window query the grouped row is first extended by the window stage,
	// so the order values follow the window results.
	var orderValueBase int
	switch {
	case isAgg && hasWindow:
		orderValueBase = len(groupKeys) + len(aggSpecs) + len(groupingSpecs) + len(windowKeys) + len(windowSpecs)
	case isAgg:
		orderValueBase = len(groupKeys) + len(aggSpecs) + len(groupingSpecs)
	case hasWindow:
		orderValueBase = s.width() + len(windowKeys) + len(windowSpecs)
	default:
		orderValueBase = s.width()
	}
	for i := range order {
		if order[i].idx >= orderExprBase {
			order[i].idx = orderValueBase + (order[i].idx - orderExprBase)
		}
	}

	// SELECT DISTINCT restriction (spec/design/grammar.md §11): once duplicates collapse, an ORDER BY
	// key must have a per-row value in the projected output — a bare/qualified column that is projected,
	// an ordinal (which names a select-list item by position), or a general expression that STRUCTURALLY
	// matches a select-list item. Otherwise 42P10 (matching PostgreSQL). Aliases are invisible to ORDER
	// BY (§8); a SELECT DISTINCT * projects every column, so the restriction never bites.
	if sel.Distinct && len(sel.OrderBy) > 0 && !items.All {
		projected := make(map[int]bool)
		for _, it := range items.Items {
			switch it.Expr.Kind {
			case exprColumn:
				if r, e := s.resolveBare(it.Expr.Column); e == nil && r.level == 0 {
					projected[r.index] = true
				}
			case exprQualifiedColumn:
				if r, e := s.resolveQualified(it.Expr.Qualifier, it.Expr.Column); e == nil && r.level == 0 {
					projected[r.index] = true
				}
			}
		}
		for i := range sel.OrderBy {
			key := &sel.OrderBy[i]
			inList := false
			switch {
			case key.Ordinal != nil:
				inList = true
			case key.Expr != nil:
				for j := range items.Items {
					if exprEqual(*key.Expr, items.Items[j].Expr) {
						inList = true
						break
					}
				}
			default:
				// A bare name that binds an output column (alias/derived name) names a select-list
				// item, so it is projected (the alias form, §10). Ambiguity was already raised above.
				if key.Qualifier == "" {
					if m, _ := orderAliasMatch(items, key.Column, s); m != nil {
						inList = true
						break
					}
				}
				var r resolved
				var e error
				if key.Qualifier != "" {
					r, e = s.resolveQualified(key.Qualifier, key.Column)
				} else {
					r, e = s.resolveBare(key.Column)
				}
				inList = e == nil && r.level == 0 && projected[r.index]
			}
			if !inList {
				return nil, newError(InvalidColumnReference,
					"for SELECT DISTINCT, ORDER BY expressions must appear in select list")
			}
		}
	}

	// The join predicates were resolved above (alongside the USING/NATURAL merges, which the scope
	// now carries). Pair each with its join kind — the kind only changes how unmatched rows are
	// handled in the executor loop, not the predicate (grammar.md §15).
	joins := make([]planJoin, len(sel.Joins))
	for k, j := range sel.Joins {
		joins[k] = planJoin{kind: j.Kind, on: joinPreds[k]}
	}

	// Assemble the owned LOGICAL plan (table NAMES + offsets/widths replace the scope's *Table, so
	// the plan outlives the scope and a correlated subquery can re-execute it per row). Resolve
	// decides names, types, and errors — never an access path: plan.phys is zero-valued here, and
	// only the optimizeSelect pass below writes it (spec/design/planner.md §2).
	for _, c := range pushed {
		filter = andConjunct(filter, c)
	}
	planRels := make([]planRel, len(s.rels))
	for i, rel := range s.rels {
		planRels[i] = planRel{tableName: rel.table.Name, db: rel.db, offset: rel.offset, colCount: len(rel.table.Columns), srf: srfPlans[i], cte: rel.cte, derived: derivedPlans[i], lateral: lateralFlags[i]}
	}
	plan := &selectPlan{
		rels: planRels, joins: joins, filter: filter, isAgg: isAgg, groupKeys: groupKeys,
		groupExprs: groupExprs,
		groupSets:  groupSets, groupingSpecs: groupingSpecs,
		aggSpecs: aggSpecs, hasWindow: hasWindow, windowSpecs: windowSpecs, windowKeys: windowKeys, having: having,
		order: order, orderExprs: orderExprs, projections: projections,
		columnNames: columnNames, columnTypes: columnTypes, columnDerivs: columnDerivs, columnTypmods: columnTypmods, distinct: sel.Distinct,
		limit: sel.Limit, offset: sel.Offset, group: group,
	}
	plan.relMasks = computeRelMasks(plan)
	// ——— Stage 2: logical rewrite rules (spec/design/planner.md §3) ———
	// The WHERE rewrite (contradiction detection, then pushdown) is a pure plan→plan transform.
	// foldUncorrelatedInPlan is NOT a planner rewrite — it executes subqueries and needs bound
	// params, so it stays post-bind in runQueryExpr.
	rewriteWhere(plan)
	// A shape is read for its outputs and pushability only, then discarded (planner.md §3.2).
	if shaping {
		return plan, nil
	}
	if err := db.planDerivedBodies(plan, tableRefs, isolated, shaped, cteBodies, parent, lateralParentScope, ctes, ptypes); err != nil {
		return nil, err
	}
	//
	// ——— Stage 3: physical/access-path selection (spec/design/planner.md §4) ———
	db.optimizeSelect(plan, s.rels)
	return plan, nil
}

// planBodyFirst runs f, the first planning of a derived body: as its shape when shape (planner.md
// §3.2), else as its real plan. A shape's view expansions are counted again when the body is planned
// for real.
func (db *engine) planBodyFirst(shape bool, f func() error) error {
	if !shape {
		return f()
	}
	saved, expansions := db.planShape, db.viewExpansions
	db.planShape = true
	err := f()
	db.planShape, db.viewExpansions = saved, expansions
	return err
}

// planAgain runs f as the second planning of a body whose shape was already planned. Planning again
// must have no other effect: the CTE reference counts the shape recorded are restored, keeping every
// CTE's mode unchanged.
func (db *engine) planAgain(ctes []*cteBinding, f func() error) error {
	refs := make([]int, len(ctes))
	for ci, b := range ctes {
		refs[ci] = b.refs
	}
	err := f()
	for ci, b := range ctes {
		b.refs = refs[ci]
	}
	return err
}

// withoutCteExpansion runs f with CTE reference expansion off (planner.md §3.4): the definition of a
// CTE that its reference may plan again.
func (db *engine) withoutCteExpansion(f func() error) error {
	saved := db.cteExpansionOff
	db.cteExpansionOff = true
	err := f()
	db.cteExpansionOff = saved
	return err
}

// planDerivedBodies plans each derived body whose shape stage 2 read for real, from its syntax
// (spec/design/planner.md §3.2). A body that stage 2 moved conjuncts into gets them appended to its
// WHERE, so its own rewrite and access-path selection see them; any other body is planned as written.
// A CTE reference that plans its CTE's body (cteBodies, planner.md §3.4) plans it from the CTE's
// syntax against the earlier bindings and no parent scope.
func (db *engine) planDerivedBodies(plan *selectPlan, tableRefs []tableRef, isolated []*viewSite, shaped []bool, cteBodies []int, parent, lateralParent *scope, ctes []*cteBinding, ptypes *paramTypes) error {
	var pushes [][]*rExpr
	if plan.pushdown != nil {
		pushes = plan.pushdown.bodyPushes
		plan.pushdown.bodyPushes = nil
	}
	for i := range shaped {
		if !shaped[i] {
			continue
		}
		var pushed []*rExpr
		if i < len(pushes) {
			pushed = pushes[i]
		}
		bodyExpr := tableRefs[i].Subquery
		if ci := cteBodies[i]; ci >= 0 {
			bodyExpr = ctes[ci].bodyAst
		}
		shape := plan.rels[i].derived
		replan := func(parent *scope, ctes []*cteBinding) (queryPlan, error) {
			if len(pushed) == 0 {
				return db.planQuery(*bodyExpr, parent, ctes, ptypes)
			}
			return db.replanPushedQuery(*bodyExpr, shape, pushed, parent, ctes, ptypes)
		}
		var body queryPlan
		err := db.planAgain(ctes, func() error {
			var rerr error
			if isolated[i] != nil {
				// A view body is planned in its isolated scope (views.md §4.1).
				return db.inViewBody(*isolated[i], func() error {
					body, rerr = replan(nil, nil)
					return rerr
				})
			}
			if ci := cteBodies[i]; ci >= 0 {
				// An inlined CTE's body is planned against the bindings before it (planner.md §3.4).
				body, rerr = replan(nil, ctes[:ci:ci])
				return rerr
			}
			if tableRefs[i].Lateral {
				// A LATERAL body planned like any derived body (it correlates to no sibling) keeps the
				// scope it was first planned against: inside a fenced parenthesized join, the lateral
				// view (§15).
				body, rerr = replan(lateralParent, ctes)
				return rerr
			}
			body, rerr = replan(parent, ctes)
			return rerr
		})
		if err != nil {
			return err
		}
		plan.rels[i].derived = &body
	}
	return nil
}

// replanPushedQuery plans a body-pushable query expression again with conjuncts in its output-column
// numbering pushed into it: a SELECT gets them substituted to the FROM slots its outputs read (below
// its grouping, when grouped) and appended to its WHERE; a set operation pushes them into each arm and
// keeps its own unified types, ORDER BY, and operator. first is the expression's first plan, whose
// output mapping the replanned SELECT reproduces.
func (db *engine) replanPushedQuery(qe queryExpr, first *queryPlan, pushed []*rExpr, parent *scope, ctes []*cteBinding, ptypes *paramTypes) (queryPlan, error) {
	if first.sel != nil {
		moved := make([]*rExpr, len(pushed))
		for k, c := range pushed {
			moved[k] = substituteBodyColumns(c, first.sel)
		}
		body, err := db.planSelectPushed(qe.Select, parent, ctes, ptypes, moved)
		if err != nil {
			return queryPlan{}, err
		}
		return queryPlan{sel: body}, nil
	}
	so := *first.setop
	lhs, err := db.replanPushedQuery(qe.SetOp.Lhs, &first.setop.lhs, pushed, parent, ctes, ptypes)
	if err != nil {
		return queryPlan{}, err
	}
	rhs, err := db.replanPushedQuery(qe.SetOp.Rhs, &first.setop.rhs, pushed, parent, ctes, ptypes)
	if err != nil {
		return queryPlan{}, err
	}
	so.lhs, so.rhs = lhs, rhs
	return queryPlan{setop: &so}, nil
}

// computeRelMasks computes the TOUCHED SET per relation (cost.md §3 "The touched set";
// large-values.md §14): the columns this query statically references, collected depth-aware so a
// correlated subquery's outer reference back into this scope counts. An aggregate query's
// projections / HAVING / ORDER BY index the synthetic group row, whose inputs are exactly the
// group keys + aggregate arguments collected here; a plain query's projections and ORDER BY keys
// index the combined row directly. An ANNOTATION of the logical plan, not an optimization
// (spec/design/planner.md §2): the mask is a correctness input to the lazy/masked scan — a wrong
// mask is a disk-mode NULL-folding bug, not a slow plan — so it is computed by the resolve half,
// before any physical rule runs.
func computeRelMasks(plan *selectPlan) [][]bool {
	totalCols := 0
	for _, rel := range plan.rels {
		totalCols += rel.colCount
	}
	touched := make([]bool, totalCols)
	collectTouched(plan.filter, 0, touched)
	for k := range plan.joins {
		collectTouched(plan.joins[k].on, 0, touched)
	}
	if plan.isAgg {
		// A column grouping key is a real input column (mark it); an expression grouping key has a
		// SYNTHETIC index (inputWidth+k, out of touched's range) — its real input columns are reached
		// through its materialized groupExprs node instead (aggregates.md §15).
		for _, gk := range plan.groupKeys {
			if gk < totalCols {
				touched[gk] = true
			}
		}
		for _, ge := range plan.groupExprs {
			collectTouched(ge, 0, touched)
		}
		for i := range plan.aggSpecs {
			collectTouched(plan.aggSpecs[i].operand, 0, touched)
			// An aggregate reads real input columns beyond its operand: the FILTER predicate
			// (agg(x) FILTER (WHERE cond) — aggregates.md §11), an ordered-set direct argument, and a
			// hypothetical-set's WITHIN GROUP key operands / direct args (aggregates.md §13/§19). Without
			// these the referenced column is left unfetched by the lazy/masked scan (large-values.md §14)
			// and folds as NULL — a memory-vs-disk divergence (count(*) FILTER, rank() WITHIN GROUP).
			collectTouched(plan.aggSpecs[i].filter, 0, touched)
			collectTouched(plan.aggSpecs[i].osaFrac, 0, touched)
			if plan.aggSpecs[i].hypo != nil {
				for _, k := range plan.aggSpecs[i].hypo.keys {
					collectTouched(k, 0, touched)
				}
				for _, a := range plan.aggSpecs[i].hypo.args {
					collectTouched(a, 0, touched)
				}
			}
			// An in-aggregate ORDER BY's keys are evaluated per input row too (aggregates.md §22).
			if o := plan.aggSpecs[i].order; o != nil {
				for _, k := range o.keys {
					collectTouched(k, 0, touched)
				}
			}
		}
	} else {
		for _, p := range plan.projections {
			collectTouched(p, 0, touched)
		}
		// A column-key ORDER BY slot is a real input column (< totalCols) — mark it; a materialized
		// expression-key slot is synthetic (>= totalCols, after rebase) whose input columns are reached
		// through its orderExprs expression instead (collected below).
		for _, o := range plan.order {
			if o.idx < totalCols {
				touched[o.idx] = true
			}
		}
		// Each materialized ORDER BY expression key reads real input columns (a plain query resolves it
		// against the FROM scope; a grouped query reaches them through its group keys / aggregate
		// arguments, already marked above).
		for _, oe := range plan.orderExprs {
			collectTouched(oe, 0, touched)
		}
		// A window query also reads each window function's PARTITION BY + ORDER BY keys, beyond what
		// the projection's window-result slots reference. A bare-column key is a real input slot
		// (< totalCols) — mark it; a materialized expression key is a synthetic slot (>= totalCols,
		// after rebase) whose input columns are reached through its windowKeys expression (below).
		for _, spec := range plan.windowSpecs {
			for _, pk := range spec.partition {
				if pk < totalCols {
					touched[pk] = true
				}
			}
			for _, o := range spec.order {
				if o.idx < totalCols {
					touched[o.idx] = true
				}
			}
			// The window function's ARGUMENT operands (sum(amount)'s amount, lag(v, off, def)'s
			// value/offset/default) and its FILTER read real input columns too — the row-based
			// window stage evaluates them per frame row (window.md §5.2). Without this the operand
			// column is left unfetched by the lazy/masked scan (large-values.md §14) and folds as
			// NULL. Mirrors the aggregate branch's collectTouched(aggSpecs[i].operand, …) above.
			for _, a := range spec.args {
				collectTouched(a, 0, touched)
			}
			collectTouched(spec.filter, 0, touched)
		}
		// Each materialized window-key expression reads real input columns (a plain window query
		// resolves its keys against the FROM scope).
		for _, ke := range plan.windowKeys {
			collectTouched(ke, 0, touched)
		}
	}
	// A set-returning relation's arguments and a LATERAL derived table's body read real input columns
	// too — an implicitly-lateral SRF arg / lateral body sees an earlier sibling relation (functions.md
	// §10, grammar.md §44). Applies to aggregate and plain queries alike (an aggregate query can carry a
	// lateral SRF). Without this the referenced column is left unfetched by the lazy/masked scan
	// (large-values.md §14) and the SRF/body reads NULL — a memory-vs-disk divergence.
	for i := range plan.rels {
		if plan.rels[i].srf != nil {
			// A LATERAL SRF (any SRF at position i>0) resolves its sibling columns as reOuterColumn at
			// level 1 (resolveSRF's lateralParent, the same frame the runtime pushes) — so collect at
			// depth 1, not 0. An i==0 SRF has no sibling correlation (constant/param args), so depth 1
			// marks nothing there. functions.md §10, grammar.md §44.
			for _, a := range plan.rels[i].srf.args {
				collectTouched(a, 1, touched)
			}
		}
		// Only a LATERAL body sees this query's columns (one frame deeper); a non-LATERAL body is
		// planned against the enclosing scope, so its outer references never reach this query.
		if plan.rels[i].derived != nil && plan.rels[i].lateral {
			collectTouchedPlan(plan.rels[i].derived, 1, touched)
		}
	}
	relMasks := make([][]bool, len(plan.rels))
	for i, rel := range plan.rels {
		relMasks[i] = touched[rel.offset : rel.offset+rel.colCount]
	}
	return relMasks
}

// joinGroupLayout is the output shape of an unaliased parenthesized join's body (spec/design/grammar.md
// §15): columns are the body outputs (the star columns, then the hidden USING/NATURAL copies), each
// keeping its source column's name, type, and collation; hidden is the output positions of those
// copies; subRels give each inner label's columns as output positions (rel unused).
type joinGroupLayout struct {
	columns []catColumn
	hidden  []int
	subRels []subRel
}

// joinGroupOutputs completes a JoinGroup body: after its `*` columns it appends every slot the star
// omitted (a USING/NATURAL copy superseded by a merge), so each inner relation's columns are all
// present, and records where each inner label's columns landed. Every star output is a bare column.
func joinGroupOutputs(s *scope, projections *[]*rExpr, names *[]string, types *[]resolvedType, derivs *[]deriv, typmods *[]typmod) *joinGroupLayout {
	pos := make(map[int]int, s.width())
	for p, pr := range *projections {
		pos[pr.index] = p
	}
	group := &joinGroupLayout{}
	extra := slices.Clone(s.hidden)
	slices.Sort(extra)
	for _, idx := range slices.Compact(extra) {
		if _, ok := pos[idx]; ok {
			continue
		}
		pos[idx] = len(*projections)
		group.hidden = append(group.hidden, len(*projections))
		c := s.columnAt(idx)
		*projections = append(*projections, &rExpr{kind: reColumn, index: idx})
		*names = append(*names, c.Name)
		*types = append(*types, resolvedTypeOfCol(c.Type, s.catalog.readSnap()))
		*derivs = append(*derivs, catColumnDeriv(c))
		*typmods = append(*typmods, columnTypmod(c))
	}
	for _, pr := range *projections {
		c := s.columnAt(pr.index)
		group.columns = append(group.columns, catColumn{Name: c.Name, Type: c.Type, Decimal: c.Decimal, VarcharLen: c.VarcharLen, Collation: c.Collation, NoCollation: c.NoCollation, CollationConflict: c.CollationConflict})
	}
	for _, r := range s.rels {
		if r.label == "" {
			continue
		}
		slots := make([]int, len(r.table.Columns))
		for c := range slots {
			slots[c] = pos[r.offset+c]
		}
		group.subRels = append(group.subRels, subRel{label: r.label, table: r.table, slots: slots})
	}
	for _, sr := range s.subRels {
		slots := make([]int, len(sr.slots))
		for c, slot := range sr.slots {
			slots[c] = pos[slot]
		}
		group.subRels = append(group.subRels, subRel{label: sr.label, table: sr.table, slots: slots})
	}
	return group
}

// parenJoinReachesLeft reports whether a parenthesized join's body holds an item that may reach the
// relations to the join's left (spec/design/grammar.md §15): a LATERAL derived table, a table function
// (implicitly lateral), or a nested parenthesized join holding one. Only such a join is planned
// against a lateral fence.
func parenJoinReachesLeft(tref *tableRef) bool {
	if tref.Subquery == nil || tref.Subquery.Select == nil || !tref.Subquery.Select.ParenJoin {
		return false
	}
	sel := tref.Subquery.Select
	items := []*tableRef{sel.From}
	for k := range sel.Joins {
		items = append(items, &sel.Joins[k].Table)
	}
	for _, t := range items {
		if t != nil && (t.Lateral || t.IsFunc || t.JsonTable != nil || parenJoinReachesLeft(t)) {
			return true
		}
	}
	return false
}

// derivedJoinGroup is the layout of a FROM item's derived body when it is an unaliased parenthesized
// join, else nil.
func derivedJoinGroup(plan *queryPlan) *joinGroupLayout {
	if plan == nil || plan.sel == nil {
		return nil
	}
	return plan.sel.group
}
