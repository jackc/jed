//! Statement execution entry, transaction control, sequence runtime, and admission checks (mirrors
//! impl/go execute.go + privilege_check.go): the sequence runtime (seq_nextval/setval/lastval/currval),
//! execute_stmt, begin_tx/commit_tx/rollback, and the per-statement admission gates (check_privileges,
//! check_lifetime_admission) — as Engine methods.

use super::*;
use crate::ast::SavepointOp;
use crate::executor::dml::insert_default_needs;

impl Engine {
    /// `nextval('name')` (spec/design/sequences.md §4): advance the named sequence and return the
    /// new value. Interior-mutable (the evaluator borrows `&Engine`): the running state lives in
    /// `pending_seq`, seeded from the working snapshot on first touch this statement, and is flushed
    /// into the working snapshot + `session_seq` on statement success ([`flush_pending_sequences`]).
    /// A missing sequence is 42P01; advancing past a bound without CYCLE is 2200H.
    pub(crate) fn seq_nextval(&self, name: &str) -> Result<i64> {
        let key = name.to_string();
        let mut pending = self.session.pending_seq.borrow_mut();
        let mut def = match pending.get(&key) {
            Some(d) => d.clone(),
            None => self.sequence(name).cloned().ok_or_else(|| {
                EngineError::new(
                    SqlState::UndefinedTable,
                    format!("relation does not exist: {name}"),
                )
            })?,
        };
        let result = if !def.is_called {
            // The first nextval returns START (the current last_value) without incrementing.
            def.is_called = true;
            def.last_value
        } else {
            // Advance by increment, treating an i64 overflow or a bound crossing identically.
            let stepped = def.last_value.checked_add(def.increment);
            let next = match stepped {
                Some(n) if def.increment > 0 && n <= def.max_value => n,
                Some(n) if def.increment < 0 && n >= def.min_value => n,
                _ => {
                    if def.cycle {
                        if def.increment > 0 {
                            def.min_value
                        } else {
                            def.max_value
                        }
                    } else {
                        return Err(EngineError::new(
                            SqlState::SequenceGeneratorLimitExceeded,
                            format!(
                                "nextval: reached {} value of sequence {name}",
                                if def.increment > 0 {
                                    "maximum"
                                } else {
                                    "minimum"
                                }
                            ),
                        ));
                    }
                }
            };
            def.last_value = next;
            next
        };
        pending.insert(key.clone(), def);
        // nextval defines this session's currval for the sequence AND makes it the lastval target
        // (the most-recent-nextval sequence; lastval then reads its current session value — §6).
        self.session
            .pending_currval
            .borrow_mut()
            .insert(key.clone(), result);
        *self.session.pending_last_name.borrow_mut() = Some(key);
        Ok(result)
    }

    /// `setval('name', n)` / `setval('name', n, is_called)` (spec/design/sequences.md §4): set the
    /// sequence's counter directly and return `n`. A missing sequence is 42P01; `n` outside
    /// `[min_value, max_value]` is 22003. `last_value = n`, `is_called` = the flag (default true);
    /// when `is_called` is true the value also defines this session's `currval` (PG: `is_called =
    /// false` leaves `currval` untouched). `setval` never updates `lastval` (PG — §6).
    pub(crate) fn seq_setval(&self, name: &str, n: i64, is_called: bool) -> Result<i64> {
        let key = name.to_string();
        let mut pending = self.session.pending_seq.borrow_mut();
        let mut def = match pending.get(&key) {
            Some(d) => d.clone(),
            None => self.sequence(name).cloned().ok_or_else(|| {
                EngineError::new(
                    SqlState::UndefinedTable,
                    format!("relation does not exist: {name}"),
                )
            })?,
        };
        if n < def.min_value || n > def.max_value {
            return Err(EngineError::new(
                SqlState::NumericValueOutOfRange,
                format!(
                    "setval: value {n} is out of bounds for sequence {name} ({}..{})",
                    def.min_value, def.max_value
                ),
            ));
        }
        def.last_value = n;
        def.is_called = is_called;
        pending.insert(key.clone(), def);
        // currval is defined only when is_called (PG do_setval: elm->last_valid set iff iscalled).
        if is_called {
            self.session.pending_currval.borrow_mut().insert(key, n);
        }
        Ok(n)
    }

    /// `lastval()` (spec/design/sequences.md §6): the **current** session value of the sequence the
    /// most recent `nextval` (of any sequence) ran on IN THIS SESSION — PG reads the last-used
    /// sequence's cached value, so a `setval` on that same sequence is reflected, while a `setval`
    /// on a *different* sequence is not (it does not change which sequence this points to). Takes no
    /// name argument (no 42P01 path); `55000` before the first `nextval` this session. The effective
    /// name and its value both honor the statement's running updates over the session state.
    pub(crate) fn seq_lastval(&self) -> Result<i64> {
        let name = self
            .session
            .pending_last_name
            .borrow()
            .clone()
            .or_else(|| self.session.session_last_name.clone());
        let key = match name {
            Some(k) => k,
            None => {
                return Err(EngineError::new(
                    SqlState::ObjectNotInPrerequisiteState,
                    "lastval is not yet defined in this session".to_string(),
                ));
            }
        };
        if let Some(v) = self.session.pending_currval.borrow().get(&key) {
            return Ok(*v);
        }
        if let Some(v) = self.session.session_seq.get(&key) {
            return Ok(*v);
        }
        // A nextval always defines the sequence's session value, so a recorded last-name with no
        // value is unreachable; fall back to 55000 defensively rather than panic.
        Err(EngineError::new(
            SqlState::ObjectNotInPrerequisiteState,
            "lastval is not yet defined in this session".to_string(),
        ))
    }

    /// `currval('name')` (spec/design/sequences.md §6): the value `nextval`/`setval(…,true)` last
    /// produced for this sequence IN THIS SESSION. Resolves the name against the catalog first
    /// (42P01 if absent), then reads the running update this statement (`pending_currval`) else the
    /// session value (`session_seq`); 55000 if it has not been defined this session.
    pub(crate) fn seq_currval(&self, name: &str) -> Result<i64> {
        if self.sequence(name).is_none() {
            return Err(EngineError::new(
                SqlState::UndefinedTable,
                format!("relation does not exist: {name}"),
            ));
        }
        let key = name.to_string();
        if let Some(v) = self.session.pending_currval.borrow().get(&key) {
            return Ok(*v);
        }
        if let Some(v) = self.session.session_seq.get(&key) {
            return Ok(*v);
        }
        Err(EngineError::new(
            SqlState::ObjectNotInPrerequisiteState,
            format!("currval of sequence {name} is not yet defined in this session"),
        ))
    }

    /// Flush the statement's pending sequence advances into the working snapshot (so a commit
    /// persists them) and the pending session updates into `session_seq`/`session_last` (so
    /// `currval`/`lastval` see them). Called on the success of a sequence-advancing statement, while
    /// a write transaction is open; a no-op when nothing advanced. On statement error the pending
    /// state is instead discarded (cleared at the next statement), giving the transactional rollback
    /// of the advance (sequences.md §5).
    pub(crate) fn flush_pending_sequences(&mut self) {
        let pending = std::mem::take(&mut *self.session.pending_seq.borrow_mut());
        for def in pending.into_values() {
            // Route each advance to its owning scope (temp-tables.md §8): a `serial`/IDENTITY temp
            // column's owned sequence flushes into its temp snapshot (zero file writes), a persistent
            // one into the main image.
            self.put_sequence_routed(def);
        }
        let currvals = std::mem::take(&mut *self.session.pending_currval.borrow_mut());
        for (key, v) in currvals {
            self.session.session_seq.insert(key, v);
        }
        if let Some(name) = self.session.pending_last_name.borrow_mut().take() {
            self.session.session_last_name = Some(name);
        }
    }

    /// The oldest still-live snapshot's txid (spec/design/transactions.md §8) — the Phase-6
    /// free-list reclamation gate. Single-handle (P5.3a) it is trivially the committed txid (no
    /// other reader pins an older one yet); P5.3b's shared read snapshots make it meaningful.
    pub fn oldest_live_txid(&self) -> u64 {
        self.committed.txid
    }

    /// Whether an explicit transaction block is currently open on the **default session**
    /// (spec/design/transactions.md §4.2). False under autocommit. Used by the host `Transaction`
    /// surface (api.md §6).
    pub fn in_transaction(&self) -> bool {
        self.session.tx.is_some()
    }

    /// The default session's transaction status (`Idle`/`Open`/`Failed`, spec/design/session.md
    /// §2.2) — the explicit three-state machine the convenience methods drive.
    pub fn status(&self) -> TxStatus {
        self.session.status()
    }

    /// Whether the open transaction has been aborted (a statement errored → it is in the failed
    /// state, §6). False under autocommit or for a clean block. The shared write handle
    /// ([`crate::shared`]) reads this at commit to know whether to publish (a failed block
    /// publishes nothing — a failed COMMIT is a ROLLBACK, PostgreSQL).
    pub(crate) fn tx_failed(&self) -> bool {
        self.session.tx.as_ref().is_some_and(|t| t.is_failed())
    }

