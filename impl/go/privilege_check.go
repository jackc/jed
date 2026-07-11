package jed

import (
	"fmt"
	"strings"
)

// Per-statement admission checks: privilege enforcement, budgets, and lane gating (session.md,
// cost.md). This file classifies a statement (stmtIsWrite, the sequence-mutator detection walk), then
// gates it before execution: privilege collection + enforcement (collect*Privs/checkPrivileges → 42501),
// lifetime-cost admission (checkLifetimeAdmission → 54P02), temp-buffer budget (checkTempBudget →
// 54P03), and the streaming read-lane gate + block poisoning (gateReadLanes/poisonOnLaneErr). Named
// privilege_check.go — the host-facing PrivilegeSet type already owns privileges.go.

// stmtIsWrite reports whether a statement mutates the database (so autocommit must capture +
// durably persist it, and a READ ONLY transaction must reject it — transactions.md §4.1/§4.3).
// Reads (SELECT, set operations) and transaction control run with no data mutation.
func stmtIsWrite(stmt statement) bool {
	// EXPLAIN is a read: plain EXPLAIN plans without executing (even of a DML inner — it never
	// mutates). Only EXPLAIN ANALYZE runs the inner statement, so it is a write iff the inner is
	// (spec/design/explain.md §3).
	if stmt.Explain != nil {
		return stmt.Explain.Analyze && stmtIsWrite(*stmt.Explain.Inner)
	}
	if stmt.CreateTable != nil || stmt.DropTable != nil ||
		stmt.CreateIndex != nil || stmt.DropIndex != nil ||
		stmt.CreateType != nil || stmt.DropType != nil ||
		stmt.CreateSequence != nil || stmt.AlterSequence != nil || stmt.DropSequence != nil ||
		stmt.Insert != nil || stmt.Update != nil || stmt.Delete != nil {
		return true
	}
	// A WITH statement with any data-modifying part is a write (it stages INSERT/UPDATE/DELETE effects
	// — writable-cte.md): it must take the write gate, accumulate into working, and commit.
	if stmt.With != nil && withHasDml(stmt.With) {
		return true
	}
	// A read-shaped statement that calls a sequence-mutating function (nextval/setval) IS a write
	// (spec/design/sequences.md §4): it must take the write gate, stage the advance, and commit
	// (autocommit) — and is 25006 in a READ ONLY transaction, exactly like any other write.
	return stmtCallsSeqMutator(stmt)
}

// stmtCallsSeqMutator reports whether stmt's expression trees contain a sequence-MUTATING function
// call (nextval; in S2, setval) anywhere — which makes an otherwise read-shaped statement a write
// (sequences.md §4). Only the read-shaped statements need checking: INSERT/UPDATE/DELETE/DDL are
// already writes (stmtIsWrite short-circuits before this), and an INSERT VALUES slot is
// literal-only (no function call). currval is a pure read and is NOT counted. The Expr walk is
// exhaustive, so no expression position is missed.
func stmtCallsSeqMutator(stmt statement) bool {
	switch {
	case stmt.Select != nil:
		return selectCallsSeqMutator(stmt.Select)
	case stmt.SetOp != nil:
		return setopCallsSeqMutator(stmt.SetOp)
	case stmt.With != nil:
		for i := range stmt.With.Ctes {
			if cteBodyCallsSeqMutator(&stmt.With.Ctes[i].Body) {
				return true
			}
		}
		return cteBodyCallsSeqMutator(&stmt.With.Body)
	default:
		return false
	}
}

// cteBodyCallsSeqMutator reports whether a cte_body calls a sequence-mutating function. A query body
// delegates to the query walk; a data-modifying body already makes the WITH a write (via withHasDml),
// so this is not reached for it via stmtCallsSeqMutator — it is treated as a write regardless
// (writable-cte.md).
func cteBodyCallsSeqMutator(body *cteBody) bool {
	if body.Query != nil {
		return queryCallsSeqMutator(body.Query)
	}
	return true
}

func queryCallsSeqMutator(qe *queryExpr) bool {
	if qe.Select != nil {
		return selectCallsSeqMutator(qe.Select)
	}
	if qe.SetOp != nil {
		return setopCallsSeqMutator(qe.SetOp)
	}
	if qe.With != nil {
		// A nested WITH's CTE bodies and main body may call a sequence mutator (cte.md §7).
		for i := range qe.With.Ctes {
			if cteBodyCallsSeqMutator(&qe.With.Ctes[i].Body) {
				return true
			}
		}
		return queryCallsSeqMutator(qe.With.Body)
	}
	return false
}

func setopCallsSeqMutator(so *setOp) bool {
	return queryCallsSeqMutator(&so.Lhs) || queryCallsSeqMutator(&so.Rhs)
}

