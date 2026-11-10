//! Session and transaction state: the per-session options a host configures (`SessionOptions`),
//! the session's settings, privileges and budgets (`SessionState`, session.md), and the open
//! transaction with its savepoint stack (`ActiveTx`, transactions.md §11).

use super::*;

/// The relocatable session settings (spec/design/session.md §3 — the bucket-A envelope subset that
/// has landed in S1): the per-statement cost ceiling, the input-size limit, and the work-memory
/// budget. Passed to [`Engine::session`] to mint an additional session; an absent field takes its
/// default. (The random and clock sources belong to the handle's environment, not here —
/// spec/design/environment.md.)
#[derive(Clone, Debug)]
pub struct SessionOptions {
    /// Cumulative scalar allocation bytes per statement; zero restores the finite default.
    pub max_scalar_bytes: i64,
    /// Live logical query-memory budget per statement (spec/design/memory.md §2); `0` ⇒ unlimited
    /// (the default). Over-budget fails `54P05`.
    pub max_query_memory_bytes: i64,
    /// Execution-cost ceiling (CLAUDE.md §13); `0` ⇒ unlimited (the default).
    pub max_cost: i64,
    /// Per-session cumulative cost budget (spec/design/session.md §5.4); `0` ⇒ unlimited (the
    /// default). Bounds the whole session: the instant the session's running total reaches it, the
    /// in-flight statement aborts `54P02` (and once spent, every further statement is rejected at
    /// admission). Sibling to `max_cost`, which bounds one statement.
    pub lifetime_max_cost: i64,
    /// Maximum input SQL length in bytes; `0` ⇒ unlimited. Default [`DEFAULT_MAX_SQL_LENGTH`].
    pub max_sql_length: usize,
    /// Cross-process writer-gate deadline in milliseconds (`0` = wait without a deadline).
    pub lock_timeout_ms: u64,
    /// Work-memory budget in bytes; `0` ⇒ **the default** (256 MiB), same as unset (unlike its
    /// `max_cost`/`lifetime_max_cost` siblings, whose default genuinely is `0` ⇒ unlimited). An
    /// unbounded/never-spill budget is a runtime-only mode via [`set_work_mem`](SessionState::set_work_mem)`(0)`,
    /// never a bare-options value. Default [`DEFAULT_WORK_MEM`].
    pub work_mem: usize,
    /// The table-privilege set granted to **every** table — the `GRANT … ON ALL TABLES` default
    /// (spec/design/session.md §5.3). Default: all four (`SELECT`/`INSERT`/`UPDATE`/`DELETE`), so a
    /// fresh session is unrestricted; `{SELECT}` (via [`PrivilegeSet::EMPTY`]`.with(Select)`) is a
    /// read-only session. Per-object adjustments are [`SessionState::grant`] / [`SessionState::revoke`].
    pub default_privileges: PrivilegeSet,
    /// Whether **persistent** DDL (CREATE / DROP / ALTER of persistent tables, indexes, types,
    /// sequences) is permitted; a denied schema change is `42501` (session.md §5.3). Default **on**.
    /// Its scope narrows with temporary tables (temp-tables.md §5): it now governs *persistent* DDL
    /// specifically, with `allow_temp_ddl` the temp-scoped sibling. Name + default unchanged, so
    /// existing callers are unaffected.
    pub allow_ddl: bool,
    /// Whether SESSION-LOCAL **temporary**-table DDL (`CREATE`/`DROP` of a temp table) is permitted
    /// (spec/design/temp-tables.md §5); a denied temp DDL is `42501`. `None` ⇒ **inherit
    /// `allow_ddl`'s value** (back-compat: a session left as-is behaves as before, one gate governing
    /// all DDL). The untrusted-scratch pattern is `allow_ddl = false` + `allow_temp_ddl = Some(true)`
    /// — private scratch tables only, everything else denied, the §5.3 default-deny posture intact.
    pub allow_temp_ddl: Option<bool>,
    /// The per-session RESIDENT budget for session-local temp tables, in **bytes**
    /// (spec/design/temp-tables.md §6); `0` ⇒ never spill. Default [`DEFAULT_TEMP_BUFFERS`]. Past it a
    /// host with a spill directory moves the temp domain to a scratch file with a page cache this size.
    pub temp_buffers: usize,
    /// The per-session storage CEILING for session-local temp tables, in **bytes**
    /// (spec/design/temp-tables.md §7); `0` ⇒ unlimited. Default [`DEFAULT_MAX_TEMP_STORAGE_BYTES`].
    /// Bounds the RETAINED temp storage neither cost ceiling covers — an over-ceiling temp write aborts
    /// `54P03`, whether the domain is resident or spilled.
    pub max_temp_storage_bytes: usize,
    /// The session **time zone** (spec/design/session.md §6.2, timezones.md §9.4): the zone a
    /// `timestamptz` is decomposed *in* by `date_trunc` / `EXTRACT` / the cross-family datetime casts.
    /// Default `"UTC"`. Accepts `UTC`, a fixed `±HH:MM` offset, or a **named** IANA zone a loaded
    /// `JTZ` bundle provides; a name no bundle provides is rejected (`22023`) when the session is minted
    /// — the resolved zone is cached on the [`SessionState`]. (An invalid value here falls back to `UTC`
    /// rather than failing the mint; use [`SessionState::set_time_zone`] for the validated setter.)
    pub time_zone: String,
}

