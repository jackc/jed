//! Statement executor (CLAUDE.md §10).
//!
//! SCAFFOLD (step-5 Phase A): dispatches a parsed statement; each arm is filled in
//! feature-by-feature (Phases B–E). The result of running a statement is an
//! `Outcome`: either a bare success (DDL/DML) or a query result set.

pub(crate) use crate::api::Rows;
pub(crate) use crate::ast::{
    AlterColumnKind, AlterConstraintDef, AlterSeqAction, AlterSequence, AlterTable,
    AlterTableAction, AlterTableEdit, Analyze, BinaryOp, ConflictAction, ConflictTarget,
    CreateIndex, CreateSequence, CreateTable, CreateType, Cte, CteBody, DefaultDef, Delete,
    DropIndex, DropSequence, DropTable, DropType, Expr, GroupItem, IndexKeyElem, Insert,
    InsertSource, InsertValue, JoinKind, JsonOnBehavior, JsonPredicateKind, JsonTable, JsonWrapper,
    JtColumn, Literal, OnConflict, OrderKey, Overriding, QueryExpr, RefAction, ReturningClause,
    Select, SelectItems, SeqOptions, SetOp, SetOpKind, Statement, SubscriptSpec, TableRef,
    TypeFieldDef, TypeMod, UnaryOp, Update, WindowDef, WithExpr, WithQuery,
};
pub(crate) use crate::catalog::{
    CheckConstraint, ColField, ColType, Column, CompositeField, CompositeType, DefaultExpr,
    ExclusionConstraint, ExclusionElement, ExclusionOp, FkAction, ForeignKeyConstraint,
    IdentityKind, IndexDef, IndexKey, IndexKeyExpr, IndexKind, SeqDataType, SeqOwner, SequenceDef,
    Table, resolve_col_type,
};
pub(crate) use crate::collation::{self, Collation};
pub(crate) use crate::cost::{Lifetime, Meter};
pub(crate) use crate::costs::COSTS;
pub(crate) use crate::date::parse_date;
pub(crate) use crate::decimal::{self, Decimal, MAX_PRECISION, MAX_SCALE};
pub(crate) use crate::encoding::{encode_bool, encode_int, encode_terminated};
pub(crate) use crate::error::{EngineError, Result, SqlState};
pub(crate) use crate::interval::{self, Interval, parse_interval};
pub(crate) use crate::json::{self, JsonNode};
pub(crate) use crate::operators::{AGGREGATES, AggregateDesc, OPERATORS, OperatorDesc, WINDOWS};
pub(crate) use crate::pmap::KeyBound;
pub(crate) use crate::privileges::{Privilege, PrivilegeSet};
pub(crate) use crate::storage::{Row, TableStore};
pub(crate) use crate::timestamp::{parse_timestamp, parse_timestamptz};
pub(crate) use crate::types::{DecimalTypmod, ScalarType, Type};
pub(crate) use crate::value::{
    ArrayVal, RangeVal, ThreeValued, Value, and3, from3, not3, or3, parse_bytea_hex, parse_uuid,
    render_uuid,
};
pub(crate) use std::collections::{BTreeSet, HashMap, HashSet};
pub(crate) use std::sync::LazyLock;

// Submodules split out of the original single-file executor. Each is a plain physical partition of
// this module: it does `use super::*;`, and its items are re-exported here so intra-executor
// references stay unqualified, exactly as when this was one file.
mod window;
pub(crate) use window::*;
mod access_path;
mod aggregate;
mod ddl;
mod dml;
pub(crate) use dml::CachedInsert;
mod eval;
mod exec_emit;
mod exec_scan;
mod hash_join;
pub(crate) use hash_join::*;
mod estimate_plan;
mod execute;
mod explain_exec;
mod kernels;
mod optimize;
mod plan_query;
mod planner;
mod srf;
pub(crate) use kernels::*;
mod store_encode;
pub(crate) use store_encode::*;
mod eval_ops;
pub(crate) use eval_ops::*;
mod resolve;
pub(crate) use resolve::*;
mod resolve_func;
pub(crate) use resolve_func::*;
mod resolve_agg;
pub(crate) use resolve_agg::*;
mod exec_helpers;
pub(crate) use exec_helpers::*;
mod access_encode;
pub(crate) use access_encode::*;
mod engine;
mod snapshot;
mod statistics;
pub(crate) use statistics::*;
// __SUBMODULES__

/// The outcome of executing one statement. Both variants carry the deterministic
/// execution `cost` accrued while running the statement (CLAUDE.md §13) — a DML
/// statement accrues its scan + filter cost even though it returns no rows.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Outcome {
    /// A statement that produces no result set (CREATE, INSERT, UPDATE, DELETE).
    Statement {
        cost: i64,
        /// How many rows a DML statement (INSERT/UPDATE/DELETE without RETURNING) touched
        /// — PostgreSQL's command-tag count (spec/design/api.md §4). `Some(0)` for a DML
        /// statement that matched nothing; `None` for DDL and transaction control, which
        /// have no row count.
        rows_affected: Option<i64>,
    },
    /// A query result: output column names, the canonical name of each column's resolved type
    /// (`i16`/`i32`/`i64`/`text`/`boolean`/`decimal`/…; `unknown` for an untyped NULL),
    /// and rows in result order. The column count is `column_names.len()`
    /// (spec/design/grammar.md §8); `column_types` is parallel to it. The type is the resolved
    /// *scalar* type — for `decimal` it is the unconstrained `decimal`, not the `numeric(p,s)`
    /// typmod (which the resolved expression type does not carry; spec/design/conformance.md §7).
    Query {
        column_names: Vec<String>,
        column_types: Vec<String>,
        rows: Vec<Vec<Value>>,
        cost: i64,
    },
}

impl Outcome {
    /// The accrued execution cost (CLAUDE.md §13), available on either variant.
    pub fn cost(&self) -> i64 {
        match self {
            Outcome::Statement { cost, .. } => *cost,
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

    /// The canonical type name of each output column of a query result (parallel to
    /// `column_names`; empty for a non-query statement) — `i16`/`text`/`decimal`/…, or
    /// `unknown` for an untyped NULL column (spec/design/conformance.md §7).
    pub fn column_types(&self) -> &[String] {
        match self {
            Outcome::Query { column_types, .. } => column_types,
            Outcome::Statement { .. } => &[],
        }
    }
}

/// The `O(1)` summary of an `execute_script` run (spec/design/session.md §4.2). Carries only
/// counts — never the result rows, which `execute_script` discards — so memory is bounded by
/// construction regardless of how many rows the script's statements touch.
/// The slice-2d version-skew verdict for one referenced collation (spec/design/collation.md §12,
/// compatibility.md §7). `Full` ⇒ a loaded bundle provides the name at the file's pinned
/// `(unicode, cldr)`, so the collation's objects are read-write. `Skewed` ⇒ a loaded bundle provides
/// the name at a **different** version, so its objects are **read-only** (reads recompute against the
/// loaded table — the heap-scan fallback; a write raises `XX002`). A pure comparison of the file pin
/// (§5) vs the loaded set — every core computes the identical verdict (the §10 cross-core contract).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CollationVerdict {
    Full,
    Skewed,
}

/// Introspection metadata for one loaded collation (`db.Collations`, spec/design/collation.md §1).
/// `content_hash` is the CRC-32 of the compiled table (the reference-mode stamp, §3/§4); the
/// `description` is provenance, excluded from the hash. `verdict` is the slice-2d version-skew
/// verdict (§12) — `Full` for the engine-global loaded set (it IS the reference); for a database's
/// *referenced* collations it is `Skewed` when the file's pin differs from the loaded bundle's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollationInfo {
    pub name: String,
    pub unicode_version: String,
    pub cldr_version: String,
    pub content_hash: u32,
    pub description: String,
    pub is_default: bool,
    pub verdict: CollationVerdict,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScriptSummary {
    /// How many statements ran (each non-empty span the splitter yielded).
    pub statements_run: u64,
    /// The sum of the DML command-tag counts (INSERT/UPDATE/DELETE rows affected). A `SELECT` or a
    /// DDL/transaction-control statement contributes nothing.
    pub rows_affected_total: i64,
    /// The total accrued execution cost across every statement (the deterministic cost meter,
    /// CLAUDE.md §13) — the figure a future `lifetime_max_cost` budget bounds.
    pub cost: i64,
}

/// The full result of running a SELECT (`run_select`): the output column names and their
/// resolved types, the rows in result order, and the accrued cost. Internal to the executor —
/// `execute_select` drops the types into the public `Outcome::Query`, while `INSERT ... SELECT`
/// uses the types to gate assignability up front (spec/design/grammar.md §24).
pub(crate) struct SelectResult {
    column_names: Vec<String>,
    column_types: Vec<ResolvedType>,
    rows: Vec<Vec<Value>>,
    cost: i64,
}

/// How a [`SelectPlan`]'s output is emitted (spec/design/streaming.md §4, S4). A SELECT runs its
/// **blocking part** (scan/join/WHERE/window/sort/GROUP BY/DISTINCT) into an intermediate buffer,
/// then emits a row at a time. [`exec_select_emit`](Engine::exec_select_emit) returns this so the
/// emission can be driven **eagerly** (the materialized `execute()` path — [`exec_select_plan`]
/// drains it into a `Vec`) or **lazily** (the `query()` path — `BufferedScan` yields it row by row,
/// bounding output memory and short-circuiting a caller's early exit). Both drives charge the
/// identical units at the identical sites, so a fully-drained query observes the same rows + total
/// cost (streaming.md §6).
pub(crate) enum Emitter {
    /// The general blocking path's intermediate buffer, windowed to `[start, end)`. Each emitted
    /// row charges `row_produced`; in [`EmitMode::Project`] it additionally evaluates the projection
    /// list (charging its `operator_eval`s), and in [`EmitMode::Identity`] the buffer rows are
    /// already the projected output (the DISTINCT dedup projected them up front — the §3 asymmetry)
    /// so emission only charges `row_produced`.
    Buffer {
        rows: Vec<Vec<Value>>,
        start: usize,
        end: usize,
        mode: EmitMode,
    },
    /// A fully-formed result the special input-streaming paths (`exec_streaming_scan` /
    /// `exec_index_order_scan` / `exec_streaming_join`) already projected AND charged `row_produced`
    /// for. Emission just hands the rows out — no further charge.
    Final { rows: Vec<Vec<Value>> },
    /// The streaming external sort's output, yielded **lazily** from the [`SortedRows`] pull iterator
    /// (spec/design/streaming.md §4/§7) — positioned past the `OFFSET`, with `remaining` windowed rows
    /// still to emit. Each emission pulls the next sorted row, charges `row_produced`, and evaluates
    /// the projection list (charging its `operator_eval`s). So the output `Vec` is **never built** and
    /// a caller's early exit skips the projection (and `row_produced`) of the rows it never pulls — the
    /// follow-on win over wrapping the materialized output as [`Emitter::Final`]. Under full drain the
    /// rows + total cost are byte-identical to the eager path (it pulls every windowed row, charging
    /// the same units at the same sites — §6).
    Sorted {
        sorted: crate::spill::SortedRows,
        remaining: usize,
    },
    /// The columnar projection fast path (`project_columnar`, packed-leaf.md §11 Track A2/A3). `cols`
    /// holds the pre-gathered dense per-column lanes (indexed by table ordinal) and `proj_cols` the
    /// projection's column indices into them; emission builds output row `j` as `[cols[proj_cols[0]][l],
    /// …]` where `l = sel[j]` (the A3 selection vector's survivor — a filtered scan) or `j` (all rows,
    /// `sel = None`) — a bare-column projection with no full-width row, charging `row_produced` per
    /// windowed row exactly like the `Project` path over a bare column ref (a zero-cost slot read, so the
    /// lane read is cost-identical). Windowed to `[start, end)`. Lazy on the `query()` path: an early exit
    /// skips the `row_produced` of the rows it never pulls.
    Columnar {
        cols: Vec<Vec<Value>>,
        proj_cols: Vec<usize>,
        sel: Option<Vec<i32>>,
        start: usize,
        end: usize,
    },
}

/// Whether an [`Emitter::Buffer`] row still needs projecting on emission (spec/design/streaming.md §4).
#[derive(Clone, Copy)]
pub(crate) enum EmitMode {
    /// Evaluate the projection list against the buffer row (charging projection `operator_eval`s).
    Project,
    /// The buffer row is already the projected output (DISTINCT pre-projected for its dedup key).
    Identity,
}

/// The default serialization page size (8 KiB — spec/design/storage.md §3), used for a fresh
/// in-memory or newly-created database when no explicit size is given.
pub const DEFAULT_PAGE_SIZE: u32 = 8192;

/// The default per-handle input-SQL byte limit (1 MiB — CLAUDE.md §13; spec/design/api.md §8,
/// cost.md §7). The §13 input-size gate's default ceiling: generous for hand-written / ORM SQL,
/// yet bounds the parse tree to a few MB so unbounded untrusted input cannot exhaust memory. A
/// caller raises it (trusted bulk loads) or sets `0` for unlimited via
/// [`Engine::set_max_sql_length`]. Identical across cores (§8).
pub const DEFAULT_MAX_SQL_LENGTH: usize = 1 << 20;

/// The default per-session storage budget for SESSION-LOCAL temporary tables, in **bytes**
/// (spec/design/temp-tables.md §7). Temp tables RETAIN bytes across statements, which neither the
/// per-statement cost ceiling (`max_cost`) nor the cumulative budget (`lifetime_max_cost`) bounds, so
/// `temp_buffers` is the §13 gate that does: the instant a session's resident temp storage (measured
/// in the same deterministic logical-byte estimator `work_mem` uses) would exceed it, the write
/// aborts `54P03`. `0` ⇒ unlimited (a trusted handle); the untrusted-scratch pattern leaves this at a
/// modest default. Identical across cores (§8); the abort point is part of the cross-core contract.
pub const DEFAULT_TEMP_BUFFERS: usize = 32 << 20;

/// The maximum composite-type nesting depth (CLAUDE.md §13; spec/design/cost.md §7b). A composite
/// type's depth is the length of its deepest chain of nested composites, counting itself: a row of
/// scalars is depth 1, `CREATE TYPE b AS (x a)` is `1 + depth(a)`, and an array field counts as its
/// element (array levels are not composite levels — `composite_ref` looks through one array level
/// the same way). A `CREATE TYPE` whose result would exceed this is rejected `54001`, and a loaded
/// catalog that exceeds it is treated as corrupt `XX001` — bounding the native recursion of every
/// derived walk (value codec, comparator, `record_out`/`record_in`, `resolve_col_type`) at the two
/// producers (DDL + load) so all downstream walks are transitively stack-safe. A fixed, cross-core
/// constant like `MAX_EXPR_DEPTH` (§8). The chain is built across many cheap statements, so neither
/// the per-statement input-size cap nor the parser nesting counter sees it (cost.md §7).
pub const MAX_COMPOSITE_DEPTH: usize = 32;

/// An immutable committed (or in-progress working) database state — the catalog + each table's
/// store + the commit counter (spec/design/transactions.md §2). The committed state is one of
/// these; a write transaction builds a new one from it (path-copying the persistent stores, so the
/// prior state is provably unchanged — pmap.rs / §3). A reader holds a `Snapshot` and is thereby
/// stable for its life: a later commit produces a *new* `Snapshot` and never mutates this one.
/// (P5.3a is single-handle; sharing a `Snapshot` across threads is P5.3b.)
// The catalog-metadata maps below are each `Arc<HashMap<..>>` so `#[derive(Clone)]` on `Snapshot`
// is a handful of `Arc` bumps — O(1), NOT O(catalog size). This matters because the read path clones
// a snapshot PER QUERY (`snapshot_engine`, to hand the cursor a frozen owned view), so a deep catalog
// clone made a point lookup's cost scale with the number of unrelated tables/types/sequences in the
// database (each `Table` deep-copies its column/index/constraint `Vec`s + owned name `String`s). The
// heavy per-store data was already shared (`PMap` is a persistent map, `TableStore`'s pager/col-types
// are `Arc`); this extends that O(1)-clone discipline to the catalog metadata itself. Writers mutate
// copy-on-write via `Arc::make_mut` (below), so a schema change clones only the map it touches, once,
// on the rare write path — never on a read.
#[derive(Clone, Default)]
pub struct Snapshot {
    /// The snapshot's version — the commit counter (transactions.md §8; the watermark unit).
    pub(crate) txid: u64,
    /// The catalog generation — a monotonic counter bumped by every schema mutation (CREATE/DROP/
    /// ALTER of a table/type/index), carried forward across `clone()` (rides `#[derive(Clone)]`).
    /// Unlike `txid` it does NOT move on data writes and is defined for in-memory databases too, so
    /// a prepared statement's relation signature includes it alongside database identity, table
    /// name, and estimator revision (spec/design/api.md §2.4).
    /// NOT bumped by sequence `nextval` (a data write on the nextval path), only by sequence DDL — a
    /// SELECT plan binds no sequence.
    pub(crate) cat_gen: u64,
    /// Opaque, non-persisted identity for this database domain. Snapshot/transaction clones share
    /// it; every fresh create/open/attachment gets a new token (estimator.md §6).
    pub(crate) estimator_identity: std::sync::Arc<EstimatorDatabaseIdentity>,
    /// Base revision shared by tables not mutated under this identity, plus per-table overrides.
    /// Replacing an `Arc` is an exact collision-free revision change; clone/rollback follow the
    /// snapshot automatically and no file-format state is involved.
    estimator_base_revision: std::sync::Arc<EstimatorRevision>,
    estimator_revisions: std::sync::Arc<HashMap<String, std::sync::Arc<EstimatorRevision>>>,
    /// Persisted, transactional P9 column facts, keyed by lowercased table name then column ordinal.
    statistics: std::sync::Arc<HashMap<String, HashMap<usize, ColumnStatistics>>>,
    tables: std::sync::Arc<HashMap<String, Table>>,
    /// User-defined composite (row) types, keyed by lowercased name (spec/design/composite.md).
    /// A database-level object set, separate from `tables`; serialized into the catalog's
    /// composite-type entries (spec/fileformat/format.md). Sorted by key when serialized so
    /// hash-map iteration order never leaks (CLAUDE.md §8).
    types: std::sync::Arc<HashMap<String, CompositeType>>,
    stores: std::sync::Arc<HashMap<String, TableStore>>,
    /// Each secondary index's B-tree (spec/design/indexes.md §3): a `TableStore` with ZERO
    /// value columns (entry keys only — the on-disk empty-payload record), keyed by the
    /// lowercased index name (index names live in the relation namespace, globally unique).
    /// Which table owns an index is recorded in that table's `Table::indexes`.
    index_stores: std::sync::Arc<HashMap<String, TableStore>>,
    /// Sequences, keyed by lowercased name (spec/design/sequences.md). A database-level object set
    /// separate from `tables`/`types`; serialized into the catalog's sequence entries
    /// (spec/fileformat/format.md, `entry_kind = 2`). The mutable counter (`last_value`/`is_called`)
    /// lives here, so `nextval` advances the working snapshot and rolls back with it (sequences.md §5).
    sequences: std::sync::Arc<HashMap<String, SequenceDef>>,
    /// Loaded collations, keyed by their exact (CASE-SENSITIVE) name — collation names are quoted
    /// identifiers (`"en-US"`, spec/design/collation.md §1). `C` is never stored (table-free, built
    /// in). Imported by the host `db.import_collation`. `Arc` so a resolved comparison / sort key can
    /// hold a cheap reference. Persisted as catalog `entry_kind = 3` baked snapshots
    /// (`format_version` 17, slice 1d — spec/fileformat/format.md, spec/design/collation.md §5).
    collations: std::sync::Arc<HashMap<String, std::sync::Arc<Collation>>>,
    /// The per-database default collation name, or `None` for `C` (spec/design/collation.md §1/§5).
    /// An un-annotated `text` column inherits this at CREATE TABLE. Settable to any loaded collation
    /// (`db.set_default_collation`); persisted as the `is_default` flag bit on that collation's
    /// `entry_kind = 3` snapshot, restored on load. `C` ⇒ no snapshot carries the bit.
    default_collation: Option<String>,
    /// Each GiST index's **resident R-tree** (spec/design/gist.md §4.1), keyed by the lowercased
    /// index name. The leaf-key store (`index_stores`) stays the maintained source of truth (so all
    /// insert/update/delete index maintenance is reused, gist.md §4.1); this tree is the acceleration
    /// structure the planner descends. Rebuilt **canonically** (`build_from_leaf_keys` — content-
    /// deterministic, a pure function of the leaf SET, gist.md §3) at every mutating statement and on
    /// load, so a committed snapshot always carries a fresh, cross-core-identical tree a SELECT can
    /// descend lock-free (the immutable-snapshot read path, §3). `Arc` so a snapshot clone stays O(1)
    /// (the tree is replaced wholesale on rebuild, never mutated in place). The on-disk form is the
    /// persisted R-tree (page types 5/6); this in-memory tree is rebuilt from the loaded leaf store.
    gist_trees: std::sync::Arc<HashMap<String, std::sync::Arc<crate::gist::GistTree>>>,
    /// This snapshot's domain paging context — the pager a store created IN-SESSION
    /// (`put_table_resolved` / `put_index_store` / `put_index`) binds at creation, so it joins the
    /// post-commit residency flip (`demote_clean_leaves`) instead of staying a fully-resident decoded
    /// tree forever. Every domain sets it: the main file/in-memory snapshot binds the storage
    /// identity's paging at load/create (format.rs / file.rs), a session-local temp snapshot its
    /// per-domain `MemoryBlockStore` pager (spec/design/temp-tables.md §6), an attachment its own
    /// storage's pager. `None` only on a bare scratch engine that never persists. Stores loaded FROM
    /// a file attach the same pager individually at load; binding at creation is what covers the
    /// stores load never sees. Rides `#[derive(Clone)]` (an `Arc` bump) so a tx's working snapshot
    /// creates stores against the same domain page space, and `#[derive(Default)]` (`None`).
    /// NEVER serialized.
    store_paging: Option<std::sync::Arc<crate::paging::SharedPaging>>,
}

/// Non-persisted equality tokens used only by prepared-plan cache validation.
#[derive(Default)]
pub(crate) struct EstimatorDatabaseIdentity {
    _marker: u8,
}
#[derive(Default)]
pub(crate) struct EstimatorRevision {
    _marker: u8,
}

/// One FOREIGN KEY dependent surfaced by a multi-table `DROP TABLE`'s dependency scan
/// (spec/design/grammar.md §13): an FK on a table that *survives* the drop, referencing a table
/// being dropped. `RESTRICT` formats `ref_table_name`/`fk_name`/`dropped_name` into its 2BP01
/// detail; `CASCADE` uses `ref_table_key`/`fk_name` to remove the now-dangling constraint.
pub(crate) struct FkDependent {
    /// Lowercased catalog key of the (surviving) referencing table — for the CASCADE removal.
    pub ref_table_key: String,
    /// The FK constraint's name.
    pub fk_name: String,
    /// Canonical name of the referencing table — for the RESTRICT detail.
    pub ref_table_name: String,
    /// Canonical name of the dropped table the FK references — for the RESTRICT detail.
    pub dropped_name: String,
}

/// The database handle: the last **committed** `Snapshot` plus, while a transaction is open, the
/// writer's working snapshot (CLAUDE.md §3, spec/design/transactions.md §2). Reads run against the
/// *visible* snapshot — the open transaction's `working` if any, else `committed`; a write mutates
/// `working` and commit swaps `committed := working` (rollback just drops `working`, since
/// `committed` was never touched). Every write — autocommit included — runs as a transaction, which
/// unifies the two paths.
pub struct Engine {
    /// The last committed, immutable state — what fresh readers (and autocommit reads) see.
    pub(crate) committed: Snapshot,
    /// The **default session** (spec/design/session.md §2.1): the per-connection state this handle
    /// runs statements through — the open transaction (the `Idle`/`Open`/`Failed` machine, §2.2),
    /// the relocated settings (`max_cost`, `max_sql_length`, `work_mem`, the entropy/clock seam),
    /// and the `currval`/`lastval` session state. A bare `Engine` IS committed storage + this one
    /// long-lived stateful default session; the convenience methods (`execute`/`begin`/
    /// `set_max_cost`/…) operate on it. `db.session(opts)` mints additional, independent sessions
    /// (run sequentially on this single-threaded handle by swapping into this slot).
    pub(crate) session: SessionState,
    /// The backing file path (`None` for an in-memory database). Set by the host API
    /// `open`/`create` (spec/design/api.md §2); `commit` writes here.
    pub(crate) path: Option<std::path::PathBuf>,
    /// The host scratch directory for external-sort runs. File hosts set this independently of
    /// `path` (normally to the OS temp directory), so a read-only database can spill without writing
    /// beside its file. `None` for hosts with no spill backing (in-memory / OPFS).
    pub(crate) spill_dir: Option<std::path::PathBuf>,
    /// The page size this database serializes with (from the file on open, from create opts,
    /// else `DEFAULT_PAGE_SIZE`). Fixed for the life of a file.
    pub(crate) page_size: u32,
    /// The on-disk page high-water mark — the page index an incremental commit extends at when the
    /// free-list is exhausted (spec/fileformat/format.md). Set from the file's meta on `open`, from
    /// the initial image on `create`; `0` (unused) for an in-memory database.
    pub(crate) page_count: u32,
    /// The free-list (P6.2 + v25): page indices a prior root abandoned, reusable by the next
    /// incremental commit (spec/fileformat/format.md *Reclamation*). **Read from the persisted chain
    /// on open** (v25 — meta offset 28), and returned to within-session by periodic compaction
    /// ([`compacted_free_list`]); drawn lowest-first before the file is extended. A page leaves the
    /// list only by being allocated into a new committed version, so it is reachable from no live
    /// snapshot and reuse is torn-write-safe. Empty for a freshly-created file (a from-scratch image
    /// leaks nothing).
    pub(crate) free_pages: Vec<u32>,
    /// The live (reachable) page count recorded at this handle's last within-session compaction — the
    /// cheap periodic trigger basis (v25): a bare-`Engine` file commit re-runs the reclamation walk
    /// only once the high-water passes ~2× it, mirroring [`crate::shared::Storage`]. `0` for an
    /// in-memory database (no persistence).
    pub(crate) live_at_compaction: u32,
    /// The version the current `free_pages` list is "as of" — the last compaction's txid, or the
    /// committed version at open. It gates within-session reuse under the reader-liveness watermark
    /// (transactions.md §8): a page dead at generation G is reusable only once no reader pins a version
    /// older than G. A bare single-handle `Engine` has no live registry (oldest_live == committed), so the
    /// gate always passes and the byte layout is unchanged.
    pub(crate) free_gen_txid: u64,
    /// The shared paging context for a file-backed database (spec/design/pager.md): the open pager
    /// (kept for the handle's life) + the bounded leaf buffer pool, shared (`Arc`) with every table
    /// store so reads fault `OnDisk` leaves through the one pool. The load reads pages through it and
    /// every commit writes through it. `None` for an in-memory database (`persist` is then a no-op);
    /// set by `open`/`create`, dropped by `close`.
    pub(crate) paging: Option<std::sync::Arc<crate::paging::SharedPaging>>,
    /// Whether this handle was opened **read-only** (spec/design/api.md §2.1,
    /// [`crate::file::OpenOptions::read_only`]). A read-only handle behaves like PostgreSQL
    /// hot standby: every transaction defaults to READ ONLY, an explicit `BEGIN READ WRITE`
    /// (or `begin(true)`) is `25006`, and an autocommit write is `25006` — so no commit ever
    /// publishes and the file is never written (it is opened without write access). Always
    /// `false` for an in-memory or normally-opened database.
    pub(crate) read_only: bool,
    /// The SESSION-LOCAL temp domain's storage identity (temp-tables.md §6): the private in-RAM
    /// `MemoryBlockStore` + pager + pinned pool its temp tables ride, with within-session compaction on.
    /// Created lazily on the first session-local temp DDL ([`Storage::new_temp`]); `None` until then. Its
    /// `page_count` is the domain's footprint — the page-based temp budget.
    pub(crate) temp_storage: Option<crate::shared::Storage>,
    /// The count of this handle's live streaming cursors (a `query` pull source, not a materialized
    /// result). A streaming cursor pins a snapshot it faults lazily, so while one is open a temp-domain
    /// compaction (`persist_temp` → `maybe_compact`) must NOT reclaim pages — it could free one the cursor
    /// still faults. Incremented when a streaming `Rows` opens (shared.rs), decremented on its `Drop`
    /// (via an `OpenStreamGuard` bundled into the cursor's pin) — hence the `Arc<AtomicUsize>`: the guard
    /// outlives the `&mut self` borrow that built the cursor.
    pub(crate) open_streams: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// The shared core this engine's session belongs to (attached-databases.md §5), or `None` for a
    /// bare/transient engine (a test [`Engine::new`], a `snapshot_engine`, a `from_snapshot` read view —
    /// none of which commit attachments). It is the engine's route to the core-owned attachment registry
    /// during a commit persist; the READ view of attachments is the pinned `attached_committed` below.
    /// Set at session mint (shared.rs).
    pub(crate) core: Option<std::sync::Arc<crate::shared::Shared>>,
    /// The PINNED committed root of every host-attached DATABASE-scoped database
    /// (attached-databases.md §5), keyed by lowercased name — this session's stable read view, snapshot
    /// isolated: refreshed from the core's published `Roots::attached` at each autocommit statement
    /// (`refresh_committed`) and pinned for the life of an explicit `BEGIN` block. Empty when nothing is
    /// attached. Session-local temp is NOT here (it rides `SessionState::temp_committed`); this holds
    /// only the DATABASE-scoped roots. Set at session mint; adopted from a tx's `attach_working` on a
    /// successful commit.
    pub(crate) attached_committed: HashMap<String, std::sync::Arc<Snapshot>>,
    /// Relations whose estimator revision was already advanced by the current top-level statement.
    /// Data-modifying CTEs may touch the same table more than once; the P2 contract advances it once.
    pub(crate) estimator_touched: HashSet<(String, String)>,
    /// Populated only while EXPLAIN ANALYZE executes its inner statement. Exact operator sub-meter
    /// snapshots are recorded here; ordinary execution pays only a RefCell/Option nil check.
    pub(crate) explain_actual: std::cell::RefCell<Option<ActualCostProfile>>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct ActualCostKey {
    frame: usize,
    node: String,
}

#[derive(Default)]
struct ActualCostFrame {
    by_node: HashMap<ActualCostKey, Vec<i64>>,
    inclusive: i64,
}

#[derive(Default)]
pub(crate) struct ActualCostProfile {
    pub(crate) by_node: HashMap<ActualCostKey, Vec<i64>>,
    frames: Vec<ActualCostFrame>,
    folded: HashMap<ActualCostKey, Vec<i64>>,
    fold_suppress: usize,
}

impl ActualCostProfile {
    fn key(&self, node: String) -> ActualCostKey {
        ActualCostKey {
            frame: self.frames.len(),
            node,
        }
    }