func selectCallsSeqMutator(s *selectStmt) bool {
	for i := range s.Items.Items {
		if exprCallsSeqMutator(&s.Items.Items[i].Expr) {
			return true
		}
	}
	if s.From != nil && tableRefCallsSeqMutator(s.From) {
		return true
	}
	for i := range s.Joins {
		if tableRefCallsSeqMutator(&s.Joins[i].Table) {
			return true
		}
		if s.Joins[i].On != nil && exprCallsSeqMutator(s.Joins[i].On) {
			return true
		}
	}
	if s.Filter != nil && exprCallsSeqMutator(s.Filter) {
		return true
	}
	for i := range s.GroupBy {
		found := false
		s.GroupBy[i].forEachExpr(func(e *exprNode) {
			if exprCallsSeqMutator(e) {
				found = true
			}
		})
		if found {
			return true
		}
	}
	if s.Having != nil && exprCallsSeqMutator(s.Having) {
		return true
	}
	return false
}

func tableRefCallsSeqMutator(t *tableRef) bool {
	for _, a := range t.Args {
		if exprCallsSeqMutator(a) {
			return true
		}
	}
	if t.Subquery != nil && queryCallsSeqMutator(t.Subquery) {
		return true
	}
	for _, row := range t.Values {
		for _, e := range row {
			if exprCallsSeqMutator(e) {
				return true
			}
		}
	}
	return false
}

// exprCallsSeqMutator is exhaustive over Expr: true iff the tree contains a nextval call.
func exprCallsSeqMutator(e *exprNode) bool {
	switch e.Kind {
	case exprFuncCall:
		if strings.EqualFold(e.FuncCall.Name, "nextval") || strings.EqualFold(e.FuncCall.Name, "setval") {
			return true
		}
		for _, a := range e.FuncCall.Args {
			if exprCallsSeqMutator(a) {
				return true
			}
		}
		return false
	case exprColumn, exprQualifiedColumn, exprLiteral, exprTypedLiteral, exprParam:
		return false
	case exprRow, exprArray:
		for i := range e.RowItems {
			if exprCallsSeqMutator(&e.RowItems[i]) {
				return true
			}
		}
		return false
	case exprFieldAccess, exprFieldStar:
		return exprCallsSeqMutator(e.Base)
	case exprQualifiedStar:
		return false // `t.*` is a leaf relation reference — no sub-expression

	case exprSubscript:
		if exprCallsSeqMutator(e.Base) {
			return true
		}
		for i := range e.Subscripts {
			sub := &e.Subscripts[i]
			if sub.Index != nil && exprCallsSeqMutator(sub.Index) {
				return true
			}
			if sub.Lower != nil && exprCallsSeqMutator(sub.Lower) {
				return true
			}
			if sub.Upper != nil && exprCallsSeqMutator(sub.Upper) {
				return true
			}
		}
		return false
	case exprCast:
		return exprCallsSeqMutator(&e.Cast.Inner)
	case exprExtract:
		return exprCallsSeqMutator(&e.Extract.Source)
	case exprCollate:
		return exprCallsSeqMutator(&e.Collate.Inner)
	case exprUnary:
		return exprCallsSeqMutator(&e.Unary.Operand)
	case exprIsNull:
		return exprCallsSeqMutator(&e.IsNullOf.Operand)
	case exprIsJson:
		return exprCallsSeqMutator(&e.IsJsonOf.Operand)
	case exprJsonCtor:
		return exprCallsSeqMutator(&e.JsonCtorOf.Operand)
	case exprJsonExists:
		return exprCallsSeqMutator(&e.JsonExists.Ctx) || exprCallsSeqMutator(&e.JsonExists.Path)
	case exprJsonValue:
		return exprCallsSeqMutator(&e.JsonValue.Ctx) || exprCallsSeqMutator(&e.JsonValue.Path)
	case exprJsonQuery:
		return exprCallsSeqMutator(&e.JsonQuery.Ctx) || exprCallsSeqMutator(&e.JsonQuery.Path)
	case exprBinary:
		return exprCallsSeqMutator(&e.Binary.Lhs) || exprCallsSeqMutator(&e.Binary.Rhs)
	case exprIsDistinct:
		return exprCallsSeqMutator(&e.IsDistinct.Lhs) || exprCallsSeqMutator(&e.IsDistinct.Rhs)
	case exprLike:
		return exprCallsSeqMutator(&e.Like.Lhs) || exprCallsSeqMutator(&e.Like.Rhs)
	case exprRegex:
		return exprCallsSeqMutator(&e.Regex.Lhs) || exprCallsSeqMutator(&e.Regex.Rhs)
	case exprIn:
		if exprCallsSeqMutator(&e.In.Lhs) {
			return true
		}
		for i := range e.In.List {
			if exprCallsSeqMutator(&e.In.List[i]) {
				return true
			}
		}
		return false
	case exprBetween:
		return exprCallsSeqMutator(&e.Between.Lhs) ||
			exprCallsSeqMutator(&e.Between.Lo) ||
			exprCallsSeqMutator(&e.Between.Hi)
	case exprCase:
		if e.Case.Operand != nil && exprCallsSeqMutator(e.Case.Operand) {
			return true
		}
		for i := range e.Case.Whens {
			if exprCallsSeqMutator(&e.Case.Whens[i].Cond) || exprCallsSeqMutator(&e.Case.Whens[i].Result) {
				return true
			}
		}
		if e.Case.Els != nil && exprCallsSeqMutator(e.Case.Els) {
			return true
		}
		return false
	case exprCoalesce:
		for i := range e.Coalesce {
			if exprCallsSeqMutator(&e.Coalesce[i]) {
				return true
			}
		}
		return false
	case exprGreatestLeast:
		for i := range e.GreatestLeast {
			if exprCallsSeqMutator(&e.GreatestLeast[i]) {
				return true
			}
		}
		return false
	case exprScalarSubquery, exprExists:
		return queryCallsSeqMutator(e.Subquery)
	case exprInSubquery:
		return exprCallsSeqMutator(&e.InSubquery.Lhs) || queryCallsSeqMutator(&e.InSubquery.Query)
	case exprQuantifiedSubquery:
		return exprCallsSeqMutator(&e.QuantifiedSubquery.Lhs) || queryCallsSeqMutator(&e.QuantifiedSubquery.Query)
	case exprQuantified:
		return exprCallsSeqMutator(&e.Quantified.Lhs) || exprCallsSeqMutator(&e.Quantified.Array)
	default:
		return false
	}
}