impl Default for SessionOptions {
    fn default() -> Self {
        SessionOptions {
            max_scalar_bytes: crate::costs::DEFAULT_SCALAR_BYTES,
            max_query_memory_bytes: 0,
            max_cost: 0,
            lifetime_max_cost: 0,
            max_sql_length: DEFAULT_MAX_SQL_LENGTH,
            lock_timeout_ms: 0,
            work_mem: crate::spill::DEFAULT_WORK_MEM,
            default_privileges: PrivilegeSet::ALL_TABLE,
            allow_ddl: true,
            allow_temp_ddl: None,
            temp_buffers: DEFAULT_TEMP_BUFFERS,
            max_temp_storage_bytes: DEFAULT_MAX_TEMP_STORAGE_BYTES,
            time_zone: "UTC".to_string(),
        }
    }
}

/// The session transaction status (spec/design/session.md §2.2) — PostgreSQL's three connection
/// states, made explicit on the session and derived from the open transaction: no transaction ⇒
/// `Idle` (autocommit); an open clean block ⇒ `Open`; an open block a statement aborted ⇒ `Failed`
/// (only ROLLBACK/COMMIT accepted, everything else `25P02`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxStatus {
    Idle,
    Open,
    Failed,
}

impl TxStatus {
    pub(crate) fn of(tx: &Option<ActiveTx>) -> TxStatus {
        match tx {
            None => TxStatus::Idle,
            Some(t) if t.is_failed() => TxStatus::Failed,
            Some(_) => TxStatus::Open,
        }
    }
}

/// A writable-CTE read pin (spec/design/writable-cte.md §2): the pre-statement snapshot of each
/// database domain a statement can read. A statement cannot attach or detach a database, so the
/// attachment set is fixed for the pin's lifetime.
pub(crate) struct ReadPin {
    pub(crate) main: Snapshot,
    pub(crate) temp: Snapshot,
    pub(crate) attached: HashMap<String, Snapshot>,
}

