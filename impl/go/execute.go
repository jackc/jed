package jed

import (
	"fmt"
	"strings"
)

// Statement execution entry, transaction control, and sequence runtime. This file holds the top-level
// execute path (ExecuteStmt/ExecuteStmtParams — parse-time param binding then admission + dispatch),
// transaction control (beginTx/newTx/commitTx/rollbackTx, restoreSessionState, the single-durable-writer
// check), and the sequence runtime (seqNextval/seqSetval/seqCurrval/seqLastval, flushPendingSequences —
// transactional per determinism.md §5). Dispatch to per-statement executors is in ddl.go.

// ExecuteStmt executes one parsed statement with no bind parameters.
func (db *engine) ExecuteStmt(stmt statement) (outcome, error) {
	return db.ExecuteStmtParams(stmt, nil)
}

// ExecuteStmtParams executes one parsed statement, binding params to its $N placeholders (nil
// for an unparameterized statement). DDL statements take no parameters — supplying any is a
// 42601 (spec/design/api.md §5).
//
// Transaction control (BEGIN/COMMIT/ROLLBACK) drives the handle's current-transaction state
// directly (spec/design/transactions.md §4.2). Otherwise the statement runs either inside the
// open explicit block or, with none open, under autocommit (§4.1):
//
//   - Inside a block (§4.2/§6): an aborted block rejects every statement but COMMIT/ROLLBACK with
//     25P02; a write in a READ ONLY block is 25006; otherwise the statement runs against the
//     working set in place — no per-statement durable write (the block publishes once, at COMMIT).
//     ANY statement error aborts the block (it enters the failed state); the statement's own
//     two-phase pass already guarantees it wrote nothing partial (§6), so the whole working set is
//     discarded only at ROLLBACK.
//   - Autocommit (§4.1): a read runs against the committed state directly; a write is its own
//     transaction — the committed state is captured first (the stores are O(1) clones via the
//     persistent map, pmap.go), the statement runs, and on success the change is made durable
//     (synchronous, the single persist chokepoint). Any failure — in the statement or in the
//     durable write — restores the captured state (rollback-on-error, discarding partial work and
//     any rowid allocations, §7). For an in-memory database persist is a no-op.
func (db *engine) ExecuteStmtParams(stmt statement, params []Value) (outcome, error) {
	return db.executeStmtParamsCached(stmt, params, nil)
}

