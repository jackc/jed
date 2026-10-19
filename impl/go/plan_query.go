package jed

import (
	"fmt"
)

// Query-expression orchestration — the layer above planSelect that composes SELECT, set operations,
// CTEs, and VALUES into a runnable query (spec/design/recursive-cte.md). This file holds the
// executeSelect/executeSetOp/executeWith entry points, CTE binding/planning and materialization
// (planCteBindings/materializeCtes/materializeRecursive) including WITH-on-DML, recursive-CTE
// analysis (analyzeRecursiveCte/countSelfRefs*), the planQuery/planSetOp/planValues planners and their
// execQueryPlan/execSetOpPlan/execValuesPlan executors, and set-operation type unification + combine.

// selectResult is the full result of running a SELECT (runSelect): the output column names and
// their resolved types, the rows in result order, and the accrued cost. Internal to the
// executor — executeSelect drops the types into the public outcome, while INSERT ... SELECT uses
// the types to gate assignability up front (spec/design/grammar.md §24).
type selectResult struct {
	columnNames []string
	columnTypes []resolvedType
	rows        [][]Value
	cost        int64
}

// executeSelect runs a SELECT as a top-level statement: runSelect, then wrap as a query outcome
// (the projection types are internal — only INSERT ... SELECT consumes them).
func (db *engine) executeSelect(sel *selectStmt, params []Value) (outcome, error) {
	r, err := db.runSelect(sel, params)
	if err != nil {
		return outcome{}, err
	}
	return outcome{Kind: outcomeQuery, ColumnNames: r.columnNames, ColumnTypes: typeNames(r.columnTypes), Rows: r.rows, Cost: r.cost}, nil
}

// executeSetOp runs a set operation as a top-level statement: runSetOp, then wrap as a query
// outcome. Cost is lhs.cost + rhs.cost — the combine, sort, and window are unmetered (cost.md §3).
func (db *engine) executeSetOp(so *setOp, params []Value) (outcome, error) {
	r, err := db.runSetOp(so, params)
	if err != nil {
		return outcome{}, err
	}
	return outcome{Kind: outcomeQuery, ColumnNames: r.columnNames, ColumnTypes: typeNames(r.columnTypes), Rows: r.rows, Cost: r.cost}, nil
}

// executeWith runs a WITH query (spec/design/cte.md) — the host-API entry point; runWith does the
// CTE orchestration.
func (db *engine) executeWith(wq *withQuery, params []Value) (outcome, error) {
	// A WITH containing any data-modifying part (a data-modifying CTE or a data-modifying primary)
	// runs through the writable-CTE orchestrator (spec/design/writable-cte.md): it pins the
	// pre-statement snapshot and runs the parts in lexical order, all-or-nothing. A pure-query WITH
	// keeps the existing read-only path (cte.md) unchanged.
	if withHasDml(wq) {
		return db.executeWithDml(wq, params)
	}
	r, err := db.runWith(wq, params)
	if err != nil {
		return outcome{}, err
	}
	return outcome{Kind: outcomeQuery, ColumnNames: r.columnNames, ColumnTypes: typeNames(r.columnTypes), Rows: r.rows, Cost: r.cost}, nil
}

// planCteBindings plans every CTE in a WITH list into bindings (spec/design/cte.md §2,
// writable-cte.md). Each body is planned against the prefix of EARLIER bindings and against parent —
// the scope enclosing the query that owns the WITH (nil at the statement top level; a nested WITH
// passes its own parent, so a body may correlate to an enclosing query level but never sees the
// owning query's FROM, cte.md §7). A body is never correlated to a reference site. Under WITH
// RECURSIVE a query CTE that references its own name is the recursive shape (its binding is pushed BEFORE planning the
// recursive term, so the self-reference resolves to it). A data-modifying CTE body resolves only its
// RETURNING schema here (its effect runs later, in the orchestrator) — a data-modifying body is never
// the recursive UNION shape, so it is always non-recursive. The refs counters are bumped as later
// query bodies / a query primary reference each binding (a data-modifying part's references are
// static-counted by the orchestrator, since it is not planned here).
//
// primary is the query that owns the list: a CTE that the later bodies and primary name exactly once
// is a candidate to be inlined at its reference (planner.md §3.4). Its definition is planned with
// reference expansion off, so a chain of such CTEs plans each body a bounded number of times.
func (db *engine) planCteBindings(ctes []cte, recursive bool, inherited []*cteBinding, parent *scope, ptypes *paramTypes, primary ctePrimary) ([]*cteBinding, error) {
	bindings := make([]*cteBinding, 0, len(ctes))
	for i := range ctes {
		cte := &ctes[i]
		lname := cte.Name
		for _, b := range bindings {
			if b.name == lname {
				return nil, newError(DuplicateAlias,
					"WITH query name "+lname+" specified more than once")
			}
		}
		searchCycle := cte.Search != nil || cte.Cycle != nil
		isRecursive, unionAll := false, false
		if recursive {
			if q := cte.Body.AsQuery(); q != nil {
				rec, ua, err := analyzeRecursiveCte(lname, *q, searchCycle)
				if err != nil {
					return nil, err
				}
				isRecursive, unionAll = rec, ua
			}
		}
		if isRecursive {
			// The body is `anchor UNION[ALL] recursive_term` (analyzeRecursiveCte verified).
			so := cte.Body.AsQuery().SetOp
			// The recursive CTE's name shadows an inherited binding throughout its body. In
			// particular, an anchor reference must not fall through to an outer same-named CTE:
			// PostgreSQL treats it as an illegal recursive reference, not an inherited one.
			if hasCteBinding(inherited, lname) && countSelfRefsQuery(so.Lhs, lname) > 0 {
				return nil, newError(InvalidRecursion, fmt.Sprintf(
					"recursive reference to query %q must not appear within its non-recursive term", lname,
				))
			}
			// A SEARCH / CYCLE clause types its cycle mark before the body is planned (PostgreSQL's
			// order — recursive-cte.md §9).
			var mark *cycleMark
			if cte.Cycle != nil {
				m, err := db.resolveCycleMark(cte.Cycle, ptypes)
				if err != nil {
					return nil, err
				}
				mark = m
			}
			visible := append(append([]*cteBinding{}, inherited...), bindings...)
			anchorPlan, err := db.planQuery(so.Lhs, parent, visible, ptypes)
			if err != nil {
				return nil, err
			}
			table, err := cteSyntheticTable(lname, &anchorPlan, cte.Columns)
			if err != nil {
				return nil, err
			}
			// SEARCH / CYCLE add columns after the CTE's own (recursive-cte.md §9).
			var sc *searchCyclePlan
			if searchCycle {
				sc, err = db.planSearchCycle(cte, table, anchorPlan.columnTypes(), mark, ptypes)
				if err != nil {
					return nil, err
				}
				names := make([]string, 0, len(table.Columns)+len(sc.names))
				for _, c := range table.Columns {
					names = append(names, c.Name)
				}
				names = append(names, sc.names...)
				types := append(append([]resolvedType{}, anchorPlan.columnTypes()...), sc.types...)
				derivs := append([]deriv{}, anchorPlan.columnDerivs()...)
				typmods := append([]typmod{}, anchorPlan.columnTypmods()...)
				for range sc.types {
					derivs = append(derivs, deriv{})
					typmods = append(typmods, typmod{})
				}
				table, err = cteSyntheticTableCols(lname, names, types, derivs, typmods, nil)
				if err != nil {
					return nil, err
				}
			}
			added := 0
			if sc != nil {
				added = len(sc.names)
			}
			bindings = append(bindings, &cteBinding{
				name: lname, table: table, plan: anchorPlan, hint: cte.Materialized, starHidden: added,
			})
			bi := len(bindings) - 1
			visible = append(append([]*cteBinding{}, inherited...), bindings...)
			// Under SEARCH / CYCLE the recursive term carries the self-reference's added columns as
			// hidden trailing outputs — the parent row's, from which the executor computes the child's.
			rhs := so.Rhs
			if sc != nil {
				rhs = withCarriedColumns(so.Rhs, cte.Name, sc.names)
			}
			rhsPlan, err := db.planQuery(rhs, parent, visible, ptypes)
			if err != nil {
				return nil, err
			}
			bindings[bi].starHidden = 0
			if err := checkRecursiveColumnTypes(so.Lhs, &bindings[bi].plan, &rhsPlan, lname, unionAll, added); err != nil {
				return nil, err
			}
			bindings[bi].recursive = &recursiveTerm{plan: rhsPlan, unionAll: unionAll, searchCycle: sc}
			continue
		}

		if q := cte.Body.AsQuery(); q != nil {
			visible := append(append([]*cteBinding{}, inherited...), bindings...)
			// Named exactly once, by a query part, and not MATERIALIZED: a candidate to be inlined at
			// its reference (planner.md §3.4). Its definition then expands no reference of its own; an
			// uncorrelated candidate keeps that definition, a correlated one is planned again with
			// expansion on (it stays an ordinary CTE).
			candidate := cteReferenceCandidate(ctes, i, primary)
			var plan queryPlan
			planDefinition := func() error {
				p, perr := db.planQuery(*q, parent, visible, ptypes)
				plan = p
				return perr
			}
			var err error
			if candidate {
				err = db.withoutCteExpansion(planDefinition)
			} else {
				err = planDefinition()
			}
			if err != nil {
				return nil, err
			}
			atReference := candidate && !queryPlanReferencesOuter(&plan, 0)
			if candidate && !atReference {
				if err := db.planAgain(visible, planDefinition); err != nil {
					return nil, err
				}
			}
			table, err := cteSyntheticTable(lname, &plan, cte.Columns)
			if err != nil {
				return nil, err
			}
			binding := &cteBinding{
				name: lname, table: table, plan: plan, hint: cte.Materialized, atReference: atReference,
			}
			if atReference {
				binding.bodyAst = q
			}
			bindings = append(bindings, binding)
			if searchCycle {
				return nil, notRecursiveError(cte)
			}
			continue
		}
		// A data-modifying CTE (writable-cte.md): resolve its RETURNING schema for the synthetic
		// relation + capture the statement to run later.
		visible := append(append([]*cteBinding{}, inherited...), bindings...)
		table, dm, err := db.planDmCte(lname, &cte.Body, visible, cte.Columns, ptypes)
		if err != nil {
			return nil, err
		}
		bindings = append(bindings, &cteBinding{
			name: lname, table: table, dm: dm, hint: cte.Materialized,
		})
		if searchCycle {
			return nil, notRecursiveError(cte)
		}
	}
	return bindings, nil
}

// notRecursiveError is SEARCH / CYCLE on a CTE that does not reference itself (recursive-cte.md §9),
// reported after its body is planned, as in PostgreSQL.
func notRecursiveError(c *cte) error {
	var pos srcPos
	if c.Search != nil {
		pos = c.Search.Pos
	} else if c.Cycle != nil {
		pos = c.Cycle.Pos
	}
	return newError(SyntaxError, "WITH query is not recursive").withPosition(int32(pos))
}

// ctePrimary is the query that owns a WITH list — its main body, which (with the later CTE bodies)
// holds the static references that decide whether a CTE is inlined at its reference (planner.md
// §3.4). Exactly one field is set: body for a statement-level WITH (whose primary may be
// data-modifying), query for a nested WITH's query body.
type ctePrimary struct {
	body  *cteBody
	query *queryExpr
}

// refs returns the static (syntactic) references to CTE name in the primary, as (query,
// data-modifying).
func (p ctePrimary) refs(name string) (int, int) {
	if p.body != nil {
		return staticCteRefs(p.body, name)
	}
	return countSelfRefsQuery(*p.query, name), 0
}

// staticCteRefs returns the static references to CTE name in one part of a WITH statement, as
// (query, data-modifying): a data-modifying part's references are counted apart, because such a part
// is planned only when it runs, so a CTE it names is never inlined at its reference (planner.md
// §3.4).
func staticCteRefs(body *cteBody, name string) (int, int) {
	n := countCteRefsDml(body, name)
	if body.IsDataModifying() {
		return 0, n
	}
	return n, 0
}