/// The per-connection **session** state (spec/design/session.md §2.1): the configured, stateful
/// context a host runs statements through, un-fused from the committed storage on [`Engine`]. It
/// owns the open transaction (the `Idle`/`Open`/`Failed` machine), the relocated handle settings,
/// the handle's environment, and the `currval`/`lastval` session state. A [`Engine`] holds one as
/// its long-lived default session; [`Engine::session`] mints additional independent ones that run
/// sequentially on a single-threaded handle (by swapping into the default slot for a call).
pub struct SessionState {
    pub(crate) max_scalar_bytes: i64,
    pub(crate) scalar_bytes: std::rc::Rc<std::cell::Cell<i64>>,
    /// The live query-memory budget (`<= 0` ⇒ unlimited) and the current statement's account,
    /// replaced at each statement start like `scalar_bytes` (spec/design/memory.md §2).
    pub(crate) max_query_memory_bytes: i64,
    pub(crate) query_bytes: std::rc::Rc<std::cell::Cell<i64>>,
    /// The pending-write balance the current statement's account opened with (memory.md §7).
    pub(crate) query_floor: std::rc::Rc<std::cell::Cell<i64>>,
    /// The open transaction, if any. `None` is autocommit between statements (transactions.md
    /// §4.1); a single-statement autocommit write opens one implicitly for its duration. The
    /// `Idle`/`Open`/`Failed` status (session.md §2.2) is derived from this ([`TxStatus::of`]).
    pub(crate) tx: Option<ActiveTx>,
    /// The execution-cost ceiling (CLAUDE.md §13; spec/design/api.md §8), or `0` for **unlimited**.
    /// Bounds every statement run on this session: its [`Meter`](crate::cost::Meter) is built with
    /// this limit and aborts `54P01` the instant accrued cost reaches it.
    pub(crate) max_cost: i64,
    /// The per-session cumulative cost budget (spec/design/session.md §5.4), or `0` for
    /// **unlimited**. Bounds the whole session: the instant [`lifetime_total`](SessionState::lifetime_total)
    /// reaches it the in-flight statement aborts `54P02`, and once spent every further statement is
    /// rejected `54P02` at admission. Sibling to [`max_cost`](SessionState::max_cost) (one statement).
    pub(crate) lifetime_max_cost: i64,
    /// The session's running **cumulative** execution cost (spec/design/session.md §5.4) — the gauge
    /// `lifetime_cost()` reads and the `54P02` budget bounds. Shared (`Rc<Cell>`) with every statement
    /// [`Meter`](crate::cost::Meter), which live-charges its units into it, so partial cost of an
    /// aborted statement counts automatically. **SessionState state, not snapshot state**: it does NOT roll
    /// back when a transaction rolls back (the compute was spent regardless).
    pub(crate) lifetime_total: std::rc::Rc<std::cell::Cell<i64>>,
    /// An optional cancellation poll armed for the duration of one statement (spec/design/api.md §11.4):
    /// the cancelable query/execute methods set it before running and restore it after, and
    /// [`new_meter`](SessionState::new_meter) copies it into the statement's [`Meter`](crate::cost::Meter),
    /// whose execution checkpoints consult it — so a flipped [`CancellationToken`](crate::CancellationToken) aborts a
    /// long-running statement with `57014` at the next execution checkpoint, not only at the cursor boundary.
    /// `None` (the default) skips atomic polling (CLAUDE.md §8). Because the
    /// token shares an `Arc<AtomicBool>`, another thread can flip the same token while this single-threaded
    /// handle runs the statement.
    pub(crate) cancel: Option<crate::cancel::CancellationToken>,
    /// The maximum input SQL length, in **bytes**, accepted on this session (CLAUDE.md §13; api.md
    /// §8, cost.md §7). `0` ⇒ **unlimited**; default [`DEFAULT_MAX_SQL_LENGTH`] (1 MiB). An
    /// over-limit statement is rejected `54000` at [`parse`](Engine::parse), before lexing.
    pub(crate) max_sql_length: usize,
    /// Cross-process writer-gate deadline in milliseconds (`0` = wait without a deadline).
    pub(crate) lock_timeout_ms: u64,
    /// The work-memory budget in **bytes** (spec/design/spill.md §2): the memory a blocking operator
    /// (the `ORDER BY` external merge sort) holds resident before it spills. `0` ⇒ unlimited (never
    /// spill); default [`DEFAULT_WORK_MEM`](crate::spill::DEFAULT_WORK_MEM). Never changes what a
    /// query observes (spill.md §6); an in-memory database ignores it.
    pub(crate) work_mem: usize,
    /// The handle's environment (spec/design/environment.md): the loaded Unicode and time-zone data,
    /// the host functions, and the random + clock sources the uuid generators / clock functions read.
    /// Every session minted from a handle (and a streaming cursor's frozen engine) shares the handle's
    /// one environment; it never changes while the handle is open.
    pub(crate) env: crate::environment::Environment,
    /// **SessionState** `currval` state (spec/design/sequences.md §6): the last value `nextval`/
    /// `setval(…,true)` produced **in this session** for each sequence (exact name). NOT in the
    /// snapshot and NOT persisted — strictly session-local, as in PostgreSQL.
    pub(crate) session_seq: HashMap<String, i64>,
    /// **SessionState** `lastval` state (sequences.md §6): the exact name of the sequence the most
    /// recent `nextval` (of any sequence) ran on — `None` before the first `nextval`.
    pub(crate) session_last_name: Option<String>,
    /// Per-**statement** running sequence advances (sequences.md §4), behind a `RefCell` for interior
    /// mutability (`EvalEnv` borrows `&Engine`). Flushed into the working snapshot on statement
    /// success; discarded on error (the transactional rollback of the advance, §5).
    pub(crate) pending_seq: std::cell::RefCell<HashMap<String, SequenceDef>>,
    /// Per-**statement** running `currval` updates → flushed into `session_seq` on success.
    pub(crate) pending_currval: std::cell::RefCell<HashMap<String, i64>>,
    /// Per-**statement** running `lastval` update → flushed into `session_last_name` on success.
    pub(crate) pending_last_name: std::cell::RefCell<Option<String>>,
    /// The authorization envelope (spec/design/session.md §5.3): the GRANT/REVOKE-style per-object
    /// privilege model the host configures and the engine enforces (`42501`) at name resolution. A
    /// fresh session is fully permissive (every table privilege, every function `EXECUTE`).
    pub(crate) privileges: crate::privileges::Privileges,
    /// Whether **persistent** DDL (CREATE / DROP / ALTER of persistent relations) is permitted on this
    /// session (session.md §5.3); a denied schema change is `42501`. Default **on**. Its scope narrows
    /// with temporary tables (temp-tables.md §5): `allow_temp_ddl` is the temp-scoped sibling gate.
    pub(crate) allow_ddl: bool,
    /// Whether session-local **temporary**-table DDL is permitted (spec/design/temp-tables.md §5); a
    /// denied temp DDL is `42501`. Resolved at session creation from
    /// [`SessionOptions::allow_temp_ddl`] (defaulting to `allow_ddl`'s value when unset).
    pub(crate) allow_temp_ddl: bool,
    /// The per-session temp-table RESIDENT budget in **bytes** (spec/design/temp-tables.md §6); `0` ⇒
    /// never spill.
    pub(crate) temp_buffers: usize,
    /// The per-session temp-table storage CEILING in **bytes** (spec/design/temp-tables.md §7); `0` ⇒
    /// unlimited. An over-ceiling temp write aborts `54P03`.
    pub(crate) max_temp_storage_bytes: usize,
    /// The session variables (spec/design/session.md §6.1): PostgreSQL's GUC model scoped to the
    /// session — a `string→string` map (PG GUCs are all text) the host sets (`set_var`/`reset_var`)
    /// and SQL reads with `current_setting`. Custom (dotted) names only in v1. **SessionState state, not
    /// snapshot state**: it does NOT roll back with a transaction (PG `SET SESSION`), and it carries
    /// across the additional-session swap because it lives on `SessionState` (like the privilege envelope).
    pub(crate) vars: HashMap<String, String>,
    /// The resolved session **time zone** (spec/design/session.md §6.2, timezones.md §9.4): the zone a
    /// `timestamptz` is decomposed *in* by `date_trunc` / `EXTRACT` / the cross-family casts. Resolved
    /// once (from [`SessionOptions::time_zone`] at mint, or [`SessionState::set_time_zone`]) to a cheap
    /// [`crate::timezone::ZoneRef`] (`UTC` = `Fixed(0)`); the evaluator reads it via the active session.
    /// **SessionState state** — carries across the additional-session swap, no storage effect.
    pub(crate) time_zone: crate::timezone::ZoneRef,
    /// The session-local **temporary-table** catalog + stores (spec/design/temp-tables.md §2): a
    /// `Snapshot` holding only this session's temp tables, their stores, and their (UNIQUE) index
    /// stores. **Never serialized** — only [`Engine::committed`] is written to the file, so a temp
    /// table makes ZERO file writes (§2). Private to this `SessionState` (so it carries across the
    /// additional-session swap and is invisible to other sessions — the [[session-design]] privacy),
    /// and dropped wholesale when the session is. Transactional like the main snapshot: an open
    /// transaction clones it into [`ActiveTx::temp_working`], which a successful COMMIT adopts back
    /// here and a ROLLBACK discards.
    pub(crate) temp_committed: Snapshot,
    /// The **read pin** for a data-modifying `WITH` statement (spec/design/writable-cte.md §2): the
    /// single pre-statement snapshot of every database domain (main, session-local temp, each host
    /// attachment) that every sub-statement reads, so the data-modifying CTEs and the primary cannot
    /// observe each other's table writes (their writes still accumulate into the transaction's
    /// working sets). Set by the writable-CTE orchestrator before the first sub-statement runs and
    /// cleared when it finishes (success or error); `None` for every other statement, where reads
    /// fall through to the working/committed snapshots as usual ([`Engine::read_snap`],
    /// [`Engine::temp_read_snap`], [`Engine::attach_read_snap`]).
    pub(crate) read_pin: Option<ReadPin>,
    /// Current recursive referential-action depth (constraints.md §6.6). A schema can assemble an
    /// arbitrarily long FK chain across cheap DDL statements, so generated child DML is capped at
    /// 256 nested calls to keep every core's native stack bounded (`54001`).
    pub(crate) fk_action_depth: usize,
    /// Foreign-key checks deferred until the outermost recursive action closure has reached its
    /// fixed point (§6.6) or, inside a data-modifying `WITH`, until the whole statement has run
    /// (writable-cte.md §7). Empty between statements.
    pub(crate) fk_deferred_checks: std::cell::RefCell<Vec<dml::FkDeferredCheck>>,
    /// Set while a data-modifying `WITH` runs its sub-statements: every foreign-key check is then
    /// queued in [`fk_deferred_checks`](Self::fk_deferred_checks) and settled against the end state
    /// of the whole statement, PostgreSQL's end-of-statement RI-trigger timing (writable-cte.md §7).
    pub(crate) fk_statement_defer: bool,
}

