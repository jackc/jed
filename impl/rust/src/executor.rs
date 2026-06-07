//! Statement executor (CLAUDE.md §10).
//!
//! SCAFFOLD (step-5 Phase A): dispatches a parsed statement; each arm is filled in
//! feature-by-feature (Phases B–E). The result of running a statement is an
//! `Outcome`: either a bare success (DDL/DML) or a query result set.

use crate::ast::{
    BinaryOp, CreateTable, Delete, DropTable, Expr, Insert, InsertSource, InsertValue, JoinKind,
    Literal, OrderKey, QueryExpr, Select, SelectItems, SetOp, SetOpKind, Statement, TypeMod,
    UnaryOp, Update,
};
use crate::catalog::{Column, Table};
use crate::cost::Meter;
use crate::costs::COSTS;
use crate::decimal::{Decimal, MAX_PRECISION, MAX_SCALE};
use crate::encoding::encode_int;
use crate::error::{EngineError, Result, SqlState};
use crate::storage::{Row, TableStore};
use crate::timestamp::{parse_timestamp, parse_timestamptz};
use crate::types::{DecimalTypmod, ScalarType};
use crate::value::{ThreeValued, Value, and3, from3, not3, or3, parse_bytea_hex, parse_uuid};
use std::collections::{HashMap, HashSet};

/// The outcome of executing one statement. Both variants carry the deterministic
/// execution `cost` accrued while running the statement (CLAUDE.md §13) — a DML
/// statement accrues its scan + filter cost even though it returns no rows.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// A statement that produces no result set (CREATE, INSERT, UPDATE, DELETE).
    Statement { cost: i64 },
    /// A query result: output column names plus rows in result order. The column count
    /// is `column_names.len()` (spec/design/grammar.md §8).
    Query {
        column_names: Vec<String>,
        rows: Vec<Vec<Value>>,
        cost: i64,
    },
}

impl Outcome {
    /// The accrued execution cost (CLAUDE.md §13), available on either variant.
    pub fn cost(&self) -> i64 {
        match self {
            Outcome::Statement { cost } => *cost,
            Outcome::Query { cost, .. } => *cost,
        }
    }

    /// The output column names of a query result (empty for a non-query statement).
    pub fn column_names(&self) -> &[String] {
        match self {
            Outcome::Query { column_names, .. } => column_names,
            Outcome::Statement { .. } => &[],
        }
    }
}

/// The full result of running a SELECT (`run_select`): the output column names and their
/// resolved types, the rows in result order, and the accrued cost. Internal to the executor —
/// `execute_select` drops the types into the public `Outcome::Query`, while `INSERT ... SELECT`
/// uses the types to gate assignability up front (spec/design/grammar.md §24).
struct SelectResult {
    column_names: Vec<String>,
    column_types: Vec<ResolvedType>,
    rows: Vec<Vec<Value>>,
    cost: i64,
}

/// The default serialization page size (8 KiB — spec/design/storage.md §3), used for a fresh
/// in-memory or newly-created database when no explicit size is given.
pub const DEFAULT_PAGE_SIZE: u32 = 8192;

/// An immutable committed (or in-progress working) database state — the catalog + each table's
/// store + the commit counter (spec/design/transactions.md §2). The committed state is one of
/// these; a write transaction builds a new one from it (path-copying the persistent stores, so the
/// prior state is provably unchanged — pmap.rs / §3). A reader holds a `Snapshot` and is thereby
/// stable for its life: a later commit produces a *new* `Snapshot` and never mutates this one.
/// (P5.3a is single-handle; sharing a `Snapshot` across threads is P5.3b.)
#[derive(Clone, Default)]
pub struct Snapshot {
    /// The snapshot's version — the commit counter (transactions.md §8; the watermark unit).
    pub(crate) txid: u64,
    tables: HashMap<String, Table>,
    stores: HashMap<String, TableStore>,
}

impl Snapshot {
    /// Look up a table definition by name (case-insensitive).
    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables.get(&name.to_ascii_lowercase())
    }

    /// The store for a table (panics if absent — callers resolve the table first).
    fn store(&self, name: &str) -> &TableStore {
        self.stores
            .get(&name.to_ascii_lowercase())
            .expect("store exists for a resolved table")
    }

    /// The store for a table, mutable (panics if absent).
    pub(crate) fn store_mut(&mut self, name: &str) -> &mut TableStore {
        self.stores
            .get_mut(&name.to_ascii_lowercase())
            .expect("store exists for a resolved table")
    }

    /// All rows of a table in primary-key (encoded byte) order, or None if the table is absent.
    fn rows_in_key_order(&self, name: &str) -> Option<Vec<Row>> {
        self.stores
            .get(&name.to_ascii_lowercase())
            .map(|s| s.iter_in_key_order().cloned().collect())
    }

    /// Register a new table and its (empty) store. Lower-cased name is the key. The store carries
    /// the page payload `cap` (= `page_size − 12`) and the column types so the page-backed B-tree
    /// can weigh records for its size-driven split (spec/fileformat/format.md).
    pub(crate) fn put_table(&mut self, table: Table, page_size: u32) {
        let key = table.name.to_ascii_lowercase();
        let cap = page_size as usize - 12; // PAGE_HEADER
        let col_types: Vec<ScalarType> = table.columns.iter().map(|c| c.ty).collect();
        self.stores
            .insert(key.clone(), TableStore::new(cap, col_types));
        self.tables.insert(key, table);
    }

    /// Remove a table's definition and its store (DROP TABLE).
    fn remove_table(&mut self, key: &str) {
        self.tables.remove(key);
        self.stores.remove(key);
    }

    /// Every table with its store, as `(lowercased key, table, store)` tuples, for the on-disk
    /// serializer (spec/fileformat/format.md). The serializer sorts by the lowercased key so
    /// hash-map iteration order never leaks (CLAUDE.md §8).
    pub(crate) fn catalog_and_stores(&self) -> Vec<(&str, &Table, &TableStore)> {
        self.tables
            .iter()
            .map(|(k, t)| (k.as_str(), t, self.stores.get(k).expect("store exists")))
            .collect()
    }
}

/// The database handle: the last **committed** `Snapshot` plus, while a transaction is open, the
/// writer's working snapshot (CLAUDE.md §3, spec/design/transactions.md §2). Reads run against the
/// *visible* snapshot — the open transaction's `working` if any, else `committed`; a write mutates
/// `working` and commit swaps `committed := working` (rollback just drops `working`, since
/// `committed` was never touched). Every write — autocommit included — runs as a transaction, which
/// unifies the two paths.
pub struct Database {
    /// The last committed, immutable state — what fresh readers (and autocommit reads) see.
    pub(crate) committed: Snapshot,
    /// The open transaction, if any. `None` is autocommit between statements (transactions.md
    /// §4.1); a single-statement autocommit write opens one implicitly for its duration.
    tx: Option<ActiveTx>,
    /// The backing file path (`None` for an in-memory database). Set by the host API
    /// `open`/`create` (spec/design/api.md §2); `commit` writes here.
    pub(crate) path: Option<std::path::PathBuf>,
    /// The page size this database serializes with (from the file on open, from create opts,
    /// else `DEFAULT_PAGE_SIZE`). Fixed for the life of a file.
    pub(crate) page_size: u32,
    /// The on-disk page high-water mark — the page index an incremental commit extends at when the
    /// free-list is exhausted (spec/fileformat/format.md). Set from the file's meta on `open`, from
    /// the initial image on `create`; `0` (unused) for an in-memory database.
    pub(crate) page_count: u32,
    /// The free-list (P6.2): page indices a prior root abandoned, reusable by the next incremental
    /// commit (spec/fileformat/format.md *Reclamation*). **Reconstructed on open** as `[2,
    /// page_count)` minus the committed root's reachable pages; drawn from lowest-first before the
    /// file is extended. A page leaves the list only by being allocated into a new committed
    /// version, so it is reachable from no live snapshot and reuse is torn-write-safe. Empty for an
    /// in-memory database and for a freshly-created file (a from-scratch image leaks nothing).
    pub(crate) free_pages: Vec<u32>,
}

/// An open transaction (spec/design/transactions.md §4.2). `writable` is the access mode — READ
/// WRITE may write, READ ONLY is read-only (a write inside → 25006). `failed` marks an aborted
/// block (after a statement error every later statement but COMMIT/ROLLBACK is 25P02 — §6).
/// `working` is the transaction's snapshot: for a writable tx it is mutated in place and published
/// at commit; for a read-only tx it is the committed snapshot pinned at BEGIN (read-your-snapshot,
/// never mutated). Either way `committed` is untouched until commit, so ROLLBACK just drops this.
struct ActiveTx {
    writable: bool,
    failed: bool,
    working: Snapshot,
}

impl Default for Database {
    fn default() -> Self {
        Self::new()
    }
}

impl Database {
    pub fn new() -> Self {
        Database::with_page_size(DEFAULT_PAGE_SIZE)
    }

    /// An in-memory handle that serializes at `page_size`. The page-backed B-tree's fan-out tracks
    /// the page size (spec/fileformat/format.md), so the in-memory tree must be built at the size it
    /// will serialize to — this builds fixtures / tests a non-default page size; a normal in-memory
    /// database uses [`Database::new`] (the default page size).
    pub fn with_page_size(page_size: u32) -> Self {
        Database {
            committed: Snapshot::default(),
            tx: None,
            path: None,
            page_size,
            page_count: 0,
            free_pages: Vec::new(),
        }
    }

    /// Build an in-memory handle whose committed state **is** `snap` (no file backing). The
    /// thread-safe shared layer ([`crate::shared`]) uses this to run the unchanged executor against
    /// a snapshot it has pinned from the shared committed cell: a read handle keeps one of these
    /// with no open transaction (reads hit `committed` = the pinned snapshot); a write handle keeps
    /// one with an open READ WRITE block and publishes its working set back to the shared cell.
    pub(crate) fn from_snapshot(snap: Snapshot) -> Self {
        Database {
            committed: snap,
            tx: None,
            path: None,
            page_size: DEFAULT_PAGE_SIZE,
            page_count: 0,
            free_pages: Vec::new(),
        }
    }

    /// The snapshot a read sees: the open transaction's `working` (read-your-writes for a
    /// writable tx; the pinned snapshot for a read-only tx), else the committed snapshot.
    fn read_snap(&self) -> &Snapshot {
        match &self.tx {
            Some(tx) => &tx.working,
            None => &self.committed,
        }
    }

    /// The working snapshot a write mutates — the open transaction's `working`. A write only ever
    /// runs with a transaction open (autocommit opens one implicitly), so this never panics in a
    /// correct flow.
    fn working_mut(&mut self) -> &mut Snapshot {
        &mut self
            .tx
            .as_mut()
            .expect("a write statement runs within a transaction")
            .working
    }

    /// The committed snapshot, immutable (spec/design/transactions.md §2). Exposed for the host
    /// `Transaction`/read surfaces and for the on-disk serializer.
    pub(crate) fn committed(&self) -> &Snapshot {
        &self.committed
    }

    /// The oldest still-live snapshot's txid (spec/design/transactions.md §8) — the Phase-6
    /// free-list reclamation gate. Single-handle (P5.3a) it is trivially the committed txid (no
    /// other reader pins an older one yet); P5.3b's shared read snapshots make it meaningful.
    pub fn oldest_live_txid(&self) -> u64 {
        self.committed.txid
    }

    /// Whether an explicit transaction block is currently open (spec/design/transactions.md
    /// §4.2). False under autocommit. Used by the host `Transaction` surface (api.md §6).
    pub fn in_transaction(&self) -> bool {
        self.tx.is_some()
    }