    pub(crate) fn record(&mut self, node: String, cost: i64) {
        let key = self.key(node);
        let target = self
            .frames
            .last_mut()
            .map(|frame| &mut frame.by_node)
            .unwrap_or(&mut self.by_node);
        target.entry(key).or_default().push(cost);
    }

    pub(crate) fn record_parent(&mut self, node: String, mut cost: i64) {
        let key = self.key(node);
        if let Some(frame) = self.frames.last_mut() {
            if let Some(pending) = self.folded.get_mut(&key)
                && !pending.is_empty()
            {
                frame.inclusive += pending.remove(0);
            }
            cost += frame.inclusive;
        }
        let target = self
            .frames
            .last_mut()
            .map(|frame| &mut frame.by_node)
            .unwrap_or(&mut self.by_node);
        target.entry(key).or_default().insert(0, cost);
    }

    pub(crate) fn record_folded(&mut self, frame: usize, node: &str, cost: i64) {
        if self.fold_suppress > 0 || cost == 0 {
            return;
        }
        self.folded
            .entry(ActualCostKey {
                frame,
                node: node.to_string(),
            })
            .or_default()
            .push(cost);
    }

    pub(crate) fn suppress_folded(&mut self) {
        self.fold_suppress += 1;
    }

    pub(crate) fn unsuppress_folded(&mut self) {
        self.fold_suppress -= 1;
    }

    pub(crate) fn begin_frame(&mut self) {
        self.frames.push(ActualCostFrame::default());
    }

    pub(crate) fn end_frame(&mut self) {
        let Some(frame) = self.frames.pop() else {
            return;
        };
        let target = self
            .frames
            .last_mut()
            .map(|frame| &mut frame.by_node)
            .unwrap_or(&mut self.by_node);
        for (key, costs) in frame.by_node {
            target.entry(key).or_default().extend(costs);
        }
    }

    pub(crate) fn discard_frame(&mut self) {
        self.frames.pop();
    }