/// Validate + canonicalize a session-variable name (spec/design/session.md §6.1). A variable must be
/// **namespaced** like a PostgreSQL custom GUC — a dotted name (`myapp.tenant`); a non-dotted name
/// would be a built-in setting, and v1 exposes none through this map (the `time_zone` built-in is a
/// separate slice), so it is `42704`. Returns the case-folded (lowercase, PG GUC names are
/// case-insensitive) map key.
pub(crate) fn require_custom_var_name(name: &str) -> Result<String> {
    if name.contains('.') {
        Ok(name.to_ascii_lowercase())
    } else {
        Err(EngineError::new(
            SqlState::UndefinedObject,
            format!("unrecognized configuration parameter: {name}"),
        ))
    }
}

impl SessionState {
    /// Legacy unreported evaluation cost still shares the statement's scalar admission.
    pub(crate) fn scratch_meter(&self) -> Meter {
        let mut m = Meter::new();
        m.scalar_limit = self.max_scalar_bytes;
        m.scalar_bytes = self.scalar_bytes.clone();
        m.query = self.query_account();
        m
    }

    /// Set cumulative scalar allocation bytes per statement; non-positive restores the default.
    pub fn set_max_scalar_bytes(&mut self, bytes: i64) {
        self.max_scalar_bytes = bytes;
    }
    pub fn max_scalar_bytes(&self) -> i64 {
        if self.max_scalar_bytes > 0 {
            self.max_scalar_bytes
        } else {
            crate::costs::DEFAULT_SCALAR_BYTES
        }
    }

    /// Set the live query-memory budget per statement (spec/design/memory.md §2); non-positive
    /// restores the default, unlimited.
    pub fn set_max_query_memory_bytes(&mut self, bytes: i64) {
        self.max_query_memory_bytes = bytes;
    }
    /// The live query-memory budget, or `0` for unlimited.
    pub fn max_query_memory_bytes(&self) -> i64 {
        self.max_query_memory_bytes.max(0)
    }