// executeStmtParamsCached is ExecuteStmtParams with the private prepared-INSERT slot threaded to
// dispatch. The slot may be consulted inside a transaction and filled when the target signature
// still matches committed state; working DDL makes the executor's committed-base guard refuse it.
func (db *engine) executeStmtParamsCached(stmt statement, params []Value, ic *insertStmtCache) (outcome, error) {
	switch {
	case stmt.Begin != nil:
		return db.beginTx(stmt.Begin.Writable, stmt.Begin.ModeSet)
	case stmt.Commit != nil:
		return db.commitTx()
	case stmt.Rollback != nil:
		return db.rollbackTx()
	}
	clear(db.estimatorTouched)
	// Fresh per-statement sequence-advance scratch (a prior statement's error may have left it
	// populated — it is discarded, not flushed, on error; sequences.md §5).
	db.session.pendingSeq = nil
	db.session.pendingCurrval = nil
	db.session.pendingLastName = ""

	// Inside an explicit block?
	if db.session.tx != nil {
		if db.session.tx.failed {
			return outcome{}, newError(InFailedSqlTransaction,
				"current transaction is aborted, commands ignored until end of transaction block")
		}
		// Run the statement; ANY error aborts the block (it enters the failed state — §6).
		var out outcome
		var err error
		if stmtIsWrite(stmt) && !db.session.tx.writable {
			err = newError(ReadOnlySqlTransaction,
				"cannot execute "+stmtKind(stmt)+" in a read-only transaction")
		} else {
			out, err = db.dispatchStmtCached(stmt, params, ic, true)
		}
		// Enforce the temp-storage budget after a successful temp write (temp-tables.md §7): an
		// over-budget statement (session-local tempBuffers) becomes a 54P03 error, which aborts the
		// block (the staged temp rows roll back at ROLLBACK). A no-op for non-temp statements.
		if err == nil {
			err = db.checkTempBudget()
		}
		if err != nil {
			db.session.tx.failed = true
			return outcome{}, err
		}
		// Land any nextval advances into the block's working snapshot; COMMIT publishes them,
		// ROLLBACK discards them with the rest of the working set (sequences.md §5).
		db.flushPendingSequences()
		return out, nil
	}

	// Autocommit (no open block): an autocommit write runs as an implicit single-statement
	// transaction — open a working snapshot off committed, run, then commit on success / discard on
	// error. Because the write mutates only working, an error leaves committed untouched (no restore
	// needed); rolled-back rowid allocations vanish with working (§7).
	if !stmtIsWrite(stmt) {
		return db.dispatchStmt(stmt, params)
	}
	// On a read-only handle the implicit transaction is READ ONLY (PostgreSQL hot-standby
	// behavior — api.md §2.1), so an autocommit write fails exactly like a write inside a
	// READ ONLY block.
	if db.readOnly {
		return outcome{}, newError(ReadOnlySqlTransaction,
			"cannot execute "+stmtKind(stmt)+" in a read-only transaction")
	}
	db.session.tx = db.newTx(true)
	out, err := db.dispatchStmtCached(stmt, params, ic, true)
	// Enforce the temp-storage budget before committing (temp-tables.md §7): an over-budget temp write
	// in this implicit transaction (session-local tempBuffers) is discarded (rolling back the temp +
	// main changes) and surfaces 54P03.
	if err == nil {
		err = db.checkTempBudget()
	}
	if err != nil {
		// The statement failed before any flush, so session state is untouched; restore from the
		// captured copy anyway to keep the discard path uniform (sequences.md §6).
		db.restoreSessionState(db.session.tx)
		db.session.tx = nil
		return outcome{}, err
	}
	// Persist any nextval advances into the working snapshot before publishing it (sequences.md
	// §5); a non-sequence statement flushes nothing.
	db.flushPendingSequences()
	if _, cerr := db.commitTx(); cerr != nil {
		return outcome{}, cerr
	}
	return out, nil
}

// beginTx opens an explicit transaction (spec/design/transactions.md §4.2). A nested BEGIN (a block
// is already open) is 25001. writable/modeSet carry the *requested* access mode: with modeSet
// false the mode was unspecified and defaults to READ WRITE on a normal handle, READ ONLY on a
// read-only handle (PostgreSQL hot-standby behavior — api.md §2.1); requesting READ WRITE on a
// read-only handle is 25006. The committed snapshot is captured as the transaction's working
// snapshot — a writable tx mutates it in place; a read-only tx reads it unchanged (read-your-
// snapshot, §4.3). Cheap: the persistent stores clone O(1) (pmap.go) and the catalog is shallow.
// committed is untouched until commit.
func (db *engine) beginTx(writable, modeSet bool) (outcome, error) {
	if db.session.tx != nil {
		return outcome{}, newError(ActiveSqlTransaction, "there is already a transaction in progress")
	}
	if modeSet && writable && db.readOnly {
		return outcome{}, newError(ReadOnlySqlTransaction,
			"cannot set transaction read-write mode on a read-only database")
	}
	if !modeSet {
		writable = !db.readOnly
	}
	db.session.tx = db.newTx(writable)
	return outcome{Kind: outcomeStatement, Cost: 0}, nil
}