// cteReferenceCandidate reports whether the CTE at ci in a WITH list may be inlined at its reference
// (planner.md §3.4): not MATERIALIZED, and named exactly once by the later CTE bodies and the
// primary, by a query part.
func cteReferenceCandidate(ctes []cte, ci int, primary ctePrimary) bool {
	name := ctes[ci].Name
	query, dml := primary.refs(name)
	for k := ci + 1; k < len(ctes); k++ {
		q, d := staticCteRefs(&ctes[k].Body, name)
		query += q
		dml += d
	}
	materialized := ctes[ci].Materialized != nil && *ctes[ci].Materialized
	return query == 1 && dml == 0 && !materialized
}

func hasCteBinding(bindings []*cteBinding, name string) bool {
	for _, binding := range bindings {
		if binding.name == name {
			return true
		}
	}
	return false
}

// planDmCte plans a data-modifying CTE body (spec/design/writable-cte.md): resolve its RETURNING
// schema (against the EARLIER bindings, so a RETURNING sublink may reference an earlier CTE) to build
// the synthetic relation, and capture the statement to execute later. A body with no RETURNING yields
// a zero-column relation flagged noReturning (a FROM reference to it is 0A000, §5). The target must
// be a base table — a CTE name / missing table is 42P01 (§1).
func (db *engine) planDmCte(lname string, body *cteBody, bindings []*cteBinding, rename []string, ptypes *paramTypes) (*catTable, *dmCte, error) {
	var tableName string
	var tablePos srcPos
	var returning *returningClause
	var baseIsOld bool
	var alias *string
	dm := &dmCte{}
	switch {
	case body.Insert != nil:
		tableName, returning, baseIsOld = body.Insert.Table, body.Insert.Returning, false
		tablePos = body.Insert.TablePos
		alias = body.Insert.Alias
		dm.insert = body.Insert
	case body.Update != nil:
		tableName, returning, baseIsOld = body.Update.Table, body.Update.Returning, false
		tablePos = body.Update.TablePos
		dm.update = body.Update
	default:
		tableName, returning, baseIsOld = body.Delete.Table, body.Delete.Returning, true
		tablePos = body.Delete.TablePos
		dm.delete = body.Delete
	}
	// A write through an updatable view targets its base table, and its RETURNING projects the
	// view's columns (views.md §15); a view that is not updatable is 55000. The CTE keeps the written
	// statement, which its execution retargets again.
	// The rewrite clears the database qualifier of a view target (views live in main); a base target
	// keeps its own, so a qualified attachment table resolves in its database.
	var vw *viewWrite
	var targetDB *string
	var err error
	switch {
	case body.Insert != nil:
		var ins *insert
		ins, vw, err = db.rewriteInsertTarget(body.Insert)
		if err == nil {
			targetDB = ins.DB
		}
	case body.Update != nil:
		var upd *update
		upd, vw, err = db.rewriteUpdateTarget(body.Update)
		if err == nil {
			targetDB = upd.DB
		}
	default:
		var del *deleteStmt
		del, vw, err = db.rewriteDeleteTarget(body.Delete)
		if err == nil {
			targetDB = del.DB
		}
	}
	if err != nil {
		return nil, nil, err
	}
	target := tableName
	if vw != nil {
		target = vw.table
	}
	if err := db.checkTableQualifier(targetDB, target); err != nil {
		return nil, nil, atTarget(err, tablePos)
	}
	tdef, ok := db.lkpTableScoped(targetDB, target) // a bare name is temp-first (temp-tables.md §3)
	if !ok {
		return nil, nil, newError(UndefinedTable, "table does not exist: "+tableName).withPosition(int32(tablePos))
	}
	if returning == nil {
		dm.noReturning = true
		table, err := cteSyntheticTableCols(lname, nil, nil, nil, nil, rename)
		if err != nil {
			return nil, nil, err
		}
		return table, dm, nil
	}
	s, err := returningScope(db, tdef, baseIsOld, returning, vw, alias)
	if err != nil {
		return nil, nil, err
	}
	s.ctes = bindings
	_, names, types, derivs, typmods, err := resolveProjections(s, returning.Items, &aggCtx{collecting: false}, ptypes)
	if err != nil {
		return nil, nil, err
	}
	table, err := cteSyntheticTableCols(lname, names, types, derivs, typmods, rename)
	if err != nil {
		return nil, nil, err
	}
	return table, dm, nil
}

// runWith runs a pure-query WITH (spec/design/cte.md) — the path for a WITH with no data-modifying
// part (a data-modifying WITH goes through executeWithDml). (1) PLAN every CTE binding against the
// prefix; (2) plan the main body with all bindings visible; (3) decide each CTE's mode from its
// reference count + [NOT] MATERIALIZED hint; (4) MATERIALIZE each referenced materialized CTE once,
// in list order (a later body sees the earlier buffers); (5) fold + EXECUTE the main body with the
// CTE context. Cost composes like set operations — a sum of the parts.
func (db *engine) runWith(wq *withQuery, params []Value) (selectResult, error) {
	bodyQ := wq.Body.AsQuery()
	planAll := func(ptypes *paramTypes) ([]*cteBinding, queryPlan, error) {
		bindings, err := db.planCteBindings(wq.Ctes, wq.Recursive, nil, nil, ptypes, ctePrimary{body: &wq.Body})
		if err != nil {
			return nil, queryPlan{}, err
		}
		// (2) Plan the main body with all bindings visible (the pure-query path always has a query
		//     primary — a data-modifying primary routes to executeWithDml).
		plan, err := db.planQuery(*bodyQ, nil, bindings, ptypes)
		return bindings, plan, err
	}
	planning := db.beginPlanning()
	defer planning.end()
	ptypes := &paramTypes{}
	bindings, plan, err := planAll(ptypes)
	if err != nil {
		return selectResult{}, err
	}
	ptys, err := ptypes.finalize()
	if err != nil {
		return selectResult{}, err
	}
	bound, err := bindParams(params, ptys, db.inputZone())
	if err != nil {
		return selectResult{}, err
	}
	if planning.bind(bound) {
		if bindings, plan, err = planAll(&paramTypes{}); err != nil {
			return selectResult{}, err
		}
	}
	modes := cteModes(bindings)
	buffers, totalCost, err := db.materializeCtes(bindings, modes, bound, cteCtx{}, nil)
	if err != nil {
		return selectResult{}, err
	}

	// (5) Fold + execute the main body against the full CTE context.
	ctx := topLevelCteCtx(modes, bindings, buffers)
	var subqueryCost int64
	if err := db.foldUncorrelatedInPlan(&plan, bound, ctx, &subqueryCost); err != nil {
		return selectResult{}, err
	}
	r, err := db.execQueryPlan(&plan, nil, bound, ctx)
	if err != nil {
		return selectResult{}, err
	}
	releaseCteBuffers(db.session.queryAccount(), buffers)
	r.cost += subqueryCost + totalCost
	db.explainActual.recordParent("WITH", r.cost)
	return r, nil
}

// materializeCtes materializes each CTE once, in list order (spec/design/cte.md §3) — the shared loop
// for the pure-query and writable-CTE paths' query/recursive CTEs. A data-modifying CTE is NOT run
// here (the orchestrator runs it for its effect — executeWithDml); its buffer slot is left empty for
// the orchestrator to fill. Returns the filled buffers + the accrued materialization cost (a later
// body sees the earlier buffers). outer is the declaring WITH node's outer-row environment (nil at
// top level), the frame every body runs in (cte.md §7).
func (db *engine) materializeCtes(bindings []*cteBinding, modes []cteMode, bound []Value, inherited cteCtx, outer []storedRow) ([][]storedRow, int64, error) {
	var totalCost int64
	buffers := make([][]storedRow, 0, len(bindings))
	for i := range bindings {
		before := totalCost
		bodyVisible := bindings[i].refs > 0 || bindings[i].recursive != nil || bindings[i].isDml() || modes[i] == cteMaterialize
		var bodyMarker int64
		if bodyVisible {
			bodyMarker = 1
		}
		db.explainActual.record("@cte-body "+bindings[i].name, bodyMarker)
		var buf []storedRow
		switch {
		case bindings[i].recursive != nil:
			b, err := db.materializeRecursive(i, bindings[i].recursive, modes, bindings, buffers, bound, inherited, outer, &totalCost)
			if err != nil {
				return nil, 0, err
			}
			buf = b
		case bindings[i].isDml():
			// A data-modifying CTE's buffer is filled by the orchestrator, not here.
		case modes[i] == cteMaterialize:
			ctx := inherited.extend(outer, modes[:i], bindings[:i], buffers)
			cplan := bindings[i].plan
			r, err := db.execQueryPlan(&cplan, outer, bound, ctx)
			if err != nil {
				return nil, 0, err
			}
			totalCost += r.cost
			buf = rowsFromValues(r.rows)
		}
		buffers = append(buffers, buf)
		db.explainActual.recordParent("CTE "+bindings[i].name, totalCost-before)
	}
	return buffers, totalCost, nil
}

// materializeRecursive materializes a RECURSIVE CTE by iterating to a fixpoint — the PostgreSQL
// working-table method (spec/design/recursive-cte.md §4). rt is the recursive term (which references
// this CTE, index ci); the anchor is bindings[ci].plan. priorBuffers are the earlier CTEs'
// materialized rows (visible to both terms). totalCost accrues every term evaluation's cost and gates
// the per-statement ceiling between iterations, so a non-terminating recursion of cheap iterations
// still aborts 54P01 at the identical accrued cost in every core (recursive-cte.md §5). outer is the
// declaring WITH node's outer-row environment (nil at top level), the frame both terms run in (cte.md
// §7).
func (db *engine) materializeRecursive(ci int, rt *recursiveTerm,
	modes []cteMode, bindings []*cteBinding, priorBuffers [][]storedRow, params []Value, inherited cteCtx, outer []storedRow, totalCost *int64,
) ([]storedRow, error) {
	anchorPlan := &bindings[ci].plan
	maxCost := db.session.maxCost
	guard := func(total int64) error {
		if maxCost > 0 && total >= maxCost {
			return newError(CostLimitExceeded, fmt.Sprintf(
				"query exceeded the cost limit of %d (accrued %d)", maxCost, total,
			))
		}
		return nil
	}
	anchorTypes := anchorPlan.columnTypes()
	rhsTypes := rt.plan.columnTypes()

	// Evaluate the anchor: its rows seed both the result and the first working table.
	ctx0 := inherited.extend(outer, modes[:ci], bindings[:ci], priorBuffers)
	ar, err := db.execQueryPlan(anchorPlan, outer, params, ctx0)
	if err != nil {
		return nil, err
	}
	*totalCost += ar.cost
	if err := guard(*totalCost); err != nil {
		return nil, err
	}
	anchorRows := ar.rows
	// SEARCH / CYCLE: each anchor row gets its added columns (recursive-cte.md §9).
	if sc := rt.searchCycle; sc != nil {
		rows, cost, err := db.searchCycleRows(sc, sc.anchor, nil, anchorRows, params, outer)
		if err != nil {
			return nil, err
		}
		*totalCost += cost
		if err := guard(*totalCost); err != nil {
			return nil, err
		}
		anchorRows = rows
	}

	// For UNION (distinct) a seen set drops rows duplicating any already-emitted row, keyed by the
	// NULL-safe distinctRowKey the set operators use.
	seen := map[string]bool{}
	keep := func(row storedRow) bool {
		if rt.unionAll {
			return true
		}
		k := distinctRowKey(row)
		if seen[k] {
			return false
		}
		seen[k] = true
		return true
	}
	// Query-memory row accounting (memory.md §5): each term's result is charged by its own execution;
	// a kept row's result copy is admitted, a duplicate is released, and a consumed working table is
	// released when the next iteration replaces it.
	acct := db.session.queryAccount()
	// The UNION dedup set is operator state (memory.md §6.2): an entry per kept row, reserved before
	// its result copy, returned when the recursive CTE completes.
	seenCharge := newStateCharge(acct)
	defer seenCharge.releaseAll()
	var result, working []storedRow
	for _, row := range anchorRows {
		if keep(row) {
			if !rt.unionAll && seenCharge.active() {
				if err := seenCharge.reserveDirect(memEntryBytes(row)); err != nil {
					return nil, err
				}
			}
			if err := acct.admitRow(row); err != nil {
				return nil, err
			}
			result = append(result, row)
			working = append(working, row)
		} else {
			acct.releaseRow(row)
		}
	}

	// The recursive term scans the WORKING table through the CTE's own buffer slot (ci); the earlier
	// CTEs keep their full buffers. Build the buffer vec once and swap slot ci per iteration. The copy
	// of the earlier buffers is itself a row buffer.
	rhsBuffers := make([][]storedRow, ci+1)
	copy(rhsBuffers, priorBuffers)
	for _, buf := range priorBuffers {
		if err := acct.reserve(measureRows(acct, buf)); err != nil {
			return nil, err
		}
	}

	cancelMeter := db.session.newMeter()
	for len(working) > 0 {
		if err := cancelMeter.PollStep(); err != nil {
			return nil, err
		}
		acct.release(measureRows(acct, rhsBuffers[ci]))
		rhsBuffers[ci] = working
		working = nil
		ctx := inherited.extend(outer, modes[:ci+1], bindings[:ci+1], rhsBuffers)
		cplan := rt.plan
		rr, err := db.execQueryPlan(&cplan, outer, params, ctx)
		if err != nil {
			return nil, err
		}
		*totalCost += rr.cost
		if err := guard(*totalCost); err != nil {
			return nil, err
		}
		before := measureRows(acct, rr.rows)
		coerceSetopRows(rr.rows, rhsTypes, anchorTypes)
		if err := acct.reserve(measureRows(acct, rr.rows) - before); err != nil {
			return nil, err
		}
		// SEARCH / CYCLE: drop a row whose parent closed a cycle, then replace the carried parent
		// columns with the row's own (recursive-cte.md §9).
		if sc := rt.searchCycle; sc != nil {
			rows, cost, err := db.searchCycleRows(sc, sc.recursive, sc.filter, rr.rows, params, outer)
			if err != nil {
				return nil, err
			}
			*totalCost += cost
			if err := guard(*totalCost); err != nil {
				return nil, err
			}
			rr.rows = rows
		}
		for _, vrow := range rr.rows {
			row := storedRow(vrow)
			if keep(row) {
				if !rt.unionAll && seenCharge.active() {
					if err := seenCharge.reserveDirect(memEntryBytes(row)); err != nil {
						return nil, err
					}
				}
				if err := acct.admitRow(row); err != nil {
					return nil, err
				}
				result = append(result, row)
				working = append(working, row)
			} else {
				acct.releaseRow(row)
			}
		}
	}
	for _, buf := range rhsBuffers {
		acct.release(measureRows(acct, buf))
	}
	seenCharge.releaseAll()
	return result, nil
}