    pub(crate) fn apply(&self, rows: &[Vec<Value>], frame_depths: &[usize], actual: &mut [i64]) {
        let mut used: HashMap<ActualCostKey, usize> = HashMap::new();
        let mut cte_used: HashMap<ActualCostKey, usize> = HashMap::new();
        let mut suppressed_depth: Option<i64> = None;
        for (i, row) in rows.iter().enumerate() {
            let Some(Value::Text(node)) = row.get(1) else {
                continue;
            };
            let depth = match row.first() {
                Some(Value::Int(depth)) => *depth,
                _ => continue,
            };
            if let Some(suppressed) = suppressed_depth {
                if depth > suppressed {
                    continue;
                }
                suppressed_depth = None;
            }
            let key = ActualCostKey {
                frame: frame_depths.get(i).copied().unwrap_or(0),
                node: node.clone(),
            };
            let at = used.entry(key.clone()).or_default();
            if let Some(cost) = self.by_node.get(&key).and_then(|values| values.get(*at)) {
                if i != 0 {
                    actual[i] = *cost;
                } // the rendered plan root already owns the exact whole-statement total
                *at += 1;
            }
            if let Some(name) = node.strip_prefix("CTE ")
                && !node.starts_with("CTE Scan ")
            {
                let marker_key = ActualCostKey {
                    frame: frame_depths.get(i).copied().unwrap_or(0),
                    node: format!("@cte-body {name}"),
                };
                let at = cte_used.entry(marker_key.clone()).or_default();
                if let Some(marker) = self
                    .by_node
                    .get(&marker_key)
                    .and_then(|values| values.get(*at))
                {
                    if *marker == 0 {
                        suppressed_depth = Some(depth);
                    }
                    *at += 1;
                }
            }
        }
    }
}

/// An RAII counter for a live streaming cursor (temp-tables.md §6): built by [`Engine::open_stream_guard`]
/// (which increments [`Engine::open_streams`]) and bundled into the cursor's pin, so its `Drop`
/// decrements the count when the cursor is closed or dropped — even though the cursor ([`crate::Rows`])
/// outlives the `&mut Engine` borrow that built it (hence the `Arc<AtomicUsize>`). While the count is
/// non-zero a session-local temp compaction defers, so a page the cursor may still fault is never freed.
pub(crate) struct OpenStreamGuard {
    count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Drop for OpenStreamGuard {
    fn drop(&mut self) {
        self.count
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The relocatable session settings (spec/design/session.md §3 — the bucket-A envelope subset that
/// has landed in S1): the per-statement cost ceiling, the input-size limit, and the work-memory
/// budget. Passed to [`Engine::session`] to mint an additional session; an absent field takes its
/// default. (The entropy/clock seam is injected via [`SessionState::set_random_source`] /
/// [`SessionState::set_clock_source`], not here.)
#[derive(Clone, Debug)]
pub struct SessionOptions {
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
    /// The per-session storage budget for session-local temp tables, in **bytes**
    /// (spec/design/temp-tables.md §7); `0` ⇒ unlimited. Default [`DEFAULT_TEMP_BUFFERS`]. Bounds the
    /// RETAINED temp storage neither cost ceiling covers — an over-budget temp write aborts `54P03`.
    pub temp_buffers: usize,
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
            max_cost: 0,
            lifetime_max_cost: 0,
            max_sql_length: DEFAULT_MAX_SQL_LENGTH,
            lock_timeout_ms: 0,
            work_mem: crate::spill::DEFAULT_WORK_MEM,
            default_privileges: PrivilegeSet::ALL_TABLE,
            allow_ddl: true,
            allow_temp_ddl: None,
            temp_buffers: DEFAULT_TEMP_BUFFERS,
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
    fn of(tx: &Option<ActiveTx>) -> TxStatus {
        match tx {
            None => TxStatus::Idle,
            Some(t) if t.is_failed() => TxStatus::Failed,
            Some(_) => TxStatus::Open,
        }
    }
}

/// The per-connection **session** state (spec/design/session.md §2.1): the configured, stateful
/// context a host runs statements through, un-fused from the committed storage on [`Engine`]. It
/// owns the open transaction (the `Idle`/`Open`/`Failed` machine), the relocated handle settings,
/// the entropy/clock seam, and the `currval`/`lastval` session state. A [`Engine`] holds one as
/// its long-lived default session; [`Engine::session`] mints additional independent ones that run
/// sequentially on a single-threaded handle (by swapping into the default slot for a call).
pub struct SessionState {
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
    /// whose `guard` consults it — so a flipped [`CancellationToken`](crate::CancellationToken) aborts a
    /// long-running statement with `57014` at the next metering point, not only at the cursor boundary.
    /// `None` (the default) ⇒ no cancellation, so the hot path is untouched (CLAUDE.md §8). Because the
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
    /// The entropy + clock seam for the uuid generators / clock functions (spec/design/entropy.md):
    /// two host-injectable functions (a random source + a clock), each defaulting to the platform
    /// primitive. Tests inject `seeded_random_source` + `fixed_clock` (the `# seed:`/`# clock:`
    /// directives) for byte-identical cross-core output.
    ///
    /// Behind an `Rc` so a streaming cursor's frozen snapshot engine (streaming.md §5) **shares** the
    /// live session's seam rather than copying it (the seam holds boxed `FnMut` closures and so is not
    /// `Clone`): a `uuidv7()` / `now()` in a streaming projection then draws from the same injected
    /// source as the eager path, keeping the result byte-identical under full drain (streaming.md §6).
    pub(crate) seam: std::rc::Rc<crate::seam::Seam>,
    /// **SessionState** `currval` state (spec/design/sequences.md §6): the last value `nextval`/
    /// `setval(…,true)` produced **in this session** for each sequence (lowercased name). NOT in the
    /// snapshot and NOT persisted — strictly session-local, as in PostgreSQL.
    pub(crate) session_seq: HashMap<String, i64>,
    /// **SessionState** `lastval` state (sequences.md §6): the lowercased name of the sequence the most
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
    /// The per-session temp-table storage budget in **bytes** (spec/design/temp-tables.md §7); `0` ⇒
    /// unlimited. An over-budget temp write aborts `54P03`.
    pub(crate) temp_buffers: usize,
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
    /// single pre-statement snapshot every sub-statement reads, so the data-modifying CTEs and the
    /// primary cannot observe each other's table writes (their writes still accumulate into the
    /// transaction's `working`). Set by the writable-CTE orchestrator before the first sub-statement
    /// runs and cleared when it finishes (success or error); `None` for every other statement, where
    /// reads fall through to `working`/`committed` as usual ([`Engine::read_snap`]).
    pub(crate) read_pin: Option<Snapshot>,
}

/// Validate + canonicalize a session-variable name (spec/design/session.md §6.1). A variable must be
/// **namespaced** like a PostgreSQL custom GUC — a dotted name (`myapp.tenant`); a non-dotted name
/// would be a built-in setting, and v1 exposes none through this map (the `time_zone` built-in is a
/// separate slice), so it is `42704`. Returns the case-folded (lowercase, PG GUC names are
/// case-insensitive) map key.
fn require_custom_var_name(name: &str) -> Result<String> {
    if name.contains('.') {
        Ok(name.to_ascii_lowercase())
    } else {
        Err(EngineError::new(
            SqlState::UndefinedObject,
            format!("unrecognized configuration parameter: {name}"),
        ))
    }
}

impl Default for SessionState {
    fn default() -> Self {
        SessionState::new()
    }
}

impl SessionState {
    /// A fresh default session: no open transaction, default settings, empty sequence state.
    pub fn new() -> Self {
        SessionState::with_options(SessionOptions::default())
    }

    /// A fresh session configured from `opts` (spec/design/session.md §2.1); the rest of the
    /// per-connection state (transaction, seam, sequence state) starts empty/default.
    pub fn with_options(opts: SessionOptions) -> Self {
        let mut privileges = crate::privileges::Privileges::default();
        privileges.set_default_table(opts.default_privileges);
        SessionState {
            tx: None,
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
            seam: std::rc::Rc::new(crate::seam::Seam::default()),
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
            temp_committed: Snapshot::default(),
            vars: HashMap::new(),
            // Resolve the configured zone once; an invalid value falls back to UTC at mint (the
            // validated path is `set_time_zone`, which surfaces 22023). timezones.md §9.4.
            time_zone: crate::timezone::resolve_zone(&opts.time_zone)
                .unwrap_or(crate::timezone::ZoneRef::Fixed(0)),
            read_pin: None,
        }
    }

    /// Set the session **time zone** (spec/design/session.md §6.2, timezones.md §9.4): the zone a
    /// `timestamptz` is decomposed *in*. Accepts `UTC`, a fixed `±HH:MM` offset, or a named IANA zone
    /// a loaded `JTZ` bundle provides; a name no bundle provides (and not a built-in) is **`22023`**
    /// (`invalid_parameter_value`), the value unchanged. The resolved zone is cached on the session.
    pub fn set_time_zone(&mut self, zone: &str) -> Result<()> {
        match crate::timezone::resolve_zone(zone) {
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
        Meter::for_session(
            self.max_cost,
            Lifetime {
                total: self.lifetime_total.clone(),
                limit: self.lifetime_max_cost,
            },
            self.cancel.clone(),
        )
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
    /// Set the per-session temp-table storage budget in bytes; `0` ⇒ unlimited.
    pub fn set_temp_buffers(&mut self, bytes: usize) {
        self.temp_buffers = bytes;
    }
    /// The current per-session temp-table storage budget.
    pub fn temp_buffers(&self) -> usize {
        self.temp_buffers
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
    /// Inject a random source for the uuid generators (entropy.md §6).
    pub fn set_random_source(&mut self, f: crate::seam::RandomSource) {
        self.seam.set_random(f);
    }
    /// Clear the injected random source (return to the OS CSPRNG).
    pub fn clear_random_source(&mut self) {
        self.seam.clear_random();
    }
    /// Inject a clock source for `uuidv7` / the clock functions (entropy.md §6).
    pub fn set_clock_source(&mut self, f: crate::seam::ClockSource) {
        self.seam.set_clock(f);
    }
    /// Clear the injected clock source (return to the wall clock).
    pub fn clear_clock_source(&mut self) {
        self.seam.clear_clock();
    }
}

/// An open transaction (spec/design/transactions.md §4.2). `writable` is the access mode — READ
/// WRITE may write, READ ONLY is read-only (a write inside → 25006). `failed` marks an aborted
/// block (after a statement error every later statement but COMMIT/ROLLBACK is 25P02 — §6).
/// `working` is the transaction's snapshot: for a writable tx it is mutated in place and published
/// at commit; for a read-only tx it is the committed snapshot pinned at BEGIN (read-your-snapshot,
/// never mutated). Either way `committed` is untouched until commit, so ROLLBACK just drops this.
pub(crate) struct ActiveTx {
    writable: bool,
    /// The block's aborted flag (spec/design/transactions.md §6). An `Arc<AtomicBool>` rather than a
    /// plain `bool` so a **streaming/deferred read cursor born in this block can poison it from its
    /// drain** (a mid-drain trap aborts the block, PG-faithful): the cursor outlives the `&mut Engine`
    /// borrow, so it holds a clone of this flag and flips it on error — the same shared-`Arc` channel
    /// the open-stream guard uses (`Engine::attach_block_poison`). Cloning is scoped to this block, so
    /// a cursor that outlives its block only touches an orphaned flag (harmless).
    failed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    working: Snapshot,
    /// The handle's `currval`/`lastval` session state (spec/design/sequences.md §6) captured when
    /// this transaction opened. A `nextval`/`setval` inside the block updates the handle's session
    /// state per-statement (so an in-block `currval` sees its own advance), but those updates must
    /// **roll back** with the transaction (§5) — so ROLLBACK (and a failed/read-only COMMIT)
    /// restores these, while a successful COMMIT keeps the advanced state.
    saved_session_seq: HashMap<String, i64>,
    saved_session_last_name: Option<String>,
    /// The transaction's working copy of the session's temp-table snapshot
    /// (spec/design/temp-tables.md §5): cloned from [`SessionState::temp_committed`] at tx open (cheap —
    /// persistent stores clone O(1)), mutated by temp DDL/DML, adopted back into `temp_committed` on a
    /// successful COMMIT and discarded on ROLLBACK. The temp analogue of `working`, kept SEPARATE so
    /// it is never serialized.
    temp_working: Snapshot,
    /// Whether this transaction mutated the **main** (persistent) snapshot — set by
    /// [`Engine::working_mut`]. Drives the commit's persist decision so a transaction that touched
    /// ONLY temp tables makes zero file writes (temp-tables.md §2).
    main_dirty: bool,
    /// Whether this transaction mutated the **session-local temp** snapshot — set by
    /// [`Engine::temp_working_mut`]. With `main_dirty` it decides whether COMMIT persists the main
    /// image (a pure-temp commit skips it; an empty block still persists, preserving prior behavior).
    temp_dirty: bool,
    /// The transaction's working copy of each host-attached database's snapshot
    /// (attached-databases.md §5), keyed by lowercased attachment name — the attachment analogue of
    /// `temp_working`. Cloned lazily from [`Engine::attached_committed`] on the first write to that
    /// attachment ([`Engine::attach_write_snap`]), so a read-only cross-attachment query allocates
    /// nothing here. Adopted into `attached_committed` + persisted+published on a successful COMMIT,
    /// discarded on ROLLBACK. Empty until an attachment is written.
    attach_working: HashMap<String, Snapshot>,
    /// Which attachments this transaction mutated (lowercased names) — the per-attachment analogue of
    /// `main_dirty`/`temp_dirty`, the set the commit persists + publishes.
    attach_dirty: HashSet<String>,
}

impl ActiveTx {
    /// Whether the block is aborted (spec/design/transactions.md §6) — reads the shared `failed` flag.
    fn is_failed(&self) -> bool {
        self.failed.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Abort the block (set the shared `failed` flag). Takes `&self` because the flag is an
    /// `Arc<AtomicBool>` — so a poison from a lane cursor's drain (which only holds `&self`-equivalent
    /// access via a clone) reaches the same store.
    fn mark_failed(&self) {
        self.failed
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
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
/// joined row. A `qualifier_only` relation is visible ONLY to qualified references — the
/// RETURNING `old`/`new` row-version pseudo-relations (grammar.md §32): bare-column
/// resolution skips it (no new ambiguity), every other statement never builds one.
#[derive(Clone)]
pub(crate) struct ScopeRel<'a> {
    label: String,
    table: &'a Table,
    offset: usize,
    qualifier_only: bool,
    /// `Some(i)` when this relation is a reference to CTE `i` (spec/design/cte.md) rather than a
    /// base table — its `table` is the binding's synthetic relation and exec delivers its rows from
    /// the `CteCtx`. `None` for a base table / SRF / pseudo-relation.
    cte: Option<usize>,
    /// The relation's explicit database qualifier (attached-databases.md §3), carried from the
    /// `TableRef` so the store is re-looked-up in the right database at exec (a store is resolved by
    /// name per-access). `None` for a bare (implicit-scope) name — then the scoped funnels fall through
    /// to the temp-first walk, so this is behavior-neutral for every unqualified query.
    db: Option<String>,
}

impl ScopeRel<'_> {
    /// Whether this relation targets a HOST-ATTACHED database (attached-databases.md §3) rather than
    /// the implicit `main`/`temp` scope. Index/PK/GiST/GIN bound pushdown is gated OFF for an attachment
    /// relation this slice — the bounded-scan exec path resolves index stores through the UNSCOPED
    /// funnel, so an attachment relation must full-scan (correct, perf-only — index acceleration for
    /// attachments is a Slice 1b perf follow-on). A full scan reads the scoped store.
    fn is_attachment(&self) -> bool {
        is_attachment_scope(self.db.as_deref())
    }
}

/// Whether a database qualifier names one of the two implicit reserved scopes `main` / `temp`
/// (attached-databases.md §3), which resolve to the SAME store the bare name would. A `None` qualifier
/// (a bare implicit-scope name) counts as reserved for routing: it too keeps the temp-first funnels.
/// Reject a USER-written catalog object name beginning `jed_` — the prefix is reserved for the
/// engine's own catalog relations (spec/design/introspection.md §4). Case-insensitive (resolution
/// folds case and there is no quoted-identifier escape — grammar.md §3). Engine-GENERATED names (a
/// serial's `<table>_<col>_seq`, an index auto-name — both legal for a table named `jed`) never
/// pass through here; the check sits with each site's namespace-collision check so established
/// validation orders (42P01/42703 before name checks) are preserved. `kind` is the object word in
/// the message: table / index / sequence / type.
fn check_reserved_name(kind: &str, name: &str) -> Result<()> {
    if name.len() >= 4 && name.as_bytes()[..4].eq_ignore_ascii_case(b"jed_") {
        return Err(EngineError::new(
            SqlState::ReservedName,
            format!(
                "{kind} name {name} is reserved (the jed_ prefix is reserved for system objects)"
            ),
        ));
    }
    Ok(())
}

fn is_reserved_scope(q: Option<&str>) -> bool {
    match q {
        None => true,
        Some(s) => matches!(s.to_ascii_lowercase().as_str(), "main" | "temp"),
    }
}

/// Whether a database qualifier names a HOST-ATTACHED database (not `None`, not reserved `main`/`temp`)
/// — the case that routes to the attachment registry and gates off index-bound pushdown this slice.
fn is_attachment_scope(q: Option<&str>) -> bool {
    !is_reserved_scope(q)
}

/// Where a finalized FROM relation's `&Table` comes from, recorded during the LATERAL-aware FROM
/// build (spec/design/grammar.md §44). A base table / CTE binding has a stable catalog address
/// (`&Table`); a synthetic relation (derived table / SRF) is recorded by INDEX into the local
/// `synthetic` vec — never a borrow — so a record can outlive a later push into that vec, which is
/// what lets a LATERAL item resolve against the earlier synthetic tables while later ones grow.
#[derive(Clone, Copy)]
pub(crate) enum RelSrc<'a> {
    Base(&'a Table),
    Cte(&'a Table, usize),
    Synthetic(usize),
}

/// A FROM relation finalized during the §44 LATERAL-aware build: its label, flat column offset, and
/// table source. Held in FROM order so the prefix `parent` scope a later LATERAL item resolves
/// against (the relations to its left) can be rebuilt, and the persistent scope assembled afterward.
pub(crate) struct FinalRel<'a> {
    label: String,
    offset: usize,
    src: RelSrc<'a>,
    /// The relation's explicit database qualifier (attached-databases.md §3), carried from the
    /// `TableRef` into the `ScopeRel`/`PlanRel` so the store routes to the right database at exec.
    /// `None` for a bare (implicit-scope) name / a synthetic relation.
    db: Option<String>,
}

impl<'a> FinalRel<'a> {
    /// The relation's `&Table` — a borrowed catalog table, or a deref into the synthetic-table vec.
    fn table<'s>(&'s self, synthetic: &'s [Box<Table>]) -> &'s Table
    where
        'a: 's,
    {
        match self.src {
            RelSrc::Base(t) | RelSrc::Cte(t, _) => t,
            RelSrc::Synthetic(idx) => &synthetic[idx],
        }
    }
}

/// Build the temporary `parent` scope a LATERAL item resolves against (spec/design/grammar.md §44):
/// the relations to its left, chained to the enclosing query's `parent` so a sibling column resolves
/// as `Outer{level=1}` and an enclosing-query column as a deeper hop. The returned scope borrows
/// `synthetic`; it is dropped before the next push, so it never blocks the build's growth.
fn build_prefix_scope<'s>(
    finalized: &'s [FinalRel<'s>],
    synthetic: &'s [Box<Table>],
    parent: Option<&'s Scope<'s>>,
    catalog: &'s Engine,
    ctes: &'s [CteBinding],
) -> Scope<'s> {
    Scope {
        rels: finalized
            .iter()
            .map(|fr| ScopeRel {
                label: fr.label.clone(),
                table: fr.table(synthetic),
                offset: fr.offset,
                qualifier_only: false,
                // The prefix is only for column resolution; a correlated reference into a CTE-backed
                // relation reads its already-delivered row, so it adds no CTE reference here.
                cte: None,
                db: fr.db.clone(),
            })
            .collect(),
        parent,
        catalog,
        allow_subquery: true,
        ctes,
        merges: Vec::new(),
        hidden: Vec::new(),
    }
}

/// A planned common table expression, owned by `plan_with` for the whole statement (so the scopes
/// that borrow its synthetic `table` outlive it — spec/design/cte.md §A.2). `name` is lowercased
/// for case-insensitive FROM matching; `table` is the synthetic relation exposing the body's output
/// columns; `source` is the planned body (a query plan, or — spec/design/writable-cte.md — a
/// data-modifying statement); `hint` is the `[NOT] MATERIALIZED` override; `refs` counts the FROM
/// references resolved to it during planning (a `Cell` — planning borrows `&self`).
///
/// For a RECURSIVE CTE (spec/design/recursive-cte.md) `source` holds the **non-recursive (anchor)
/// term** (its column types fix the synthetic relation's) and `recursive` carries the recursive
/// term + the `UNION ALL` flag; the binding is in scope inside its own recursive term, so the
/// self-reference resolves to it (`refs` then counts the self-reference too).
pub(crate) struct CteBinding {
    name: String,
    table: Box<Table>,
    source: CteSource,
    recursive: Option<RecursiveTerm>,
    hint: Option<bool>,
    refs: std::cell::Cell<usize>,
}

/// What a CTE binding evaluates to (spec/design/cte.md, writable-cte.md). A plain CTE holds a
/// planned query body; a **data-modifying** CTE holds the statement to execute (for its effect +
/// `RETURNING` buffer). A data-modifying CTE is always materialized (writable-cte.md §3), so the
/// inline-execution path never touches a `Dml` source.
pub(crate) enum CteSource {
    Query(QueryPlan),
    Dml(DmCte),
}

/// A data-modifying CTE's body (spec/design/writable-cte.md): the `INSERT`/`UPDATE`/`DELETE` to run
/// (cloned from the AST, executed with the statement's CTE context threaded in) and whether it has
/// no `RETURNING` clause — in which case a FROM reference to it is `0A000` (§5).
pub(crate) struct DmCte {
    stmt: DmStmt,
    no_returning: bool,
}

/// A data-modifying statement in a writable-CTE position (a CTE body or the `WITH` primary).
pub(crate) enum DmStmt {
    Insert(Box<Insert>),
    Update(Box<Update>),
    Delete(Box<Delete>),
}

/// The recursive half of a `WITH RECURSIVE` CTE (spec/design/recursive-cte.md §4): the planned
/// recursive term (the `UNION`'s right operand, which references the CTE once) and whether the body
/// is `UNION ALL` (keep every row) versus `UNION` (drop rows duplicating any already emitted).
pub(crate) struct RecursiveTerm {
    plan: QueryPlan,
    union_all: bool,
}

/// How a column reference resolved against the scope CHAIN (spec/design/grammar.md §26).
/// `Local` is a column of one of THIS query's relations (a flat row index into the joined
/// row); `Outer` is a correlated reference to an enclosing query — `level` hops outward
/// (1 = immediate parent) and `index` is the flat column index within that ancestor's row.
#[derive(Clone, Copy)]
pub(crate) enum Resolved {
    Local(usize),
    Outer { level: usize, index: usize },
}

/// A `USING` / `NATURAL` **merged column** (spec/design/grammar.md §15): `name` is the (lowercased)
/// join column and `index` the flat row index a bare reference to it resolves to — the **surviving
/// side**: the left column for `INNER`/`LEFT`, the right column for `RIGHT`. (`FULL JOIN ... USING`,
/// whose merge is `COALESCE(left, right)` and so is not a single column, is a deferred `0A000`
/// narrowing.) Both underlying copies are recorded in the scope's `hidden` set.
#[derive(Clone)]
pub(crate) struct MergeCol {
    name: String,
    index: usize,
}

/// The relations a query's FROM clause puts in scope, in FROM order, plus the enclosing
/// scope chain (for correlated references — grammar.md §26) and the catalog (so resolving a
/// subquery can look up its own FROM tables).
pub(crate) struct Scope<'a> {
    rels: Vec<ScopeRel<'a>>,
    /// The enclosing query's scope, for correlated-reference resolution (None at top level).
    parent: Option<&'a Scope<'a>>,
    /// The catalog, so a subquery's inner FROM tables can be resolved during planning.
    catalog: &'a Engine,
    /// Whether a subquery is allowed in this scope's expressions: true inside a SELECT (and
    /// its nested subqueries), false for UPDATE/DELETE (a subquery there is 0A000 this slice).
    allow_subquery: bool,
    /// The statement's CTE bindings visible here (spec/design/cte.md §2). Inherited DIRECTLY down
    /// into nested scopes (a subquery sees the same `ctes`), NOT via the `parent` chain — so CTE
    /// lookup never counts as a correlation level. Empty for every non-`WITH` statement.
    ctes: &'a [CteBinding],
    /// `USING` / `NATURAL` merged columns (spec/design/grammar.md §15) — a bare reference to a merge
    /// name resolves to its `index` (checked before the per-relation search, so it is never the
    /// underlying copies' `42702` ambiguity). Empty for every scope except a SELECT whose FROM has a
    /// `USING`/`NATURAL` join.
    merges: Vec<MergeCol>,
    /// Flat indices SUPERSEDED by a merge — the underlying left+right copies, omitted from `*`
    /// expansion (still reachable qualified). Empty unless `merges` is non-empty.
    hidden: Vec<usize>,
}

impl<'a> Scope<'a> {
    /// A one-relation scope with no parent (the single-table UPDATE / DELETE case). Subqueries
    /// ARE allowed: a correlated reference resolves to the target row via the per-row outer
    /// environment (the subquery's parent is this scope), an uncorrelated one folds once
    /// (spec/design/grammar.md §26). SELECT builds its own scope in `plan_select`.
    fn single(catalog: &'a Engine, table: &'a Table) -> Scope<'a> {
        Scope {
            rels: vec![ScopeRel {
                label: table.name.to_ascii_lowercase(),
                table,
                offset: 0,
                qualifier_only: false,
                cte: None,
                db: None,
            }],
            parent: None,
            catalog,
            allow_subquery: true,
            ctes: &[],
            merges: Vec::new(),
            hidden: Vec::new(),
        }
    }

    /// A column-less scope — the environment a `DEFAULT` expression resolves against
    /// (constraints.md §2): a default may not reference a column (rejected as 0A000 by the
    /// structural pre-walk before resolution) and may not contain a subquery, so there are no
    /// relations and subqueries are disallowed.
    fn empty(catalog: &'a Engine) -> Scope<'a> {
        Scope {
            rels: Vec::new(),
            parent: None,
            catalog,
            allow_subquery: false,
            ctes: &[],
            merges: Vec::new(),
            hidden: Vec::new(),
        }
    }

    /// The scope a RETURNING list resolves against (grammar.md §32): the target table at
    /// offset 0 (bare and table-qualified references read the BASE row), plus the `old`/`new`
    /// row-version pseudo-relations as QUALIFIER-ONLY rels over the concatenated projection
    /// row `[base | other]`. `base_is_old` says which version the base row is: false for
    /// INSERT/UPDATE (base = the new row, `old` reads the other half), true for DELETE
    /// (base = the old row, `new` reads the other half) — the absent version is the all-NULL
    /// row the caller appends. Explicit `WITH (OLD AS o, NEW AS n)` aliases are installed
    /// first and hide that version's standard name; an unaliased default is suppressed when
    /// its label is already occupied (including by the target table). Explicit aliases may not
    /// collide with the target or each other (`42712`).
    fn returning(
        catalog: &'a Engine,
        table: &'a Table,
        base_is_old: bool,
        returning: &ReturningClause,
    ) -> Result<Scope<'a>> {
        let n = table.columns.len();
        let label = table.name.to_ascii_lowercase();
        let (old_offset, new_offset) = if base_is_old { (0, n) } else { (n, 0) };
        let mut rels = vec![ScopeRel {
            label: label.clone(),
            table,
            offset: 0,
            qualifier_only: false,
            cte: None,
            db: None,
        }];
        for (alias, offset) in [
            (returning.old_alias.as_deref(), old_offset),
            (returning.new_alias.as_deref(), new_offset),
        ] {
            if let Some(alias) = alias {
                let alias = alias.to_ascii_lowercase();
                if rels.iter().any(|r| r.label == alias) {
                    return Err(EngineError::new(
                        SqlState::DuplicateAlias,
                        format!("table name {alias} specified more than once"),
                    ));
                }
                rels.push(ScopeRel {
                    label: alias,
                    table,
                    offset,
                    qualifier_only: true,
                    cte: None,
                    db: None,
                });
            }
        }
        for (pseudo, offset, aliased) in [
            ("old", old_offset, returning.old_alias.is_some()),
            ("new", new_offset, returning.new_alias.is_some()),
        ] {
            if !aliased && !rels.iter().any(|r| r.label == pseudo) {
                rels.push(ScopeRel {
                    label: pseudo.to_string(),
                    table,
                    offset,
                    qualifier_only: true,
                    cte: None,
                    db: None,
                });
            }
        }
        Ok(Scope {
            rels,
            parent: None,
            catalog,
            allow_subquery: true,
            ctes: &[],
            merges: Vec::new(),
            hidden: Vec::new(),
        })
    }

    /// The scope a DO UPDATE's `SET`/`WHERE` resolve against (spec/design/upsert.md §5): the
    /// target table at offset 0 (bare and table-qualified references read the EXISTING
    /// conflicting row), plus `excluded` as a QUALIFIER-ONLY relation at offset `n` over the
    /// combined row `[existing | proposed]` (`excluded.col` reads the proposed row). A target
    /// table literally named `excluded` SHADOWS the pseudo-relation (PostgreSQL's rule, like
    /// the RETURNING `old`/`new` qualifiers, §32).
    fn on_conflict_excluded(catalog: &'a Engine, table: &'a Table) -> Scope<'a> {
        let n = table.columns.len();
        let label = table.name.to_ascii_lowercase();
        let mut rels = vec![ScopeRel {
            label: label.clone(),
            table,
            offset: 0,
            qualifier_only: false,
            cte: None,
            db: None,
        }];
        if label != "excluded" {
            rels.push(ScopeRel {
                label: "excluded".to_string(),
                table,
                offset: n,
                qualifier_only: true,
                cte: None,
                db: None,
            });
        }
        Scope {
            rels,
            parent: None,
            catalog,
            allow_subquery: true,
            ctes: &[],
            merges: Vec::new(),
            hidden: Vec::new(),
        }
    }

    /// Resolve a bare column name against THIS scope, then OUTWARD through the parent chain.
    /// Within one scope: two+ relations have it → 42702 ambiguous; exactly one → `Local`; none
    /// → fall through to the parent. A name found only in an ancestor is an `Outer` reference
    /// (nearest scope wins — an inner match shadows an outer one, matching PostgreSQL). 42703
    /// only if no scope in the chain has it. A qualifier-only rel (the RETURNING `old`/`new`
    /// pseudo-relations) is invisible here — no new ambiguity (grammar.md §32).
    fn resolve_bare(&self, name: &str) -> Result<Resolved> {
        // A USING/NATURAL MERGE column resolves to its surviving side (grammar.md §15), seeded here
        // so the bare name binds the merged column rather than its two (hidden) underlying copies —
        // which is why such a join column is unambiguous. A *non-hidden* column elsewhere with the
        // same name still makes the reference ambiguous (a third relation sharing the name).
        let mut found: Option<usize> = self
            .merges
            .iter()
            .find(|m| m.name.eq_ignore_ascii_case(name))
            .map(|m| m.index);
        for r in &self.rels {
            if r.qualifier_only {
                continue;
            }
            // Count EVERY matching column, not just the first per relation: a synthetic relation (a
            // CTE or derived table) may carry two columns of the same name, and a bare reference to
            // that name is ambiguous (42702) exactly as a match across two relations is (cte.md §2,
            // grammar.md §42). Base tables have unique column names, so this only ever fires for a
            // duplicate-output-name synthetic relation.
            for (local, c) in r.table.columns.iter().enumerate() {
                let idx = r.offset + local;
                // A merge's underlying copies are superseded by the merge above — skip them.
                if self.hidden.contains(&idx) {
                    continue;
                }
                if c.name.eq_ignore_ascii_case(name) {
                    if found.is_some() {
                        return Err(ambiguous_column(name));
                    }
                    found = Some(idx);
                }
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

    /// The flat column count of this scope (the input-row width). The window base offset: a window
    /// query appends each window function's result after the input columns (spec/design/window.md
    /// §5.1), so window slot = `width() + window_index`.
    fn width(&self) -> usize {
        self.rels.iter().map(|r| r.table.columns.len()).sum()
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
///
/// Not `Copy`: the `Composite` arm owns a heap shape (open types — CLAUDE.md §4), so the type
/// is cloned/borrowed rather than copied. Every other arm is still a trivial tag/scalar.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum ResolvedType {
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
    /// The `interval` family (a span). Compares only with itself (by the canonical span).
    Interval,
    /// The `date` family (calendar date). A strict island — compares only with itself (by the
    /// i32 day count); no implicit cast to `timestamp` this slice (spec/design/date.md §4).
    Date,
    /// The float family, carrying its width (spec/design/float.md §2). The two widths form a
    /// promotion tower: `f32 → f64` is the one implicit float cast; mixed-width arithmetic
    /// and comparison promote to `f64` first. A strict island — no implicit int/decimal ↔ float.
    Float(ScalarType),
    Null,
    /// A composite (row) type (spec/design/composite.md §5). `name` is `Some` for a named catalog
    /// type — rendered in the `# types:` output and the basis for cross-comparability — or `None`
    /// for an anonymous `ROW(...)` result. `fields` are the resolved (field-name, type) pairs in
    /// declaration order (the basis for field access — S4 — and structural assignability). Boxed so
    /// the common scalar `ResolvedType` stays small.
    Composite(Box<CompositeRType>),
    /// An array type (spec/design/array.md §2), carrying its resolved element type. Two arrays are
    /// comparable iff their element types are equal; an array is assignable to an array column of
    /// the same element type. Boxed to keep the scalar `ResolvedType` small.
    Array(Box<ResolvedType>),
    /// A range type (spec/design/ranges.md §2), carrying its resolved element (subtype) type. Two
    /// ranges are comparable iff their elements are equal; a range is assignable to a range column
    /// of the same element. The element is always one of the six scalar subtypes. Boxed to keep the
    /// scalar `ResolvedType` small.
    Range(Box<ResolvedType>),
    /// The `json` family (verbatim text — spec/design/json.md §4). NOT comparable (PG ships no
    /// btree/hash opclass — §5): a comparison/ORDER BY/DISTINCT on json resolves to 42883.
    Json,
    /// The `jsonb` family (canonical binary — spec/design/json.md §2). Comparable with itself by
    /// PG's total btree order (§5).
    Jsonb,
    /// The `jsonpath` type (spec/design/jsonpath.md, P1a). NOT comparable (42883); literal-only.
    JsonPath,
}

/// The resolved shape of a composite type — its (optional) name and resolved field list. The
/// `name` is `None` for an anonymous `ROW(...)` result, `Some` for a named catalog type.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct CompositeRType {
    name: Option<String>,
    fields: Vec<(String, ResolvedType)>,
}

impl ResolvedType {
    /// This type's name, for the `# types:` output and a `42804` assignability message (the integer
    /// width is exact). A named composite is its type name; an anonymous `ROW(...)` is `record` (PG).
    fn type_name(&self) -> String {
        match self {
            ResolvedType::Int(st) => st.canonical_name().to_string(),
            ResolvedType::Bool => "boolean".to_string(),
            ResolvedType::Text => "text".to_string(),
            ResolvedType::Decimal => "decimal".to_string(),
            ResolvedType::Bytea => "bytea".to_string(),
            ResolvedType::Uuid => "uuid".to_string(),
            ResolvedType::Timestamp => "timestamp".to_string(),
            ResolvedType::Timestamptz => "timestamptz".to_string(),
            ResolvedType::Interval => "interval".to_string(),
            ResolvedType::Date => "date".to_string(),
            ResolvedType::Float(st) => st.canonical_name().to_string(),
            ResolvedType::Null => "unknown".to_string(),
            ResolvedType::Composite(c) => c.name.clone().unwrap_or_else(|| "record".to_string()),
            ResolvedType::Array(elem) => format!("{}[]", elem.type_name()),
            ResolvedType::Json => "json".to_string(),
            ResolvedType::JsonPath => "jsonpath".to_string(),
            ResolvedType::Jsonb => "jsonb".to_string(),
            // A range names itself by its element subtype (i32 → i32range — spec/design/ranges.md).
            ResolvedType::Range(elem) => resolved_range_element_scalar(elem)
                .and_then(crate::range::range_name_for_element)
                .map(|n| n.to_string())
                .unwrap_or_else(|| format!("range<{}>", elem.type_name())),
        }
    }

    /// Whether a projected value of this type is assignable to a `col_ty` column for storage —
    /// the FAMILY-level gate `INSERT ... SELECT` applies up front (spec/design/grammar.md §24),
    /// before any row is produced (so it fires even over an empty source). It is the
    /// family-level subset of `store_value` and MUST agree with it: an integer assigns to an
    /// integer or decimal column (int→decimal widens), a decimal only to a decimal column
    /// (decimal→int is explicit-CAST only), text to text/uuid/bytea/timestamp/timestamptz/interval (the
    /// documented text adaptation — the per-row store then parses, trapping 22P02/22007 on
    /// malformed input), boolean→boolean, uuid→uuid, bytea→bytea, a timestamp only to a timestamp
    /// column and a timestamptz only to a timestamptz column (the two never cross — they do not
    /// even compare, timestamp.md), and a NULL-typed projection to any column (a NOT NULL target
    /// then traps 23502 per row). A non-assignable pair is a 42804.
    fn assignable_to(&self, col_ty: ScalarType) -> bool {
        match self {
            ResolvedType::Null => true,
            // A composite source never assigns to a scalar column (the composite-target case is
            // handled structurally at the call site — spec/design/composite.md §4).
            ResolvedType::Composite(_) => false,
            // An array source never assigns to a scalar column (INSERT ... SELECT into an array
            // column is deferred — spec/design/array.md §12).
            ResolvedType::Array(_) => false,
            // A range source never assigns to a scalar column (a range column is not yet storable —
            // spec/design/ranges.md §8; range storage lands in R2).
            ResolvedType::Range(_) => false,
            ResolvedType::Int(_) => col_ty.is_integer() || col_ty.is_decimal(),
            ResolvedType::Decimal => col_ty.is_decimal(),
            ResolvedType::Bool => col_ty.is_bool(),
            ResolvedType::Text => {
                col_ty.is_text()
                    || col_ty.is_uuid()
                    || col_ty.is_bytea()
                    || col_ty.is_timestamp()
                    || col_ty.is_timestamptz()
                    || col_ty.is_interval()
                    || col_ty.is_date()
            }
            ResolvedType::Bytea => col_ty.is_bytea(),
            ResolvedType::Uuid => col_ty.is_uuid(),
            ResolvedType::Json => col_ty.is_json(),
            ResolvedType::Jsonb => col_ty.is_jsonb(),
            ResolvedType::JsonPath => matches!(col_ty, ScalarType::JsonPath),
            ResolvedType::Timestamp => col_ty.is_timestamp(),
            ResolvedType::Timestamptz => col_ty.is_timestamptz(),
            ResolvedType::Interval => col_ty.is_interval(),
            ResolvedType::Date => col_ty.is_date(),
            // A float assigns to a float column of equal-or-wider width: f32 → f32/f64
            // (the implicit widening cast), f64 → f64 only (f64 → f32 is explicit).
            // store_value enforces the same rule per row (spec/types/casts.toml).
            ResolvedType::Float(st) => col_ty.is_float() && st.rank() <= col_ty.rank(),
        }
    }
}

/// Render a projection's resolved types as their canonical names for the public `Outcome::Query`
/// — the `# types:` directive's assertion surface (spec/design/conformance.md §7). Same names as
/// the `42804` message (`type_name`): the exact integer width, the unconstrained `decimal`.
fn type_names(types: &[ResolvedType]) -> Vec<String> {
    types.iter().map(|t| t.type_name().to_string()).collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

impl ArithOp {
    /// The catalog operator name (catalog.toml) for this arithmetic op — the key for its
    /// per-operator `cost` base (functions.md §8, `operator_cost`).
    fn op_name(self) -> &'static str {
        match self {
            ArithOp::Add => "add",
            ArithOp::Sub => "sub",
            ArithOp::Mul => "mul",
            ArithOp::Div => "div",
            ArithOp::Mod => "mod",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CmpOp {
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
}

impl CmpOp {
    /// The catalog operator name (catalog.toml) for this comparison — the key for its
    /// per-operator `cost` base (functions.md §8, `operator_cost`).
    fn op_name(self) -> &'static str {
        match self {
            CmpOp::Eq => "eq",
            CmpOp::Ne => "ne",
            CmpOp::Lt => "lt",
            CmpOp::Gt => "gt",
            CmpOp::Le => "le",
            CmpOp::Ge => "ge",
        }
    }
}

/// The scalar functions (kind = "function", spec/design/functions.md §9), parsed from a call
/// name (case-insensitive). Evaluated per row; the overload (integer vs decimal) is recovered
/// at eval from the argument's runtime value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScalarFunc {
    Abs,
    Round,
    // Float functions (spec/design/float.md §8). EXACT / correctly-rounded (in-contract):
    Ceil,
    Floor,
    Trunc,
    Sqrt,
    // TRANSCENDENTAL (exempted — native libm, may differ by an ULP cross-core):
    Exp,
    Ln,
    Log10,
    Pow,
    /// log — base-10 (1-arg) / arbitrary-base (2-arg) logarithm over decimal (decimal.md §8).
    /// Decimal-only (no float `log` overload); the EXACT-numeric kernel, IN-CONTRACT.
    Log,
    Sin,
    Cos,
    Tan,
    /// cbrt — the real cube root (float.md §8). Transcendental/irrational, exempted; no domain
    /// restriction (cbrt of a negative is the negative real root).
    Cbrt,
    /// pi() → f64 — the constant π (float.md §8). Zero-arg; IN-CONTRACT (the same f64 literal in
    /// every core), so NOT in the transcendental ledger.
    Pi,
    /// radians(x) → f64 — degrees → radians (float.md §8): x · RADIANS_PER_DEGREE. A single
    /// correctly-rounded IEEE multiply, IN-CONTRACT (not ledgered).
    Radians,
    /// degrees(x) → f64 — radians → degrees (float.md §8): x / RADIANS_PER_DEGREE. A single
    /// correctly-rounded IEEE divide, IN-CONTRACT (not ledgered).
    Degrees,
    /// asin(x) → f64 — inverse sine in radians (float.md §8). Transcendental, exempted; domain
    /// [-1, 1], |x| > 1 (or ±Inf) → 22003, NaN propagates.
    Asin,
    /// acos(x) → f64 — inverse cosine in radians (float.md §8). Transcendental, exempted; same
    /// domain [-1, 1] as asin.
    Acos,
    /// atan(x) → f64 — inverse tangent in radians (float.md §8). Transcendental, exempted; no
    /// domain restriction (atan(±Inf) = ±π/2).
    Atan,
    /// atan2(y, x) → f64 — quadrant-aware inverse tangent of y/x (float.md §8). Transcendental,
    /// exempted; two float operands (the resolver widens both to f64), no domain trap.
    Atan2,
    /// cot(x) → f64 — the cotangent, 1/tan(x) (float.md §8). Transcendental, exempted; cot(0) =
    /// +Infinity (no trap).
    Cot,
    /// Hyperbolic functions (float.md §8). Transcendental, exempted. sinh/cosh/tanh/asinh have no
    /// domain trap (sinh/cosh overflow to ±Inf, PG-faithful); acosh traps below 1, atanh outside
    /// [-1, 1] (but atanh(±1) = ±Inf is admissible).
    Sinh,
    Cosh,
    Tanh,
    Asinh,
    Acosh,
    Atanh,
    /// sign(x) → -1 / 0 / +1 (float.md §8). Two overloads: decimal → numeric (scale 0), float →
    /// f64 (EXACT/in-contract; sign(NaN) = sign(±0) = 0, sign(±Inf) = ±1). Dispatches on the
    /// operand value, like abs.
    Sign,
    /// div(a, b) → numeric — the TRUNCATED (toward zero) integer quotient of two numerics, at
    /// scale 0 (PG div(numeric, numeric)). Computed exactly as (a − a%b)/b so it is EXACT/in-contract.
    /// Resolver-routed (the catalog name "div" is taken by the `/` operator), accepts integer +
    /// decimal operands (integers promote), 22012 on a zero divisor.
    Div,
    /// gcd(a, b) → the greatest common divisor (non-negative), EXACT/in-contract. Integer operands →
    /// the promoted integer type (Euclid; a result whose magnitude overflows the type → 22003); a
    /// decimal operand → numeric at scale max(sₐ, s_b). gcd(0, 0) = 0. Resolver-routed.
    Gcd,
    /// lcm(a, b) → the least common multiple (non-negative), EXACT/in-contract, |a/gcd · b|.
    /// lcm(_, 0) = 0. Integer → the promoted type (overflow → 22003); decimal → numeric at scale
    /// max(sₐ, s_b). Resolver-routed (shares gcd's resolution).
    Lcm,
    /// factorial(n) → numeric — n! at scale 0 (PG factorial(bigint)). A negative operand → 22003.
    /// The O(n) multiply loop is metered per step (decimal_work, guarded) so the cost ceiling bounds
    /// a large factorial before the limb work runs (§13).
    Factorial,
    /// width_bucket(op, low, high, count) → i32 — the equi-width histogram bucket. Two overloads
    /// (numeric exact, float in f64); dispatches on the operand value. 2201G on a bad count / equal
    /// bounds (and, for float, a NaN operand / infinite bound); a result past int4 → 22003.
    WidthBucket,
    /// scale(numeric) → i32 — the decimal's display (fractional-digit) scale (decimal.md).
    Scale,
    /// min_scale(numeric) → i32 — the smallest scale that represents the value exactly (trailing
    /// fractional zeros dropped); zero has min_scale 0 (decimal.md).
    MinScale,
    /// trim_scale(numeric) → numeric — the value re-scaled down to its min_scale (trailing zeros
    /// removed), value-identical (decimal.md).
    TrimScale,
    /// make_interval — builds an interval from its (named/defaulted) integer components plus the
    /// f64 `secs` (spec/design/functions.md §11). The one scalar function returning interval.
    MakeInterval,
    /// make_timestamp(year, month, mday, hour, min, sec) → timestamp — the make_interval sibling
    /// (§11): every parameter named (none defaulted), the wall clock assembled from the fields.
    MakeTimestamp,
    /// make_timestamptz(year, month, mday, hour, min, sec[, timezone]) → timestamptz (§11) — as
    /// make_timestamp, then interprets the wall clock in the session zone (6-arg) or the explicit
    /// `timezone` text (7-arg), charging one `timezone` unit.
    MakeTimestamptz,
    /// make_date(year, month, day) → date — the MakeTimestamp sibling (functions.md §11); a
    /// negative year is BC, year zero / bad fields trap 22008.
    MakeDate,
    /// CURRENT_DATE (parser-desugared, also callable — functions.md §12, date.md §6): the
    /// statement clock's day in the session zone — the 'today' literal as a function. STABLE;
    /// charges one timezone unit beyond operator_eval.
    CurrentDate,
    /// date_part(field, source) → f64 — the float8-returning EXTRACT twin (timezones.md §9.2):
    /// the shared extract kernel, then decimal → f64. The field is a RUNTIME text value validated
    /// per row; a date source WIDENS TO MIDNIGHT (the timestamp matrix applies — PG's own
    /// definition); a timestamptz source decomposes in the session zone.
    DatePart,
    /// uuid_extract_version(uuid) → i16 — the version nibble, NULL off-RFC-variant (§12).
    UuidExtractVersion,
    /// uuid_extract_timestamp(uuid) → timestamptz — the embedded instant for v1/v7, else NULL (§12).
    UuidExtractTimestamp,
    /// uuidv4() → uuid — random (the entropy seam, spec/design/entropy.md §3). VOLATILE.
    Uuidv4,
    /// uuidv7([interval]) → uuid — ms timestamp + monotonic counter + random (the entropy+clock
    /// seam, entropy.md §3); the optional interval shifts the embedded instant. VOLATILE.
    Uuidv7,
    /// now() → timestamptz — the statement clock (the clock seam, entropy.md §5), read ONCE and
    /// reused for the whole statement. STABLE. `current_timestamp` is parser sugar for this.
    Now,
    /// clock_timestamp() → timestamptz — the clock seam read on EVERY call, so it may advance
    /// within a statement (entropy.md §5). VOLATILE.
    ClockTimestamp,
    /// nextval(text) → i64 — advance the named sequence and return the new value
    /// (spec/design/sequences.md §4). VOLATILE; MUTATES the working snapshot (via `pending_seq`),
    /// so a statement calling it runs on the write path.
    Nextval,
    /// currval(text) → i64 — the value `nextval`/`setval` last produced for the named sequence IN
    /// THIS SESSION (sequences.md §6). VOLATILE; reads per-session state, 55000 before defined.
    Currval,
    /// setval(text, i64[, bool]) → i64 — set the named sequence's counter to the value and
    /// return it (sequences.md §4). VOLATILE; MUTATES the working snapshot, so a statement calling
    /// it runs on the write path. Arity 2 (is_called defaults true) or 3.
    Setval,
    /// lastval() → i64 — the value the most recent `nextval` (any sequence) returned IN THIS
    /// SESSION (sequences.md §6). VOLATILE; reads per-session state, 55000 before the first nextval.
    Lastval,
    /// current_setting(text[, bool]) → text — the named session variable's value (spec/design/session.md
    /// §6.1). STABLE; reads per-session state (the variable map). An unset name is `42704` unless the
    /// two-arg `missing_ok` is true (→ NULL). Arity 1 or 2.
    CurrentSetting,
    // json/jsonb processing functions (B1, spec/design/json-sql-functions.md §2). The `Json*` and
    // `Jsonb*` variants share a kernel; the only difference is the json overload parses the verbatim
    // text first. All propagate a SQL NULL input.
    /// json[b]_typeof → the JSON type name (object/array/string/number/boolean/null).
    JsonbTypeof,
    JsonTypeof,
    /// json[b]_array_length → the array element count; a non-array is 22023.
    JsonbArrayLength,
    JsonArrayLength,
    /// json[b]_strip_nulls → recursively remove object members whose value is JSON null.
    JsonbStripNulls,
    JsonStripNulls,
    /// jsonb_pretty → an indented multi-line render.
    JsonbPretty,
    /// to_jsonb(anyelement) → the JSON image of any value (the `value_to_node` kernel). STRICT.
    ToJsonb,
    /// to_json(anyelement) → the JSON image as `json` (the `value_to_node` kernel, rendered compact).
    ToJson,
    /// JSON_SCALAR(anyelement) → the value's JSON scalar as `json` (number/boolean/string). STRICT.
    JsonScalar,
    /// JSON_SERIALIZE(json|jsonb) → the value's text serialization (json verbatim, jsonb canonical).
    JsonSerialize,
    // --- string / text functions (spec/design/string-functions.md). All STRICT (NULL propagates,
    // handled by the generic ScalarFunc null short-circuit). Character functions count Unicode code
    // points (`chars()`); octet/bit functions count UTF-8 bytes.
    /// length(text) → i32 — the number of characters (code points). length('héllo') = 5.
    Length,
    /// octet_length(text) → i32 — the number of UTF-8 bytes. octet_length('héllo') = 6.
    OctetLength,
    /// bit_length(text) → i32 — the number of UTF-8 bits = octet_length × 8. bit_length('héllo') = 48.
    BitLength,
    /// substr(text, start[, count]) → text — the function form of SUBSTRING (1-based, code-point
    /// indexed). A negative count is 22011 (string-functions.md §3).
    Substr,
    /// left(text, n) → text — the first n characters; a negative n drops the last |n| (§3).
    Left,
    /// right(text, n) → text — the last n characters; a negative n drops the first |n| (§3).
    Right,
    /// lpad(text, length[, fill]) → text — left-pad to `length` chars with `fill` (default space);
    /// a longer string truncates; an over-large length traps 54000 (§3).
    Lpad,
    /// rpad(text, length[, fill]) → text — the right-hand mirror of lpad (§3).
    Rpad,
    /// btrim(text[, chars]) → text — trim characters in the `chars` set from both ends (§3).
    Btrim,
    /// ltrim(text[, chars]) → text — trim the `chars` set from the LEADING end only (§3).
    Ltrim,
    /// rtrim(text[, chars]) → text — trim the `chars` set from the TRAILING end only (§3).
    Rtrim,
    /// replace(text, from, to) → text — replace every occurrence of substring `from` with `to` (§3).
    Replace,
    /// translate(text, from, to) → text — per-character map/delete by position in `from`/`to` (§3).
    Translate,
    /// repeat(text, n) → text — the string concatenated n times; over-large result traps 54000 (§3).
    Repeat,
    /// reverse(text) → text — the code points in reverse order (§3).
    Reverse,
    /// strpos(text, substring) → i32 — 1-based code-point position of the first match, else 0 (§3).
    Strpos,
    /// split_part(text, delimiter, n) → text — the n-th field of the split; n=0 traps 22023 (§3).
    SplitPart,
    /// starts_with(text, prefix) → boolean — true iff the string begins with `prefix` (§3).
    StartsWith,
    /// ascii(text) → i32 — the Unicode code point of the first character; empty → 0 (§3).
    Ascii,
    /// chr(int) → text — the one-character string for a Unicode code point; bad point traps (§3).
    Chr,
    /// initcap(text) → text — titlecase each word (ASCII word boundaries + ASCII case fold, §3).
    Initcap,
    /// to_hex(int) → text — lowercase hex of the value's 64-bit two's-complement pattern (§3).
    ToHex,
    /// encode(bytea, format) → text — render bytes as hex / base64 / escape (§3).
    Encode,
    /// decode(text, format) → bytea — parse hex / base64 / escape back to binary (§3).
    Decode,
    /// quote_literal(text) → text — wrap as a SQL string literal (§3).
    QuoteLiteral,
    /// quote_ident(text) → text — wrap as a SQL identifier (§3).
    QuoteIdent,
    /// quote_nullable(text) → text — like quote_literal but NON-STRICT (NULL → 'NULL', §3).
    QuoteNullable,
}

/// The polymorphic array functions (spec/design/array-functions.md). Distinct from
/// [`ScalarFunc`] because they resolve over the `anyarray`/`anyelement` pseudo-families (§2) and
/// the builders return an *array* type (not a `ScalarType`), so they get their own resolved node
/// ([`RExpr::ArrayFunc`]). The kernel id is the function name; the eval recovers everything else
/// from the operand values (the array's own shape header), so the node carries no result type.
#[derive(Debug)]
pub(crate) enum ArrayFunc {
    /// array_ndims(anyarray) → i32 — the dimension count; NULL for the empty array.
    ArrayNdims,
    /// array_length(anyarray, integer) → i32 — length of a dimension; NULL if empty / out of range.
    ArrayLength,
    /// array_lower(anyarray, integer) → i32 — a dimension's lower bound; NULL if empty / out of range.
    ArrayLower,
    /// array_upper(anyarray, integer) → i32 — a dimension's upper bound; NULL if empty / out of range.
    ArrayUpper,
    /// cardinality(anyarray) → i32 — the total element count; 0 for the empty array.
    Cardinality,
    /// array_dims(anyarray) → text — the bound spec `[l1:u1][l2:u2]…`; NULL for the empty array.
    ArrayDims,
    /// array_append(anyarray, anyelement) → anyarray — non-strict; NULL/empty array → `{e}`;
    /// a multidimensional array is 22000 (§3.2).
    ArrayAppend,
    /// array_prepend(anyelement, anyarray) → anyarray — the mirror of array_append.
    ArrayPrepend,
    /// array_cat(anyarray, anyarray) → anyarray — non-strict identity-aware concatenation along
    /// the outer dimension; incompatible dimensionalities are 2202E (§3.2).
    ArrayCat,
    /// array_remove(anyarray, anyelement) → anyarray — drop every element NOT DISTINCT FROM the
    /// value (1-D/empty only — a multidimensional array is 0A000); the lower bound is preserved (§8).
    ArrayRemove,
    /// array_replace(anyarray, anyelement, anyelement) → anyarray — substitute every element NOT
    /// DISTINCT FROM `from` with `to`; any dimensionality, shape preserved (§8).
    ArrayReplace,
    /// array_position(anyarray, anyelement[, integer]) → i32 — the first match's SUBSCRIPT (in the
    /// array's lower-bound space), NULL if absent; 1-D/empty only (0A000); the optional `start`
    /// subscript begins the scan and a NULL `start` is 22004 (§8).
    ArrayPosition,
    /// array_positions(anyarray, anyelement) → i32[] — the i32[] of every match's subscript
    /// (empty {} if none); 1-D/empty only (0A000) (§8).
    ArrayPositions,
    /// `a @> b` (anyarray, anyarray) → boolean — does `a` CONTAIN `b`: is every element of `b`
    /// present in `a` (STRICT equality, NULL matches nothing) over the flattened multiset, any
    /// dimensionality (§10). A NULL whole-array operand → NULL.
    Contains,
    /// `a <@ b` (anyarray, anyarray) → boolean — is `a` CONTAINED BY `b` (i.e. `b @> a`) (§10).
    ContainedBy,
    /// `a && b` (anyarray, anyarray) → boolean — do `a` and `b` OVERLAP: share at least one element
    /// (STRICT equality) over the flattened multiset, any dimensionality (§10).
    Overlaps,
    /// `array_to_json(anyarray)` → json — the array as a JSON array, rendered COMPACT (the `to_jsonb`
    /// node kernel). STRICT. A multidimensional array is a deferred `0A000` (like to_jsonb); the
    /// optional 2nd `pretty boolean` argument is a deferred follow-on. (json-sql-functions.md §2)
    ArrayToJson,
}

/// The polymorphic range ACCESSOR functions (spec/design/range-functions.md §1, RF1). Like
/// [`ArrayFunc`], they resolve over a pseudo-family (`anyrange`, binding ELEM := the element type)
/// and get their own resolved node ([`RExpr::RangeFunc`]); the kernel recovers everything from the
/// operand range value (self-describing). All are STRICT (a NULL range → NULL). `lower`/`upper`
/// return the bound value (ELEM) or NULL when empty/unbounded; the rest return boolean.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RangeFunc {
    /// lower(anyrange) → anyelement — the lower bound value; NULL if the range is empty or
    /// unbounded below.
    Lower,
    /// upper(anyrange) → anyelement — the upper bound value; NULL if empty or unbounded above.
    Upper,
    /// isempty(anyrange) → boolean — is this the empty range.
    IsEmpty,
    /// lower_inc(anyrange) → boolean — is the lower bound inclusive (always false for empty / an
    /// infinite lower bound).
    LowerInc,
    /// upper_inc(anyrange) → boolean — is the upper bound inclusive (always false for empty / an
    /// infinite upper bound).
    UpperInc,
    /// lower_inf(anyrange) → boolean — is the lower bound infinite (false for the empty range).
    LowerInf,
    /// upper_inf(anyrange) → boolean — is the upper bound infinite (false for the empty range).
    UpperInf,
}

/// The regular-expression scalar functions (spec/design/regex.md §8). The kernel id; the eval
/// recovers the arg shape (3/4 for replace, 2/3 for match) from `args.len()`. Kernels in `regex.rs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RegexFunc {
    /// regexp_replace(source, pattern, replacement [, flags]) → text.
    Replace,
    /// regexp_match(source, pattern [, flags]) → text[].
    Match,
    /// regexp_like(string, pattern [, flags]) → boolean (regex.md §8b).
    Like,
    /// regexp_count(string, pattern [, start [, flags]]) → integer (regex.md §8b).
    Count,
    /// regexp_substr(string, pattern [, start [, N [, flags [, subexpr]]]]) → text (regex.md §8b).
    Substr,
    /// regexp_instr(string, pattern [, start [, N [, endoption [, flags [, subexpr]]]]]) → integer
    /// (regex.md §8b).
    Instr,
}

/// The range BOOLEAN operators (spec/design/range-functions.md §3, RF3). Each is a binary infix
/// operator returning a definite boolean (a NULL operand short-circuits to NULL at eval, like the
/// array containment operators). `ContainsElem`/`ElemContainedBy` are the element overloads of
/// `@>`/`<@` (the other operand is a bare element coerced to the range's element type); the rest are
/// range-against-range. The kernels live in `range.rs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RangeOp {
    /// `a @> b` — range `a` contains range `b`.
    Contains,
    /// `r @> e` — range `r` contains element `e` (the element overload of `@>`).
    ContainsElem,
    /// `a <@ b` — range `a` is contained by range `b`.
    ContainedBy,
    /// `e <@ r` — element `e` is contained by range `r` (the element overload of `<@`).
    ElemContainedBy,
    /// `a && b` — ranges `a` and `b` overlap.
    Overlaps,
    /// `a << b` — `a` is strictly left of `b`.
    Before,
    /// `a >> b` — `a` is strictly right of `b`.
    After,
    /// `a &< b` — `a` does not extend to the right of `b`.
    Overleft,
    /// `a &> b` — `a` does not extend to the left of `b`.
    Overright,
    /// `a -|- b` — `a` and `b` are adjacent.
    Adjacent,
}

/// The range SET operators (spec/design/range-functions.md §4, RF4). Each combines two ranges over a
/// common element type into a new range (`RExpr::RangeSetOp`). `Union`/`Difference` raise `22000` on a
/// non-contiguous result; `Intersect`/`Merge` never error. The kernels live in `range.rs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RangeSetOp {
    /// `a + b` — union: the smallest single range covering both (22000 if they leave a gap).
    Union,
    /// `a * b` — intersection: the overlap (empty when the ranges are disjoint).
    Intersect,
    /// `a - b` — difference: the part of `a` not in `b` (22000 if `b` splits `a` in two).
    Difference,
    /// `range_merge(a, b)` — like union but spans any gap between the ranges silently (never errors).
    Merge,
}

/// The VARIADIC argument-counting functions (spec/design/array-functions.md §12). Distinct from
/// [`ScalarFunc`] because they are non-strict (`null = "none"`, like [`ArrayFunc`]) and take either
/// a spread of arguments or a single array via the `VARIADIC` keyword — the call form is carried on
/// the [`RExpr::Variadic`] node. Both return `i32`.
#[derive(Debug)]
pub(crate) enum VariadicFunc {
    /// num_nulls(VARIADIC "any") → i32 — the count of NULL arguments (spread form), or of NULL
    /// flattened elements (VARIADIC-array form; a NULL whole-array operand → NULL). Never NULL in
    /// spread form.
    NumNulls,
    /// num_nonnulls(VARIADIC "any") → i32 — the mirror: the count of non-NULL arguments/elements.
    NumNonnulls,
}

/// Which scalar jsonpath query function an [`RExpr::JsonPathFn`] node is (jsonpath.md §5).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum JsonPathFnKind {
    /// `jsonb_path_exists` → boolean (the sequence is non-empty).
    Exists,
    /// `jsonb_path_query_first` → the first sequence item, or NULL if empty.
    QueryFirst,
    /// `jsonb_path_query_array` → the sequence wrapped in a JSON array.
    QueryArray,
    /// `jsonb_path_match` → the single boolean the path/predicate produces (22038 if not exactly one
    /// boolean item).
    Match,
    /// The PostgreSQL `@@` operator: the same match with non-singleton/non-boolean results suppressed
    /// to SQL NULL.
    MatchSilent,
}

/// Which SQL/JSON query function an [`RExpr::JsonSqlFn`] node is (json-sql-functions.md §5).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum JsonSqlKind {
    /// `JSON_EXISTS` → boolean (non-empty sequence); errors honor ON ERROR (default FALSE).
    Exists,
    /// `JSON_VALUE` → a single scalar coerced to the RETURNING type (default text).
    Value,
    /// `JSON_QUERY` → a json/jsonb value (wrapper / quotes controlled).
    Query,
}

/// Which json/jsonb builder an [`RExpr::JsonBuild`] node is (json-sql-functions.md §2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum JsonBuildKind {
    /// json[b]_build_array — every argument is one array element (NULL → JSON null).
    Array,
    /// json[b]_build_object — alternating key/value arguments (odd count / NULL key → 22023).
    Object,
}

/// One resolved subscript spec in an [`RExpr::Subscript`] (spec/design/array.md §6): a single
/// index `a[i]`, or a slice `a[m:n]` whose bounds may be omitted (`a[:n]`, `a[m:]`, `a[:]`).
pub(crate) enum RSubscript {
    Index(Box<RExpr>),
    Slice {
        lower: Option<Box<RExpr>>,
        upper: Option<Box<RExpr>>,
    },
}

/// A resolved expression: a tree over fixed column indices, ready to evaluate against
/// a row. Arithmetic nodes carry their (promotion-tower) result type so the computed
/// value can be range-checked against it (the i16+i16 → i16 boundary).
/// Which jsonb delete form a [`RExpr::JsonDelete`] applies (json-sql-functions.md §1, J6).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DeleteKind {
    /// `jsonb - text` — delete a key (object) or matching string elements (array).
    Key,
    /// `jsonb - int` — delete the array element at an index.
    Index,
    /// `jsonb - text[]` — delete each key.
    Keys,
    /// `jsonb #- text[]` — delete the element at a path.
    Path,
}