    /// The monotonic commit counter (spec/design/api.md §2): 0 for a fresh in-memory database,
    /// the file's value on open, bumped by 1 per `commit`.
    pub fn txid(&self) -> u64 {
        self.committed.txid
    }

    /// The page size this database serializes with (spec/design/api.md §2).
    pub fn page_size(&self) -> u32 {
        self.page_size
    }

    /// The committed **logical** page high-water — the number of pages the on-disk image references
    /// (the count the meta records, format.md). This is the size an incremental commit extends at
    /// (spec/fileformat/format.md *Reclamation*); it is **not** the physical file length, which the
    /// chunked preallocation ([`crate::pager`], spec/design/pager.md §7) runs ahead of with trailing
    /// zero slack. `0` for a fresh in-memory database.
    pub fn page_count(&self) -> u32 {
        self.page_count
    }

    /// Set the execution-cost ceiling for statements run on this handle (CLAUDE.md §13;
    /// spec/design/api.md §8). A positive `limit` bounds every subsequent statement: it
    /// aborts with `54P01` the instant accrued cost reaches `limit` (spec/design/cost.md §6).
    /// `limit <= 0` (the default) is **unlimited**. The primary guard for safely evaluating
    /// untrusted, user-supplied queries; a handle setting, not stored in the file.
    pub fn set_max_cost(&mut self, limit: i64) {
        self.session.max_cost = limit;
    }

    /// Set the **per-session cumulative cost budget** on the default session (spec/design/session.md
    /// §5.4); `limit <= 0` (the default) is **unlimited**. Where `max_cost` bounds one statement
    /// (`54P01`), this bounds the whole session: the instant the session's running cumulative cost
    /// reaches `limit` the in-flight statement aborts `54P02`, and once spent every further statement
    /// is rejected `54P02` at admission. The multi-tenant / untrusted-host gate atop `max_cost`; a
    /// handle setting, not stored in the file.
    pub fn set_lifetime_max_cost(&mut self, limit: i64) {
        self.session.lifetime_max_cost = limit;
    }

    /// The default session's per-session cumulative cost budget (`0` ⇒ unlimited).
    /// See [`set_lifetime_max_cost`](Engine::set_lifetime_max_cost).
    pub fn lifetime_max_cost(&self) -> i64 {
        self.session.lifetime_max_cost
    }

    /// The default session's running **cumulative** execution cost so far (spec/design/session.md
    /// §5.4) — the gauge the `lifetime_max_cost` budget bounds. Tracked even when unlimited; survives
    /// a transaction rollback (session state, not snapshot state).
    pub fn lifetime_cost(&self) -> i64 {
        self.session.lifetime_total.get()
    }

    /// Replace the default session's default table-privilege set — the `GRANT … ON ALL TABLES`
    /// default (spec/design/session.md §5.3). `PrivilegeSet::EMPTY.with(Privilege::Select)` makes the
    /// session read-only (a write resolves to `42501`). A handle setting, not stored in the file.
    pub fn set_default_privileges(&mut self, privs: PrivilegeSet) {
        self.session.privileges.set_default_table(privs);
    }

    /// Grant `privs` on a specific object (table or function) on the default session, beyond the
    /// default (§5.3).
    pub fn grant(&mut self, privs: PrivilegeSet, object: &str) {
        self.session.privileges.grant(privs, object);
    }

    /// Revoke `privs` from a specific object on the default session (revoke wins over grant and the
    /// default, §5.3).
    pub fn revoke(&mut self, privs: PrivilegeSet, object: &str) {
        self.session.privileges.revoke(privs, object);
    }

    /// Reset the default session's authorization envelope to fully permissive — every table
    /// privilege, no per-object delta, DDL allowed (§5.3). The conformance harness calls this before
    /// each record so a `# default_privileges:` / `# grant:` / `# revoke:` / `# allow_ddl:` directive
    /// never leaks past the record it decorates.
    pub fn reset_privileges(&mut self) {
        self.session.privileges = crate::privileges::Privileges::default();
        self.session.allow_ddl = true;
        // The temp-DDL gate is part of the authorization envelope (temp-tables.md §5); reset it with
        // the rest so a `# allow_temp_ddl:` directive never leaks past its record. Default-inherit the
        // reset `allow_ddl = true`.
        self.session.allow_temp_ddl = true;
    }

    /// Read-only access to the default session's authorization envelope (§5.3).
    pub fn privileges(&self) -> &crate::privileges::Privileges {
        &self.session.privileges
    }

    /// Set whether DDL is permitted on the default session (§5.3); a denied schema change is `42501`.
    pub fn set_allow_ddl(&mut self, allow: bool) {
        self.session.allow_ddl = allow;
    }

    /// Whether DDL is permitted on the default session.
    pub fn allow_ddl(&self) -> bool {
        self.session.allow_ddl
    }

    /// Set whether session-local temporary-table DDL is permitted on the default session
    /// (spec/design/temp-tables.md §5); a denied temp DDL is `42501`. The temp-scoped split of
    /// `allow_ddl` — a host may grant this while withholding persistent DDL (the untrusted-scratch
    /// pattern). A handle setting, not stored in the file.
    pub fn set_allow_temp_ddl(&mut self, allow: bool) {
        self.session.allow_temp_ddl = allow;
    }

    /// Whether session-local temporary-table DDL is permitted on the default session.
    pub fn allow_temp_ddl(&self) -> bool {
        self.session.allow_temp_ddl
    }

    /// Set the default session's temp-table RESIDENT budget in **bytes** (spec/design/temp-tables.md
    /// §6); `0` ⇒ never spill. Past it a file host moves the temp domain to a scratch file whose page
    /// cache it bounds. A handle setting, not stored in the file; it never changes results or cost.
    pub fn set_temp_buffers(&mut self, bytes: usize) {
        self.session.temp_buffers = bytes;
    }

    /// The default session's temp-table resident budget (`0` ⇒ never spill).
    pub fn temp_buffers(&self) -> usize {
        self.session.temp_buffers
    }

    /// Set the default session's temp-table storage CEILING in **bytes** (spec/design/temp-tables.md
    /// §7); `0` ⇒ unlimited. An over-ceiling temp write aborts `54P03`. A handle setting, not stored
    /// in the file.
    pub fn set_max_temp_storage_bytes(&mut self, bytes: usize) {
        self.session.max_temp_storage_bytes = bytes;
    }

    /// The default session's temp-table storage ceiling (`0` ⇒ unlimited).
    pub fn max_temp_storage_bytes(&self) -> usize {
        self.session.max_temp_storage_bytes
    }

    /// Set a session variable on the default session (spec/design/session.md §6.1). Custom variables
    /// must be **namespaced** (a dotted name); a non-dotted name is `42704`. Read it back in SQL with
    /// `current_setting('name'[, missing_ok])`.
    pub fn set_var(&mut self, name: &str, value: &str) -> Result<()> {
        self.session.set_var(name, value)
    }

    /// Clear a session variable on the default session (§6.1); a non-dotted name is `42704`.
    pub fn reset_var(&mut self, name: &str) -> Result<()> {
        self.session.reset_var(name)
    }

    /// Set the **time zone** on the default session (spec/design/session.md §6.2, timezones.md §9.4):
    /// the zone a `timestamptz` is decomposed *in* by `date_trunc` / `EXTRACT` / the cross-family
    /// casts. Accepts `UTC`, a fixed `±HH:MM` offset, or a named IANA zone a loaded `JTZ` bundle
    /// provides; otherwise `22023`.
    pub fn set_time_zone(&mut self, zone: &str) -> Result<()> {
        self.session.set_time_zone(zone)
    }

    /// Read a session variable's value on the default session (§6.1), or `None` if it is not set.
    pub fn var(&self, name: &str) -> Option<String> {
        self.session.var(name)
    }

    /// Clear every session variable on the default session (§6.1) — PostgreSQL's `RESET ALL` for the
    /// variable map (also the conformance harness `# set:` reset hook).
    pub fn reset_vars(&mut self) {
        self.session.reset_vars();
    }

    /// Set the maximum input SQL length, in **bytes**, accepted on this handle (CLAUDE.md §13;
    /// spec/design/api.md §8). A statement whose text exceeds `bytes` is rejected with `54000`
    /// at parse entry, before lexing — the §13 input-size gate (cost.md §7). `0` is **unlimited**
    /// (a trusted caller's opt-out); the default is [`DEFAULT_MAX_SQL_LENGTH`] (1 MiB). A handle
    /// setting, not stored in the file (mirrors `set_max_cost`).
    pub fn set_max_sql_length(&mut self, bytes: usize) {
        self.session.max_sql_length = bytes;
    }

    /// The current input-SQL byte limit (`0` ⇒ unlimited). See [`set_max_sql_length`](Engine::set_max_sql_length).
    pub fn max_sql_length(&self) -> usize {
        self.session.max_sql_length
    }