// releaseCteBuffers releases a WITH's CTE buffers, which die with the statement part that
// materialized them (memory.md §5.3) — essential for a nested WITH re-run per outer row.
func releaseCteBuffers(acct queryAccount, buffers [][]storedRow) {
	if !acct.active() {
		return
	}
	for _, buf := range buffers {
		acct.release(measureRows(acct, buf))
	}
}

// executeWithDml runs a data-modifying WITH statement (spec/design/writable-cte.md): a WITH
// containing a data-modifying CTE and/or a data-modifying primary. It PINS the pre-statement snapshot
// for every sub-statement's reads (§2 — so the parts cannot see each other's table writes; data
// crosses only via a CTE's RETURNING buffer), runs the parts in lexical order, and returns the
// primary's result. The whole statement is one all-or-nothing transaction — the autocommit (or block)
// wrapper publishes the accumulated working only if this returns nil error (§6).
func (db *engine) executeWithDml(wq *withQuery, params []Value) (outcome, error) {
	// Pin the pre-statement snapshot of every database domain (main, temp, each attachment). A write
	// statement runs with a transaction open (autocommit opened one), and nothing is written yet, so
	// each pin equals that domain's working state. Cleared on every exit path so the next statement
	// reads normally.
	db.session.readPin = db.captureReadPin()
	// Every foreign-key check waits for the end of the whole statement (§7), then runs against the
	// working state every sub-statement produced, in the order the checks were queued.
	db.session.fkStatementDefer = true
	out, err := db.runWithDml(wq, params)
	db.session.readPin = nil
	db.session.fkStatementDefer = false
	db.session.fkDeferredChecks = nil
	return out, err
}

// runWithDml is the body of executeWithDml, run under the read pin. Plans every CTE binding + the
// query primary, runs the data-modifying CTEs / materialized query CTEs in list order, then the
// primary — every read against the pin, every write into the transaction's working.
func (db *engine) runWithDml(wq *withQuery, params []Value) (outcome, error) {
	// Every part is planned before the values are bound; a probe that read one restarts the
	// statement with the values visible (estimator.md §6.1).
	planning := db.beginPlanning()
	defer planning.end()
	ptypes := &paramTypes{}
	// (1) Plan every CTE binding (query plans + data-modifying RETURNING schemas).
	bindings, err := db.planCteBindings(wq.Ctes, wq.Recursive, nil, nil, ptypes, ctePrimary{body: &wq.Body})
	if err != nil {
		return outcome{}, err
	}
	// (2) Plan a query primary now (to bump refs + surface resolution errors, incl. a 0A000 FROM
	//     reference to a no-RETURNING data-modifying CTE). A data-modifying primary is resolved and
	//     run later (it sees the bindings via the threaded context); its references are static-counted
	//     in (2b).
	var primaryPlan *queryPlan
	if q := wq.Body.AsQuery(); q != nil {
		p, perr := db.planQuery(*q, nil, bindings, ptypes)
		if perr != nil {
			return outcome{}, perr
		}
		primaryPlan = &p
	}
	// (2b) Add the references each NON-planned data-modifying part (a data-modifying CTE body, or a
	//      data-modifying primary) contributes to each binding, so the inline-vs-materialize decision
	//      is correct for a query CTE referenced only by a data-modifying part (§3). Query bodies / a
	//      query primary were already plan-counted in (1)/(2).
	for i := range wq.Ctes {
		if wq.Ctes[i].Body.IsDataModifying() {
			for _, b := range bindings {
				b.refs += countCteRefsDml(&wq.Ctes[i].Body, b.name)
			}
		}
	}
	if wq.Body.IsDataModifying() {
		for _, b := range bindings {
			b.refs += countCteRefsDml(&wq.Body, b.name)
		}
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
			out, err = db.runWithDml(wq, params)
			return err
		})
		return out, err
	}
	modes := cteModes(bindings)

	// (3) Run each CTE in list order, filling its buffer. A data-modifying CTE executes for its effect
	//     + RETURNING buffer; the query/recursive CTEs use the shared materialize loop's logic.
	var totalCost int64
	buffers := make([][]storedRow, 0, len(bindings))
	for i := range bindings {
		before := totalCost
		bodyVisible := bindings[i].refs > 0 || bindings[i].recursive != nil || bindings[i].isDml() || modes[i] == cteMaterialize
		var bodyMarker int64
		if bodyVisible {
			bodyMarker = 1
		}
		db.explainActual.record("@cte-body "+bindings[i].name, bodyMarker)
		var buf []storedRow
		switch {
		case bindings[i].recursive != nil:
			b, rerr := db.materializeRecursive(i, bindings[i].recursive, modes, bindings, buffers, bound, cteCtx{}, nil, &totalCost)
			if rerr != nil {
				return outcome{}, rerr
			}
			buf = b
		case bindings[i].isDml():
			ctx := topLevelCteCtx(modes[:i], bindings[:i], buffers)
			rows, cost, derr := db.execDmCte(i, bindings, bound, ctx)
			if derr != nil {
				return outcome{}, derr
			}
			totalCost += cost
			buf = rows
		case modes[i] == cteMaterialize:
			ctx := topLevelCteCtx(modes[:i], bindings[:i], buffers)
			cplan := bindings[i].plan
			r, rerr := db.execQueryPlan(&cplan, nil, bound, ctx)
			if rerr != nil {
				return outcome{}, rerr
			}
			totalCost += r.cost
			buf = rowsFromValues(r.rows)
		}
		buffers = append(buffers, buf)
		db.explainActual.recordParent("CTE "+bindings[i].name, totalCost-before)
	}

	// (4) Execute the primary against the full CTE context, adding the materialization cost.
	ctx := topLevelCteCtx(modes, bindings, buffers)
	var out outcome
	switch {
	case wq.Body.AsQuery() != nil:
		var subqueryCost int64
		if err := db.foldUncorrelatedInPlan(primaryPlan, bound, ctx, &subqueryCost); err != nil {
			return outcome{}, err
		}
		r, rerr := db.execQueryPlan(primaryPlan, nil, bound, ctx)
		if rerr != nil {
			return outcome{}, rerr
		}
		out = outcome{
			Kind:        outcomeQuery,
			ColumnNames: r.columnNames,
			ColumnTypes: typeNames(r.columnTypes),
			Rows:        r.rows,
			Cost:        r.cost + subqueryCost,
		}
	case wq.Body.Insert != nil:
		out, err = db.executeInsert(wq.Body.Insert, params, ctx)
	case wq.Body.Update != nil:
		out, err = db.executeUpdate(wq.Body.Update, params, ctx)
	default:
		out, err = db.executeDelete(wq.Body.Delete, params, ctx)
	}
	if err != nil {
		return outcome{}, err
	}
	switch {
	case wq.Body.Insert != nil:
		db.explainActual.record("Insert "+wq.Body.Insert.Table, out.Cost)
	case wq.Body.Update != nil:
		db.explainActual.record("Update "+wq.Body.Update.Table, out.Cost)
	case wq.Body.Delete != nil:
		db.explainActual.record("Delete "+wq.Body.Delete.Table, out.Cost)
	}
	// (5) Settle every foreign-key check the parts queued, against the end state of the whole
	//     statement (§7). A parent-side reverse match is metered (constraints.md §6.11).
	fkMeter := db.session.newMeter()
	if err := db.flushFkDeferredChecks(fkMeter); err != nil {
		return outcome{}, err
	}
	releaseCteBuffers(db.session.queryAccount(), buffers)
	out = addOutcomeCost(out, totalCost+fkMeter.Accrued)
	db.explainActual.recordParent("WITH", out.Cost)
	return out, nil
}

// execDmCte executes a data-modifying CTE (spec/design/writable-cte.md §3): run the INSERT/UPDATE/
// DELETE at binding i for its effect, with the earlier bindings/buffers in scope (so its inner
// queries may reference an earlier CTE), and return its RETURNING rows (the buffer the later parts
// scan) + its cost. A body with no RETURNING runs for its effect and buffers no rows.
func (db *engine) execDmCte(i int, bindings []*cteBinding, params []Value, ctx cteCtx) ([]storedRow, int64, error) {
	dm := bindings[i].dm
	var out outcome
	var err error
	switch {
	case dm.insert != nil:
		out, err = db.executeInsert(dm.insert, params, ctx)
	case dm.update != nil:
		out, err = db.executeUpdate(dm.update, params, ctx)
	default:
		out, err = db.executeDelete(dm.delete, params, ctx)
	}
	if err != nil {
		return nil, 0, err
	}
	switch {
	case dm.insert != nil:
		db.explainActual.record("Insert "+dm.insert.Table, out.Cost)
	case dm.update != nil:
		db.explainActual.record("Update "+dm.update.Table, out.Cost)
	default:
		db.explainActual.record("Delete "+dm.delete.Table, out.Cost)
	}
	if out.Kind == outcomeQuery {
		return rowsFromValues(out.Rows), out.Cost, nil
	}
	return nil, out.Cost, nil
}

// === WITH RECURSIVE analysis (spec/design/recursive-cte.md) ==========================
//
// A WITH RECURSIVE CTE is recursive iff its body references its own name (anywhere, deep). A
// recursive CTE must take the well-formed shape `non_recursive_term UNION [ALL] recursive_term`
// with the self-reference appearing exactly once, as a direct FROM/JOIN relation of the recursive
// term. These structural checks mirror PostgreSQL's checkWellFormedRecursion, run on the parsed AST
// before planning; the error surface is recursive-cte.md §6.