/// Which jsonb key-existence operator a [`RExpr::JsonHasKey`] applies (json-sql-functions.md §1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum HasKeyKind {
    /// `?` — a single key (text) exists.
    One,
    /// `?|` — any key of a `text[]` exists.
    Any,
    /// `?&` — all keys of a `text[]` exist.
    All,
}

/// Which jsonb accessor operator a [`RExpr::JsonGet`] applies (spec/design/json-sql-functions.md §1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum JsonGetOp {
    /// `->` — field by key (text arg) or element by index (integer arg); result jsonb.
    Arrow,
    /// `->>` — same access, rendered as text.
    ArrowText,
    /// `#>` — get at a `text[]` path; result jsonb.
    HashArrow,
    /// `#>>` — get at a `text[]` path, rendered as text.
    HashArrowText,
}

pub(crate) enum RExpr {
    Column(usize),
    ConstInt(i64),
    ConstBool(bool),
    ConstText(String),
    ConstDecimal(Decimal),
    /// A `f32` / `f64` constant (a typed float literal, an adapted decimal/int literal
    /// in a float context, or a folded subquery value — spec/design/float.md §4).
    ConstFloat32(f32),
    ConstFloat64(f64),
    ConstBytea(Vec<u8>),
    ConstUuid([u8; 16]),
    /// A `json` constant — JSON text stored VERBATIM (spec/design/json.md §4), validated
    /// well-formed at resolve.
    ConstJson(String),
    /// A `jsonb` constant — the canonical tagged-node tree (spec/design/json.md §2), parsed +
    /// canonicalized at resolve. Boxed to keep `RExpr` small (a `JsonNode` is a recursive tree).
    ConstJsonb(Box<JsonNode>),
    /// A `jsonpath` constant — the canonical normalized source text (spec/design/jsonpath.md, P1a),
    /// compiled + rendered at resolve.
    ConstJsonPath(String),
    /// A parsed `timestamp` / `timestamptz` literal: the i64 microsecond instant.
    ConstTimestamp(i64),
    ConstTimestamptz(i64),
    /// A parsed `date` literal: the i32 day count since 1970-01-01 (spec/design/date.md).
    ConstDate(i32),
    /// A parsed `interval` literal: the three-field span (spec/design/interval.md).
    ConstInterval(Interval),
    ConstNull,
    /// A `ROW(...)` constructor (spec/design/composite.md §1): evaluate each field expression and
    /// assemble a `Value::Composite`. Also the folded form of a composite constant (`value_to_rexpr`
    /// wraps each field's constant). One `operator_eval` per node (cost.md §9).
    Row(Vec<RExpr>),
    /// An `ARRAY[…]` constructor (spec/design/array.md §1): evaluate each element expression
    /// (coercing to the unified element type) and assemble a `Value::Array`. When `nested` is true
    /// the elements are themselves arrays and are **stacked** into a value of one higher dimension
    /// (all sub-arrays must share dims/lbounds — else `2202E`); otherwise it is a flat 1-D array
    /// (lower bound 1). One `operator_eval` per node.
    Array {
        elems: Vec<RExpr>,
        nested: bool,
    },
    /// A constant array value (the folded form of an array constant — `value_to_rexpr` — preserving
    /// its shape). Boxed so the (rarely-used) shaped payload does not widen every `RExpr` frame.
    /// Eval returns it directly.
    ConstArray(Box<ArrayVal>),
    /// A constant range value (the folded form of a range constant — `'[1,5)'::i32range`, already
    /// canonicalized at resolve). Boxed so the payload does not widen every `RExpr` frame. Eval
    /// returns it directly.
    ConstRange(Box<RangeVal>),
    /// Field selection `(composite).field` (spec/design/composite.md §S4): evaluate `base` to a
    /// composite value and return its `index`-th field (the field ordinal, fixed at resolve). A
    /// whole-value-NULL composite yields NULL for any field. One `operator_eval` per node.
    Field {
        base: Box<RExpr>,
        index: usize,
    },
    /// Array subscript `base[..][..]` (spec/design/array.md §6): one or more subscript specs applied
    /// to `base`. If `is_slice` is false every spec is an index and the node reads a single element
    /// (the element type) — NULL when the subscript count ≠ ndim or any index is out of range. If
    /// `is_slice` is true the node returns a sub-array (the array type); a scalar index `i` in that
    /// context means `1:i`. A NULL array or any NULL bound yields NULL. One `operator_eval` per node.
    Subscript {
        base: Box<RExpr>,
        subscripts: Vec<RSubscript>,
        is_slice: bool,
    },
    /// A bind parameter, by 0-based index into the bound-values slice passed to `eval`. Its
    /// static type was inferred from context at resolve (spec/design/api.md §5); the value
    /// is supplied (and coerced to that type) before evaluation.
    Param(usize),
    Cast {
        inner: Box<RExpr>,
        target: ScalarType,
        /// For a decimal target, the optional `numeric(p,s)` typmod to coerce to.
        typmod: Option<DecimalTypmod>,
        /// For a `varchar(n)` target (a `text` cast with a length), the max length to
        /// **truncate** to — an explicit cast silently truncates, never raising 22001
        /// (spec/design/types.md §15). `None` for any non-text / unbounded target.
        varchar_len: Option<u32>,
    },
    /// A cast that *involves* an array type (spec/design/array.md §7) — the three follow-on array
    /// casts, none expressible by the scalar [`RExpr::Cast`] node (whose `target` is a `ScalarType`):
    /// runtime `text → T[]` (`array_in` per row), `array → text` (`array_out` per row), and
    /// element-wise `array → other-element-array` (each element through the scalar cast). `to_elem`
    /// is `Some(target element ColType)` for the two array-producing casts (text→array, array→array)
    /// and `None` for `array → text`. The eval branches on the runtime value (Text vs Array).
    ArrayCast {
        inner: Box<RExpr>,
        to_elem: Option<ColType>,
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
        /// The derived collation for this comparison (spec/design/collation.md §7). `None` is the
        /// `C` / default byte order (the unchanged fast path); `Some` is a loaded non-`C` collation
        /// that orders the ORDERING comparisons (`< <= > >=`) by its UCA sort key. `=`/`<>` are
        /// byte-equality regardless (deterministic-collation equality IS byte-identity, §7), so the
        /// collation only changes the ordering ops at eval — but it is derived (and conflict-checked,
        /// 42P22) for every comparison op.
        collation: Option<std::sync::Arc<Collation>>,
    },
    And(Box<RExpr>, Box<RExpr>),
    Or(Box<RExpr>, Box<RExpr>),
    /// A jsonb accessor operator (`-> ->> #> #>>`, spec/design/json-sql-functions.md §1, J4). `op`
    /// selects field/index vs path and text-vs-jsonb; `base` evaluates to a jsonb document; `arg` is
    /// the key (text), array index (integer), or path (`text[]`). The result is jsonb (`-> #>`) or
    /// text (`->> #>>`), and is SQL NULL when the access misses (or when base/arg is NULL).
    JsonGet {
        op: JsonGetOp,
        base: Box<RExpr>,
        arg: Box<RExpr>,
    },
    /// `a @> b` jsonb deep containment (spec/design/json-sql-functions.md §1, J5) — does `a` contain
    /// `b`. `<@` resolves to this with the operands swapped. Boolean; strict (a NULL operand → NULL).
    JsonContains {
        a: Box<RExpr>,
        b: Box<RExpr>,
    },
    /// `jsonb ? text` / `?| text[]` / `?& text[]` key-existence (spec/design/json-sql-functions.md §1,
    /// J5). `kind` selects one-key / any-key / all-keys. Boolean; strict.
    JsonHasKey {
        kind: HasKeyKind,
        base: Box<RExpr>,
        arg: Box<RExpr>,
    },
    /// `a || b` jsonb concatenate / shallow-merge (spec/design/json-sql-functions.md §1, J6). Result
    /// jsonb; strict (a NULL operand → SQL NULL).
    JsonConcat {
        a: Box<RExpr>,
        b: Box<RExpr>,
    },
    /// `jsonb - text|int|text[]` (delete key/index/keys) and `jsonb #- text[]` (delete at path) —
    /// the J6 mutation deletes (spec/design/json-sql-functions.md §1). `kind` selects the form;
    /// `base` is the jsonb document, `arg` the key/index/key-array/path. Result jsonb; strict; a
    /// delete from a scalar (or an integer index into an object) is `22023`.
    JsonDelete {
        kind: DeleteKind,
        base: Box<RExpr>,
        arg: Box<RExpr>,
    },
    /// `jsonb_set` / `jsonb_insert` (json-sql-functions.md §2): a jsonb path mutation. `args` is
    /// `[target jsonb, path text[], value jsonb, (flag boolean)]` — STRICT (any NULL → SQL NULL).
    /// `mode` selects replace-or-create (Set) vs insert (Insert); the optional flag is
    /// create_if_missing (Set) / insert_after (Insert), defaulting to true / false respectively.
    JsonSetInsert {
        mode: json::PathSetMode,
        args: Vec<RExpr>,
    },
    /// `json_object` / `jsonb_object` (json-sql-functions.md §2): build an object from text array(s).
    /// `args` is one `text[]` of alternating keys/values, or two `text[]` (keys, values). The VALUES
    /// are always JSON strings (a NULL value → JSON null); a NULL key → 22004. STRICT in the whole
    /// array argument(s). `json` selects the json (insertion order + dups + " : " spacing) vs jsonb
    /// (canonical) result.
    JsonObjectFromArrays {
        json: bool,
        args: Vec<RExpr>,
    },
    /// A scalar jsonpath query function (P2, jsonpath.md §5): `jsonb_path_exists` /
    /// `jsonb_path_query_first` / `jsonb_path_query_array`. `args` = `[ctx jsonb, path jsonpath]`;
    /// STRICT (any NULL → SQL NULL). The path is recompiled from its canonical text at eval.
    JsonPathFn {
        kind: JsonPathFnKind,
        args: Vec<RExpr>,
    },
    /// A SQL/JSON query function `JSON_EXISTS` / `JSON_VALUE` / `JSON_QUERY` (json-sql-functions.md
    /// §5, S2). `ctx` produces the context jsonb (or json/text, coerced), `path` the jsonpath; the
    /// behaviors / wrapper / quotes drive the result. The result type is fixed at resolve.
    JsonSqlFn {
        kind: JsonSqlKind,
        ctx: Box<RExpr>,
        path: Box<RExpr>,
        /// The RETURNING scalar type (`Bool` for JSON_EXISTS; the JSON_VALUE scalar target;
        /// `Jsonb`/`Json` for JSON_QUERY) — drives the result coercion.
        returning: ScalarType,
        decimal: Option<DecimalTypmod>,
        wrapper: JsonWrapper,
        on_empty: JsonOnBehavior,
        on_error: JsonOnBehavior,
    },
    IsNull {
        operand: Box<RExpr>,
        negated: bool,
    },
    /// `operand IS [NOT] JSON …` (json-sql-functions.md §5): well-formedness + optional kind /
    /// unique-keys test over a string / json / jsonb operand. A NULL operand → NULL; else a definite
    /// boolean (NOT-negated when `negated`).
    IsJson {
        operand: Box<RExpr>,
        negated: bool,
        kind: JsonPredicateKind,
        unique_keys: bool,
    },
    /// `JSON(text [(WITH|WITHOUT) UNIQUE [KEYS]])` (json-sql-functions.md §5): validate a string as a
    /// `json` value (verbatim). Malformed → 22P02; `WITH UNIQUE KEYS` on a duplicate key → 22030.
    /// STRICT (a NULL operand → SQL NULL).
    JsonCtor {
        operand: Box<RExpr>,
        unique_keys: bool,
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
    /// matching. `negated` carries the NOT keyword (NOT LIKE = the negation of the match);
    /// `insensitive` carries `ILIKE` — both operands are simple-lowercased (collation.md §16)
    /// under the engine casing regime before matching.
    Like {
        lhs: Box<RExpr>,
        rhs: Box<RExpr>,
        negated: bool,
        insensitive: bool,
    },
    /// `lhs ~ rhs` / `~*` / `!~` / `!~*` — regular-expression match (regex.md). Both operands
    /// resolve to text (or NULL); a NULL operand makes the result NULL (before the matcher runs).
    /// Matched by the hand-written Pike VM (regex.rs) over Unicode code points; `negated` carries
    /// `!~`/`!~*`, `insensitive` carries `~*`/`!~*` (both operands simple-lowercased like ILIKE).
    /// `program` holds the precompiled NFA for a CONSTANT pattern (compiled once at resolve, the
    /// `col ~ 'literal'` case — regex.md §5); `None` means the pattern is non-constant and compiled
    /// per row at eval. `compile_charged` is the one-shot flag that charges a precompiled program's
    /// `regex_compile` cost once per statement execution (on first eval), not per row.
    Regex {
        lhs: Box<RExpr>,
        rhs: Box<RExpr>,
        negated: bool,
        insensitive: bool,
        program: Option<crate::regex::Program>,
        compile_charged: std::cell::Cell<bool>,
    },
    /// `upper(text)` / `lower(text)` — Unicode case folding (collation.md §16). `upper` selects the
    /// direction. Folds via the engine-global property table when a bundle is loaded, else the ASCII
    /// baseline (fold `a–z`/`A–Z`, pass other code points through). A NULL operand propagates.
    Casing {
        upper: bool,
        arg: Box<RExpr>,
    },
    /// `value AT TIME ZONE zone` (grammar.md §49, timezones.md §6) — desugared from the operator and
    /// from a bare `timezone(zone, value)` call. `to_timestamptz` selects the direction: `false` is
    /// `timestamptz → timestamp` (render the UTC instant as the local wall clock in `zone`); `true`
    /// is `timestamp → timestamptz` (interpret the wall clock as in `zone`, producing the UTC
    /// instant). Reads the engine-global loaded zone set (timezone.rs); an unknown zone is `22023`,
    /// a NULL operand propagates. `±infinity` passes through unchanged.
    AtTimeZone {
        zone: Box<RExpr>,
        value: Box<RExpr>,
        to_timestamptz: bool,
    },
    /// `date_trunc(unit, value[, zone])` (timezones.md §9.1) — truncate `value` down to `unit`.
    /// `unit` is a runtime text expression (case-insensitive; an unrecognized unit is `22023` at
    /// eval). For a `timestamptz` `value` the truncation is in `zone` (the 3-arg form) or the session
    /// zone (`zone = None`, the 2-arg form), charging the `timezone` unit; for `timestamp`/`interval`
    /// it is zone-free. The result family is the `value` family (the runtime `Value` dispatches).
    DateTrunc {
        unit: Box<RExpr>,
        value: Box<RExpr>,
        zone: Option<Box<RExpr>>,
    },
    /// `EXTRACT(field FROM value)` (timezones.md §9.2) — the `numeric` value of `field` (lowercased,
    /// validated at resolve). For a `timestamptz` `value`, every field but `epoch` is computed in the
    /// session zone (charging `timezone`); the `timezone*` fields read the session offset.
    Extract {
        field: String,
        value: Box<RExpr>,
    },
    /// A cross-family datetime cast (timezones.md §9.3) to `to` (`timestamp`/`timestamptz`/`date`)
    /// from another datetime family — the runtime `Value` carries the source family — or the
    /// runtime text → date cast (date.md §6). Casts crossing the `timestamptz` boundary consult
    /// the session zone (charging `timezone`); `±infinity` and NULL pass through. `to` is one of
    /// `Timestamp` / `Timestamptz` / `Date`.
    DateConvert {
        inner: Box<RExpr>,
        to: ScalarType,
    },
    /// A clock-relative date literal — `'today'` / `'now'` (0), `'tomorrow'` (+1), `'yesterday'`
    /// (−1) — resolved to a STABLE node, never folded (date.md §6): at eval it reads the
    /// STATEMENT clock (once per statement, like `now()`) and takes its day in the SESSION zone
    /// (charging `timezone`), shifted by `offset_days`. Flagged non-immutable at birth (42P17 in
    /// an index expression). `'epoch'` is not this node — it folds to the constant 1970-01-01.
    DateClock {
        offset_days: i32,
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
    /// A resolved `COALESCE(a, b, …)` (grammar.md §51) — lazy like `Case`: arguments are
    /// evaluated left to right, each at most once, stopping at the first non-NULL (the second
    /// sanctioned short-circuit, cost.md §3). Argument types unify exactly like CASE result
    /// arms; `coerce_decimal` widens integer arguments when the unified type is decimal.
    Coalesce {
        args: Vec<RExpr>,
        coerce_decimal: bool,
    },
    /// A resolved `GREATEST(a, b, …)` / `LEAST(a, b, …)` (grammar.md §52) — the variadic max/min.
    /// EAGER (unlike `Coalesce`): every argument is evaluated. NULL arguments are ignored; the
    /// result is NULL only when every argument is NULL. `greatest` selects max vs min; the winner
    /// is chosen by the unified type's total order (`value_cmp`, or `collation` for text).
    /// Argument types unify to one common ORDERABLE type (grammar.md §52 — numeric promote, float
    /// widths widen to f64, other families structural; a non-orderable type such as json/jsonpath
    /// is rejected at resolve). `coerce_decimal` widens integer arguments when the unified type is
    /// decimal; `collation` is the derived text comparison collation (None ⇒ byte order).
    GreatestLeast {
        args: Vec<RExpr>,
        coerce_decimal: bool,
        greatest: bool,
        collation: Option<std::sync::Arc<Collation>>,
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
    /// A polymorphic array-function call (spec/design/array-functions.md §3). Resolved over the
    /// `anyarray`/`anyelement` pseudo-families (§2); the resolved element/array type lives in the
    /// surrounding `ResolvedType` (carried out of resolve), not on the node — the kernel produces
    /// the result `Value` from the operand values alone (an array `Value` is self-describing). The
    /// introspectors propagate NULL; the builders (`array_append`/`prepend`/`cat`) are non-strict
    /// (`null = "none"`), so NULL handling lives in the kernel, not a blanket short-circuit here.
    ArrayFunc {
        func: ArrayFunc,
        args: Vec<RExpr>,
    },
    /// A polymorphic range accessor (spec/design/range-functions.md §1 — lower/upper/isempty/
    /// lower_inc/upper_inc/lower_inf/upper_inf). Like [`RExpr::ArrayFunc`], the resolved element type
    /// lives in the surrounding `ResolvedType`; the kernel produces the result `Value` from the
    /// operand range value alone (self-describing). All are STRICT (NULL handled in the kernel).
    RangeFunc {
        func: RangeFunc,
        args: Vec<RExpr>,
    },
    /// A regular-expression scalar function (spec/design/regex.md §8 — `regexp_replace` → text,
    /// `regexp_match` → text[]). Like [`RExpr::ArrayFunc`] the result type lives in the surrounding
    /// `ResolvedType` (text or text[]), not a scalar `result`. STRICT (a NULL arg → NULL, short-
    /// circuited in eval). `args` are the resolved text operands (source, pattern, [replacement,]
    /// [flags]); `program` is the precompiled NFA for a constant pattern (regex.md §5), `compile_charged`
    /// the one-shot flag charging its `regex_compile` cost once per execution.
    RegexFunc {
        func: RegexFunc,
        args: Vec<RExpr>,
        program: Option<crate::regex::Program>,
        compile_charged: std::cell::Cell<bool>,
    },
    /// A range CONSTRUCTOR call (spec/design/range-functions.md §2 — `i32range(lo, hi[, bounds])` and
    /// the five siblings). `elem` is the range's element scalar (the result range type is recovered
    /// from it, a bijection); `args` are the 2 bounds plus an optional bounds-flags TEXT. Non-strict
    /// (`null = "none"`): a NULL bound is an infinite bound, handled in the kernel. The kernel coerces
    /// each bound to `elem` (assignment-style), reads the bounds flags, and finalizes (canonicalize /
    /// order-check / empty-normalize).
    RangeCtor {
        elem: ScalarType,
        args: Vec<RExpr>,
    },
    /// A range BOOLEAN operator (spec/design/range-functions.md §3 — `@> <@ && << >> &< &> -|-`).
    /// `args` are the two operands. STRICT: a NULL operand → NULL (handled in the eval arm). `elem`
    /// is the range's element scalar — used only by the `ContainsElem`/`ElemContainedBy` element
    /// overloads to coerce the bare-element operand to the range's element type at eval; unused (but
    /// carried) for the range-against-range operators.
    RangeOp {
        op: RangeOp,
        args: Vec<RExpr>,
        elem: ScalarType,
    },
    /// A range SET operator (spec/design/range-functions.md §4 — `+` union, `-` difference, `*`
    /// intersection, and `range_merge`). `args` are the two range operands. STRICT: a NULL operand →
    /// NULL (handled in the eval arm). Unlike [`RExpr::RangeOp`] it carries no element scalar — the
    /// kernels work off the self-describing operand values, and the result range type is fixed at
    /// resolve. The kernels (`range_union`/`range_intersect`/`range_minus`) live in `range.rs`;
    /// `+`/`-` raise 22000 on a non-contiguous result.
    RangeSetOp {
        op: RangeSetOp,
        args: Vec<RExpr>,
    },
    /// A VARIADIC argument-counting call (spec/design/array-functions.md §12 — num_nulls/
    /// num_nonnulls). Non-strict (`null = "none"`): the kernel inspects null-ness itself, so there
    /// is no blanket NULL short-circuit. `array_form` records the call shape: `false` = the SPREAD
    /// form (count `args`' null-ness directly, never NULL); `true` = the VARIADIC-array form (one
    /// `args` operand — a NULL array → NULL, else count its flattened elements' null-ness). Result
    /// is always i32.
    Variadic {
        func: VariadicFunc,
        args: Vec<RExpr>,
        array_form: bool,
    },
    /// A VARIADIC json/jsonb builder (json-sql-functions.md §2 — json[b]_build_array / _object).
    /// Non-strict: a NULL argument is included as JSON null (array) or a value (object). `json`
    /// selects the json (compact / PG builder-spacing) vs jsonb (canonical) render; `array_form`
    /// records the VARIADIC-array call shape (the lone array operand is spread; a NULL array → NULL).
    JsonBuild {
        kind: JsonBuildKind,
        json: bool,
        args: Vec<RExpr>,
        array_form: bool,
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
    /// A quantified array comparison `lhs op ANY/ALL(array)` (spec/design/array-functions.md §11):
    /// the array spelling of `IN`. At eval `lhs` is evaluated ONCE, `array` once; then a 3-valued
    /// fold over the array's flattened elements — `ANY` (all=false) is the OR-fold (TRUE if any
    /// `lhs op e` is TRUE, else NULL if any is NULL, else FALSE; empty → FALSE), `ALL` (all=true)
    /// the AND-fold (FALSE if any is FALSE, else NULL if any is NULL, else TRUE; empty → TRUE). A
    /// NULL array → NULL. Charges like `InValues`: one `operator_eval` for the node plus one per
    /// element compared (so `max_cost` bounds the walk).
    Quantified {
        op: CmpOp,
        all: bool,
        lhs: Box<RExpr>,
        array: Box<RExpr>,
    },
}

/// Which subquery form an `RExpr::Subquery` is (spec/design/grammar.md §26).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubqueryKind {
    Scalar,
    Exists,
    In,
    /// `lhs op ANY/ALL(SELECT …)` — the quantified-subquery form (array-functions.md §11.6). `lhs`
    /// is the outer value; the body's single column folds through `quantified_membership` exactly
    /// like the array `Quantified` node. Survives as a `Subquery` node only when CORRELATED; an
    /// uncorrelated one is folded to a constant-array `RExpr::Quantified`.
    Quantified {
        op: CmpOp,
        all: bool,
    },
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
pub(crate) enum QueryPlan {
    Select(SelectPlan),
    SetOp(Box<SetOpPlan>),
    /// A VALUES-body relation — `FROM (VALUES …) AS v` (spec/design/grammar.md §42): a computed
    /// relation of literal rows, the FROM-position sibling of `INSERT … VALUES`. Only ever produced
    /// as a derived-table body (the parser admits `VALUES` solely there), so it never appears as a
    /// set-op operand or a subquery operand.
    Values(ValuesPlan),
    /// A nested `WITH … query_expr` (spec/design/cte.md §7): the nested CTE bindings + their
    /// inline/materialize modes, and the inner query plan that references them. Establishes its own
    /// CTE scope at execution ([`exec_query_plan`] materializes the bindings and runs `body` against
    /// them). The output columns are the body's. Boxed to keep `QueryPlan` small.
    With(Box<WithPlan>),
}

/// A planned nested `WITH … query_expr` (spec/design/cte.md §7). `bindings` are the nested CTEs
/// (planned against each other only — not the enclosing scope), `modes` their per-binding
/// inline/materialize decision ([`cte_modes`]), and `body` the inner query that references them.
/// At execution the bindings are materialized once and `body` runs against a fresh CTE context.
pub(crate) struct WithPlan {
    bindings: Vec<CteBinding>,
    modes: Vec<CteMode>,
    body: QueryPlan,
}

impl QueryPlan {
    /// The output column types — for a scalar/IN subquery's plan-time column-count check (42601)
    /// and its folded/element type.
    fn column_types(&self) -> &[ResolvedType] {
        match self {
            QueryPlan::Select(s) => &s.column_types,
            QueryPlan::SetOp(s) => &s.column_types,
            QueryPlan::Values(v) => &v.column_types,
            QueryPlan::With(w) => w.body.column_types(),
        }
    }

    /// The output column names — the basis for a CTE's synthetic relation when there is no
    /// column-rename list (spec/design/cte.md §1).
    fn column_names(&self) -> &[String] {
        match self {
            QueryPlan::Select(s) => &s.column_names,
            QueryPlan::SetOp(s) => &s.column_names,
            QueryPlan::Values(v) => &v.column_names,
            QueryPlan::With(w) => w.body.column_names(),
        }
    }
}

/// A resolved VALUES-body relation (spec/design/grammar.md §42), executable to its literal rows.
/// `rows` is the resolved value expressions — `rows[r][c]` is row `r`, column `c` — each resolved
/// as a CONSTANT (the body is non-`LATERAL`, planned `parent = None`, so it reads no row).
/// `column_types` is the per-column type unified across the rows like a set operation (§25), and
/// `column_names` is `column1, column2, …` (PostgreSQL; the derived table's optional column-rename
/// list overrides them at the synthetic relation). All rows have `column_types.len()` values.
pub(crate) struct ValuesPlan {
    rows: Vec<Vec<RExpr>>,
    column_types: Vec<ResolvedType>,
    column_names: Vec<String>,
}

// === WITH RECURSIVE analysis (spec/design/recursive-cte.md) ==========================
//
// A `WITH RECURSIVE` CTE is *recursive* iff its body references its own name (anywhere, deep). A
// recursive CTE must take the well-formed shape `non_recursive_term UNION [ALL] recursive_term`
// with the self-reference appearing exactly once, as a direct FROM/JOIN relation of the recursive
// term. These structural checks mirror PostgreSQL's `checkWellFormedRecursion`, run on the parsed
// AST before planning; the error surface is recursive-cte.md §6.

/// The recursiveness of a CTE in a `WITH RECURSIVE` list. `NonRecursive` = the body does not
/// reference the CTE (an ordinary CTE, even under `RECURSIVE`). `Recursive` = the validated
/// fixpoint shape, carrying the `UNION ALL` flag.
pub(crate) enum CteShape {
    NonRecursive,
    Recursive { union_all: bool },
}

/// Classify a CTE body for `WITH RECURSIVE` (recursive-cte.md §6). Returns `NonRecursive` when the
/// body does not reference `name`; otherwise validates the recursive shape and returns
/// `Recursive { union_all }`, or an error (`42P19` for a malformed recursion, `0A000` for a
/// deferred shape).
fn analyze_recursive_cte(name: &str, body: &QueryExpr) -> Result<CteShape> {
    if count_self_refs_query(body, name) == 0 {
        return Ok(CteShape::NonRecursive);
    }
    // The body must be a top-level UNION / UNION ALL.
    let so = match body {
        QueryExpr::SetOp(so) if so.op == SetOpKind::Union => so,
        _ => {
            return Err(EngineError::new(
                SqlState::InvalidRecursion,
                format!(
                    "recursive query \"{name}\" does not have the form non-recursive-term UNION [ALL] recursive-term"
                ),
            ));
        }
    };
    // ORDER BY / LIMIT / OFFSET on a recursive query is not implemented (matching PostgreSQL).
    if !so.order_by.is_empty() {
        return Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "ORDER BY in a recursive query is not implemented".to_string(),
        ));
    }
    if so.limit.is_some() || so.offset.is_some() {
        return Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "LIMIT in a recursive query is not implemented".to_string(),
        ));
    }
    // The non-recursive (anchor) term — the UNION's left — must not reference the CTE.
    if count_self_refs_query(&so.lhs, name) > 0 {
        return Err(EngineError::new(
            SqlState::InvalidRecursion,
            format!(
                "recursive reference to query \"{name}\" must not appear within its non-recursive term"
            ),
        ));
    }
    // The recursive term — the UNION's right — must be a plain SELECT (a set-operation recursive
    // term is a jed narrowing, 0A000) referencing the CTE exactly once, in a valid position.
    let rhs_sel = match &so.rhs {
        QueryExpr::Select(s) => s,
        QueryExpr::SetOp(_) => {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "a set operation in the recursive term of a recursive query is not supported yet"
                    .to_string(),
            ));
        }
        QueryExpr::With(_) => {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "a nested WITH in the recursive term of a recursive query is not supported yet"
                    .to_string(),
            ));
        }
    };
    validate_recursive_term(name, rhs_sel)?;
    Ok(CteShape::Recursive { union_all: so.all })
}

