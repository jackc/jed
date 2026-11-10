//! Statement executor (CLAUDE.md §10).
//!
//! This module holds the `Engine`, its `Snapshot` and the statement `Outcome` (either a bare
//! success for DDL/DML, or a query result set), and re-exports the submodules: session state
//! (`session_state`), name resolution (`scope`, `resolve*`), the expression IR (`rexpr`) and plan
//! IR (`plan_ir`), planning (`planner`, `optimize`), and execution (`execute`, `exec_*`, `dml`,
//! `ddl`). Every submodule sees the others through `use super::*`.

pub(crate) use crate::api::Rows;
pub(crate) use crate::ast::{
    AlterColumnAction, AlterColumnKind, AlterConstraintDef, AlterSeqAction, AlterSequence,
    AlterTable, AlterTableAction, AlterTableEdit, AlterView, AlterViewAction, Analyze, BinaryOp,
    CheckOption, ConflictAction, ConflictColumn, ConflictTarget, CreateIndex, CreateSequence,
    CreateTable, CreateTableAs, CreateType, CreateView, Cte, CteBody, DefaultDef, Delete,
    DropIndex, DropSequence, DropTable, DropType, DropView, Expr, FkTarget, GroupItem,
    IndexKeyElem, Insert, InsertSource, InsertValue, JoinKind, JsonBehavior, JsonOnBehavior,
    JsonPassing, JsonPredicateKind, JsonTable, JsonWrapper, JtColumn, Literal, Merge, OnConflict,
    OrderKey, Overriding, QueryExpr, RefAction, Reindex, ReindexKind, ReturningClause, Select,
    SelectItems, SeqOptions, SetOp, SetOpKind, SrcPos, Statement, SubscriptSpec, TableRef,
    TableRefPos, TypeFieldDef, TypeMod, UnaryOp, Update, ViewEdit, ViewOption, WindowDef, WithExpr,
    WithQuery,
};
pub(crate) use crate::catalog::{
    CheckConstraint, ColField, ColType, Column, CompositeField, CompositeType, DefaultExpr,
    ExclusionConstraint, ExclusionElement, ExclusionOp, FkAction, ForeignKeyConstraint,
    HostFuncDep, IdentityKind, IndexDef, IndexKey, IndexKeyExpr, IndexKind, SeqDataType, SeqOwner,
    SequenceDef, Table, TimeZoneDep, TimeZoneDeps, ViewDef, ViewDep, ViewDepKind,
    catalog_type_text, is_unparsable_error, resolve_col_type, unparsable_check_error,
    unparsable_default_error, unparsable_index_error, unparsable_view_error,
};
pub(crate) use crate::collation::{self, Collation};
pub(crate) use crate::cost::{Lifetime, Meter};
pub(crate) use crate::costs::COSTS;
pub(crate) use crate::date::parse_date;
pub(crate) use crate::decimal::{self, Decimal, MAX_PRECISION, MAX_SCALE};
pub(crate) use crate::encoding::{KeyOrder, encode_bool, encode_int, encode_terminated};
pub(crate) use crate::error::{EngineError, Result, SqlState};
pub(crate) use crate::interval::{self, Interval, parse_interval};
pub(crate) use crate::json::{self, JsonNode};
pub(crate) use crate::operators::{
    AGGREGATES, ARG_COERCIONS, AggregateDesc, OPERATORS, OperatorDesc, SET_RETURNING, WINDOWS,
};
pub(crate) use crate::pmap::KeyBound;
pub(crate) use crate::privileges::{Privilege, PrivilegeSet};
pub(crate) use crate::spill::SortKey;
pub(crate) use crate::stack::outline;
pub(crate) use crate::storage::{Row, TableStore};
pub(crate) use crate::timestamp::{InputZone, parse_timestamp};
pub(crate) use crate::types::{DecimalTypmod, ScalarType, TemporalTypmod, Type};
pub(crate) use crate::value::{
    ArrayVal, RangeVal, ThreeValued, Value, and3, from3, not3, or3, parse_bytea_hex, parse_uuid,
};
pub(crate) use std::borrow::Cow;
pub(crate) use std::collections::{BTreeSet, HashMap, HashSet};
pub(crate) use std::sync::LazyLock;