    /// The current statement's query-memory account (spec/design/memory.md §2).
    pub(crate) fn query_account(&self) -> crate::cost::QueryAccount {
        crate::cost::QueryAccount {
            used: self.query_bytes.clone(),
            limit: self.max_query_memory_bytes,
            floor: self.query_floor.clone(),
        }
    }

    /// The cost-guard verdict of a finished statement that accrued `cost` (memory.md §2): a fresh
    /// session meter at that cost reports a reached `max_cost` or lifetime budget. Computed only when
    /// a query-memory budget is active — the one consumer, a collector's admission, is inert
    /// otherwise.
    pub(crate) fn finished_cost_guard(&self, cost: i64) -> Option<EngineError> {
        if self.max_query_memory_bytes <= 0 {
            return None;
        }
        let mut meter = self.new_meter();
        meter.accrued = cost;
        meter.guard().err()
    }

    /// Start a statement's scalar allowance and query-memory account afresh. Under a finite budget
    /// the account opens holding the transaction's pending writes, so the statement's own
    /// reservations must fit beside them (spec/design/memory.md §7).
    pub(crate) fn reset_statement_memory(&mut self) {
        self.scalar_bytes = std::rc::Rc::new(std::cell::Cell::new(0));
        let pending = if self.max_query_memory_bytes > 0 {
            self.pending_write_bytes()
        } else {
            0
        };
        self.query_bytes = std::rc::Rc::new(std::cell::Cell::new(pending));
        self.query_floor = std::rc::Rc::new(std::cell::Cell::new(pending));
    }

    /// Whether the session's writes keep the per-key record that makes pending writes net (memory.md
    /// §7.1): only under a finite query-memory budget, so the default path pays nothing.
    pub(crate) fn net_staging(&self) -> bool {
        self.max_query_memory_bytes > 0
    }

    /// The stored bytes the open transaction's working snapshots (main and attached databases) have
    /// staged — its pending writes (memory.md §7). Session-local temp tables are bounded by
    /// `temp_buffers` instead. Zero with no open transaction.
    pub(crate) fn pending_write_bytes(&self) -> i64 {
        self.tx.as_ref().map_or(0, |tx| {
            tx.attach_working
                .values()
                .fold(tx.working.staged_bytes(), |sum, ws| {
                    sum.saturating_add(ws.staged_bytes())
                })
        })
    }

    /// A fresh default session over `env`: no open transaction, default settings, empty sequence
    /// state.
    pub fn new(env: crate::environment::Environment) -> Self {
        SessionState::with_options(SessionOptions::default(), env)
    }

    /// A fresh session configured from `opts` (spec/design/session.md §2.1) over the handle's
    /// environment `env`; the rest of the per-connection state (transaction, sequence state) starts
    /// empty/default.
    pub fn with_options(opts: SessionOptions, env: crate::environment::Environment) -> Self {
        let mut privileges = crate::privileges::Privileges::default();
        privileges.set_default_table(opts.default_privileges);
        SessionState {
            tx: None,
            max_scalar_bytes: opts.max_scalar_bytes,
            scalar_bytes: std::rc::Rc::new(std::cell::Cell::new(0)),
            max_query_memory_bytes: opts.max_query_memory_bytes,
            query_bytes: std::rc::Rc::new(std::cell::Cell::new(0)),
            query_floor: std::rc::Rc::new(std::cell::Cell::new(0)),
            max_cost: opts.max_cost,
            lifetime_max_cost: opts.lifetime_max_cost,
            lifetime_total: std::rc::Rc::new(std::cell::Cell::new(0)),
            cancel: None,
            max_sql_length: opts.max_sql_length,
            lock_timeout_ms: opts.lock_timeout_ms,
            // `0` means "the default budget", not "unlimited" — the zero value stays a safe finite
            // budget (matching Go/TS). Unbounded/never-spill is reached via `set_work_mem(0)`.
            work_mem: if opts.work_mem == 0 {
                crate::spill::DEFAULT_WORK_MEM
            } else {
                opts.work_mem
            },
            session_seq: HashMap::new(),
            session_last_name: None,
            pending_seq: std::cell::RefCell::new(HashMap::new()),
            pending_currval: std::cell::RefCell::new(HashMap::new()),
            pending_last_name: std::cell::RefCell::new(None),
            privileges,
            allow_ddl: opts.allow_ddl,
            // Back-compat default-inheritance (temp-tables.md §5): an unset `allow_temp_ddl` takes
            // `allow_ddl`'s value, so a session configured before temp tables existed behaves exactly
            // as it did (one gate governing all DDL).
            allow_temp_ddl: opts.allow_temp_ddl.unwrap_or(opts.allow_ddl),
            temp_buffers: opts.temp_buffers,
            max_temp_storage_bytes: opts.max_temp_storage_bytes,
            temp_committed: Snapshot::with_environment(env.clone()),
            vars: HashMap::new(),
            // Resolve the configured zone once; an invalid value falls back to UTC at mint (the
            // validated path is `set_time_zone`, which surfaces 22023). timezones.md §9.4.
            time_zone: env
                .resolve_zone(&opts.time_zone)
                .unwrap_or(crate::timezone::ZoneRef::Fixed(0)),
            env,
            read_pin: None,
            fk_action_depth: 0,
            fk_deferred_checks: std::cell::RefCell::new(Vec::new()),
            fk_statement_defer: false,
        }
    }