/// Validate the recursive term (the UNION's right SELECT) of a recursive CTE (recursive-cte.md §6).
/// The self-reference must appear exactly once, as a direct FROM/JOIN relation, not on the nullable
/// side of an outer join; the term must contain no aggregate. The checks fire in PostgreSQL's order
/// — a self-reference in a bad CONTEXT (a sublink, an outer join) is reported as that context even
/// when a valid FROM reference also exists, so context checks precede the once-only count.
fn validate_recursive_term(name: &str, sel: &Select) -> Result<()> {
    // A self-reference inside an expression sublink is `within a subquery` (matching PostgreSQL),
    // regardless of any valid FROM reference also present.
    if count_sublink_self_refs(sel, name) >= 1 {
        return Err(EngineError::new(
            SqlState::InvalidRecursion,
            format!("recursive reference to query \"{name}\" must not appear within a subquery"),
        ));
    }
    // A self-reference nested inside a FROM derived table is a jed narrowing (0A000) — PostgreSQL
    // allows `FROM (… c …)`.
    if count_from_subquery_self_refs(sel, name) >= 1 {
        return Err(EngineError::new(
            SqlState::FeatureNotSupported,
            format!(
                "recursive reference to query \"{name}\" inside a FROM subquery is not supported yet"
            ),
        ));
    }
    // The remaining self-references are all direct FROM/JOIN relations.
    let direct = count_direct_from_self_refs(sel, name);
    if direct > 1 {
        return Err(EngineError::new(
            SqlState::InvalidRecursion,
            format!("recursive reference to query \"{name}\" must not appear more than once"),
        ));
    }
    // An aggregate in the recursive term is rejected (matching PostgreSQL).
    if items_have_aggregate(&sel.items)
        || sel.having.as_ref().is_some_and(|h| expr_has_aggregate(h))
    {
        return Err(EngineError::new(
            SqlState::InvalidRecursion,
            "aggregate functions are not allowed in a recursive query's recursive term".to_string(),
        ));
    }
    // A direct reference on the nullable side of an outer join is `within an outer join`.
    if direct == 1 && direct_self_ref_on_nullable_side(sel, name) {
        return Err(EngineError::new(
            SqlState::InvalidRecursion,
            format!("recursive reference to query \"{name}\" must not appear within an outer join"),
        ));
    }
    Ok(())
}