// newTx opens a transaction over a clone of the committed snapshot, capturing the handle's
// currval/lastval session state so it can be restored if the transaction is discarded (the
// rollback of any in-block nextval/setval session updates — spec/design/sequences.md §5/§6).
func (db *engine) newTx(writable bool) *activeTx {
	saved := make(map[string]int64, len(db.session.sessionSeq))
	for k, v := range db.session.sessionSeq {
		saved[k] = v
	}
	return &activeTx{
		writable:             writable,
		working:              db.committed.clone(),
		tempWorking:          db.session.tempCommitted.clone(),
		savedSessionSeq:      saved,
		savedSessionLastName: db.session.sessionLastName,
	}
}

// restoreSessionState restores the handle's currval/lastval session state from a discarded
// transaction's captured copy (spec/design/sequences.md §5/§6) — the rollback of any in-block
// nextval/setval session updates. Called wherever a transaction is dropped without publishing.
func (db *engine) restoreSessionState(tx *activeTx) {
	db.session.sessionSeq = tx.savedSessionSeq
	db.session.sessionLastName = tx.savedSessionLastName
}

// commitTx commits the current transaction (spec/design/transactions.md §4.2). With no open block
// it is a lenient no-op success. A failed block, or any read-only tx, publishes nothing — the
// working snapshot is dropped (a failed COMMIT is thus a ROLLBACK, PostgreSQL). A READ WRITE block
// publishes its working snapshot: bump its txid (file-backed only — an in-memory database stays at
// txid 0), make it durable (the single persist chokepoint, §9), then swap it in as committed. A
// durable-write failure leaves committed untouched and propagates. Returns to autocommit.
func (db *engine) commitTx() (outcome, error) {
	tx := db.session.tx
	if tx == nil {
		return outcome{Kind: outcomeStatement, Cost: 0}, nil
	}
	db.session.tx = nil
	if tx.failed || !tx.writable {
		// A failed or read-only block publishes nothing — a failed COMMIT is a ROLLBACK (PG), so any
		// in-block session updates revert with the discarded working set (§5/§6). The discarded
		// tempWorking rolls back temp changes too (dropped with tx).
		db.restoreSessionState(tx)
		return outcome{Kind: outcomeStatement, Cost: 0}, nil
	}
	working := tx.working
	// One durable writer per transaction (attached-databases.md §5): at most one FILE-backed database —
	// MAIN or an attached file — may be written per tx (any number of in-memory attachments + session
	// temp are free). Checked here, before any durable page is written (in the shared-core path the main
	// persist is deferred to Session.publish, and the attachment durable commits are the loop below), so a
	// violating tx commits nothing and rolls back cleanly. Deterministic (a count, order-independent).
	if err := db.checkOneDurableWriter(tx); err != nil {
		return outcome{}, err
	}
	// Persist the main image when it changed; a transaction that touched ONLY session-local temp tables
	// skips it entirely so a temp table makes ZERO file writes (temp-tables.md §2). An empty block (no
	// kind dirty) still persists, preserving prior behavior. Temp state is adopted regardless — never
	// serialized, only swapped into the in-memory committed temp snapshot.
	pureTemp := !tx.mainDirty && tx.tempDirty
	if !pureTemp {
		if db.path != "" {
			working.txid = db.committed.txid + 1
		}
		if err := db.persist(working); err != nil { // no-op for an in-memory database
			return outcome{}, err
		}
		db.committed = working
	}
	// A dirty session-local temp domain materializes its working snapshot into its MemoryBlockStore
	// (compact packed leaves + within-session compaction) before it is adopted — zero main-file writes
	// (temp-tables.md §6). Compaction is safe iff no streaming cursor holds an older temp tree.
	if tx.tempDirty && db.tempStorage != nil {
		if err := db.tempStorage.persistTemp(tx.tempWorking, db.openStreams == 0); err != nil {
			return outcome{}, err
		}
	}
	db.session.tempCommitted = tx.tempWorking
	// Adopt each dirtied host-attached database (attached-databases.md §5, the N-root commit) and adopt it
	// into this engine's pinned attached view, so publish swaps a new roots.attached. An IN-MEMORY
	// attachment materializes into its block store persistTemp-style (the same incremental copy-on-write
	// pack as temp, NO fsync — no durability barrier); a FILE attachment (Slice 2) commits DURABLY through
	// commitDurable (dirty pages + alternating meta slot + fsync, its own page space) and takes the
	// post-commit residency flip. The root is DATABASE-scoped (published, cross-session-visible). At most
	// one file attachment is dirty here (the one-durable-writer check above), so ≤1 fsync path runs.
	// Within-session compaction (in-memory only) is safe iff no cross-session reader pins an older root
	// (the live-registry watermark — the committing writer holds the gate but is not in `live`).
	if len(tx.attachDirty) > 0 {
		na := make(map[string]*snapshot, len(db.attachedCommitted))
		for k, v := range db.attachedCommitted {
			na[k] = v
		}
		canReclaim := db.core == nil || !db.core.hasLiveReaders()
		for name := range tx.attachDirty {
			ws := tx.attachWorking[name]
			att := db.core.attachment(name)
			if att == nil {
				continue // detached mid-transaction (unreachable under the writer gate) — nothing to persist
			}
			if att.isFile() {
				// Advance the version for the alternating meta slot + reopen (like the main file commit).
				ws.txid = db.attachedCommitted[name].txid + 1
				var err error
				if att.coordinator != nil && att.coordinator.lease() == leaseShared {
					err = att.storage.commitShared(ws, att.coordinator)
					if err != nil {
						att.coordinator.setLease(leasePoisoned)
					}
				} else {
					// A local reader pins every attached root, so reuse is safe only once that common
					// watermark has drained.
					err = att.storage.commitDurable(ws, canReclaim, canReclaim)
				}
				if err != nil {
					return outcome{}, err
				}
				ws.demoteCleanLeaves() // post-commit residency flip (bplus-reshape.md B4), like Session.publish
			} else if err := att.storage.persistTemp(ws, canReclaim); err != nil {
				return outcome{}, err
			}
			na[name] = ws
		}
		db.attachedCommitted = na
	}
	return outcome{Kind: outcomeStatement, Cost: 0}, nil
}