// privTableReq is one (table, required privilege) pair collected from a statement.
type privTableReq struct {
	name string
	priv Privilege
}

// privReq is the privilege requirements collected from one statement (spec/design/session.md §5.3):
// the per-table privileges, the named functions (each needs EXECUTE), and whether the statement is
// DDL (gated by allowDDL). Collected by an exhaustive AST walk (mirroring exprCallsSeqMutator).
type privReq struct {
	tables    []privTableReq
	functions []string
	isDDL     bool
	// isTempDDL is whether the DDL targets a SESSION-LOCAL temporary table (CREATE TEMP TABLE) — gated
	// by allowTempDDL instead of allowDDL (spec/design/temp-tables.md §5). Set only for a CREATE TEMP;
	// a DROP is classified by resolving the name.
	isTempDDL bool
}

func (r *privReq) needTable(name string, p Privilege) {
	r.tables = append(r.tables, privTableReq{name: name, priv: p})
}
func (r *privReq) needFunction(name string) { r.functions = append(r.functions, name) }

// checkPrivileges enforces the session's authorization envelope for stmt (spec/design/session.md
// §5.3). A fully-permissive session (the default) needs no check. Otherwise DDL is gated by allowDDL,
// and DML requires a per-table privilege for each table it reads (SELECT) or writes
// (INSERT/UPDATE/DELETE) and EXECUTE for each named function it calls. Enforcement is at name
// resolution: a table privilege is required only for a name that resolves to an existing catalog
// table (a missing table stays 42P01; a CTE / derived-table label is statement-local, not a catalog
// object). Missing privilege → 42501.
// checkLifetimeAdmission rejects a statement at admission when the session's lifetime cost budget is
// already spent (spec/design/session.md §5.4): if a budget is set and the session's cumulative cost
// has reached it, no further statement may run (it "cannot accrue") — 54P02. A no-op when the budget
// is unlimited (the default), so the common path pays one comparison.
func (db *engine) checkLifetimeAdmission() error {
	limit := db.session.lifetimeMaxCost
	total := *db.session.lifetimeTotal
	if limit > 0 && total >= limit {
		return newError(SessionCostLimitExceeded, fmt.Sprintf(
			"session exceeded the lifetime cost limit of %d (accrued %d)", limit, total,
		))
	}
	return nil
}

// checkTempBudget enforces the per-session temp-table storage budget (tempBuffers, spec/design/
// temp-tables.md §7) — the §13 gate on RETAINED temp bytes. Checked after each temp-writing statement:
// if the session's temp footprint (byte-identical on-disk record bytes, summed over every temp table +
// index) EXCEEDS the budget, abort 54P03. The over-budget write is in tempWorking, so the abort
// discards it (autocommit) or fails the block (rolled back at ROLLBACK) — nothing commits. tempBuffers
// 0 ⇒ unlimited; a transaction that did not touch temp cannot have grown it, so the check self-gates on
// tempDirty and is a no-op for ordinary (persistent) statements. The WITHIN-statement bound is maxCost.
func (db *engine) checkTempBudget() error {
	limit := db.session.tempBuffers
	if limit == 0 {
		return nil
	}
	if db.session.tx == nil || !db.session.tx.tempDirty {
		return nil
	}
	// Page-based footprint of the session-local temp domain (temp-tables.md §7, Design decision 3): the
	// committed MemoryBlockStore high-water × page size — the honest resident-RAM measure now that temp
	// rides a pager (a record-byte walk would skip demoted OnDisk leaves and undercount a multi-leaf temp
	// table, defeating the §13 bound). Deterministic and cross-core-identical: pageCount is a pure
	// function of operations via the B+tree + within-session compaction. It reflects the state one commit
	// behind (the pending write commits at statement end), so a domain already over budget aborts the NEXT
	// temp write and rolls it back — the "already over budget ⇒ further writes abort" contract (§7).
	var used uint64
	if db.tempStorage != nil {
		used = uint64(db.tempStorage.pageCount) * uint64(db.pageSize)
	}
	if used > uint64(limit) {
		return newError(TempStorageLimitExceeded, fmt.Sprintf(
			"temporary table storage exceeded the limit of %d bytes", limit,
		))
	}
	return nil
}