/// Self-references reachable only through an expression sublink (a scalar/`IN`/`EXISTS`/quantified
/// subquery) in this SELECT's top-level expressions — the `within a subquery` position.
fn count_sublink_self_refs(s: &Select, name: &str) -> usize {
    select_exprs(s)
        .iter()
        .map(|e| count_self_refs_expr(e, name))
        .sum()
}

/// Check a recursive CTE's column types (recursive-cte.md §2): the output types are FIXED by the
/// non-recursive (anchor) term, and the recursive term's columns must be assignable to them — a
/// literal adapts, an equal type passes, a WIDER type is `42804` (matching PostgreSQL). Mechanically
/// the would-be UNION unified type must EQUAL the anchor type; any widening of the anchor is the
/// error. An arity mismatch is `42601`, like a plain UNION.
fn check_recursive_column_types(
    anchor: &QueryPlan,
    recursive: &QueryPlan,
    name: &str,
) -> Result<()> {
    let a = anchor.column_types();
    let r = recursive.column_types();
    if a.len() != r.len() {
        return Err(EngineError::new(
            SqlState::SyntaxError,
            "each UNION query must have the same number of columns".to_string(),
        ));
    }
    for (i, (at, rt)) in a.iter().zip(r.iter()).enumerate() {
        let unified = unify_setop_column(at, rt, SetOpKind::Union)?;
        if &unified != at {
            return Err(EngineError::new(
                SqlState::DatatypeMismatch,
                format!(
                    "recursive query \"{name}\" column {} has type {} in non-recursive term but type {} overall",
                    i + 1,
                    at.type_name(),
                    unified.type_name(),
                ),
            ));
        }
    }
    Ok(())
}

/// Total self-references to `name` anywhere in a query expression (deep — FROM relations at every
/// nesting level plus expression sublinks).
fn count_self_refs_query(qe: &QueryExpr, name: &str) -> usize {
    match qe {
        QueryExpr::Select(s) => count_self_refs_select(s, name),
        QueryExpr::SetOp(so) => {
            count_self_refs_query(&so.lhs, name) + count_self_refs_query(&so.rhs, name)
        }
        // A nested `WITH` establishes its own CTE scope (spec/design/cte.md §7): an enclosing CTE
        // name is NOT visible inside it (a reference there resolves to a base table / the nested
        // CTE, never the enclosing one), so it contributes no self-reference to the enclosing name.
        QueryExpr::With(_) => 0,
    }
}

/// Total self-references in a SELECT: its FROM relations (deep) plus all of its expressions' sublinks.
fn count_self_refs_select(s: &Select, name: &str) -> usize {
    let mut n = 0;
    for tref in from_relations(s) {
        n += count_self_refs_tableref(tref, name);
    }
    for e in select_exprs(s) {
        n += count_self_refs_expr(e, name);
    }
    n
}

/// Self-references reachable through one FROM relation: a plain table reference with the matching
/// name (+1), a derived-table subquery (recurse), or a table-function's / VALUES' argument exprs.
fn count_self_refs_tableref(tref: &TableRef, name: &str) -> usize {
    if is_plain_relation(tref) {
        return usize::from(tref.name.eq_ignore_ascii_case(name));
    }
    let mut n = 0;
    if let Some(sub) = &tref.subquery {
        n += count_self_refs_query(sub, name);
    }
    if let Some(args) = &tref.args {
        for a in args {
            n += count_self_refs_expr(a, name);
        }
    }
    if let Some(rows) = &tref.values {
        for row in rows {
            for e in row {
                n += count_self_refs_expr(e, name);
            }
        }
    }
    n
}

/// Self-references inside an expression — only reachable through a sublink (a subquery is an
/// independent query whose own FROM may reference the CTE). Walks every Expr variant to find the
/// sublinks, then recurses into each sublink's query. The walk is exhaustive (like
/// `expr_has_aggregate`) so a new Expr variant is a compile error here, not a silent miss.
fn count_self_refs_expr(e: &Expr, name: &str) -> usize {
    let sub = |x: &Expr| count_self_refs_expr(x, name);
    match e {
        Expr::ScalarSubquery(q) | Expr::Exists(q) => count_self_refs_query(q, name),
        Expr::InSubquery { lhs, query, .. } | Expr::QuantifiedSubquery { lhs, query, .. } => {
            sub(lhs) + count_self_refs_query(query, name)
        }
        Expr::Column(_)
        | Expr::QualifiedColumn { .. }
        | Expr::Literal(_)
        | Expr::TypedLiteral { .. }
        | Expr::Param(_) => 0,
        Expr::Row(items) | Expr::Array(items) => items.iter().map(sub).sum(),
        Expr::FieldAccess { base, .. } | Expr::FieldStar { base } => sub(base),
        // `t.*` is a leaf (a relation name, no sub-expression) — no sublink to recurse into.
        Expr::QualifiedStar { .. } => 0,
        Expr::Subscript { base, subscripts } => {
            sub(base)
                + subscripts
                    .iter()
                    .flat_map(subscript_spec_exprs)
                    .map(sub)
                    .sum::<usize>()
        }
        Expr::Cast { inner, .. }
        | Expr::Collate { inner, .. }
        | Expr::Extract { source: inner, .. } => sub(inner),
        Expr::Unary { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::IsJson { operand, .. }
        | Expr::JsonCtor { operand, .. } => sub(operand),
        Expr::JsonExists { ctx, path, .. }
        | Expr::JsonValue { ctx, path, .. }
        | Expr::JsonQuery { ctx, path, .. } => sub(ctx) + sub(path),
        Expr::Binary { lhs, rhs, .. }
        | Expr::IsDistinctFrom { lhs, rhs, .. }
        | Expr::Like { lhs, rhs, .. }
        | Expr::Regex { lhs, rhs, .. } => sub(lhs) + sub(rhs),
        Expr::In { lhs, list, .. } => sub(lhs) + list.iter().map(sub).sum::<usize>(),
        Expr::Quantified { lhs, array, .. } => sub(lhs) + sub(array),
        Expr::Between { lhs, lo, hi, .. } => sub(lhs) + sub(lo) + sub(hi),
        Expr::Case {
            operand,
            whens,
            els,
        } => {
            operand.as_deref().map_or(0, sub)
                + whens.iter().map(|(c, r)| sub(c) + sub(r)).sum::<usize>()
                + els.as_deref().map_or(0, sub)
        }
        Expr::Coalesce(args) => args.iter().map(sub).sum(),
        Expr::GreatestLeast { args, .. } => args.iter().map(sub).sum(),
        Expr::FuncCall { args, .. } => args.iter().map(sub).sum(),
    }
}

/// Whether a `WITH` statement contains any data-modifying part — a data-modifying CTE body or a
/// data-modifying primary (spec/design/writable-cte.md). Such a statement runs through the
/// writable-CTE orchestrator (the read pin + lexical-order, all-or-nothing execution); a pure-query
/// `WITH` keeps the [`Engine::run_with`] path.
fn with_has_dml(wq: &WithQuery) -> bool {
    wq.body.is_data_modifying() || wq.ctes.iter().any(|c| c.body.is_data_modifying())
}

/// Each CTE binding's evaluation mode (spec/design/cte.md §3, writable-cte.md §3): a RECURSIVE or
/// data-modifying CTE is ALWAYS materialized; otherwise a `MATERIALIZED` hint or ≥2 references →
/// Materialize, else Inline.
fn cte_modes(bindings: &[CteBinding]) -> Vec<CteMode> {
    bindings
        .iter()
        .map(|b| {
            if b.recursive.is_some() || matches!(b.source, CteSource::Dml(_)) {
                return CteMode::Materialize;
            }
            match b.hint {
                Some(true) => CteMode::Materialize,
                Some(false) => CteMode::Inline,
                None if b.refs.get() >= 2 => CteMode::Materialize,
                None => CteMode::Inline,
            }
        })
        .collect()
}

/// Add `extra` cost to an outcome (the writable-CTE orchestrator folds the materialization cost of
/// the data-modifying / query CTEs into the primary's result — spec/design/writable-cte.md §8).
fn add_outcome_cost(outcome: Outcome, extra: i64) -> Outcome {
    match outcome {
        Outcome::Query {
            column_names,
            column_types,
            rows,
            cost,
        } => Outcome::Query {
            column_names,
            column_types,
            rows,
            cost: cost + extra,
        },
        Outcome::Statement {
            cost,
            rows_affected,
        } => Outcome::Statement {
            cost: cost + extra,
            rows_affected,
        },
    }
}

/// References to CTE `name` reachable through a `cte_body`'s inner queries — the writable-CTE
/// analogue of [`count_self_refs_query`] (spec/design/writable-cte.md §3). A query body delegates to
/// the query counter; a data-modifying body counts the references in its source query / `WHERE` /
/// `SET` RHSs / `ON CONFLICT` / `RETURNING` sublinks. Used by the orchestrator to count the
/// references a NON-planned data-modifying part contributes to the inline-vs-materialize decision.
fn count_cte_refs_dml(body: &CteBody, name: &str) -> usize {
    match body {
        CteBody::Query(q) => count_self_refs_query(q, name),
        CteBody::Insert(ins) => {
            let mut n = match &ins.source {
                InsertSource::Select(sel) => count_self_refs_select(sel, name),
                // VALUES slots hold literals / params / ROW / ARRAY (no sublinks this slice).
                InsertSource::Values(_) => 0,
            };
            if let Some(oc) = &ins.on_conflict {
                if let ConflictAction::DoUpdate {
                    assignments,
                    filter,
                } = &oc.action
                {
                    for a in assignments {
                        n += count_self_refs_expr(&a.value, name);
                    }
                    if let Some(f) = filter {
                        n += count_self_refs_expr(f, name);
                    }
                }
            }
            n + count_returning_refs(&ins.returning, name)
        }
        CteBody::Update(upd) => {
            let mut n = 0;
            for a in &upd.assignments {
                n += count_self_refs_expr(&a.value, name);
            }
            if let Some(f) = &upd.filter {
                n += count_self_refs_expr(f, name);
            }
            n + count_returning_refs(&upd.returning, name)
        }
        CteBody::Delete(del) => {
            let mut n = 0;
            if let Some(f) = &del.filter {
                n += count_self_refs_expr(f, name);
            }
            n + count_returning_refs(&del.returning, name)
        }
    }
}

/// References to CTE `name` in a `RETURNING` item list's sublinks.
fn count_returning_refs(returning: &Option<ReturningClause>, name: &str) -> usize {
    match returning {
        Some(ReturningClause {
            items: SelectItems::Items(items),
            ..
        }) => items
            .iter()
            .map(|it| count_self_refs_expr(&it.expr, name))
            .sum(),
        _ => 0,
    }
}

/// Self-references that are DIRECT FROM/JOIN relations of this SELECT (a plain table ref matching
/// the name, not nested in a subquery). This is the only valid position for a recursive reference.
fn count_direct_from_self_refs(s: &Select, name: &str) -> usize {
    from_relations(s)
        .filter(|tref| is_plain_relation(tref) && tref.name.eq_ignore_ascii_case(name))
        .count()
}

/// Self-references nested inside a FROM-position subquery / table-function args / VALUES of this
/// SELECT (the deferred `0A000` shape — PostgreSQL allows a self-reference in `FROM (… c …)`).
fn count_from_subquery_self_refs(s: &Select, name: &str) -> usize {
    from_relations(s)
        .filter(|tref| !is_plain_relation(tref))
        .map(|tref| count_self_refs_tableref(tref, name))
        .sum()
}

/// Whether the SELECT's single direct self-reference sits on the NULLABLE side of an outer join —
/// the position PostgreSQL rejects (`within an outer join`). The FROM is a left-deep chain: relation
/// 0 is `from`, relation `i+1` is `joins[i].table`, combined by `joins[i].kind`. A LEFT/FULL join
/// makes its right operand nullable; a RIGHT/FULL join makes the whole accumulated left nullable.
fn direct_self_ref_on_nullable_side(s: &Select, name: &str) -> bool {
    let rels: Vec<&TableRef> = from_relations(s).collect();
    let mut nullable = vec![false; rels.len()];
    for (j, jc) in s.joins.iter().enumerate() {
        let right = j + 1;
        match jc.kind {
            JoinKind::Left => nullable[right] = true,
            JoinKind::Right => nullable.iter_mut().take(right).for_each(|n| *n = true),
            JoinKind::Full => nullable.iter_mut().take(right + 1).for_each(|n| *n = true),
            JoinKind::Inner | JoinKind::Cross => {}
        }
    }
    rels.iter().enumerate().any(|(i, tref)| {
        is_plain_relation(tref) && tref.name.eq_ignore_ascii_case(name) && nullable[i]
    })
}

/// A FROM relation that is a plain table NAME — not a derived-table subquery, a table function, or
/// a VALUES body. Only a plain relation can resolve to a CTE.
fn is_plain_relation(tref: &TableRef) -> bool {
    tref.subquery.is_none() && tref.args.is_none() && tref.values.is_none()
}

/// The FROM relations of a SELECT in left-deep order: `from` (if present) then each join's table.
fn from_relations(s: &Select) -> impl Iterator<Item = &TableRef> {
    s.from.iter().chain(s.joins.iter().map(|j| &j.table))
}

/// Every top-level expression of a SELECT that can hold a sublink (select items, WHERE, GROUP BY,
/// HAVING, join ON conditions) — for the sublink self-reference walk. ORDER BY keys are bare /
/// qualified column references (never expressions — `OrderKey`), so they carry no sublink.
fn select_exprs(s: &Select) -> Vec<&Expr> {
    let mut v: Vec<&Expr> = Vec::new();
    if let SelectItems::Items(items) = &s.items {
        v.extend(items.iter().map(|it| &it.expr));
    }
    v.extend(s.filter.iter());
    for item in &s.group_by {
        item.for_each_expr(&mut |e| v.push(e));
    }
    v.extend(s.having.iter());
    v.extend(s.joins.iter().filter_map(|j| j.on.as_ref()));
    v
}

/// Build the synthetic relation a CTE reference resolves against (spec/design/cte.md §2): one
/// column per body output, named by the rename list (a count mismatch is 42P10) or the body's own
/// output names, typed from the planned body. The relation has no primary key / constraints — it is
/// read-only and its rows come from the CTE context, never a store.
fn cte_synthetic_table(
    name: &str,
    plan: &QueryPlan,
    rename: Option<&[String]>,
) -> Result<Box<Table>> {
    cte_synthetic_table_cols(name, plan.column_names(), plan.column_types(), rename)
}

/// The shared core of [`cte_synthetic_table`], over explicit body column names + types — so a
/// data-modifying CTE (whose "body output" is its `RETURNING` projection, not a `QueryPlan`) builds
/// its synthetic relation the same way (spec/design/writable-cte.md §1).
fn cte_synthetic_table_cols(
    name: &str,
    body_names: &[String],
    body_types: &[ResolvedType],
    rename: Option<&[String]>,
) -> Result<Box<Table>> {
    let col_names: Vec<String> = match rename {
        // PostgreSQL allows FEWER aliases than the body has columns — the first `cols.len()` columns
        // take the aliases, the rest keep their body output names (a partial rename). Only MORE
        // aliases than columns is an error (42P10).
        Some(cols) => {
            if cols.len() > body_types.len() {
                return Err(EngineError::new(
                    SqlState::InvalidColumnReference,
                    format!(
                        "WITH query \"{name}\" has {} columns available but {} columns specified",
                        body_types.len(),
                        cols.len()
                    ),
                ));
            }
            (0..body_types.len())
                .map(|i| {
                    cols.get(i)
                        .cloned()
                        .unwrap_or_else(|| body_names[i].clone())
                })
                .collect()
        }
        None => body_names.to_vec(),
    };
    let columns = col_names
        .iter()
        .zip(body_types.iter())
        .map(|(n, t)| {
            Ok(Column {
                name: n.clone(),
                ty: type_from_resolved(t)?,
                decimal: None,
                varchar_len: None,
                primary_key: false,
                not_null: false,
                default: None,
                default_expr: None,
                identity: None,
                collation: None,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Box::new(Table {
        name: name.to_string(),
        columns,
        pk: Vec::new(),
        checks: Vec::new(),
        indexes: Vec::new(),
        foreign_keys: Vec::new(),
        exclusions: Vec::new(),
    }))
}

/// The catalog `Type` whose `resolved_type_of_col` round-trips to `rt` — used to give a CTE's
/// synthetic columns a `Type` (spec/design/cte.md). An untyped NULL column maps to `text`
/// (PostgreSQL's unknown -> text rule). A decimal's per-column typmod is irrelevant for a read-only
/// CTE column (values flow through unchanged), so it is dropped. An anonymous `ROW(...)` composite
/// has no catalog type to name — deferred (0A000), a corner not reached by the corpus.
fn type_from_resolved(rt: &ResolvedType) -> Result<Type> {
    Ok(match rt {
        ResolvedType::Int(s) | ResolvedType::Float(s) => Type::Scalar(*s),
        ResolvedType::Bool => Type::Scalar(ScalarType::Bool),
        ResolvedType::Text | ResolvedType::Null => Type::Scalar(ScalarType::Text),
        ResolvedType::Decimal => Type::Scalar(ScalarType::Decimal),
        ResolvedType::Bytea => Type::Scalar(ScalarType::Bytea),
        ResolvedType::Uuid => Type::Scalar(ScalarType::Uuid),
        ResolvedType::Json => Type::Scalar(ScalarType::Json),
        ResolvedType::JsonPath => Type::Scalar(ScalarType::JsonPath),
        ResolvedType::Jsonb => Type::Scalar(ScalarType::Jsonb),
        ResolvedType::Timestamp => Type::Scalar(ScalarType::Timestamp),
        ResolvedType::Timestamptz => Type::Scalar(ScalarType::Timestamptz),
        ResolvedType::Date => Type::Scalar(ScalarType::Date),
        ResolvedType::Interval => Type::Scalar(ScalarType::Interval),
        ResolvedType::Composite(r) => match &r.name {
            Some(n) => Type::Composite(crate::types::CompositeRef { name: n.clone() }),
            None => {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "an anonymous composite column in a CTE is not supported yet",
                ));
            }
        },
        ResolvedType::Array(elem) => Type::Array(Box::new(type_from_resolved(elem)?)),
        // A range-typed CTE column is deferred (range columns are not storable yet — R2); the
        // value itself works in expression position, just not as a materialized column type.
        ResolvedType::Range(_) => {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "a range column in a CTE is not supported yet",
            ));
        }
    })
}

/// The scalar element type of a resolved range element (`ResolvedType::Range(elem)`'s `elem`). A
/// range's element is always one of the six scalar subtypes; `None` for anything else (which never
/// occurs for a valid range). Used to name a range (`i32` → `i32range`) and to build its codec.
fn resolved_range_element_scalar(elem: &ResolvedType) -> Option<ScalarType> {
    match elem {
        ResolvedType::Int(s) => Some(*s),
        ResolvedType::Decimal => Some(ScalarType::Decimal),
        ResolvedType::Timestamp => Some(ScalarType::Timestamp),
        ResolvedType::Timestamptz => Some(ScalarType::Timestamptz),
        ResolvedType::Date => Some(ScalarType::Date),
        _ => None,
    }
}

/// One relation in a SELECT plan: the table name (looked up in the store at exec), the flat
/// offset of its first column in the joined row, and its column count (for NULL-padding). When
/// `srf` is `Some`, the relation is a COMPUTED set-returning function (generate_series) rather
/// than a base table: `table_name` is then the function name (never looked up in the store) and
/// the executor generates the rows instead of scanning (spec/design/functions.md §10).
pub(crate) struct PlanRel {
    table_name: String,
    /// The relation's explicit database qualifier (attached-databases.md §3), passed to the scope-aware
    /// store funnels at exec (`store_scoped` etc.). `None` for a bare implicit-scope name → the funnels
    /// fall through to the temp-first walk (behavior-neutral for every unqualified query).
    db: Option<String>,
    offset: usize,
    col_count: usize,
    srf: Option<SrfPlan>,
    /// When `Some(i)`, this relation is a reference to common-table-expression `i` (the index into
    /// the statement's CTE list — spec/design/cte.md), not a base table: `table_name` is then the
    /// CTE name (never looked up in the store) and the executor delivers its rows from the
    /// per-statement `CteCtx` (a materialized buffer, or the inlined body run in place).
    cte: Option<usize>,
    /// When `Some(plan)`, this relation is a DERIVED TABLE — `FROM (SELECT …) AS t`
    /// (spec/design/grammar.md §42): a parenthesized subquery used as a relation, mechanically an
    /// anonymous always-inlined single-reference CTE. `table_name` is the alias (never looked up in
    /// the store); the executor runs `plan` in place, charging its intrinsic cost — no `cte_scan_row`.
    /// Non-lateral (`lateral = false`) it reads no outer row; a lateral one reads the left-hand row.
    derived: Option<Box<QueryPlan>>,
    /// When true, this relation is a CORRELATED `LATERAL` item (spec/design/grammar.md §44): its
    /// derived body / SRF args reference an earlier sibling (or an enclosing query), so the executor
    /// re-materializes it ONCE PER combined left-hand row (with that row pushed as its immediate outer
    /// — the correlated-subquery mechanism), rather than materializing it once. Always `false` for the
    /// first relation. Only a `srf` or `derived` relation is ever lateral.
    lateral: bool,
}

/// How a referenced CTE is evaluated (spec/design/cte.md §3, cost.md §3). Decided per CTE from its
/// reference count and `[NOT] MATERIALIZED` hint: a single-reference CTE is `Inline`, a
/// multi-reference (or `MATERIALIZED`) one is `Materialize`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CteMode {
    /// Run the body in place at each reference (re-evaluates per outer row under correlation,
    /// matching PostgreSQL); charges the body's intrinsic cost, no `cte_scan_row`.
    Inline,
    /// Run the body once, buffer the rows; each reference scans the buffer, charging `cte_scan_row`
    /// per buffered row.
    Materialize,
}

/// The per-statement CTE execution context, threaded through `exec_*` and `EvalEnv` so a FROM
/// reference (any nesting depth) can deliver a CTE's rows (spec/design/cte.md §5). `modes` and
/// `bindings` are fixed after planning; `buffers` is filled before the main query runs — one slot
/// per CTE in list order, holding the materialized rows of a `Materialize` CTE (an empty
/// placeholder for an `Inline` one, whose body is run in place from `bindings[ci].source` instead).
/// `bindings` also serves a data-modifying CTE's own inner queries, which resolve against the
/// earlier bindings when the writable-CTE orchestrator executes them (writable-cte.md §2).
#[derive(Clone, Copy)]
pub(crate) struct CteCtx<'a> {
    modes: &'a [CteMode],
    bindings: &'a [CteBinding],
    buffers: &'a [Vec<Row>],
}