// Submodules split out of the original single-file executor. Each is a plain physical partition of
// this module: it does `use super::*;`, and its items are re-exported here so intra-executor
// references stay unqualified, exactly as when this was one file.
mod window;
pub(crate) use window::*;
mod access_path;
pub(crate) use access_path::{GinQuery, gin_query_terms, index_store_bound_entries};
mod aggregate;
pub(crate) use aggregate::{
    agg_order_tuple, jsonb_image_node, push_json_agg_elem, push_json_object_agg_pair,
    sort_agg_order_rows,
};
mod autoname;
pub(crate) use autoname::*;
mod create_table_as;
mod ddl;
pub(crate) use ddl::IndexDeriveSkip;
mod index_dependencies;
pub(crate) use index_dependencies::*;
mod dml;
mod error_detail;
mod error_hint;
mod merge;
pub(crate) use dml::CachedInsert;
pub(crate) use error_detail::{DUPLICATED, EXISTS};
mod blocking_spill;
mod eval;
mod exec_emit;
pub(crate) use exec_emit::FkKeyedTarget;
mod exec_scan;
mod hash_join;
pub(crate) use hash_join::*;
mod estimate_plan;
mod execute;
mod explain_exec;
mod kernels;
mod key_meter;
pub(crate) use key_meter::*;
mod optimize;
mod plan_params;
mod plan_query;
mod planner;
mod srf;
pub(crate) use kernels::*;
mod text_build;
pub(crate) use text_build::*;
mod store_encode;
pub(crate) use store_encode::*;
mod arith;
pub(crate) use arith::*;
mod date_ops;
pub(crate) use date_ops::*;
mod eval_ops;
pub(crate) use eval_ops::*;
mod search_cycle;
pub(crate) use search_cycle::*;
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
mod agg_plan;
pub(crate) use agg_plan::*;
mod window_desugar;
pub(crate) use window_desugar::*;
mod stored_expr_rules;
pub(crate) use stored_expr_rules::*;
mod rewrite;
pub(crate) use rewrite::*;
mod trap_class;
pub(crate) use trap_class::*;
mod engine;
mod host_batch;
pub(crate) use host_batch::{HostBatch, ReadAhead, read_ahead_cap};
mod snapshot;
mod statistics;
pub(crate) use statistics::*;
mod views;
pub(crate) use views::*;
mod view_write;
pub(crate) use view_write::*;
mod row_json;
pub(crate) use row_json::*;
mod implication;
pub(crate) use implication::{
    filter_implies_predicate, filters_imply_predicate, predicate_rejects_null,
};
mod span_filter;
pub(crate) use span_filter::{LaneFilter, ScratchRow};
mod row_index;
pub(crate) use row_index::RowIndex;
mod session_state;
pub use session_state::*;
mod scope;
pub(crate) use scope::*;
mod rexpr;
pub(crate) use rexpr::*;
mod plan_ir;
pub(crate) use plan_ir::*;
mod cte_analysis;
pub(crate) use cte_analysis::*;
mod srf_tables;
pub(crate) use srf_tables::*;
mod json_sql;
pub(crate) use json_sql::*;

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
    Spool {
        rows: crate::spill_buffer::SpoolReader,
        remaining: usize,
        mode: EmitMode,
        charged: bool,
    },
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
    /// The columnar projection fast path (`project_columnar`, packed-leaf.md §11 Tracks A2–A4). `cols`
    /// holds the pre-gathered dense per-column lanes of the surviving records (indexed by table ordinal)
    /// and `proj_cols` the projection's column indices into them; emission builds output row `j` as
    /// `[cols[proj_cols[0]][j], …]` — a bare-column projection with no full-width row, charging `row_produced` per
    /// windowed row exactly like the `Project` path over a bare column ref (a zero-cost slot read, so the
    /// lane read is cost-identical). Windowed to `[start, end)`. Lazy on the `query()` path: an early exit
    /// skips the `row_produced` of the rows it never pulls.
    Columnar {
        cols: Vec<Vec<Value>>,
        proj_cols: Vec<usize>,
        start: usize,
        end: usize,
        /// The gathered lanes' query-memory charge (memory.md §6.5), returned when emission completes.
        charge: crate::cost::StateCharge,
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

/// The default per-session RESIDENT budget for SESSION-LOCAL temporary tables, in **bytes**
/// (spec/design/temp-tables.md §6/§7) — PostgreSQL's `temp_buffers`. Once a session's temp domain
/// outgrows it, a host with a spill directory migrates the domain to a scratch file and bounds its
/// page cache by it. Residency only: results, cost, and the `54P03` point never depend on it. `0` ⇒
/// never spill (unbounded residency).
pub const DEFAULT_TEMP_BUFFERS: usize = 32 << 20;

/// The default per-session storage ceiling for SESSION-LOCAL temporary tables, in **bytes**
/// (spec/design/temp-tables.md §7). Temp tables RETAIN bytes across statements, which neither the
/// per-statement cost ceiling (`max_cost`) nor the cumulative budget (`lifetime_max_cost`) bounds, so
/// `max_temp_storage_bytes` is the §13 gate that does: once the temp domain's committed page bytes
/// exceed it — resident or spilled alike — the next temp write aborts `54P03`. `0` ⇒ unlimited (a
/// trusted handle). Identical across cores (§8); the abort point is part of the cross-core contract.
pub const DEFAULT_MAX_TEMP_STORAGE_BYTES: usize = 1 << 30;

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
    // Sequence/statistics metadata mutation generation; runtime-only and cloned transactionally.
    data_gen: u64,
    /// Opaque, non-persisted identity for this database domain. Snapshot/transaction clones share
    /// it; every fresh create/open/attachment gets a new token (estimator.md §6).
    pub(crate) estimator_identity: std::sync::Arc<EstimatorDatabaseIdentity>,
    /// Base revision shared by tables not mutated under this identity, plus per-table overrides.
    /// Replacing an `Arc` is an exact collision-free revision change; clone/rollback follow the
    /// snapshot automatically and no file-format state is involved.
    estimator_base_revision: std::sync::Arc<EstimatorRevision>,
    estimator_revisions: std::sync::Arc<HashMap<String, std::sync::Arc<EstimatorRevision>>>,
    /// Persisted, transactional P9 column facts, keyed by exact table name then column ordinal.
    statistics: std::sync::Arc<HashMap<String, HashMap<usize, ColumnStatistics>>>,
    tables: std::sync::Arc<HashMap<String, Table>>,
    /// User-defined composite (row) types, keyed by exact name (spec/design/composite.md).
    /// A database-level object set, separate from `tables`; serialized into the catalog's
    /// composite-type entries (spec/fileformat/format.md). Sorted by key when serialized so
    /// hash-map iteration order never leaks (CLAUDE.md §8).
    types: std::sync::Arc<HashMap<String, CompositeType>>,
    stores: std::sync::Arc<HashMap<String, TableStore>>,
    /// Each secondary index's B-tree (spec/design/indexes.md §3): a `TableStore` with ZERO
    /// value columns (entry keys only — the on-disk empty-payload record), keyed by the
    /// exact index name (index names live in the relation namespace, globally unique).
    /// Which table owns an index is recorded in that table's `Table::indexes`.
    index_stores: std::sync::Arc<HashMap<String, TableStore>>,
    /// Sequences, keyed by exact name (spec/design/sequences.md). A database-level object set
    /// separate from `tables`/`types`; serialized into the catalog's sequence entries
    /// (spec/fileformat/format.md, `entry_kind = 2`). The mutable counter (`last_value`/`is_called`)
    /// lives here, so `nextval` advances the working snapshot and rolls back with it (sequences.md §5).
    sequences: std::sync::Arc<HashMap<String, SequenceDef>>,
    /// Views, keyed by exact name (spec/design/views.md). They share the relation namespace with
    /// tables, indexes, and sequences; serialized as `entry_kind = 5` catalog entries (format v38).
    views: std::sync::Arc<HashMap<String, ViewDef>>,
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
    /// Each GiST index's **resident R-tree** (spec/design/gist.md §4.1), keyed by the exact
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
    /// The environment this snapshot's database was opened with (spec/design/environment.md) — what
    /// a collation name resolves against when the catalog has no file pin for it, and the version a
    /// pin is checked against (`resolve_collation` / `collation_skew`). One per handle: the main
    /// database, its attachments, and its sessions' temp domains all carry the handle's. Rides
    /// `#[derive(Clone)]` (an `Arc` bump); `#[derive(Default)]` is the empty environment, which only a
    /// bare scratch snapshot keeps. NEVER serialized.
    pub(crate) env: crate::environment::Environment,
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
    /// Exact catalog key of the (surviving) referencing table — for the CASCADE removal.
    pub ref_table_key: String,
    /// The FK constraint's name.
    pub fk_name: String,
    /// Canonical name of the referencing table — for the RESTRICT detail.
    pub ref_table_name: String,
    /// Canonical name of the dropped table the FK references — for the RESTRICT detail.
    pub dropped_name: String,
}