    pub fn set_lock_timeout_ms(&mut self, milliseconds: u64) {
        self.session.lock_timeout_ms = milliseconds;
    }

    pub fn lock_timeout_ms(&self) -> u64 {
        self.session.lock_timeout_ms
    }

    /// Whether this handle was opened read-only (spec/design/api.md §2.1): every transaction
    /// defaults to READ ONLY, writes are `25006`, and the file is never written.
    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// The current execution-cost ceiling (`0` ⇒ unlimited). See [`set_max_cost`](Engine::set_max_cost).
    pub fn max_cost(&self) -> i64 {
        self.session.max_cost
    }

    /// Set the work-memory budget (in **bytes**) for blocking operators run on this handle
    /// (spec/design/spill.md §3, api.md §2.1): the `ORDER BY` external merge sort holds at most
    /// roughly this many bytes of rows resident before it spills sorted runs to disk. `0` is
    /// **unlimited** (never spill). It never changes what a query observes (results + cost are
    /// invariant — spill.md §6), only when an operator spills; an in-memory database ignores it (no
    /// file to spill to). A handle setting, not stored in the file (mirrors `set_max_cost`).
    /// Cumulative scalar allocation budget per statement; non-positive restores the default.
    pub fn set_max_scalar_bytes(&mut self, bytes: i64) {
        self.session.set_max_scalar_bytes(bytes);
    }
    pub fn max_scalar_bytes(&self) -> i64 {
        self.session.max_scalar_bytes()
    }
    /// Live query-memory budget per statement; non-positive restores the default, unlimited.
    pub fn set_max_query_memory_bytes(&mut self, bytes: i64) {
        self.session.set_max_query_memory_bytes(bytes);
    }
    pub fn max_query_memory_bytes(&self) -> i64 {
        self.session.max_query_memory_bytes()
    }
    pub fn set_work_mem(&mut self, bytes: usize) {
        self.session.work_mem = bytes;
    }

    /// The current work-memory budget in bytes (`0` ⇒ unlimited). See [`set_work_mem`](Engine::set_work_mem).
    pub fn work_mem(&self) -> usize {
        self.session.work_mem
    }

    /// The backing file path, or `None` for an in-memory database.
    pub fn path(&self) -> Option<&std::path::Path> {
        self.path.as_deref()
    }

    /// Look up a table definition by exact name. SessionState-local temp tables resolve
    /// FIRST (spec/design/temp-tables.md §3); preclude-overlaps guarantees a name is temp XOR
    /// persistent, so this is just "where the table lives", not a precedence contest. Falls through
    /// to the currently-visible main snapshot (the open transaction's working set, else committed).
    pub fn table(&self, name: &str) -> Option<&Table> {
        // Resolution walk (temp-tables.md §3): session-local temp → persistent. Preclude-overlaps keeps
        // a name in at most one scope, so this is just "where it lives".
        if let Some(t) = self.temp_read_snap().table(name) {
            return Some(t);
        }
        self.read_snap().table(name)
    }

    /// Look up a composite type definition by exact name in the currently-visible
    /// snapshot (spec/design/composite.md).
    pub fn composite_type(&self, name: &str) -> Option<&CompositeType> {
        self.read_snap().composite_type(name)
    }

    /// The canonical name of every table in the currently-visible snapshot, sorted ascending
    /// by exact name (the catalog's standing order — no map-iteration order may leak,
    /// CLAUDE.md §8). Secondary indexes are not tables and are excluded (api.md §6).
    pub fn table_names(&self) -> Vec<String> {
        self.read_snap().table_names()
    }

    /// All rows of a table in primary-key (encoded byte) order, or None if the table does not exist.
    /// Reads the visible snapshot. A test/debug convenience — the SELECT path scans through
    /// `iter_in_key_order` directly (propagating fault errors); this unwraps that `Result` for the
    /// in-memory callers (tests), which never fault.
    pub fn rows_in_key_order(&self, name: &str) -> Option<Vec<Row>> {
        self.read_snap().rows_in_key_order(name)
    }

    /// Register a new table and its (empty) store in the working snapshot (DDL is transactional —
    /// transactions.md §4.5).
    pub(crate) fn put_table(&mut self, table: Table) {
        let ps = self.page_size;
        self.working_mut().put_table(table, ps);
    }

    // `import_collation` / `export_collation` are GONE (the reference-only pivot,
    // spec/design/collation.md §4.2): a collation is provided by a host-loaded bundle and used by
    // name, never loaded into a *database*. There is no runtime path that constructs or bakes a
    // collation table — the only load is an environment's `load_unicode_data` of jed's own pinned
    // bundle bytes.

    /// The loaded zone set of the handle's environment (`db.LoadedTimeZones`, timezones.md §3.3,
    /// spec/design/environment.md) — every named zone (and alias) a loaded bundle provides, each as
    /// `(name, tzdata_version)`, ascending by name. `UTC` and fixed offsets are built in and not listed.
    pub fn loaded_time_zones(&self) -> Vec<crate::timezone::TimeZoneInfo> {
        self.session.env.loaded_time_zones()
    }

    /// The loaded collation set of the handle's environment (`db.LoadedCollations`,
    /// spec/design/collation.md §4.2) — every collation a loaded bundle provides, available to any
    /// database on this handle, ascending by name. A property of the environment, not of this
    /// database; for the collations this database *references*, use [`Engine::collations`].
    pub fn loaded_collations(&self) -> Vec<CollationInfo> {
        self.session.env.loaded_collations()
    }

    /// Set the per-database default collation (`db.SetDefaultCollation`, spec/design/collation.md
    /// §1). An un-annotated `text` column created afterward inherits it. The name must be `C` (resets
    /// to byte order) or a **loaded** collation (else 42704). Persisted as the `is_default` flag on
    /// that collation's reference entry at the next `commit` (the entry is emitted because the default
    /// references it — §5).
    pub fn set_default_collation(&mut self, name: &str) -> Result<()> {
        let base = self.host_write_base()?;
        if name != "C" && base.resolve_collation(name).is_none() {
            return Err(EngineError::new(
                SqlState::UndefinedObject,
                format!("collation \"{name}\" does not exist"),
            ));
        }
        let value = if name == "C" {
            None
        } else {
            Some(name.to_string())
        };
        if base.default_collation == value {
            self.adopt_host_write(None);
            return Ok(());
        }
        let mut work = base.clone();
        work.set_default_collation(value);
        self.adopt_host_write(Some(work));
        Ok(())
    }

    /// The snapshot a host catalog op (`set_default_collation` / `upgrade_collations`) writes on:
    /// the open block's working set, else committed. Inside a block the op obeys the block's write
    /// rules — 25P02 when aborted, 25006 when READ ONLY — rather than staging a change COMMIT discards.
    fn host_write_base(&self) -> Result<&Snapshot> {
        let Some(tx) = self.session.tx.as_ref() else {
            return Ok(&self.committed);
        };
        if tx.is_failed() {
            return Err(EngineError::new(
                SqlState::InFailedSqlTransaction,
                "current transaction is aborted, commands ignored until end of transaction block",
            ));
        }
        if !tx.writable {
            return Err(EngineError::new(
                SqlState::ReadOnlySqlTransaction,
                "cannot change collations in a read-only transaction",
            ));
        }
        Ok(&tx.working)
    }

    /// Install a host catalog op's result (`None` = unchanged): staged into the open block (published
    /// at its COMMIT), or — outside a block — swapped in as committed and reported to the session's
    /// publish.
    fn adopt_host_write(&mut self, work: Option<Snapshot>) {
        if let Some(tx) = self.session.tx.as_mut() {
            if let Some(work) = work {
                tx.working = work;
                tx.main_dirty = true;
            }
            return;
        }
        self.commit_changes = CommitChanges {
            main: work.is_some(),
            attached: false,
        };
        if let Some(work) = work {
            self.committed = work;
        }
    }

    /// Adopt a newly-loaded Unicode version for this database's skewed collations
    /// (`db.UpgradeCollations` — the REINDEX / COLLATION UPGRADE migration, spec/design/collation.md
    /// §12). A **privileged host op** like [`Engine::set_default_collation`] — **not** SQL-reachable,
    /// so an untrusted query can never trigger it (CLAUDE.md §13). For every collation whose file pin
    /// differs from the loaded bundle (`Skewed`), it rebuilds the collated keys (PK + indexes) under
    /// the loaded table and re-pins the stamp, clearing the skew so the affected tables are read-write
    /// again and regain collated-index pushdown. Whole-database + atomic (the rebuild stages in a
    /// snapshot clone swapped in only on success); idempotent (no skew ⇒ a no-op returning `0`). The
    /// change is persisted by the next explicit [`Engine::commit`]. Returns the number of collations
    /// re-pinned.
    pub fn upgrade_collations(&mut self) -> Result<usize> {
        let mut work = self.host_write_base()?.clone();
        let n = work.upgrade_collations(self.page_size)?;
        self.adopt_host_write((n > 0).then_some(work));
        Ok(n)
    }