// analyzeRecursiveCte classifies a CTE body for WITH RECURSIVE (recursive-cte.md §6). It returns
// (false, _, nil) when the body does not reference name (an ordinary CTE, even under RECURSIVE);
// otherwise it validates the recursive shape and returns (true, unionAll, nil), or an error (42P19
// for a malformed recursion, 0A000 for a deferred shape).
func analyzeRecursiveCte(name string, body queryExpr, searchCycle bool) (bool, bool, error) {
	if countSelfRefsQuery(body, name) == 0 {
		return false, false, nil
	}
	so := body.SetOp
	if so == nil || so.Op != setOpUnion {
		return false, false, newError(InvalidRecursion, fmt.Sprintf(
			"recursive query %q does not have the form non-recursive-term UNION [ALL] recursive-term", name,
		))
	}
	if len(so.OrderBy) > 0 {
		return false, false, newError(FeatureNotSupported, "ORDER BY in a recursive query is not implemented")
	}
	if so.Limit != nil || so.Offset != nil {
		return false, false, newError(FeatureNotSupported, "LIMIT in a recursive query is not implemented")
	}
	if countSelfRefsQuery(so.Lhs, name) > 0 {
		return false, false, newError(InvalidRecursion, fmt.Sprintf(
			"recursive reference to query %q must not appear within its non-recursive term", name,
		))
	}
	// SEARCH / CYCLE need each side of the UNION to be one SELECT (PostgreSQL's messages and codes).
	if searchCycle {
		if so.Lhs.SetOp != nil {
			return false, false, newError(FeatureNotSupported,
				"with a SEARCH or CYCLE clause, the left side of the UNION must be a SELECT")
		}
		if so.Rhs.SetOp != nil {
			return false, false, newError(SyntaxError,
				"with a SEARCH or CYCLE clause, the right side of the UNION must be a SELECT")
		}
	}
	if so.Rhs.With != nil {
		return false, false, newError(FeatureNotSupported,
			"a nested WITH in the recursive term of a recursive query is not supported yet")
	}
	if so.Rhs.Select == nil {
		return false, false, newError(FeatureNotSupported,
			"a set operation in the recursive term of a recursive query is not supported yet")
	}
	if err := validateRecursiveTerm(name, so.Rhs.Select); err != nil {
		return false, false, err
	}
	return true, so.All, nil
}

// validateRecursiveTerm validates the recursive term (the UNION's right SELECT) of a recursive CTE
// (recursive-cte.md §6). The self-reference must appear exactly once, as a direct FROM/JOIN
// relation, not on the nullable side of an outer join; the term must contain no aggregate. The
// checks fire in PostgreSQL's order — a self-reference in a bad CONTEXT (a sublink, an outer join)
// is reported as that context even when a valid FROM reference also exists.
func validateRecursiveTerm(name string, sel *selectStmt) error {
	if countSublinkSelfRefs(sel, name) >= 1 {
		return newError(InvalidRecursion, fmt.Sprintf(
			"recursive reference to query %q must not appear within a subquery", name,
		))
	}
	if countFromSubquerySelfRefs(sel, name) >= 1 {
		return newError(FeatureNotSupported, fmt.Sprintf(
			"recursive reference to query %q inside a FROM subquery is not supported yet", name,
		))
	}
	direct := countDirectFromSelfRefs(sel, name)
	if direct > 1 {
		return newError(InvalidRecursion, fmt.Sprintf(
			"recursive reference to query %q must not appear more than once", name,
		))
	}
	if itemsHaveAggregate(sel.Items) || (sel.Having != nil && exprHasAggregate(*sel.Having)) {
		return newError(InvalidRecursion,
			"aggregate functions are not allowed in a recursive query's recursive term")
	}
	if direct == 1 && directSelfRefOnNullableSide(sel, name) {
		return newError(InvalidRecursion, fmt.Sprintf(
			"recursive reference to query %q must not appear within an outer join", name,
		))
	}
	return nil
}

// countSelfRefsQuery counts self-references to name anywhere in a query expression (deep — FROM
// relations at every nesting level plus expression sublinks).
func countSelfRefsQuery(qe queryExpr, name string) int {
	if qe.Select != nil {
		return countSelfRefsSelect(qe.Select, name)
	}
	if qe.SetOp != nil {
		return countSelfRefsQuery(qe.SetOp.Lhs, name) + countSelfRefsQuery(qe.SetOp.Rhs, name)
	}
	// A nested WITH inherits this binding until an inner same-named CTE is declared. That inner
	// binding shadows the outer one for later bodies and the main query, but its own non-recursive
	// body still sees the outer binding (cte.md §7).
	n := 0
	shadowed := false
	for i := range qe.With.Ctes {
		if !shadowed {
			n += countCteRefsDml(&qe.With.Ctes[i].Body, name)
		}
		if qe.With.Ctes[i].Name == name {
			shadowed = true
		}
	}
	if !shadowed {
		n += countSelfRefsQuery(*qe.With.Body, name)
	}
	return n
}

// countSelfRefsSelect counts self-references in a SELECT: its FROM relations (deep) plus all of its
// expressions' sublinks.
func countSelfRefsSelect(s *selectStmt, name string) int {
	n := 0
	for _, tref := range fromRelations(s) {
		n += countSelfRefsTableref(tref, name)
	}
	for _, e := range selectExprs(s) {
		n += countSelfRefsExpr(e, name)
	}
	return n
}

// countSelfRefsTableref counts self-references reachable through one FROM relation: a plain table
// reference with the matching name (+1), a derived-table subquery (recurse), or a table-function's
// / VALUES' argument exprs.
func countSelfRefsTableref(tref *tableRef, name string) int {
	if isPlainRelation(tref) {
		if tref.Name == name {
			return 1
		}
		return 0
	}
	n := 0
	if tref.Subquery != nil {
		n += countSelfRefsQuery(*tref.Subquery, name)
	}
	for _, a := range tref.Args {
		n += countSelfRefsExpr(*a, name)
	}
	for _, row := range tref.Values {
		for _, e := range row {
			n += countSelfRefsExpr(*e, name)
		}
	}
	return n
}

// countSelfRefsExpr counts self-references inside an expression — only reachable through a sublink
// (a subquery is an independent query whose own FROM may reference the CTE). The walk is exhaustive
// (like exprHasAggregate).
func countSelfRefsExpr(e exprNode, name string) int {
	switch e.Kind {
	case exprScalarSubquery, exprExists:
		return countSelfRefsQuery(*e.Subquery, name)
	case exprInSubquery:
		return countSelfRefsExpr(e.InSubquery.Lhs, name) + countSelfRefsQuery(e.InSubquery.Query, name)
	case exprQuantifiedSubquery:
		return countSelfRefsExpr(e.QuantifiedSubquery.Lhs, name) + countSelfRefsQuery(e.QuantifiedSubquery.Query, name)
	case exprCast:
		return countSelfRefsExpr(e.Cast.Inner, name)
	case exprExtract:
		return countSelfRefsExpr(e.Extract.Source, name)
	case exprCollate:
		return countSelfRefsExpr(e.Collate.Inner, name)
	case exprUnary:
		return countSelfRefsExpr(e.Unary.Operand, name)
	case exprIsNull:
		return countSelfRefsExpr(e.IsNullOf.Operand, name)
	case exprIsJson:
		return countSelfRefsExpr(e.IsJsonOf.Operand, name)
	case exprJsonCtor:
		return countSelfRefsExpr(e.JsonCtorOf.Operand, name)
	case exprJsonExists:
		n := countSelfRefsExpr(e.JsonExists.Ctx, name) + countSelfRefsExpr(e.JsonExists.Path, name)
		for _, p := range e.JsonExists.Passing {
			n += countSelfRefsExpr(p.Expr, name)
		}
		return n
	case exprJsonValue:
		n := countSelfRefsExpr(e.JsonValue.Ctx, name) + countSelfRefsExpr(e.JsonValue.Path, name)
		for _, p := range e.JsonValue.Passing {
			n += countSelfRefsExpr(p.Expr, name)
		}
		return n
	case exprJsonQuery:
		n := countSelfRefsExpr(e.JsonQuery.Ctx, name) + countSelfRefsExpr(e.JsonQuery.Path, name)
		for _, p := range e.JsonQuery.Passing {
			n += countSelfRefsExpr(p.Expr, name)
		}
		return n
	case exprBinary:
		return countSelfRefsExpr(e.Binary.Lhs, name) + countSelfRefsExpr(e.Binary.Rhs, name)
	case exprIsDistinct:
		return countSelfRefsExpr(e.IsDistinct.Lhs, name) + countSelfRefsExpr(e.IsDistinct.Rhs, name)
	case exprIn:
		n := countSelfRefsExpr(e.In.Lhs, name)
		for _, x := range e.In.List {
			n += countSelfRefsExpr(x, name)
		}
		return n
	case exprBetween:
		return countSelfRefsExpr(e.Between.Lhs, name) + countSelfRefsExpr(e.Between.Lo, name) + countSelfRefsExpr(e.Between.Hi, name)
	case exprLike:
		return countSelfRefsExpr(e.Like.Lhs, name) + countSelfRefsExpr(e.Like.Rhs, name)
	case exprRegex:
		return countSelfRefsExpr(e.Regex.Lhs, name) + countSelfRefsExpr(e.Regex.Rhs, name)
	case exprCase:
		n := 0
		if e.Case.Operand != nil {
			n += countSelfRefsExpr(*e.Case.Operand, name)
		}
		for _, w := range e.Case.Whens {
			n += countSelfRefsExpr(w.Cond, name) + countSelfRefsExpr(w.Result, name)
		}
		if e.Case.Els != nil {
			n += countSelfRefsExpr(*e.Case.Els, name)
		}
		return n
	case exprCoalesce:
		n := 0
		for _, a := range e.Coalesce {
			n += countSelfRefsExpr(a, name)
		}
		return n
	case exprGreatestLeast:
		n := 0
		for _, a := range e.GreatestLeast {
			n += countSelfRefsExpr(a, name)
		}
		return n
	case exprNullIf:
		n := 0
		for _, a := range e.NullIf {
			n += countSelfRefsExpr(a, name)
		}
		return n
	case exprFuncCall:
		n := 0
		for _, a := range e.FuncCall.Args {
			n += countSelfRefsExpr(*a, name)
		}
		return n
	case exprFieldAccess, exprFieldStar:
		return countSelfRefsExpr(*e.Base, name)
	case exprQualifiedStar:
		return 0 // a leaf relation reference — no sublink to recurse into

	case exprSubscript:
		n := countSelfRefsExpr(*e.Base, name)
		for _, sp := range e.Subscripts {
			for _, x := range subscriptSpecExprs(sp) {
				n += countSelfRefsExpr(*x, name)
			}
		}
		return n
	case exprRow, exprArray:
		n := 0
		for _, it := range e.RowItems {
			n += countSelfRefsExpr(it, name)
		}
		return n
	case exprQuantified:
		return countSelfRefsExpr(e.Quantified.Lhs, name) + countSelfRefsExpr(e.Quantified.Array, name)
	default:
		return 0
	}
}

// withHasDml reports whether a WITH statement contains any data-modifying part — a data-modifying
// CTE body or a data-modifying primary (spec/design/writable-cte.md). Such a statement runs through
// the writable-CTE orchestrator (the read pin + lexical-order, all-or-nothing execution); a
// pure-query WITH keeps the runWith path.
func withHasDml(wq *withQuery) bool {
	if wq.Body.IsDataModifying() {
		return true
	}
	for i := range wq.Ctes {
		if wq.Ctes[i].Body.IsDataModifying() {
			return true
		}
	}
	return false
}

// cteModes returns each CTE binding's evaluation mode (spec/design/cte.md §3, writable-cte.md §3): a
// RECURSIVE or data-modifying CTE is ALWAYS materialized; otherwise a MATERIALIZED hint or ≥2
// references → Materialize, else Inline.
func cteModes(bindings []*cteBinding) []cteMode {
	modes := make([]cteMode, len(bindings))
	for i, b := range bindings {
		switch {
		case b.recursive != nil || b.isDml():
			modes[i] = cteMaterialize
		case b.expanded:
			// A body planned at its reference is inlined there (planner.md §3.4), even if the
			// static count missed a reference the planner then resolved.
			modes[i] = cteInline
		case b.hint != nil && *b.hint:
			modes[i] = cteMaterialize
		case b.hint != nil && !*b.hint:
			modes[i] = cteInline
		case b.refs >= 2:
			modes[i] = cteMaterialize
		default:
			modes[i] = cteInline
		}
	}
	return modes
}