func (db *engine) checkPrivileges(stmt statement) error {
	// Fast path: a session that allows ALL DDL (persistent + temp) and grants every privilege pays
	// nothing. Both gates must be on, since temp DDL now has its own gate (§5).
	if db.session.allowDDL && db.session.allowTempDDL && db.session.privileges.IsPermissive() {
		return nil
	}
	var req privReq
	collectStmtPrivs(stmt, &req)
	if req.isDDL {
		// DDL is gated by the kind of relation it targets (temp-tables.md §5): a session-local temp
		// table by allowTempDDL, everything else (persistent) by allowDDL. A CREATE TABLE is classified
		// statically; the rest by resolving the name — a DROP TABLE / CREATE INDEX by its target table,
		// a DROP INDEX by the index (preclude-overlaps keeps a name in one scope).
		var allowed bool
		switch {
		case req.isTempDDL ||
			(stmt.DropTable != nil && db.anyTempTable(stmt.DropTable.Names)) ||
			(stmt.CreateIndex != nil && db.isTempTable(stmt.CreateIndex.Table)) ||
			(stmt.DropIndex != nil && db.isTempIndex(stmt.DropIndex.Name)) ||
			(stmt.DropSequence != nil && db.anyTempSequence(stmt.DropSequence.Names)) ||
			(stmt.AlterSequence != nil && db.isTempSequence(stmt.AlterSequence.Name)):
			allowed = db.session.allowTempDDL
		default:
			allowed = db.session.allowDDL
		}
		if !allowed {
			return newError(InsufficientPrivilege, "permission denied: DDL is not permitted in this session")
		}
	}
	snap := db.readSnap()
	for _, t := range req.tables {
		key := strings.ToLower(t.name)
		// Only a name that resolves to an existing catalog table is privilege-checked; a missing one is
		// left to raise 42P01 in execution (existence before authorization). A built-in catalog relation
		// (jed_tables / jed_columns) is gated exactly like a user table — per-table SELECT on its own
		// name under the session envelope, no special case (introspection.md §5) — so an explicit-grant
		// session sees the schema only if the host granted it.
		exists := isCatalogRelName(key)
		if !exists {
			_, exists = snap.table(key)
		}
		if exists && !db.session.privileges.AllowsTable(key, t.priv) {
			return newError(InsufficientPrivilege, "permission denied for table "+key)
		}
	}
	for _, fn := range req.functions {
		key := strings.ToLower(fn)
		if !db.session.privileges.AllowsFunction(key) {
			return newError(InsufficientPrivilege, "permission denied for function "+key)
		}
	}
	return nil
}

// gateReadLanes runs the admission gates that the lazy read lanes (tryScanQuery / tryDeferredQuery)
// would otherwise skip. Those gates live on the materialized dispatchStmt / ExecuteStmtParams path, but
// a SELECT served by a streaming/deferred cursor never reaches it — so before Exec/Query became the one
// total seam, a read through the ergonomic Query path bypassed authorization entirely (a §13 hole).
// Enforcing them here makes Query a total AND safe seam: a read inside a failed block is 25P02, a
// lifetime-exhausted session is 54P02, and a restricted read is 42501 — whichever lane ends up serving
// it. The caller applies this only to reads (transaction control must still work in a failed block, and
// a write keeps its existing gating inside dispatch); the three checks are pure, so a read that falls
// through to the materialized path re-running them is harmless (identical result).
func (db *engine) gateReadLanes(stmt statement) error {
	if db.session.tx != nil && db.session.tx.failed {
		return newError(InFailedSqlTransaction,
			"current transaction is aborted, commands ignored until end of transaction block")
	}
	if err := db.checkLifetimeAdmission(); err != nil {
		return err
	}
	return db.checkPrivileges(stmt)
}