    /// The per-database default collation name — `"C"` (byte order) unless `set_default_collation`
    /// moved it (`db.DefaultCollation`, spec/design/collation.md §1).
    pub fn default_collation(&self) -> String {
        self.read_snap()
            .default_collation()
            .unwrap_or("C")
            .to_string()
    }

    /// Introspect the collations **this database references** (`db.Collations`,
    /// spec/design/collation.md §4.2) — every collation its schema uses (a column's `COLLATE`, or the
    /// per-database default), each as `(name, unicode_version, cldr_version, content_hash,
    /// description, is_default)`, in ascending name order. This is the *per-file* view; for the
    /// environment's **loaded** set, use [`Engine::loaded_collations`]. `C` is built in and not
    /// listed.
    pub fn collations(&self) -> Vec<CollationInfo> {
        let default = self.committed.default_collation();
        // referenced_collations resolves each referenced name (from a loaded bundle).
        self.committed
            .referenced_collations()
            .unwrap_or_default()
            .into_iter()
            .map(|c| CollationInfo {
                name: c.name.clone(),
                unicode_version: c.unicode_version.clone(),
                cldr_version: c.cldr_version.clone(),
                content_hash: crate::format::crc32_ieee(&collation::serialize_table(&c)),
                description: c.description.clone(),
                is_default: default == Some(c.name.as_str()),
                // The slice-2d verdict: Skewed when the file's pin differs from the loaded bundle's
                // version (the object is read-only), else Full (collation.md §12).
                verdict: if self.committed.collation_skew(&c.name).is_some() {
                    CollationVerdict::Skewed
                } else {
                    CollationVerdict::Full
                },
            })
            .collect()
    }

    /// Execute one parsed statement with no bind parameters.
    pub fn execute_stmt(&mut self, stmt: Statement) -> Result<Outcome> {
        self.execute_stmt_params(stmt, &[])
    }

    /// Execute one parsed statement, binding `params` to its `$N` placeholders (an empty slice
    /// for an unparameterized statement). The DDL statements take no parameters — supplying any
    /// is a 42601 (spec/design/api.md §5).
    ///
    /// Transaction control (`BEGIN`/`COMMIT`/`ROLLBACK`) drives the handle's current-transaction
    /// state directly (spec/design/transactions.md §4.2). Otherwise the statement runs either
    /// inside the open explicit block or, with none open, under **autocommit** (§4.1):
    ///
    /// - **Inside a block** (§4.2/§6): an aborted block rejects every statement but COMMIT/ROLLBACK
    ///   with 25P02; a write in a READ ONLY block is 25006; otherwise the statement runs against
    ///   the working set in place — no per-statement durable write (the block publishes once, at
    ///   COMMIT). **Any** statement error aborts the block (it enters the failed state); the
    ///   statement's own two-phase pass already guarantees it wrote nothing partial (§6), so the
    ///   whole working set is discarded only at ROLLBACK.
    /// - **Autocommit** (§4.1): a read runs against the committed state directly; a write is its
    ///   own transaction — the committed state is captured first (the stores are O(1) clones via
    ///   the persistent map, [`crate::pmap`]), the statement runs, and on success the change is
    ///   made durable (synchronous, the single `persist` chokepoint). Any failure — in the
    ///   statement or in the durable write — restores the captured state (rollback-on-error),
    ///   discarding partial work and any rowid allocations (§7). For an in-memory database
    ///   `persist` is a no-op, so autocommit is pure in-memory visibility.
    pub fn execute_stmt_params(&mut self, stmt: Statement, params: &[Value]) -> Result<Outcome> {
        self.execute_stmt_params_cached(stmt, params, None)
    }

    /// `execute_stmt_params` with the private prepared-INSERT slot threaded to dispatch. A valid
    /// committed entry may be consulted in a block; a successful execution may publish a new entry
    /// only when its visible target signature still exactly matches committed state
    /// (spec/design/api.md §2.4).
    pub(crate) fn execute_stmt_params_cached(
        &mut self,
        stmt: Statement,
        params: &[Value],
        insert_cache: Option<&std::cell::RefCell<Option<CachedInsert>>>,
    ) -> Result<Outcome> {
        self.session.reset_statement_memory();
        match stmt {
            Statement::Begin { writable } => return self.begin_tx(writable),
            Statement::Commit => return self.commit_tx(),
            Statement::Rollback => return self.rollback_tx(),
            Statement::Savepoint { op, name } => return self.savepoint_stmt(op, &name),
            _ => {}
        }
        self.estimator_touched.clear();
        // Fresh per-statement sequence-advance scratch (a prior statement's error may have left it
        // populated — it is discarded, not flushed, on error; sequences.md §5).
        self.session.pending_seq.borrow_mut().clear();
        self.session.pending_currval.borrow_mut().clear();
        *self.session.pending_last_name.borrow_mut() = None;

        // Inside an explicit block? Read the flags, dropping the borrow before dispatch.
        if self.session.tx.is_some() {
            let (failed, writable) = {
                let tx = self.session.tx.as_ref().expect("tx is open");
                (tx.is_failed(), tx.writable)
            };
            if failed {
                return Err(EngineError::new(
                    SqlState::InFailedSqlTransaction,
                    "current transaction is aborted, commands ignored until end of transaction block",
                ));
            }
            // Run the statement; ANY error aborts the block (it enters the failed state — §6).
            let result = if let Some(kind) = reindex_outside_block(&stmt) {
                // REINDEX CONCURRENTLY / DATABASE / SYSTEM cannot run inside a transaction block
                // (PostgreSQL's PreventInTransactionBlock — spec/design/indexes.md §10).
                Err(EngineError::new(
                    SqlState::ActiveSqlTransaction,
                    format!("{kind} cannot run inside a transaction block"),
                ))
            } else if self.stmt_is_write_with_views(&stmt) && !writable {
                Err(EngineError::new(
                    SqlState::ReadOnlySqlTransaction,
                    format!(
                        "cannot execute {} in a read-only transaction",
                        stmt_kind(&stmt)
                    ),
                ))
            } else {
                // INSERT cache fill remains guarded by an exact visible-vs-committed schema
                // signature, so row-only blocks may fill while working DDL cannot.
                self.dispatch_stmt_cached(stmt, params, insert_cache, true)
            };
            // Enforce the post-statement budgets (temp storage, pending writes): an over-budget
            // statement becomes an error, which aborts the block (its staged rows roll back at
            // ROLLBACK).
            let result =
                result.and_then(|out| self.check_statement_budgets(out.cost()).map(|()| out));
            if result.is_ok() {
                // Land any nextval advances into the block's working snapshot; COMMIT publishes
                // them, ROLLBACK discards them with the rest of the working set (sequences.md §5).
                self.flush_pending_sequences();
            } else {
                self.session.tx.as_ref().expect("tx is open").mark_failed();
            }
            return result;
        }

        // Autocommit (no open block): an autocommit write runs as an implicit single-statement
        // transaction — open a working snapshot off `committed`, run, then commit on success /
        // discard on error. Because the write mutates only `working`, an error leaves `committed`
        // untouched (no restore needed); rolled-back rowid allocations vanish with `working` (§7).
        if !self.stmt_is_write_with_views(&stmt) {
            return self.dispatch_stmt(stmt, params);
        }
        // On a read-only handle the implicit transaction is READ ONLY (PostgreSQL hot-standby
        // behavior — api.md §2.1), so an autocommit write fails exactly like a write inside a
        // READ ONLY block.
        if self.read_only {
            return Err(EngineError::new(
                SqlState::ReadOnlySqlTransaction,
                format!(
                    "cannot execute {} in a read-only transaction",
                    stmt_kind(&stmt)
                ),
            ));
        }
        self.session.tx = Some(ActiveTx::open(
            true,
            self.committed.clone(),
            self.session.temp_committed.clone(),
            &self.session,
        ));
        match self.dispatch_stmt_cached(stmt, params, insert_cache, true) {
            Ok(outcome) => {
                // Enforce the post-statement budgets before committing: an over-budget implicit
                // transaction is discarded (rolling back its temp + main changes) and commits nothing.
                if let Err(e) = self.check_statement_budgets(outcome.cost()) {
                    if let Some(tx) = self.session.tx.take() {
                        self.restore_session_state(tx);
                    }
                    return Err(e);
                }
                // Persist any nextval advances into the working snapshot before publishing it
                // (sequences.md §5); a non-sequence statement flushes nothing.
                self.flush_pending_sequences();
                self.commit_tx().map(|_| outcome)
            }
            Err(e) => {
                // The statement failed before any flush, so session state is untouched; restore
                // from the captured copy anyway to keep the discard path uniform (sequences.md §6).
                if let Some(tx) = self.session.tx.take() {
                    self.restore_session_state(tx);
                }
                Err(e)
            }
        }
    }