    /// Set the session **time zone** (spec/design/session.md §6.2, timezones.md §9.4): the zone a
    /// `timestamptz` is decomposed *in*. Accepts `UTC`, a fixed `±HH:MM` offset, or a named IANA zone
    /// a loaded `JTZ` bundle provides; a name no bundle provides (and not a built-in) is **`22023`**
    /// (`invalid_parameter_value`), the value unchanged. The resolved zone is cached on the session.
    pub fn set_time_zone(&mut self, zone: &str) -> Result<()> {
        match self.env.resolve_zone(zone) {
            Some(zr) => {
                self.time_zone = zr;
                Ok(())
            }
            None => Err(EngineError::new(
                SqlState::InvalidParameterValue,
                format!("time zone \"{zone}\" not recognized"),
            )),
        }
    }

    /// The transaction status (`Idle`/`Open`/`Failed`, spec/design/session.md §2.2).
    pub fn status(&self) -> TxStatus {
        TxStatus::of(&self.tx)
    }

    /// Whether an explicit transaction block is open on this session.
    pub fn in_transaction(&self) -> bool {
        self.tx.is_some()
    }

    /// Set the execution-cost ceiling (§5.2); `<= 0` ⇒ unlimited.
    pub fn set_max_cost(&mut self, limit: i64) {
        self.max_cost = limit;
    }
    /// The current execution-cost ceiling.
    pub fn max_cost(&self) -> i64 {
        self.max_cost
    }
    /// Set the per-session cumulative cost budget (spec/design/session.md §5.4); `<= 0` ⇒ unlimited.
    /// Bounds the whole session: a statement aborts `54P02` the instant the session's cumulative cost
    /// reaches `limit`, and once spent every further statement is rejected `54P02` at admission.
    pub fn set_lifetime_max_cost(&mut self, limit: i64) {
        self.lifetime_max_cost = limit;
    }
    /// The current per-session cumulative cost budget (`0` ⇒ unlimited).
    pub fn lifetime_max_cost(&self) -> i64 {
        self.lifetime_max_cost
    }
    /// The session's running **cumulative** execution cost so far (spec/design/session.md §5.4) — the
    /// gauge the `lifetime_max_cost` budget bounds. Tracked even when the budget is unlimited; survives
    /// a transaction rollback (session state, not snapshot state).
    pub fn lifetime_cost(&self) -> i64 {
        self.lifetime_total.get()
    }
    /// Build the [`Meter`](crate::cost::Meter) for a statement run on this session: the per-statement
    /// `max_cost` ceiling (`54P01`) plus a handle to the session's cumulative total + budget (`54P02`).
    /// Every statement's meter is minted here, so all execution cost live-charges into the cumulative.
    pub(crate) fn new_meter(&self) -> Meter {
        let mut meter = Meter::for_session(
            self.max_cost,
            Lifetime {
                total: self.lifetime_total.clone(),
                limit: self.lifetime_max_cost,
            },
            self.cancel.clone(),
        );
        meter.scalar_limit = self.max_scalar_bytes;
        meter.scalar_bytes = self.scalar_bytes.clone();
        meter.query = self.query_account();
        meter
    }
    /// Set the maximum input SQL length in bytes; `0` ⇒ unlimited.
    pub fn set_max_sql_length(&mut self, bytes: usize) {
        self.max_sql_length = bytes;
    }
    /// The current input-SQL byte limit.
    pub fn max_sql_length(&self) -> usize {
        self.max_sql_length
    }

    pub fn set_lock_timeout_ms(&mut self, milliseconds: u64) {
        self.lock_timeout_ms = milliseconds;
    }