// addOutcomeCost adds extra cost to an outcome (the writable-CTE orchestrator folds the
// materialization cost of the data-modifying / query CTEs into the primary's result —
// spec/design/writable-cte.md §8).
func addOutcomeCost(outcome outcome, extra int64) outcome {
	outcome.Cost += extra
	return outcome
}

// countCteRefsDml counts references to CTE name reachable through a cte_body's inner queries — the
// writable-CTE analogue of countSelfRefsQuery (spec/design/writable-cte.md §3). A query body
// delegates to the query counter; a data-modifying body counts the references in its source query /
// WHERE / SET RHSs / ON CONFLICT / RETURNING sublinks. Used by the orchestrator to count the
// references a NON-planned data-modifying part contributes to the inline-vs-materialize decision.
func countCteRefsDml(body *cteBody, name string) int {
	switch {
	case body.Query != nil:
		return countSelfRefsQuery(*body.Query, name)
	case body.Insert != nil:
		ins := body.Insert
		n := 0
		if ins.Query != nil {
			n += countSelfRefsQuery(*ins.Query, name)
		}
		// A VALUES expression slot may hold a sublink (grammar.md §16).
		for _, values := range ins.Rows {
			for _, v := range values {
				if v.IsExpr {
					n += countSelfRefsExpr(*v.Expr, name)
				}
			}
		}
		if ins.OnConflict != nil && ins.OnConflict.DoUpdate {
			for i := range ins.OnConflict.Assignments {
				n += countSelfRefsExpr(ins.OnConflict.Assignments[i].Value, name)
			}
			if ins.OnConflict.Filter != nil {
				n += countSelfRefsExpr(*ins.OnConflict.Filter, name)
			}
		}
		return n + countReturningRefs(ins.Returning, name)
	case body.Update != nil:
		upd := body.Update
		n := 0
		for i := range upd.Assignments {
			n += countSelfRefsExpr(upd.Assignments[i].Value, name)
		}
		if upd.Filter != nil {
			n += countSelfRefsExpr(*upd.Filter, name)
		}
		return n + countReturningRefs(upd.Returning, name)
	default:
		del := body.Delete
		n := 0
		if del.Filter != nil {
			n += countSelfRefsExpr(*del.Filter, name)
		}
		return n + countReturningRefs(del.Returning, name)
	}
}

// countReturningRefs counts references to CTE name in a RETURNING item list's sublinks (the star
// form RETURNING * has no expressions, so it contributes none).
func countReturningRefs(returning *returningClause, name string) int {
	if returning == nil || returning.Items.All {
		return 0
	}
	n := 0
	for i := range returning.Items.Items {
		n += countSelfRefsExpr(returning.Items.Items[i].Expr, name)
	}
	return n
}

// countDirectFromSelfRefs counts self-references that are DIRECT FROM/JOIN relations of this SELECT
// (a plain table ref matching the name). This is the only valid position for a recursive reference.
func countDirectFromSelfRefs(s *selectStmt, name string) int {
	n := 0
	for _, tref := range fromRelations(s) {
		if isPlainRelation(tref) && tref.Name == name {
			n++
		}
	}
	return n
}

// countFromSubquerySelfRefs counts self-references nested inside a FROM-position subquery /
// table-function args / VALUES of this SELECT (the deferred 0A000 shape).
func countFromSubquerySelfRefs(s *selectStmt, name string) int {
	n := 0
	for _, tref := range fromRelations(s) {
		if !isPlainRelation(tref) {
			n += countSelfRefsTableref(tref, name)
		}
	}
	return n
}

// countSublinkSelfRefs counts self-references reachable only through an expression sublink in this
// SELECT's top-level expressions — the `within a subquery` position.
func countSublinkSelfRefs(s *selectStmt, name string) int {
	n := 0
	for _, e := range selectExprs(s) {
		n += countSelfRefsExpr(e, name)
	}
	return n
}

// directSelfRefOnNullableSide reports whether the SELECT's single direct self-reference sits on the
// NULLABLE side of an outer join — the position PostgreSQL rejects. The FROM is a left-deep chain:
// relation 0 is From, relation i+1 is Joins[i].Table, combined by Joins[i].Kind. A LEFT/FULL join
// makes its right operand nullable; a RIGHT/FULL join makes the whole accumulated left nullable.
func directSelfRefOnNullableSide(s *selectStmt, name string) bool {
	rels := fromRelations(s)
	nullable := make([]bool, len(rels))
	for j := range s.Joins {
		right := j + 1
		switch s.Joins[j].Kind {
		case joinLeft:
			nullable[right] = true
		case joinRight:
			for i := 0; i <= j; i++ {
				nullable[i] = true
			}
		case joinFull:
			for i := 0; i <= right; i++ {
				nullable[i] = true
			}
		}
	}
	for i, tref := range rels {
		if isPlainRelation(tref) && tref.Name == name && nullable[i] {
			return true
		}
	}
	return false
}

// isPlainRelation reports whether a FROM relation is a plain table NAME — not a derived-table
// subquery, a table function, or a VALUES body. Only a plain relation can resolve to a CTE.
func isPlainRelation(tref *tableRef) bool {
	return !tref.IsFunc && tref.Subquery == nil && tref.Values == nil
}

// fromRelations returns the FROM relations of a SELECT in left-deep order: From (if present) then
// each join's table.
func fromRelations(s *selectStmt) []*tableRef {
	rels := make([]*tableRef, 0, 1+len(s.Joins))
	if s.From != nil {
		rels = append(rels, s.From)
	}
	for i := range s.Joins {
		rels = append(rels, &s.Joins[i].Table)
	}
	return rels
}

// selectExprs returns every top-level expression of a SELECT that can hold a sublink (select items,
// WHERE, GROUP BY, HAVING, join ON conditions). ORDER BY keys are bare/qualified column references
// (never expressions), so they carry no sublink.
func selectExprs(s *selectStmt) []exprNode {
	var v []exprNode
	for _, it := range s.Items.Items {
		v = append(v, it.Expr)
	}
	if s.Filter != nil {
		v = append(v, *s.Filter)
	}
	for i := range s.GroupBy {
		s.GroupBy[i].forEachExpr(func(e *exprNode) {
			v = append(v, *e)
		})
	}
	if s.Having != nil {
		v = append(v, *s.Having)
	}
	for i := range s.Joins {
		if s.Joins[i].On != nil {
			v = append(v, *s.Joins[i].On)
		}
	}
	return v
}

// checkRecursiveColumnTypes checks a recursive CTE's column types (recursive-cte.md §2): the output
// types are FIXED by the non-recursive (anchor) term, and the recursive term's columns must be
// assignable to them — a literal adapts, an equal type passes, a WIDER type is 42804 (matching
// PostgreSQL). Mechanically the would-be UNION unified type must EQUAL the anchor type; any widening
// of the anchor is the error. An arity mismatch is 42601, like a plain UNION.
func checkRecursiveColumnTypes(anchorAST queryExpr, anchor, recursive *queryPlan, name string, unionAll bool, carried int) error {
	a := anchor.columnTypes()
	all := recursive.columnTypes()
	r := all[:len(all)-carried]
	if len(a) != len(r) {
		return newError(SyntaxError, "each UNION query must have the same number of columns")
	}
	for i := range a {
		// PostgreSQL reports a column the recursive term changes at the non-recursive term's column.
		atCol := int32(outputColumnLocation(anchorAST, i))
		unified, err := unifySetopColumn(a[i], r[i], setOpUnion)
		if err != nil {
			return err
		}
		if rtName(unified) != rtName(a[i]) {
			return newError(DatatypeMismatch, fmt.Sprintf(
				"recursive query %q column %d has type %s in non-recursive term but type %s overall",
				name, i+1, rtName(a[i]), rtName(unified),
			)).hinted(hintRecursiveTermType).withPosition(atCol)
		}
		// The non-recursive term also fixed the column's typmod, so the overall common typmod must
		// equal it (PostgreSQL compares type and typmod — create-table-as.md §2).
		atMod := anchor.columnTypmods()[i]
		overallMod := commonTypmod([]typmod{atMod, recursive.columnTypmods()[i]})
		if !overallMod.equal(atMod) {
			return newError(DatatypeMismatch, fmt.Sprintf(
				"recursive query %q column %d has type %s in non-recursive term but type %s overall",
				name, i+1, typmodTypeName(a[i], atMod), typmodTypeName(a[i], overallMod),
			)).hinted(hintRecursiveTermType).withPosition(atCol)
		}
		if !unionAll {
			if err := requireSetopEquality(a[i]); err != nil {
				return err
			}
		}
		// The non-recursive term fixed the relation's collation, so the UNION's overall common
		// collation must equal it (PostgreSQL's recursive-CTE check, spec/design/collation.md §1): a
		// recursive term cannot re-collate a column. Mismatch is 42P21.
		if a[i].collatable() {
			ad := anchor.columnDerivs()[i]
			overall, err := commonCollation([]deriv{ad, recursive.columnDerivs()[i]}, unionAll)
			if err != nil {
				return err
			}
			nonRec, all := collationIdentity(ad), collationIdentity(overall)
			if nonRec != all {
				return newError(CollationMismatch, fmt.Sprintf(
					"recursive query \"%s\" column %d has collation \"%s\" in non-recursive term but collation \"%s\" overall",
					name, i+1, nonRec, all,
				)).hinted(hintRecursiveTermCollation).withPosition(atCol)
			}
		}
	}
	return nil
}

// typmodTypeName is a resolved type's name with its typmod applied (decimal(10,2), varchar(5)), as a
// column of it would be declared.
func typmodTypeName(ty resolvedType, tm typmod) string {
	switch {
	case ty.kind == rtDecimal && tm.Decimal != nil:
		return fmt.Sprintf("decimal(%d,%d)", tm.Decimal.Precision, tm.Decimal.Scale)
	case ty.kind == rtText && tm.VarcharLen != nil:
		return fmt.Sprintf("varchar(%d)", *tm.VarcharLen)
	default:
		return rtName(ty)
	}
}

// collationIdentity is the collation a derivation settles to, named as PostgreSQL reports it (the
// recursive-CTE check, spec/design/collation.md §1): a collation's name, default for no collation
// (PG's default collation — a literal's), (null) for a conflicted / indeterminate one (PG's
// InvalidOid).
func collationIdentity(d deriv) string {
	switch d.kind {
	case derivImplicit, derivExplicit:
		return d.name
	case derivIndeterminate, derivConflicted:
		return "(null)"
	default:
		return "default"
	}
}

// cteSyntheticTable builds the synthetic relation a CTE reference resolves against
// (spec/design/cte.md §2): one column per body output, named by the rename list (a count mismatch is
// 42P10) or the body's own output names, typed from the planned body. The relation has no primary
// key / constraints — it is read-only and its rows come from the CTE context, never a store. Each
// column carries its body output's collation derivation (spec/design/collation.md §1 "Derived
// relations").
func cteSyntheticTable(name string, plan *queryPlan, rename []string) (*catTable, error) {
	return cteSyntheticTableCols(name, plan.columnNames(), plan.columnTypes(), plan.columnDerivs(), plan.columnTypmods(), rename)
}