    /// Open an explicit transaction block (spec/design/transactions.md §4.2). A nested `BEGIN` (a
    /// block is already open) is 25001. `writable` is the *requested* access mode: `None`
    /// (unspecified) defaults to READ WRITE on a normal handle and READ ONLY on a read-only
    /// handle (PostgreSQL hot-standby behavior — api.md §2.1); requesting READ WRITE on a
    /// read-only handle is 25006. The committed snapshot is captured as the transaction's
    /// working snapshot — a writable tx mutates it in place; a read-only tx reads it unchanged
    /// (read-your-snapshot, §4.3). Cheap: the persistent stores clone O(1) (pmap.rs) and the
    /// catalog is a shallow copy. `committed` is untouched until commit.
    pub(crate) fn begin_tx(&mut self, writable: Option<bool>) -> Result<Outcome> {
        if self.session.tx.is_some() {
            return Err(EngineError::new(
                SqlState::ActiveSqlTransaction,
                "there is already a transaction in progress",
            ));
        }
        if writable == Some(true) && self.read_only {
            return Err(EngineError::new(
                SqlState::ReadOnlySqlTransaction,
                "cannot set transaction read-write mode on a read-only database",
            ));
        }
        self.session.tx = Some(ActiveTx::open(
            writable.unwrap_or(!self.read_only),
            self.committed.clone(),
            self.session.temp_committed.clone(),
            &self.session,
        ));
        Ok(Outcome::Statement {
            cost: 0,
            rows_affected: None,
        })
    }

    /// Restore the handle's `currval`/`lastval` session state from a discarded transaction's
    /// captured copy (spec/design/sequences.md §5/§6) — the rollback of any in-block `nextval`/
    /// `setval` session updates. Called wherever a transaction is dropped without publishing.
    pub(crate) fn restore_session_state(&mut self, tx: ActiveTx) {
        self.session.session_seq = tx.saved_session_seq;
        self.session.session_last_name = tx.saved_session_last_name;
    }