    /// Whether the open transaction has been aborted (a statement errored → it is in the failed
    /// state, §6). False under autocommit or for a clean block. The shared write handle
    /// ([`crate::shared`]) reads this at commit to know whether to publish (a failed block
    /// publishes nothing — a failed COMMIT is a ROLLBACK, PostgreSQL).
    pub(crate) fn tx_failed(&self) -> bool {
        self.tx.as_ref().is_some_and(|t| t.failed)
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

    /// The backing file path, or `None` for an in-memory database.
    pub fn path(&self) -> Option<&std::path::Path> {
        self.path.as_deref()
    }

    /// Look up a table definition by name (case-insensitive) in the currently-visible snapshot
    /// (the open transaction's working set, else the committed state).
    pub fn table(&self, name: &str) -> Option<&Table> {
        self.read_snap().table(name)
    }

    /// All rows of a table in primary-key (encoded byte) order, or None if the table does not
    /// exist. Reads the visible snapshot. Used by SELECT and by tests.
    pub fn rows_in_key_order(&self, name: &str) -> Option<Vec<Row>> {
        self.read_snap().rows_in_key_order(name)
    }

    /// Register a new table and its (empty) store in the working snapshot (DDL is transactional —
    /// transactions.md §4.5).
    pub(crate) fn put_table(&mut self, table: Table) {
        let ps = self.page_size;
        self.working_mut().put_table(table, ps);
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

        // Inside an explicit block? Read the flags, dropping the borrow before dispatch.
        if self.tx.is_some() {
            let (failed, writable) = {
                let tx = self.tx.as_ref().expect("tx is open");
                (tx.failed, tx.writable)
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
            if result.is_err() {
                self.tx.as_mut().expect("tx is open").failed = true;
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
        self.tx = Some(ActiveTx {
            writable: true,
            failed: false,
            working: self.committed.clone(),
        });
        match self.dispatch_stmt(stmt, params) {
            Ok(outcome) => self.commit_tx().map(|_| outcome),
            Err(e) => {
                self.tx = None;
                Err(e)
            }
        }
    }

    /// Open an explicit transaction block (spec/design/transactions.md §4.2). A nested `BEGIN` (a
    /// block is already open) is 25001. The committed snapshot is captured as the transaction's
    /// working snapshot — a writable tx mutates it in place; a read-only tx reads it unchanged
    /// (read-your-snapshot, §4.3). Cheap: the persistent stores clone O(1) (pmap.rs) and the
    /// catalog is a shallow copy. `committed` is untouched until commit.
    pub(crate) fn begin_tx(&mut self, writable: bool) -> Result<Outcome> {
        if self.tx.is_some() {
            return Err(EngineError::new(
                SqlState::ActiveSqlTransaction,
                "there is already a transaction in progress",
            ));
        }
        self.tx = Some(ActiveTx {
            writable,
            failed: false,
            working: self.committed.clone(),
        });
        Ok(Outcome::Statement { cost: 0 })
    }

    /// Commit the current transaction (spec/design/transactions.md §4.2). With no open block it is
    /// a lenient no-op success. A **failed** block, or any read-only tx, publishes nothing — the
    /// working snapshot is simply dropped (a failed COMMIT is thus a ROLLBACK, PostgreSQL). A READ
    /// WRITE block publishes its working snapshot: bump its txid, make it durable (the single
    /// `persist` chokepoint, §9), then swap it in as the new `committed` — a single pointer swap,
    /// the §3 short commit window. A durable-write failure leaves `committed` untouched and
    /// propagates (the commit failed; the working set is discarded). Returns to autocommit.
    pub(crate) fn commit_tx(&mut self) -> Result<Outcome> {
        let tx = match self.tx.take() {
            None => return Ok(Outcome::Statement { cost: 0 }),
            Some(tx) => tx,
        };
        if tx.failed || !tx.writable {
            return Ok(Outcome::Statement { cost: 0 });
        }
        let mut working = tx.working;
        // The txid is the durable commit counter (spec/design/api.md §2): it advances only on a
        // file-backed commit. An in-memory commit swaps the snapshot but leaves txid unchanged
        // (an in-memory database stays at txid 0 — there is nothing to recover).
        if self.path.is_some() {
            working.txid = self.committed.txid + 1;
        }
        self.persist(&working)?; // no-op for an in-memory database
        self.committed = working;
        Ok(Outcome::Statement { cost: 0 })
    }

    /// Roll back the current transaction (spec/design/transactions.md §4.2). With no open block it
    /// is a no-op success. Otherwise the working snapshot is **dropped** — every staged
    /// INSERT/UPDATE/DELETE and DDL CREATE/DROP, plus any rowid allocations (§7), vanish with it;
    /// `committed` was never mutated, so there is nothing to restore. Returns to autocommit.
    pub(crate) fn rollback_tx(&mut self) -> Result<Outcome> {
        self.tx = None;
        Ok(Outcome::Statement { cost: 0 })
    }

    /// Dispatch one parsed statement to its executor. The autocommit transaction handling
    /// (capture / durable commit / rollback-on-error) lives in `execute_stmt_params`.
    fn dispatch_stmt(&mut self, stmt: Statement, params: &[Value]) -> Result<Outcome> {
        match stmt {
            Statement::CreateTable(ct) => {
                reject_params_for_ddl(params)?;
                self.execute_create_table(ct)
            }
            Statement::DropTable(dt) => {
                reject_params_for_ddl(params)?;
                self.execute_drop_table(dt)
            }
            Statement::Insert(ins) => self.execute_insert(ins, params),
            Statement::Select(sel) => self.execute_select(sel, params),
            Statement::SetOp(so) => self.execute_set_op(so, params),
            Statement::Update(upd) => self.execute_update(upd, params),
            Statement::Delete(del) => self.execute_delete(del, params),
            // Transaction control is handled by `execute_stmt_params` before dispatch.
            Statement::Begin { .. } | Statement::Commit | Statement::Rollback => {
                unreachable!("transaction control is handled before dispatch")
            }
        }
    }

    /// Analyze and run a CREATE TABLE: resolve each column's type name, enforce a
    /// single primary key (which is implicitly NOT NULL), reject duplicate table
    /// and column names, then register the table.
    fn execute_create_table(&mut self, ct: CreateTable) -> Result<Outcome> {
        if self.table(&ct.name).is_some() {
            return Err(EngineError::new(
                SqlState::DuplicateTable,
                format!("table already exists: {}", ct.name),
            ));
        }

        let mut columns = Vec::with_capacity(ct.columns.len());
        let mut pk_seen = false;
        for def in &ct.columns {
            if columns
                .iter()
                .any(|c: &Column| c.name.eq_ignore_ascii_case(&def.name))
            {
                return Err(EngineError::new(
                    SqlState::DuplicateColumn,
                    format!("duplicate column name: {}", def.name),
                ));
            }
            let (ty, decimal) = resolve_type_and_typmod(&def.type_name, &def.type_mod)?;
            if def.primary_key {
                // Integers and uuid may be a key. uuid is the FIRST non-integer key type — its
                // fixed `uuid-raw16` encoding (spec/design/encoding.md §2.7) is exercised. The
                // other non-integer types' order-preserving key encodings (text §2.4, decimal
                // §2.5, bytea §2.6, boolean's bool-byte) are authored but unexercised, so a
                // text/decimal/bytea/boolean PRIMARY KEY is a documented 0A000 narrowing
                // (spec/design/types.md §9/§11/§12/§13), relaxable in a later in-key slice.
                // timestamp / timestamptz are also allowed — they share the int64 `int-be-signflip`
                // key encoding (exercised + byte-pinned, spec/design/timestamp.md §6).
                if !ty.is_integer() && !ty.is_uuid() && !ty.is_timestamp() && !ty.is_timestamptz() {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        format!("a {} primary key is not supported yet", ty.canonical_name()),
                    ));
                }
                if pk_seen {
                    return Err(EngineError::new(
                        SqlState::InvalidTableDefinition,
                        "a table may have at most one primary key",
                    ));
                }
                pk_seen = true;
            }
            // Evaluate + type-coerce the DEFAULT literal once, here. A bad default fails at
            // CREATE TABLE: out of range 22003, cross-family 42804, decimal over-precision
            // 22003. NOT NULL is NOT enforced here (not_null=false), so a `DEFAULT NULL` on a
            // NOT NULL column is accepted and traps 23502 only when applied (constraints.md §2).
            let default = match &def.default {
                Some(lit) => Some(store_value(
                    literal_to_value(lit),
                    ty,
                    decimal,
                    false,
                    &def.name,
                )?),
                None => None,
            };
            columns.push(Column {
                name: def.name.clone(),
                ty,
                decimal,
                primary_key: def.primary_key,
                not_null: def.primary_key || def.not_null, // PRIMARY KEY ⇒ NOT NULL
                default,
            });
        }

        self.put_table(Table {
            name: ct.name,
            columns,
        });
        // DDL touches no rows and evaluates no expressions: zero cost.
        Ok(Outcome::Statement { cost: 0 })
    }

    /// Run a DROP TABLE: remove the table's definition and its row store from the
    /// catalog (both keyed by the lower-cased name). A table that does not exist is the
    /// same 42P01 the DML paths raise — there is no `IF EXISTS` this slice
    /// (spec/design/grammar.md §13). Like CREATE TABLE it touches no rows and evaluates
    /// no expression tree (the store is discarded wholesale), so it accrues zero cost.
    fn execute_drop_table(&mut self, dt: DropTable) -> Result<Outcome> {
        if self.table(&dt.name).is_none() {
            return Err(EngineError::new(
                SqlState::UndefinedTable,
                format!("table does not exist: {}", dt.name),
            ));
        }
        let key = dt.name.to_ascii_lowercase();
        self.working_mut().remove_table(&key);
        Ok(Outcome::Statement { cost: 0 })
    }

    /// Analyze and run an INSERT whose rows come from a `VALUES` list or a `SELECT`
    /// (spec/design/grammar.md §12 / §24). An optional column list names the target columns
    /// (unknown → 42703, duplicate → 42701); an unlisted column, or a `DEFAULT` keyword slot,
    /// takes the column's stored default, else NULL. Each value is type-checked (NULL into NOT
    /// NULL traps 23502; an integer outside the column type's range traps 22003 — CLAUDE.md §8);
    /// a duplicate primary key traps 23505. An INSERT is **two-phase / all-or-nothing**, mirroring
    /// UPDATE: every row is validated — including its storage key — before any row is inserted,
    /// so a mid-batch failure stores nothing. The two sources differ only in where the candidate
    /// rows come from and in cost: `VALUES` is zero (literals + constant defaults), `SELECT` is
    /// the embedded query's accrued cost. The `SELECT` source additionally validates output
    /// arity (42601) and per-column type assignability (42804) **up front**, before any row is
    /// produced — so both fire even over an empty source.
    fn execute_insert(&mut self, ins: Insert, params: &[Value]) -> Result<Outcome> {
        let Insert {
            table,
            columns: col_list,
            source,
        } = ins;

        let tdef = self.table(&table).ok_or_else(|| {
            EngineError::new(
                SqlState::UndefinedTable,
                format!("table does not exist: {table}"),
            )
        })?;

        // Snapshot the catalog data each row is validated against, ending the `tdef`
        // borrow so phase 1 can read the store (dup-key check) and phase 2 can mutate it.
        let table_name = tdef.name.clone();
        let columns: Vec<Column> = tdef.columns.clone();
        let pk = tdef.primary_key_index().map(|i| (i, tdef.columns[i].ty));

        // Resolve the optional column list once. `provided[i] = Some(p)` means table column i
        // takes value position `p` in each row; `None` means column i is omitted (its default,
        // else NULL). With no list it is the identity over all columns. `arity` is how many
        // values each row must carry (for a SELECT source, how many columns it must project).
        let n = columns.len();
        let has_list = col_list.is_some();
        let (provided, arity): (Vec<Option<usize>>, usize) = match &col_list {
            Some(names) => {
                let mut provided = vec![None; n];
                for (p, name) in names.iter().enumerate() {
                    let idx = columns
                        .iter()
                        .position(|c| c.name.eq_ignore_ascii_case(name))
                        .ok_or_else(|| {
                            EngineError::new(
                                SqlState::UndefinedColumn,
                                format!("column {name} of relation {table_name} does not exist"),
                            )
                        })?;
                    if provided[idx].is_some() {
                        return Err(EngineError::new(
                            SqlState::DuplicateColumn,
                            format!("column {} specified more than once", columns[idx].name),
                        ));
                    }
                    provided[idx] = Some(p);
                }
                (provided, names.len())
            }
            None => ((0..n).map(Some).collect(), n),
        };

        match source {
            InsertSource::Values(rows_in) => {
                // A `$N` in a VALUES slot is typed as its TARGET COLUMN's type. Collect those
                // types across every row (a `$N` reused under two columns unifies; api.md §5),
                // then bind the supplied values up front so a bad bind fails before any store.
                let mut ptypes = ParamTypes::default();
                for values in &rows_in {
                    for (i, col) in columns.iter().enumerate() {
                        if let Some(p) = provided[i] {
                            if let Some(InsertValue::Param(nn)) = values.get(p) {
                                ptypes.note((*nn as usize) - 1, Some(col.ty))?;
                            }
                        }
                    }
                }
                let bound = bind_params(params, &ptypes.finalize()?)?;

                // Materialize each row into its value-position-indexed candidates (length
                // `arity`), checking arity (42601) and resolving each slot: a literal, a bound
                // `$N`, or a `DEFAULT` keyword → that column's default else NULL. The shared
                // `insert_rows` then builds the declaration-order row and validates it.
                let mut rows: Vec<Vec<Value>> = Vec::with_capacity(rows_in.len());
                for values in &rows_in {
                    if values.len() != arity {
                        return Err(EngineError::new(
                            SqlState::SyntaxError,
                            format!(
                                "INSERT row has {} values but {} {} expected for table {}",
                                values.len(),
                                arity,
                                if has_list {
                                    "target columns are"
                                } else {
                                    "columns are"
                                },
                                table_name,
                            ),
                        ));
                    }
                    let mut rv = vec![Value::Null; arity];
                    for (i, col) in columns.iter().enumerate() {
                        if let Some(p) = provided[i] {
                            rv[p] = match &values[p] {
                                InsertValue::Lit(lit) => literal_to_value(lit),
                                InsertValue::Param(nn) => bound[(*nn as usize) - 1].clone(),
                                InsertValue::Default => col.default.clone().unwrap_or(Value::Null),
                            };
                        }
                    }
                    rows.push(rv);
                }
                self.insert_rows(&table, &columns, pk, &provided, rows)?;
                // INSERT ... VALUES reads no rows and evaluates no expression tree — its values
                // are literals and pre-evaluated constant defaults (leaves): zero cost.
                Ok(Outcome::Statement { cost: 0 })
            }
            InsertSource::Select(sel) => {
                // Run the source query first; it returns OWNED rows, so the `&mut self` borrow
                // ends here and phase 2 may mutate the store (a self-insert reads the pre-insert
                // snapshot — §24). Params bind through the SELECT's own resolver.
                let q = self.run_select(*sel, params)?;

                // Arity: the SELECT's output column count must match the target — checked before
                // any row is produced, so it fires even when the source returns zero rows.
                if q.column_names.len() != arity {
                    return Err(EngineError::new(
                        SqlState::SyntaxError,
                        format!(
                            "INSERT into table {} has {} target {} but SELECT produces {}",
                            table_name,
                            arity,
                            if arity == 1 { "column" } else { "columns" },
                            q.column_names.len(),
                        ),
                    ));
                }

                // Type-assignability, the up-front PostgreSQL gate (§24): each projected
                // column's TYPE must be assignable to its target column. Fires even at zero rows
                // (this is the difference from per-row checking). The per-row `store_value` in
                // `insert_rows` then still range-checks values (22003) and enforces NOT NULL.
                for (i, col) in columns.iter().enumerate() {
                    if let Some(p) = provided[i] {
                        if !q.column_types[p].assignable_to(col.ty) {
                            return Err(type_error(format!(
                                "column {} is of type {} but expression is of type {}",
                                col.name,
                                col.ty.canonical_name(),
                                q.column_types[p].type_name(),
                            )));
                        }
                    }
                }

                self.insert_rows(&table, &columns, pk, &provided, q.rows)?;
                // Cost = the embedded SELECT's accrued cost (§24); storing rows is unmetered.
                Ok(Outcome::Statement { cost: q.cost })
            }
        }
    }

    /// Phase 1 + phase 2 of an INSERT, shared by the `VALUES` and `SELECT` sources. Each element
    /// of `rows` is one row's candidate values indexed by VALUE POSITION `p` (length `arity`);
    /// the declaration-order stored row is built via `provided` (an omitted column takes its
    /// default else NULL) and each value is type-coerced + range-checked by `store_value`
    /// (23502 / 22003 / 22P02 / 42804). The storage key is computed and checked for a duplicate
    /// (23505 — within this batch via `seen_keys` AND against the store) BEFORE any row is
    /// written; only once every row validates are they all inserted (phase 2), allocating a
    /// fresh monotonic rowid in row order for a table with no primary key. All-or-nothing: a
    /// failure leaves the store untouched and burns no rowids.
    fn insert_rows(
        &mut self,
        table: &str,
        columns: &[Column],
        pk: Option<(usize, ScalarType)>,
        provided: &[Option<usize>],
        rows: Vec<Vec<Value>>,
    ) -> Result<()> {
        let n = columns.len();
        let mut prepared: Vec<(Option<Vec<u8>>, Row)> = Vec::with_capacity(rows.len());
        let mut seen_keys: HashSet<Vec<u8>> = HashSet::new();
        for values in &rows {
            let mut row = Vec::with_capacity(n);
            for (i, col) in columns.iter().enumerate() {
                let candidate = match provided[i] {
                    Some(p) => values[p].clone(),
                    None => col.default.clone().unwrap_or(Value::Null),
                };
                row.push(store_value(
                    candidate,
                    col.ty,
                    col.decimal,
                    col.not_null,
                    &col.name,
                )?);
            }

            let key = match pk {
                Some((i, pk_ty)) => {
                    let k = match &row[i] {
                        Value::Int(nn) => encode_int(pk_ty, *nn),
                        // uuid is the first non-integer key: its key is the bare 16 bytes
                        // (uuid-raw16, encoding.md §2.7) — a PK is NOT NULL, so no presence tag.
                        Value::Uuid(u) => u.to_vec(),
                        // A timestamp / timestamptz PRIMARY KEY is supported: its key bytes are
                        // the int64 instant codec (spec/design/timestamp.md §6).
                        Value::Timestamp(m) | Value::Timestamptz(m) => encode_int(pk_ty, *m),
                        // Unreachable: a PK column is NOT NULL, enforced above.
                        Value::Null => unreachable!("primary key column is NOT NULL"),
                        // Unreachable: a boolean PRIMARY KEY is rejected at CREATE TABLE (0A000).
                        Value::Bool(_) => {
                            unreachable!("a boolean primary key is rejected at CREATE TABLE")
                        }
                        // Unreachable: a text/decimal/bytea PRIMARY KEY is rejected at CREATE
                        // TABLE (0A000) — those non-integer PKs are caught by the CREATE gate.
                        Value::Text(_) | Value::Decimal(_) | Value::Bytea(_) => {
                            unreachable!(
                                "a text/decimal/bytea primary key is rejected at CREATE TABLE"
                            )
                        }
                    };
                    if seen_keys.contains(&k) || self.store(table).get(&k).is_some() {
                        return Err(EngineError::new(
                            SqlState::UniqueViolation,
                            "duplicate key value violates primary key uniqueness",
                        ));
                    }
                    seen_keys.insert(k.clone());
                    Some(k)
                }
                None => None,
            };
            prepared.push((key, row));
        }

        // Phase 2 — every row validated, so each insert is guaranteed to succeed. A synthetic
        // rowid is allocated here, in row order, so a failed validation pass burns none
        // (spec/fileformat/format.md, spec/design/grammar.md §12).
        let store = self.store_mut(table);
        for (key, row) in prepared {
            let key = key.unwrap_or_else(|| encode_int(ScalarType::Int64, store.alloc_rowid()));
            assert!(
                store.insert(key, row),
                "pre-validated INSERT key must be unique"
            );
        }
        Ok(())
    }

    /// Analyze and run a DELETE: resolve the table and optional predicate, collect
    /// the keys of matching rows (only a TRUE predicate matches — Kleene), then
    /// remove them. No WHERE deletes every row. Keys are collected before mutating
    /// so the map is not modified while iterating.
    fn execute_delete(&mut self, del: Delete, params: &[Value]) -> Result<Outcome> {
        let table = self.table(&del.table).ok_or_else(|| {
            EngineError::new(
                SqlState::UndefinedTable,
                format!("table does not exist: {}", del.table),
            )
        })?;
        // DELETE is single-table; resolve its WHERE against a one-relation scope.
        let scope = Scope::single(self, table);
        let mut ptypes = ParamTypes::default();
        let mut filter = match &del.filter {
            Some(p) => Some(resolve_boolean_filter(&scope, p, &mut ptypes)?),
            None => None,
        };
        let bound = bind_params(params, &ptypes.finalize()?)?;

        // Fold globally-uncorrelated WHERE subqueries once (their cost is added a single time —
        // spec/design/grammar.md §26, cost.md §3); a correlated one stays and re-runs per row via
        // the per-row outer environment below. The uncorrelated execution reads the pre-DELETE
        // snapshot (we collect keys before mutating), matching PostgreSQL.
        let mut meter = Meter::new();
        if let Some(f) = &mut filter {
            self.fold_uncorrelated_in_rexpr(f, &bound, &mut meter.accrued)?;
        }

        // Collect matching keys before mutating (so the map is not modified mid-scan).
        // A WHERE arithmetic can trap (22003/22012), so this is an explicit loop that
        // propagates the error rather than a `.filter` closure. Each scanned row and each
        // filter evaluation accrues cost (CLAUDE.md §13; spec/design/cost.md §3).
        let mut keys: Vec<Vec<u8>> = Vec::new();
        // A correlated subquery in the WHERE re-runs per row: the eval environment pushes the
        // current row, so `target.col` (an `OuterColumn`) reads it. `outer` starts empty (DELETE
        // is the top-level statement — no enclosing query).
        let env = EvalEnv {
            exec: self,
            params: &bound,
            outer: &[],
        };
        // A full scan walks the table's whole B-tree: page_read per node (block, before the
        // rows), then storage_row_read per row (spec/design/cost.md §3 "page_read").
        meter.charge(COSTS.page_read * self.store(&del.table).node_count() as i64);
        for (k, row) in self.store(&del.table).iter_entries() {
            meter.charge(COSTS.storage_row_read);
            let matched = match &filter {
                None => true,
                Some(f) => f.eval(row, &env, &mut meter)?.is_true(),
            };
            if matched {
                keys.push(k.clone());
            }
        }

        let store = self.store_mut(&del.table);
        for k in &keys {
            store.remove(k);
        }
        Ok(Outcome::Statement {
            cost: meter.accrued,
        })
    }

    /// Analyze and run an UPDATE. Two-phase / all-or-nothing: phase 1 builds and
    /// type-checks every matching row's new values (assignments evaluate against the
    /// *old* row, so `SET a = b, b = a` swaps); a `22003`/`23502` aborts with no
    /// writes. Phase 2 applies. Assigning a PRIMARY KEY column traps `0A000` (the
    /// storage key must not change this slice); a duplicate target column traps
    /// `42701`. No WHERE updates every row.
    fn execute_update(&mut self, upd: Update, params: &[Value]) -> Result<Outcome> {
        let table = self.table(&upd.table).ok_or_else(|| {
            EngineError::new(
                SqlState::UndefinedTable,
                format!("table does not exist: {}", upd.table),
            )
        })?;
        // UPDATE is single-table; the RHS / WHERE resolve against a one-relation scope so the
        // shared resolver serves it too (a qualified `WHERE t.a` against the sole table is fine).
        let scope = Scope::single(self, table);

        // Resolve assignments up front (fail fast, deterministic).
        let pk_idx = table.primary_key_index();
        let mut ptypes = ParamTypes::default();
        let mut plans: Vec<AssignPlan> = Vec::with_capacity(upd.assignments.len());
        for a in &upd.assignments {
            let idx = col_idx(table, &a.column)?;
            if Some(idx) == pk_idx {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "updating a primary key column is not supported",
                ));
            }
            if plans.iter().any(|p| p.idx == idx) {
                return Err(EngineError::new(
                    SqlState::DuplicateColumn,
                    format!("column {} assigned more than once", a.column),
                ));
            }
            let col = &table.columns[idx];
            // The RHS is a general expression evaluated against the *old* row; a literal
            // operand adapts to the target column's type. The result must be assignable to
            // the column's family (integer/decimal/text or NULL; never boolean; decimal→int
            // is explicit-CAST only) — spec/design/decimal.md §6.
            let (source, ty) = resolve(
                &scope,
                &a.value,
                Some(col.ty),
                &mut AggCtx::Forbidden,
                &mut ptypes,
            )?;
            require_assignable(ty, col.ty, &a.column)?;
            plans.push(AssignPlan {
                idx,
                name: col.name.clone(),
                target: col.ty,
                decimal: col.decimal,
                not_null: col.not_null,
                source,
            });
        }

        let mut filter = match &upd.filter {
            Some(p) => Some(resolve_boolean_filter(&scope, p, &mut ptypes)?),
            None => None,
        };
        // All assignment RHSs + the WHERE are resolved: finalize + bind before any scan.
        let bound = bind_params(params, &ptypes.finalize()?)?;

        // Fold globally-uncorrelated subqueries (in any assignment RHS or the WHERE) once — their
        // cost is added a single time (grammar.md §26, cost.md §3); a correlated one stays and
        // re-runs per row via the outer environment. The uncorrelated execution reads the
        // pre-UPDATE snapshot (phase 1 only reads; phase 2 writes), matching PostgreSQL.
        let mut meter = Meter::new();
        for plan in &mut plans {
            self.fold_uncorrelated_in_rexpr(&mut plan.source, &bound, &mut meter.accrued)?;
        }
        if let Some(f) = &mut filter {
            self.fold_uncorrelated_in_rexpr(f, &bound, &mut meter.accrued)?;
        }

        // Phase 1: build + validate every matching row's new values; no writes yet. Each
        // scanned row, the filter, and each assignment RHS accrue cost (the phase-2 writes
        // do not — they evaluate nothing; spec/design/cost.md §3).
        let mut updates: Vec<(Vec<u8>, Row)> = Vec::new();
        // A correlated subquery (in an RHS or the WHERE) re-runs per row: the eval environment
        // pushes the current (old) row, so `target.col` (an `OuterColumn`) reads it. `outer`
        // starts empty (UPDATE is the top-level statement — no enclosing query).
        let env = EvalEnv {
            exec: self,
            params: &bound,
            outer: &[],
        };
        // A full scan walks the table's whole B-tree: page_read per node (block, before the
        // rows), then storage_row_read per row (spec/design/cost.md §3 "page_read").
        meter.charge(COSTS.page_read * self.store(&upd.table).node_count() as i64);
        for (key, row) in self.store(&upd.table).iter_entries() {
            meter.charge(COSTS.storage_row_read);
            let matched = match &filter {
                None => true,
                Some(f) => f.eval(row, &env, &mut meter)?.is_true(),
            };
            if !matched {
                continue;
            }
            let mut new_row = row.clone();
            for plan in &plans {
                let raw = plan.source.eval(row, &env, &mut meter)?;
                new_row[plan.idx] = plan.check(raw)?;
            }
            updates.push((key.clone(), new_row));
        }

        // Phase 2: apply (keys unchanged — a PK column can't be assigned).
        let store = self.store_mut(&upd.table);
        for (key, row) in updates {
            store.replace(&key, row);
        }
        Ok(Outcome::Statement {
            cost: meter.accrued,
        })
    }

    /// Run a SELECT as a top-level statement: `run_select`, then wrap as a query Outcome
    /// (the projection types are internal — only `INSERT ... SELECT` consumes them).
    fn execute_select(&mut self, sel: Select, params: &[Value]) -> Result<Outcome> {
        let r = self.run_select(sel, params)?;
        Ok(Outcome::Query {
            column_names: r.column_names,
            rows: r.rows,
            cost: r.cost,
        })
    }

    /// Execute a set operation (spec/design/grammar.md §25): run the operand query expressions,
    /// unify their column types, combine the rows per the operator + ALL flag, then apply the
    /// trailing ORDER BY / LIMIT / OFFSET. Cost is `lhs.cost + rhs.cost` — the combine, sort, and
    /// window are unmetered (spec/design/cost.md §3).
    fn execute_set_op(&mut self, so: SetOp, params: &[Value]) -> Result<Outcome> {
        let r = self.run_set_op(so, params)?;
        Ok(Outcome::Query {
            column_names: r.column_names,
            rows: r.rows,
            cost: r.cost,
        })
    }

    /// Run a query expression to a `SelectResult`. The top-level orchestrator (CLAUDE.md §2):
    /// (1) PLAN the whole expression tree once against an empty scope chain, threading one
    /// `ParamTypes` so `$N` inference is statement-wide; (2) finalize + bind the parameters;
    /// (3) the `fold_uncorrelated` pass executes each globally-uncorrelated subquery once and
    /// folds it to a constant (preserving the once-only cost — spec/design/cost.md §3);
    /// (4) EXECUTE the plan against an empty outer-row environment. Correlated subqueries that
    /// survive the fold are re-executed per outer row by the evaluator (grammar.md §26).
    fn run_query_expr(&self, qe: QueryExpr, params: &[Value]) -> Result<SelectResult> {
        let mut ptypes = ParamTypes::default();
        let mut plan = self.plan_query(&qe, None, &mut ptypes)?;
        let bound = bind_params(params, &ptypes.finalize()?)?;
        let mut subquery_cost: i64 = 0;
        self.fold_uncorrelated_in_plan(&mut plan, &bound, &mut subquery_cost)?;
        let mut r = self.exec_query_plan(&plan, &[], &bound)?;
        r.cost += subquery_cost;
        Ok(r)
    }

    /// Run a lone `SELECT` — the entry point `execute_select` and `INSERT ... SELECT` use.
    fn run_select(&self, sel: Select, params: &[Value]) -> Result<SelectResult> {
        self.run_query_expr(QueryExpr::Select(Box::new(sel)), params)
    }

    /// Run a set operation as a top-level statement.
    fn run_set_op(&self, so: SetOp, params: &[Value]) -> Result<SelectResult> {
        self.run_query_expr(QueryExpr::SetOp(Box::new(so)), params)
    }

    /// Resolve a query expression into an owned `QueryPlan` against the scope chain (`parent` =
    /// the enclosing query's scope, `None` at top level). A subquery is planned here, once
    /// (spec/design/grammar.md §26).
    fn plan_query(
        &self,
        qe: &QueryExpr,
        parent: Option<&Scope>,
        ptypes: &mut ParamTypes,
    ) -> Result<QueryPlan> {
        match qe {
            QueryExpr::Select(sel) => Ok(QueryPlan::Select(self.plan_select(sel, parent, ptypes)?)),
            QueryExpr::SetOp(so) => Ok(QueryPlan::SetOp(Box::new(
                self.plan_set_op(so, parent, ptypes)?,
            ))),
        }
    }

    /// Execute a resolved plan against an outer-row environment (`outer` = the enclosing rows,
    /// innermost last; empty at top level) and the bound parameters.
    fn exec_query_plan(
        &self,
        plan: &QueryPlan,
        outer: &[&[Value]],
        params: &[Value],
    ) -> Result<SelectResult> {
        match plan {
            QueryPlan::Select(sp) => self.exec_select_plan(sp, outer, params),
            QueryPlan::SetOp(sop) => self.exec_set_op_plan(sop, outer, params),
        }
    }