    pub fn lock_timeout_ms(&self) -> u64 {
        self.lock_timeout_ms
    }
    /// Set the work-memory budget in bytes; `0` ⇒ unlimited.
    pub fn set_work_mem(&mut self, bytes: usize) {
        self.work_mem = bytes;
    }
    /// The current work-memory budget.
    pub fn work_mem(&self) -> usize {
        self.work_mem
    }
    /// Set whether session-local temporary-table DDL is permitted (spec/design/temp-tables.md §5).
    pub fn set_allow_temp_ddl(&mut self, allow: bool) {
        self.allow_temp_ddl = allow;
    }
    /// Whether session-local temporary-table DDL is permitted on this session.
    pub fn allow_temp_ddl(&self) -> bool {
        self.allow_temp_ddl
    }
    /// Set the per-session temp-table resident budget in bytes; `0` ⇒ never spill.
    pub fn set_temp_buffers(&mut self, bytes: usize) {
        self.temp_buffers = bytes;
    }
    /// The current per-session temp-table resident budget.
    pub fn temp_buffers(&self) -> usize {
        self.temp_buffers
    }
    /// Set the per-session temp-table storage ceiling in bytes; `0` ⇒ unlimited.
    pub fn set_max_temp_storage_bytes(&mut self, bytes: usize) {
        self.max_temp_storage_bytes = bytes;
    }
    /// The current per-session temp-table storage ceiling.
    pub fn max_temp_storage_bytes(&self) -> usize {
        self.max_temp_storage_bytes
    }
    /// Replace the default table-privilege set — the `GRANT … ON ALL TABLES` default (§5.3). A
    /// read-only session is `PrivilegeSet::EMPTY.with(Privilege::Select)`.
    pub fn set_default_privileges(&mut self, privs: PrivilegeSet) {
        self.privileges.set_default_table(privs);
    }
    /// Grant `privs` on a specific object (table or function), beyond the default (§5.3).
    pub fn grant(&mut self, privs: PrivilegeSet, object: &str) {
        self.privileges.grant(privs, object);
    }
    /// Revoke `privs` from a specific object (revoke wins over grant and the default, §5.3).
    pub fn revoke(&mut self, privs: PrivilegeSet, object: &str) {
        self.privileges.revoke(privs, object);
    }
    /// Read-only access to the authorization envelope (§5.3).
    pub fn privileges(&self) -> &crate::privileges::Privileges {
        &self.privileges
    }
    /// Set whether DDL is permitted on this session (§5.3); a denied schema change is `42501`.
    pub fn set_allow_ddl(&mut self, allow: bool) {
        self.allow_ddl = allow;
    }
    /// Whether DDL is permitted on this session.
    pub fn allow_ddl(&self) -> bool {
        self.allow_ddl
    }
    /// Set a session variable (spec/design/session.md §6.1) — PostgreSQL's GUC model, scoped to the
    /// session. Custom variables must be **namespaced** (a dotted name like `myapp.tenant`); a
    /// non-dotted name is `42704` (no built-in setting is reachable through this map in v1 — the
    /// `time_zone` built-in is its own slice). The name is case-insensitive (folded to lowercase, PG);
    /// the value is text. SessionState state, not snapshot state — it does NOT roll back with a transaction.
    pub fn set_var(&mut self, name: &str, value: &str) -> Result<()> {
        let key = require_custom_var_name(name)?;
        self.vars.insert(key, value.to_string());
        Ok(())
    }
    /// Clear a session variable (§6.1). A non-dotted name is `42704` (as for `set_var`); an unset
    /// name is a no-op success (PG `RESET` of an unset custom variable).
    pub fn reset_var(&mut self, name: &str) -> Result<()> {
        let key = require_custom_var_name(name)?;
        self.vars.remove(&key);
        Ok(())
    }
    /// Read a session variable's value (§6.1), or `None` if it is not set. The host getter never
    /// errors — it is the SQL `current_setting` read that raises `42704` on an unset name.
    pub fn var(&self, name: &str) -> Option<String> {
        self.vars.get(&name.to_ascii_lowercase()).cloned()
    }
    /// Clear every session variable (§6.1) — PostgreSQL's `RESET ALL` for the variable map. Also the
    /// per-record reset hook the conformance harness's `# set:` directive uses (so a directive never
    /// leaks past its record).
    pub fn reset_vars(&mut self) {
        self.vars.clear();
    }
}