// failOpenBlock puts an open, failable transaction block into the aborted state (tx.failed). A no-op
// outside a block, and idempotent. This is the block-abort that a lazy read lane bypasses: the
// materialized ExecuteStmtParams poisons in its block branch, but a SELECT served by a streaming /
// deferred cursor never reaches it (transactions.md §6). PostgreSQL aborts a block on ANY statement
// error, so a failing read has to poison here — otherwise the next statement wrongly succeeds instead
// of 25P02. Only reads reach these paths (transaction control and writes go to dispatch, which
// self-poisons with the right nuance — a nested BEGIN's 25001 must NOT abort).
func (db *engine) failOpenBlock() {
	if db.session.tx != nil {
		db.session.tx.failed = true
	}
}

// poisonOnLaneErr aborts an open block when a lazy read lane returns an error at open time (a missing
// table, a denied read, a plan-time trap) — the counterpart to gateReadLanes: gateReadLanes enforces
// the admission gates the lanes skip, poisonOnLaneErr the block-abort they skip. Wraps a lane error
// return; the returned err is unchanged.
func (db *engine) poisonOnLaneErr(err error) error {
	if err != nil {
		db.failOpenBlock()
	}
	return err
}

// attachBlockPoison hooks a lazy-lane cursor so a DRAIN-time read error inside an open block aborts it
// too. A streaming (S3) / deferred (S7) cursor's error surfaces during the caller's Next(), after
// queryStmt has returned, so the open-time poisonOnLaneErr can't see it — the cursor's onErr hook does
// (executor's blocking buffered read already surfaces its error at open, poisoned above). A no-op when
// no block is open; the hook re-checks the block at error time (a read may outlive the block it began
// in — poisoning an already-ended block is harmless).
func (db *engine) attachBlockPoison(rows *Rows) *Rows {
	if db.session.tx != nil {
		rows.attachErrHook(func(error) { db.failOpenBlock() })
	}
	return rows
}

// collectStmtPrivs collects the privilege requirements of stmt (spec/design/session.md §5.3).
// Transaction control carries none (handled before dispatch); DDL just sets isDDL.
func collectStmtPrivs(stmt statement, req *privReq) {
	locals := map[string]bool{}
	switch {
	case stmt.CreateTable != nil:
		req.isDDL = true
		// A temp table's DDL is gated by the temp-scoped split of allowDDL (temp-tables.md §5):
		// allowTempDDL for a session-local temp table.
		req.isTempDDL = stmt.CreateTable.Temp
	case stmt.DropTable != nil, stmt.CreateIndex != nil, stmt.DropIndex != nil,
		stmt.CreateType != nil, stmt.DropType != nil, stmt.CreateSequence != nil, stmt.DropSequence != nil,
		stmt.AlterSequence != nil:
		req.isDDL = true
	case stmt.Insert != nil:
		collectInsertPrivs(stmt.Insert, req, locals)
	case stmt.Select != nil:
		collectSelectPrivs(stmt.Select, req, locals)
	case stmt.SetOp != nil:
		collectSetopPrivs(stmt.SetOp, req, locals)
	case stmt.With != nil:
		collectWithPrivs(stmt.With, req, locals)
	case stmt.Update != nil:
		collectUpdatePrivs(stmt.Update, req, locals)
	case stmt.Delete != nil:
		collectDeletePrivs(stmt.Delete, req, locals)
	case stmt.Explain != nil:
		// EXPLAIN requires the inner statement's privileges (EXPLAIN INSERT needs INSERT, matching
		// PG). Plain EXPLAIN never executes, but authorization is checked on the inner regardless.
		collectStmtPrivs(*stmt.Explain.Inner, req)
	}
}

func collectInsertPrivs(ins *insert, req *privReq, locals map[string]bool) {
	// The write target needs INSERT. A bare INSERT … VALUES reads nothing (the slots are literals /
	// params), so it needs only INSERT; an INSERT … SELECT source needs SELECT on its tables.
	req.needTable(ins.Table, PrivInsert)
	if ins.Select != nil {
		collectSelectPrivs(ins.Select, req, locals)
	}
	if ins.OnConflict != nil && ins.OnConflict.DoUpdate {
		for i := range ins.OnConflict.Assignments {
			collectExprPrivs(&ins.OnConflict.Assignments[i].Value, req, locals)
		}
		if ins.OnConflict.Filter != nil {
			collectExprPrivs(ins.OnConflict.Filter, req, locals)
		}
	}
	collectItemsPrivs(ins.Returning, req, locals)
}

func collectUpdatePrivs(upd *update, req *privReq, locals map[string]bool) {
	req.needTable(upd.Table, PrivUpdate)
	// SELECT on the target if it reads any column — a WHERE, a RETURNING, or a column/subquery-
	// referencing assignment RHS (a constant-only SET a = 1 with no WHERE/RETURNING reads nothing).
	reads := upd.Filter != nil || upd.Returning != nil
	for i := range upd.Assignments {
		if exprReadsColumns(&upd.Assignments[i].Value) {
			reads = true
		}
	}
	if reads {
		req.needTable(upd.Table, PrivSelect)
	}
	for i := range upd.Assignments {
		collectExprPrivs(&upd.Assignments[i].Value, req, locals)
	}
	if upd.Filter != nil {
		collectExprPrivs(upd.Filter, req, locals)
	}
	collectItemsPrivs(upd.Returning, req, locals)
}