    /// Plan a set operation (spec/design/grammar.md §25): plan both operands with the same
    /// parent scope, check arity + unify column types up front (so the 42601/42804 fire even
    /// over empty operands), and resolve the trailing ORDER BY by output column name.
    fn plan_set_op(
        &self,
        so: &SetOp,
        parent: Option<&Scope>,
        ptypes: &mut ParamTypes,
    ) -> Result<SetOpPlan> {
        let lhs = self.plan_query(&so.lhs, parent, ptypes)?;
        let rhs = self.plan_query(&so.rhs, parent, ptypes)?;

        // Arity: both operands must produce the same number of columns. PostgreSQL uses 42601.
        if lhs.column_types().len() != rhs.column_types().len() {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                format!(
                    "each {} query must have the same number of columns",
                    setop_name(so.op)
                ),
            ));
        }

        // Per-column type unification (42804 on an incompatible pair). Output column NAMES are
        // the LEFT operand's (PostgreSQL).
        let column_types: Vec<ResolvedType> = lhs
            .column_types()
            .iter()
            .zip(rhs.column_types().iter())
            .map(|(&l, &r)| unify_setop_column(l, r, so.op))
            .collect::<Result<_>>()?;
        let column_names = match &lhs {
            QueryPlan::Select(s) => s.column_names.clone(),
            QueryPlan::SetOp(s) => s.column_names.clone(),
        };

        // Trailing ORDER BY resolves keys by OUTPUT column name (no relation scope after a set
        // operation): a qualified key is 42P01, an unknown name is 42703.
        let mut order: Vec<(usize, bool, bool)> = Vec::with_capacity(so.order_by.len());
        for key in &so.order_by {
            let slot = resolve_setop_order_key(key, &column_names)?;
            order.push((slot, key.descending, key.nulls_first));
        }

        Ok(SetOpPlan {
            op: so.op,
            all: so.all,
            lhs,
            rhs,
            column_names,
            column_types,
            order,
            limit: so.limit,
            offset: so.offset,
        })
    }

    /// Execute a resolved set operation: run both operands against the outer environment,
    /// coerce to the unified types, combine per the operator + ALL flag, then sort + window.
    /// Cost is `lhs.cost + rhs.cost` — the combine, sort, and window are unmetered (cost.md §3).
    fn exec_set_op_plan(
        &self,
        plan: &SetOpPlan,
        outer: &[&[Value]],
        params: &[Value],
    ) -> Result<SelectResult> {
        let left = self.exec_query_plan(&plan.lhs, outer, params)?;
        let right = self.exec_query_plan(&plan.rhs, outer, params)?;

        // Convert each operand's values to the unified column types BEFORE matching — the only
        // runtime conversion is integer -> decimal (so an int value and a decimal value compare
        // equal). Integer width promotion needs none (every integer is i64).
        let mut left_rows = left.rows;
        let mut right_rows = right.rows;
        coerce_setop_rows(&mut left_rows, &left.column_types, &plan.column_types);
        coerce_setop_rows(&mut right_rows, &right.column_types, &plan.column_types);

        let mut rows = combine_setop(plan.op, plan.all, left_rows, right_rows);
        let cost = left.cost + right.cost;

        if !plan.order.is_empty() {
            rows.sort_by(|a, b| {
                for &(idx, descending, nulls_first) in &plan.order {
                    let ord = key_cmp(&a[idx], &b[idx], descending, nulls_first);
                    if ord.is_ne() {
                        return ord;
                    }
                }
                std::cmp::Ordering::Equal
            });
        }

        // LIMIT / OFFSET window — clamp in the integer domain (counts are non-negative, parser),
        // applied AFTER the sort; unmetered, like every window.
        let len = rows.len();
        let start = plan.offset.unwrap_or(0).min(len as i64) as usize;
        let end = match plan.limit {
            Some(lim) if lim < (len - start) as i64 => start + lim as usize,
            _ => len,
        };
        let rows = rows[start..end].to_vec();

        Ok(SelectResult {
            column_names: plan.column_names.clone(),
            column_types: plan.column_types.clone(),
            rows,
            cost,
        })
    }

    /// Analyze and run a SELECT: resolve projected columns and the WHERE/ORDER BY
    /// columns against the catalog, scan the table in primary-key order, filter by
    /// the predicate (three-valued — only TRUE keeps a row), optionally re-sort by
    /// ORDER BY, then project. Rows are produced in a deterministic order
    /// (CLAUDE.md §10). Returns the rows together with each output column's NAME and resolved
    /// TYPE (the types let `INSERT ... SELECT` gate assignability up front — §24) and the
    /// accrued cost. The `&mut self` borrow ends when this returns owned rows, so a caller may
    /// then mutate the store (e.g. `INSERT INTO t SELECT ... FROM t` reads the pre-insert
    /// snapshot, then writes).
    /// Resolve a SELECT into a `SelectPlan` against the scope chain (`parent` = the enclosing
    /// query's scope, for correlated references — grammar.md §26). The resolve half of the old
    /// `run_select`: build the FROM scope, resolve every clause to `RExpr`, infer `$N` types
    /// into `ptypes`. No row is touched and no parameter is bound here (the top-level
    /// `run_query_expr` binds once, after the whole tree is planned).
    fn plan_select(
        &self,
        sel: &Select,
        parent: Option<&Scope>,
        ptypes: &mut ParamTypes,
    ) -> Result<SelectPlan> {
        // Build the FROM scope: resolve each table reference (42P01 if unknown), compute each
        // relation's flat column offset in FROM order, and reject a duplicate label — a
        // self-join without distinct aliases is 42712 (spec/design/grammar.md §15). The scope
        // links to `parent` (for correlation) and the catalog (so a subquery can resolve its own
        // FROM); `allow_subquery` is true (subqueries are legal in a SELECT — UPDATE/DELETE pass
        // a `Scope::single` with it false).
        let mut rels: Vec<ScopeRel> = Vec::with_capacity(1 + sel.joins.len());
        let mut seen_labels: HashSet<String> = HashSet::new();
        let mut offset = 0usize;
        for tref in std::iter::once(&sel.from).chain(sel.joins.iter().map(|j| &j.table)) {
            let table = self.table(&tref.name).ok_or_else(|| {
                EngineError::new(
                    SqlState::UndefinedTable,
                    format!("table does not exist: {}", tref.name),
                )
            })?;
            let label = tref
                .alias
                .clone()
                .unwrap_or_else(|| table.name.clone())
                .to_ascii_lowercase();
            if !seen_labels.insert(label.clone()) {
                return Err(EngineError::new(
                    SqlState::DuplicateAlias,
                    format!("table name {label} specified more than once"),
                ));
            }
            rels.push(ScopeRel {
                label,
                table,
                offset,
            });
            offset += table.columns.len();
        }
        let scope = Scope {
            rels,
            parent,
            catalog: self,
            allow_subquery: true,
        };

        // Resolve GROUP BY keys to flat row indices (a key is a bare/qualified column —
        // grammar.md §18). An unknown column is 42703, an ambiguous bare key 42702.
        let mut group_keys: Vec<usize> = Vec::with_capacity(sel.group_by.len());
        for key in &sel.group_by {
            let r = match key {
                Expr::Column(name) => scope.resolve_bare(name)?,
                Expr::QualifiedColumn { qualifier, name } => {
                    scope.resolve_qualified(qualifier, name)?
                }
                _ => unreachable!("the parser restricts GROUP BY keys to column references"),
            };
            match r {
                Resolved::Local(idx) => group_keys.push(idx),
                // Grouping by an enclosing-query column (a per-outer-row constant) is degenerate
                // and unsupported this slice — the key machinery is flat local indices (§26).
                Resolved::Outer { .. } => {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "GROUP BY may not reference an outer query column",
                    ));
                }
            }
        }

        // An aggregate query has a GROUP BY or an aggregate in the select list. Its projection
        // resolves in Collect mode — aggregates collect into synthetic slots and a non-grouped
        // column is 42803 (spec/design/aggregates.md §4/§6); a plain query resolves in Forbidden
        // mode (columns normal; a stray aggregate would be 42803). Output names per grammar.md §8.
        // GROUP BY, an aggregate in the select list, OR a HAVING clause all make this an
        // aggregate query (HAVING alone groups the whole table — grammar.md §19).
        let is_agg =
            !group_keys.is_empty() || items_have_aggregate(&sel.items) || sel.having.is_some();
        let mut agg_ctx = if is_agg {
            AggCtx::Collect {
                group_keys: group_keys.clone(),
                specs: Vec::new(),
            }
        } else {
            AggCtx::Forbidden
        };
        let (projections, column_names, column_types) =
            resolve_projections(&scope, &sel.items, &mut agg_ctx, ptypes)?;
        // HAVING resolves against the same grouped scope (Collect) — it may reference aggregates
        // (collected into the SAME specs, so their slots follow the projection's) and grouping
        // keys; a non-grouped column is 42803. It must be boolean (42804). Resolved after the
        // projection so the synthetic row is [group_keys..., projection aggs..., HAVING aggs...].
        let having = match &sel.having {
            Some(h) => {
                let (node, ty) = resolve(&scope, h, None, &mut agg_ctx, ptypes)?;
                match ty {
                    ResolvedType::Bool | ResolvedType::Null => Some(node),
                    _ => return Err(type_error("argument of HAVING must be boolean")),
                }
            }
            None => None,
        };
        let agg_specs: Vec<AggSpec> = match agg_ctx {
            AggCtx::Collect { specs, .. } => specs,
            AggCtx::Forbidden => Vec::new(),
        };
        // SELECT DISTINCT over an aggregate query's output (output-row dedup) is deferred (0A000).
        if is_agg && sel.distinct {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "SELECT DISTINCT with aggregates is not supported yet",
            ));
        }
        let filter = match &sel.filter {
            Some(p) => Some(resolve_boolean_filter(&scope, p, ptypes)?),
            None => None,
        };
        // ORDER BY resolution. In an aggregate query a key resolves against the GROUP KEYS — a
        // grouping column gives its synthetic-row slot, a non-grouping column is 42803 (the
        // grouping-error rule, grammar.md §18); the sort runs on the group rows. In a plain
        // query keys resolve against the FROM scope (a flat row index). An outer (correlated)
        // ORDER BY key — ordering by an enclosing-query constant — is degenerate and 0A000 (§26).
        let mut order: Vec<(usize, bool, bool)> = Vec::with_capacity(sel.order_by.len());
        for key in &sel.order_by {
            let r = match &key.qualifier {
                Some(q) => scope.resolve_qualified(q, &key.column)?,
                None => scope.resolve_bare(&key.column)?,
            };
            let idx = match r {
                Resolved::Local(i) => i,
                Resolved::Outer { .. } => {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "ORDER BY may not reference an outer query column",
                    ));
                }
            };
            let slot = if is_agg {
                group_keys
                    .iter()
                    .position(|&gk| gk == idx)
                    .ok_or_else(|| grouping_error_column(&key.column))?
            } else {
                idx
            };
            order.push((slot, key.descending, key.nulls_first));
        }

        // SELECT DISTINCT restriction (spec/design/grammar.md §11): once duplicates are
        // collapsed, an ORDER BY key not in the projected output has no single value per row,
        // so each key must appear as a bare/qualified column in the select list (resolved to
        // the same flat index; or the list is `*`). Matches PostgreSQL (42P10). Aliases are
        // invisible to ORDER BY (§8), so an aliased bare column still counts as projecting it.
        // Only a Local match counts as "projected" (an outer reference has no per-row value).
        if sel.distinct && !order.is_empty() {
            if let SelectItems::Items(items) = &sel.items {
                let mut projected: HashSet<usize> = HashSet::new();
                for it in items {
                    let idx = match &it.expr {
                        Expr::Column(name) => match scope.resolve_bare(name) {
                            Ok(Resolved::Local(i)) => Some(i),
                            _ => None,
                        },
                        Expr::QualifiedColumn { qualifier, name } => {
                            match scope.resolve_qualified(qualifier, name) {
                                Ok(Resolved::Local(i)) => Some(i),
                                _ => None,
                            }
                        }
                        _ => None,
                    };
                    if let Some(i) = idx {
                        projected.insert(i);
                    }
                }
                if order.iter().any(|&(idx, _, _)| !projected.contains(&idx)) {
                    return Err(EngineError::new(
                        SqlState::InvalidColumnReference,
                        "for SELECT DISTINCT, ORDER BY expressions must appear in select list",
                    ));
                }
            }
        }

        // Resolve each JOIN's ON predicate against the PARTIAL scope visible at that node (the
        // relations joined so far — scope.rels[..=k+1]), so a forward reference to a
        // not-yet-joined table is a clean 42P01/42703 instead of an out-of-range row index.
        // CROSS has no ON; INNER and the OUTER kinds (LEFT/RIGHT/FULL) all resolve their ON the
        // same way — the join kind only changes how unmatched rows are handled in the loop below
        // (spec/design/grammar.md §15). The partial scope keeps the same `parent` chain, so a
        // correlated reference in an ON predicate resolves outward (§26).
        let mut joins: Vec<PlanJoin> = Vec::with_capacity(sel.joins.len());
        for (k, j) in sel.joins.iter().enumerate() {
            let on = match &j.on {
                None => None,
                Some(on_expr) => {
                    let partial = Scope {
                        rels: scope.rels[..=k + 1].to_vec(),
                        parent,
                        catalog: self,
                        allow_subquery: true,
                    };
                    Some(resolve_boolean_filter(&partial, on_expr, ptypes)?)
                }
            };
            joins.push(PlanJoin { kind: j.kind, on });
        }

        // Assemble the owned plan (table NAMES + offsets/widths replace the scope's `&Table`s,
        // so the plan outlives the scope and a correlated subquery can re-execute it per row).
        let rels: Vec<PlanRel> = scope
            .rels
            .iter()
            .map(|r| PlanRel {
                table_name: r.table.name.clone(),
                offset: r.offset,
                col_count: r.table.columns.len(),
            })
            .collect();
        Ok(SelectPlan {
            rels,
            joins,
            filter,
            is_agg,
            group_keys,
            agg_specs,
            having,
            order,
            projections,
            column_names,
            column_types,
            distinct: sel.distinct,
            limit: sel.limit,
            offset: sel.offset,
        })
    }

    /// Execute a resolved SELECT against an outer-row environment (`outer` = the enclosing
    /// rows, innermost last; empty at top level) and the bound parameters. The execute half of
    /// the old `run_select`: materialize, nested-loop join, WHERE, then aggregate / DISTINCT /
    /// window + project. The per-row evaluator gets an `EvalEnv` carrying the engine + outer
    /// rows, so a correlated subquery in any clause re-executes against them (grammar.md §26).
    fn exec_select_plan(
        &self,
        plan: &SelectPlan,
        outer: &[&[Value]],
        params: &[Value],
    ) -> Result<SelectResult> {
        let env = EvalEnv {
            exec: self,
            params,
            outer,
        };

        // Materialize each base table once, in primary-key order. A full scan walks the table's
        // whole B-tree, so it charges page_read per node (block, before the rows) and
        // storage_row_read per physical row (spec/design/cost.md §3 "page_read"/JOIN). The nested
        // loop re-reads from these in-memory buffers, which are not stores and charge nothing.
        let mut meter = Meter::new();
        let mut materialized: Vec<Vec<Row>> = Vec::with_capacity(plan.rels.len());
        for rel in &plan.rels {
            let store = self.store(&rel.table_name);
            meter.charge(COSTS.page_read * store.node_count() as i64);
            let mut table_rows: Vec<Row> = Vec::new();
            for row in store.iter_in_key_order() {
                meter.charge(COSTS.storage_row_read);
                table_rows.push(row.clone());
            }
            materialized.push(table_rows);
        }

        // Left-deep nested-loop join. `running` holds the combined rows over the relations
        // joined so far (starting with the first table's rows). For each join, concatenate
        // every running row with every right-table row; CROSS keeps all pairs, INNER keeps a
        // pair iff its ON predicate is TRUE (three-valued — a NULL join key never matches).
        // LEFT/FULL additionally emit each unmatched left row NULL-extended over the right
        // side; RIGHT/FULL emit each unmatched right row NULL-extended over the left side.
        // The NULL-extension pushes evaluate no ON (no operator_eval — spec/design/cost.md §3).
        // Output order is deterministic: running order (outer) then right key order (inner),
        // each unmatched left row after its (empty) match run, all unmatched right rows last in
        // right key order — so a join is deterministic even with no ORDER BY (CLAUDE.md §10).
        let mut running: Vec<Row> = std::mem::take(&mut materialized[0]);
        for (k, pj) in plan.joins.iter().enumerate() {
            let right_rows = &materialized[k + 1];
            let on = &pj.on;
            let emit_left = matches!(pj.kind, JoinKind::Left | JoinKind::Full);
            let emit_right = matches!(pj.kind, JoinKind::Right | JoinKind::Full);
            // NULL-pad widths come from the PLAN, never a sampled row, so they are correct even
            // when `running`/`right_rows` is empty: the right table begins at flat offset
            // rels[k+1].offset (= the width of every running row) and is that many columns wide.
            let left_pad = plan.rels[k + 1].offset;
            let right_pad = plan.rels[k + 1].col_count;
            let mut next: Vec<Row> = Vec::new();
            let mut right_matched = vec![false; right_rows.len()];
            for left in &running {
                let mut left_matched = false;
                for (ri, right) in right_rows.iter().enumerate() {
                    let mut combined = left.clone();
                    combined.extend_from_slice(right);
                    let keep = match on {
                        None => true,
                        Some(pred) => pred.eval(&combined, &env, &mut meter)?.is_true(),
                    };
                    if keep {
                        next.push(combined);
                        left_matched = true;
                        right_matched[ri] = true;
                    }
                }
                if emit_left && !left_matched {
                    let mut combined = left.clone();
                    combined.resize(combined.len() + right_pad, Value::Null);
                    next.push(combined);
                }
            }
            if emit_right {
                for (ri, right) in right_rows.iter().enumerate() {
                    if !right_matched[ri] {
                        let mut combined: Row = vec![Value::Null; left_pad];
                        combined.extend_from_slice(right);
                        next.push(combined);
                    }
                }
            }
            running = next;
        }

        // WHERE over the combined rows (consume `running`, no extra clone). A WHERE arithmetic
        // can trap (22003/22012); each surviving combined row's filter accrues operator_eval.
        let mut rows: Vec<Row> = Vec::new();
        for row in running {
            let keep = match &plan.filter {
                None => true,
                Some(f) => f.eval(&row, &env, &mut meter)?.is_true(),
            };
            if keep {
                rows.push(row);
            }
        }

        // ORDER BY: a stable sort applying each key left to right — the first non-equal key
        // decides, and a full tie keeps the scan order (the sort is stable). Each key's NULL
        // placement is decoupled from its value-direction flip, so an explicit NULLS
        // FIRST|LAST overrides the default (spec/design/grammar.md §10).
        // (Aggregate queries sort their GROUP rows in the aggregate branch below — not these
        // pre-aggregation rows — so the sort here is gated to plain queries.)
        if !plan.is_agg && !plan.order.is_empty() {
            rows.sort_by(|a, b| {
                for &(idx, descending, nulls_first) in &plan.order {
                    let ord = key_cmp(&a[idx], &b[idx], descending, nulls_first);
                    if ord.is_ne() {
                        return ord;
                    }
                }
                std::cmp::Ordering::Equal
            });
        }

        // LIMIT / OFFSET window bounds over a result of `len` rows. Clamp in the integer
        // domain against the row count before indexing — never truncate a huge count into
        // usize (CLAUDE.md §8; spec/design/grammar.md §9). The counts are already
        // non-negative (parser).
        let window_bounds = |len: usize| -> (usize, usize) {
            let start = plan.offset.unwrap_or(0).min(len as i64) as usize;
            let end = match plan.limit {
                Some(lim) if lim < (len - start) as i64 => start + lim as usize,
                _ => len,
            };
            (start, end)
        };

        // Build the output rows. The two paths differ in pipeline order
        // (spec/design/grammar.md §11): without DISTINCT the window slices the sorted
        // source rows and ONLY the windowed rows are projected; with DISTINCT every
        // (sorted) filtered row is projected — dedup must see them all — duplicates drop
        // by first occurrence, and the window then slices the DISTINCT rows.
        let out_rows = if plan.is_agg {
            // Aggregate query — group + accumulate (aggregates.md §5). Fold every filtered row into
            // the accumulators — charging aggregate_accumulate per (row × aggregate) and the
            // operand's own operator_evals — then finalize to the synthetic row [agg_0..] and
            // project it. Even an empty input yields ONE group row (COUNT 0, others NULL —
            // spec/design/aggregates.md §4). The bucketing/finalize is unmetered (cost.md §3).
            // Bucket the post-WHERE rows by their group-key values. The bucket key is the
            // value-canonical Vec<Value> (its Eq/Hash collapse 1.5/1.50 and group NULL with
            // NULL — value.rs); the map is only an index, never iterated, so output order comes
            // from the insertion-ordered `groups` (no hashmap-order leak — CLAUDE.md §8/§10).
            // Whole-table aggregation (no GROUP BY) is one pre-created empty-key group, so it
            // emits ONE row even over zero input (COUNT 0, others NULL); GROUP BY over an empty
            // table creates no groups -> zero rows.
            let mut index: HashMap<Vec<Value>, usize> = HashMap::new();
            let mut groups: Vec<(Vec<Value>, Vec<Acc>)> = Vec::new();
            if plan.group_keys.is_empty() {
                groups.push((
                    Vec::new(),
                    plan.agg_specs.iter().map(|s| Acc::new(s.plan)).collect(),
                ));
                index.insert(Vec::new(), 0);
            }
            for row in &rows {
                let key: Vec<Value> = plan.group_keys.iter().map(|&gk| row[gk].clone()).collect();
                let gi = match index.get(&key) {
                    Some(&i) => i,
                    None => {
                        let i = groups.len();
                        index.insert(key.clone(), i);
                        groups.push((
                            key,
                            plan.agg_specs.iter().map(|s| Acc::new(s.plan)).collect(),
                        ));
                        i
                    }
                };
                for (si, spec) in plan.agg_specs.iter().enumerate() {
                    meter.charge(COSTS.aggregate_accumulate);
                    let v = match &spec.operand {
                        Some(op) => op.eval(row, &env, &mut meter)?,
                        None => Value::Null, // COUNT(*) ignores the value
                    };
                    groups[gi].1[si].fold(v)?;
                }
            }
            // Build one synthetic row per group: [group_key_values..., aggregate_results...].
            let mut group_rows: Vec<Vec<Value>> = Vec::with_capacity(groups.len());
            for (key, accs) in groups {
                let mut srow = key;
                for acc in accs {
                    srow.push(acc.finalize()?);
                }
                group_rows.push(srow);
            }
            // HAVING: filter the grouped rows (after aggregation, before ORDER BY). The
            // predicate is evaluated against each group's synthetic row (charging its
            // operator_evals per group); only a TRUE result keeps the group. A dropped group
            // then charges no row_produced (spec/design/aggregates.md §8).
            if let Some(h) = &plan.having {
                let mut kept: Vec<Vec<Value>> = Vec::with_capacity(group_rows.len());
                for srow in group_rows {
                    if h.eval(&srow, &env, &mut meter)?.is_true() {
                        kept.push(srow);
                    }
                }
                group_rows = kept;
            }
            // ORDER BY over the grouped output (keys are synthetic group-key slots).
            if !plan.order.is_empty() {
                group_rows.sort_by(|a, b| {
                    for &(slot, descending, nulls_first) in &plan.order {
                        let ord = key_cmp(&a[slot], &b[slot], descending, nulls_first);
                        if ord.is_ne() {
                            return ord;
                        }
                    }
                    std::cmp::Ordering::Equal
                });
            }
            // Window + project; only an emitted row charges row_produced + projection cost.
            let (start, end) = window_bounds(group_rows.len());
            let mut out_rows = Vec::with_capacity(end - start);
            for srow in &group_rows[start..end] {
                meter.charge(COSTS.row_produced);
                let mut out = Vec::with_capacity(plan.projections.len());
                for p in &plan.projections {
                    out.push(p.eval(srow, &env, &mut meter)?);
                }
                out_rows.push(out);
            }
            out_rows
        } else if plan.distinct {
            // Project every filtered row (charging projection cost per row, the §3
            // asymmetry), keeping first occurrences. `seen` is membership-only: the
            // output order comes from the deterministic source iteration, never from set
            // iteration (no hashmap-order leak — CLAUDE.md §8/§10).
            let mut seen: std::collections::HashSet<Vec<Value>> = std::collections::HashSet::new();
            let mut distinct_rows: Vec<Vec<Value>> = Vec::new();
            for row in &rows {
                let mut out = Vec::with_capacity(plan.projections.len());
                for p in &plan.projections {
                    out.push(p.eval(row, &env, &mut meter)?);
                }
                if seen.insert(out.clone()) {
                    distinct_rows.push(out);
                }
            }
            // LIMIT / OFFSET applies to the DISTINCT rows; only the emitted rows charge
            // row_produced (spec/design/cost.md §3).
            let (start, end) = window_bounds(distinct_rows.len());
            let mut out_rows = Vec::with_capacity(end - start);
            for row in distinct_rows.drain(start..end) {
                meter.charge(COSTS.row_produced);
                out_rows.push(row);
            }
            out_rows
        } else {
            // Window the sorted rows BEFORE projection, so rows skipped by OFFSET or
            // excluded by LIMIT accrue no row_produced/projection cost (they were still
            // scanned + filtered above). Producing a row, and each projection-list
            // evaluation, accrue cost. (ORDER BY's sort comparisons are not metered —
            // spec/design/cost.md §3.)
            let (start, end) = window_bounds(rows.len());
            let mut out_rows = Vec::with_capacity(end - start);
            for row in &rows[start..end] {
                meter.charge(COSTS.row_produced);
                let mut out = Vec::with_capacity(plan.projections.len());
                for p in &plan.projections {
                    out.push(p.eval(row, &env, &mut meter)?);
                }
                out_rows.push(out);
            }
            out_rows
        };

        Ok(SelectResult {
            column_names: plan.column_names.clone(),
            column_types: plan.column_types.clone(),
            rows: out_rows,
            // The scan/eval cost (correlated subqueries fold their per-row cost in via the
            // evaluator). Globally-uncorrelated subqueries are folded once before exec and their
            // cost is added at the `run_query_expr` level (spec/design/cost.md §3).
            cost: meter.accrued,
        })
    }

    // ---- Uncorrelated subquery folding (spec/design/grammar.md §26) ----------------------
    //
    // After the whole statement tree is planned + the parameters bound, this bottom-up pass
    // walks every `RExpr::Subquery` node in the plan tree: it first folds within the node's own
    // sub-plan, then — if the subquery references NO enclosing scope (a global constant, PG's
    // "initplan") — executes it ONCE and replaces it with a constant (scalar -> its value;
    // EXISTS -> a boolean; IN -> an `InValues` over the result column), accruing the subquery's
    // cost once (preserving the committed once-only cost — cost.md §3). A CORRELATED subquery is
    // left in place; the evaluator re-executes it per outer row. So after this pass the only
    // surviving `Subquery` nodes are correlated.

    fn fold_uncorrelated_in_plan(
        &self,
        plan: &mut QueryPlan,
        bound: &[Value],
        cost: &mut i64,
    ) -> Result<()> {
        match plan {
            QueryPlan::Select(sp) => self.fold_uncorrelated_in_select(sp, bound, cost),
            QueryPlan::SetOp(sop) => {
                self.fold_uncorrelated_in_plan(&mut sop.lhs, bound, cost)?;
                self.fold_uncorrelated_in_plan(&mut sop.rhs, bound, cost)
            }
        }
    }

    fn fold_uncorrelated_in_select(
        &self,
        sp: &mut SelectPlan,
        bound: &[Value],
        cost: &mut i64,
    ) -> Result<()> {
        for j in &mut sp.joins {
            if let Some(on) = &mut j.on {
                self.fold_uncorrelated_in_rexpr(on, bound, cost)?;
            }
        }
        if let Some(f) = &mut sp.filter {
            self.fold_uncorrelated_in_rexpr(f, bound, cost)?;
        }
        if let Some(h) = &mut sp.having {
            self.fold_uncorrelated_in_rexpr(h, bound, cost)?;
        }
        for s in &mut sp.agg_specs {
            if let Some(op) = &mut s.operand {
                self.fold_uncorrelated_in_rexpr(op, bound, cost)?;
            }
        }
        for p in &mut sp.projections {
            self.fold_uncorrelated_in_rexpr(p, bound, cost)?;
        }
        Ok(())
    }

    /// Fold this node if it is an uncorrelated `Subquery`, else recurse into its children.
    fn fold_uncorrelated_in_rexpr(
        &self,
        e: &mut RExpr,
        bound: &[Value],
        cost: &mut i64,
    ) -> Result<()> {
        if matches!(e, RExpr::Subquery { .. }) {
            // Bottom-up: fold within this subquery's own sub-plan (and its IN lhs) first, so a
            // globally-uncorrelated subquery nested inside it is already a constant before we run
            // it. Then leave it untouched if it is correlated (re-run per outer row at eval).
            if let RExpr::Subquery { plan, lhs, .. } = e {
                if let Some(l) = lhs {
                    self.fold_uncorrelated_in_rexpr(l, bound, cost)?;
                }
                self.fold_uncorrelated_in_plan(plan, bound, cost)?;
                if query_plan_references_outer(plan, 0) {
                    return Ok(());
                }
            }
            // Uncorrelated: execute ONCE and fold to a constant / InValues. Take ownership so the
            // sub-plan can be moved/run without aliasing the node we are about to overwrite.
            let taken = std::mem::replace(e, RExpr::ConstNull);
            let RExpr::Subquery {
                plan,
                kind,
                lhs,
                negated,
            } = taken
            else {
                unreachable!("guarded by matches! above")
            };
            let r = self.exec_query_plan(&plan, &[], bound)?;
            *cost += r.cost;
            *e = match kind {
                SubqueryKind::Scalar => {
                    if r.rows.len() > 1 {
                        return Err(EngineError::new(
                            SqlState::CardinalityViolation,
                            "more than one row returned by a subquery used as an expression",
                        ));
                    }
                    let value = r
                        .rows
                        .into_iter()
                        .next()
                        .map(|mut row| row.swap_remove(0))
                        .unwrap_or(Value::Null);
                    value_to_rexpr(&value)
                }
                SubqueryKind::Exists => RExpr::ConstBool(!r.rows.is_empty() != negated),
                SubqueryKind::In => {
                    let list: Vec<Value> = r
                        .rows
                        .into_iter()
                        .map(|mut row| row.swap_remove(0))
                        .collect();
                    RExpr::InValues {
                        lhs: lhs.expect("an IN subquery carries its resolved lhs"),
                        list,
                        negated,
                    }
                }
            };
            return Ok(());
        }
        match e {
            RExpr::Cast { inner, .. } => self.fold_uncorrelated_in_rexpr(inner, bound, cost),
            RExpr::Neg { operand, .. } => self.fold_uncorrelated_in_rexpr(operand, bound, cost),
            RExpr::Not(x) => self.fold_uncorrelated_in_rexpr(x, bound, cost),
            RExpr::Arith { lhs, rhs, .. }
            | RExpr::Compare { lhs, rhs, .. }
            | RExpr::Distinct { lhs, rhs, .. }
            | RExpr::Like { lhs, rhs, .. } => {
                self.fold_uncorrelated_in_rexpr(lhs, bound, cost)?;
                self.fold_uncorrelated_in_rexpr(rhs, bound, cost)
            }
            RExpr::And(l, r) | RExpr::Or(l, r) => {
                self.fold_uncorrelated_in_rexpr(l, bound, cost)?;
                self.fold_uncorrelated_in_rexpr(r, bound, cost)
            }
            RExpr::IsNull { operand, .. } => self.fold_uncorrelated_in_rexpr(operand, bound, cost),
            RExpr::Case { arms, els, .. } => {
                for (c, res) in arms {
                    self.fold_uncorrelated_in_rexpr(c, bound, cost)?;
                    self.fold_uncorrelated_in_rexpr(res, bound, cost)?;
                }
                self.fold_uncorrelated_in_rexpr(els, bound, cost)
            }
            RExpr::ScalarFunc { args, .. } => {
                for a in args {
                    self.fold_uncorrelated_in_rexpr(a, bound, cost)?;
                }
                Ok(())
            }
            RExpr::InValues { lhs, .. } => self.fold_uncorrelated_in_rexpr(lhs, bound, cost),
            // Leaves and the (already-handled) Subquery: nothing to recurse into.
            RExpr::Subquery { .. }
            | RExpr::Column(_)
            | RExpr::OuterColumn { .. }
            | RExpr::Param(_)
            | RExpr::ConstInt(_)
            | RExpr::ConstBool(_)
            | RExpr::ConstText(_)
            | RExpr::ConstDecimal(_)
            | RExpr::ConstBytea(_)
            | RExpr::ConstUuid(_)
            | RExpr::ConstTimestamp(_)
            | RExpr::ConstTimestamptz(_)
            | RExpr::ConstNull => Ok(()),
        }
    }

    /// Shared read access to a table's store in the visible snapshot (the table is known to
    /// exist) — the open transaction's working set, else the committed state.
    fn store(&self, name: &str) -> &TableStore {
        self.read_snap().store(name)
    }

    /// Mutable access to a table's store in the working snapshot (the table is known to exist;
    /// a write runs within a transaction, so the working set is present).
    pub(crate) fn store_mut(&mut self, name: &str) -> &mut TableStore {
        self.working_mut().store_mut(name)
    }
}