impl CteCtx<'_> {
    /// The empty context — no CTEs in scope (every non-`WITH` execution path).
    fn empty() -> CteCtx<'static> {
        CteCtx {
            modes: &[],
            bindings: &[],
            buffers: &[],
        }
    }
}

/// Which set-returning function a [`SrfPlan`] is, selecting the row generator at exec
/// (spec/design/functions.md §10, array-functions.md §9). The dispatch is hand-written per core;
/// the resolution narrows the catalog name to one of these.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SrfKind {
    /// `generate_series(start, stop[, step])` — an integer series (functions.md §10).
    GenerateSeries,
    /// `unnest(anyarray)` — one row per array element, flattened row-major (array-functions.md §9).
    Unnest,
    /// `jsonb_array_elements(jsonb)` — one `jsonb` row per array element (json-sql-functions.md §3).
    JsonbArrayElements,
    /// `jsonb_array_elements_text(jsonb)` — one `text` row per array element (the `->>`-style render).
    JsonbArrayElementsText,
    /// `jsonb_object_keys(jsonb)` — one `text` row per object key, in canonical key order.
    JsonbObjectKeys,
    /// `json_object_keys(json)` — one `text` row per object key, in INPUT order (duplicates kept).
    JsonObjectKeys,
    /// `jsonb_each(jsonb)` — one `(key text, value jsonb)` row per top-level object member, canonical
    /// key order (json-sql-functions.md §3). A two-column SRF (the C0 multi-column synthetic table).
    JsonbEach,
    /// `jsonb_each_text(jsonb)` — one `(key text, value text)` row per member (the `->>`-style value).
    JsonbEachText,
    /// `json[b]_to_record` / `json[b]_to_recordset` (R1, json-table.md §2): map a JSON object's members
    /// to the C0 col-def-list columns by name, coercing each to its declared type. `set` = the
    /// recordset form (one row per array element); else one record row. `jsonb` selects the input type.
    JsonRecord { jsonb: bool, set: bool },
    /// `jsonb_path_query(jsonb, jsonpath)` (P2, jsonpath.md §5.2): one `jsonb` row per item of the
    /// path's evaluation sequence over the context document. `args` is `[ctx, path]`.
    JsonbPathQuery,
    /// `JSON_TABLE(ctx, path COLUMNS (…))` (T1, json-table.md §3): a multi-column relation produced by
    /// the recursive default-plan expansion. `args` is `[ctx]`; the resolved column tree is the
    /// SrfPlan's `json_table` field.
    JsonTable,
    /// The `jed_tables` catalog relation (spec/design/introspection.md §5): a read-only COMPUTED
    /// relation — one row per user table of the qualified database, derived at execution from its
    /// pinned catalog snapshot. Not a function (it is resolved as a table name), but it rides the
    /// SRF plan shape so every "computed, not scanned" gate handles it: no store, no index
    /// pushdown, no PK order, excluded from the fast-path lanes. `args` is empty; the scope is the
    /// SrfPlan's `introspect_scope`.
    JedTables,
    /// The `jed_columns` catalog relation (introspection.md §5) — one row per column of every user
    /// table of the qualified database, in (table, ordinal) order.
    JedColumns,
    /// The `jed_indexes` catalog relation (introspection.md §5.1, slice I2) — one row per secondary
    /// index of every user table of the qualified database (name, table, columns, is_unique,
    /// method), in (table, index-name) order.
    JedIndexes,
    /// The `jed_constraints` catalog relation (introspection.md §5.1, slice I2) — one row per
    /// CHECK / UNIQUE / FK / EXCLUDE constraint of every user table, in (table, kind, name) order.
    JedConstraints,
    /// The P9 one-row-per-analyzed-column statistics summary relation.
    JedStatistics,
}

/// A resolved `JSON_TABLE` plan (T1, json-table.md §3) — the compiled root path + the column tree.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct JtPlan {
    /// The compiled root jsonpath (its evaluation over `ctx` yields the row items).
    root_path: String,
    /// The total number of flattened output columns.
    width: usize,
    /// The top-level column tree.
    columns: Vec<JtCol>,
}

/// One resolved `JSON_TABLE` column (json-table.md §3.3). Leaf columns carry their flat output index.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum JtCol {
    /// `FOR ORDINALITY` — the level's 1-based row counter.
    Ordinality { idx: usize },
    /// A regular column: evaluate `path` over the row item, apply JSON_VALUE (scalar) or JSON_QUERY
    /// (json/jsonb) semantics, coerce to `returning`.
    Regular {
        idx: usize,
        returning: ScalarType,
        decimal: Option<DecimalTypmod>,
        path: String,
        /// JSON_QUERY semantics (json/jsonb returning) vs JSON_VALUE (scalar).
        query: bool,
        wrapper: JsonWrapper,
        on_empty: JsonOnBehavior,
        on_error: JsonOnBehavior,
    },
    /// An `EXISTS` column: JSON_EXISTS of `path`, coerced to `returning` (bool/int).
    Exists {
        idx: usize,
        returning: ScalarType,
        path: String,
        on_error: JsonOnBehavior,
    },
    /// A `NESTED PATH` subtree: expanded over the row item (the default-plan LEFT OUTER / sibling UNION).
    Nested { path: String, columns: Vec<JtCol> },
}

/// A resolved set-returning-function row source (spec/design/functions.md §10, array-functions.md
/// §9). `kind` selects the generator: `generate_series(start, stop[, step])` (`args` = 2 or 3
/// integers) or `unnest(anyarray)` (`args` = the single array expression). Non-LATERAL, so each
/// arg evaluates against the params/outer environment with no local row. The produced column's
/// type lives on the synthetic relation (built in `resolve_srf`), so the plan needs only the
/// resolved arg expressions here.
pub(crate) struct SrfPlan {
    kind: SrfKind,
    args: Vec<RExpr>,
    /// The declared output columns for a record-returning SRF (`JsonRecord`) — the C0 col-def list,
    /// used to map JSON members to columns by name + coerce. Empty for every other SRF kind.
    record_cols: Vec<Column>,
    /// The resolved column tree for a `JSON_TABLE` SRF (`JsonTable`), else `None`.
    json_table: Option<Box<JtPlan>>,
    /// The validated database scope of a catalog relation (`JedTables` / `JedColumns` —
    /// introspection.md §5): `"main"` (also the unqualified default), `"temp"`, or a lowercased
    /// attachment name. Empty for every other kind.
    introspect_scope: String,
}

/// Classify a relation name as a built-in catalog relation (introspection.md §5): `jed_tables` /
/// `jed_columns`, case-insensitively (identifier resolution folds case; grammar.md §3 leaves no
/// quoted escape). Built-in names resolve in every database's relation namespace, checked AFTER a
/// statement-local CTE (a CTE shadows a catalog relation — PG-matching, oracle-checked) and BEFORE
/// the user catalog (post-I0 the two can never collide; for a pre-reservation legacy file the
/// built-in wins and the user relation is unreachable by name — §5).
fn catalog_rel_kind(name: &str) -> Option<SrfKind> {
    match name.to_ascii_lowercase().as_str() {
        "jed_tables" => Some(SrfKind::JedTables),
        "jed_columns" => Some(SrfKind::JedColumns),
        "jed_indexes" => Some(SrfKind::JedIndexes),
        "jed_constraints" => Some(SrfKind::JedConstraints),
        "jed_statistics" => Some(SrfKind::JedStatistics),
        _ => None,
    }
}

/// Whether `name` is a built-in catalog relation (`jed_tables` / `jed_columns`). The write paths
/// use it to reject a catalog relation as a mutation/DDL target (`42809` — a catalog relation is
/// read-only, introspection.md §5); the privilege gate uses it so a built-in is SELECT-gated
/// exactly like a user table under an explicit-grant session envelope.
fn is_catalog_rel_name(name: &str) -> bool {
    catalog_rel_kind(name).is_some()
}

/// The access-method name rendered by `jed_indexes.method` (introspection.md §5.1): the PostgreSQL
/// `amname` spelling of the index kind.
fn index_method_name(kind: IndexKind) -> &'static str {
    match kind {
        IndexKind::Btree => "btree",
        IndexKind::Gin => "gin",
        IndexKind::Gist => "gist",
    }
}

/// Reject a mutation target (INSERT / UPDATE / DELETE / CREATE INDEX ON) naming a built-in catalog
/// relation: `42809` wrong_object_type, `cannot modify system relation` (introspection.md §5 — the
/// relations are read-only computed views of the catalog). Checked by NAME, before qualifier
/// validation: the built-in resolves in every database's namespace, so the rejection is
/// scope-independent.
fn check_catalog_rel_write(name: &str) -> Result<()> {
    if is_catalog_rel_name(name) {
        return Err(EngineError::new(
            SqlState::WrongObjectType,
            format!(
                "cannot modify system relation \"{}\"",
                name.to_ascii_lowercase()
            ),
        ));
    }
    Ok(())
}

/// Build the FIXED synthetic schema of a catalog relation (introspection.md §5). Unlike an SRF's
/// single-column alias rule, a FROM alias renames the RELATION only — the column names are part of
/// the introspection surface. Growth is by ADDING columns (consumers select by name, not position
/// — §5).
fn catalog_rel_table(kind: SrfKind) -> Box<Table> {
    let mk_col = |name: &str, ty: Type, not_null: bool| Column {
        name: name.to_string(),
        ty,
        decimal: None,
        varchar_len: None,
        primary_key: false,
        not_null,
        default: None,
        default_expr: None,
        identity: None,
        collation: None,
    };
    let col = |name: &str, ty: ScalarType, not_null: bool| mk_col(name, Type::Scalar(ty), not_null);
    // A `text[]` member-list column (introspection.md §5.1 — the indexed / member column names).
    let text_arr = |name: &str, not_null: bool| {
        mk_col(
            name,
            Type::Array(Box::new(Type::Scalar(ScalarType::Text))),
            not_null,
        )
    };
    let table = |name: &str, columns: Vec<Column>| {
        Box::new(Table {
            name: name.to_string(),
            columns,
            pk: Vec::new(),
            checks: Vec::new(),
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            exclusions: Vec::new(),
        })
    };
    match kind {
        SrfKind::JedTables => table("jed_tables", vec![col("name", ScalarType::Text, true)]),
        SrfKind::JedColumns => table(
            "jed_columns",
            vec![
                col("table_name", ScalarType::Text, true),
                col("name", ScalarType::Text, true),
                col("ordinal", ScalarType::Int32, true),
                col("type", ScalarType::Text, true),
                col("not_null", ScalarType::Bool, true),
                col("pk_ordinal", ScalarType::Int32, false),
            ],
        ),
        SrfKind::JedIndexes => table(
            "jed_indexes",
            vec![
                col("name", ScalarType::Text, true),
                col("table_name", ScalarType::Text, true),
                text_arr("columns", true),
                col("is_unique", ScalarType::Bool, true),
                col("method", ScalarType::Text, true),
                // A partial index's predicate canonical text; NULL for a non-partial index
                // (indexes.md §9, introspection.md §5.1).
                col("predicate", ScalarType::Text, false),
            ],
        ),
        SrfKind::JedConstraints => table(
            "jed_constraints",
            vec![
                col("name", ScalarType::Text, true),
                col("table_name", ScalarType::Text, true),
                col("type", ScalarType::Text, true),
                text_arr("columns", false),
                col("expression", ScalarType::Text, false),
                col("ref_table", ScalarType::Text, false),
                text_arr("ref_columns", false),
            ],
        ),
        SrfKind::JedStatistics => table(
            "jed_statistics",
            vec![
                col("table_name", ScalarType::Text, true),
                col("column_name", ScalarType::Text, true),
                col("analyzed_rows", ScalarType::Int64, true),
                col("is_stale", ScalarType::Bool, true),
                col("null_count", ScalarType::Int64, true),
                col("distinct_count", ScalarType::Int64, false),
                col("sample_rows", ScalarType::Int64, true),
                col("average_width", ScalarType::Int64, false),
                col("mcv_count", ScalarType::Int32, true),
                col("histogram_count", ScalarType::Int32, true),
            ],
        ),
        _ => unreachable!("only catalog-relation kinds reach catalog_rel_table"),
    }
}

/// Render a column's declared type in the CANONICAL introspection form (introspection.md §5): the
/// scalar's canonical name with its typmod applied at the leaf (`varchar(10)`, `decimal(8,2)`), a
/// composite's name as created, a range's canonical id (`i32range`, `numrange`, …), and `[]`
/// appended for an array (the typmod applies to the element: `varchar(5)[]`). This text is a
/// compatibility surface the moment it ships — pinned by the corpus.
fn catalog_type_text(ty: &Type, dec: Option<&DecimalTypmod>, vlen: Option<u32>) -> String {
    match ty {
        Type::Array(elem) => format!("{}[]", catalog_type_text(elem, dec, vlen)),
        Type::Scalar(ScalarType::Text) if vlen.is_some() => {
            format!("varchar({})", vlen.unwrap())
        }
        Type::Scalar(ScalarType::Decimal) if dec.is_some() => {
            let d = dec.unwrap();
            format!("decimal({},{})", d.precision, d.scale)
        }
        // The scalar / composite / range canonical rendering is shared with error messages
        // (types.rs): composite → its name as created, range → the ranges.toml id.
        _ => ty.canonical_name(),
    }
}

/// Build a set-returning function's **synthetic one-column relation** (spec/design/functions.md
/// §10). The table's `name` is the function name (the un-aliased label fallback); the lone column's
/// NAME follows PostgreSQL's single-column function-alias rule — the table alias when one is given,
/// else the function name — and its TYPE is `col_ty` (the promoted integer for `generate_series`,
/// the bound element type for `unnest`).
/// Map a parsed referential action to its persisted form, rejecting the unsupported write-actions
/// (CASCADE / SET NULL / SET DEFAULT) as `0A000` (spec/design/constraints.md §6.6). `clause` is
/// `"DELETE"` or `"UPDATE"` for the message.
fn fk_action(a: RefAction, clause: &str) -> Result<FkAction> {
    match a {
        RefAction::NoAction => Ok(FkAction::NoAction),
        RefAction::Restrict => Ok(FkAction::Restrict),
        RefAction::Cascade => Err(EngineError::new(
            SqlState::FeatureNotSupported,
            format!("ON {clause} CASCADE is not supported"),
        )),
        RefAction::SetNull => Err(EngineError::new(
            SqlState::FeatureNotSupported,
            format!("ON {clause} SET NULL is not supported"),
        )),
        RefAction::SetDefault => Err(EngineError::new(
            SqlState::FeatureNotSupported,
            format!("ON {clause} SET DEFAULT is not supported"),
        )),
    }
}

/// A column-ordinal list as a sorted, deduplicated set (for the order-independent FK
/// referenced-columns ⇄ PK/unique-key set comparison — spec/design/constraints.md §6.2).
fn sorted_unique(v: &[usize]) -> Vec<usize> {
    let mut s = v.to_vec();
    s.sort_unstable();
    s.dedup();
    s
}

fn srf_table(func_name: &str, alias: Option<&str>, col_ty: Type) -> Box<Table> {
    Box::new(Table {
        name: func_name.to_string(),
        columns: vec![Column {
            name: alias.unwrap_or(func_name).to_string(),
            ty: col_ty,
            decimal: None,
            varchar_len: None,
            primary_key: false,
            not_null: false,
            default: None,
            default_expr: None,
            identity: None,
            collation: None,
        }],
        pk: Vec::new(),
        checks: Vec::new(),
        indexes: Vec::new(),
        foreign_keys: Vec::new(),
        exclusions: Vec::new(),
    })
}

/// Build one output row for `json[b]_to_record(set)` (R1): map each declared column to the JSON
/// object's member of that name, coercing it to the column type. A missing member or a JSON null →
/// SQL NULL; a non-object node → `22023`. (json-table.md §2)
fn json_record_row(
    node: &JsonNode,
    cols: &[Column],
    env: &EvalEnv,
    meter: &mut Meter,
) -> Result<Row> {
    let members = match node {
        JsonNode::Object(m) => m,
        _ => {
            return Err(EngineError::new(
                SqlState::InvalidParameterValue,
                "argument of json_to_record must be a JSON object",
            ));
        }
    };
    let mut row = Vec::with_capacity(cols.len());
    for col in cols {
        let member = members.iter().find(|(k, _)| k == &col.name).map(|(_, v)| v);
        let val = match member {
            None | Some(JsonNode::Null) => Value::Null, // missing / JSON null → SQL NULL
            Some(v) => coerce_json_member(v, &col.ty, col.decimal.clone(), env, meter)?,
        };
        row.push(val);
    }
    Ok(row)
}

/// Coerce a JSON member node to a record column's type (R1, the JSON_VALUE scalar path): a `jsonb`
/// column embeds the node, a `json` column its canonical text, every other scalar coerces the node's
/// `->>`-style text through the cast machinery (so `"42"` / `42` → an `int` column, etc.). A
/// composite/array column type is a deferred `0A000`.
/// Resolve a SQL/JSON query function `JSON_EXISTS` / `JSON_VALUE` / `JSON_QUERY`
/// (json-sql-functions.md §5, S2) → an [`RExpr::JsonSqlFn`] + its fixed result type.
#[allow(clippy::too_many_arguments)]
fn resolve_json_sql_fn(
    scope: &Scope,
    kind: JsonSqlKind,
    ctx: &Expr,
    path: &Expr,
    returning: &Option<String>,
    wrapper: JsonWrapper,
    keep_quotes: bool,
    on_empty: &Option<JsonOnBehavior>,
    on_error: &Option<JsonOnBehavior>,
    agg: &mut AggCtx,
    params: &mut ParamTypes,
) -> Result<(RExpr, ResolvedType)> {
    // The context item — json / jsonb / text, coerced to a jsonb document at eval; a bare string
    // literal adapts to jsonb.
    let (rctx, ctx_ty) = resolve(scope, ctx, Some(ScalarType::Jsonb), agg, params)?;
    if !matches!(
        ctx_ty,
        ResolvedType::Jsonb | ResolvedType::Json | ResolvedType::Text | ResolvedType::Null
    ) {
        return Err(EngineError::new(
            SqlState::DatatypeMismatch,
            format!(
                "the context item of a SQL/JSON query function must be json/jsonb/text, not {}",
                ctx_ty.type_name()
            ),
        ));
    }
    // The path — a jsonpath; a bare string literal compiles.
    let (rpath, path_ty) = resolve(scope, path, Some(ScalarType::JsonPath), agg, params)?;
    if !matches!(path_ty, ResolvedType::JsonPath | ResolvedType::Null) {
        return Err(EngineError::new(
            SqlState::DatatypeMismatch,
            "the path of a SQL/JSON query function must be a jsonpath",
        ));
    }
    // OMIT QUOTES is the deferred S2 follow-on (the jsonb-of-bare-text result quirk).
    if !keep_quotes {
        return Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "JSON_QUERY OMIT QUOTES is not supported yet",
        ));
    }
    // The fixed RETURNING scalar type.
    let returning_st = match (kind, returning) {
        (JsonSqlKind::Exists, _) => ScalarType::Bool,
        (JsonSqlKind::Value, None) => ScalarType::Text,
        (JsonSqlKind::Query, None) => ScalarType::Jsonb,
        (_, Some(name)) => ScalarType::from_name(name).ok_or_else(|| {
            EngineError::new(
                SqlState::UndefinedObject,
                format!("type \"{name}\" does not exist"),
            )
        })?,
    };
    // JSON_QUERY's result must be a JSON type (json/jsonb); JSON_VALUE's must be a scalar — a
    // composite/array RETURNING is a deferred 0A000 (it cannot hold an extracted scalar).
    if matches!(kind, JsonSqlKind::Query)
        && !matches!(returning_st, ScalarType::Json | ScalarType::Jsonb)
    {
        return Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "JSON_QUERY RETURNING a non-json type is not supported yet",
        ));
    }
    let on_empty = on_empty.unwrap_or(JsonOnBehavior::Null);
    let on_error = on_error.unwrap_or(match kind {
        JsonSqlKind::Exists => JsonOnBehavior::False,
        _ => JsonOnBehavior::Null,
    });
    Ok((
        RExpr::JsonSqlFn {
            kind,
            ctx: Box::new(rctx),
            path: Box::new(rpath),
            returning: returning_st,
            decimal: None,
            wrapper,
            on_empty,
            on_error,
        },
        resolved_type_of(returning_st),
    ))
}

/// A SQL/JSON error that the query functions' `ON ERROR` clause catches: a data exception (class
/// `22`). Resource / cost aborts (class `53`/`54`) propagate unconditionally.
fn is_sqljson_error(e: &EngineError) -> bool {
    e.code().starts_with("22")
}

/// Apply a constant `ON ERROR` / `ON EMPTY` behavior → a value of the RETURNING type. `underlying`
/// is the SQL/JSON error this behavior replaces (raised verbatim by `ERROR`).
fn apply_json_behavior(
    behavior: JsonOnBehavior,
    underlying: EngineError,
    returning: ScalarType,
    env: &EvalEnv,
    meter: &mut Meter,
) -> Result<Value> {
    match behavior {
        JsonOnBehavior::Error => Err(underlying),
        JsonOnBehavior::Null => Ok(Value::Null),
        JsonOnBehavior::True => Ok(Value::Bool(true)),
        JsonOnBehavior::False => Ok(Value::Bool(false)),
        JsonOnBehavior::Unknown => Ok(Value::Null),
        JsonOnBehavior::EmptyArray => {
            json_node_as_returning(JsonNode::Array(Vec::new()), returning, env, meter)
        }
        JsonOnBehavior::EmptyObject => {
            json_node_as_returning(JsonNode::Object(Vec::new()), returning, env, meter)
        }
    }
}

/// Render a json result node as the RETURNING type: `jsonb` embeds, `json` its canonical text, any
/// other scalar coerces the node's `->>`-style text through the cast machinery.
fn json_node_as_returning(
    node: JsonNode,
    returning: ScalarType,
    env: &EvalEnv,
    meter: &mut Meter,
) -> Result<Value> {
    coerce_json_member(&node, &Type::Scalar(returning), None, env, meter)
}

/// Apply the SQL/JSON query-function semantics (JSON_VALUE / JSON_QUERY) to an evaluated sequence.
/// (JSON_EXISTS is handled inline — non-empty → true.)
#[allow(clippy::too_many_arguments)]
fn eval_json_sql_result(
    kind: JsonSqlKind,
    seq: Vec<JsonNode>,
    returning: ScalarType,
    decimal: Option<DecimalTypmod>,
    wrapper: JsonWrapper,
    on_empty: JsonOnBehavior,
    on_error: JsonOnBehavior,
    env: &EvalEnv,
    meter: &mut Meter,
) -> Result<Value> {
    match kind {
        JsonSqlKind::Exists => Ok(Value::Bool(!seq.is_empty())),
        JsonSqlKind::Value => {
            if seq.is_empty() {
                return apply_json_behavior(
                    on_empty,
                    EngineError::new(SqlState::NoSqlJsonItem, "no SQL/JSON item"),
                    returning,
                    env,
                    meter,
                );
            }
            if seq.len() > 1 {
                return apply_json_behavior(
                    on_error,
                    EngineError::new(
                        SqlState::MoreThanOneSqlJsonItem,
                        "JSON path expression in JSON_VALUE should return singleton scalar item",
                    ),
                    returning,
                    env,
                    meter,
                );
            }
            let item = &seq[0];
            // JSON_VALUE requires a SCALAR item (PG 2203F otherwise).
            if matches!(item, JsonNode::Array(_) | JsonNode::Object(_)) {
                return apply_json_behavior(
                    on_error,
                    EngineError::new(
                        SqlState::SqlJsonMemberNotFound,
                        "JSON path expression in JSON_VALUE should return singleton scalar item",
                    ),
                    returning,
                    env,
                    meter,
                );
            }
            // Coerce the scalar to the RETURNING type (a JSON null → SQL NULL). A coercion failure is
            // a SQL/JSON error honored by ON ERROR.
            match coerce_json_member(item, &Type::Scalar(returning), decimal, env, meter) {
                Ok(v) => Ok(v),
                Err(e) if is_sqljson_error(&e) => {
                    apply_json_behavior(on_error, e, returning, env, meter)
                }
                Err(e) => Err(e),
            }
        }
        JsonSqlKind::Query => {
            let node = match wrapper {
                JsonWrapper::Unconditional => JsonNode::Array(seq),
                JsonWrapper::Conditional => {
                    if seq.len() == 1 {
                        seq.into_iter().next().unwrap()
                    } else {
                        JsonNode::Array(seq)
                    }
                }
                JsonWrapper::Without => {
                    if seq.is_empty() {
                        return apply_json_behavior(
                            on_empty,
                            EngineError::new(SqlState::NoSqlJsonItem, "no SQL/JSON item"),
                            returning,
                            env,
                            meter,
                        );
                    }
                    if seq.len() > 1 {
                        return apply_json_behavior(
                            on_error,
                            EngineError::new(
                                SqlState::MoreThanOneSqlJsonItem,
                                "JSON path expression in JSON_QUERY should return singleton item without wrapper",
                            ),
                            returning,
                            env,
                            meter,
                        );
                    }
                    seq.into_iter().next().unwrap()
                }
            };
            json_node_as_returning(node, returning, env, meter)
        }
    }
}