// cteSyntheticTableCols is the shared core of cteSyntheticTable, over explicit body column names +
// types + collation derivations + typmods (nil = none for every column) — so a data-modifying CTE
// (whose "body output" is its RETURNING projection, not a queryPlan) builds its synthetic relation the
// same way (spec/design/writable-cte.md §1). Each column records its body output's typmod, so a
// reference to it keeps the typmod (spec/design/create-table-as.md §2).
func cteSyntheticTableCols(name string, bodyNames []string, bodyTypes []resolvedType, bodyDerivs []deriv, bodyTypmods []typmod, rename []string) (*catTable, error) {
	var colNames []string
	if rename != nil {
		// PostgreSQL allows FEWER aliases than the body has columns — the first len(rename) columns
		// take the aliases, the rest keep their body output names (a partial rename). Only MORE
		// aliases than columns is an error (42P10).
		if len(rename) > len(bodyTypes) {
			return nil, newError(InvalidColumnReference, fmt.Sprintf(
				"WITH query \"%s\" has %d columns available but %d columns specified",
				name, len(bodyTypes), len(rename),
			))
		}
		colNames = make([]string, len(bodyTypes))
		for i := range bodyTypes {
			if i < len(rename) {
				colNames[i] = rename[i]
			} else {
				colNames[i] = bodyNames[i]
			}
		}
	} else {
		colNames = append([]string(nil), bodyNames...)
	}
	columns := make([]catColumn, len(colNames))
	for i, n := range colNames {
		ty, err := typeFromResolved(bodyTypes[i])
		if err != nil {
			return nil, err
		}
		columns[i] = catColumn{Name: n, Type: ty}
		var d deriv
		if bodyDerivs != nil {
			d = bodyDerivs[i]
		}
		setSyntheticCollation(&columns[i], d)
		if bodyTypmods != nil {
			columns[i].Decimal = bodyTypmods[i].Decimal
			columns[i].VarcharLen = bodyTypmods[i].VarcharLen
			columns[i].Temporal = bodyTypmods[i].Temporal
		}
	}
	return &catTable{Name: name, Columns: columns}, nil
}

// setSyntheticCollation records a body output's collation derivation on its synthetic column
// (spec/design/collation.md §1 "Derived relations", PostgreSQL's rule): an implicit OR explicit body
// collation becomes the column's IMPLICIT collation (an outer reference is a plain column reference);
// any other body output has no collation and takes a neighbour's (PG's default). An indeterminate or
// conflicted body output has none either (PG's InvalidOid) — it still yields to a neighbour, but
// consuming it alone is 42P22.
func setSyntheticCollation(col *catColumn, d deriv) {
	switch d.kind {
	case derivImplicit, derivExplicit:
		if d.name != "C" {
			col.Collation = d.name
		}
	case derivIndeterminate, derivConflicted:
		col.CollationConflict = true
	default:
		col.NoCollation = true
	}
}

// typeFromResolved is the catalog Type for a resolved expression type — used to give a CTE's
// synthetic columns a Type (spec/design/cte.md). An untyped NULL column maps to text (PostgreSQL's
// unknown -> text rule). A decimal's per-column typmod is irrelevant for a read-only CTE column
// (values flow through unchanged), so it is dropped. An anonymous ROW(...) composite becomes a record
// Type carrying its field list (spec/design/composite.md §1).
func typeFromResolved(rt resolvedType) (dataType, error) {
	switch rt.kind {
	case rtInt:
		return scalarT(rt.intTy), nil
	case rtFloat32:
		return scalarT(scalarFloat32), nil
	case rtFloat64:
		return scalarT(scalarFloat64), nil
	case rtBool:
		return scalarT(scalarBool), nil
	case rtText, rtNull:
		return scalarT(scalarText), nil
	case rtDecimal:
		return scalarT(scalarDecimal), nil
	case rtBytea:
		return scalarT(scalarBytea), nil
	case rtUuid:
		return scalarT(scalarUuid), nil
	case rtTimestamp:
		return scalarT(scalarTimestamp), nil
	case rtTimestamptz:
		return scalarT(scalarTimestamptz), nil
	case rtDate:
		return scalarT(scalarDate), nil
	case rtInterval:
		return scalarT(scalarInterval), nil
	case rtJson:
		return scalarT(scalarJson), nil
	case rtJsonb:
		return scalarT(scalarJsonb), nil
	case rtJsonPath:
		return scalarT(scalarJsonPath), nil
	case rtComposite:
		if rt.comp != nil && rt.comp.named {
			return compositeT(rt.comp.name), nil
		}
		fields := make([]recordField, len(rt.comp.fields))
		for i, f := range rt.comp.fields {
			ft, err := typeFromResolved(f.ty)
			if err != nil {
				return dataType{}, err
			}
			fields[i] = recordField{Name: f.name, Type: ft}
		}
		return recordT(fields), nil
	case rtArray:
		elem, err := typeFromResolved(*rt.elem)
		if err != nil {
			return dataType{}, err
		}
		return arrayT(elem), nil
	case rtRange:
		elem, err := typeFromResolved(*rt.elem)
		if err != nil {
			return dataType{}, err
		}
		return rangeT(elem), nil
	default:
		return dataType{}, newError(FeatureNotSupported, "unsupported CTE column type")
	}
}

// runQueryExpr runs a query expression to a selectResult — a lone SELECT via runSelect, or a set
// operation via runSetOp (recursively, so a chain `a UNION b INTERSECT c` evaluates as the parsed
// precedence tree).
// runQueryExpr is the top-level orchestrator (spec/design/grammar.md §26): PLAN the whole
// expression tree once against an empty scope chain (threading one paramTypes so $N inference is
// statement-wide), bind the parameters, then the foldUncorrelated pass executes each
// globally-uncorrelated subquery once and folds it to a constant (preserving the once-only cost),
// and finally EXECUTE against an empty outer-row environment. Correlated subqueries that survive
// the fold are re-executed per outer row by the evaluator.
func (db *engine) runQueryExpr(qe queryExpr, params []Value) (selectResult, error) {
	planning := db.beginPlanning()
	defer planning.end()
	ptypes := &paramTypes{}
	plan, err := db.planQuery(qe, nil, nil, ptypes)
	if err != nil {
		return selectResult{}, err
	}
	ptys, err := ptypes.finalize()
	if err != nil {
		return selectResult{}, err
	}
	bound, err := bindParams(params, ptys, db.inputZone())
	if err != nil {
		return selectResult{}, err
	}
	// A plan that read a parameter's value is planned again with the bound values: the custom plan
	// (estimator.md §6.1).
	if planning.bind(bound) {
		if plan, err = db.planQuery(qe, nil, nil, &paramTypes{}); err != nil {
			return selectResult{}, err
		}
	}
	var subqueryCost int64
	if err := db.foldUncorrelatedInPlan(&plan, bound, cteCtx{}, &subqueryCost); err != nil {
		return selectResult{}, err
	}
	r, err := db.execQueryPlan(&plan, nil, bound, cteCtx{})
	if err != nil {
		return selectResult{}, err
	}
	r.cost += subqueryCost
	return r, nil
}

// runSelect runs a lone SELECT — the entry point executeSelect and INSERT ... SELECT use.
func (db *engine) runSelect(sel *selectStmt, params []Value) (selectResult, error) {
	return db.runQueryExpr(queryExpr{Select: sel}, params)
}

// runSetOp runs a set operation as a top-level statement.
func (db *engine) runSetOp(so *setOp, params []Value) (selectResult, error) {
	return db.runQueryExpr(queryExpr{SetOp: so}, params)
}

// planQuery resolves a query expression into an owned queryPlan against the scope chain (parent
// = the enclosing query's scope, nil at top level). ctes are the statement's CTE bindings visible
// here (spec/design/cte.md §2), empty for a non-WITH statement. A subquery is planned here, once
// (§26).
func (db *engine) planQuery(qe queryExpr, parent *scope, ctes []*cteBinding, ptypes *paramTypes) (queryPlan, error) {
	if qe.Select != nil {
		sp, err := db.planSelect(qe.Select, parent, ctes, ptypes)
		if err != nil {
			return queryPlan{}, err
		}
		return queryPlan{sel: sp}, nil
	}
	if qe.With != nil {
		wp, err := db.planWithExpr(qe.With, parent, ctes, ptypes)
		if err != nil {
			return queryPlan{}, err
		}
		return queryPlan{with: wp}, nil
	}
	sop, err := db.planSetOp(qe.SetOp, parent, ctes, ptypes)
	if err != nil {
		return queryPlan{}, err
	}
	return queryPlan{setop: sop}, nil
}

// planWithExpr plans a nested `WITH … query_expr` (spec/design/cte.md §7) into a withPlan. Its CTEs
// inherit the enclosing bindings and shadow same-named bindings from their declaration onward. The
// inner main query and the CTE bodies are both planned against the enclosing parent (so a LATERAL
// derived-table body still correlates to its left siblings, and a CTE body may read an enclosing
// query level — PostgreSQL; it never sees the owning query's FROM). A data-modifying CTE here is rejected 0A000 — PostgreSQL restricts a DML-WITH to the top level.
func (db *engine) planWithExpr(we *withExpr, parent *scope, ctes []*cteBinding, ptypes *paramTypes) (*withPlan, error) {
	for i := range we.Ctes {
		if we.Ctes[i].Body.IsDataModifying() {
			return nil, newError(FeatureNotSupported,
				fmt.Sprintf("WITH clause containing a data-modifying statement (%s) is only supported at the top level", we.Ctes[i].Name))
		}
	}
	bindings, err := db.planCteBindings(we.Ctes, we.Recursive, ctes, parent, ptypes, ctePrimary{query: we.Body})
	if err != nil {
		return nil, err
	}
	visible := append(append([]*cteBinding{}, ctes...), bindings...)
	body, err := db.planQuery(*we.Body, parent, visible, ptypes)
	if err != nil {
		return nil, err
	}
	return &withPlan{bindings: bindings, modes: cteModes(bindings), body: body, inheritedLen: len(ctes)}, nil
}

// execQueryPlan executes a resolved plan against an outer-row environment (outer = the enclosing
// rows, innermost last; nil at top level) and the bound parameters. ctes is the per-statement CTE
// execution context (spec/design/cte.md §5), the zero cteCtx for a non-WITH statement.
func (db *engine) execQueryPlan(plan *queryPlan, outer []storedRow, params []Value, ctes cteCtx) (selectResult, error) {
	db.explainActual.beginFrame()
	defer db.explainActual.endFrame()
	if plan.sel != nil {
		return db.execSelectPlan(plan.sel, outer, params, ctes)
	}
	if plan.values != nil {
		return db.execValuesPlan(plan.values, outer, params, ctes)
	}
	if plan.with != nil {
		return db.execWithPlan(plan.with, outer, params, ctes)
	}
	return db.execSetOpPlan(plan.setop, outer, params, ctes)
}

// execHiddenQueryPlan executes an expression subquery whose operators are not rendered as separate
// EXPLAIN rows. Its returned cost still accrues into the containing visible operator checkpoint.
func (db *engine) execHiddenQueryPlan(plan *queryPlan, outer []storedRow, params []Value, ctes cteCtx) (selectResult, error) {
	db.explainActual.beginFrame()
	defer db.explainActual.discardFrame()
	return db.execQueryPlan(plan, outer, params, ctes)
}

// execWithPlan executes a nested WITH plan (spec/design/cte.md §7): retain the exact inherited
// prefix visible at plan time, materialize the local bindings once, layer them over that prefix, and
// run the inner body. The body still sees the outer row environment (so a
// LATERAL nested-WITH derived-table body correlates to its left siblings). The materialization cost
// folds into the body's cost — the same shape as the top-level runWith (cte.md §3).
func (db *engine) execWithPlan(wp *withPlan, outer []storedRow, params []Value, ctes cteCtx) (selectResult, error) {
	inherited := ctes.prefix(wp.inheritedLen)
	buffers, totalCost, err := db.materializeCtes(wp.bindings, wp.modes, params, inherited, outer)
	if err != nil {
		return selectResult{}, err
	}
	ctx := inherited.extend(outer, wp.modes, wp.bindings, buffers)
	r, err := db.execQueryPlan(&wp.body, outer, params, ctx)
	if err != nil {
		return selectResult{}, err
	}
	releaseCteBuffers(db.session.queryAccount(), buffers)
	r.cost += totalCost
	db.explainActual.recordParent("WITH", r.cost)
	return r, nil
}