// ============================================================================
// Resolution scope (multi-table FROM — spec/design/grammar.md §15).
//
// A `Scope` is the ordered list of relations a SELECT's FROM clause puts in scope, each
// carrying the flat COLUMN OFFSET at which its columns begin in the concatenated (joined)
// row. A resolved column reference bakes a single flat index `offset + local` into
// `RExpr::Column`, so the joined row is just each relation's row concatenated in FROM order
// and the expression evaluator is unchanged. A single-table SELECT / UPDATE / DELETE is a
// one-relation scope (offset 0), so the same resolver serves every statement.
//
// NOTE (forward-compat): the scope keys resolution ONLY on column name and type — never on a
// column's NOT NULL / PRIMARY KEY flags. A column on the nullable side of a future outer join
// is NULL-extended at runtime regardless of its declared nullability, so no resolver shortcut
// may fold on it (spec/design/grammar.md §15).
// ============================================================================

/// One relation in a FROM scope: its label (alias, else table name — lower-cased for
/// case-insensitive matching), the table, and the flat offset of its first column in the
/// joined row.
#[derive(Clone)]
struct ScopeRel<'a> {
    label: String,
    table: &'a Table,
    offset: usize,
}

/// How a column reference resolved against the scope CHAIN (spec/design/grammar.md §26).
/// `Local` is a column of one of THIS query's relations (a flat row index into the joined
/// row); `Outer` is a correlated reference to an enclosing query — `level` hops outward
/// (1 = immediate parent) and `index` is the flat column index within that ancestor's row.
#[derive(Clone, Copy)]
enum Resolved {
    Local(usize),
    Outer { level: usize, index: usize },
}

/// The relations a query's FROM clause puts in scope, in FROM order, plus the enclosing
/// scope chain (for correlated references — grammar.md §26) and the catalog (so resolving a
/// subquery can look up its own FROM tables).
struct Scope<'a> {
    rels: Vec<ScopeRel<'a>>,
    /// The enclosing query's scope, for correlated-reference resolution (None at top level).
    parent: Option<&'a Scope<'a>>,
    /// The catalog, so a subquery's inner FROM tables can be resolved during planning.
    catalog: &'a Database,
    /// Whether a subquery is allowed in this scope's expressions: true inside a SELECT (and
    /// its nested subqueries), false for UPDATE/DELETE (a subquery there is 0A000 this slice).
    allow_subquery: bool,
}

impl<'a> Scope<'a> {
    /// A one-relation scope with no parent (the single-table UPDATE / DELETE case). Subqueries
    /// ARE allowed: a correlated reference resolves to the target row via the per-row outer
    /// environment (the subquery's parent is this scope), an uncorrelated one folds once
    /// (spec/design/grammar.md §26). SELECT builds its own scope in `plan_select`.
    fn single(catalog: &'a Database, table: &'a Table) -> Scope<'a> {
        Scope {
            rels: vec![ScopeRel {
                label: table.name.to_ascii_lowercase(),
                table,
                offset: 0,
            }],
            parent: None,
            catalog,
            allow_subquery: true,
        }
    }

    /// Resolve a bare column name against THIS scope, then OUTWARD through the parent chain.
    /// Within one scope: two+ relations have it → 42702 ambiguous; exactly one → `Local`; none
    /// → fall through to the parent. A name found only in an ancestor is an `Outer` reference
    /// (nearest scope wins — an inner match shadows an outer one, matching PostgreSQL). 42703
    /// only if no scope in the chain has it.
    fn resolve_bare(&self, name: &str) -> Result<Resolved> {
        let mut found: Option<usize> = None;
        for r in &self.rels {
            if let Some(local) = r.table.column_index(name) {
                if found.is_some() {
                    return Err(ambiguous_column(name));
                }
                found = Some(r.offset + local);
            }
        }
        if let Some(idx) = found {
            return Ok(Resolved::Local(idx));
        }
        match self.parent {
            Some(p) => Ok(outer_of(p.resolve_bare(name)?)),
            None => Err(undefined_column(name)),
        }
    }

    /// Resolve a qualified `rel.col` against THIS scope, then outward. A qualifier that names a
    /// relation in this scope binds here — a missing column is then 42703 (no fall-through, so
    /// an inner relation shadows a same-named outer one). Only an unknown qualifier walks
    /// outward (42P01 if no ancestor has it).
    fn resolve_qualified(&self, qualifier: &str, name: &str) -> Result<Resolved> {
        let q = qualifier.to_ascii_lowercase();
        if let Some(rel) = self.rels.iter().find(|r| r.label == q) {
            let local = rel
                .table
                .column_index(name)
                .ok_or_else(|| undefined_column(name))?;
            return Ok(Resolved::Local(rel.offset + local));
        }
        match self.parent {
            Some(p) => Ok(outer_of(p.resolve_qualified(qualifier, name)?)),
            None => Err(missing_from_entry(qualifier)),
        }
    }

    /// The column at a flat index in THIS scope (the index is known valid — resolution made it).
    fn column_at(&self, flat: usize) -> &Column {
        for r in &self.rels {
            let n = r.table.columns.len();
            if flat >= r.offset && flat < r.offset + n {
                return &r.table.columns[flat - r.offset];
            }
        }
        unreachable!("a resolved flat column index is always in range")
    }

    /// The ancestor scope `level` hops outward (1 = immediate parent).
    fn ancestor(&self, level: usize) -> &Scope<'a> {
        let mut s = self;
        for _ in 0..level {
            s = s
                .parent
                .expect("a correlated level is within the scope-chain depth");
        }
        s
    }

    /// The column a resolution refers to — `Local` in this scope, or `Outer` in an ancestor.
    fn column_of(&self, r: Resolved) -> &Column {
        match r {
            Resolved::Local(idx) => self.column_at(idx),
            Resolved::Outer { level, index } => self.ancestor(level).column_at(index),
        }
    }
}

/// Lift a parent-scope resolution into the child's frame: a `Local` in the parent is one hop
/// out (`level` 1); an `Outer` in the parent is one deeper hop (`level + 1`).
fn outer_of(r: Resolved) -> Resolved {
    match r {
        Resolved::Local(index) => Resolved::Outer { level: 1, index },
        Resolved::Outer { level, index } => Resolved::Outer {
            level: level + 1,
            index,
        },
    }
}

// ============================================================================
// Resolved expression layer.
//
// Parse → `Expr` (names) → resolve → `RExpr` (column indices, known result types,
// folded constants) → eval per row → `Value`. The resolver is where all
// type-checking and the literal range-check live; the evaluator is a pure tree-walk.
// ============================================================================

/// The static type of a resolved expression. `Null` is an untyped NULL literal (its
/// type, if needed, is settled by the surrounding operator/context). `Text` is the
/// `text` family (one collation, `C`); it does not promote.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ResolvedType {
    Int(ScalarType),
    Bool,
    Text,
    /// The decimal family (one type; the per-column typmod is carried separately, not here).
    Decimal,
    /// The bytea family (raw bytes); does not promote.
    Bytea,
    /// The uuid family (fixed 16 bytes); does not promote. The first non-integer key type.
    Uuid,
    /// The `timestamp` family (zoneless instant). Does not compare/cast to `Timestamptz`.
    Timestamp,
    /// The `timestamptz` family (UTC instant). Does not compare/cast to `Timestamp`.
    Timestamptz,
    Null,
}

impl ResolvedType {
    /// The resolved type of a stored column of `ty` — used for the output type of a bare column
    /// projection (`SELECT *` / `SELECT col`). A column always has a concrete type, never `Null`.
    fn of_column(ty: ScalarType) -> ResolvedType {
        match ty {
            ScalarType::Int16 | ScalarType::Int32 | ScalarType::Int64 => ResolvedType::Int(ty),
            ScalarType::Bool => ResolvedType::Bool,
            ScalarType::Text => ResolvedType::Text,
            ScalarType::Decimal => ResolvedType::Decimal,
            ScalarType::Bytea => ResolvedType::Bytea,
            ScalarType::Uuid => ResolvedType::Uuid,
            ScalarType::Timestamp => ResolvedType::Timestamp,
            ScalarType::Timestamptz => ResolvedType::Timestamptz,
        }
    }

    /// This type's name, for a `42804` assignability message (the integer width is exact).
    fn type_name(self) -> &'static str {
        match self {
            ResolvedType::Int(st) => st.canonical_name(),
            ResolvedType::Bool => "boolean",
            ResolvedType::Text => "text",
            ResolvedType::Decimal => "decimal",
            ResolvedType::Bytea => "bytea",
            ResolvedType::Uuid => "uuid",
            ResolvedType::Timestamp => "timestamp",
            ResolvedType::Timestamptz => "timestamptz",
            ResolvedType::Null => "unknown",
        }
    }

    /// Whether a projected value of this type is assignable to a `col_ty` column for storage —
    /// the FAMILY-level gate `INSERT ... SELECT` applies up front (spec/design/grammar.md §24),
    /// before any row is produced (so it fires even over an empty source). It is the
    /// family-level subset of `store_value` and MUST agree with it: an integer assigns to an
    /// integer or decimal column (int→decimal widens), a decimal only to a decimal column
    /// (decimal→int is explicit-CAST only), text to text/uuid/bytea/timestamp/timestamptz (the
    /// documented text adaptation — the per-row store then parses, trapping 22P02/22007 on
    /// malformed input), boolean→boolean, uuid→uuid, bytea→bytea, a timestamp only to a timestamp
    /// column and a timestamptz only to a timestamptz column (the two never cross — they do not
    /// even compare, timestamp.md), and a NULL-typed projection to any column (a NOT NULL target
    /// then traps 23502 per row). A non-assignable pair is a 42804.
    fn assignable_to(self, col_ty: ScalarType) -> bool {
        match self {
            ResolvedType::Null => true,
            ResolvedType::Int(_) => col_ty.is_integer() || col_ty.is_decimal(),
            ResolvedType::Decimal => col_ty.is_decimal(),
            ResolvedType::Bool => col_ty.is_bool(),
            ResolvedType::Text => {
                col_ty.is_text()
                    || col_ty.is_uuid()
                    || col_ty.is_bytea()
                    || col_ty.is_timestamp()
                    || col_ty.is_timestamptz()
            }
            ResolvedType::Bytea => col_ty.is_bytea(),
            ResolvedType::Uuid => col_ty.is_uuid(),
            ResolvedType::Timestamp => col_ty.is_timestamp(),
            ResolvedType::Timestamptz => col_ty.is_timestamptz(),
        }
    }
}

#[derive(Clone, Copy)]
enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

#[derive(Clone, Copy)]
enum CmpOp {
    Eq,
    Lt,
    Gt,
    Le,
    Ge,
}

/// The scalar functions (kind = "function", spec/design/functions.md §9), parsed from a call
/// name (case-insensitive). Evaluated per row; the overload (integer vs decimal) is recovered
/// at eval from the argument's runtime value.
#[derive(Clone, Copy)]
enum ScalarFunc {
    Abs,
    Round,
}

/// A resolved expression: a tree over fixed column indices, ready to evaluate against
/// a row. Arithmetic nodes carry their (promotion-tower) result type so the computed
/// value can be range-checked against it (the int16+int16 → int16 boundary).
enum RExpr {
    Column(usize),
    ConstInt(i64),
    ConstBool(bool),
    ConstText(String),
    ConstDecimal(Decimal),
    ConstBytea(Vec<u8>),
    ConstUuid([u8; 16]),
    /// A parsed `timestamp` / `timestamptz` literal: the int64 microsecond instant.
    ConstTimestamp(i64),
    ConstTimestamptz(i64),
    ConstNull,
    /// A bind parameter, by 0-based index into the bound-values slice passed to `eval`. Its
    /// static type was inferred from context at resolve (spec/design/api.md §5); the value
    /// is supplied (and coerced to that type) before evaluation.
    Param(usize),
    Cast {
        inner: Box<RExpr>,
        target: ScalarType,
        /// For a decimal target, the optional `numeric(p,s)` typmod to coerce to.
        typmod: Option<DecimalTypmod>,
    },
    Neg {
        operand: Box<RExpr>,
        result: ScalarType,
    },
    Not(Box<RExpr>),
    Arith {
        op: ArithOp,
        lhs: Box<RExpr>,
        rhs: Box<RExpr>,
        result: ScalarType,
    },
    Compare {
        op: CmpOp,
        lhs: Box<RExpr>,
        rhs: Box<RExpr>,
    },
    And(Box<RExpr>, Box<RExpr>),
    Or(Box<RExpr>, Box<RExpr>),
    IsNull {
        operand: Box<RExpr>,
        negated: bool,
    },
    /// `lhs IS [NOT] DISTINCT FROM rhs` — NULL-safe equality. `negated = true` is the
    /// `IS NOT DISTINCT FROM` ("are they the same?") form; `false` is `IS DISTINCT FROM`.
    /// Always evaluates to a definite boolean.
    Distinct {
        lhs: Box<RExpr>,
        rhs: Box<RExpr>,
        negated: bool,
    },
    /// `lhs LIKE rhs` / `lhs NOT LIKE rhs` — text pattern match (grammar.md §22). Both operands
    /// resolve to text (or NULL); a NULL operand makes the result NULL. The matcher runs over
    /// Unicode code points and traps 22025 on a pattern ending in a lone escape reached during
    /// matching. `negated` carries the NOT keyword (NOT LIKE = the negation of the match).
    Like {
        lhs: Box<RExpr>,
        rhs: Box<RExpr>,
        negated: bool,
    },
    /// A resolved `CASE` (grammar.md §23). `arms` is `(condition, result)` pairs — the condition
    /// is the searched boolean predicate, or the simple form's resolved `operand = value`
    /// equality. `els` is the ELSE result (`ConstNull` for an implicit ELSE). Evaluated lazily:
    /// the first TRUE condition's result wins. `coerce_decimal` is set when the unified result
    /// type is decimal, so integer results widen to decimal at eval.
    Case {
        arms: Vec<(RExpr, RExpr)>,
        els: Box<RExpr>,
        coerce_decimal: bool,
    },
    /// A scalar-function call (abs/round, spec/design/functions.md §9), evaluated per row in
    /// any context. `result` is the static result type — for `abs` over an integer it is the
    /// operand's integer type, so the magnitude is range-checked at that boundary (the same
    /// 22003 discipline as `Neg`); for `abs` over decimal and all `round` forms it is decimal.
    /// Arguments propagate NULL.
    ScalarFunc {
        func: ScalarFunc,
        args: Vec<RExpr>,
        result: ScalarType,
    },
    /// A correlated column reference (spec/design/grammar.md §26): the column `index` of the
    /// enclosing-query row `level` hops out (1 = immediate parent). A **leaf** — charges no
    /// `operator_eval`, like `Column`; at eval it reads from the outer-row environment.
    OuterColumn {
        level: usize,
        index: usize,
    },
    /// A subquery resolved once against the scope chain (spec/design/grammar.md §26). After the
    /// `fold_uncorrelated` pass only CORRELATED subqueries survive as this node (uncorrelated
    /// ones are folded to a constant / `InValues`). At eval the inner `plan` is re-executed
    /// against the pushed outer-row environment, once per outer row that reaches this node.
    /// (The element/scalar static type was settled at resolve — the value alone suffices here.)
    Subquery {
        plan: Box<QueryPlan>,
        kind: SubqueryKind,
        /// For `In`: the outer value tested for membership. `None` for Scalar/Exists.
        lhs: Option<Box<RExpr>>,
        /// For `In` / `Exists`: the `NOT` flag.
        negated: bool,
    },
    /// A folded **uncorrelated** `IN (subquery)` (spec/design/grammar.md §26): the subquery ran
    /// once (at the fold pass) yielding the constant `list`; per row it tests `lhs` for 3-valued
    /// membership (empty → `negated`; a NULL with no positive match → NULL). One `operator_eval`
    /// for the node plus one per element compared.
    InValues {
        lhs: Box<RExpr>,
        list: Vec<Value>,
        negated: bool,
    },
}

/// Which subquery form an `RExpr::Subquery` is (spec/design/grammar.md §26).
#[derive(Clone, Copy, PartialEq, Eq)]
enum SubqueryKind {
    Scalar,
    Exists,
    In,
}

// ============================================================================
// Query plans — the resolved, owned form of a query, executable repeatedly (a correlated
// subquery is re-run once per outer row). `plan_query` (the resolve half of the old
// `run_select`) produces a `QueryPlan`; `exec_query_plan` (the execute half) consumes it
// against an outer-row environment. The split lets a subquery be resolved ONCE — so its
// structural/type errors surface even when the outer produces zero rows — yet executed many
// times (spec/design/grammar.md §26).
// ============================================================================

/// A resolved query expression: a SELECT plan or a set-operation plan (mirrors `QueryExpr`).
enum QueryPlan {
    Select(SelectPlan),
    SetOp(Box<SetOpPlan>),
}

impl QueryPlan {
    /// The output column types — for a scalar/IN subquery's plan-time column-count check (42601)
    /// and its folded/element type.
    fn column_types(&self) -> &[ResolvedType] {
        match self {
            QueryPlan::Select(s) => &s.column_types,
            QueryPlan::SetOp(s) => &s.column_types,
        }
    }
}

/// One relation in a SELECT plan: the table name (looked up in the store at exec), the flat
/// offset of its first column in the joined row, and its column count (for NULL-padding).
struct PlanRel {
    table_name: String,
    offset: usize,
    col_count: usize,
}

/// One join in a SELECT plan: its kind and resolved ON predicate (`None` for CROSS). The right
/// relation is `rels[k+1]`.
struct PlanJoin {
    kind: JoinKind,
    on: Option<RExpr>,
}

/// A resolved SELECT, executable against an outer-row environment (the execute half of the old
/// `run_select`, lifted to a value so a correlated subquery can re-run it per outer row).
struct SelectPlan {
    rels: Vec<PlanRel>,
    joins: Vec<PlanJoin>,
    filter: Option<RExpr>,
    is_agg: bool,
    group_keys: Vec<usize>,
    agg_specs: Vec<AggSpec>,
    having: Option<RExpr>,
    /// (flat slot, descending, nulls_first) per ORDER BY key.
    order: Vec<(usize, bool, bool)>,
    projections: Vec<RExpr>,
    column_names: Vec<String>,
    column_types: Vec<ResolvedType>,
    distinct: bool,
    limit: Option<i64>,
    offset: Option<i64>,
}

/// A resolved set operation (spec/design/grammar.md §25): both operands planned with the same
/// parent scope (so a correlated set-op subquery works), the unified output types, and the
/// trailing ORDER BY / LIMIT / OFFSET resolved by output column.
struct SetOpPlan {
    op: SetOpKind,
    all: bool,
    lhs: QueryPlan,
    rhs: QueryPlan,
    column_names: Vec<String>,
    column_types: Vec<ResolvedType>,
    /// (output slot, descending, nulls_first) — the trailing ORDER BY resolved by output name.
    order: Vec<(usize, bool, bool)>,
    limit: Option<i64>,
    offset: Option<i64>,
}

// ============================================================================
// Aggregate resolution + accumulation (spec/design/aggregates.md).
//
// An aggregate query's select list resolves in `Collect` mode: each aggregate call is
// collected into an `AggSpec` (its plan + resolved argument) and replaced by a reference to
// a synthetic-row slot (an `RExpr::Column(slot)` indexing the finalized aggregate results),
// so the existing evaluator projects the result with no new node. Outside Collect mode
// (`Forbidden`: WHERE / ON / an aggregate's own argument / any non-aggregate query) a column
// resolves normally and an aggregate call is a 42803 grouping error.
// ============================================================================

/// The aggregate-resolution context threaded through `resolve`.
enum AggCtx {
    /// Aggregates are not allowed here (a FuncCall is 42803); columns resolve normally.
    Forbidden,
    /// An aggregate query's projection: a FuncCall collects into `specs` and resolves to a
    /// synthetic slot (group_keys.len() + its index); a column resolves to its position among
    /// `group_keys` (a synthetic slot in 0..group_keys.len()) if it is a grouping key, else
    /// 42803. `group_keys` holds the resolved flat indices of the GROUP BY columns (empty for
    /// whole-table aggregation — then every bare column is 42803). The synthetic row the
    /// projection evaluates against is `[group_key_values…, aggregate_results…]`.
    Collect {
        group_keys: Vec<usize>,
        specs: Vec<AggSpec>,
    },
}

/// The five aggregate functions, parsed from a call name (case-insensitive).
#[derive(Clone, Copy)]
enum AggFunc {
    Count,
    Sum,
    Min,
    Max,
    Avg,
}

/// The runtime plan for one aggregate, fixed at resolve from the function + operand type
/// (the PG widening — spec/design/aggregates.md §3).
#[derive(Clone, Copy, PartialEq, Eq)]
enum AggPlan {
    /// COUNT(*) — count every row (NULLs included).
    CountStar,
    /// COUNT(expr) — count non-NULL inputs.
    Count,
    /// SUM(int16|int32) — accumulate i64, result int64 (traps 22003 at the int64 bound).
    SumInt,
    /// SUM(int64|decimal) — accumulate decimal, result decimal (traps 22003 at the cap).
    SumDecimal,
    /// AVG — accumulate a decimal sum + i64 count; result sum/count (decimal), NULL if count 0.
    Avg,
    Min,
    Max,
}

/// One resolved aggregate: its plan and its resolved argument expression (evaluated per
/// input row against the real row). `operand` is `None` for COUNT(*).
struct AggSpec {
    plan: AggPlan,
    operand: Option<RExpr>,
}

/// A running aggregate accumulator (one per AggSpec), folded per input row then finalized.
enum Acc {
    CountStar(i64),
    Count(i64),
    SumInt { sum: i64, seen: bool },
    SumDecimal { sum: Decimal, seen: bool },
    Avg { sum: Decimal, count: i64 },
    MinMax { cur: Option<Value>, is_min: bool },
}

impl Acc {
    fn new(plan: AggPlan) -> Acc {
        match plan {
            AggPlan::CountStar => Acc::CountStar(0),
            AggPlan::Count => Acc::Count(0),
            AggPlan::SumInt => Acc::SumInt {
                sum: 0,
                seen: false,
            },
            AggPlan::SumDecimal => Acc::SumDecimal {
                sum: Decimal::from_i64(0),
                seen: false,
            },
            AggPlan::Avg => Acc::Avg {
                sum: Decimal::from_i64(0),
                count: 0,
            },
            AggPlan::Min => Acc::MinMax {
                cur: None,
                is_min: true,
            },
            AggPlan::Max => Acc::MinMax {
                cur: None,
                is_min: false,
            },
        }
    }