// checkOneDurableWriter enforces the one-durable-writer rule (attached-databases.md §5): a single
// transaction may modify at most one FILE-backed (durable) database — MAIN or one attached file. Any
// number of in-memory attachments and the session temp domain are free (their commit is a crash-free
// pointer swap). Counts the durable databases this tx dirtied; > 1 is 0A000 (the honest v1 narrowing —
// multi-file atomic write is Slice 3). Called at commit, before any durable page is written.
func (db *engine) checkOneDurableWriter(tx *activeTx) error {
	durable := 0
	if tx.mainDirty && db.mainIsDurable() {
		durable++
	}
	if db.core != nil {
		for name := range tx.attachDirty {
			if att := db.core.attachment(name); att != nil && att.isFile() {
				durable++
			}
		}
	}
	if durable > 1 {
		return newError(FeatureNotSupported, "a transaction may modify at most one durable database")
	}
	return nil
}

// mainIsDurable reports whether this handle's MAIN database is file-backed (durable) rather than
// in-memory — the input to the one-durable-writer count (§5). In the shared-core path the backing path
// lives on the core's storage; a standalone engine carries it on db.path.
func (db *engine) mainIsDurable() bool {
	if db.core != nil {
		return db.core.storage.path != ""
	}
	return db.path != ""
}

// rollbackTx rolls back the current transaction (spec/design/transactions.md §4.2). With no open
// block it is a no-op success. Otherwise the working snapshot is dropped — every staged
// INSERT/UPDATE/DELETE and DDL CREATE/DROP, plus any rowid allocations (§7), vanish with it;
// committed was never mutated, so there is nothing to restore there. The handle's currval/lastval
// session state, however, was updated in place by in-block nextval/setval, so it is restored from
// the block's captured copy (sequences.md §5/§6).
func (db *engine) rollbackTx() (outcome, error) {
	if db.session.tx != nil {
		db.restoreSessionState(db.session.tx)
	}
	db.session.tx = nil
	return outcome{Kind: outcomeStatement, Cost: 0}, nil
}

