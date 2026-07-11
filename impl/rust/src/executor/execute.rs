//! Statement execution entry, transaction control, sequence runtime, and admission checks (mirrors
//! impl/go execute.go + privilege_check.go): the sequence runtime (seq_nextval/setval/lastval/currval),
//! execute_stmt, begin_tx/commit_tx/rollback, and the per-statement admission gates (check_privileges,
//! check_lifetime_admission) — as Engine methods.

use super::*;

impl Engine {
    /// `nextval('name')` (spec/design/sequences.md §4): advance the named sequence and return the
    /// new value. Interior-mutable (the evaluator borrows `&Engine`): the running state lives in
    /// `pending_seq`, seeded from the working snapshot on first touch this statement, and is flushed
    /// into the working snapshot + `session_seq` on statement success ([`flush_pending_sequences`]).
    /// A missing sequence is 42P01; advancing past a bound without CYCLE is 2200H.
    pub(crate) fn seq_nextval(&self, name: &str) -> Result<i64> {
        let key = name.to_ascii_lowercase();
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
        let key = name.to_ascii_lowercase();
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
        let key = name.to_ascii_lowercase();
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

    /// Set the default session's per-session temp-table storage budget in **bytes**
    /// (spec/design/temp-tables.md §7); `0` ⇒ unlimited. An over-budget temp write aborts `54P03`. A
    /// handle setting, not stored in the file.
    pub fn set_temp_buffers(&mut self, bytes: usize) {
        self.session.temp_buffers = bytes;
    }

    /// The default session's per-session temp-table storage budget (`0` ⇒ unlimited).
    pub fn temp_buffers(&self) -> usize {
        self.session.temp_buffers
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

    /// Whether this handle was opened read-only (spec/design/api.md §2.1): every transaction
    /// defaults to READ ONLY, writes are `25006`, and the file is never written.
    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// The current execution-cost ceiling (`0` ⇒ unlimited). See [`set_max_cost`](Engine::set_max_cost).
    pub fn max_cost(&self) -> i64 {
        self.session.max_cost
    }

    /// Inject a random source for the uuid generators (spec/design/entropy.md §6) — the
    /// deterministic / reproducible path. Pass [`seeded_random_source`](crate::seam::seeded_random_source)
    /// for a byte-identical cross-core stream (the conformance path; tests use the `# seed:`
    /// directive). A handle setting, not stored in the file.
    pub fn set_random_source(&mut self, f: crate::seam::RandomSource) {
        self.session.seam.set_random(f);
    }

    /// Clear the injected random source: the generators return to the OS CSPRNG, drawn per value
    /// (production — unpredictable output).
    pub fn clear_random_source(&mut self) {
        self.session.seam.clear_random();
    }

    /// Inject a clock source for `uuidv7` (entropy.md §6) — e.g. [`fixed_clock`](crate::seam::fixed_clock)
    /// (the `# clock:` directive). After this, `uuidv7()` embeds the source's instant instead of the
    /// wall clock. A handle setting, not stored in the file.
    pub fn set_clock_source(&mut self, f: crate::seam::ClockSource) {
        self.session.seam.set_clock(f);
    }

    /// Clear the injected clock source: `uuidv7` returns to reading the wall clock (production).
    pub fn clear_clock_source(&mut self) {
        self.session.seam.clear_clock();
    }

    /// Set the work-memory budget (in **bytes**) for blocking operators run on this handle
    /// (spec/design/spill.md §3, api.md §2.1): the `ORDER BY` external merge sort holds at most
    /// roughly this many bytes of rows resident before it spills sorted runs to disk. `0` is
    /// **unlimited** (never spill). It never changes what a query observes (results + cost are
    /// invariant — spill.md §6), only when an operator spills; an in-memory database ignores it (no
    /// file to spill to). A handle setting, not stored in the file (mirrors `set_max_cost`).
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

    /// Look up a table definition by name (case-insensitive). SessionState-local temp tables resolve
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

    /// Look up a composite type definition by name (case-insensitive) in the currently-visible
    /// snapshot (spec/design/composite.md).
    pub fn composite_type(&self, name: &str) -> Option<&CompositeType> {
        self.read_snap().composite_type(name)
    }

    /// The canonical name of every table in the currently-visible snapshot, sorted ascending
    /// by lowercased name (the catalog's standing order — no map-iteration order may leak,
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
    // collation table — the only load is `load_unicode_data` of jed's own pinned bundle bytes.

    /// Load a `JUCD` Unicode-data bundle (`db.LoadUnicodeData`, spec/design/collation.md §4.2): its
    /// collations become resolvable by name for `COLLATE`, per-column collation, and `ORDER BY …
    /// COLLATE`. The loaded set is **engine-global** (§9), so a bundle loaded through any handle is
    /// visible everywhere — including to a later `Engine::open` of a file that *references* one of
    /// its collations. Privileged host op (not SQL-reachable, no path, no engine I/O — §11);
    /// **additive** and idempotent for an already-loaded bundle. A malformed bundle is `XX001`.
    /// (Mirrors the engine-global [`crate::collation::load_unicode_data`], which the host may call
    /// before opening any file.)
    pub fn load_unicode_data(&self, bytes: &[u8]) -> Result<()> {
        crate::collation::load_unicode_data(bytes)
    }

    /// Load a `JTZ` time-zone bundle into the engine-global loaded set (`db.LoadTimeZoneData`,
    /// spec/design/timezones.md §3.3). The bytes are jed's own pinned TZif (RFC 8536) wrapped in a
    /// manifest; the loaded zones become usable by `AT TIME ZONE`. Like the collation seam, this is a
    /// privileged host op (not SQL-reachable, no path, no engine I/O — §10), **additive** and
    /// idempotent for an already-loaded bundle, engine-global so it may be called before `open`. A
    /// malformed bundle is `XX001`. (`UTC` and fixed offsets are built in and need no load.)
    pub fn load_time_zone_data(&self, bytes: &[u8]) -> Result<()> {
        crate::timezone::load_time_zone_data(bytes)
    }

    /// Introspect the engine-global **loaded** zone set (`db.LoadedTimeZones`, timezones.md §3.3) —
    /// every named zone (and alias) a loaded bundle provides, each as `(name, tzdata_version)`,
    /// ascending by name. A property of the running engine, not of this database. `UTC` and fixed
    /// offsets are built in and not listed.
    pub fn loaded_time_zones(&self) -> Vec<crate::timezone::TimeZoneInfo> {
        crate::timezone::loaded_time_zones()
    }

    /// Introspect the engine-global **loaded** collation set (`db.LoadedCollations`,
    /// spec/design/collation.md §4.2) — every collation a loaded bundle provides, available to any
    /// database on this handle, each as `(name, unicode_version, cldr_version, content_hash,
    /// description, is_default)`, ascending by name. A property of the running *engine*, not of this
    /// database; for the collations this database *references*, use [`Engine::collations`].
    /// `is_default` is always `false` here (that is a per-database property). `C` is built in and not
    /// listed.
    pub fn loaded_collations(&self) -> Vec<CollationInfo> {
        collation::loaded_collation_tables()
            .into_iter()
            .map(|c| CollationInfo {
                name: c.name.clone(),
                unicode_version: c.unicode_version.clone(),
                cldr_version: c.cldr_version.clone(),
                content_hash: crate::format::crc32_ieee(&collation::serialize_table(&c)),
                description: c.description.clone(),
                is_default: false,
                // The loaded set IS the version reference — it can never be skewed against itself.
                verdict: CollationVerdict::Full,
            })
            .collect()
    }

    /// Set the per-database default collation (`db.SetDefaultCollation`, spec/design/collation.md
    /// §1). An un-annotated `text` column created afterward inherits it. The name must be `C` (resets
    /// to byte order) or a **loaded** collation (else 42704). Persisted as the `is_default` flag on
    /// that collation's reference entry at the next `commit` (the entry is emitted because the default
    /// references it — §5).
    pub fn set_default_collation(&mut self, name: &str) -> Result<()> {
        if name == "C" {
            self.committed.set_default_collation(None);
            return Ok(());
        }
        if self.committed.resolve_collation(name).is_none() {
            return Err(EngineError::new(
                SqlState::UndefinedObject,
                format!("collation \"{name}\" does not exist"),
            ));
        }
        self.committed.set_default_collation(Some(name.to_string()));
        Ok(())
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
        let mut work = self.committed.clone();
        let n = work.upgrade_collations(self.page_size)?;
        if n > 0 {
            self.committed = work;
        }
        Ok(n)
    }

    /// The per-database default collation name — `"C"` (byte order) unless `set_default_collation`
    /// moved it (`db.DefaultCollation`, spec/design/collation.md §1).
    pub fn default_collation(&self) -> String {
        self.committed
            .default_collation()
            .unwrap_or("C")
            .to_string()
    }

    /// Introspect the collations **this database references** (`db.Collations`,
    /// spec/design/collation.md §4.2) — every collation its schema uses (a column's `COLLATE`, or the
    /// per-database default), each as `(name, unicode_version, cldr_version, content_hash,
    /// description, is_default)`, in ascending name order. This is the *per-file* view; for the
    /// engine-global **loaded** set, use [`Engine::loaded_collations`]. `C` is built in and not
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
        match stmt {
            Statement::Begin { writable } => return self.begin_tx(writable),
            Statement::Commit => return self.commit_tx(),
            Statement::Rollback => return self.rollback_tx(),
            _ => {}
        }
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
            let result = if stmt_is_write(&stmt) && !writable {
                Err(EngineError::new(
                    SqlState::ReadOnlySqlTransaction,
                    format!(
                        "cannot execute {} in a read-only transaction",
                        stmt_kind(&stmt)
                    ),
                ))
            } else {
                self.dispatch_stmt(stmt, params)
            };
            // Enforce the temp-storage budget after a successful temp write (temp-tables.md §7): an
            // over-budget statement (session-local `temp_buffers`) becomes a `54P03` error, which
            // aborts the block (the staged temp rows roll back at ROLLBACK). A no-op for non-temp
            // statements.
            let result = result.and_then(|out| self.check_temp_budget().map(|()| out));
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
        if !stmt_is_write(&stmt) {
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
        self.session.tx = Some(ActiveTx {
            writable: true,
            failed: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            working: self.committed.clone(),
            saved_session_seq: self.session.session_seq.clone(),
            saved_session_last_name: self.session.session_last_name.clone(),
            temp_working: self.session.temp_committed.clone(),
            main_dirty: false,
            temp_dirty: false,
            attach_working: HashMap::new(),
            attach_dirty: HashSet::new(),
        });
        match self.dispatch_stmt(stmt, params) {
            Ok(outcome) => {
                // Enforce the temp-storage budget before committing (temp-tables.md §7): if this
                // (implicit) transaction's temp write pushed the session over `temp_buffers`, discard
                // the transaction (rolling back the over-budget temp + main changes) and surface 54P03.
                if let Err(e) = self.check_temp_budget() {
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
        self.session.tx = Some(ActiveTx {
            writable: writable.unwrap_or(!self.read_only),
            failed: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            working: self.committed.clone(),
            saved_session_seq: self.session.session_seq.clone(),
            saved_session_last_name: self.session.session_last_name.clone(),
            temp_working: self.session.temp_committed.clone(),
            main_dirty: false,
            temp_dirty: false,
            attach_working: HashMap::new(),
            attach_dirty: HashSet::new(),
        });
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
        let tx = match self.session.tx.take() {
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
        // Persist the main image when it changed; a transaction that touched ONLY session-local temp
        // tables skips it entirely so a temp table makes ZERO file writes (spec/design/temp-tables.md
        // §2). An empty block (no kind dirty) still persists, preserving prior behavior. Temp state is
        // adopted regardless — it is never serialized, only swapped into the in-memory committed temp
        // snapshot.
        let pure_temp = !main_dirty && temp_dirty;
        if !pure_temp {
            // The txid is the durable commit counter (spec/design/api.md §2): it advances only on a
            // file-backed commit. An in-memory commit swaps the snapshot but leaves txid unchanged
            // (an in-memory database stays at txid 0 — there is nothing to recover).
            if self.path.is_some() {
                working.txid = self.committed.txid + 1;
            }
            self.persist(&working)?; // no-op for an in-memory database
            self.committed = working;
        }
        // A dirty session-local temp domain materializes its working snapshot into its `MemoryBlockStore`
        // (compact packed leaves + within-session compaction) before it is adopted — zero main-file
        // writes (temp-tables.md §6). Compaction is safe iff no streaming cursor holds an older temp tree.
        if temp_dirty {
            let can_reclaim = self.open_streams.load(std::sync::atomic::Ordering::Relaxed) == 0;
            if let Some(ts) = self.temp_storage.as_mut() {
                ts.persist_temp(&mut temp_working, can_reclaim)?;
            }
        }
        // Adopt the transaction's temp changes into the committed temp snapshot (temp-tables.md §5) — the
        // temp analogue of publishing `committed`, but purely in memory. SessionState-local temp lives on
        // the session.
        self.session.temp_committed = temp_working;
        // Adopt each dirtied host-attached database (attached-databases.md §5, the N-root commit) and
        // adopt it into this engine's pinned attached view, so `publish` swaps a new `Roots::attached`. An
        // IN-MEMORY attachment packs persist_temp-style (the same incremental copy-on-write pack as temp,
        // NO fsync — no durability barrier); a FILE attachment (Slice 2) commits DURABLY (dirty pages +
        // alternating meta slot + fsync, its own page space) — [`Shared::commit_attachment`] branches on
        // the storage kind. The root is DATABASE-scoped (published, cross-session-visible). At most one
        // file attachment is dirty here (the one-durable-writer check above), so ≤1 fsync path runs.
        // Within-session compaction (in-memory only) is safe only when no cross-session reader pins an
        // older root (the live-registry watermark — the committing writer holds the gate but is not in
        // `live`).
        if !attach_dirty.is_empty() {
            let core = self.core.clone();
            let can_reclaim = core.as_ref().is_none_or(|c| !c.has_live_readers());
            let mut na = self.attached_committed.clone();
            for name in &attach_dirty {
                let Some(mut ws) = attach_working.remove(name) else {
                    continue;
                };
                if let Some(c) = &core {
                    // A detached-mid-transaction attachment (unreachable under the writer gate) no-ops.
                    let base_txid = self.attached_committed.get(name).map_or(0, |a| a.txid);
                    c.commit_attachment(name, &mut ws, base_txid, can_reclaim)?;
                }
                na.insert(name.clone(), std::sync::Arc::new(ws));
            }
            self.attached_committed = na;
        }
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
                    Some(q) => q.eq_ignore_ascii_case("temp"),
                    None => self.is_temp_table(&at.name),
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
            let key = name.to_ascii_lowercase();
            // Only a name that resolves to an existing catalog table is privilege-checked; a missing
            // one is left to raise 42P01 in execution (existence before authorization). A built-in
            // catalog relation (jed_tables / jed_columns) is gated exactly like a user table —
            // per-table SELECT on its own name under the session envelope, no special case
            // (introspection.md §5) — so an explicit-grant session sees the schema only if the host
            // granted it.
            let exists = is_catalog_rel_name(&key) || snap.table(&key).is_some();
            if exists && !self.session.privileges.allows_table(&key, *priv_) {
                return Err(EngineError::new(
                    SqlState::InsufficientPrivilege,
                    format!("permission denied for table {key}"),
                ));
            }
        }
        for name in &req.functions {
            let key = name.to_ascii_lowercase();
            if !self.session.privileges.allows_function(&key) {
                return Err(EngineError::new(
                    SqlState::InsufficientPrivilege,
                    format!("permission denied for function {key}"),
                ));
            }
        }
        Ok(())
    }

    /// Enforce the read-path admission gates a lazy read lane bypasses (the safe-total-`query`
    /// contract, CLAUDE.md §13). A SELECT served by a streaming (S3) / deferred (S7) cursor never
    /// reaches the materialized `execute_stmt_params`, where the failed-block / lifetime / privilege
    /// gates live — so a total `query` would leak restricted rows and run reads in a failed/exhausted
    /// session. `query_ast_cached` calls this before the lanes for a **read**: `25P02` (aborted block)
    /// / `54P02` (lifetime budget) / `42501` (privilege). Reads only — transaction control must still
    /// work in a failed block, and a write is gated inside `dispatch_stmt` on the materialized
    /// fall-through. The three checks are pure, so the fall-through re-running them is harmless.
    pub(crate) fn gate_read_lanes(&self, stmt: &Statement) -> Result<()> {
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