    /// Fold one input value into the accumulator. NULL arguments are skipped (COUNT(*) ignores
    /// the value and always counts). Traps 22003 on SUM/AVG overflow at the result bound.
    fn fold(&mut self, value: Value) -> Result<()> {
        match self {
            Acc::CountStar(n) => *n += 1,
            Acc::Count(n) => {
                if !matches!(value, Value::Null) {
                    *n += 1;
                }
            }
            Acc::SumInt { sum, seen } => {
                if let Value::Int(v) = value {
                    *sum = sum
                        .checked_add(v)
                        .ok_or_else(|| overflow(ScalarType::Int64))?;
                    *seen = true;
                }
            }
            Acc::SumDecimal { sum, seen } => {
                if !matches!(value, Value::Null) {
                    *sum = sum.add(&to_decimal(value))?;
                    *seen = true;
                }
            }
            Acc::Avg { sum, count } => {
                if !matches!(value, Value::Null) {
                    *sum = sum.add(&to_decimal(value))?;
                    *count += 1;
                }
            }
            Acc::MinMax { cur, is_min } => {
                if !matches!(value, Value::Null) {
                    let next = match cur.take() {
                        None => value,
                        Some(c) => {
                            let ord = value_cmp(&c, &value);
                            let keep_current = if *is_min {
                                ord != std::cmp::Ordering::Greater
                            } else {
                                ord != std::cmp::Ordering::Less
                            };
                            if keep_current { c } else { value }
                        }
                    };
                    *cur = Some(next);
                }
            }
        }
        Ok(())
    }

    /// Produce the aggregate's final value over the group. COUNT → its count (0 over empty);
    /// SUM/MIN/MAX → NULL over an empty/all-NULL group; AVG → sum/count (NULL if count 0).
    fn finalize(self) -> Result<Value> {
        Ok(match self {
            Acc::CountStar(n) | Acc::Count(n) => Value::Int(n),
            Acc::SumInt { sum, seen } => {
                if seen {
                    Value::Int(sum)
                } else {
                    Value::Null
                }
            }
            Acc::SumDecimal { sum, seen } => {
                if seen {
                    Value::Decimal(sum)
                } else {
                    Value::Null
                }
            }
            Acc::Avg { sum, count } => {
                if count == 0 {
                    Value::Null
                } else {
                    Value::Decimal(sum.div(&Decimal::from_i64(count))?)
                }
            }
            Acc::MinMax { cur, .. } => cur.unwrap_or(Value::Null),
        })
    }
}

/// Whether any select item contains an aggregate call — i.e. this is an aggregate query.
fn items_have_aggregate(items: &SelectItems) -> bool {
    match items {
        SelectItems::All => false,
        SelectItems::Items(items) => items.iter().any(|it| expr_has_aggregate(&it.expr)),
    }
}

/// Whether an expression tree contains an AGGREGATE call anywhere. A scalar-function call is
/// not itself an aggregate, but may CONTAIN one (`abs(sum(x))`), so its arguments are walked.
fn expr_has_aggregate(e: &Expr) -> bool {
    match e {
        Expr::FuncCall { name, args, .. } => {
            is_aggregate_name(name) || args.iter().any(expr_has_aggregate)
        }
        Expr::Column(_) | Expr::QualifiedColumn { .. } | Expr::Literal(_) | Expr::Param(_) => false,
        Expr::Cast { inner, .. } => expr_has_aggregate(inner),
        Expr::Unary { operand, .. } => expr_has_aggregate(operand),
        Expr::IsNull { operand, .. } => expr_has_aggregate(operand),
        Expr::Binary { lhs, rhs, .. } | Expr::IsDistinctFrom { lhs, rhs, .. } => {
            expr_has_aggregate(lhs) || expr_has_aggregate(rhs)
        }
        Expr::In { lhs, list, .. } => {
            expr_has_aggregate(lhs) || list.iter().any(expr_has_aggregate)
        }
        Expr::Between { lhs, lo, hi, .. } => {
            expr_has_aggregate(lhs) || expr_has_aggregate(lo) || expr_has_aggregate(hi)
        }
        Expr::Like { lhs, rhs, .. } => expr_has_aggregate(lhs) || expr_has_aggregate(rhs),
        Expr::Case {
            operand,
            whens,
            els,
        } => {
            operand.as_deref().is_some_and(expr_has_aggregate)
                || whens
                    .iter()
                    .any(|(c, r)| expr_has_aggregate(c) || expr_has_aggregate(r))
                || els.as_deref().is_some_and(expr_has_aggregate)
        }
        // A subquery is an independent query: an aggregate INSIDE it does not make the OUTER query
        // an aggregate query (the outer reference, if any, is just a constant to the subquery).
        Expr::ScalarSubquery(_) | Expr::Exists(_) | Expr::InSubquery { .. } => false,
    }
}

/// The environment threaded into the per-row evaluator (spec/design/grammar.md §26): the
/// engine (to run a correlated subquery's plan), the bound parameters, and the stack of
/// enclosing rows (innermost LAST) a correlated reference reads. `outer` is empty at the top
/// level; a correlated subquery pushes the current row before running its inner plan, so an
/// `OuterColumn { level, index }` reads `outer[outer.len() - level][index]`.
struct EvalEnv<'a> {
    exec: &'a Database,
    params: &'a [Value],
    outer: &'a [&'a [Value]],
}

/// Build the constant `RExpr` for a folded uncorrelated-subquery value (§26). The static type
/// was settled at resolve, so a NULL value here is just `ConstNull`.
fn value_to_rexpr(v: &Value) -> RExpr {
    match v {
        Value::Null => RExpr::ConstNull,
        Value::Int(n) => RExpr::ConstInt(*n),
        Value::Bool(b) => RExpr::ConstBool(*b),
        Value::Text(s) => RExpr::ConstText(s.clone()),
        Value::Decimal(d) => RExpr::ConstDecimal(d.clone()),
        Value::Bytea(b) => RExpr::ConstBytea(b.clone()),
        Value::Uuid(u) => RExpr::ConstUuid(*u),
        Value::Timestamp(m) => RExpr::ConstTimestamp(*m),
        Value::Timestamptz(m) => RExpr::ConstTimestamptz(*m),
    }
}

/// Whether a resolved plan references any scope STRICTLY OUTSIDE itself — i.e. it is correlated
/// (spec/design/grammar.md §26). `depth` is how many nested-subquery frames we have descended
/// INTO this plan (0 = the plan's own clauses); an `OuterColumn { level }` points above this
/// plan iff `level > depth`. The `fold_uncorrelated` pass calls this with `depth = 0` on a
/// subquery's sub-plan to decide whether to fold it (uncorrelated) or leave it (correlated).
fn query_plan_references_outer(plan: &QueryPlan, depth: usize) -> bool {
    match plan {
        QueryPlan::Select(sp) => select_plan_references_outer(sp, depth),
        QueryPlan::SetOp(sop) => {
            query_plan_references_outer(&sop.lhs, depth)
                || query_plan_references_outer(&sop.rhs, depth)
        }
    }
}

fn select_plan_references_outer(sp: &SelectPlan, depth: usize) -> bool {
    sp.joins.iter().any(|j| {
        j.on.as_ref()
            .is_some_and(|on| rexpr_references_outer(on, depth))
    }) || sp
        .filter
        .as_ref()
        .is_some_and(|f| rexpr_references_outer(f, depth))
        || sp
            .having
            .as_ref()
            .is_some_and(|h| rexpr_references_outer(h, depth))
        || sp.agg_specs.iter().any(|s| {
            s.operand
                .as_ref()
                .is_some_and(|op| rexpr_references_outer(op, depth))
        })
        || sp
            .projections
            .iter()
            .any(|p| rexpr_references_outer(p, depth))
}

fn rexpr_references_outer(e: &RExpr, depth: usize) -> bool {
    match e {
        RExpr::OuterColumn { level, .. } => *level > depth,
        // A nested subquery's own clauses are one frame deeper; its IN lhs is at this frame.
        RExpr::Subquery { plan, lhs, .. } => {
            lhs.as_ref()
                .is_some_and(|l| rexpr_references_outer(l, depth))
                || query_plan_references_outer(plan, depth + 1)
        }
        RExpr::InValues { lhs, .. } => rexpr_references_outer(lhs, depth),
        RExpr::Cast { inner, .. } => rexpr_references_outer(inner, depth),
        RExpr::Neg { operand, .. } => rexpr_references_outer(operand, depth),
        RExpr::Not(x) => rexpr_references_outer(x, depth),
        RExpr::Arith { lhs, rhs, .. }
        | RExpr::Compare { lhs, rhs, .. }
        | RExpr::Distinct { lhs, rhs, .. }
        | RExpr::Like { lhs, rhs, .. } => {
            rexpr_references_outer(lhs, depth) || rexpr_references_outer(rhs, depth)
        }
        RExpr::And(l, r) | RExpr::Or(l, r) => {
            rexpr_references_outer(l, depth) || rexpr_references_outer(r, depth)
        }
        RExpr::IsNull { operand, .. } => rexpr_references_outer(operand, depth),
        RExpr::Case { arms, els, .. } => {
            arms.iter()
                .any(|(c, r)| rexpr_references_outer(c, depth) || rexpr_references_outer(r, depth))
                || rexpr_references_outer(els, depth)
        }
        RExpr::ScalarFunc { args, .. } => args.iter().any(|a| rexpr_references_outer(a, depth)),
        RExpr::Column(_)
        | RExpr::Param(_)
        | RExpr::ConstInt(_)
        | RExpr::ConstBool(_)
        | RExpr::ConstText(_)
        | RExpr::ConstDecimal(_)
        | RExpr::ConstBytea(_)
        | RExpr::ConstUuid(_)
        | RExpr::ConstTimestamp(_)
        | RExpr::ConstTimestamptz(_)
        | RExpr::ConstNull => false,
    }
}

/// Three-valued `lhs IN (list)` membership (spec/design/grammar.md §26), charging one
/// `operator_eval` per element compared. An EMPTY list is `negated` (`x IN ()` = FALSE,
/// `x NOT IN ()` = TRUE) independent of `lv`. Otherwise: a positive match → TRUE; else a NULL
/// element (or NULL `lv`) → NULL (unknown); else FALSE. `NOT IN` is the Kleene negation. Shared
/// by the folded `InValues` node and the correlated `Subquery { In }` eval.
fn in_membership(lv: &Value, list: &[Value], negated: bool, m: &mut Meter) -> Value {
    if list.is_empty() {
        return Value::Bool(negated);
    }
    let mut any_match = false;
    let mut any_null = false;
    for v in list {
        m.charge(COSTS.operator_eval);
        match lv.eq3(v) {
            ThreeValued::True => any_match = true,
            ThreeValued::Unknown => any_null = true,
            ThreeValued::False => {}
        }
    }
    let in_val = if any_match {
        Value::Bool(true)
    } else if any_null {
        Value::Null
    } else {
        Value::Bool(false)
    };
    if negated { not3(&in_val) } else { in_val }
}

/// Build a binary-operator `Expr` node (used by the IN/BETWEEN desugar in `resolve`).
fn binary_expr(op: BinaryOp, lhs: Expr, rhs: Expr) -> Expr {
    Expr::Binary {
        op,
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
    }
}

/// Parse an aggregate function name (case-insensitive); an unknown name is 42883.
fn parse_agg_func(name: &str) -> Result<AggFunc> {
    Ok(match name.to_ascii_lowercase().as_str() {
        "count" => AggFunc::Count,
        "sum" => AggFunc::Sum,
        "min" => AggFunc::Min,
        "max" => AggFunc::Max,
        "avg" => AggFunc::Avg,
        _ => {
            return Err(EngineError::new(
                SqlState::UndefinedFunction,
                format!("function does not exist: {name}"),
            ));
        }
    })
}

/// Resolve an aggregate call into a synthetic-row reference, collecting its `AggSpec`. Only
/// valid in `Collect` mode; in `Forbidden` mode (WHERE/ON/nested) it is 42803. The operand is
/// resolved in a fresh `Forbidden` sub-context (a nested aggregate is 42803; its columns
/// resolve against the real row). The result type follows the PG widening (aggregates.md §3).
fn resolve_aggregate(
    scope: &Scope,
    name: &str,
    args: &[Expr],
    star: bool,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    if matches!(agg, AggCtx::Forbidden) {
        return Err(EngineError::new(
            SqlState::GroupingError,
            "aggregate functions are not allowed here",
        ));
    }
    let func = parse_agg_func(name)?;
    let mut sub = AggCtx::Forbidden;
    let (plan, operand, result) = match func {
        AggFunc::Count if star => (
            AggPlan::CountStar,
            None,
            ResolvedType::Int(ScalarType::Int64),
        ),
        AggFunc::Count => {
            let (r, _t) = resolve(scope, expect_arg(args)?, None, &mut sub, params)?;
            (
                AggPlan::Count,
                Some(r),
                ResolvedType::Int(ScalarType::Int64),
            )
        }
        _ if star => {
            return Err(EngineError::new(
                SqlState::SyntaxError,
                "* is only valid as the argument of COUNT",
            ));
        }
        AggFunc::Sum => {
            let (r, t) = resolve(scope, expect_arg(args)?, None, &mut sub, params)?;
            match t {
                // int16/int32 -> int64 (accumulate i64); int64 -> decimal (PG widening).
                ResolvedType::Int(it) if it == ScalarType::Int64 => {
                    (AggPlan::SumDecimal, Some(r), ResolvedType::Decimal)
                }
                ResolvedType::Int(_) => (
                    AggPlan::SumInt,
                    Some(r),
                    ResolvedType::Int(ScalarType::Int64),
                ),
                ResolvedType::Decimal => (AggPlan::SumDecimal, Some(r), ResolvedType::Decimal),
                _ => return Err(no_agg_overload("sum")),
            }
        }
        AggFunc::Avg => {
            let (r, t) = resolve(scope, expect_arg(args)?, None, &mut sub, params)?;
            match t {
                ResolvedType::Int(_) | ResolvedType::Decimal => {
                    (AggPlan::Avg, Some(r), ResolvedType::Decimal)
                }
                _ => return Err(no_agg_overload("avg")),
            }
        }
        // MIN/MAX accept any ordered scalar; the result is the argument's type.
        AggFunc::Min => {
            let (r, t) = resolve(scope, expect_arg(args)?, None, &mut sub, params)?;
            (AggPlan::Min, Some(r), t)
        }
        AggFunc::Max => {
            let (r, t) = resolve(scope, expect_arg(args)?, None, &mut sub, params)?;
            (AggPlan::Max, Some(r), t)
        }
    };
    if let AggCtx::Collect { group_keys, specs } = agg {
        // Aggregate results follow the group-key values in the synthetic row.
        let slot = group_keys.len() + specs.len();
        specs.push(AggSpec { plan, operand });
        Ok((RExpr::Column(slot), result))
    } else {
        unreachable!("an aggregate in a non-Collect context is handled above")
    }
}

/// Resolve a column reference (already at real flat index `idx`) under an aggregate context.
/// In Forbidden mode it reads the real row directly; in Collect mode it must be a grouping key
/// — resolved to its synthetic-row slot (its position among the group keys) — else 42803.
fn collect_column(
    scope: &Scope,
    agg: &AggCtx,
    idx: usize,
    name: &str,
) -> Result<(RExpr, ResolvedType)> {
    let ty = resolved_type_of(scope.column_at(idx).ty);
    match agg {
        AggCtx::Forbidden => Ok((RExpr::Column(idx), ty)),
        AggCtx::Collect { group_keys, .. } => match group_keys.iter().position(|&gk| gk == idx) {
            Some(pos) => Ok((RExpr::Column(pos), ty)),
            None => Err(grouping_error_column(name)),
        },
    }
}

/// The single argument of a non-star aggregate call. Each aggregate takes exactly one
/// argument; a different count matches no aggregate overload and is 42883 (PG).
fn expect_arg(args: &[Expr]) -> Result<&Expr> {
    match args {
        [a] => Ok(a),
        _ => Err(EngineError::new(
            SqlState::UndefinedFunction,
            "no aggregate function matches the given argument count",
        )),
    }
}

/// An aggregate over an operand family it has no overload for (e.g. SUM(text)) — 42883.
fn no_agg_overload(func: &str) -> EngineError {
    EngineError::new(
        SqlState::UndefinedFunction,
        format!("no {func} aggregate for that argument type"),
    )
}

/// Whether `name` (case-insensitive) is one of the five aggregate functions.
fn is_aggregate_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "count" | "sum" | "min" | "max" | "avg"
    )
}

/// A scalar function over argument types it has no overload for (e.g. abs(text), round(int,
/// text)) — 42883, like an aggregate with no matching overload.
fn no_func_overload(func: &str) -> EngineError {
    EngineError::new(
        SqlState::UndefinedFunction,
        format!("no {func} function for those argument types"),
    )
}

/// Resolve a function call: an aggregate (COUNT/SUM/MIN/MAX/AVG), a scalar function
/// (abs/round, spec/design/functions.md §9), or 42883 (undefined_function) for any other name.
/// Aggregates and scalar functions share the call syntax (grammar.md §17); they are
/// distinguished here, at resolve.
fn resolve_func_call(
    scope: &Scope,
    name: &str,
    args: &[Expr],
    star: bool,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    let lname = name.to_ascii_lowercase();
    match lname.as_str() {
        "count" | "sum" | "min" | "max" | "avg" => {
            resolve_aggregate(scope, &lname, args, star, agg, params)
        }
        "abs" | "round" => resolve_scalar_func(scope, &lname, args, star, agg, params),
        _ => Err(EngineError::new(
            SqlState::UndefinedFunction,
            format!("function does not exist: {name}"),
        )),
    }
}

/// Resolve a scalar-function call (abs/round) into a per-row `ScalarFunc` node. Unlike an
/// aggregate it is legal in any context, so its arguments resolve in the SAME `agg` context
/// (a nested aggregate is still collected in a projection and 42803 in WHERE). The overload is
/// picked by the argument families; no match is 42883. spec/design/functions.md §9.
fn resolve_scalar_func(
    scope: &Scope,
    name: &str, // already lowercased
    args: &[Expr],
    star: bool,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    if star {
        return Err(EngineError::new(
            SqlState::SyntaxError,
            "* is only valid as the argument of COUNT",
        ));
    }
    let mut rargs = Vec::with_capacity(args.len());
    let mut tys = Vec::with_capacity(args.len());
    for a in args {
        let (r, t) = resolve(scope, a, None, agg, params)?;
        rargs.push(r);
        tys.push(t);
    }
    let result = match (name, tys.as_slice()) {
        // abs: result is the operand's own type (range-checked at its boundary for integers).
        ("abs", [ResolvedType::Int(it)]) => *it,
        ("abs", [ResolvedType::Decimal]) => ScalarType::Decimal,
        // round: always decimal; integer overloads return numeric (PG round(5)).
        ("round", [ResolvedType::Decimal])
        | ("round", [ResolvedType::Decimal, ResolvedType::Int(_)])
        | ("round", [ResolvedType::Int(_)])
        | ("round", [ResolvedType::Int(_), ResolvedType::Int(_)]) => ScalarType::Decimal,
        _ => return Err(no_func_overload(name)),
    };
    let func = match name {
        "abs" => ScalarFunc::Abs,
        _ => ScalarFunc::Round,
    };
    Ok((
        RExpr::ScalarFunc {
            func,
            args: rargs,
            result,
        },
        resolved_type_of(result),
    ))
}

/// The 42803 raised for a non-aggregated column outside an aggregate with no GROUP BY.
fn grouping_error_column(name: &str) -> EngineError {
    EngineError::new(
        SqlState::GroupingError,
        format!(
            "column {name} must appear in the GROUP BY clause or be used in an aggregate function"
        ),
    )
}

/// Resolve `SELECT` items against the FROM scope into evaluable projections (any result type
/// is allowed in the select list, including boolean — `SELECT a = b`), each paired with its
/// output column name (spec/design/grammar.md §8). `*` expands across ALL relations in FROM
/// order, each relation's columns in catalog order (§15).
fn resolve_projections(
    scope: &Scope,
    items: &SelectItems,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(Vec<RExpr>, Vec<String>, Vec<ResolvedType>)> {
    match items {
        SelectItems::All => {
            let mut nodes = Vec::new();
            let mut names = Vec::new();
            let mut types = Vec::new();
            for rel in &scope.rels {
                for (i, c) in rel.table.columns.iter().enumerate() {
                    nodes.push(RExpr::Column(rel.offset + i));
                    names.push(c.name.clone());
                    types.push(ResolvedType::of_column(c.ty));
                }
            }
            Ok((nodes, names, types))
        }
        SelectItems::Items(items) => {
            let mut nodes = Vec::with_capacity(items.len());
            let mut names = Vec::with_capacity(items.len());
            let mut types = Vec::with_capacity(items.len());
            for it in items {
                let (node, ty) = resolve(scope, &it.expr, None, agg, params)?;
                names.push(match &it.alias {
                    Some(a) => a.clone(),
                    None => output_name(scope, &it.expr),
                });
                nodes.push(node);
                types.push(ty);
            }
            Ok((nodes, names, types))
        }
    }
}

/// The output column name of an un-aliased select item (spec/design/grammar.md §8/§15): a
/// bare or qualified column reference takes the catalog's canonical name (the `CREATE TABLE`
/// spelling, not the SELECT spelling, and never the qualifier — so casing/qualifier never
/// leaks); every other expression takes the fixed `?column?`. The column is known to exist —
/// `resolve` validated it.
fn output_name(scope: &Scope, e: &Expr) -> String {
    match e {
        // A bare/qualified column takes the catalog's canonical name, whether it resolves to a
        // local relation or (correlated) an enclosing one — `column_of` handles both.
        Expr::Column(name) => match scope.resolve_bare(name) {
            Ok(r) => scope.column_of(r).name.clone(),
            Err(_) => name.clone(),
        },
        Expr::QualifiedColumn { qualifier, name } => match scope.resolve_qualified(qualifier, name)
        {
            Ok(r) => scope.column_of(r).name.clone(),
            Err(_) => name.clone(),
        },
        // An un-aliased aggregate call is named by its lowercased function name (PG;
        // spec/design/grammar.md §8). Any other expression takes the fixed `?column?`.
        Expr::FuncCall { name, .. } => name.to_ascii_lowercase(),
        _ => "?column?".to_string(),
    }
}

/// Resolve a WHERE / ON expression: it must resolve to boolean (or an untyped NULL, which
/// is always unknown → no rows). An integer-valued WHERE/ON is a 42804 type error.
fn resolve_boolean_filter(scope: &Scope, e: &Expr, params: &mut ParamTypes) -> Result<RExpr> {
    // WHERE / ON filters run before any grouping, so an aggregate here is 42803 (Forbidden).
    let mut agg = AggCtx::Forbidden;
    let (node, ty) = resolve(scope, e, None, &mut agg, params)?;
    match ty {
        ResolvedType::Bool | ResolvedType::Null => Ok(node),
        ResolvedType::Int(_)
        | ResolvedType::Text
        | ResolvedType::Decimal
        | ResolvedType::Bytea
        | ResolvedType::Uuid
        | ResolvedType::Timestamp
        | ResolvedType::Timestamptz => Err(type_error("argument of WHERE must be boolean")),
    }
}

/// Per-statement accumulator of bind-parameter types, inferred from context during resolve
/// (spec/design/api.md §5). `types[i]` is the inferred scalar type of `$(i+1)`; `None` marks a
/// parameter referenced before any context fixed its type. Shared across every clause of a
/// statement (so a `$1` used in both WHERE and the select list unifies), then `finalize`d.
#[derive(Default)]
struct ParamTypes {
    types: Vec<Option<ScalarType>>,
}

impl ParamTypes {
    /// Record that `$(idx0+1)` appears with context type `ty` (`None` = no context here).
    /// Unifies with any prior inference for the same index: equal types agree, two integer
    /// widths widen to the wider, an incompatible concrete pair is 42804.
    fn note(&mut self, idx0: usize, ty: Option<ScalarType>) -> Result<()> {
        if idx0 >= self.types.len() {
            self.types.resize(idx0 + 1, None);
        }
        if let Some(new) = ty {
            self.types[idx0] = Some(match self.types[idx0] {
                None => new,
                Some(old) => unify_param_type(old, new, idx0)?,
            });
        }
        Ok(())
    }

    /// Finalize to the ordered parameter types. A slot referenced but never typed — including a
    /// gap in `$1..$N` — is 42P18 indeterminate_datatype.
    fn finalize(self) -> Result<Vec<ScalarType>> {
        let mut out = Vec::with_capacity(self.types.len());
        for (i, t) in self.types.into_iter().enumerate() {
            match t {
                Some(ty) => out.push(ty),
                None => {
                    return Err(EngineError::new(
                        SqlState::IndeterminateDatatype,
                        format!("could not determine data type of parameter ${}", i + 1),
                    ));
                }
            }
        }
        Ok(out)
    }
}

/// Unify two inferred types for the same bind parameter: equal agrees; two integer widths
/// widen to the wider (so `$1` works against both an int16 and an int32 column); any other
/// mismatch is 42804 (spec/design/api.md §5).
fn unify_param_type(a: ScalarType, b: ScalarType, idx0: usize) -> Result<ScalarType> {
    if a == b {
        return Ok(a);
    }
    if a.is_integer() && b.is_integer() {
        return Ok(if a.rank() >= b.rank() { a } else { b });
    }
    Err(EngineError::new(
        SqlState::DatatypeMismatch,
        format!("inconsistent types inferred for parameter ${}", idx0 + 1),
    ))
}

/// Coerce each supplied bind value to its inferred parameter type, two-phase / all-or-nothing
/// like INSERT (spec/design/api.md §5): a count mismatch is 42601 and every value is validated
/// up front (22003/42804/22P02/23502 via `store_value`) before any row is touched.
fn bind_params(supplied: &[Value], types: &[ScalarType]) -> Result<Vec<Value>> {
    if supplied.len() != types.len() {
        return Err(EngineError::new(
            SqlState::SyntaxError,
            format!(
                "bind parameter count mismatch: statement expects {}, got {}",
                types.len(),
                supplied.len()
            ),
        ));
    }
    let mut bound = Vec::with_capacity(types.len());
    for (i, (v, ty)) in supplied.iter().zip(types).enumerate() {
        // A bound parameter is coerced exactly like a literal in that position: typmod is
        // unconstrained (a comparison/insert against a column re-applies the column typmod),
        // not_null is false (NULL is a legal bound value; a NOT NULL target re-checks at store).
        bound.push(store_value(
            v.clone(),
            *ty,
            None,
            false,
            &format!("${}", i + 1),
        )?);
    }
    Ok(bound)
}

/// A DDL statement (CREATE/DROP TABLE) has no expressions and so takes no bind parameters;
/// supplying any is a 42601 (spec/design/api.md §5).
fn reject_params_for_ddl(params: &[Value]) -> Result<()> {
    if params.is_empty() {
        Ok(())
    } else {
        Err(EngineError::new(
            SqlState::SyntaxError,
            "bind parameters are not allowed in a DDL statement",
        ))
    }
}

/// Whether a statement mutates the database (so autocommit must capture + durably persist it,
/// and a READ ONLY transaction must reject it — spec/design/transactions.md §4.1/§4.3). Reads
/// (`SELECT`, set operations) and transaction control run against the committed state / handle
/// state with no data mutation.
pub(crate) fn stmt_is_write(stmt: &Statement) -> bool {
    matches!(
        stmt,
        Statement::CreateTable(_)
            | Statement::DropTable(_)
            | Statement::Insert(_)
            | Statement::Update(_)
            | Statement::Delete(_)
    )
}

/// A short label for a statement kind, for the 25006 read-only-violation message (the message
/// text is informational — never matched; spec/design/conformance.md §2).
fn stmt_kind(stmt: &Statement) -> &'static str {
    match stmt {
        Statement::CreateTable(_) => "CREATE TABLE",
        Statement::DropTable(_) => "DROP TABLE",
        Statement::Insert(_) => "INSERT",
        Statement::Update(_) => "UPDATE",
        Statement::Delete(_) => "DELETE",
        Statement::Select(_) | Statement::SetOp(_) => "SELECT",
        Statement::Begin { .. } => "BEGIN",
        Statement::Commit => "COMMIT",
        Statement::Rollback => "ROLLBACK",
    }
}