    /// Commit the current transaction (spec/design/transactions.md §4.2). With no open block it is
    /// a lenient no-op success. A **failed** block, or any read-only tx, publishes nothing — the
    /// working snapshot is simply dropped (a failed COMMIT is thus a ROLLBACK, PostgreSQL). A READ
    /// WRITE block publishes its working snapshot: bump its txid, make it durable (the single
    /// `persist` chokepoint, §9), then swap it in as the new `committed` — a single pointer swap,
    /// the §3 short commit window. A durable-write failure leaves `committed` untouched and
    /// propagates (the commit failed; the working set is discarded). Returns to autocommit.
    pub(crate) fn commit_tx(&mut self) -> Result<Outcome> {
        self.staged_attachments.clear();
        self.commit_changes = CommitChanges::default();
        let mut tx = match self.session.tx.take() {
            None => {
                return Ok(Outcome::Statement {
                    cost: 0,
                    rows_affected: None,
                });
            }
            Some(tx) => tx,
        };
        if tx.is_failed() || !tx.writable {
            // A failed or read-only block publishes nothing — a failed COMMIT is a ROLLBACK (PG),
            // so any in-block session updates revert with the discarded working set (§5/§6). The
            // discarded `temp_working` rolls back temp changes too (dropped with `tx`).
            self.restore_session_state(tx);
            return Ok(Outcome::Statement {
                cost: 0,
                rows_affected: None,
            });
        }
        // The temp tables' ON COMMIT actions (temp-tables.md §5.1) — DELETE ROWS empties, DROP drops —
        // apply to the working temp snapshot first, so they commit (or are discarded) atomically with
        // the rest of the transaction and the refinement, budget precheck, and staging below all see
        // their result.
        // A DELETE ROWS table referenced from outside the set fails the commit (0A000), which then
        // discards the transaction like any other failed commit.
        match tx.temp_working.apply_on_commit_actions() {
            Ok(true) => tx.temp_dirty = true,
            Ok(false) => {}
            Err(e) => {
                self.restore_session_state(tx);
                return Err(e);
            }
        }
        // Writable-store access alone does not prove a mutation (transactions.md §2.1). Refine before
        // the budget precheck and the durable-writer count, so an unchanged domain is neither checked
        // nor persisted.
        tx.main_dirty = tx.main_dirty && tx.working.changed_since(&self.committed);
        tx.temp_dirty =
            tx.temp_dirty && tx.temp_working.changed_since(&self.session.temp_committed);
        tx.attach_dirty
            .retain(|name| tx.attach_working[name].changed_since(&self.attached_committed[name]));
        // A multi-root commit packs session temp and attachments before main persists at publish:
        // check every in-memory budget first, so a later domain's 54P06 discards the whole
        // transaction — session state restored as on ROLLBACK — before any domain has packed a page
        // (memory.md §8.3). An unchanged main does not persist, so it has no budget to check.
        if tx.temp_dirty || !tx.attach_dirty.is_empty() {
            if let Some(c) = &self.core {
                let attached: Vec<(&str, &Snapshot)> = tx
                    .attach_dirty
                    .iter()
                    .filter_map(|name| tx.attach_working.get(name).map(|ws| (name.as_str(), ws)))
                    .collect();
                let main = tx.main_dirty.then(|| {
                    let mut main = tx.working.clone();
                    main.txid = c.committed_version() + 1;
                    main
                });
                let main_stages_rows = tx.main_dirty && tx.working.staged_bytes() != 0;
                if let Err(e) = c.precheck_budgets(main.as_ref(), main_stages_rows, &attached) {
                    drop(attached);
                    self.restore_session_state(tx);
                    return Err(e);
                }
            }
        }
        let main_dirty = tx.main_dirty;
        let temp_dirty = tx.temp_dirty;
        let mut temp_working = tx.temp_working;
        let mut working = tx.working;
        let mut attach_working = tx.attach_working;
        let attach_dirty = tx.attach_dirty;
        // One durable writer per transaction (attached-databases.md §5): at most one FILE-backed database
        // — MAIN or an attached file — may be written per tx (any number of in-memory attachments +
        // session temp are free). Checked here, before any durable page is written (in the shared-core
        // path the main persist is deferred to `Session::publish`, and the attachment durable commits are
        // the loop below), so a violating tx commits nothing. Deterministic (a count, order-independent).
        {
            let mut durable = usize::from(main_dirty && self.main_is_durable());
            if let Some(c) = &self.core {
                durable += attach_dirty
                    .iter()
                    .filter(|name| c.attachment_is_file(name))
                    .count();
            }
            if durable > 1 {
                // `tx` was already taken above, so the working sets drop here — nothing is committed.
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "a transaction may modify at most one durable database",
                ));
            }
        }
        // A dirty session-local temp domain stages its working snapshot into its store (compact packed
        // leaves; a spill to the host scratch directory once it outgrows `temp_buffers`) BEFORE the main
        // persist, and adopts the page accounting only after it — so a failed temp write (a scratch-file
        // I/O error) commits nothing, and a failed main persist leaves the temp store matching its
        // still-committed root. Zero main-file writes (temp-tables.md §6).
        let temp_staged = match (temp_dirty, self.temp_storage.as_mut()) {
            (true, Some(ts)) => Some(ts.stage_temp(
                &mut temp_working,
                self.session.temp_buffers,
                self.spill.as_ref(),
            )?),
            _ => None,
        };
        self.commit_stages_rows = false;
        // Only a changed main domain persists (transactions.md §2.1): a transaction that touched ONLY
        // session-local temp tables makes ZERO file writes (spec/design/temp-tables.md §2), and an empty
        // block makes none at all. Temp state is adopted regardless — it is never serialized into main,
        // only swapped into the in-memory committed temp snapshot.
        if main_dirty {
            // The txid is the durable commit counter (spec/design/api.md §2): it advances only on a
            // file-backed commit. An in-memory commit swaps the snapshot but leaves txid unchanged
            // (an in-memory database stays at txid 0 — there is nothing to recover).
            if self.path.is_some() {
                working.txid = self.committed.txid + 1;
            }
            // Published writes are no longer pending (memory.md §7).
            self.commit_stages_rows = working.staged_bytes() != 0;
            working.clear_staged();
            self.persist(&working)?; // no-op for an in-memory database
            self.committed = working;
        }
        // Adopt the staged temp commit's page accounting and run within-session compaction relative to
        // the new temp root. Compaction is safe iff no streaming cursor holds an older temp tree.
        if let (Some(staged), Some(ts)) = (temp_staged, self.temp_storage.as_mut()) {
            let can_reclaim = self.open_streams.load(std::sync::atomic::Ordering::Relaxed) == 0;
            ts.adopt_staged(&temp_working, staged, can_reclaim)?;
        }
        // Adopt the transaction's temp changes into the committed temp snapshot (temp-tables.md §5) — the
        // temp analogue of publishing `committed`, but purely in memory. SessionState-local temp lives on
        // the session. Its stores' staging state resets like main's (memory.md §7), though temp writes
        // are never pending.
        temp_working.clear_staged();
        self.session.temp_committed = temp_working;
        // Adopt each dirtied host-attached database (attached-databases.md §5, the N-root commit) and
        // adopt it into this engine's pinned attached view, so `publish` swaps a new `Roots::attached`. An
        // IN-MEMORY attachment STAGES temp-style (the same incremental copy-on-write pack as temp,
        // NO fsync — no durability barrier) into free or past-high-water pages; its page accounting and
        // compaction are adopted only after main persists and the roots publish, so a failure anywhere
        // before that leaves its storage matching its still-published root. A FILE attachment (Slice 2)
        // commits DURABLY (dirty pages + alternating meta slot + fsync, its own page space) —
        // [`Shared::commit_attachment`] branches on the storage kind — and goes last, after every
        // in-memory stage, so only main's persist follows its durable point. The root is DATABASE-scoped
        // (published, cross-session-visible). At most one file attachment is dirty here (the
        // one-durable-writer check above), so ≤1 fsync path runs. A file attachment's reclamation is
        // safe only when no cross-session reader pins an older root (the live-registry watermark — the
        // committing writer holds the gate but is not in `live`).
        if !attach_dirty.is_empty() {
            let core = self.core.clone();
            let can_reclaim = core.as_ref().is_none_or(|c| !c.has_live_readers());
            let mut names: Vec<&String> = attach_dirty.iter().collect();
            names.sort_by_key(|name| {
                let file = core.as_ref().is_some_and(|c| c.attachment_is_file(name));
                (file, name.as_str())
            });
            let mut staged = Vec::new();
            let mut na = self.attached_committed.clone();
            for name in names {
                let Some(mut ws) = attach_working.remove(name) else {
                    continue;
                };
                let stages_rows = ws.staged_bytes() != 0;
                ws.clear_staged();
                if let Some(c) = &core {
                    // A detached-mid-transaction attachment (unreachable under the writer gate) no-ops.
                    let base_txid = self.attached_committed.get(name).map_or(0, |a| a.txid);
                    staged.extend(c.commit_attachment(
                        name,
                        &mut ws,
                        base_txid,
                        can_reclaim,
                        stages_rows,
                    )?);
                }
                na.insert(name.clone(), std::sync::Arc::new(ws));
            }
            self.attached_committed = na;
            self.staged_attachments = staged;
        }
        self.commit_changes = CommitChanges {
            main: main_dirty,
            attached: !attach_dirty.is_empty(),
        };
        Ok(Outcome::Statement {
            cost: 0,
            rows_affected: None,
        })
    }

    /// Roll back the current transaction (spec/design/transactions.md §4.2). With no open block it
    /// is a no-op success. Otherwise the working snapshot is **dropped** — every staged
    /// INSERT/UPDATE/DELETE and DDL CREATE/DROP, plus any rowid allocations (§7), vanish with it;
    /// `committed` was never mutated, so there is nothing to restore there. The handle's
    /// `currval`/`lastval` session state, however, was updated in place by in-block `nextval`/
    /// `setval`, so it is restored from the block's captured copy (sequences.md §5/§6).
    pub(crate) fn rollback_tx(&mut self) -> Result<Outcome> {
        if let Some(tx) = self.session.tx.take() {
            self.restore_session_state(tx);
        }
        Ok(Outcome::Statement {
            cost: 0,
            rows_affected: None,
        })
    }

    /// `SAVEPOINT` / `RELEASE` / `ROLLBACK TO` (spec/design/transactions.md §11), PostgreSQL's
    /// semantics:
    ///
    /// - With no open block every form is `25P01` (no_active_sql_transaction); there is no block to
    ///   abort.
    /// - In a failed block `SAVEPOINT` and `RELEASE` are `25P02`; `ROLLBACK TO` of an existing
    ///   savepoint is the recovery path (it clears the failed state), and of an unknown one is
    ///   `3B001`, leaving the block failed.
    /// - In a clean block an unknown name is `3B001`, which — like any statement error inside a
    ///   block — aborts it.
    ///
    /// Names shadow: `RELEASE`/`ROLLBACK TO` act on the newest savepoint of that name. `RELEASE`
    /// destroys it and every later savepoint, keeping their changes; `ROLLBACK TO` restores the state
    /// it marked and destroys every later savepoint but keeps it, so it can be rolled back to again.
    pub(crate) fn savepoint_stmt(&mut self, op: SavepointOp, name: &str) -> Result<Outcome> {
        let done = Ok(Outcome::Statement {
            cost: 0,
            rows_affected: None,
        });
        let Some(tx) = self.session.tx.as_mut() else {
            let what = match op {
                SavepointOp::Define => "SAVEPOINT",
                SavepointOp::Release => "RELEASE SAVEPOINT",
                SavepointOp::RollbackTo => "ROLLBACK TO SAVEPOINT",
            };
            return Err(EngineError::new(
                SqlState::NoActiveSqlTransaction,
                format!("{what} can only be used in transaction blocks"),
            ));
        };
        let failed = tx.is_failed();
        if failed && op != SavepointOp::RollbackTo {
            return Err(EngineError::new(
                SqlState::InFailedSqlTransaction,
                "current transaction is aborted, commands ignored until end of transaction block",
            ));
        }
        let missing = || {
            EngineError::new(
                SqlState::InvalidSavepointSpecification,
                format!("savepoint does not exist: {name}"),
            )
        };
        match op {
            SavepointOp::Define => {
                let frame = SavepointFrame {
                    name: name.to_string(),
                    working: tx.working.clone(),
                    temp_working: tx.temp_working.clone(),
                    attach_working: tx.attach_working.clone(),
                    main_dirty: tx.main_dirty,
                    temp_dirty: tx.temp_dirty,
                    attach_dirty: tx.attach_dirty.clone(),
                    session_seq: self.session.session_seq.clone(),
                    session_last_name: self.session.session_last_name.clone(),
                };
                tx.savepoints.push(frame);
                // The frame now holds the staged versions too, so a later rewrite or delete must not
                // return their bytes (memory.md §7.2).
                tx.forget_staged_keys();
            }
            SavepointOp::Release => {
                let Some(at) = tx.find_savepoint(name) else {
                    tx.mark_failed();
                    return Err(missing());
                };
                tx.savepoints.truncate(at);
            }
            SavepointOp::RollbackTo => {
                let Some(at) = tx.find_savepoint(name) else {
                    // A clean block aborts; a failed one stays failed (PostgreSQL).
                    tx.mark_failed();
                    return Err(missing());
                };
                tx.savepoints.truncate(at + 1);
                let frame = &tx.savepoints[at];
                tx.working = frame.working.clone();
                tx.temp_working = frame.temp_working.clone();
                tx.attach_working = frame.attach_working.clone();
                tx.main_dirty = frame.main_dirty;
                tx.temp_dirty = frame.temp_dirty;
                tx.attach_dirty = frame.attach_dirty.clone();
                self.session.session_seq = frame.session_seq.clone();
                self.session.session_last_name = frame.session_last_name.clone();
                // The frame stays on the stack and keeps holding its versions (memory.md §7.2).
                tx.forget_staged_keys();
                // Recovery: the block is usable again. The flag is cleared in place, so a cursor
                // born earlier in the block still poisons it if its drain later fails.
                tx.failed.store(false, std::sync::atomic::Ordering::Relaxed);
            }
        }
        done
    }

    /// Enforce the session's authorization envelope for `stmt` (spec/design/session.md §5.3).
    ///
    /// A fully-permissive session (the default — every table privilege, no per-object delta, DDL
    /// allowed) needs no check, so the common path returns immediately. Otherwise:
    ///
    /// - **DDL** (CREATE / DROP / ALTER) requires `allow_ddl`; denied ⇒ `42501`.
    /// - **DML** requires a per-table privilege for each table it reads (`SELECT`) or writes
    ///   (`INSERT`/`UPDATE`/`DELETE`) and `EXECUTE` for each named function it calls.
    ///
    /// Enforcement is at **name resolution**: a table privilege is required only for a name that
    /// resolves to an **existing** catalog table (a missing table stays `42P01`, raised later in
    /// execution; a CTE / derived-table label is statement-local, not a catalog object, and is
    /// skipped). The requirements are collected in a deterministic source-walk order, so the same
    /// statement aborts at the same point in every core (only the `42501` code is corpus-asserted;
    /// the offending-object message is informational).
    /// Reject a statement at **admission** when the session's lifetime cost budget is already spent
    /// (spec/design/session.md §5.4): if a budget is set and the session's cumulative cost has reached
    /// it, no further statement may run (it "cannot accrue") — `54P02`. A no-op when the budget is
    /// unlimited (the default), so the common path pays one comparison.
    pub(crate) fn check_lifetime_admission(&self) -> Result<()> {
        let limit = self.session.lifetime_max_cost;
        let total = self.session.lifetime_total.get();
        if limit > 0 && total >= limit {
            return Err(EngineError::new(
                SqlState::SessionCostLimitExceeded,
                format!("session exceeded the lifetime cost limit of {limit} (accrued {total})"),
            ));
        }
        Ok(())
    }

    pub(crate) fn check_privileges(&self, stmt: &Statement) -> Result<()> {
        // Fast path: a session that allows ALL DDL (persistent + temp) and grants every privilege pays
        // nothing. Both gates must be on, since temp DDL now has its own gate (temp-tables.md §5): a
        // session with `allow_ddl` on but the temp gate off must still reach the detailed check.
        if self.session.allow_ddl
            && self.session.allow_temp_ddl
            && self.session.privileges.is_permissive()
        {
            return Ok(());
        }
        let mut req = PrivReq::default();
        collect_stmt_privs(stmt, &mut req);
        // The functions a read view's body calls need the reader's EXECUTE (views.md §9).
        self.collect_view_function_privs(&mut req);
        // The catalog expressions a write evaluates — applied column defaults and the target's CHECK
        // constraints — carry no expression tree in the statement. Preflight walks them explicitly,
        // before execution resolves/evaluates them, so a revoked function cannot be invoked through
        // them (session.md §5.3).
        self.collect_catalog_expr_privs(stmt, &mut req);
        // REINDEX INDEX reads its owning table's rows: SELECT on that table (indexes.md §10).
        if let Statement::Reindex(rx) = stmt
            && rx.kind == ReindexKind::Index
            && let Some(name) = &rx.name
        {
            let owner = match rx.db.as_deref() {
                Some(q) => self.snap_for_scope(q).and_then(|s| s.find_index(name)),
                None => self
                    .temp_read_snap()
                    .find_index(name)
                    .or_else(|| self.find_index(name)),
            };
            if let Some((table_key, _)) = owner {
                let table_key = table_key.to_string();
                req.need_table(&table_key, Privilege::Select);
            }
        }
        if req.is_ddl {
            // DDL is gated by the kind of relation it targets (temp-tables.md §5): a session-local temp
            // table by `allow_temp_ddl`, everything else (persistent) by `allow_ddl`. The split lets a
            // host grant bounded scratch tables to an untrusted session while withholding persistent
            // schema changes. A CREATE TABLE is classified statically (`is_temp_ddl`); the remaining
            // temp-affecting DDL is classified by resolving the name — a DROP TABLE / CREATE INDEX by its
            // target table, a DROP INDEX by the index (preclude-overlaps keeps a name in at most one
            // scope).
            let allowed = if req.is_temp_ddl
                || matches!(stmt, Statement::DropTable(dt) if dt.names.iter().any(|n| self.is_temp_table(n)))
                || matches!(stmt, Statement::CreateIndex(ci) if self.is_temp_table(&ci.table))
                || matches!(stmt, Statement::DropIndex(di) if self.is_temp_index(&di.name))
                || matches!(stmt, Statement::DropSequence(ds) if ds.names.iter().any(|n| self.is_temp_sequence(n)))
                || matches!(stmt, Statement::AlterTable(at) if match at.db.as_deref() {
                    Some(q) => q == "temp",
                    None => self.is_temp_table(&at.name) || self.is_temp_view(&at.name),
                })
                // A temporary view's DDL is temp DDL too (views.md §10): spelled TEMP or `temp.`,
                // or naming one of the session's temporary views.
                || matches!(stmt, Statement::CreateView(cv) if cv.temporary || cv.db.as_deref() == Some("temp"))
                || matches!(stmt, Statement::DropView(dv) if dv.names.iter().any(|(db, n)| match db.as_deref() {
                    Some(q) => q == "temp",
                    None => self.is_temp_view(n),
                }))
                || matches!(stmt, Statement::AlterView(av) if match av.db.as_deref() {
                    Some(q) => q == "temp",
                    None => self.is_temp_view(&av.name),
                })
                || matches!(stmt, Statement::AlterSequence(als) if self.is_temp_sequence(&als.name))
            {
                self.session.allow_temp_ddl
            } else {
                self.session.allow_ddl
            };
            if !allowed {
                return Err(EngineError::new(
                    SqlState::InsufficientPrivilege,
                    "permission denied: DDL is not permitted in this session",
                ));
            }
        }
        let snap = self.read_snap();
        for (name, priv_) in &req.tables {
            let key = name.to_string();
            // Only a name that resolves to an existing catalog table is privilege-checked; a missing
            // one is left to raise 42P01 in execution (existence before authorization). A built-in
            // catalog relation (jed_tables / jed_columns) is gated exactly like a user table —
            // per-table SELECT on its own name under the session envelope, no special case
            // (introspection.md §5) — so an explicit-grant session sees the schema only if the host
            // granted it.
            let exists = is_catalog_rel_name(&key)
                || snap.table(&key).is_some()
                || snap.view(&key).is_some();
            if exists && !self.session.privileges.allows_table(&key, *priv_) {
                return Err(EngineError::new(
                    SqlState::InsufficientPrivilege,
                    format!("permission denied for table {key}"),
                ));
            }
        }
        for name in &req.functions {
            let key = name.to_string();
            if !self.session.privileges.allows_function(&key) {
                return Err(EngineError::new(
                    SqlState::InsufficientPrivilege,
                    format!("permission denied for function {key}"),
                ));
            }
        }
        Ok(())
    }

    /// Add the named functions of the catalog expressions a write evaluates (session.md §5.3): the
    /// column defaults an INSERT fills (an omitted column, a `DEFAULT` slot) or an UPDATE / `ON
    /// CONFLICT DO UPDATE` assigns with `SET c = DEFAULT`, and the target table's CHECK constraints.
    /// These carry no expression in the statement itself, so preflight walks the selected catalog
    /// expressions before execution resolves or evaluates them. A write through an updatable view is
    /// retargeted first, as execution does (views.md §15.2), so the base table's defaults and CHECKs
    /// apply. Covers top-level, writable-CTE, and EXPLAIN-contained statements. An unresolved target
    /// or column is left to execution, so its error precedence is unchanged.
    fn collect_catalog_expr_privs(&self, stmt: &Statement, req: &mut PrivReq) {
        match stmt {
            Statement::Update(upd) => self.collect_update_catalog_privs(upd, req),
            Statement::Insert(ins) => self.collect_insert_catalog_privs(ins, req),
            Statement::Merge(m) => self.collect_merge_catalog_privs(m, req),
            Statement::With(wq) => {
                for cte in &wq.ctes {
                    self.collect_cte_catalog_privs(&cte.body, req);
                }
                self.collect_cte_catalog_privs(&wq.body, req);
            }
            Statement::Explain { inner, .. } => self.collect_catalog_expr_privs(inner, req),
            // A `CREATE TABLE … AS` whose own WITH writes (create-table-as.md §1) runs those writes,
            // unless WITH NO DATA never runs the query.
            Statement::CreateTable(ct) => {
                if let Some(cta) = &ct.as_query
                    && cta.with_data
                    && let QueryExpr::With(w) = &cta.query
                {
                    for cte in &w.ctes {
                        self.collect_cte_catalog_privs(&cte.body, req);
                    }
                }
            }
            _ => {}
        }
    }

    fn collect_cte_catalog_privs(&self, body: &CteBody, req: &mut PrivReq) {
        match body {
            CteBody::Update(upd) => self.collect_update_catalog_privs(upd, req),
            CteBody::Insert(ins) => self.collect_insert_catalog_privs(ins, req),
            CteBody::Merge(m) => self.collect_merge_catalog_privs(m, req),
            _ => {}
        }
    }

    /// The catalog expressions a MERGE evaluates (spec/design/merge.md §10): the defaults of each
    /// INSERT action's omitted columns and `DEFAULT` slots, each UPDATE action's `SET c = DEFAULT`,
    /// and the target's CHECK constraints when any action writes.
    fn collect_merge_catalog_privs(&self, m: &crate::ast::Merge, req: &mut PrivReq) {
        use crate::ast::{MergeAction, MergeValue};
        let Some(table) = self.table_scoped(m.db.as_deref(), &m.table) else {
            return;
        };
        let locals = HashSet::new();
        let mut writes = false;
        for c in &m.clauses {
            match &c.action {
                MergeAction::Update(assigns) => {
                    writes = true;
                    collect_assignment_default_privs(table, assigns, req);
                }
                MergeAction::Insert {
                    columns,
                    overriding,
                    values,
                    ..
                } => {
                    writes = true;
                    let targets: Vec<Option<usize>> = match columns {
                        None => (0..table.columns.len()).map(Some).collect(),
                        Some(names) => names.iter().map(|n| table.column_index(n)).collect(),
                    };
                    let mut defaulted = vec![true; table.columns.len()];
                    for (p, v) in values.iter().enumerate() {
                        if let (MergeValue::Expr(_), Some(Some(i))) = (v, targets.get(p)) {
                            let identity_user = table.columns[*i].identity.is_some()
                                && *overriding == Some(Overriding::User);
                            if !identity_user {
                                defaulted[*i] = false;
                            }
                        }
                    }
                    for (col, need) in table.columns.iter().zip(defaulted) {
                        if let (true, Some(default)) = (need, &col.default_expr)
                            && !default.unparsed
                        {
                            collect_expr_privs(&default.expr, req, &locals);
                        }
                    }
                }
                MergeAction::Delete | MergeAction::DoNothing => {}
            }
        }
        if writes {
            collect_check_privs(table, req);
        }
    }

    fn collect_update_catalog_privs(&self, upd: &Update, req: &mut PrivReq) {
        let original = upd;
        let mut upd = upd.clone();
        if self.rewrite_update_target(&mut upd).is_err() {
            return;
        }
        // A `SET c = DEFAULT` through a view assigns the view column default (views.md §16.1).
        let locals = HashSet::new();
        for (a, was) in upd.assignments.iter().zip(&original.assignments) {
            if was.is_default {
                collect_expr_privs(&a.value, req, &locals);
            }
        }
        let Some(table) = self.table_scoped(upd.db.as_deref(), &upd.table) else {
            return;
        };
        collect_assignment_default_privs(table, &upd.assignments, req);
        collect_check_privs(table, req);
    }

    fn collect_insert_catalog_privs(&self, ins: &Insert, req: &mut PrivReq) {
        let original = ins;
        let mut ins = ins.clone();
        let Ok(vw) = self.rewrite_insert_target(&mut ins) else {
            return;
        };
        let Some(table) = self.table_scoped(ins.db.as_deref(), &ins.table) else {
            return;
        };
        // The view column defaults the INSERT applies replace the table's (views.md §16.1).
        let defaulted;
        let table = match vw.as_ref().filter(|v| !v.default_overrides.is_empty()) {
            Some(v) => match self.table_with_view_defaults(table, v, ins.overriding) {
                Ok(t) => {
                    defaulted = t;
                    &defaulted
                }
                Err(_) => return,
            },
            None => table,
        };
        if let (
            Some(OnConflict {
                action: ConflictAction::DoUpdate { assignments, .. },
                ..
            }),
            Some(OnConflict {
                action:
                    ConflictAction::DoUpdate {
                        assignments: was, ..
                    },
                ..
            }),
        ) = (&ins.on_conflict, &original.on_conflict)
        {
            let locals = HashSet::new();
            for (a, w) in assignments.iter().zip(was) {
                if w.is_default && vw.is_some() {
                    collect_expr_privs(&a.value, req, &locals);
                }
            }
        }
        // The columns this INSERT defaults, exactly as execution selects them (dml.rs): every column
        // the list omits (all of them for DEFAULT VALUES), every OVERRIDING USER VALUE identity
        // column, and every column a VALUES row fills with DEFAULT. An unknown or repeated list
        // column is skipped — execution reports it.
        let mut provided: Vec<Option<usize>> = match &ins.columns {
            None => (0..table.columns.len()).map(Some).collect(),
            Some(names) => {
                let mut provided = vec![None; table.columns.len()];
                for (p, name) in names.iter().enumerate() {
                    if let Some(idx) = table.columns.iter().position(|c| c.name == *name) {
                        provided[idx].get_or_insert(p);
                    }
                }
                provided
            }
        };
        if ins.overriding == Some(Overriding::User) {
            for (i, col) in table.columns.iter().enumerate() {
                if col.identity.is_some() {
                    provided[i] = None;
                }
            }
        }
        let rows = match &ins.source {
            InsertSource::Values(rows) => rows.as_slice(),
            InsertSource::Query(_) => &[],
        };
        let locals = HashSet::new();
        for (col, need) in table
            .columns
            .iter()
            .zip(insert_default_needs(&provided, rows))
        {
            if let (true, Some(default)) = (need, &col.default_expr)
                && !default.unparsed
            {
                collect_expr_privs(&default.expr, req, &locals);
            }
        }
        if let Some(OnConflict {
            action: ConflictAction::DoUpdate { assignments, .. },
            ..
        }) = &ins.on_conflict
        {
            collect_assignment_default_privs(table, assignments, req);
        }
        collect_check_privs(table, req);
    }

    /// Enforce the read-path admission gates a lazy read lane bypasses (the safe-total-`query`
    /// contract, CLAUDE.md §13). A SELECT served by a streaming (S3) / deferred (S7) cursor never
    /// reaches the materialized `execute_stmt_params`, where the failed-block / lifetime / privilege
    /// gates live — so a total `query` would leak restricted rows and run reads in a failed/exhausted
    /// session. `query_ast_cached` calls this before the lanes for a **read**: `25P02` (aborted block)
    /// / `54P02` (lifetime budget) / `42501` (privilege). Reads only — transaction control must still
    /// work in a failed block, and a write is gated inside `dispatch_stmt` on the materialized
    /// fall-through. The three checks are pure, so the fall-through re-running them is harmless.
    pub(crate) fn gate_read_lanes(&mut self, stmt: &Statement) -> Result<()> {
        self.session.reset_statement_memory();
        if self.tx_failed() {
            return Err(EngineError::new(
                SqlState::InFailedSqlTransaction,
                "current transaction is aborted, commands ignored until end of transaction block",
            ));
        }
        self.check_lifetime_admission()?;
        self.check_privileges(stmt)
    }

    /// Abort an open, failable block (spec/design/transactions.md §6) — the block-abort a lazy read
    /// lane bypasses. The materialized `execute_stmt_params` poisons in its block branch, but a SELECT
    /// served by a streaming/deferred cursor never reaches it; PostgreSQL aborts a block on ANY
    /// statement error, so a failing read must poison here. A no-op outside a block; idempotent. Only
    /// reads reach these paths (transaction control and writes go to `dispatch_stmt`, which
    /// self-poisons with the right nuance — a nested BEGIN's `25001` must NOT abort).
    pub(crate) fn fail_open_block(&self) {
        if let Some(tx) = self.session.tx.as_ref() {
            tx.mark_failed();
        }
    }

    /// Abort an open block when a lazy read lane errors at **open time** (a missing table, a denied
    /// read, a plan-time trap) — the counterpart to [`gate_read_lanes`](Engine::gate_read_lanes).
    /// Wraps a lane error return; `err` is returned unchanged.
    pub(crate) fn poison_on_lane_err(&self, err: EngineError) -> EngineError {
        self.fail_open_block();
        err
    }

    /// Hook a lazy-lane cursor so a **drain-time** read error inside an open block aborts it too
    /// (spec/design/transactions.md §6). A streaming (S3) / deferred (S7) cursor's error surfaces
    /// during the caller's drain — after `query` returned — so the open-time `poison_on_lane_err`
    /// cannot see it. The cursor cannot reach the block state (it outlives the `&mut Engine` borrow),
    /// so it holds a clone of the block's shared `failed` flag and flips it on error (the same
    /// shared-`Arc` channel the open-stream guard uses). A no-op when no block is open; a cursor that
    /// outlives its block only touches an orphaned flag (harmless).
    pub(crate) fn attach_block_poison(&self, rows: &mut Rows) {
        if let Some(tx) = self.session.tx.as_ref() {
            let flag = tx.failed.clone();
            rows.attach_error_hook(Box::new(move || {
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
            }));
        }
    }
}