func collectDeletePrivs(del *deleteStmt, req *privReq, locals map[string]bool) {
	req.needTable(del.Table, PrivDelete)
	// DELETE reads the target's columns through a WHERE or a RETURNING.
	if del.Filter != nil || del.Returning != nil {
		req.needTable(del.Table, PrivSelect)
	}
	if del.Filter != nil {
		collectExprPrivs(del.Filter, req, locals)
	}
	collectItemsPrivs(del.Returning, req, locals)
}

func collectQueryPrivs(qe *queryExpr, req *privReq, locals map[string]bool) {
	if qe.Select != nil {
		collectSelectPrivs(qe.Select, req, locals)
	} else if qe.SetOp != nil {
		collectSetopPrivs(qe.SetOp, req, locals)
	} else if qe.With != nil {
		// A nested WITH establishes its own CTE scope (spec/design/cte.md §7): the enclosing locals
		// are NOT inherited (an enclosing CTE name resolves to a base table inside, so it is
		// privilege-checked), and the nested CTE names shadow base tables only within this node.
		scope := map[string]bool{}
		for i := range qe.With.Ctes {
			collectCteBodyPrivs(&qe.With.Ctes[i].Body, req, scope)
			scope[strings.ToLower(qe.With.Ctes[i].Name)] = true
		}
		collectQueryPrivs(qe.With.Body, req, scope)
	}
}

func collectSetopPrivs(so *setOp, req *privReq, locals map[string]bool) {
	collectQueryPrivs(&so.Lhs, req, locals)
	collectQueryPrivs(&so.Rhs, req, locals)
}

func collectWithPrivs(wq *withQuery, req *privReq, locals map[string]bool) {
	// A CTE name shadows a base table inside the WITH (a FROM <cte> is not a catalog object), so it is
	// added to the local scope and never privilege-checked. Forward-only visibility: each CTE body
	// sees the CTE names declared before it. A data-modifying body / primary needs the write privilege
	// on its target table (writable-cte.md).
	scope := map[string]bool{}
	for k := range locals {
		scope[k] = true
	}
	for i := range wq.Ctes {
		collectCteBodyPrivs(&wq.Ctes[i].Body, req, scope)
		scope[strings.ToLower(wq.Ctes[i].Name)] = true
	}
	collectCteBodyPrivs(&wq.Body, req, scope)
}

// collectCteBodyPrivs collects the privilege requirements of a cte_body — a query, or a
// data-modifying statement (spec/design/writable-cte.md) which needs the write privilege on its
// target.
func collectCteBodyPrivs(body *cteBody, req *privReq, locals map[string]bool) {
	switch {
	case body.Query != nil:
		collectQueryPrivs(body.Query, req, locals)
	case body.Insert != nil:
		collectInsertPrivs(body.Insert, req, locals)
	case body.Update != nil:
		collectUpdatePrivs(body.Update, req, locals)
	default:
		collectDeletePrivs(body.Delete, req, locals)
	}
}

func collectSelectPrivs(s *selectStmt, req *privReq, locals map[string]bool) {
	if s.From != nil {
		collectTableRefPrivs(s.From, req, locals)
	}
	for i := range s.Joins {
		collectTableRefPrivs(&s.Joins[i].Table, req, locals)
		if s.Joins[i].On != nil {
			collectExprPrivs(s.Joins[i].On, req, locals)
		}
	}
	for i := range s.Items.Items {
		collectExprPrivs(&s.Items.Items[i].Expr, req, locals)
	}
	if s.Filter != nil {
		collectExprPrivs(s.Filter, req, locals)
	}
	for i := range s.GroupBy {
		s.GroupBy[i].forEachExpr(func(e *exprNode) {
			collectExprPrivs(e, req, locals)
		})
	}
	if s.Having != nil {
		collectExprPrivs(s.Having, req, locals)
	}
}

func collectTableRefPrivs(t *tableRef, req *privReq, locals map[string]bool) {
	switch {
	case t.IsFunc:
		// A set-returning function used as a row source — EXECUTE on the function; its args are exprs.
		req.needFunction(t.Name)
		for _, a := range t.Args {
			collectExprPrivs(a, req, locals)
		}
	case t.Subquery != nil:
		collectQueryPrivs(t.Subquery, req, locals)
	case t.Values != nil:
		for _, row := range t.Values {
			for _, e := range row {
				collectExprPrivs(e, req, locals)
			}
		}
	default:
		// A base-table reference (not a CTE / derived-table label) — needs SELECT.
		if !locals[strings.ToLower(t.Name)] {
			req.needTable(t.Name, PrivSelect)
		}
	}
}