/// Changed domains handed from the executor commit to shared publication.
#[derive(Clone, Copy, Default)]
pub(crate) struct CommitChanges {
    pub main: bool,
    pub attached: bool,
}

/// The database handle: the last **committed** `Snapshot` plus, while a transaction is open, the
/// writer's working snapshot (CLAUDE.md §3, spec/design/transactions.md §2). Reads run against the
/// *visible* snapshot — the open transaction's `working` if any, else `committed`; a write mutates
/// `working` and commit swaps `committed := working` (rollback just drops `working`, since
/// `committed` was never touched). Every write — autocommit included — runs as a transaction, which
/// unifies the two paths.
pub struct Engine {
    pub(crate) commit_changes: CommitChanges,
    /// The last committed, immutable state — what fresh readers (and autocommit reads) see.
    pub(crate) committed: Snapshot,
    /// The **default session** (spec/design/session.md §2.1): the per-connection state this handle
    /// runs statements through — the open transaction (the `Idle`/`Open`/`Failed` machine, §2.2),
    /// the relocated settings (`max_cost`, `max_sql_length`, `work_mem`), the handle's environment,
    /// and the `currval`/`lastval` session state. A bare `Engine` IS committed storage + this one
    /// long-lived stateful default session; the convenience methods (`execute`/`begin`/
    /// `set_max_cost`/…) operate on it. `db.session(opts)` mints additional, independent sessions
    /// (run sequentially on this single-threaded handle by swapping into this slot).
    pub(crate) session: SessionState,
    /// The backing file path (`None` for an in-memory database). Set by the host API
    /// `open`/`create` (spec/design/api.md §2); `commit` writes here.
    pub(crate) path: Option<std::path::PathBuf>,
    /// The scratch-file host spill and temp-table spill write to ([`crate::spillhost`]). File hosts
    /// set this independently of `path` (normally to the OS temp directory), so a read-only database
    /// can spill without writing beside its file. `None` for hosts with no spill backing (in-memory /
    /// OPFS).
    pub(crate) spill: Option<std::sync::Arc<dyn crate::spillhost::SpillHost>>,
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
    /// only once the pages off the free list pass ~2× it, mirroring [`crate::shared::Storage`]. `0` for an
    /// in-memory database (no persistence).
    pub(crate) live_at_compaction: u32,
    /// The live-page accounting of the committed file state (spec/design/memory.md §8.7): read from
    /// the meta at open and advanced by every file commit. Zero for an in-memory engine.
    pub(crate) live: crate::format::LiveCount,
    /// The version the current `free_pages` list is "as of" — the last compaction's txid, or the
    /// committed version at open. It gates within-session reuse under the reader-liveness watermark
    /// (transactions.md §8): a page dead at generation G is reusable only once no reader pins a version
    /// older than G. A bare single-handle `Engine` has no live registry oldest_live == committed, so the
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
    /// compaction (`adopt_staged` → `maybe_compact`) must NOT reclaim pages — it could free one the cursor
    /// still faults. Incremented when a streaming `Rows` opens (shared.rs), decremented on its `Drop`
    /// (via an `OpenStreamGuard` bundled into the cursor's pin) — hence the `Arc<AtomicUsize>`: the guard
    /// outlives the `&mut self` borrow that built the cursor.
    pub(crate) open_streams: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Whether the last `commit_tx` staged any record version into main (memory.md §8.3): a commit
    /// that stages none — pure deletes and drops — may exceed `max_storage_bytes` so a full database
    /// can be repaired. Read by the shared core's publish, which persists main after `commit_tx`.
    pub(crate) commit_stages_rows: bool,
    /// Whether this handle has made the file's directory entry durable — the first durable commit
    /// after open does it once (validated-cow.md §3; the shared core's `Storage::sync_directory_once`).
    pub(crate) dir_synced: bool,
    /// The in-memory attachment commits the last `commit_tx` staged (attached-databases.md §5): their
    /// pages are in each attachment's block store, but its page accounting and post-commit compaction
    /// are adopted only after main persists and the roots publish. Taken by the shared core's publish;
    /// dropped (publishing nothing) on any failure before it.
    pub(crate) staged_attachments: Vec<crate::shared::StagedCommit>,
    /// The shared core this engine's session belongs to (attached-databases.md §5), or `None` for a
    /// bare/transient engine (a test [`Engine::new`], a `snapshot_engine`, a `from_snapshot` read view —
    /// none of which commit attachments). It is the engine's route to the core-owned attachment registry
    /// during a commit persist; the READ view of attachments is the pinned `attached_committed` below.
    /// Set at session mint (shared.rs).
    pub(crate) core: Option<std::sync::Arc<crate::shared::Shared>>,
    /// The PINNED committed root of every host-attached DATABASE-scoped database
    /// (attached-databases.md §5), keyed by exact name — this session's stable read view, snapshot
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
    /// Populated only while a view body is planned at `CREATE VIEW`, view re-validation, or a
    /// rename rewrite (spec/design/views.md §5–§8): the FROM items, columns, and `*` expansions the
    /// planner binds. `None` on every ordinary statement, which then pays one `RefCell` nil check.
    pub(crate) view_trace: std::cell::RefCell<Option<ViewTrace>>,
    /// How many view references are being expanded inside one another right now (views.md §4.1).
    pub(crate) view_depth: std::cell::Cell<usize>,
    /// The nesting depth, within the statement, at which the text being planned starts: 0 for the
    /// statement, else the depth of the view body being expanded (views.md §4.1).
    pub(crate) view_base: std::cell::Cell<usize>,
    /// Whether the planner is building a derived body's **shape** (spec/design/planner.md §3.2): a
    /// first plan read only for its output columns and pushability, then discarded and planned
    /// again. A shape skips its own bodies' second planning and stage 3, so every derived body is
    /// planned at most twice per enclosing planning and nesting depth `d` costs `O(d²)`, not `2^d`.
    pub(crate) plan_shape: std::cell::Cell<bool>,
    /// Whether a FROM reference to a CTE inlined at its reference may plan that CTE's body there
    /// (spec/design/planner.md §3.4). Off while such a CTE's own definition is planned, so the
    /// definition — which runs only if the reference is never expanded — expands nothing, and a
    /// chain of such CTEs plans each body a bounded number of times.
    pub(crate) cte_expansion_off: std::cell::Cell<bool>,
    /// How many views the outermost view reference being expanded has expanded so far, itself
    /// included (views.md §4.1): at most `MAX_VIEW_EXPANSIONS` (54001).
    pub(crate) view_expansions: std::cell::Cell<usize>,
    /// The output columns of every view planned during the outermost view expansion in progress,
    /// by view name: a view reference inside a shape reads them instead of planning the view body
    /// again (planner.md §3.2), so a chain of `d` views is planned in `O(d)`. Cleared when the
    /// outermost expansion ends.
    pub(crate) view_columns: std::cell::RefCell<HashMap<String, ViewColumns>>,
    /// The FROM relations hidden from the derived bodies being planned right now, innermost last
    /// (spec/design/error-fields.md §11.8). Read only to build a hint.
    pub(crate) unseen_frames: std::cell::RefCell<Vec<UnseenFrame>>,
}

/// A view body's output columns: names, types, collation derivations, and typmods.
pub(crate) type ViewColumns = (Vec<String>, Vec<ResolvedType>, Vec<Deriv>, Vec<Typmod>);

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

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}