/// Add the named functions of the column defaults an assignment list selects with `SET c = DEFAULT`.
fn collect_assignment_default_privs(
    table: &Table,
    assignments: &[crate::ast::Assignment],
    req: &mut PrivReq,
) {
    let locals = HashSet::new();
    for assignment in assignments.iter().filter(|a| a.is_default) {
        let default = table
            .columns
            .iter()
            .find(|col| col.name == assignment.column)
            .and_then(|col| col.default_expr.as_ref());
        if let Some(default) = default
            && !default.unparsed
        {
            collect_expr_privs(&default.expr, req, &locals);
        }
    }
}

/// Add the named functions of a write target's CHECK constraints, which the write evaluates on each
/// new row.
fn collect_check_privs(table: &Table, req: &mut PrivReq) {
    let locals = HashSet::new();
    // An unparsed check calls nothing this binary can name; the write itself fails XX003.
    for check in table.checks.iter().filter(|c| !c.unparsed) {
        collect_expr_privs(&check.expr, req, &locals);
    }
}

/// The PostgreSQL statement name when `stmt` is a REINDEX form that cannot run inside a transaction
/// block (`PreventInTransactionBlock`, spec/design/indexes.md §10): `CONCURRENTLY` first, then
/// `DATABASE` / `SYSTEM` — PostgreSQL's check order. `None` for every other statement.
fn reindex_outside_block(stmt: &Statement) -> Option<&'static str> {
    let Statement::Reindex(rx) = stmt else {
        return None;
    };
    if rx.concurrently {
        return Some("REINDEX CONCURRENTLY");
    }
    match rx.kind {
        ReindexKind::Database => Some("REINDEX DATABASE"),
        ReindexKind::System => Some("REINDEX SYSTEM"),
        _ => None,
    }
}