func collectItemsPrivs(items *selectItems, req *privReq, locals map[string]bool) {
	if items == nil {
		return
	}
	for i := range items.Items {
		collectExprPrivs(&items.Items[i].Expr, req, locals)
	}
}

// collectExprPrivs is exhaustive over Expr (mirroring exprCallsSeqMutator): collect every named
// function call (EXECUTE) and walk every subquery (its tables need SELECT).
func collectExprPrivs(e *exprNode, req *privReq, locals map[string]bool) {
	switch e.Kind {
	case exprFuncCall:
		req.needFunction(e.FuncCall.Name)
		for _, a := range e.FuncCall.Args {
			collectExprPrivs(a, req, locals)
		}
	case exprColumn, exprQualifiedColumn, exprLiteral, exprTypedLiteral, exprParam:
		// leaf — nothing to collect
	case exprRow, exprArray:
		for i := range e.RowItems {
			collectExprPrivs(&e.RowItems[i], req, locals)
		}
	case exprFieldAccess, exprFieldStar:
		collectExprPrivs(e.Base, req, locals)
	case exprQualifiedStar:
		// `t.*` names a relation already in FROM — its SELECT privilege is required by the FROM
		// clause itself, so the star adds no new function/table privilege here.
	case exprSubscript:
		collectExprPrivs(e.Base, req, locals)
		for i := range e.Subscripts {
			sub := &e.Subscripts[i]
			if sub.Index != nil {
				collectExprPrivs(sub.Index, req, locals)
			}
			if sub.Lower != nil {
				collectExprPrivs(sub.Lower, req, locals)
			}
			if sub.Upper != nil {
				collectExprPrivs(sub.Upper, req, locals)
			}
		}
	case exprCast:
		collectExprPrivs(&e.Cast.Inner, req, locals)
	case exprExtract:
		collectExprPrivs(&e.Extract.Source, req, locals)
	case exprCollate:
		collectExprPrivs(&e.Collate.Inner, req, locals)
	case exprUnary:
		collectExprPrivs(&e.Unary.Operand, req, locals)
	case exprIsNull:
		collectExprPrivs(&e.IsNullOf.Operand, req, locals)
	case exprIsJson:
		collectExprPrivs(&e.IsJsonOf.Operand, req, locals)
	case exprJsonCtor:
		collectExprPrivs(&e.JsonCtorOf.Operand, req, locals)
	case exprJsonExists:
		collectExprPrivs(&e.JsonExists.Ctx, req, locals)
		collectExprPrivs(&e.JsonExists.Path, req, locals)
	case exprJsonValue:
		collectExprPrivs(&e.JsonValue.Ctx, req, locals)
		collectExprPrivs(&e.JsonValue.Path, req, locals)
	case exprJsonQuery:
		collectExprPrivs(&e.JsonQuery.Ctx, req, locals)
		collectExprPrivs(&e.JsonQuery.Path, req, locals)
	case exprBinary:
		collectExprPrivs(&e.Binary.Lhs, req, locals)
		collectExprPrivs(&e.Binary.Rhs, req, locals)
	case exprIsDistinct:
		collectExprPrivs(&e.IsDistinct.Lhs, req, locals)
		collectExprPrivs(&e.IsDistinct.Rhs, req, locals)
	case exprLike:
		collectExprPrivs(&e.Like.Lhs, req, locals)
		collectExprPrivs(&e.Like.Rhs, req, locals)
	case exprRegex:
		collectExprPrivs(&e.Regex.Lhs, req, locals)
		collectExprPrivs(&e.Regex.Rhs, req, locals)
	case exprIn:
		collectExprPrivs(&e.In.Lhs, req, locals)
		for i := range e.In.List {
			collectExprPrivs(&e.In.List[i], req, locals)
		}
	case exprBetween:
		collectExprPrivs(&e.Between.Lhs, req, locals)
		collectExprPrivs(&e.Between.Lo, req, locals)
		collectExprPrivs(&e.Between.Hi, req, locals)
	case exprCase:
		if e.Case.Operand != nil {
			collectExprPrivs(e.Case.Operand, req, locals)
		}
		for i := range e.Case.Whens {
			collectExprPrivs(&e.Case.Whens[i].Cond, req, locals)
			collectExprPrivs(&e.Case.Whens[i].Result, req, locals)
		}
		if e.Case.Els != nil {
			collectExprPrivs(e.Case.Els, req, locals)
		}
	case exprCoalesce:
		for i := range e.Coalesce {
			collectExprPrivs(&e.Coalesce[i], req, locals)
		}
	case exprGreatestLeast:
		for i := range e.GreatestLeast {
			collectExprPrivs(&e.GreatestLeast[i], req, locals)
		}
	case exprScalarSubquery, exprExists:
		collectQueryPrivs(e.Subquery, req, locals)
	case exprInSubquery:
		collectExprPrivs(&e.InSubquery.Lhs, req, locals)
		collectQueryPrivs(&e.InSubquery.Query, req, locals)
	case exprQuantifiedSubquery:
		collectExprPrivs(&e.QuantifiedSubquery.Lhs, req, locals)
		collectQueryPrivs(&e.QuantifiedSubquery.Query, req, locals)
	case exprQuantified:
		collectExprPrivs(&e.Quantified.Lhs, req, locals)
		collectExprPrivs(&e.Quantified.Array, req, locals)
	}
}