// planSetOp plans a set operation (spec/design/grammar.md §25): plan both operands with the same
// parent scope, check arity + unify column types up front (so the 42601/42804 fire even over
// empty operands), and resolve the trailing ORDER BY by output column name.
func (db *engine) planSetOp(so *setOp, parent *scope, ctes []*cteBinding, ptypes *paramTypes) (*setOpPlan, error) {
	lhs, err := db.planQuery(so.Lhs, parent, ctes, ptypes)
	if err != nil {
		return nil, err
	}
	rhs, err := db.planQuery(so.Rhs, parent, ctes, ptypes)
	if err != nil {
		return nil, err
	}

	if len(lhs.columnTypes()) != len(rhs.columnTypes()) {
		return nil, newError(SyntaxError, fmt.Sprintf(
			"each %s query must have the same number of columns", setopName(so.Op),
		))
	}
	columnTypes := make([]resolvedType, len(lhs.columnTypes()))
	for i := range columnTypes {
		t, err := unifySetopColumn(lhs.columnTypes()[i], rhs.columnTypes()[i], so.Op)
		if err != nil {
			// A mismatch is located at the right operand's column (PostgreSQL's select_common_type).
			return nil, positionError(err, outputColumnLocation(so.Rhs, i))
		}
		columnTypes[i] = t
	}
	// Every form but UNION ALL matches rows, so each column needs an equality operator (42883 for
	// json / jsonpath, as PostgreSQL).
	if so.Op != setOpUnion || !so.All {
		// PostgreSQL reports a missing equality operator at the left operand's column.
		for i, t := range columnTypes {
			if err := requireSetopEquality(t); err != nil {
				return nil, positionError(err, outputColumnLocation(so.Lhs, i))
			}
		}
	}
	columnNames := lhs.columnNames()
	// Each text column's common collation (spec/design/collation.md §1 "Set operations",
	// PostgreSQL): an implicit conflict is 42P21 except under UNION ALL, whose column then has none
	// (42P22 only when consumed).
	noneOK := so.Op == setOpUnion && so.All
	columnDerivs := make([]deriv, len(columnTypes))
	for i, t := range columnTypes {
		if !t.collatable() {
			continue
		}
		d, err := commonCollation([]deriv{lhs.columnDerivs()[i], rhs.columnDerivs()[i]}, noneOK)
		if err != nil {
			return nil, err
		}
		columnDerivs[i] = d
	}
	// Each column keeps the operands' typmod when they agree (PostgreSQL's select_common_typmod,
	// create-table-as.md §2).
	columnTypmods := make([]typmod, len(columnTypes))
	for i := range columnTypes {
		columnTypmods[i] = commonTypmod([]typmod{lhs.columnTypmods()[i], rhs.columnTypmods()[i]})
	}

	order := make([]orderSlot, 0, len(so.OrderBy))
	for i := range so.OrderBy {
		key := &so.OrderBy[i]
		idx, err := resolveSetopOrderKey(key, columnNames)
		if err != nil {
			return nil, err
		}
		// An explicit COLLATE on a set-operation ORDER BY key (spec/design/collation.md §1): the
		// output column must be text (42804); the name resolves ("C", else loaded or 42704). Absent
		// one, a text key sorts by the column's common collation.
		var coll *Collation
		if key.Collation == "" && columnTypes[idx].collatable() {
			if coll, err = resolveDeriv(db, columnDerivs[idx]); err != nil {
				return nil, err
			}
		} else if key.Collation != "" {
			if !columnTypes[idx].collatable() {
				return nil, typeError("collations are not supported by this column's type")
			}
			if coll, err = resolveCollationName(db, key.Collation); err != nil {
				return nil, err
			}
		}
		order = append(order, orderSlot{idx: idx, descending: key.Descending, nullsFirst: key.NullsFirst, collation: coll})
	}

	return &setOpPlan{
		op: so.Op, all: so.All, lhs: lhs, rhs: rhs,
		columnNames: columnNames, columnTypes: columnTypes, columnDerivs: columnDerivs, columnTypmods: columnTypmods,
		order: order, limit: so.Limit, offset: so.Offset,
	}, nil
}

// execSetOpPlan executes a resolved set operation: run both operands against the outer
// environment, coerce to the unified types, combine, then sort + window. Cost is lhs.cost +
// rhs.cost — the combine, sort, and window are unmetered (cost.md §3).
func (db *engine) execSetOpPlan(plan *setOpPlan, outer []storedRow, params []Value, ctes cteCtx) (selectResult, error) {
	left, err := db.execQueryPlan(&plan.lhs, outer, params, ctes)
	if err != nil {
		return selectResult{}, err
	}
	right, err := db.execQueryPlan(&plan.rhs, outer, params, ctes)
	if err != nil {
		return selectResult{}, err
	}

	// Both arms arrive charged (memory.md §5.2). Coercion growth is reserved; rows the combine or the
	// LIMIT/OFFSET window drops are released (§5.3).
	acct := db.session.queryAccount()
	before := measureRows(acct, left.rows) + measureRows(acct, right.rows)
	coerceSetopRows(left.rows, left.columnTypes, plan.columnTypes)
	coerceSetopRows(right.rows, right.columnTypes, plan.columnTypes)
	coerced := measureRows(acct, left.rows) + measureRows(acct, right.rows)
	if err := acct.reserve(coerced - before); err != nil {
		return selectResult{}, err
	}

	rows, err := combineSetop(plan.op, plan.all, left.rows, right.rows, acct)
	if err != nil {
		return selectResult{}, err
	}
	acct.release(coerced - measureRows(acct, rows))
	cost := left.cost + right.cost
	rootNode := setOpNodeName(plan.op)
	if plan.limit != nil || plan.offset != nil {
		rootNode = "Limit"
	} else if len(plan.order) > 0 {
		rootNode = "Sort"
	}
	if rootNode != setOpNodeName(plan.op) {
		db.explainActual.recordParent(setOpNodeName(plan.op), cost)
	}

	if len(plan.order) > 0 {
		if err := sortRows(rows, plan.order, acct); err != nil {
			return selectResult{}, err
		}
		if rootNode != "Sort" {
			db.explainActual.recordParent("Sort", cost)
		}
	}

	n := int64(len(rows))
	start := int64(0)
	if plan.offset != nil && *plan.offset < n {
		start = *plan.offset
	} else if plan.offset != nil {
		start = n
	}
	end := n
	if plan.limit != nil && *plan.limit < n-start {
		end = start + *plan.limit
	}
	acct.release(measureRows(acct, rows[:start]) + measureRows(acct, rows[end:]))
	rows = rows[start:end]
	db.explainActual.recordParent(rootNode, cost)

	return selectResult{columnNames: plan.columnNames, columnTypes: plan.columnTypes, rows: rows, cost: cost}, nil
}

// planValues resolves a VALUES-body relation into a *valuesPlan (spec/design/grammar.md §42) — the
// body of a FROM (VALUES …) derived table. Each value resolves against an EMPTY scope whose parent is
// the enclosing query's scope: a non-LATERAL body sees no FROM sibling (42703/42P01) but correlates to
// an enclosing query level, and an aggregate is 42803; it still sees the statement's CTE bindings (an
// uncorrelated subquery inside a value resolves like anywhere). Every row must have the same arity (42601); the columns' types
// unify across rows like a set operation (42804 on a mismatch). A bind parameter is then noted at
// its column's unified type (so VALUES (1),($1) types $1 as int); a column with no concrete type —
// all NULL/param — leaves its $N untyped, surfacing 42P18 at finalize (jed's no-cross-context
// inference posture, §26).
func (db *engine) planValues(rows [][]*exprNode, parent *scope, ctes []*cteBinding, ptypes *paramTypes) (*valuesPlan, error) {
	arity := len(rows[0]) // the parser guarantees at least one row, each with at least one value
	// No local relations. A column reference correlates through parent — the enclosing query's scope
	// (§42), or for a LATERAL VALUES body (§44) the earlier FROM relations first — else is unresolved. CTE bindings stay visible and
	// subqueries are allowed (an uncorrelated one folds before the rows run).
	s := &scope{parent: parent, catalog: db, allowSubquery: true, ctes: ctes}
	resolvedRows := make([][]*rExpr, len(rows))
	colTypes := make([]resolvedType, arity)
	// Per column: the 0-based bind-parameter slots appearing in it, typed in a second pass from the
	// unified column type (a $N takes its column's type, like a set-operation operand).
	colParams := make([][]int, arity)
	// Per column: each row's collation derivation, combined after the loop (collation.md §1).
	colDerivs := make([][]deriv, arity)
	// Per column: each row's typmod, combined after the loop (create-table-as.md §2).
	colTypmods := make([][]typmod, arity)
	for ri, row := range rows {
		if len(row) != arity {
			return nil, newError(SyntaxError, "VALUES lists must all be the same length")
		}
		resolvedRow := make([]*rExpr, arity)
		for ci, val := range row {
			node, ty, err := resolve(s, *val, nil, &aggCtx{}, ptypes) // forbidden: an aggregate is 42803
			if err != nil {
				return nil, err
			}
			if node.kind == reParam {
				colParams[ci] = append(colParams[ci], node.index)
			}
			d, err := deriveCollation(s, *val)
			if err != nil {
				return nil, err
			}
			colDerivs[ci] = append(colDerivs[ci], d)
			colTypmods[ci] = append(colTypmods[ci], deriveTypmod(s, *val))
			if ri == 0 {
				colTypes[ci] = ty
			} else {
				u, err := unifyValuesColumn(colTypes[ci], ty)
				if err != nil {
					return nil, positionError(err, exprLocation(*val))
				}
				colTypes[ci] = u
			}
			resolvedRow[ci] = node
		}
		resolvedRows[ri] = resolvedRow
	}
	// Second pass: note each column's bind parameters at the unified column type. A column with no
	// scalar type (all NULL/param) passes nil — the parameter stays untyped (42P18).
	for ci := range colParams {
		hint := scalarForParamHint(colTypes[ci])
		for _, idx0 := range colParams[ci] {
			if err := ptypes.note(idx0, hint); err != nil {
				return nil, err
			}
		}
	}
	// PostgreSQL names a VALUES relation's columns column1, column2, … ; the derived table's optional
	// column-rename list overrides them at the synthetic relation (cteSyntheticTable).
	colNames := make([]string, arity)
	for i := range colNames {
		colNames[i] = fmt.Sprintf("column%d", i+1)
	}
	// Each text column's common collation across the rows — PostgreSQL treats a VALUES column like a
	// UNION ALL one (an implicit conflict leaves it with none, never an error).
	colDerivsOut := make([]deriv, arity)
	for ci, t := range colTypes {
		if !t.collatable() {
			continue
		}
		d, err := commonCollation(colDerivs[ci], true)
		if err != nil {
			return nil, err
		}
		colDerivsOut[ci] = d
	}
	colTypmodsOut := make([]typmod, arity)
	for ci := range colTypmodsOut {
		colTypmodsOut[ci] = commonTypmod(colTypmods[ci])
	}
	return &valuesPlan{rows: resolvedRows, columnTypes: colTypes, columnNames: colNames, columnDerivs: colDerivsOut, columnTypmods: colTypmodsOut}, nil
}

// execValuesPlan executes a resolved VALUES-body relation (spec/design/grammar.md §42): evaluate
// each row's values as constants over an EMPTY environment (no local row, no outer row —
// non-LATERAL), coerce each to the unified column type (the only runtime changes are int ->
// decimal and f32 -> f64, the set-operation rule), and emit the rows. Charges row_produced per row plus each value's
// operator_eval (the evaluator) — the derived table's intrinsic cost (cost.md §3), folded into the
// caller's meter via execQueryPlan.
func (db *engine) execValuesPlan(plan *valuesPlan, outer []storedRow, params []Value, ctes cteCtx) (selectResult, error) {
	env := &evalEnv{exec: db, params: params, outer: outer, rng: newStmtRng(), ctes: ctes}
	meter := db.session.newMeter()
	rows := make([][]Value, 0, len(plan.rows))
	for _, row := range plan.rows {
		if err := meter.Step(); err != nil { // enforce the cost ceiling per produced row (CLAUDE.md §13)
			return selectResult{}, err
		}
		meter.Charge(costs.RowProduced)
		out := make([]Value, len(plan.columnTypes))
		for ci, e := range row {
			v, err := e.eval(nil, env, meter)
			if err != nil {
				return selectResult{}, err
			}
			// Widen to the column's unified type (the set-operation rule): int → decimal, f32 →
			// f64; every other unified type is a value no-op.
			out[ci] = widenToUnified(v, plan.columnTypes[ci])
		}
		if err := meter.admitRow(out); err != nil { // a materialized relation row (memory.md §5.1)
			return selectResult{}, err
		}
		rows = append(rows, out)
	}
	db.explainActual.recordParent("Values", meter.Accrued)
	return selectResult{columnNames: plan.columnNames, columnTypes: plan.columnTypes, rows: rows, cost: meter.Accrued}, nil
}