/// The resolved (static) type of a column of scalar type `ty`.
fn resolved_type_of(ty: ScalarType) -> ResolvedType {
    if ty.is_text() {
        ResolvedType::Text
    } else if ty.is_bool() {
        ResolvedType::Bool
    } else if ty.is_decimal() {
        ResolvedType::Decimal
    } else if ty.is_bytea() {
        ResolvedType::Bytea
    } else if ty.is_uuid() {
        ResolvedType::Uuid
    } else if ty.is_timestamp() {
        ResolvedType::Timestamp
    } else if ty.is_timestamptz() {
        ResolvedType::Timestamptz
    } else {
        ResolvedType::Int(ty)
    }
}

/// Resolve one `Expr` into an `RExpr` plus its static type, against the FROM `scope`. `ctx`
/// is the type an untyped integer literal should adapt to (spec/design/types.md §6); `None`
/// defaults a bare literal to int64. A column reference resolves to a flat row index via the
/// scope — a bare name ambiguous across relations is 42702, an unknown qualifier is 42P01
/// (spec/design/grammar.md §15).
/// Turn a chain resolution into a resolved node + type. A `Local` column obeys the grouping
/// rule (a synthetic-slot reference in an aggregate projection, else 42803). An `Outer`
/// (correlated) reference is a per-outer-row CONSTANT, so it bypasses the grouping rule and
/// resolves to an `OuterColumn` reading the enclosing row at eval; its type is the ancestor
/// column's (spec/design/grammar.md §26).
fn resolve_column_ref(
    scope: &Scope,
    agg: &AggCtx,
    r: Resolved,
    name: &str,
) -> Result<(RExpr, ResolvedType)> {
    match r {
        Resolved::Local(idx) => collect_column(scope, agg, idx, name),
        Resolved::Outer { level, index } => {
            let ty = resolved_type_of(scope.column_of(r).ty);
            Ok((RExpr::OuterColumn { level, index }, ty))
        }
    }
}

/// Plan a subquery operand against the scope chain (spec/design/grammar.md §26). Rejects a
/// non-SELECT context (UPDATE/DELETE/INSERT — `allow_subquery=false`) with 0A000. A `$N` inside
/// the subquery is allowed: the shared `params` table is threaded into the inner plan, so a
/// parameter typed by an inner context (`WHERE inner.col = $1`) infers statement-wide and is
/// unified with any outer use of the same `$N`. A parameter with **no** type context anywhere
/// stays uninferred and `finalize` raises 42P18 (a documented divergence from PostgreSQL, which
/// defaults such a `$N` to text — grammar.md §26). The inner query is resolved ONCE, with `scope`
/// as its parent, so correlated references become `OuterColumn` and errors fire even over an
/// empty outer.
fn plan_subquery(scope: &Scope, inner: &QueryExpr, params: &mut ParamTypes) -> Result<QueryPlan> {
    if !scope.allow_subquery {
        return Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "subqueries are only supported in a SELECT statement",
        ));
    }
    scope.catalog.plan_query(inner, Some(scope), params)
}

fn resolve(
    scope: &Scope,
    e: &Expr,
    ctx: Option<ScalarType>,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    match e {
        Expr::Column(name) => {
            // Resolve against the scope CHAIN (§26). Existence first (42703/42702 take priority,
            // matching PostgreSQL); a Local match then obeys the grouping rule, an Outer
            // (correlated) match is a per-outer-row constant exempt from it (see helper).
            let r = scope.resolve_bare(name)?;
            resolve_column_ref(scope, agg, r, name)
        }
        Expr::QualifiedColumn { qualifier, name } => {
            let r = scope.resolve_qualified(qualifier, name)?;
            resolve_column_ref(scope, agg, r, name)
        }
        Expr::Param(n1) => {
            // A bind parameter is an adaptable operand (like an integer/string literal): it
            // takes its type from `ctx` — the sibling operand, target column, or CAST target.
            // Record the inferred type (None = no context here; `finalize` 42P18s a parameter
            // that never gets one). spec/design/api.md §5.
            let idx0 = (*n1 as usize) - 1;
            params.note(idx0, ctx)?;
            let rty = match ctx {
                Some(t) => resolved_type_of(t),
                None => ResolvedType::Null,
            };
            Ok((RExpr::Param(idx0), rty))
        }
        Expr::FuncCall { name, args, star } => {
            resolve_func_call(scope, name, args, *star, agg, params)
        }
        Expr::Literal(Literal::Null) => Ok((RExpr::ConstNull, ResolvedType::Null)),
        Expr::Literal(Literal::Bool(b)) => Ok((RExpr::ConstBool(*b), ResolvedType::Bool)),
        Expr::Literal(Literal::Int(n)) => {
            // An integer literal adapts only to an *integer* context; a non-integer context
            // (a text/decimal column or assignment target) does not apply — it defaults to
            // int64, and the surrounding check then reports the family mismatch (42804) or
            // widens it (int→decimal), never panics on a non-integer range.
            let ty = match ctx {
                Some(t) if t.is_integer() => t,
                _ => ScalarType::Int64,
            };
            if !ty.in_range(*n) {
                return Err(overflow(ty));
            }
            Ok((RExpr::ConstInt(*n), ResolvedType::Int(ty)))
        }
        Expr::Literal(Literal::Text(s)) => {
            // A string literal is text by default (collation `C`). It adapts to a BYTEA context
            // (decode the hex input, 22P02), a UUID context (PG-flexible uuid input, 22P02 —
            // types.md §6/§13/§14), or a TIMESTAMP/TIMESTAMPTZ context (parse the datetime,
            // 22007/22008 — spec/design/timestamp.md). Any other context keeps it text.
            match ctx {
                Some(t) if t.is_bytea() => Ok((
                    RExpr::ConstBytea(decode_bytea_literal(s)?),
                    ResolvedType::Bytea,
                )),
                Some(t) if t.is_uuid() => Ok((
                    RExpr::ConstUuid(decode_uuid_literal(s)?),
                    ResolvedType::Uuid,
                )),
                Some(t) if t.is_timestamp() => Ok((
                    RExpr::ConstTimestamp(parse_timestamp(s)?),
                    ResolvedType::Timestamp,
                )),
                Some(t) if t.is_timestamptz() => Ok((
                    RExpr::ConstTimestamptz(parse_timestamptz(s)?),
                    ResolvedType::Timestamptz,
                )),
                _ => Ok((RExpr::ConstText(s.clone()), ResolvedType::Text)),
            }
        }
        Expr::Literal(Literal::Decimal(d)) => {
            // A decimal literal is always decimal; it does not adapt to context (like text).
            // Cap-check it here (an over-long coefficient/scale traps 22003 at resolve —
            // spec/design/decimal.md §6).
            let d = d.clone().check_cap()?;
            Ok((RExpr::ConstDecimal(d), ResolvedType::Decimal))
        }
        // A subquery in expression position (spec/design/grammar.md §26): PLANNED ONCE against the
        // scope chain here, so its column-count / type errors fire even over an empty outer.
        // `plan_subquery` rejects a non-SELECT context and a `$N` inside (both 0A000). The fold
        // pass folds an uncorrelated one to a constant; a correlated one (an OuterColumn in its
        // plan) is re-executed per outer row by the evaluator.
        Expr::ScalarSubquery(inner) => {
            let plan = plan_subquery(scope, inner, params)?;
            if plan.column_types().len() != 1 {
                return Err(EngineError::new(
                    SqlState::SyntaxError,
                    "subquery must return only one column",
                ));
            }
            let out_type = plan.column_types()[0];
            Ok((
                RExpr::Subquery {
                    plan: Box::new(plan),
                    kind: SubqueryKind::Scalar,
                    lhs: None,
                    negated: false,
                },
                out_type,
            ))
        }
        Expr::Exists(inner) => {
            // EXISTS ignores the select list entirely; the result is boolean, never NULL. A NOT
            // EXISTS parses as the unary `NOT` wrapping this, so `negated` here is always false.
            let plan = plan_subquery(scope, inner, params)?;
            Ok((
                RExpr::Subquery {
                    plan: Box::new(plan),
                    kind: SubqueryKind::Exists,
                    lhs: None,
                    negated: false,
                },
                ResolvedType::Bool,
            ))
        }
        Expr::InSubquery {
            lhs,
            query,
            negated,
        } => {
            // The LHS is an OUTER expression (resolved in the current scope / agg context); the
            // subquery yields the single membership column. The test is `lhs = element`, so the
            // pair must be comparable (42804), exactly like a literal IN.
            let (rlhs, lt) = resolve(scope, lhs, None, agg, params)?;
            let plan = plan_subquery(scope, query, params)?;
            if plan.column_types().len() != 1 {
                return Err(EngineError::new(
                    SqlState::SyntaxError,
                    "subquery has too many columns",
                ));
            }
            classify_comparable(lt, plan.column_types()[0])?;
            Ok((
                RExpr::Subquery {
                    plan: Box::new(plan),
                    kind: SubqueryKind::In,
                    lhs: Some(Box::new(rlhs)),
                    negated: *negated,
                },
                ResolvedType::Bool,
            ))
        }
        Expr::Cast {
            inner,
            type_name,
            type_mod,
        } => {
            let (target, typmod) = resolve_type_and_typmod(type_name, type_mod)?;
            // Text casts are deferred (not in the cast matrix — spec/design/types.md §5/§11):
            // casting TO text is a 0A000 this slice.
            if target.is_text() {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "casting to text is not supported yet",
                ));
            }
            // Boolean casts are likewise deferred (boolean⇄integer is a later cast slice —
            // spec/types/casts.toml): casting TO boolean is a 0A000 this slice. Without this
            // guard `resolve_type_and_typmod` now returns boolean, so it must be caught here.
            if target.is_bool() {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "casting to boolean is not supported yet",
                ));
            }
            // bytea casts are likewise deferred (types.md §5/§13): casting TO bytea is 0A000.
            if target.is_bytea() {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "casting to bytea is not supported yet",
                ));
            }
            // uuid casts are likewise deferred (types.md §5/§14): casting TO uuid is 0A000.
            if target.is_uuid() {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "casting to uuid is not supported yet",
                ));
            }
            // timestamp casts are deferred (spec/design/timestamp.md §6): casting TO a datetime
            // is 0A000 (a string lands in a timestamp column by literal adaptation, not a CAST).
            if target.is_timestamp() || target.is_timestamptz() {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "casting to a timestamp type is not supported yet",
                ));
            }
            // The inner value is range-checked / coerced against `target` at eval, so it
            // resolves with no literal context here.
            let (rinner, ity) = resolve(scope, inner, None, agg, params)?;
            match ity {
                // int→int (range check), int→decimal (widen), decimal→int (explicit, round),
                // decimal→decimal (re-scale), and NULL are all castable.
                ResolvedType::Int(_) | ResolvedType::Decimal | ResolvedType::Null => {}
                ResolvedType::Bool => {
                    return Err(type_error(format!(
                        "cannot cast boolean to {}",
                        target.canonical_name()
                    )));
                }
                // Casting FROM text is likewise deferred (0A000).
                ResolvedType::Text => {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "casting from text is not supported yet",
                    ));
                }
                // Casting FROM bytea is likewise deferred (0A000).
                ResolvedType::Bytea => {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "casting from bytea is not supported yet",
                    ));
                }
                // Casting FROM uuid is likewise deferred (0A000).
                ResolvedType::Uuid => {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "casting from uuid is not supported yet",
                    ));
                }
                // Casting FROM a timestamp is likewise deferred (0A000).
                ResolvedType::Timestamp | ResolvedType::Timestamptz => {
                    return Err(EngineError::new(
                        SqlState::FeatureNotSupported,
                        "casting from a timestamp type is not supported yet",
                    ));
                }
            }
            let result_ty = if target.is_decimal() {
                ResolvedType::Decimal
            } else {
                ResolvedType::Int(target)
            };
            Ok((
                RExpr::Cast {
                    inner: Box::new(rinner),
                    target,
                    typmod,
                },
                result_ty,
            ))
        }
        Expr::Unary {
            op: UnaryOp::Neg,
            operand,
        } => {
            let (rop, ty) = resolve(scope, operand, ctx, agg, params)?;
            let result = match ty {
                ResolvedType::Int(t) => t,
                ResolvedType::Decimal => ScalarType::Decimal,
                ResolvedType::Null => ScalarType::Int64, // -NULL = NULL
                ResolvedType::Bool
                | ResolvedType::Text
                | ResolvedType::Bytea
                | ResolvedType::Uuid
                | ResolvedType::Timestamp
                | ResolvedType::Timestamptz => {
                    return Err(type_error("unary minus requires a numeric operand"));
                }
            };
            let rty = if result.is_decimal() {
                ResolvedType::Decimal
            } else {
                ResolvedType::Int(result)
            };
            Ok((
                RExpr::Neg {
                    operand: Box::new(rop),
                    result,
                },
                rty,
            ))
        }
        Expr::Unary {
            op: UnaryOp::Not,
            operand,
        } => {
            let (rop, ty) = resolve(scope, operand, None, agg, params)?;
            require_bool(ty, "NOT requires a boolean operand")?;
            Ok((RExpr::Not(Box::new(rop)), ResolvedType::Bool))
        }
        Expr::IsNull { operand, negated } => {
            // IS [NOT] NULL accepts any operand type and always yields a definite boolean.
            let (rop, _ty) = resolve(scope, operand, None, agg, params)?;
            Ok((
                RExpr::IsNull {
                    operand: Box::new(rop),
                    negated: *negated,
                },
                ResolvedType::Bool,
            ))
        }
        Expr::IsDistinctFrom { lhs, rhs, negated } => {
            // NULL-safe equality: the SAME operand contract as `=` — resolve the pair
            // (a literal adapts to its sibling; a text literal stays text), then require
            // the operands be comparable (both integer-ish or both text-ish; a mixed pair
            // is 42804). The result is always a definite boolean (functions.md §3).
            let (rl, lt, rr, rt) = resolve_operand_pair(scope, lhs, rhs, agg, params)?;
            classify_comparable(lt, rt)?;
            Ok((
                RExpr::Distinct {
                    lhs: Box::new(rl),
                    rhs: Box::new(rr),
                    negated: *negated,
                },
                ResolvedType::Bool,
            ))
        }
        Expr::Binary { op, lhs, rhs } => resolve_binary(scope, *op, lhs, rhs, agg, params),
        Expr::In { lhs, list, negated } => {
            // An EMPTY list reaches here only from folding an IN-subquery whose result was empty
            // (grammar.md §26; the parser rejects literal `IN ()` → 42601). The value is a constant
            // — `x IN (empty)` = FALSE, `x NOT IN (empty)` = TRUE — for every x including NULL.
            // Still resolve the LHS so an undefined column / aggregate-context error fires, then
            // return the constant (a leaf — no operator_eval, cost.md §3).
            if list.is_empty() {
                let _ = resolve(scope, lhs, None, agg, params)?;
                return Ok((RExpr::ConstBool(*negated), ResolvedType::Bool));
            }
            // Desugar to the OR-chain PostgreSQL DEFINES `IN` as: `x IN (a,b,c)` ≡
            // `x = a OR x = b OR x = c`; `NOT IN` is its negation (grammar.md §20). The list
            // is non-empty (the parser rejects `IN ()` → 42601). Resolving the desugared tree
            // reuses the `=`/OR/NOT machinery verbatim, so the three-valued NULL semantics,
            // per-element operand typing (a too-wide literal → 22003, a cross-family element →
            // 42804), and cost all fall out. The LHS is evaluated once per element (the
            // OR-chain model — a documented cost consequence, cost.md §3).
            let mut folded: Option<Expr> = None;
            for elem in list {
                let eq = binary_expr(BinaryOp::Eq, (**lhs).clone(), elem.clone());
                folded = Some(match folded {
                    None => eq,
                    Some(acc) => binary_expr(BinaryOp::Or, acc, eq),
                });
            }
            let mut desugared = folded.expect("IN list is non-empty (parser guarantees ≥1)");
            if *negated {
                desugared = Expr::Unary {
                    op: UnaryOp::Not,
                    operand: Box::new(desugared),
                };
            }
            resolve(scope, &desugared, ctx, agg, params)
        }
        Expr::Between {
            lhs,
            lo,
            hi,
            negated,
        } => {
            // Desugar to `lhs >= lo AND lhs <= hi` (grammar.md §21). The Kleene AND gives the PG
            // result for a NULL bound: `5 BETWEEN 10 AND NULL` is `FALSE AND NULL` = FALSE (a
            // FALSE operand dominates), while `5 BETWEEN 1 AND NULL` is `TRUE AND NULL` = NULL.
            // `NOT BETWEEN` negates the whole conjunction. The LHS is evaluated twice (the
            // desugar model — a documented cost consequence, cost.md §3).
            let ge = binary_expr(BinaryOp::Ge, (**lhs).clone(), (**lo).clone());
            let le = binary_expr(BinaryOp::Le, (**lhs).clone(), (**hi).clone());
            let mut desugared = binary_expr(BinaryOp::And, ge, le);
            if *negated {
                desugared = Expr::Unary {
                    op: UnaryOp::Not,
                    operand: Box::new(desugared),
                };
            }
            resolve(scope, &desugared, ctx, agg, params)
        }
        Expr::Like { lhs, rhs, negated } => {
            // LIKE is text×text → boolean (grammar.md §22). Resolve the pair (a string literal
            // stays text), then require BOTH operands be text (or a bare NULL); a non-text
            // operand is 42804. We do NOT use classify_comparable here — it would wrongly accept
            // bytea×bytea, which LIKE does not define.
            let (rl, lt, rr, rt) = resolve_operand_pair(scope, lhs, rhs, agg, params)?;
            require_text_or_null(lt)?;
            require_text_or_null(rt)?;
            Ok((
                RExpr::Like {
                    lhs: Box::new(rl),
                    rhs: Box::new(rr),
                    negated: *negated,
                },
                ResolvedType::Bool,
            ))
        }
        Expr::Case {
            operand,
            whens,
            els,
        } => {
            // Resolve each branch's condition: searched form requires a boolean WHEN (42804
            // otherwise); simple form desugars to `operand = value` (reusing the `=` operand
            // pairing + comparability check, so the value adapts to the operand's type). The
            // operand is evaluated once per tested branch (the desugar model, like IN).
            let mut arms: Vec<(RExpr, RExpr)> = Vec::with_capacity(whens.len());
            let mut result_types: Vec<ResolvedType> = Vec::with_capacity(whens.len() + 1);
            for (cond, res) in whens {
                let rcond = match operand {
                    Some(op) => {
                        let eq = binary_expr(BinaryOp::Eq, (**op).clone(), cond.clone());
                        resolve(scope, &eq, None, agg, params)?.0
                    }
                    None => {
                        let (rc, cty) = resolve(scope, cond, None, agg, params)?;
                        require_bool(cty, "CASE WHEN condition must be boolean")?;
                        rc
                    }
                };
                let (rres, rty) = resolve(scope, res, None, agg, params)?;
                result_types.push(rty);
                arms.push((rcond, rres));
            }
            let (rels, ety) = match els {
                Some(e) => resolve(scope, e, None, agg, params)?,
                None => (RExpr::ConstNull, ResolvedType::Null),
            };
            result_types.push(ety);
            // Unify the THEN/ELSE result types into the CASE's common type (the render type).
            let unified = unify_case_types(&result_types)?;
            Ok((
                RExpr::Case {
                    arms,
                    els: Box::new(rels),
                    coerce_decimal: unified == ResolvedType::Decimal,
                },
                unified,
            ))
        }
    }
}

fn resolve_binary(
    scope: &Scope,
    op: BinaryOp,
    lhs: &Expr,
    rhs: &Expr,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    match op {
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => {
            // Arithmetic is overloaded across integer and decimal. Resolve the operand pair
            // (an integer literal adapts to an integer sibling), then pick the family: both
            // integer → integer arithmetic (promotion tower); at least one decimal → decimal
            // arithmetic (the integer operand widens at eval); a text/boolean operand is a
            // 42804 (spec/design/decimal.md §4).
            let (rl, lt, rr, rt) = resolve_operand_pair(scope, lhs, rhs, agg, params)?;
            require_numeric_operand(lt)?;
            require_numeric_operand(rt)?;
            let aop = match op {
                BinaryOp::Add => ArithOp::Add,
                BinaryOp::Sub => ArithOp::Sub,
                BinaryOp::Mul => ArithOp::Mul,
                BinaryOp::Div => ArithOp::Div,
                BinaryOp::Mod => ArithOp::Mod,
                _ => unreachable!(),
            };
            let (result, rty) = if lt == ResolvedType::Decimal || rt == ResolvedType::Decimal {
                (ScalarType::Decimal, ResolvedType::Decimal)
            } else {
                let p = promote(lt, rt);
                (p, ResolvedType::Int(p))
            };
            Ok((
                RExpr::Arith {
                    op: aop,
                    lhs: Box::new(rl),
                    rhs: Box::new(rr),
                    result,
                },
                rty,
            ))
        }
        BinaryOp::Eq | BinaryOp::Lt | BinaryOp::Gt | BinaryOp::Le | BinaryOp::Ge => {
            // Comparison is overloaded across families: integer×integer or text×text.
            // Resolve the operands (a literal adapts to its sibling; text literals stay
            // text), then require they be comparable — a mixed integer/text pair is 42804.
            // The runtime comparison (eq3/lt3/gt3) dispatches on the value variants.
            let (rl, lt, rr, rt) = resolve_operand_pair(scope, lhs, rhs, agg, params)?;
            classify_comparable(lt, rt)?;
            let cop = match op {
                BinaryOp::Eq => CmpOp::Eq,
                BinaryOp::Lt => CmpOp::Lt,
                BinaryOp::Gt => CmpOp::Gt,
                BinaryOp::Le => CmpOp::Le,
                BinaryOp::Ge => CmpOp::Ge,
                _ => unreachable!(),
            };
            Ok((
                RExpr::Compare {
                    op: cop,
                    lhs: Box::new(rl),
                    rhs: Box::new(rr),
                },
                ResolvedType::Bool,
            ))
        }
        BinaryOp::And | BinaryOp::Or => {
            let (rl, lt) = resolve(scope, lhs, None, agg, params)?;
            let (rr, rt) = resolve(scope, rhs, None, agg, params)?;
            require_bool(lt, "AND/OR requires boolean operands")?;
            require_bool(rt, "AND/OR requires boolean operands")?;
            let node = if matches!(op, BinaryOp::And) {
                RExpr::And(Box::new(rl), Box::new(rr))
            } else {
                RExpr::Or(Box::new(rl), Box::new(rr))
            };
            Ok((node, ResolvedType::Bool))
        }
    }
}