/// An open transaction (spec/design/transactions.md §4.2). `writable` is the access mode — READ
/// WRITE may write, READ ONLY is read-only (a write inside → 25006). `failed` marks an aborted
/// block (after a statement error every later statement but COMMIT/ROLLBACK is 25P02 — §6).
/// `working` is the transaction's snapshot: for a writable tx it is mutated in place and published
/// at commit; for a read-only tx it is the committed snapshot pinned at BEGIN (read-your-snapshot,
/// never mutated). Either way `committed` is untouched until commit, so ROLLBACK just drops this.
pub(crate) struct ActiveTx {
    pub(crate) writable: bool,
    /// The block's aborted flag (spec/design/transactions.md §6). An `Arc<AtomicBool>` rather than a
    /// plain `bool` so a **streaming/deferred read cursor born in this block can poison it from its
    /// drain** (a mid-drain trap aborts the block, PG-faithful): the cursor outlives the `&mut Engine`
    /// borrow, so it holds a clone of this flag and flips it on error — the same shared-`Arc` channel
    /// the open-stream guard uses (`Engine::attach_block_poison`). Cloning is scoped to this block, so
    /// a cursor that outlives its block only touches an orphaned flag (harmless).
    pub(crate) failed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub(crate) working: Snapshot,
    /// The handle's `currval`/`lastval` session state (spec/design/sequences.md §6) captured when
    /// this transaction opened. A `nextval`/`setval` inside the block updates the handle's session
    /// state per-statement (so an in-block `currval` sees its own advance), but those updates must
    /// **roll back** with the transaction (§5) — so ROLLBACK (and a failed/read-only COMMIT)
    /// restores these, while a successful COMMIT keeps the advanced state.
    pub(crate) saved_session_seq: HashMap<String, i64>,
    pub(crate) saved_session_last_name: Option<String>,
    /// The transaction's working copy of the session's temp-table snapshot
    /// (spec/design/temp-tables.md §5): cloned from [`SessionState::temp_committed`] at tx open (cheap —
    /// persistent stores clone O(1)), mutated by temp DDL/DML, adopted back into `temp_committed` on a
    /// successful COMMIT and discarded on ROLLBACK. The temp analogue of `working`, kept SEPARATE so
    /// it is never serialized.
    pub(crate) temp_working: Snapshot,
    /// Whether this transaction mutated the **main** (persistent) snapshot — set by
    /// [`Engine::working_mut`]. Drives the commit's persist decision so a transaction that touched
    /// ONLY temp tables makes zero file writes (temp-tables.md §2).
    pub(crate) main_dirty: bool,
    /// Whether this transaction mutated the **session-local temp** snapshot — set by
    /// [`Engine::temp_working_mut`]. With `main_dirty` it decides whether COMMIT persists the main
    /// image; candidate domains are refined against their base snapshots before persistence.
    pub(crate) temp_dirty: bool,
    /// The transaction's working copy of each host-attached database's snapshot
    /// (attached-databases.md §5), keyed by exact attachment name — the attachment analogue of
    /// `temp_working`. Cloned lazily from [`Engine::attached_committed`] on the first write to that
    /// attachment ([`Engine::attach_write_snap`]), so a read-only cross-attachment query allocates
    /// nothing here. Adopted into `attached_committed` + persisted+published on a successful COMMIT,
    /// discarded on ROLLBACK. Empty until an attachment is written.
    pub(crate) attach_working: HashMap<String, Snapshot>,
    /// Which attachments this transaction mutated (exact names) — the per-attachment analogue of
    /// `main_dirty`/`temp_dirty`, the set the commit persists + publishes.
    pub(crate) attach_dirty: HashSet<String>,
    /// The block's savepoints, oldest first (spec/design/transactions.md §11). `SAVEPOINT` pushes a
    /// frame, `RELEASE` truncates to below the newest frame of that name, `ROLLBACK TO` restores the
    /// newest frame of that name and truncates to just above it. Empty outside any savepoint.
    pub(crate) savepoints: Vec<SavepointFrame>,
}

/// One savepoint's marked state (spec/design/transactions.md §11): every piece of the open
/// transaction that a statement can change, captured at `SAVEPOINT`. The snapshots are persistent
/// structures, so the copy is O(1) and shares every unchanged node with `working`. Restoring the
/// snapshots restores the table/index roots, the catalog (DDL), sequences, statistics and estimator
/// revisions, temp tables, attachment writes, and the staged pending-write bytes (memory.md §7) at
/// once, because each of those is snapshot state.
pub(crate) struct SavepointFrame {
    pub(crate) name: String,
    pub(crate) working: Snapshot,
    pub(crate) temp_working: Snapshot,
    pub(crate) attach_working: HashMap<String, Snapshot>,
    pub(crate) main_dirty: bool,
    pub(crate) temp_dirty: bool,
    pub(crate) attach_dirty: HashSet<String>,
    pub(crate) session_seq: HashMap<String, i64>,
    pub(crate) session_last_name: Option<String>,
}

impl ActiveTx {
    /// A fresh transaction over the given base snapshots (no savepoints, nothing dirty).
    pub(crate) fn open(
        writable: bool,
        working: Snapshot,
        temp_working: Snapshot,
        session: &SessionState,
    ) -> ActiveTx {
        ActiveTx {
            writable,
            failed: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            working,
            saved_session_seq: session.session_seq.clone(),
            saved_session_last_name: session.session_last_name.clone(),
            temp_working,
            main_dirty: false,
            temp_dirty: false,
            attach_working: HashMap::new(),
            attach_dirty: HashSet::new(),
            savepoints: Vec::new(),
        }
    }

    /// Forget the per-key staged record of every pending domain (main and attachments), keeping the
    /// balance: a savepoint frame shares the staged versions (memory.md §7.2).
    pub(crate) fn forget_staged_keys(&mut self) {
        self.working.forget_staged_keys();
        for snap in self.attach_working.values_mut() {
            snap.forget_staged_keys();
        }
    }

    /// The index of the newest savepoint named exactly `name` (names shadow — PostgreSQL).
    pub(crate) fn find_savepoint(&self, name: &str) -> Option<usize> {
        self.savepoints.iter().rposition(|f| f.name == name)
    }
}

impl ActiveTx {
    /// Whether the block is aborted (spec/design/transactions.md §6) — reads the shared `failed` flag.
    pub(crate) fn is_failed(&self) -> bool {
        self.failed.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Abort the block (set the shared `failed` flag). Takes `&self` because the flag is an
    /// `Arc<AtomicBool>` — so a poison from a lane cursor's drain (which only holds `&self`-equivalent
    /// access via a clone) reaches the same store.
    pub(crate) fn mark_failed(&self) {
        self.failed
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}