// setopName is the operator's name for an error message (PostgreSQL phrasing).
func setopName(op setOpKind) string {
	switch op {
	case setOpUnion:
		return "UNION"
	case setOpIntersect:
		return "INTERSECT"
	default:
		return "EXCEPT"
	}
}

// unifySetopColumn unifies one output column's type across the two operands of a set operation
// (spec/design/grammar.md §25, commonType); a pair with no common type is 42804. An all-NULL column
// stays rtNull (PostgreSQL would call a top-level one text, but the type is never observed in
// output).
// outputColumnLocation is the location of a query's output column i — a set-operation operand, an
// INSERT … SELECT source (spec/design/error-fields.md §10.2): the select-list expression of a plain
// SELECT; unknown (0) for a `*` list or any other operand.
// outputColumnIsStringLiteral reports whether output column i of an INSERT ... SELECT source is a
// bare string literal that PostgreSQL leaves `unknown` for the target column to type: an item of a
// plain SELECT's list (also under a nested WITH) that no ORDER BY or GROUP BY key names by ordinal or
// alias. A set operation, a VALUES query, SELECT DISTINCT, or a sort/grouping key on the item
// resolves it to text first (spec/design/grammar.md §24).
func outputColumnIsStringLiteral(q queryExpr, i int) bool {
	if q.With != nil {
		return outputColumnIsStringLiteral(*q.With.Body, i)
	}
	s := q.Select
	if s == nil || s.Distinct || s.Items.All || i >= len(s.Items.Items) {
		return false
	}
	item := s.Items.Items[i]
	if item.Expr.Kind != exprLiteral || item.Expr.Literal.Kind != literalText {
		return false
	}
	ordinal := int64(i + 1)
	isAlias := func(name string) bool { return item.Alias != nil && *item.Alias == name }
	for _, k := range s.OrderBy {
		if (k.Ordinal != nil && *k.Ordinal == ordinal) ||
			(k.Ordinal == nil && k.Expr == nil && k.Qualifier == "" && isAlias(k.Column)) {
			return false
		}
	}
	grouped := false
	for gi := range s.GroupBy {
		s.GroupBy[gi].forEachExpr(func(e *exprNode) {
			switch {
			case e.Kind == exprLiteral && e.Literal.Kind == literalInt && e.Literal.Int == ordinal:
				grouped = true
			case e.Kind == exprColumn && isAlias(e.Column):
				grouped = true
			}
		})
	}
	return !grouped
}

func outputColumnLocation(q queryExpr, i int) srcPos {
	if q.Select == nil || q.Select.Items.All || i >= len(q.Select.Items.Items) {
		return 0
	}
	return exprLocation(q.Select.Items.Items[i].Expr)
}

func unifySetopColumn(a, b resolvedType, op setOpKind) (resolvedType, error) {
	if t, ok := commonType(a, b); ok {
		return t, nil
	}
	return resolvedType{}, newError(DatatypeMismatch, fmt.Sprintf(
		"%s types %s and %s cannot be matched", setopName(op), rtName(a), rtName(b),
	))
}

// unifyValuesColumn unifies two row value types for the SAME VALUES-body column
// (spec/design/grammar.md §42) by the set-operation rule (§25, commonType); a pair with no common
// type is 42804.
func unifyValuesColumn(a, b resolvedType) (resolvedType, error) {
	if t, ok := commonType(a, b); ok {
		return t, nil
	}
	return resolvedType{}, newError(DatatypeMismatch, fmt.Sprintf(
		"VALUES types %s and %s cannot be matched", rtName(a), rtName(b),
	))
}

// requireSetopEquality: a set operation that MATCHES rows (every form but UNION ALL, and a UNION
// recursive CTE) needs an equality operator on each output column (spec/design/grammar.md §25). json
// and jsonpath ship none — nor does an array or composite containing one — so such a column is
// 42883, exactly PostgreSQL's "could not identify an equality operator". The comparability matrix
// decides it.
func requireSetopEquality(t resolvedType) error {
	if classifyComparable(t, t) != nil {
		return newError(UndefinedFunction, "could not identify an equality operator for type "+rtName(t)).noHint()
	}
	return nil
}

// widenToUnified widens one value to a unified column type — the value side of commonType: an
// integer becomes a decimal (scale 0) and an f32 becomes the exact f64; every other unification is a
// value no-op (integer-width promotion is free — all integers are i64).
func widenToUnified(v Value, to resolvedType) Value {
	switch {
	case to.kind == rtDecimal && v.Kind == ValInt:
		return DecimalValue(decimalFromInt64(v.Int))
	case to.kind == rtFloat64 && v.Kind == ValFloat32:
		return Float64Value(float64(v.F32()))
	}
	return v
}

// scalarForParamHint is the scalar type to note a bind parameter at, given its VALUES column's
// unified type (spec/design/grammar.md §42). A scalar type flows through; a NULL / composite / array
// column has no scalar parameter type, so nil is returned and the parameter stays untyped (42P18 at
// finalize).
func scalarForParamHint(rt resolvedType) *scalarType {
	switch rt.kind {
	case rtInt:
		t := rt.intTy // rtInt carries its width in intTy
		return &t
	case rtFloat32:
		t := scalarFloat32
		return &t
	case rtFloat64:
		t := scalarFloat64
		return &t
	case rtBool:
		t := scalarBool
		return &t
	case rtText:
		t := scalarText
		return &t
	case rtDecimal:
		t := scalarDecimal
		return &t
	case rtBytea:
		t := scalarBytea
		return &t
	case rtUuid:
		t := scalarUuid
		return &t
	case rtTimestamp:
		t := scalarTimestamp
		return &t
	case rtTimestamptz:
		t := scalarTimestamptz
		return &t
	case rtDate:
		t := scalarDate
		return &t
	case rtInterval:
		t := scalarInterval
		return &t
	case rtJson:
		t := scalarJson
		return &t
	case rtJsonb:
		t := scalarJsonb
		return &t
	case rtJsonPath:
		t := scalarJsonPath
		return &t
	default:
		return nil
	}
}

// coerceSetopRows converts each row's values in place to the unified set-operation column types
// (widenToUnified — integer → decimal, f32 → f64; a NULL stays NULL). Only columns whose type
// actually changed are touched.
func coerceSetopRows(rows [][]Value, from, to []resolvedType) {
	for i := range to {
		widens := (from[i].kind == rtInt && to[i].kind == rtDecimal) ||
			(from[i].kind == rtFloat32 && to[i].kind == rtFloat64)
		if !widens {
			continue
		}
		for r := range rows {
			rows[r][i] = widenToUnified(rows[r][i], to[i])
		}
	}
}

// combineSetop combines the operands' rows per the set operator + ALL flag (spec/design/grammar.md
// §25). Rows match by the NULL-safe, value-canonical distinctRowKey (two NULLs match, 1.5 == 1.50,
// and a converted int matches the decimal). The emitted representative for a matched / deduplicated
// key is its FIRST occurrence scanning the LEFT operand then the right, and emitted rows keep that
// left-then-right scan order — deterministic and identical across cores. (A later ORDER BY
// re-sorts; without one, output order is unspecified and the corpus compares rowsort.)
func combineSetop(op setOpKind, all bool, left, right [][]Value, acct queryAccount) ([][]Value, error) {
	// The dedup / count tables are operator state (memory.md §6.2): one entry_bytes per distinct
	// right-arm row of INTERSECT/EXCEPT (built first), then per row a distinct UNION/INTERSECT/EXCEPT
	// keeps — all returned when the combine completes.
	charge := newStateCharge(acct)
	defer charge.releaseAll()
	entry := func(row []Value) error {
		if charge.active() {
			return charge.reserveDirect(memEntryBytes(row))
		}
		return nil
	}
	switch {
	case op == setOpUnion && all:
		out := make([][]Value, 0, len(left)+len(right))
		out = append(out, left...)
		out = append(out, right...)
		return out, nil
	case op == setOpUnion:
		seen := make(map[string]bool)
		out := make([][]Value, 0)
		for _, row := range left {
			if k := distinctRowKey(row); !seen[k] {
				seen[k] = true
				if err := entry(row); err != nil {
					return nil, err
				}
				out = append(out, row)
			}
		}
		for _, row := range right {
			if k := distinctRowKey(row); !seen[k] {
				seen[k] = true
				if err := entry(row); err != nil {
					return nil, err
				}
				out = append(out, row)
			}
		}
		return out, nil
	case op == setOpIntersect && all:
		counts := make(map[string]int)
		for _, row := range right {
			k := distinctRowKey(row)
			if _, ok := counts[k]; !ok {
				if err := entry(row); err != nil {
					return nil, err
				}
			}
			counts[k]++
		}
		out := make([][]Value, 0)
		for _, row := range left {
			k := distinctRowKey(row)
			if counts[k] > 0 {
				counts[k]--
				out = append(out, row)
			}
		}
		return out, nil
	case op == setOpIntersect:
		rightSet := make(map[string]bool)
		for _, row := range right {
			k := distinctRowKey(row)
			if !rightSet[k] {
				if err := entry(row); err != nil {
					return nil, err
				}
				rightSet[k] = true
			}
		}
		emitted := make(map[string]bool)
		out := make([][]Value, 0)
		for _, row := range left {
			k := distinctRowKey(row)
			if rightSet[k] && !emitted[k] {
				emitted[k] = true
				if err := entry(row); err != nil {
					return nil, err
				}
				out = append(out, row)
			}
		}
		return out, nil
	case op == setOpExcept && all:
		counts := make(map[string]int)
		for _, row := range right {
			k := distinctRowKey(row)
			if _, ok := counts[k]; !ok {
				if err := entry(row); err != nil {
					return nil, err
				}
			}
			counts[k]++
		}
		out := make([][]Value, 0)
		for _, row := range left {
			k := distinctRowKey(row)
			if counts[k] > 0 {
				counts[k]--
			} else {
				out = append(out, row)
			}
		}
		return out, nil
	default: // EXCEPT, distinct
		rightSet := make(map[string]bool)
		for _, row := range right {
			k := distinctRowKey(row)
			if !rightSet[k] {
				if err := entry(row); err != nil {
					return nil, err
				}
				rightSet[k] = true
			}
		}
		emitted := make(map[string]bool)
		out := make([][]Value, 0)
		for _, row := range left {
			k := distinctRowKey(row)
			if !rightSet[k] && !emitted[k] {
				emitted[k] = true
				if err := entry(row); err != nil {
					return nil, err
				}
				out = append(out, row)
			}
		}
		return out, nil
	}
}

// resolveSetopOrderKey resolves a trailing ORDER BY key for a set operation against the OUTPUT
// column names (the left operand's). A qualified key is 42P01 (no relation scope after a set
// operation); an unknown name is 42703. Returns the output column index.
func resolveSetopOrderKey(key *orderKey, names []string) (int, error) {
	// A set-operation ORDER BY accepts only an output column name or ordinal — a general expression key
	// (after the inputs are unified) is 0A000, matching PostgreSQL's "invalid UNION/INTERSECT/EXCEPT
	// ORDER BY clause" (grammar.md §10).
	if key.Expr != nil {
		return 0, newError(FeatureNotSupported, "invalid UNION/INTERSECT/EXCEPT ORDER BY clause")
	}
	// An output-column ordinal (`... ORDER BY 1`) resolves by position into the output columns; out
	// of [1, ncols] is 42P10 (grammar.md §10). It precedes the name path (an ordinal has no column).
	if key.Ordinal != nil {
		ord := *key.Ordinal
		if ord < 1 || ord > int64(len(names)) {
			return 0, newError(InvalidColumnReference,
				fmt.Sprintf("ORDER BY position %d is not in select list", ord))
		}
		return int(ord - 1), nil
	}
	if key.Qualifier != "" {
		return 0, newError(UndefinedTable, "missing FROM-clause entry for table "+key.Qualifier).
			withPosition(int32(key.Pos))
	}
	for i, n := range names {
		if n == key.Column {
			return i, nil
		}
	}
	return 0, newError(UndefinedColumn, "column "+key.Column+" does not exist").withPosition(int32(key.Pos))
}