/// Resolve the two operands of a binary operator, giving each adaptable literal the other
/// operand's type as context: a bare *integer* literal adopts the sibling's integer type (so
/// `small + 1` types `1` as int16, and `small + 100000` traps 22003 at resolve), and a
/// *string* literal adapts to a bytea sibling (decoding the hex input — types.md §6/§13),
/// otherwise staying text. When the sibling offers no usable context, the literal defaults to
/// its own family and the caller's family check reports the mismatch. This does NOT enforce a
/// family — `resolve_int_pair`/arithmetic and `classify_comparable` (comparison) layer that on top.
fn resolve_operand_pair(
    scope: &Scope,
    lhs: &Expr,
    rhs: &Expr,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType, RExpr, ResolvedType)> {
    let lhs_lit = is_adaptable_operand(lhs);
    let rhs_lit = is_adaptable_operand(rhs);
    let (rl, lt, rr, rt) = if lhs_lit && rhs_lit {
        // Two bare adaptable operands: no column context. Default an integer literal (and a
        // bind parameter) to int64; a string literal stays text (no bytea context — types.md §6).
        let (rl, lt) = resolve(scope, lhs, Some(ScalarType::Int64), agg, params)?;
        let (rr, rt) = resolve(scope, rhs, Some(ScalarType::Int64), agg, params)?;
        (rl, lt, rr, rt)
    } else if lhs_lit {
        let (rr, rt) = resolve(scope, rhs, None, agg, params)?;
        let (rl, lt) = resolve(scope, lhs, ctx_of(rt), agg, params)?;
        (rl, lt, rr, rt)
    } else if rhs_lit {
        let (rl, lt) = resolve(scope, lhs, None, agg, params)?;
        let (rr, rt) = resolve(scope, rhs, ctx_of(lt), agg, params)?;
        (rl, lt, rr, rt)
    } else {
        let (rl, lt) = resolve(scope, lhs, None, agg, params)?;
        let (rr, rt) = resolve(scope, rhs, None, agg, params)?;
        (rl, lt, rr, rt)
    };
    Ok((rl, lt, rr, rt))
}

/// Whether `e` is an *adaptable operand* — one that takes its type from its sibling: an integer
/// or string literal, or a bind parameter `$N` (spec/design/api.md §5). NULL, boolean, and
/// decimal literals do not take a sibling's context here.
fn is_adaptable_operand(e: &Expr) -> bool {
    matches!(
        e,
        Expr::Literal(Literal::Int(_)) | Expr::Literal(Literal::Text(_)) | Expr::Param(_)
    )
}

/// The context type a sibling operand offers an adaptable operand. For an integer literal this
/// is the integer width it adopts; for a string literal, `bytea`/`uuid`/`text` (so it can decode
/// the hex/uuid input); a bind parameter additionally adopts a `decimal`/`boolean` sibling (a
/// literal ignores those — its arm keeps int64/text — so widening the mapping is safe). Only a
/// bare NULL offers no context.
fn ctx_of(ty: ResolvedType) -> Option<ScalarType> {
    match ty {
        ResolvedType::Int(t) => Some(t),
        ResolvedType::Bytea => Some(ScalarType::Bytea),
        ResolvedType::Uuid => Some(ScalarType::Uuid),
        ResolvedType::Text => Some(ScalarType::Text),
        ResolvedType::Bool => Some(ScalarType::Bool),
        ResolvedType::Decimal => Some(ScalarType::Decimal),
        ResolvedType::Null => None,
        // A datetime sibling offers its type so a string literal parses as that datetime.
        ResolvedType::Timestamp => Some(ScalarType::Timestamp),
        ResolvedType::Timestamptz => Some(ScalarType::Timestamptz),
    }
}

/// Require that an arithmetic operand is numeric (integer or decimal, or NULL); a boolean,
/// text, or bytea operand is a 42804 type error.
fn require_numeric_operand(ty: ResolvedType) -> Result<()> {
    match ty {
        ResolvedType::Int(_) | ResolvedType::Decimal | ResolvedType::Null => Ok(()),
        ResolvedType::Bool
        | ResolvedType::Text
        | ResolvedType::Bytea
        | ResolvedType::Uuid
        | ResolvedType::Timestamp
        | ResolvedType::Timestamptz => {
            Err(type_error("arithmetic operators require numeric operands"))
        }
    }
}

/// Require that a comparison operand pair is comparable (spec/types/compare.toml): both
/// numeric (integer and/or decimal — the integer promotes to decimal), both text, both
/// boolean, or both bytea (NULL counts as any). A cross-family pair (numeric/text,
/// boolean/non-boolean, bytea/non-bytea, …) is a 42804 type error — comparison is overloaded
/// across these families but never compares across them.
fn classify_comparable(lt: ResolvedType, rt: ResolvedType) -> Result<()> {
    use ResolvedType::{Bool, Bytea, Decimal, Int, Null, Text, Timestamp, Timestamptz, Uuid};
    match (lt, rt) {
        // timestamp / timestamptz compare only within their own family (or with a bare NULL).
        // A mixed timestamp × timestamptz pair — or a datetime vs any other family — would need
        // a zone, so it is a 42804 type error (spec/design/timestamp.md §5).
        (Timestamp, Timestamp) | (Timestamptz, Timestamptz) => Ok(()),
        (Timestamp, Null) | (Null, Timestamp) | (Timestamptz, Null) | (Null, Timestamptz) => Ok(()),
        (Timestamp, _) | (_, Timestamp) | (Timestamptz, _) | (_, Timestamptz) => Err(type_error(
            "cannot compare a timestamp value with a value of a different type",
        )),
        // Boolean compares only with boolean (or NULL); boolean with a number/text/bytea is a mismatch.
        (Bool, Int(_))
        | (Int(_), Bool)
        | (Bool, Text)
        | (Text, Bool)
        | (Bool, Decimal)
        | (Decimal, Bool)
        | (Bool, Bytea)
        | (Bytea, Bool)
        | (Bool, Uuid)
        | (Uuid, Bool) => Err(type_error(
            "cannot compare a boolean value with a non-boolean value",
        )),
        (Int(_), Text) | (Text, Int(_)) | (Decimal, Text) | (Text, Decimal) => Err(type_error(
            "cannot compare a text value with a numeric value",
        )),
        // bytea compares only with bytea (or NULL); bytea with a number, text, or uuid is a mismatch.
        (Bytea, Int(_))
        | (Int(_), Bytea)
        | (Bytea, Decimal)
        | (Decimal, Bytea)
        | (Bytea, Text)
        | (Text, Bytea)
        | (Bytea, Uuid)
        | (Uuid, Bytea) => Err(type_error(
            "cannot compare a bytea value with a non-bytea value",
        )),
        // uuid compares only with uuid (or NULL); uuid with a number or text is a mismatch
        // (the uuid/bool and uuid/bytea pairs are caught above).
        (Uuid, Int(_))
        | (Int(_), Uuid)
        | (Uuid, Decimal)
        | (Decimal, Uuid)
        | (Uuid, Text)
        | (Text, Uuid) => Err(type_error(
            "cannot compare a uuid value with a non-uuid value",
        )),
        // Same-family pairs (numeric/numeric incl. int↔decimal, text/text, bool/bool,
        // bytea/bytea, uuid/uuid) and any pairing with a bare NULL literal are comparable.
        _ => Ok(()),
    }
}

/// The `ScalarType` of an integer-typed resolved expression, or `None` for a NULL
/// literal or a non-integer type (used to pick a sibling literal's context).
fn int_type(ty: ResolvedType) -> Option<ScalarType> {
    match ty {
        ResolvedType::Int(t) => Some(t),
        _ => None,
    }
}

/// The promotion-tower result type of two arithmetic operands: the higher-ranked
/// integer type, or int64 when both are untyped NULLs.
fn promote(a: ResolvedType, b: ResolvedType) -> ScalarType {
    match (int_type(a), int_type(b)) {
        (Some(x), Some(y)) => {
            if x.rank() >= y.rank() {
                x
            } else {
                y
            }
        }
        (Some(x), None) => x,
        (None, Some(y)) => y,
        (None, None) => ScalarType::Int64,
    }
}

/// LIKE requires both operands be `text` (or a bare NULL literal, which is comparable with
/// anything and makes the result NULL at eval). A non-text operand is a 42804 type error
/// (spec/design/grammar.md §22).
fn require_text_or_null(ty: ResolvedType) -> Result<()> {
    match ty {
        ResolvedType::Text | ResolvedType::Null => Ok(()),
        _ => Err(type_error("LIKE requires text operands")),
    }
}

/// Unify a CASE's result-arm types (the THEN results + the ELSE, or `Null` for an implicit
/// ELSE) into one common type (spec/design/grammar.md §23): NULL-typed arms are dropped (they
/// adapt); an all-NULL CASE is `text` (PostgreSQL). The non-NULL arms must share a family — all
/// numeric unify to `decimal` if any is decimal, else the widest integer (the promotion tower);
/// otherwise they must all be the same non-numeric family (text/boolean/bytea). A cross-family
/// mix (e.g. integer and text) is 42804.
fn unify_case_types(arms: &[ResolvedType]) -> Result<ResolvedType> {
    let non_null: Vec<ResolvedType> = arms
        .iter()
        .copied()
        .filter(|t| *t != ResolvedType::Null)
        .collect();
    let Some(&first) = non_null.first() else {
        // Every arm is NULL/untyped — PostgreSQL types the CASE as text.
        return Ok(ResolvedType::Text);
    };
    let all_numeric = non_null
        .iter()
        .all(|t| matches!(t, ResolvedType::Int(_) | ResolvedType::Decimal));
    if all_numeric {
        if non_null.iter().any(|t| *t == ResolvedType::Decimal) {
            return Ok(ResolvedType::Decimal);
        }
        // All integer: the widest via the promotion tower (width is unobservable in output —
        // every integer renders under the `I` tag — but the fold keeps the type precise).
        let mut acc = first;
        for t in &non_null[1..] {
            acc = ResolvedType::Int(promote(acc, *t));
        }
        return Ok(acc);
    }
    // Non-numeric: every arm must be the same family as the first (cross-family is 42804).
    for t in &non_null[1..] {
        if std::mem::discriminant(t) != std::mem::discriminant(&first) {
            return Err(type_error("CASE result types must be compatible"));
        }
    }
    Ok(first)
}

/// Coerce a CASE arm's value to the unified result type. The only runtime coercion needed is
/// widening an integer result to decimal when the unified type is decimal — integer-width
/// unification needs none (all integers are `i64`), and an all-NULL CASE is text but every arm
/// evaluates to NULL anyway.
fn coerce_case(v: Value, to_decimal: bool) -> Value {
    match (to_decimal, v) {
        (true, Value::Int(n)) => Value::Decimal(Decimal::from_i64(n)),
        (_, v) => v,
    }
}

/// The operator's name for an error message (PostgreSQL phrasing).
fn setop_name(op: SetOpKind) -> &'static str {
    match op {
        SetOpKind::Union => "UNION",
        SetOpKind::Intersect => "INTERSECT",
        SetOpKind::Except => "EXCEPT",
    }
}

/// Unify one output column's type across the two operands of a set operation
/// (spec/design/grammar.md §25, types.md §4): integer widths promote to the widest; integer with
/// decimal -> decimal; a NULL-typed operand takes the other's type (an all-NULL column stays NULL
/// — PostgreSQL would call a top-level one `text`, but the type is never observed in output); a
/// same-family non-numeric pair gives that type; anything else is 42804. The set of unifiable
/// pairs mirrors the comparability matrix (compare.toml).
fn unify_setop_column(a: ResolvedType, b: ResolvedType, op: SetOpKind) -> Result<ResolvedType> {
    use ResolvedType::*;
    let out = match (a, b) {
        (Null, Null) => Null,
        (Null, x) | (x, Null) => x,
        (Int(_), Int(_)) => Int(promote(a, b)),
        (Decimal, Decimal) | (Int(_), Decimal) | (Decimal, Int(_)) => Decimal,
        (Text, Text) => Text,
        (Bool, Bool) => Bool,
        (Bytea, Bytea) => Bytea,
        (Uuid, Uuid) => Uuid,
        (Timestamp, Timestamp) => Timestamp,
        (Timestamptz, Timestamptz) => Timestamptz,
        _ => {
            return Err(EngineError::new(
                SqlState::DatatypeMismatch,
                format!(
                    "{} types {} and {} cannot be matched",
                    setop_name(op),
                    a.type_name(),
                    b.type_name()
                ),
            ));
        }
    };
    Ok(out)
}

/// Convert each row's values in place to the unified set-operation column types — the only runtime
/// change is integer -> decimal (a NULL stays NULL; integer-width promotion is a value no-op since
/// every integer is i64). Same conversion `coerce_case` uses for CASE.
fn coerce_setop_rows(rows: &mut [Vec<Value>], from: &[ResolvedType], to: &[ResolvedType]) {
    for (i, (&f, &t)) in from.iter().zip(to.iter()).enumerate() {
        if matches!(f, ResolvedType::Int(_)) && t == ResolvedType::Decimal {
            for row in rows.iter_mut() {
                if let Value::Int(n) = &row[i] {
                    let n = *n;
                    row[i] = Value::Decimal(Decimal::from_i64(n));
                }
            }
        }
    }
}

/// Combine the operands' rows per the set operator + ALL flag (spec/design/grammar.md §25). Rows
/// match by NULL-safe, value-canonical equality (the `Value` Eq/Hash — two NULLs match, 1.5 ==
/// 1.50, and a converted int matches the decimal). The emitted representative for a matched /
/// deduplicated key is its FIRST occurrence scanning the LEFT operand then the right, and emitted
/// rows keep that left-then-right scan order — deterministic and identical across cores. (A later
/// ORDER BY re-sorts; without one, output order is unspecified and the corpus compares rowsort.)
fn combine_setop(
    op: SetOpKind,
    all: bool,
    left: Vec<Vec<Value>>,
    right: Vec<Vec<Value>>,
) -> Vec<Vec<Value>> {
    match (op, all) {
        // UNION ALL: every left row then every right row, no dedup.
        (SetOpKind::Union, true) => {
            let mut rows = left;
            rows.extend(right);
            rows
        }
        // UNION: one copy per key present in either, first occurrence (left scanned first).
        (SetOpKind::Union, false) => {
            let mut seen: HashSet<Vec<Value>> = HashSet::new();
            let mut out = Vec::new();
            for row in left.into_iter().chain(right) {
                if seen.insert(row.clone()) {
                    out.push(row);
                }
            }
            out
        }
        // INTERSECT ALL: min(m, n) copies — emit a left row while the right still has budget.
        (SetOpKind::Intersect, true) => {
            let mut counts: HashMap<Vec<Value>, usize> = HashMap::new();
            for row in right {
                *counts.entry(row).or_insert(0) += 1;
            }
            let mut out = Vec::new();
            for row in left {
                if let Some(c) = counts.get_mut(&row) {
                    if *c > 0 {
                        *c -= 1;
                        out.push(row);
                    }
                }
            }
            out
        }
        // INTERSECT: one copy per distinct left key also present in the right.
        (SetOpKind::Intersect, false) => {
            let right_set: HashSet<Vec<Value>> = right.into_iter().collect();
            let mut emitted: HashSet<Vec<Value>> = HashSet::new();
            let mut out = Vec::new();
            for row in left {
                if right_set.contains(&row) && emitted.insert(row.clone()) {
                    out.push(row);
                }
            }
            out
        }
        // EXCEPT ALL: max(0, m - n) copies — the right cancels the first n left occurrences.
        (SetOpKind::Except, true) => {
            let mut counts: HashMap<Vec<Value>, usize> = HashMap::new();
            for row in right {
                *counts.entry(row).or_insert(0) += 1;
            }
            let mut out = Vec::new();
            for row in left {
                match counts.get_mut(&row) {
                    Some(c) if *c > 0 => *c -= 1,
                    _ => out.push(row),
                }
            }
            out
        }
        // EXCEPT: one copy per distinct left key absent from the right.
        (SetOpKind::Except, false) => {
            let right_set: HashSet<Vec<Value>> = right.into_iter().collect();
            let mut emitted: HashSet<Vec<Value>> = HashSet::new();
            let mut out = Vec::new();
            for row in left {
                if !right_set.contains(&row) && emitted.insert(row.clone()) {
                    out.push(row);
                }
            }
            out
        }
    }
}

/// Resolve a trailing ORDER BY key for a set operation against the OUTPUT column names (the left
/// operand's). A qualified key is 42P01 (no relation scope after a set operation); an unknown name
/// is 42703. Returns the output column index.
fn resolve_setop_order_key(key: &OrderKey, names: &[String]) -> Result<usize> {
    if let Some(q) = &key.qualifier {
        return Err(EngineError::new(
            SqlState::UndefinedTable,
            format!("missing FROM-clause entry for table {q}"),
        ));
    }
    names
        .iter()
        .position(|n| n.eq_ignore_ascii_case(&key.column))
        .ok_or_else(|| {
            EngineError::new(
                SqlState::UndefinedColumn,
                format!("column {} does not exist", key.column),
            )
        })
}

fn require_bool(ty: ResolvedType, msg: &str) -> Result<()> {
    match ty {
        ResolvedType::Bool | ResolvedType::Null => Ok(()),
        ResolvedType::Int(_)
        | ResolvedType::Text
        | ResolvedType::Decimal
        | ResolvedType::Bytea
        | ResolvedType::Uuid
        | ResolvedType::Timestamp
        | ResolvedType::Timestamptz => Err(type_error(msg)),
    }
}

/// A value assigned to a column must match its family: an integer column takes an
/// integer (or NULL) value; a text column takes a text (or NULL) value; a boolean column
/// takes a boolean (or NULL) value. Any cross-family pair is a 42804 type error. Mirrors
/// the INSERT literal type-check, generalized to expressions.
fn require_assignable(ty: ResolvedType, col_ty: ScalarType, col: &str) -> Result<()> {
    let ok = if col_ty.is_integer() {
        matches!(ty, ResolvedType::Int(_) | ResolvedType::Null)
    } else if col_ty.is_decimal() {
        // int → decimal is implicit (lossless); decimal → decimal re-scales. A decimal value
        // into an integer column is NOT assignable (decimal→int is explicit-CAST only).
        matches!(
            ty,
            ResolvedType::Int(_) | ResolvedType::Decimal | ResolvedType::Null
        )
    } else if col_ty.is_bool() {
        matches!(ty, ResolvedType::Bool | ResolvedType::Null)
    } else if col_ty.is_bytea() {
        matches!(ty, ResolvedType::Bytea | ResolvedType::Null)
    } else if col_ty.is_uuid() {
        matches!(ty, ResolvedType::Uuid | ResolvedType::Null)
    } else if col_ty.is_timestamp() {
        matches!(ty, ResolvedType::Timestamp | ResolvedType::Null)
    } else if col_ty.is_timestamptz() {
        matches!(ty, ResolvedType::Timestamptz | ResolvedType::Null)
    } else {
        // text column
        matches!(ty, ResolvedType::Text | ResolvedType::Null)
    };
    if ok {
        Ok(())
    } else {
        Err(type_error(format!(
            "cannot assign a value to column {col} of type {}",
            col_ty.canonical_name()
        )))
    }
}

fn col_idx(table: &Table, name: &str) -> Result<usize> {
    table
        .column_index(name)
        .ok_or_else(|| undefined_column(name))
}

/// 42703 — a column name that no relation in scope defines.
fn undefined_column(name: &str) -> EngineError {
    EngineError::new(
        SqlState::UndefinedColumn,
        format!("column does not exist: {name}"),
    )
}

/// 42702 — a bare column name that more than one relation in scope defines (grammar.md §15).
fn ambiguous_column(name: &str) -> EngineError {
    EngineError::new(
        SqlState::AmbiguousColumn,
        format!("column reference {name} is ambiguous"),
    )
}

/// 42P01 — a qualifier that names no relation in the FROM clause (grammar.md §15).
fn missing_from_entry(qualifier: &str) -> EngineError {
    EngineError::new(
        SqlState::UndefinedTable,
        format!("missing FROM-clause entry for table {qualifier}"),
    )
}

/// Resolve a type name + optional type modifier used in a column definition or a CAST target.
/// All canonical names and aliases (including `boolean`/`bool` and `numeric`/`decimal`/`dec`)
/// resolve here; a genuinely unknown name is a 42704. A type modifier is meaningful only for
/// decimal (validated to `numeric(p,s)` — 22023); on any other type it is `0A000` (varchar(n)
/// and other parameterized types are deferred — spec/design/grammar.md §14). Type-specific
/// narrowings (a text/boolean/decimal PRIMARY KEY, a CAST to text/boolean) are enforced at the
/// call site, not here.
fn resolve_type_and_typmod(
    name: &str,
    type_mod: &Option<TypeMod>,
) -> Result<(ScalarType, Option<DecimalTypmod>)> {
    let ty = if let Some(ty) = ScalarType::from_name(name) {
        ty
    } else {
        return Err(EngineError::new(
            SqlState::UndefinedObject,
            format!("type does not exist: {name}"),
        ));
    };
    let typmod = match type_mod {
        None => None,
        Some(tm) => {
            if ty.is_decimal() {
                Some(validate_decimal_typmod(tm)?)
            } else {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    format!(
                        "a type modifier is not supported for type {}",
                        ty.canonical_name()
                    ),
                ));
            }
        }
    };
    Ok((ty, typmod))
}

/// Validate a decimal `numeric(p[,s])` type modifier: `1 <= p <= 1000`, `0 <= s <= p`; else
/// trap 22023 (spec/design/decimal.md §2). `numeric(p)` means scale 0.
fn validate_decimal_typmod(tm: &TypeMod) -> Result<DecimalTypmod> {
    let p = tm.precision;
    if p < 1 || p > MAX_PRECISION as u64 {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            format!("NUMERIC precision {p} must be between 1 and {MAX_PRECISION}"),
        ));
    }
    let s = tm.scale.unwrap_or(0);
    if s > p || s > MAX_SCALE as u64 {
        return Err(EngineError::new(
            SqlState::InvalidParameterValue,
            format!("NUMERIC scale {s} must be between 0 and precision {p}"),
        ));
    }
    Ok(DecimalTypmod {
        precision: p as u16,
        scale: s as u16,
    })
}

fn overflow(ty: ScalarType) -> EngineError {
    EngineError::new(
        SqlState::NumericValueOutOfRange,
        format!("value out of range for type {}", ty.canonical_name()),
    )
}

fn type_error(msg: impl Into<String>) -> EngineError {
    EngineError::new(SqlState::DatatypeMismatch, msg.into())
}

/// Decode a single-quoted literal's content as a bytea value via the hex input form
/// (`value::parse_bytea_hex`), mapping malformed hex to a `22P02`
/// (invalid_text_representation). Used when a string literal adapts to a bytea context
/// (types.md §6/§13); the trap is deterministic and fires at resolve time, before any scan.
fn decode_bytea_literal(s: &str) -> Result<Vec<u8>> {
    parse_bytea_hex(s).map_err(|detail| {
        EngineError::new(
            SqlState::InvalidTextRepresentation,
            format!("invalid input syntax for type bytea: {detail}"),
        )
    })
}

/// Decode a single-quoted literal's content as a uuid value via PostgreSQL-flexible input
/// (`value::parse_uuid`), mapping malformed input to a `22P02` (invalid_text_representation).
/// Used when a string literal adapts to a uuid context (types.md §6/§14); the trap is
/// deterministic and fires at resolve time, before any scan.
fn decode_uuid_literal(s: &str) -> Result<[u8; 16]> {
    parse_uuid(s).map_err(|detail| {
        EngineError::new(
            SqlState::InvalidTextRepresentation,
            format!("invalid input syntax for type uuid: {detail}"),
        )
    })
}

/// A resolved UPDATE assignment: which column to write, the target type/nullability so
/// the new value is re-checked exactly like INSERT, and the resolved RHS expression
/// (evaluated against the *old* row).
struct AssignPlan {
    idx: usize,
    name: String,
    target: ScalarType,
    decimal: Option<DecimalTypmod>,
    not_null: bool,
    source: RExpr,
}

impl AssignPlan {
    /// Type-check + coerce a candidate value against this column — the same `store_value`
    /// path INSERT uses (NULL into NOT NULL → 23502; an integer outside range → 22003; an
    /// integer into a decimal column widens and coerces to the typmod; a decimal into a
    /// decimal column rounds to its scale; a boolean into a boolean column is accepted
    /// as-is). The resolver already proved the value's family is assignable (never
    /// decimal→int implicitly).
    fn check(&self, v: Value) -> Result<Value> {
        store_value(v, self.target, self.decimal, self.not_null, &self.name)
    }
}