// exprReadsColumns reports whether e reads a stored column or a subquery's rows — the trigger for an
// UPDATE's SELECT requirement on its target (spec/design/session.md §5.3). A column reference or any
// subquery counts; a pure constant / parameter expression does not. Exhaustive over Expr.
func exprReadsColumns(e *exprNode) bool {
	switch e.Kind {
	case exprColumn, exprQualifiedColumn:
		return true
	case exprScalarSubquery, exprExists, exprInSubquery, exprQuantifiedSubquery:
		return true
	case exprLiteral, exprTypedLiteral, exprParam:
		return false
	case exprRow, exprArray:
		for i := range e.RowItems {
			if exprReadsColumns(&e.RowItems[i]) {
				return true
			}
		}
		return false
	case exprFieldAccess, exprFieldStar:
		return exprReadsColumns(e.Base)
	case exprQualifiedStar:
		return true // `t.*` reads the relation's columns (e.g. `RETURNING t.*`)

	case exprSubscript:
		if exprReadsColumns(e.Base) {
			return true
		}
		for i := range e.Subscripts {
			sub := &e.Subscripts[i]
			if sub.Index != nil && exprReadsColumns(sub.Index) {
				return true
			}
			if sub.Lower != nil && exprReadsColumns(sub.Lower) {
				return true
			}
			if sub.Upper != nil && exprReadsColumns(sub.Upper) {
				return true
			}
		}
		return false
	case exprCast:
		return exprReadsColumns(&e.Cast.Inner)
	case exprExtract:
		return exprReadsColumns(&e.Extract.Source)
	case exprCollate:
		return exprReadsColumns(&e.Collate.Inner)
	case exprUnary:
		return exprReadsColumns(&e.Unary.Operand)
	case exprIsNull:
		return exprReadsColumns(&e.IsNullOf.Operand)
	case exprIsJson:
		return exprReadsColumns(&e.IsJsonOf.Operand)
	case exprJsonCtor:
		return exprReadsColumns(&e.JsonCtorOf.Operand)
	case exprJsonExists:
		return exprReadsColumns(&e.JsonExists.Ctx) || exprReadsColumns(&e.JsonExists.Path)
	case exprJsonValue:
		return exprReadsColumns(&e.JsonValue.Ctx) || exprReadsColumns(&e.JsonValue.Path)
	case exprJsonQuery:
		return exprReadsColumns(&e.JsonQuery.Ctx) || exprReadsColumns(&e.JsonQuery.Path)
	case exprFuncCall:
		for _, a := range e.FuncCall.Args {
			if exprReadsColumns(a) {
				return true
			}
		}
		return false
	case exprBinary:
		return exprReadsColumns(&e.Binary.Lhs) || exprReadsColumns(&e.Binary.Rhs)
	case exprIsDistinct:
		return exprReadsColumns(&e.IsDistinct.Lhs) || exprReadsColumns(&e.IsDistinct.Rhs)
	case exprLike:
		return exprReadsColumns(&e.Like.Lhs) || exprReadsColumns(&e.Like.Rhs)
	case exprRegex:
		return exprReadsColumns(&e.Regex.Lhs) || exprReadsColumns(&e.Regex.Rhs)
	case exprIn:
		if exprReadsColumns(&e.In.Lhs) {
			return true
		}
		for i := range e.In.List {
			if exprReadsColumns(&e.In.List[i]) {
				return true
			}
		}
		return false
	case exprBetween:
		return exprReadsColumns(&e.Between.Lhs) || exprReadsColumns(&e.Between.Lo) || exprReadsColumns(&e.Between.Hi)
	case exprCase:
		if e.Case.Operand != nil && exprReadsColumns(e.Case.Operand) {
			return true
		}
		for i := range e.Case.Whens {
			if exprReadsColumns(&e.Case.Whens[i].Cond) || exprReadsColumns(&e.Case.Whens[i].Result) {
				return true
			}
		}
		if e.Case.Els != nil && exprReadsColumns(e.Case.Els) {
			return true
		}
		return false
	case exprCoalesce:
		for i := range e.Coalesce {
			if exprReadsColumns(&e.Coalesce[i]) {
				return true
			}
		}
		return false
	case exprGreatestLeast:
		for i := range e.GreatestLeast {
			if exprReadsColumns(&e.GreatestLeast[i]) {
				return true
			}
		}
		return false
	case exprQuantified:
		return exprReadsColumns(&e.Quantified.Lhs) || exprReadsColumns(&e.Quantified.Array)
	default:
		return false
	}
}