// seqNextval implements nextval('name') (spec/design/sequences.md §4): advance the named sequence
// and return the new value. The running state lives in pendingSeq, seeded from the working
// snapshot on first touch this statement, and is flushed into the working snapshot + sessionSeq on
// statement success (flushPendingSequences). A missing sequence is 42P01; advancing past a bound
// without CYCLE is 2200H.
func (db *engine) seqNextval(name string) (int64, error) {
	key := strings.ToLower(name)
	var def sequenceDef
	if db.session.pendingSeq != nil {
		if d, ok := db.session.pendingSeq[key]; ok {
			def = *d
		} else if snapDef := db.sequence(name); snapDef != nil {
			def = *snapDef
		} else {
			return 0, newError(UndefinedTable, "relation does not exist: "+name)
		}
	} else if snapDef := db.sequence(name); snapDef != nil {
		def = *snapDef
	} else {
		return 0, newError(UndefinedTable, "relation does not exist: "+name)
	}
	var result int64
	if !def.IsCalled {
		// The first nextval returns START (the current LastValue) without incrementing.
		def.IsCalled = true
		result = def.LastValue
	} else {
		// Advance by increment, treating an i64 overflow or a bound crossing identically.
		next, overflow := checkedAddInt64(def.LastValue, def.Increment)
		inRange := !overflow &&
			((def.Increment > 0 && next <= def.MaxValue) ||
				(def.Increment < 0 && next >= def.MinValue))
		if !inRange {
			if def.Cycle {
				if def.Increment > 0 {
					next = def.MinValue
				} else {
					next = def.MaxValue
				}
			} else {
				kind := "maximum"
				if def.Increment < 0 {
					kind = "minimum"
				}
				return 0, newError(SequenceGeneratorLimitExceeded,
					"nextval: reached "+kind+" value of sequence "+name)
			}
		}
		def.LastValue = next
		result = next
	}
	if db.session.pendingSeq == nil {
		db.session.pendingSeq = make(map[string]*sequenceDef)
	}
	d := def
	db.session.pendingSeq[key] = &d
	// nextval defines this session's currval for the sequence AND makes it the lastval target (the
	// most-recent-nextval sequence; lastval then reads its current session value — §6).
	if db.session.pendingCurrval == nil {
		db.session.pendingCurrval = make(map[string]int64)
	}
	db.session.pendingCurrval[key] = result
	db.session.pendingLastName = key
	return result, nil
}

// seqSetval implements setval('name', n) / setval('name', n, isCalled) (spec/design/sequences.md
// §4): set the sequence's counter directly and return n. A missing sequence is 42P01; n outside
// [MinValue, MaxValue] is 22003. LastValue = n, IsCalled = the flag (default true); when isCalled is
// true the value also defines this session's currval (PG: isCalled=false leaves currval untouched).
// setval never updates lastval (PG — §6).
func (db *engine) seqSetval(name string, n int64, isCalled bool) (int64, error) {
	key := strings.ToLower(name)
	var def sequenceDef
	if d, ok := db.session.pendingSeq[key]; ok {
		def = *d
	} else if snapDef := db.sequence(name); snapDef != nil {
		def = *snapDef
	} else {
		return 0, newError(UndefinedTable, "relation does not exist: "+name)
	}
	if n < def.MinValue || n > def.MaxValue {
		return 0, newError(NumericValueOutOfRange,
			fmt.Sprintf("setval: value %d is out of bounds for sequence %s (%d..%d)",
				n, name, def.MinValue, def.MaxValue))
	}
	def.LastValue = n
	def.IsCalled = isCalled
	if db.session.pendingSeq == nil {
		db.session.pendingSeq = make(map[string]*sequenceDef)
	}
	d := def
	db.session.pendingSeq[key] = &d
	// currval is defined only when isCalled (PG do_setval: elm->last_valid set iff iscalled).
	if isCalled {
		if db.session.pendingCurrval == nil {
			db.session.pendingCurrval = make(map[string]int64)
		}
		db.session.pendingCurrval[key] = n
	}
	return n, nil
}