fn coerce_json_member(
    node: &JsonNode,
    col_ty: &Type,
    decimal: Option<DecimalTypmod>,
    env: &EvalEnv,
    meter: &mut Meter,
) -> Result<Value> {
    match col_ty {
        Type::Scalar(ScalarType::Jsonb) => Ok(Value::Jsonb(node.clone())),
        Type::Scalar(ScalarType::Json) => Ok(Value::Json(json::jsonb_out(node))),
        Type::Scalar(st) => match json::node_to_text(node) {
            None => Ok(Value::Null),
            Some(text) => {
                let (rexpr, _) = coerce_string_literal(&text, *st, decimal, None)?;
                rexpr.eval(&[], env, meter)
            }
        },
        _ => Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "a composite/array record column is not supported yet",
        )),
    }
}

// ----------------------------------------------------------------------------------------------
// JSON_TABLE helpers (T1, json-table.md §3)
// ----------------------------------------------------------------------------------------------

/// A sparse assignment of a `JSON_TABLE` row — `(flat column index, value)` pairs; unassigned
/// columns are NULL (the LEFT-OUTER / sibling-UNION fill).
pub(crate) type JtAssign = Vec<(usize, Value)>;

/// Build a synthetic `JSON_TABLE` output column.
fn jt_column(name: &str, ty: ScalarType, decimal: Option<DecimalTypmod>) -> Column {
    Column {
        name: name.to_string(),
        ty: Type::Scalar(ty),
        decimal,
        varchar_len: None,
        primary_key: false,
        not_null: false,
        default: None,
        default_expr: None,
        identity: None,
        collation: None,
    }
}

/// Resolve a `JSON_TABLE` column type name → its scalar type (a composite → `0A000`, an unknown
/// name → `42704`).
fn jt_scalar_type(db: &Engine, type_name: &str) -> Result<ScalarType> {
    if let Some(st) = ScalarType::from_name(type_name) {
        Ok(st)
    } else if db.composite_type(type_name).is_some() {
        Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "a composite JSON_TABLE column is not supported yet",
        ))
    } else {
        Err(EngineError::new(
            SqlState::UndefinedObject,
            format!("type \"{type_name}\" does not exist"),
        ))
    }
}

/// Compile a `JSON_TABLE` column path — the explicit `PATH p`, or the default `$.<column_name>` —
/// to its canonical rendered form (validating; malformed → 42601).
fn jt_compile_path(path: Option<&str>, name: &str) -> Result<String> {
    let src = match path {
        Some(p) => p.to_string(),
        None => format!("$.{name}"),
    };
    Ok(crate::jsonpath::JsonPath::compile(&src)?.render())
}

/// Expand a `JSON_TABLE` COLUMNS level over a sequence of row items → the sparse rows (the
/// parent→child LEFT OUTER product with sibling NESTED paths UNIONed, json-table.md §3.3).
fn expand_jt_level(
    cols: &[JtCol],
    items: &[JsonNode],
    env: &EvalEnv,
    meter: &mut Meter,
) -> Result<Vec<JtAssign>> {
    let mut rows: Vec<JtAssign> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        meter.guard()?;
        let ord = (i + 1) as i64;
        // This level's non-nested columns (regular / exists / ordinality).
        let mut local: JtAssign = Vec::new();
        for col in cols {
            match col {
                JtCol::Ordinality { idx } => local.push((*idx, Value::Int(ord))),
                JtCol::Regular {
                    idx,
                    returning,
                    decimal,
                    path,
                    query,
                    wrapper,
                    on_empty,
                    on_error,
                } => {
                    let v = eval_jt_regular(
                        item, path, *query, *returning, *decimal, *wrapper, *on_empty, *on_error,
                        env, meter,
                    )?;
                    local.push((*idx, v));
                }
                JtCol::Exists {
                    idx,
                    returning,
                    path,
                    on_error,
                } => {
                    let v = eval_jt_exists(item, path, *returning, *on_error)?;
                    local.push((*idx, v));
                }
                JtCol::Nested { .. } => {}
            }
        }
        // The NESTED siblings, expanded over this item (UNIONed + LEFT OUTER fill).
        let nested: Vec<&JtCol> = cols
            .iter()
            .filter(|c| matches!(c, JtCol::Nested { .. }))
            .collect();
        let nested_rows = expand_jt_nested(&nested, item, env, meter)?;
        for nr in nested_rows {
            let mut row = local.clone();
            row.extend(nr);
            rows.push(row);
        }
    }
    Ok(rows)
}

/// Expand the NESTED siblings of a level over one parent item — the default-plan **UNION** of the
/// siblings (each row fills only its own subtree), with the parent→child **LEFT OUTER** fill (no
/// child rows at all → one all-NULL nested row).
fn expand_jt_nested(
    children: &[&JtCol],
    item: &JsonNode,
    env: &EvalEnv,
    meter: &mut Meter,
) -> Result<Vec<JtAssign>> {
    if children.is_empty() {
        return Ok(vec![Vec::new()]);
    }
    let mut union: Vec<JtAssign> = Vec::new();
    for child in children {
        if let JtCol::Nested { path, columns } = child {
            let p = crate::jsonpath::JsonPath::compile(path)?;
            let child_seq = crate::jsonpath::eval(&p, item).unwrap_or_default();
            union.extend(expand_jt_level(columns, &child_seq, env, meter)?);
        }
    }
    if union.is_empty() {
        union.push(Vec::new());
    }
    Ok(union)
}

/// Evaluate a regular `JSON_TABLE` column over a row item — JSON_VALUE (scalar) / JSON_QUERY
/// (json/jsonb) semantics, with the column's wrapper / ON EMPTY / ON ERROR.
#[allow(clippy::too_many_arguments)]
fn eval_jt_regular(
    item: &JsonNode,
    path: &str,
    query: bool,
    returning: ScalarType,
    decimal: Option<DecimalTypmod>,
    wrapper: JsonWrapper,
    on_empty: JsonOnBehavior,
    on_error: JsonOnBehavior,
    env: &EvalEnv,
    meter: &mut Meter,
) -> Result<Value> {
    let p = crate::jsonpath::JsonPath::compile(path)?;
    let seq = match crate::jsonpath::eval(&p, item) {
        Ok(s) => s,
        Err(e) if is_sqljson_error(&e) => {
            return apply_json_behavior(on_error, e, returning, env, meter);
        }
        Err(e) => return Err(e),
    };
    let kind = if query {
        JsonSqlKind::Query
    } else {
        JsonSqlKind::Value
    };
    eval_json_sql_result(
        kind, seq, returning, decimal, wrapper, on_empty, on_error, env, meter,
    )
}

/// Evaluate an `EXISTS` `JSON_TABLE` column over a row item — JSON_EXISTS, coerced to the column
/// type (a NON-empty sequence is true; a structural error honors ON ERROR, default FALSE).
fn eval_jt_exists(
    item: &JsonNode,
    path: &str,
    returning: ScalarType,
    on_error: JsonOnBehavior,
) -> Result<Value> {
    let p = crate::jsonpath::JsonPath::compile(path)?;
    let exists = match crate::jsonpath::eval(&p, item) {
        Ok(seq) => !seq.is_empty(),
        Err(e) if is_sqljson_error(&e) => match on_error {
            JsonOnBehavior::Error => return Err(e),
            JsonOnBehavior::True => true,
            JsonOnBehavior::Unknown => return Ok(Value::Null),
            _ => false,
        },
        Err(e) => return Err(e),
    };
    // Coerce the boolean to the column type (a `boolean` column → bool; an integer column → 1/0).
    if returning.is_bool() {
        Ok(Value::Bool(exists))
    } else if returning.is_integer() {
        Ok(Value::Int(i64::from(exists)))
    } else {
        Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "an EXISTS JSON_TABLE column must be boolean or integer this slice",
        ))
    }
}

/// The catalog name of a json two-column SRF, for its non-object error message.
fn srf_kind_name(kind: SrfKind) -> &'static str {
    match kind {
        SrfKind::JsonbEach => "jsonb_each",
        SrfKind::JsonbEachText => "jsonb_each_text",
        _ => unreachable!("srf_kind_name is only for the json two-column SRFs"),
    }
}

/// A MULTI-COLUMN synthetic table for a set-returning function (C0, json-table.md §1) — the
/// generalization of [`srf_table`] to N named/typed columns. The column NAMES are fixed by the
/// function (e.g. `jsonb_each` → `key`, `value`); the FROM alias renames the relation, not its
/// columns. Used by `json[b]_each[_text]` (and, with a col-def list, the record functions).
fn srf_table_cols(func_name: &str, alias: Option<&str>, cols: Vec<(&str, Type)>) -> Box<Table> {
    Box::new(Table {
        name: alias.unwrap_or(func_name).to_string(),
        columns: cols
            .into_iter()
            .map(|(name, ty)| Column {
                name: name.to_string(),
                ty,
                decimal: None,
                varchar_len: None,
                primary_key: false,
                not_null: false,
                default: None,
                default_expr: None,
                identity: None,
                collation: None,
            })
            .collect(),
        pk: Vec::new(),
        checks: Vec::new(),
        indexes: Vec::new(),
        foreign_keys: Vec::new(),
        exclusions: Vec::new(),
    })
}

/// One join in a SELECT plan: its kind and resolved ON predicate (`None` for CROSS). The right
/// relation is `rels[k+1]`.
pub(crate) struct PlanJoin {
    kind: JoinKind,
    on: Option<RExpr>,
}

/// A resolved SELECT, executable against an outer-row environment (the execute half of the old
/// `run_select`, lifted to a value so a correlated subquery can re-run it per outer row).
/// One resolved grouping set of a `GROUP BY` (spec/design/aggregates.md §12). For a plain `GROUP BY`
/// there is exactly one of these; `ROLLUP`/`CUBE`/`GROUPING SETS` produce several. Each is bucketed
/// independently over the post-`WHERE` rows and its groups projected into the shared synthetic row,
/// whose first `group_keys.len()` slots are the *master* grouping columns (the ordered union of all
/// sets' columns).
pub(crate) struct GroupSetPlan {
    /// The flat input-row indices this set buckets on (its key, in key order). Empty = one grand-total
    /// group (always emits one row, even over an empty input — the `()` / whole-table case).
    key_cols: Vec<usize>,
    /// Per master-grouping-column slot (length `group_keys.len()`): `Some(j)` if this set includes
    /// that column — its synthetic value is the bucket key's `j`-th component — else `None`, meaning
    /// the column is not grouped in this set and its synthetic value is `NULL`.
    slot_src: Vec<Option<usize>>,
    /// The `GROUPING()` bitmask for rows from this set: bit `p` is set iff master slot `p` is NOT in
    /// this set (so `GROUPING(col)` returns 1 for a column grouped away in this set).
    mask: i64,
}

pub(crate) struct SelectPlan {
    rels: Vec<PlanRel>,
    joins: Vec<PlanJoin>,
    filter: Option<RExpr>,
    is_agg: bool,
    group_keys: Vec<usize>,
    /// The materialized general-expression `GROUP BY` keys (`GROUP BY a + b`, aggregates.md §15), in
    /// synthetic-slot order. Before bucketing, each post-WHERE row evaluates these and appends the
    /// values at flat slots `input_width + k`, so a master grouping key index in `group_keys` /
    /// `group_sets` may point at one — the whole-row bucket machinery stays slot-based. Empty when
    /// every grouping key is a plain column (the common case, byte-identical to before).
    group_exprs: Vec<RExpr>,
    /// The grouping sets to compute (spec/design/aggregates.md §12). A plain `GROUP BY` (and the
    /// whole-table aggregate) is a single set; `ROLLUP`/`CUBE`/`GROUPING SETS` produce several.
    group_sets: Vec<GroupSetPlan>,
    /// One entry per `GROUPING(...)` call in the projection / HAVING, in synthetic-slot order: the
    /// master-grouping-column positions of its arguments. Each call's value per group row is computed
    /// from the row's grouping-set `mask` and appended after the aggregate results.
    grouping_specs: Vec<Vec<usize>>,
    agg_specs: Vec<AggSpec>,
    /// `true` when the select list has a window function — the query runs the blocking WINDOW
    /// stage (after WHERE, before ORDER BY/LIMIT) and takes the eager path (never streaming).
    /// Mutually exclusive with `is_agg` in S0 (spec/design/window.md §5.2).
    has_window: bool,
    /// One resolved window function per select-list `OVER` call (empty unless `has_window`). The
    /// window stage appends each spec's per-row result after the input columns and the materialized
    /// window keys, so the projection references result `i` as flat slot
    /// `input_width + window_keys.len() + i` (spec/design/window.md §5.1).
    window_specs: Vec<WindowSpec>,
    /// The materialized window-key expressions (a non-column PARTITION BY / ORDER BY key —
    /// `PARTITION BY a + b`, `ORDER BY a % 2`), in synthetic-slot order. Before the window stage each
    /// row evaluates these and appends the values at flat slots `input_width + k`, so the partition /
    /// sort / frame machinery (which is slot-based) is unchanged (spec/design/window.md §5.1). Empty
    /// when every window key is a bare column (the common case, byte-identical to before).
    window_keys: Vec<RExpr>,
    having: Option<RExpr>,
    /// (flat slot, descending, nulls_first, collation) per ORDER BY key. A column / ordinal key's slot
    /// is its real input / grouped-row slot; a general-**expression** key's slot is `final_row_width +
    /// k`, indexing the k-th materialized order value appended to the pre-sort row (grammar.md §10).
    order: Vec<crate::spill::SortKey>,
    /// The materialized `ORDER BY` expression-key expressions (`ORDER BY a + 1`, `ORDER BY abs(b)`), in
    /// the order their sort slots reference them. Just before the sort each row evaluates these and
    /// appends the values at `final_row_width + k` (after any window / grouped columns), so the
    /// slot-based sort stays unchanged — the window-key precedent (window.md §5.1). Empty when every
    /// ORDER BY key is a bare column or ordinal (the common case, byte-identical to before).
    order_exprs: Vec<RExpr>,
    projections: Vec<RExpr>,
    column_names: Vec<String>,
    column_types: Vec<ResolvedType>,
    distinct: bool,
    limit: Option<i64>,
    offset: Option<i64>,
    /// The **touched set** per relation (cost.md §3 "The touched set"; large-values.md §14): which
    /// of its columns this query statically references. Drives the chain-`page_read` /
    /// `value_decompress` portion of the scan's up-front cost block — an untouched spilled or
    /// compressed column charges nothing, however many records the bound admits. An ANNOTATION of
    /// the logical plan, not an optimization: a wrong mask is a disk-mode NULL-folding correctness
    /// bug, not a slow plan — so it is computed by the resolve half (`compute_rel_masks`), never by
    /// a physical rule (spec/design/planner.md §2).
    rel_masks: Vec<Vec<bool>>,
    /// The plan's physical / access-path decisions — set ONLY by the `optimize_select` pass
    /// (optimize.rs); default (zero-valued) when resolve hands the plan over
    /// (spec/design/planner.md §4).
    phys: PhysicalPlan,
}

/// The physical/access-path half of a [`SelectPlan`]: every field is the output of one discrete
/// rule of the `optimize_select` pass (spec/design/planner.md §4), applied in a fixed order after
/// the resolve half has built the logical plan. A defaulted `PhysicalPlan` is always correct — the
/// executor then full-scans and eager-sorts.
#[derive(Default)]
pub(crate) struct PhysicalPlan {
    /// Physical join position -> logical FROM ordinal. P7 sets `[0, 1]` or `[1, 0]` for eligible
    /// two-base INNER/CROSS joins; empty retains source order at barriers. Resolved slots never move.
    relation_order: Vec<usize>,
    /// One entry per physical append step for P8's N-way all-base INNER/CROSS plan. The step owns
    /// the authored ON trees that become dependency-complete there, in source order, plus an
    /// optional deterministic hash operator. An INL step is identified by the appended relation's
    /// `rel_inl_bounds` entry; otherwise no hash means nested loop. Empty retains the legacy join
    /// tree (including every semantic barrier and P7's two-relation representation).
    join_steps: Vec<PhysicalJoinStep>,
    /// Deterministic two-input hash operator. Builds the right input and probes the left using
    /// same-type bare-column equality keys in source order. `None` keeps nested loop.
    hash_join: Option<HashJoinPlan>,
    /// `ORDER BY` is satisfied by the single base relation's **primary-key scan order** — the
    /// table tree already yields rows in this order, so the sort is elided (and with a `LIMIT`
    /// the scan short-circuits a top-N). True iff the query is a single-table, non-aggregate,
    /// non-`DISTINCT` `SELECT` whose `ORDER BY` keys are a prefix of the PK columns, all one
    /// direction, with the column's stored key collation (spec/design/cost.md §3 "ORDER BY
    /// satisfied by primary-key order"). Secondary-index order is a follow-on.
    pk_ordered: bool,
    /// The PK scan direction when `pk_ordered`: `true` ⇒ the order is all-`DESC` over the full PK,
    /// served by a **reverse** scan; `false` ⇒ all-`ASC` (forward). Always `false` when
    /// `!pk_ordered`.
    pk_reverse: bool,
    /// `ORDER BY` is satisfied by walking a **B-tree secondary index** in key order (with a `LIMIT`
    /// top-N) — `Some(index)` when the PK scan does not satisfy the order but the index does
    /// (cost.md §3 "secondary-index order"). Mutually exclusive with `pk_ordered` (the PK scan is
    /// cheaper). `None` keeps the eager/streaming sort.
    index_order: Option<IndexOrder>,
    /// `ORDER BY` is satisfied by the OUTER relation's primary-key scan order in a two-table
    /// INNER/CROSS join (cost.md §3 "JOIN"): the join drives/probes the outer in PK order, so its
    /// output is already in order — the sort is elided and a `LIMIT` short-circuits the loop. Set only
    /// for exactly two non-lateral base relations, a `LIMIT`, and a forward outer-PK `ORDER BY`.
    join_pk_ordered: bool,
    /// `K = OFFSET + LIMIT` for a blocking plain-SELECT sort. `None` means the rule did not fire
    /// (or K overflowed), so the ordinary full sort remains authoritative.
    top_k: Option<i64>,
    /// Scan-bound pushdown, **one entry per relation** in `rels`: the WHERE conjuncts that
    /// bound that relation's scan — a primary-key range, or (when no PK bound applies) a
    /// secondary-index equality (cost.md §3 "bounded scan" / "index-bounded scan"). `None` ⇒
    /// a full scan of that relation. In a JOIN each base table is bounded independently by
    /// the WHERE predicates against a CONSTANT (literal/param/outer) — a cross-relation
    /// `b.pk = a.x` is the index-nested-loop case (still a follow-on; `const_source` rejects
    /// a sibling column). The residual filter stays the WHOLE `filter`, re-applied after the
    /// join — the bound only narrows which rows are scanned.
    rel_bounds: Vec<Option<ScanBound>>,
    /// Deterministic base candidate estimates. P6b composes them into complete one-base-relation
    /// access/ordering pipelines; joins retain staged legacy policies. Execution never reads this.
    rel_estimates: Vec<Vec<crate::estimator::CandidateEstimate>>,
    /// **Index-nested-loop** scan bounds, one per relation (cost.md §3 "JOIN"). `Some` for a join
    /// inner relation whose primary key / indexed column is compared to a **sibling** column of an
    /// earlier relation (`a JOIN b ON b.pk = a.x`) — a `BoundSrc::Sibling` bound resolved per outer
    /// row from the combined left-hand row. When set, that relation is NOT materialized once up
    /// front; the join loop re-materializes it per left row (like a correlated `LATERAL`), seeking
    /// instead of full-scanning — O(N·M) → O(N·log M). `None` ⇒ the ordinary once-materialized
    /// `rel_bounds` path. A set entry takes precedence over `rel_bounds` for that relation.
    rel_inl_bounds: Vec<Option<ScanBound>>,
}

pub(crate) struct HashJoinPlan {
    keys: Vec<HashJoinKey>,
}

pub(crate) struct PhysicalJoinStep {
    on_indices: Vec<usize>,
    hash_join: Option<HashJoinPlan>,
}

pub(crate) struct HashJoinKey {
    left: usize,
    right: usize,
    ty: Type,
}

// ---- Primary-key predicate pushdown (spec/design/cost.md §3 "bounded scan / point lookup") ----
//
// A single-table WHERE on the primary key bounds the storage-key range a scan must visit. Detection
// is two-stage: `detect_pk_bound` at plan time (structural — which conjuncts are PK comparisons),
// `build_key_bound` at exec time (the const values, and any $N, are known only then). The bound is a
// SUPERSET of the matching keys: the whole WHERE stays the residual filter (re-applied to each scanned
// row), so the result is always correct — the bound only narrows which rows are scanned, and the
// page_read/storage_row_read drop to what it touches. The unbounded case keeps the full scan, so its
// cost never moves.

/// The constant data extracted from a bound term's const-source operand (avoids cloning the whole
/// `RExpr` and keeps `PkBound` owned). `Timestamp` covers both timestamp and timestamptz (same
/// encoding; the PK type disambiguates). `Param`, `Outer`, and `Sibling` resolve to a value at exec
/// time: `Param` from the bound parameters, `Outer` from an enclosing query's row (a correlated
/// reference — the inner subquery's PK is bounded by the current outer row's column, so it seeks
/// instead of re-scanning the whole inner table per outer row; spec/design/cost.md §3 "bounded
/// scan", grammar.md §26), and `Sibling` from the current combined (left/running) row of a
/// left-deep join — an EARLIER join relation's column, so the inner relation seeks per outer row
/// instead of full-scanning for every outer row (index-nested-loop join, cost.md §3 "JOIN"). The
/// `Sibling` source is the join analog of `Outer`: the same per-outer-row bound, resolved from the
/// sibling row rather than an enclosing query's row.
pub(crate) enum BoundSrc {
    Int(i64),
    Bool(bool),
    Uuid([u8; 16]),
    Timestamp(i64),
    Date(i32),
    Text(String),
    Bytea(Vec<u8>),
    Decimal(Decimal),
    Float32(f32),
    Float64(f64),
    Interval(Interval),
    Null,
    Param(usize),
    Outer {
        level: usize,
        index: usize,
    },
    /// Index-nested-loop: the GLOBAL column index of an earlier join relation, read from the
    /// current combined left-hand row at exec time (cost.md §3 "JOIN"). Only ever produced by
    /// `detect_inl_bound` for a join inner relation; never appears in the once-materialized
    /// `rel_bounds`.
    Sibling(usize),
}

/// One `pk <op> const-source` from a WHERE AND-chain, normalized so the PK is the LEFT side (a
/// `5 < pk` flips to `pk > 5`).
pub(crate) struct BoundTerm {
    op: CmpOp,
    src: BoundSrc,
}

pub(crate) struct PkEqCol {
    name: String,
    col_type: ScalarType,
    srcs: Vec<BoundSrc>,
    ranges: Vec<BoundTerm>,
    coll: Option<std::sync::Arc<Collation>>,
}

pub(crate) struct PkRange {
    name: String,
    col_type: ScalarType,
    terms: Vec<BoundTerm>,
    coll: Option<std::sync::Arc<Collation>>,
}

/// The plan-time result of PK tuple analysis: a maximal equality prefix plus an optional range on
/// the next member. The concrete key range is built per execution by `build_key_bound`.
pub(crate) struct PkBound {
    eq_cols: Vec<PkEqCol>,
    range: Option<PkRange>,
    member_count: usize,
}

pub(crate) struct IntervalSpec {
    terms: Vec<BoundTerm>,
}

/// Canonical interval-set plan over a single-column PK. Specs are OR disjuncts; `clip` is the
/// co-present top-level AND bounds on that key.
pub(crate) struct PkKeySet {
    pk_type: ScalarType,
    coll: Option<std::sync::Arc<Collation>>,
    specs: Vec<IntervalSpec>,
    clip: Vec<BoundTerm>,
}

/// The [`PkKeySet`] analog over a leading B-tree secondary-index column (indexes.md §5): each
/// distinct encoded value becomes an index point probe (prefix scan + per-entry row lookup), and
/// the rows are gathered in ascending value order. `tail_types` is the remaining key components'
/// types (as in [`IndexBound`]) — the per-entry key-suffix skip.
pub(crate) struct IndexKeySet {
    name_key: String,
    col_type: ScalarType,
    coll: Option<std::sync::Arc<Collation>>,
    tail_types: Vec<ScalarType>,
    specs: Vec<IntervalSpec>,
    clip: Vec<BoundTerm>,
}

/// A per-relation scan bound (cost.md §3): a primary-key range, a secondary-index
/// equality (spec/design/indexes.md §5), a GIN-bounded scan over an array column
/// (spec/design/gin.md §6), a GiST-bounded scan, or a canonical interval set. Candidate inventory
/// and consumer selection are deliberately separate; this remains the executor-facing shape of
/// the selected candidate.
pub(crate) enum ScanBound {
    Pk(PkBound),
    Index(IndexBound),
    Gin(GinBound),
    Gist(GistBound),
    PkSet(PkKeySet),
    IndexSet(IndexKeySet),
}

/// Small physical plan shared by UPDATE/DELETE execution and DML EXPLAIN. The resolved filter stays
/// outside as the residual predicate; `bound` is only the chosen candidate superset. `db` carries
/// the target qualifier so a full scan continues through the scoped store funnel.
pub(crate) struct MutationScanPlan {
    bound: Option<ScanBound>,
    db: Option<String>,
}

/// Normalized result of executing any mutation access path: keyed rows plus the exact up-front
/// page/decompression units charged before per-row `storage_row_read`.
pub(crate) struct MutationScanBatch {
    entries: Vec<(Vec<u8>, Row)>,
    pages: usize,
    slabs: usize,
}