/// Coerce a value into a column for storage (shared by INSERT and UPDATE). NULL honours NOT
/// NULL (23502); an integer into an integer column is range-checked (22003); an integer into
/// a decimal column widens (int→decimal) then coerces to the typmod; a decimal into a decimal
/// column coerces to the typmod (rounds to scale, precision-checks → 22003); a cross-family
/// value (decimal→int, text→int, etc.) is a 42804 (decimal→int is explicit-CAST only).
fn store_value(
    v: Value,
    col_ty: ScalarType,
    typmod: Option<DecimalTypmod>,
    not_null: bool,
    col_name: &str,
) -> Result<Value> {
    match v {
        Value::Null => {
            if not_null {
                return Err(EngineError::new(
                    SqlState::NotNullViolation,
                    format!("null value in column {col_name} violates not-null constraint"),
                ));
            }
            Ok(Value::Null)
        }
        Value::Int(n) => {
            if col_ty.is_integer() {
                if col_ty.in_range(n) {
                    Ok(Value::Int(n))
                } else {
                    Err(overflow(col_ty))
                }
            } else if col_ty.is_decimal() {
                Ok(Value::Decimal(coerce_decimal(
                    Decimal::from_i64(n),
                    typmod,
                )?))
            } else {
                Err(type_error(format!(
                    "cannot store an integer value in {} column {col_name}",
                    col_ty.canonical_name()
                )))
            }
        }
        Value::Decimal(d) => {
            if col_ty.is_decimal() {
                Ok(Value::Decimal(coerce_decimal(d, typmod)?))
            } else {
                Err(type_error(format!(
                    "cannot store a decimal value in {} column {col_name}",
                    col_ty.canonical_name()
                )))
            }
        }
        Value::Text(s) => {
            if col_ty.is_text() {
                Ok(Value::Text(s))
            } else if col_ty.is_bytea() {
                // A string literal adapts to a bytea column, decoding the hex input form
                // (types.md §6/§13); malformed hex traps 22P02.
                Ok(Value::Bytea(decode_bytea_literal(&s)?))
            } else if col_ty.is_uuid() {
                // A string literal adapts to a uuid column via the PG-flexible input
                // (types.md §6/§14); malformed input traps 22P02.
                Ok(Value::Uuid(decode_uuid_literal(&s)?))
            } else if col_ty.is_timestamp() {
                // A string literal adapts to a timestamp column (spec/design/timestamp.md);
                // malformed input traps 22007, an out-of-range field 22008.
                Ok(Value::Timestamp(parse_timestamp(&s)?))
            } else if col_ty.is_timestamptz() {
                Ok(Value::Timestamptz(parse_timestamptz(&s)?))
            } else {
                Err(type_error(format!(
                    "cannot store a text value in {} column {col_name}",
                    col_ty.canonical_name()
                )))
            }
        }
        Value::Bytea(b) => {
            if col_ty.is_bytea() {
                Ok(Value::Bytea(b))
            } else {
                Err(type_error(format!(
                    "cannot store a bytea value in {} column {col_name}",
                    col_ty.canonical_name()
                )))
            }
        }
        Value::Uuid(u) => {
            if col_ty.is_uuid() {
                Ok(Value::Uuid(u))
            } else {
                Err(type_error(format!(
                    "cannot store a uuid value in {} column {col_name}",
                    col_ty.canonical_name()
                )))
            }
        }
        Value::Timestamp(m) => {
            if col_ty.is_timestamp() {
                Ok(Value::Timestamp(m))
            } else {
                Err(type_error(format!(
                    "cannot store a timestamp value in {} column {col_name}",
                    col_ty.canonical_name()
                )))
            }
        }
        Value::Timestamptz(m) => {
            if col_ty.is_timestamptz() {
                Ok(Value::Timestamptz(m))
            } else {
                Err(type_error(format!(
                    "cannot store a timestamptz value in {} column {col_name}",
                    col_ty.canonical_name()
                )))
            }
        }
        Value::Bool(b) => {
            if col_ty.is_bool() {
                Ok(Value::Bool(b))
            } else {
                Err(type_error(format!(
                    "cannot store a boolean value in {} column {col_name}",
                    col_ty.canonical_name()
                )))
            }
        }
    }
}

/// Coerce a decimal into a column's typmod: round to the declared scale and precision-check
/// (22003) for `numeric(p,s)`; for an unconstrained `numeric` column just cap-check
/// (spec/design/decimal.md §2).
fn coerce_decimal(d: Decimal, typmod: Option<DecimalTypmod>) -> Result<Decimal> {
    match typmod {
        Some(t) => d.coerce_to_typmod(t.precision as u32, t.scale as u32),
        None => d.check_cap(),
    }
}

/// Wrap a parsed literal as a runtime value (the type-check/coercion is `store_value`).
fn literal_to_value(lit: &Literal) -> Value {
    match lit {
        Literal::Null => Value::Null,
        Literal::Int(n) => Value::Int(*n),
        Literal::Bool(b) => Value::Bool(*b),
        Literal::Text(s) => Value::Text(s.clone()),
        Literal::Decimal(d) => Value::Decimal(d.clone()),
    }
}

impl RExpr {
    /// Evaluate against a row, accruing cost into `m`. Returns a `Value` (which may be a
    /// boolean for comparisons/connectives). Arithmetic traps 22003 on overflow and 22012
    /// on a zero divisor; NULL propagates through arithmetic; the connectives are Kleene.
    ///
    /// Cost: each **interior** node charges `operator_eval` once, pre-order (the node, then
    /// its operands LHS-before-RHS); leaf nodes (column/constants) charge nothing. Both
    /// operands are always evaluated — there is no short-circuit, so the count never
    /// depends on operand values (spec/design/cost.md §3).
    fn eval(&self, row: &[Value], env: &EvalEnv, m: &mut Meter) -> Result<Value> {
        match self {
            // The value is read out of a borrowed stored row, so it is cloned (Value is
            // Clone, not Copy, now that a text value owns a String).
            RExpr::Column(i) => Ok(row[*i].clone()),
            // A correlated reference: the column `index` of the enclosing row `level` hops out
            // (1 = immediate parent). A leaf — reads from the outer-row environment (§26).
            RExpr::OuterColumn { level, index } => {
                Ok(env.outer[env.outer.len() - level][*index].clone())
            }
            // A bind parameter — the supplied value, already coerced to its inferred type by
            // `bind_params` before execution (spec/design/api.md §5).
            RExpr::Param(i) => Ok(env.params[*i].clone()),
            RExpr::ConstInt(n) => Ok(Value::Int(*n)),
            RExpr::ConstBool(b) => Ok(Value::Bool(*b)),
            RExpr::ConstText(s) => Ok(Value::Text(s.clone())),
            RExpr::ConstDecimal(d) => Ok(Value::Decimal(d.clone())),
            RExpr::ConstBytea(b) => Ok(Value::Bytea(b.clone())),
            RExpr::ConstUuid(u) => Ok(Value::Uuid(*u)),
            RExpr::ConstTimestamp(m) => Ok(Value::Timestamp(*m)),
            RExpr::ConstTimestamptz(m) => Ok(Value::Timestamptz(*m)),
            RExpr::ConstNull => Ok(Value::Null),
            RExpr::Cast {
                inner,
                target,
                typmod,
            } => {
                m.charge(COSTS.operator_eval);
                match inner.eval(row, env, m)? {
                    Value::Null => Ok(Value::Null),
                    Value::Int(n) => {
                        if target.is_decimal() {
                            // int → decimal (lossless), then coerce to the typmod.
                            Ok(Value::Decimal(coerce_decimal(
                                Decimal::from_i64(n),
                                *typmod,
                            )?))
                        } else if target.in_range(n) {
                            Ok(Value::Int(n))
                        } else {
                            Err(overflow(*target))
                        }
                    }
                    Value::Decimal(d) => {
                        if target.is_decimal() {
                            // decimal → decimal: re-scale to the target typmod.
                            Ok(Value::Decimal(coerce_decimal(d, *typmod)?))
                        } else {
                            // decimal → int (explicit): round half-away to scale 0, then
                            // range-check the target integer type (22003).
                            let v = d.to_i64_round().ok_or_else(|| overflow(*target))?;
                            if target.in_range(v) {
                                Ok(Value::Int(v))
                            } else {
                                Err(overflow(*target))
                            }
                        }
                    }
                    Value::Bool(_) => unreachable!("resolver rejects a boolean cast operand"),
                    Value::Text(_) => unreachable!("resolver rejects a text cast operand"),
                    Value::Bytea(_) => unreachable!("resolver rejects a bytea cast operand"),
                    Value::Uuid(_) => unreachable!("resolver rejects a uuid cast operand"),
                    Value::Timestamp(_) | Value::Timestamptz(_) => {
                        unreachable!("resolver rejects a timestamp cast operand")
                    }
                }
            }
            RExpr::Neg { operand, result } => {
                m.charge(COSTS.operator_eval);
                match operand.eval(row, env, m)? {
                    Value::Null => Ok(Value::Null),
                    Value::Int(n) if result.is_decimal() => {
                        Ok(Value::Decimal(Decimal::from_i64(n).neg()))
                    }
                    Value::Int(n) => {
                        // checked_neg guards i64::MIN; then range-check the result type.
                        let v = n.checked_neg().ok_or_else(|| overflow(*result))?;
                        if result.in_range(v) {
                            Ok(Value::Int(v))
                        } else {
                            Err(overflow(*result))
                        }
                    }
                    Value::Decimal(d) => Ok(Value::Decimal(d.neg())),
                    Value::Bool(_) => unreachable!("resolver rejects a boolean unary minus"),
                    Value::Text(_) => unreachable!("resolver rejects a text unary minus"),
                    Value::Bytea(_) => unreachable!("resolver rejects a bytea unary minus"),
                    Value::Uuid(_) => unreachable!("resolver rejects a uuid unary minus"),
                    Value::Timestamp(_) | Value::Timestamptz(_) => {
                        unreachable!("resolver rejects a timestamp unary minus")
                    }
                }
            }
            RExpr::Not(e) => {
                m.charge(COSTS.operator_eval);
                let v = e.eval(row, env, m)?;
                Ok(not3(&v))
            }
            RExpr::Arith {
                op,
                lhs,
                rhs,
                result,
            } => {
                m.charge(COSTS.operator_eval);
                let a = lhs.eval(row, env, m)?;
                let b = rhs.eval(row, env, m)?;
                if matches!(a, Value::Null) || matches!(b, Value::Null) {
                    return Ok(Value::Null);
                }
                if result.is_decimal() {
                    // Decimal arithmetic: widen any integer operand to decimal, then apply the
                    // op with PG's scale rules (spec/design/decimal.md §4).
                    eval_decimal_arith(*op, to_decimal(a), to_decimal(b))
                } else {
                    match (a, b) {
                        (Value::Int(x), Value::Int(y)) => eval_arith(*op, x, y, *result),
                        _ => unreachable!("resolver rejects non-integer arithmetic operands"),
                    }
                }
            }
            RExpr::Compare { op, lhs, rhs } => {
                m.charge(COSTS.operator_eval);
                let a = lhs.eval(row, env, m)?;
                let b = rhs.eval(row, env, m)?;
                let tv = match op {
                    CmpOp::Eq => a.eq3(&b),
                    CmpOp::Lt => a.lt3(&b),
                    CmpOp::Gt => a.gt3(&b),
                    CmpOp::Le => a.lt3(&b).or(a.eq3(&b)),
                    CmpOp::Ge => a.gt3(&b).or(a.eq3(&b)),
                };
                Ok(from3(tv))
            }
            RExpr::And(l, r) => {
                m.charge(COSTS.operator_eval);
                let lv = l.eval(row, env, m)?;
                let rv = r.eval(row, env, m)?;
                Ok(and3(&lv, &rv))
            }
            RExpr::Or(l, r) => {
                m.charge(COSTS.operator_eval);
                let lv = l.eval(row, env, m)?;
                let rv = r.eval(row, env, m)?;
                Ok(or3(&lv, &rv))
            }
            RExpr::IsNull { operand, negated } => {
                m.charge(COSTS.operator_eval);
                let is_null = matches!(operand.eval(row, env, m)?, Value::Null);
                // IS [NOT] NULL is always a definite boolean, never unknown (CLAUDE.md §4).
                Ok(Value::Bool(is_null != *negated))
            }
            RExpr::Distinct { lhs, rhs, negated } => {
                m.charge(COSTS.operator_eval);
                let lv = lhs.eval(row, env, m)?;
                let rv = rhs.eval(row, env, m)?;
                let same = lv.not_distinct_from(&rv);
                // `negated` carries the NOT keyword: IS NOT DISTINCT FROM (negated) asks
                // "are they the same?" → `same`; IS DISTINCT FROM asks the opposite. Either
                // way the result is a definite boolean — never unknown (the null_safe
                // discipline, functions.md §3).
                Ok(Value::Bool(same == *negated))
            }
            RExpr::Like { lhs, rhs, negated } => {
                m.charge(COSTS.operator_eval);
                let subject = lhs.eval(row, env, m)?;
                let pattern = rhs.eval(row, env, m)?;
                // NULL propagates BEFORE the matcher runs, so a malformed pattern against a
                // NULL operand is still NULL, never 22025 (matches PG — grammar.md §22).
                if matches!(subject, Value::Null) || matches!(pattern, Value::Null) {
                    return Ok(Value::Null);
                }
                let (s, p) = match (&subject, &pattern) {
                    (Value::Text(s), Value::Text(p)) => (s.as_str(), p.as_str()),
                    _ => unreachable!("resolver requires text LIKE operands"),
                };
                let matched = like_match(s, p)?;
                // `negated` carries NOT LIKE: matched != negated flips the result for NOT LIKE.
                Ok(Value::Bool(matched != *negated))
            }
            RExpr::Case {
                arms,
                els,
                coerce_decimal,
            } => {
                // CASE is the ONE deliberate exception to "no short-circuit" (cost.md §3):
                // conditions are evaluated in order and evaluation STOPS at the first TRUE — a
                // FALSE or NULL/UNKNOWN condition falls through, and later arms (and their
                // results) are NOT evaluated. This is required for PG semantics (e.g.
                // `CASE WHEN a=0 THEN 0 ELSE 1/a END` must not divide by zero). Charge the node,
                // then only the conditions up to the match plus the selected result accrue.
                m.charge(COSTS.operator_eval);
                for (cond, result) in arms {
                    if cond.eval(row, env, m)?.is_true() {
                        return Ok(coerce_case(result.eval(row, env, m)?, *coerce_decimal));
                    }
                }
                Ok(coerce_case(els.eval(row, env, m)?, *coerce_decimal))
            }
            RExpr::ScalarFunc { func, args, result } => {
                // One operator_eval per call (the uniform weight); arguments charge their own.
                m.charge(COSTS.operator_eval);
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    let v = a.eval(row, env, m)?;
                    if matches!(v, Value::Null) {
                        return Ok(Value::Null); // NULL propagates
                    }
                    vals.push(v);
                }
                match func {
                    ScalarFunc::Abs => match &vals[0] {
                        // abs over an integer: |x| then range-check at the result type's
                        // boundary (abs(int16 -32768) → 22003), exactly like Neg.
                        Value::Int(n) => {
                            let v = n.checked_abs().ok_or_else(|| overflow(*result))?;
                            if result.in_range(v) {
                                Ok(Value::Int(v))
                            } else {
                                Err(overflow(*result))
                            }
                        }
                        Value::Decimal(d) => Ok(Value::Decimal(d.abs())),
                        _ => unreachable!("resolver restricts abs to integer/decimal operands"),
                    },
                    ScalarFunc::Round => {
                        let d = match &vals[0] {
                            Value::Int(n) => Decimal::from_i64(*n),
                            Value::Decimal(d) => d.clone(),
                            _ => {
                                unreachable!("resolver restricts round to integer/decimal operands")
                            }
                        };
                        let places = match vals.get(1) {
                            None => 0,
                            Some(Value::Int(k)) => *k,
                            Some(_) => unreachable!("resolver restricts round's count to integer"),
                        };
                        Ok(Value::Decimal(d.round_places(places)))
                    }
                }
            }
            // A correlated subquery (spec/design/grammar.md §26): re-executed once per outer row.
            // Push the current row onto the outer-row stack, run the inner plan against it, fold
            // its accrued cost into this meter, plus one operator_eval for the node. (Uncorrelated
            // subqueries were folded to a constant / `InValues` before exec, so this is correlated.)
            RExpr::Subquery {
                plan,
                kind,
                lhs,
                negated,
            } => {
                m.charge(COSTS.operator_eval);
                let mut child: Vec<&[Value]> = env.outer.to_vec();
                child.push(row);
                let r = env.exec.exec_query_plan(plan, &child, env.params)?;
                m.charge(r.cost);
                match kind {
                    SubqueryKind::Scalar => {
                        if r.rows.len() > 1 {
                            return Err(EngineError::new(
                                SqlState::CardinalityViolation,
                                "more than one row returned by a subquery used as an expression",
                            ));
                        }
                        // 0 rows -> NULL (the static type was settled at resolve via the column
                        // type, so a cross-family comparison already errored at plan time).
                        Ok(r.rows
                            .into_iter()
                            .next()
                            .map(|mut row| row.swap_remove(0))
                            .unwrap_or(Value::Null))
                    }
                    // EXISTS ignores the select list entirely and is never NULL.
                    SubqueryKind::Exists => Ok(Value::Bool(!r.rows.is_empty() != *negated)),
                    SubqueryKind::In => {
                        let lv = lhs
                            .as_ref()
                            .expect("an IN subquery carries its resolved lhs")
                            .eval(row, env, m)?;
                        let list: Vec<Value> = r
                            .rows
                            .into_iter()
                            .map(|mut row| row.swap_remove(0))
                            .collect();
                        Ok(in_membership(&lv, &list, *negated, m))
                    }
                }
            }
            // A folded uncorrelated `IN (subquery)` — the list is constant; test membership per row.
            RExpr::InValues { lhs, list, negated } => {
                m.charge(COSTS.operator_eval);
                let lv = lhs.eval(row, env, m)?;
                Ok(in_membership(&lv, list, *negated, m))
            }
        }
    }
}

/// The SQL `LIKE` matcher (spec/design/grammar.md §22): `%` matches any (possibly empty) run
/// of characters, `_` matches exactly one character, and `\` (the default escape) makes the
/// next pattern character literal. It iterates by Unicode **code point** (so astral characters
/// match `_` correctly — a CLAUDE.md §8 determinism surface), via a two-pointer greedy
/// backtracking walk identical across the cores. It returns `Err(22025)` when the escape
/// character is the **last** pattern character *reached during matching* (PostgreSQL's "LIKE
/// pattern must not end with escape character") — data-dependent, since an earlier mismatch
/// returns `false` before the escape is reached.
fn like_match(subject: &str, pattern: &str) -> Result<bool> {
    let s: Vec<char> = subject.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    let mut si = 0usize;
    let mut pi = 0usize;
    // The last '%' position in the pattern (a backtrack point) and the subject index when it
    // was taken; `None` until a '%' has been seen.
    let mut star_pi: Option<usize> = None;
    let mut star_si = 0usize;
    while si < s.len() {
        if pi < p.len() && p[pi] == '\\' {
            // Escape: the next pattern character must match the subject literally.
            if pi + 1 >= p.len() {
                return Err(EngineError::new(
                    SqlState::InvalidEscapeSequence,
                    "LIKE pattern must not end with escape character",
                ));
            }
            if s[si] == p[pi + 1] {
                si += 1;
                pi += 2;
                continue;
            }
            // literal mismatch → fall through to backtrack
        } else if pi < p.len() && p[pi] == '_' {
            si += 1;
            pi += 1;
            continue;
        } else if pi < p.len() && p[pi] == '%' {
            star_pi = Some(pi);
            star_si = si;
            pi += 1;
            continue;
        } else if pi < p.len() && p[pi] == s[si] {
            si += 1;
            pi += 1;
            continue;
        }
        // Mismatch: backtrack to the last '%' (it absorbs one more subject character), else no.
        if let Some(sp) = star_pi {
            pi = sp + 1;
            star_si += 1;
            si = star_si;
            continue;
        }
        return Ok(false);
    }
    // Subject consumed: any pattern remainder must be all '%' to match.
    while pi < p.len() && p[pi] == '%' {
        pi += 1;
    }
    Ok(pi == p.len())
}

/// Evaluate an integer arithmetic op in 64-bit, trapping 22012 on a zero divisor and
/// 22003 if the 64-bit op overflows OR the in-range result falls outside the declared
/// result type (the int16+int16 → int16 boundary — spec/design/functions.md §7).
fn eval_arith(op: ArithOp, x: i64, y: i64, result: ScalarType) -> Result<Value> {
    let computed = match op {
        ArithOp::Add => x.checked_add(y),
        ArithOp::Sub => x.checked_sub(y),
        ArithOp::Mul => x.checked_mul(y),
        ArithOp::Div => {
            if y == 0 {
                return Err(EngineError::new(
                    SqlState::DivisionByZero,
                    "division by zero",
                ));
            }
            x.checked_div(y)
        }
        ArithOp::Mod => {
            if y == 0 {
                return Err(EngineError::new(
                    SqlState::DivisionByZero,
                    "division by zero",
                ));
            }
            x.checked_rem(y)
        }
    };
    let v = computed.ok_or_else(|| overflow(result))?;
    if result.in_range(v) {
        Ok(Value::Int(v))
    } else {
        Err(overflow(result))
    }
}

/// Widen a numeric value to `Decimal` (an integer operand of decimal arithmetic).
fn to_decimal(v: Value) -> Decimal {
    match v {
        Value::Decimal(d) => d,
        Value::Int(n) => Decimal::from_i64(n),
        _ => unreachable!("resolver guarantees a numeric operand here"),
    }
}

/// Evaluate decimal arithmetic with PG's result-scale rules (spec/design/decimal.md §4),
/// trapping 22003 at the cap and 22012 on a zero divisor/modulus.
fn eval_decimal_arith(op: ArithOp, a: Decimal, b: Decimal) -> Result<Value> {
    let r = match op {
        ArithOp::Add => a.add(&b)?,
        ArithOp::Sub => a.sub(&b)?,
        ArithOp::Mul => a.mul(&b)?,
        ArithOp::Div => a.div(&b)?,
        ArithOp::Mod => a.rem(&b)?,
    };
    Ok(Value::Decimal(r))
}

/// One ORDER BY key's total-order comparison. NULL placement is governed by `nulls_first`
/// and applied INDEPENDENTLY of the value-direction flip (`descending`), so an explicit
/// `NULLS FIRST|LAST` overrides the direction default (spec/design/grammar.md §10). The
/// physical key order ratifies NULL as the largest value (the PostgreSQL model), which
/// surfaces as the parse-time default `nulls_first = descending` (ASC → last, DESC → first).
fn key_cmp(a: &Value, b: &Value, descending: bool, nulls_first: bool) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => {
            if nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (_, Value::Null) => {
            if nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        _ => {
            let base = value_cmp(a, b);
            if descending { base.reverse() } else { base }
        }
    }
}

/// Total order over NON-NULL values: signed-integer ascending, text by the `C`
/// collation — raw UTF-8 bytes, which for UTF-8 equals code-point order
/// (spec/design/types.md §11) — and boolean by value, false < true (types.md §9). The
/// cross-family arms (a fixed `bool < int < text` order) are kept only for totality —
/// ORDER BY is over a single typed column, so they are unreachable from SELECT. NULLs are
/// handled by `key_cmp` before this is reached.
fn value_cmp(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Decimal(x), Value::Decimal(y)) => x.cmp_value(y),
        (Value::Text(x), Value::Text(y)) => x.as_bytes().cmp(y.as_bytes()),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Bytea(x), Value::Bytea(y)) => x.cmp(y),
        (Value::Uuid(x), Value::Uuid(y)) => x.cmp(y),
        // Timestamps order by the int64 instant (-infinity < finite < infinity).
        (Value::Timestamp(x), Value::Timestamp(y)) => x.cmp(y),
        (Value::Timestamptz(x), Value::Timestamptz(y)) => x.cmp(y),
        (Value::Null, Value::Null) => Ordering::Equal,
        // Cross-family arms exist only for totality — ORDER BY is over a single typed column,
        // so a mixed pair is unreachable. A fixed family order keeps the comparator total.
        _ => family_rank(a).cmp(&family_rank(b)),
    }
}

/// A fixed total order across value families, used only to keep `value_cmp` total for the
/// unreachable cross-family case (ORDER BY is single-column-typed).
fn family_rank(v: &Value) -> u8 {
    match v {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Int(_) => 2,
        Value::Decimal(_) => 3,
        Value::Text(_) => 4,
        Value::Bytea(_) => 5,
        Value::Uuid(_) => 6,
        Value::Timestamp(_) => 6,
        Value::Timestamptz(_) => 7,
    }
}