// seqCurrval implements currval('name') (spec/design/sequences.md §6): the value nextval/
// setval(…,true) last produced for this sequence IN THIS SESSION. Resolves the name against the
// catalog first (42P01 if absent), then reads the running update this statement (pendingCurrval)
// else the session value (sessionSeq); 55000 if it has not been defined this session.
func (db *engine) seqCurrval(name string) (int64, error) {
	if db.sequence(name) == nil {
		return 0, newError(UndefinedTable, "relation does not exist: "+name)
	}
	key := strings.ToLower(name)
	if v, ok := db.session.pendingCurrval[key]; ok {
		return v, nil
	}
	if v, ok := db.session.sessionSeq[key]; ok {
		return v, nil
	}
	return 0, newError(ObjectNotInPrerequisiteState,
		"currval of sequence "+name+" is not yet defined in this session")
}

// seqLastval implements lastval() (spec/design/sequences.md §6): the CURRENT session value of the
// sequence the most recent nextval (of any sequence) ran on IN THIS SESSION — PG reads the last-used
// sequence's cached value, so a setval on that same sequence is reflected, while a setval on a
// different sequence is not. Takes no name argument (no 42P01); 55000 before the first nextval. The
// effective name and its value both honor the statement's running updates over the session state.
func (db *engine) seqLastval() (int64, error) {
	key := db.session.pendingLastName
	if key == "" {
		key = db.session.sessionLastName
	}
	if key == "" {
		return 0, newError(ObjectNotInPrerequisiteState,
			"lastval is not yet defined in this session")
	}
	if v, ok := db.session.pendingCurrval[key]; ok {
		return v, nil
	}
	if v, ok := db.session.sessionSeq[key]; ok {
		return v, nil
	}
	// A nextval always defines the sequence's session value, so a recorded last-name with no value
	// is unreachable; fall back to 55000 defensively rather than returning a wrong value.
	return 0, newError(ObjectNotInPrerequisiteState,
		"lastval is not yet defined in this session")
}

// flushPendingSequences lands the statement's pending sequence advances into the working snapshot
// (so a commit persists them) and the pending session updates into sessionSeq/sessionLastName (so
// currval/lastval see them). Called on the success of a sequence-advancing statement, while a write
// transaction is open; a no-op when nothing advanced. On statement error the pending state is
// instead discarded (cleared at the next statement), giving the transactional rollback (§5).
func (db *engine) flushPendingSequences() {
	for _, def := range db.session.pendingSeq {
		// Route each advance to its owning scope (temp-tables.md §8): a serial/IDENTITY temp column's
		// owned sequence flushes into its temp snapshot (zero file writes), a persistent one into main.
		db.putSequenceRouted(def)
	}
	if len(db.session.pendingCurrval) > 0 && db.session.sessionSeq == nil {
		db.session.sessionSeq = make(map[string]int64)
	}
	for key, v := range db.session.pendingCurrval {
		db.session.sessionSeq[key] = v
	}
	if db.session.pendingLastName != "" {
		db.session.sessionLastName = db.session.pendingLastName
	}
	db.session.pendingSeq = nil
	db.session.pendingCurrval = nil
	db.session.pendingLastName = ""
}

// checkedAddInt64 adds a + b, reporting overflow=true (and an undefined sum) when the result does
// not fit in an i64 — the overflow-safe sequence advance (sequences.md §4).
func checkedAddInt64(a, b int64) (sum int64, overflow bool) {
	sum = a + b
	// Overflow iff the operands share a sign that the sum does not.
	if (a > 0 && b > 0 && sum < 0) || (a < 0 && b < 0 && sum >= 0) {
		return 0, true
	}
	return sum, false
}
